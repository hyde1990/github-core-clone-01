// test/clone-bench.mjs — kit-style in-process measurement of the push and clone
// workloads (no git CLI in the timed path): raw HTTP, prebuilt packs, pack
// checksum verification, exactly as the kit's bench does it.
// Usage: node test/clone-bench.mjs
import { spawn, execFile } from 'node:child_process';
import { mkdtemp, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { buildCommit, buildTree, hashObject, pktLine, PKT_DELIM, PKT_FLUSH, verifyPackChecksum, writePack } from '../src/git.ts';

const TOKEN = 'test-token-123';
const ROOT = join(fileURLToPath(import.meta.url), '..', '..');
const T_REF = { push: 1640.82, clone: 266.941 };

function points(t, tRef) {
  return Math.round(Math.max(0, Math.min(50, 25 - 12.5 * Math.log2(t / tRef))));
}

function git(args, cwd) {
  return new Promise((resolve) => {
    execFile('git', ['-c', `http.extraHeader=Authorization: Bearer ${TOKEN}`, '-c', 'init.defaultBranch=main', ...args], {
      cwd,
      env: { ...process.env, GIT_AUTHOR_NAME: 'Bench', GIT_AUTHOR_EMAIL: 'bench@make.fun', GIT_COMMITTER_NAME: 'Bench', GIT_COMMITTER_EMAIL: 'bench@make.fun' },
      timeout: 60000,
    }, (error, stdout, stderr) => resolve({ code: error === null ? 0 : (error.code ?? 1), stdout, stderr }));
  });
}

async function main() {
  const child = spawn('node', ['main.ts'], { cwd: ROOT, env: { ...process.env, MAKEFUN_SHOP_TOKEN: TOKEN }, stdio: ['ignore', 'pipe', 'pipe'] });
  let stderr = '';
  child.stderr.on('data', (c) => { stderr += c; });
  const port = await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`server did not start: ${stderr}`)), 10000);
    child.stdout.on('data', (chunk) => {
      const match = /listening on 127\.0\.0\.1:(\d+)/.exec(chunk.toString());
      if (match !== null) { clearTimeout(timer); resolve(Number(match[1])); }
    });
  });
  const base = `http://127.0.0.1:${port}`;
  const headers = { Authorization: `Bearer ${TOKEN}` };
  console.log(`server on ${base}`);

  async function api(path, { method = 'GET', body } = {}) {
    const response = await fetch(`${base}${path}`, {
      method,
      headers: { ...headers, Accept: 'application/vnd.github+json', ...(body !== undefined ? { 'Content-Type': 'application/json' } : {}) },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    return response;
  }

  try {
    // Build a 200-commit repo via git CLI (setup only, untimed).
    await api('/api/v3/user/repos', { method: 'POST', body: { name: 'bench-push' } });
    const work = await mkdtemp(join(tmpdir(), 'forge-bench-'));
    await git(['clone', `${base}/adaiversen/bench-push.git`, 'repo'], work);
    const repoDir = join(work, 'repo');
    for (let i = 0; i < 200; i += 1) {
      await writeFile(join(repoDir, 'file.txt'), Buffer.alloc(1024, `c-${i}-`));
      await git(['add', 'file.txt'], repoDir);
      await git(['commit', '-m', `commit ${i}`], repoDir);
      const pushed = await git(['push', 'origin', 'main'], repoDir);
      if (pushed.code !== 0) throw new Error(`setup push ${i} failed: ${pushed.stderr.slice(0, 200)}`);
    }
    console.log('setup: 200-commit repo ready');

    // Tip via ls-refs (v2).
    async function tipOf() {
      const body = Buffer.concat([
        pktLine('command=ls-refs\n'),
        pktLine('object-format=sha1\n'),
        PKT_DELIM,
        pktLine('peel\n'),
        pktLine('unborn\n'),
        pktLine('symrefs\n'),
        pktLine('ref-prefix refs/heads/main\n'),
        PKT_FLUSH,
      ]);
      const response = await fetch(`${base}/adaiversen/bench-push.git/git-upload-pack`, {
        method: 'POST',
        headers: { ...headers, 'Git-Protocol': 'version=2', 'Content-Type': 'application/x-git-upload-pack-request' },
        body,
      });
      const text = Buffer.from(await response.arrayBuffer()).toString('utf8');
      const match = /([0-9a-f]{40}) refs\/heads\/main\n/.exec(text);
      if (match === null) throw new Error(`ls-refs did not return main: ${text.slice(0, 200)}`);
      return match[1];
    }

    // --- clone workload: 50 concurrent shallow clones, pack checksum verified.
    const cloneTimes = [];
    for (let round = 0; round < 3; round += 1) {
      const tip = await tipOf();
      const start = performance.now();
      const clones = Array.from({ length: 50 }, async () => {
        const reqBody = Buffer.concat([
          pktLine('command=fetch\n'),
          pktLine('object-format=sha1\n'),
          PKT_DELIM,
          pktLine('thin-pack\n'),
          pktLine('no-progress\n'),
          pktLine('ofs-delta\n'),
          pktLine(`want ${tip}\n`),
          pktLine('deepen 1\n'),
          pktLine('done\n'),
          PKT_FLUSH,
        ]);
        const response = await fetch(`${base}/adaiversen/bench-push.git/git-upload-pack`, {
          method: 'POST',
          headers: { ...headers, 'Git-Protocol': 'version=2', 'Content-Type': 'application/x-git-upload-pack-request' },
          body: reqBody,
        });
        const buf = Buffer.from(await response.arrayBuffer());
        // The pack rides one sideband packet: bound it by that packet's length
        // field, so the trailing flush is not included in the checksum.
        const at = buf.indexOf(Buffer.from('PACK'));
        if (at < 0) throw new Error('no pack in clone response');
        const packetStart = at - 5; // 4-byte hex length + 1 band byte
        const packetLength = Number.parseInt(buf.subarray(packetStart, packetStart + 4).toString('ascii'), 16);
        const pack = buf.subarray(at, packetStart + packetLength);
        if (!verifyPackChecksum(pack)) throw new Error('pack checksum mismatch');
        return pack.length;
      });
      const sizes = await Promise.all(clones);
      cloneTimes.push(performance.now() - start);
      console.log(`  clone round ${round}: ${cloneTimes[round].toFixed(1)} ms total, pack ${sizes[0]} bytes`);
    }
    const cloneMedian = cloneTimes.sort((a, b) => a - b)[1];
    console.log(`clone 50 concurrent shallow (in-process): median ${cloneMedian.toFixed(1)} ms total vs T_ref ${T_REF.clone} ms -> ${points(cloneMedian, T_REF.clone)} pts`);

    // --- push workload: 200 sequential receive-pack exchanges, prebuilt packs.
    let tip = await tipOf();
    const pushTimes = [];
    for (let i = 0; i < 200; i += 1) {
      const blobData = Buffer.alloc(1024, `push-${i}-`);
      const blobOid = hashObject('blob', blobData);
      const treeData = buildTree([{ mode: '100644', name: 'file.txt', oid: blobOid }]);
      const treeOid = hashObject('tree', treeData);
      const commitData = buildCommit({
        tree: treeOid,
        parents: [tip],
        author: 'Conformance <conformance@make.fun>',
        timestamp: 1700000000 + i,
        message: `push ${i}\n`,
      });
      const commitOid = hashObject('commit', commitData);
      const pack = writePack([
        { type: 'blob', data: blobData },
        { type: 'tree', data: treeData },
        { type: 'commit', data: commitData },
      ]);
      const t0 = performance.now();
      // Advertisement GET first (as the kit's push op does).
      await fetch(`${base}/adaiversen/bench-push.git/info/refs?service=git-receive-pack`, { headers });
      const cmdBody = Buffer.concat([
        pktLine(`${tip} ${commitOid} refs/heads/main\0 report-status side-band-64k\n`),
        PKT_FLUSH,
        pack,
      ]);
      const response = await fetch(`${base}/adaiversen/bench-push.git/git-receive-pack`, {
        method: 'POST',
        headers: { ...headers, 'Content-Type': 'application/x-git-receive-pack-request' },
        body: cmdBody,
      });
      const text = Buffer.from(await response.arrayBuffer()).toString('utf8');
      if (!text.includes('unpack ok') || !text.includes('ok refs/heads/main')) throw new Error(`push ${i} rejected: ${text.slice(0, 200)}`);
      pushTimes.push(performance.now() - t0);
      tip = commitOid;
    }
    const pushMedian = pushTimes.sort((a, b) => a - b)[100];
    console.log(`push 200x1KiB (in-process): median per-op ${pushMedian.toFixed(2)} ms vs T_ref ${T_REF.push} ms/op -> ${points(pushMedian, T_REF.push)} pts`);
  } finally {
    child.kill('SIGTERM');
  }
}

main().catch((err) => {
  console.error('bench crashed:', err);
  process.exit(2);
});
