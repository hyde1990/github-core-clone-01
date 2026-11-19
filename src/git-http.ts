// git-http.ts — git smart HTTP: info/refs advertisements (v0 + v2), upload-pack
// (v0 + v2 fetch, ls-refs) and receive-pack (v0 push). Pure Node stdlib; the
// pack protocol is implemented in src/git.ts. No git binary is involved.
//
// Wire notes (from the suite's git-client.ts, which is the only client):
// - upload-pack speaks v2 when the request carries `Git-Protocol: version=2`,
//   else v0. receive-pack is v0 only.
// - v2 advertisement: `version 2` + capability lines, no service preamble.
// - v0 advertisement: `# service=git-...` preamble + ref lines, first line
//   carries capabilities after NUL.
// - fetch v2 answers sections (acknowledgments, shallow-info, packfile) split
//   by delim packets; the packfile section carries sideband band-1 packets.
// - push answers report-status inside sideband band 1 (itself pkt-lined).

import type { IncomingMessage, ServerResponse } from 'node:http';
import {
  collectReachable,
  decodePktLines,
  encodePktLines,
  isObjectId,
  parseCommit,
  parseTag,
  parseTree,
  PKT_DELIM,
  PKT_FLUSH,
  pktLine,
  readPack,
  verifyPackChecksum,
  writePack,
  ZERO_OID,
  type GitObject,
} from './git.ts';
import { sendBuffer, sendJson } from './http-util.ts';
import { store, type Repo } from './store.ts';

const UPLOAD_AD = 'application/x-git-upload-pack-advertisement';
const UPLOAD_RESULT = 'application/x-git-upload-pack-result';
const RECEIVE_AD = 'application/x-git-receive-pack-advertisement';
const RECEIVE_RESULT = 'application/x-git-receive-pack-result';

export function gitAuthToken(req: IncomingMessage): string | null {
  const header = req.headers.authorization ?? '';
  const match = /^Bearer\s+(.+)$/.exec(header);
  return match === null ? null : match[1]!;
}

export function shopToken(): string {
  return process.env.MAKEFUN_SHOP_TOKEN ?? '';
}

/** 401 unless the request carries the trial's token. Returns true when authorised. */
export function requireGitAuth(req: IncomingMessage, res: ServerResponse): boolean {
  const token = shopToken();
  if (token === '' || gitAuthToken(req) === token) return true;
  sendJson(res, 401, { message: 'Requires authentication' });
  return false;
}

interface RefLine {
  name: string;
  oid: string;
  peeled?: string;
  symrefTarget?: string;
}

/** All refs of a repo, sorted by name, with HEAD first when asked. */
function listRefs(repo: Repo, includeHead: boolean): RefLine[] {
  const out: RefLine[] = [];
  const mainTip = repo.refs.get('refs/heads/main');
  if (includeHead) {
    out.push({
      name: 'HEAD',
      oid: mainTip ?? ZERO_OID,
      symrefTarget: 'refs/heads/main',
    });
  }
  const names = [...repo.refs.keys()].sort();
  for (const name of names) {
    const oid = repo.refs.get(name)!;
    const line: RefLine = { name, oid };
    if (name.startsWith('refs/tags/')) {
      const object = repo.objects.get(oid);
      if (object !== undefined && object.type === 'tag') {
        const peeled = store.peelToCommit(repo, oid);
        if (peeled !== null) line.peeled = peeled;
      }
    }
    out.push(line);
  }
  return out;
}

function v0RefLine(line: RefLine, caps: string | null): string {
  const base = `${line.oid} ${line.name}`;
  return caps === null ? `${base}\n` : `${base}\0${caps}\n`;
}

// -------------------------------------------------------------------------------------------
// advertisements
// -------------------------------------------------------------------------------------------

function advertisementV2Upload(): Buffer {
  const lines = [
    'version 2\n',
    'fetch=shallow filter\n',
    'ls-refs=unborn\n',
    'object-format=sha1\n',
    'flush',
  ];
  return encodePktLines(lines);
}

function advertisementV0(repo: Repo, service: 'git-upload-pack' | 'git-receive-pack'): Buffer {
  const refs = listRefs(repo, true);
  const caps =
    service === 'git-upload-pack'
      ? 'side-band-64k thin-pack ofs-delta shallow symref=HEAD:refs/heads/main'
      : 'report-status side-band-64k delete-refs ofs-delta symref=HEAD:refs/heads/main';
  const parts: Buffer[] = [pktLine(`# service=${service}\n`), PKT_FLUSH];
  if (repo.refs.size === 0) {
    // An unborn repository advertises the capabilities pseudo-ref, not a HEAD line.
    parts.push(pktLine(`${ZERO_OID} capabilities^{}\0${caps}\n`));
  } else {
    refs.forEach((line, index) => {
      parts.push(pktLine(v0RefLine(line, index === 0 ? caps : null)));
      if (line.peeled !== undefined) parts.push(pktLine(`${line.peeled} ${line.name}^{}\n`));
    });
  }
  parts.push(PKT_FLUSH);
  return Buffer.concat(parts);
}

export function handleInfoRefs(
  req: IncomingMessage,
  res: ServerResponse,
  owner: string,
  repoName: string,
  service: string | null,
): void {
  if (!requireGitAuth(req, res)) return;
  const repo = store.getRepo(owner, repoName);
  if (repo === undefined) {
    sendJson(res, 404, { message: 'Not Found' });
    return;
  }
  if (service !== 'git-upload-pack' && service !== 'git-receive-pack') {
    sendJson(res, 400, { message: 'Invalid service' });
    return;
  }
  const protocol = req.headers['git-protocol'];
  if (service === 'git-upload-pack' && protocol === 'version=2') {
    sendBuffer(res, 200, advertisementV2Upload(), UPLOAD_AD);
    return;
  }
  sendBuffer(
    res,
    200,
    advertisementV0(repo, service),
    service === 'git-upload-pack' ? UPLOAD_AD : RECEIVE_AD,
  );
}

// -------------------------------------------------------------------------------------------
// fetch: object selection
// -------------------------------------------------------------------------------------------

interface FetchPlan {
  /** Objects to send, in a stable order. */
  objects: GitObject[];
  shallow: string[];
  unshallow: string[];
}

/** Which objects a fetch sends, honouring haves, depth and the client's known shallow boundary. */
function planFetch(
  repo: Repo,
  wants: string[],
  haves: string[],
  knownShallow: string[],
  depth: number | null,
): FetchPlan {
  const lookup = store.lookup(repo);
  const haveClosure = haves.length > 0 ? collectReachable(haves, lookup, { allowMissing: true }) : new Set<string>();
  const objects = new Map<string, GitObject>();
  const shallow: string[] = [];
  const unshallow: string[] = [];

  const addObject = (oid: string): void => {
    const object = repo.objects.get(oid);
    if (object !== undefined && !objects.has(oid)) objects.set(oid, object);
  };

  if (depth !== null) {
    // Generations from the wants: gen 0 = the wants, gen k = their k-th ancestors.
    // A parent the client already holds (a have) is not re-sent — but anything
    // else within the depth window is delivered, even when it is reachable from
    // a have: a shallow boundary's parents are not held by the client, so a
    // have-closure filter here would swallow exactly the commits a deepen must
    // deliver.
    const haveSet = new Set(haves);
    const genOf = new Map<string, number>();
    const queue: Array<{ oid: string; gen: number }> = [];
    for (const want of wants) {
      if (isObjectId(want) && repo.objects.has(want)) queue.push({ oid: want, gen: 0 });
    }
    const includedCommits: string[] = [];
    while (queue.length > 0) {
      const { oid, gen } = queue.shift()!;
      if (genOf.has(oid)) continue;
      genOf.set(oid, gen);
      const object = repo.objects.get(oid);
      if (object === undefined || object.type !== 'commit') continue;
      if (gen >= depth) continue;
      if (!haveSet.has(oid)) includedCommits.push(oid);
      if (gen === depth - 1) {
        // Boundary commit: its parents are cut off; it is the new shallow boundary.
        if (!shallow.includes(oid)) shallow.push(oid);
        continue;
      }
      for (const parent of parseCommit(object.data).parents) {
        if (!haveSet.has(parent)) queue.push({ oid: parent, gen: gen + 1 });
      }
    }
    for (const commitOid of includedCommits) {
      addObject(commitOid);
      const tree = store.commitTree(repo, commitOid);
      if (tree !== null) {
        for (const oid of collectReachable([tree], lookup, { allowMissing: true })) addObject(oid);
      }
    }
    // Unshallow: a previously shallow commit whose parents are all present on
    // the client now — held already, or delivered in this fetch.
    for (const oid of knownShallow) {
      const object = repo.objects.get(oid);
      if (object === undefined || object.type !== 'commit') continue;
      if (!genOf.has(oid)) continue;
      const parents = parseCommit(object.data).parents;
      if (parents.every((parent) => haveSet.has(parent) || (genOf.has(parent) && (genOf.get(parent) ?? 99) < depth))) {
        if (!unshallow.includes(oid)) unshallow.push(oid);
      }
    }
  } else {
    // Full fetch minus the have closure: BFS from the wants, never crossing a have.
    const seen = new Set<string>();
    const queue: string[] = [];
    for (const want of wants) if (isObjectId(want)) queue.push(want);
    while (queue.length > 0) {
      const oid = queue.pop()!;
      if (seen.has(oid) || haveClosure.has(oid)) continue;
      seen.add(oid);
      const object = repo.objects.get(oid);
      if (object === undefined) continue;
      objects.set(oid, object);
      if (object.type === 'commit') {
        const commit = parseCommit(object.data);
        queue.push(commit.tree);
        for (const parent of commit.parents) queue.push(parent);
      } else if (object.type === 'tree') {
        for (const entry of parseTree(object.data)) {
          if (entry.mode !== '160000') queue.push(entry.oid);
        }
      } else if (object.type === 'tag') {
        queue.push(parseTag(object.data).object);
      }
    }
  }
  // Stable order: commits first (in discovery order), then trees, then blobs, then tags.
  // The client's readPack does not care about order; this keeps bytes deterministic.
  const ordered = [...objects.values()].sort((a, b) => {
    const rank = (type: string): number => (type === 'commit' ? 0 : type === 'tree' ? 1 : type === 'blob' ? 2 : 3);
    return rank(a.type) - rank(b.type);
  });
  return { objects: ordered, shallow, unshallow };
}

function sidebandPack(pack: Buffer): Buffer {
  const parts: Buffer[] = [];
  const CHUNK = 65515;
  for (let offset = 0; offset < pack.length; offset += CHUNK) {
    const chunk = pack.subarray(offset, Math.min(pack.length, offset + CHUNK));
    parts.push(pktLine(Buffer.concat([Buffer.from([1]), chunk])));
  }
  if (pack.length === 0) parts.push(pktLine(Buffer.from([1])));
  return Buffer.concat(parts);
}

function fetchResponseV2(repo: Repo, wants: string[], haves: string[], knownShallow: string[], depth: number | null): Buffer {
  const plan = planFetch(repo, wants, haves, knownShallow, depth);
  const pack = writePack(plan.objects);
  const parts: Buffer[] = [];
  if (haves.length > 0) {
    parts.push(pktLine('acknowledgments\n'), pktLine('NAK\n'), PKT_DELIM);
  }
  if (plan.shallow.length > 0 || plan.unshallow.length > 0) {
    parts.push(pktLine('shallow-info\n'));
    for (const oid of plan.shallow) parts.push(pktLine(`shallow ${oid}\n`));
    for (const oid of plan.unshallow) parts.push(pktLine(`unshallow ${oid}\n`));
    parts.push(PKT_DELIM);
  }
  parts.push(pktLine('packfile\n'));
  parts.push(sidebandPack(pack));
  parts.push(PKT_FLUSH);
  return Buffer.concat(parts);
}

function fetchResponseV0(repo: Repo, wants: string[], haves: string[], knownShallow: string[], depth: number | null): Buffer {
  const plan = planFetch(repo, wants, haves, knownShallow, depth);
  const pack = writePack(plan.objects);
  const parts: Buffer[] = [];
  for (const oid of plan.shallow) parts.push(pktLine(`shallow ${oid}\n`));
  for (const oid of plan.unshallow) parts.push(pktLine(`unshallow ${oid}\n`));
  if (haves.length > 0) parts.push(pktLine('NAK\n'));
  parts.push(sidebandPack(pack));
  parts.push(PKT_FLUSH);
  return Buffer.concat(parts);
}

// -------------------------------------------------------------------------------------------
// upload-pack
// -------------------------------------------------------------------------------------------

interface FetchRequest {
  wants: string[];
  haves: string[];
  shallow: string[];
  depth: number | null;
}

function parseFetchV2(body: Buffer): FetchRequest | null {
  let packets;
  try {
    packets = decodePktLines(body);
  } catch {
    return null;
  }
  const request: FetchRequest = { wants: [], haves: [], shallow: [], depth: null };
  let inArgs = false;
  for (const packet of packets) {
    if (packet.kind === 'flush' || packet.kind === 'delim') {
      inArgs = true;
      continue;
    }
    const line = packet.payload.toString('utf8').replace(/\n$/, '');
    if (!inArgs) {
      if (line !== 'command=fetch' && !line.startsWith('agent=') && !line.startsWith('object-format=')) return null;
      continue;
    }
    if (line.startsWith('want ')) request.wants.push(line.slice(5).trim());
    else if (line.startsWith('have ')) request.haves.push(line.slice(5).trim());
    else if (line.startsWith('shallow ')) request.shallow.push(line.slice(8).trim());
    else if (line.startsWith('deepen ')) {
      const n = Number.parseInt(line.slice(7).trim(), 10);
      if (Number.isFinite(n) && n > 0) request.depth = n;
    }
    // deepen-since, deepen-not, filter, thin-pack, no-progress, ofs-delta, done: accepted, ignored.
  }
  return request;
}

function parseFetchV0(body: Buffer): FetchRequest | null {
  let packets;
  try {
    packets = decodePktLines(body);
  } catch {
    return null;
  }
  const request: FetchRequest = { wants: [], haves: [], shallow: [], depth: null };
  let section = 0;
  for (const packet of packets) {
    if (packet.kind === 'flush') {
      section += 1;
      continue;
    }
    if (packet.kind === 'delim') continue;
    const line = packet.payload.toString('utf8').replace(/\n$/, '');
    if (section === 0) {
      if (line.startsWith('want ')) request.wants.push(line.slice(5).trim().split(' ')[0]!);
      else if (line.startsWith('shallow ')) request.shallow.push(line.slice(8).trim());
      else if (line.startsWith('deepen ')) {
        const n = Number.parseInt(line.slice(7).trim(), 10);
        if (Number.isFinite(n) && n > 0) request.depth = n;
      }
    } else if (line.startsWith('have ')) {
      request.haves.push(line.slice(5).trim());
    }
  }
  return request;
}

export function handleUploadPack(
  req: IncomingMessage,
  res: ServerResponse,
  owner: string,
  repoName: string,
  body: Buffer,
): void {
  if (!requireGitAuth(req, res)) return;
  const repo = store.getRepo(owner, repoName);
  if (repo === undefined) {
    sendJson(res, 404, { message: 'Not Found' });
    return;
  }
  const isV2 = req.headers['git-protocol'] === 'version=2';
  // ls-refs rides upload-pack as a v2 command: detect it before the fetch parse.
  if (isV2) {
    try {
      const packets = decodePktLines(body);
      const first = packets.find((p) => p.kind === 'data');
      if (first !== undefined && first.payload.toString('utf8').startsWith('command=ls-refs')) {
        handleLsRefs(res, repo, body);
        return;
      }
    } catch {
      sendJson(res, 400, { message: 'Malformed git request' });
      return;
    }
  }
  const parsed = isV2 ? parseFetchV2(body) : parseFetchV0(body);
  if (parsed === null) {
    // A malformed request must not take the server down: answer 400 and stay up.
    sendJson(res, 400, { message: 'Malformed git request' });
    return;
  }
  if (parsed.wants.length === 0) {
    // A fetch with no wants still answers a packfile section (an empty pack).
    const empty = isV2
      ? fetchResponseV2(repo, [], [], [], null)
      : fetchResponseV0(repo, [], [], [], null);
    sendBuffer(res, 200, empty, UPLOAD_RESULT);
    return;
  }
  const out = isV2
    ? fetchResponseV2(repo, parsed.wants, parsed.haves, parsed.shallow, parsed.depth)
    : fetchResponseV0(repo, parsed.wants, parsed.haves, parsed.shallow, parsed.depth);
  sendBuffer(res, 200, out, UPLOAD_RESULT);
}

// -------------------------------------------------------------------------------------------
// ls-refs (v2)
// -------------------------------------------------------------------------------------------

function handleLsRefs(res: ServerResponse, repo: Repo, body: Buffer): void {
  let packets;
  try {
    packets = decodePktLines(body);
  } catch {
    sendJson(res, 400, { message: 'Malformed git request' });
    return;
  }
  let peel = false;
  let symrefs = false;
  let unborn = false;
  const prefixes: string[] = [];
  let inArgs = false;
  for (const packet of packets) {
    if (packet.kind === 'flush' || packet.kind === 'delim') {
      inArgs = true;
      continue;
    }
    const line = packet.payload.toString('utf8').replace(/\n$/, '');
    if (!inArgs) continue;
    if (line === 'peel') peel = true;
    else if (line === 'symrefs') symrefs = true;
    else if (line === 'unborn') unborn = true;
    else if (line.startsWith('ref-prefix ')) prefixes.push(line.slice('ref-prefix '.length));
  }
  const lines: string[] = [];
  const refs = listRefs(repo, false);
  const mainTip = repo.refs.get('refs/heads/main');
  const headPasses =
    prefixes.length === 0 || prefixes.some((prefix) => 'HEAD'.startsWith(prefix) || prefix.startsWith('HEAD'));
  if (mainTip !== undefined && headPasses) {
    lines.push(`${mainTip} HEAD${symrefs ? ' symref-target:refs/heads/main' : ''}\n`);
  } else if (mainTip === undefined && unborn && headPasses) {
    lines.push(`unborn HEAD${symrefs ? ' symref-target:refs/heads/main' : ''}\n`);
  }
  for (const ref of refs) {
    if (prefixes.length > 0 && !prefixes.some((prefix) => ref.name.startsWith(prefix))) continue;
    let line = `${ref.oid} ${ref.name}`;
    if (peel && ref.peeled !== undefined) line += ` peeled:${ref.peeled}`;
    lines.push(`${line}\n`);
  }
  const parts: Buffer[] = lines.map((line) => pktLine(line));
  parts.push(PKT_FLUSH);
  sendBuffer(res, 200, Buffer.concat(parts), UPLOAD_RESULT);
}

// -------------------------------------------------------------------------------------------
// receive-pack (v0 push)
// -------------------------------------------------------------------------------------------

interface PushCommand {
  oldOid: string;
  newOid: string;
  ref: string;
}

function pushReport(results: Array<{ ref: string; ok: boolean; reason: string }>, unpackLine = 'unpack ok\n'): Buffer {
  const inner: Buffer[] = [pktLine(unpackLine)];
  for (const result of results) {
    inner.push(pktLine(result.ok ? `ok ${result.ref}\n` : `ng ${result.ref} ${result.reason}\n`));
  }
  // The report stream itself ends with a flush packet, inside the sideband
  // stream: the client's report reader reads pkt-lines until that flush, and
  // the outer flush below only terminates the sideband stream.
  inner.push(PKT_FLUSH);
  const innerStream = Buffer.concat(inner);
  // The report stream rides sideband band 1, itself pkt-lined; split if huge.
  const parts: Buffer[] = [];
  const CHUNK = 65515;
  for (let offset = 0; offset < innerStream.length; offset += CHUNK) {
    const chunk = innerStream.subarray(offset, Math.min(innerStream.length, offset + CHUNK));
    parts.push(pktLine(Buffer.concat([Buffer.from([1]), chunk])));
  }
  parts.push(PKT_FLUSH);
  return Buffer.concat(parts);
}

export function handleReceivePack(
  req: IncomingMessage,
  res: ServerResponse,
  owner: string,
  repoName: string,
  body: Buffer,
): void {
  if (!requireGitAuth(req, res)) return;
  const repo = store.getRepo(owner, repoName);
  if (repo === undefined) {
    sendJson(res, 404, { message: 'Not Found' });
    return;
  }
  // Walk the raw body: command pkt-lines up to the flush, then the pack bytes.
  const commands: PushCommand[] = [];
  let offset = 0;
  let packOffset = body.length;
  let malformed = false;
  while (offset + 4 <= body.length) {
    const head = body.subarray(offset, offset + 4).toString('ascii');
    if (!/^[0-9a-fA-F]{4}$/.test(head)) {
      malformed = true;
      break;
    }
    const length = parseInt(head, 16);
    if (length === 0) {
      packOffset = offset + 4;
      break;
    }
    if (length === 1) {
      offset += 4;
      continue;
    }
    if (length < 4 || offset + length > body.length) {
      malformed = true;
      break;
    }
    const line = body.subarray(offset + 4, offset + length).toString('utf8').replace(/\n$/, '');
    const nul = line.indexOf('\0');
    const headLine = nul < 0 ? line : line.slice(0, nul);
    const parts = headLine.split(' ');
    if (parts.length >= 3 && isObjectId(parts[0]!) && isObjectId(parts[1]!)) {
      commands.push({ oldOid: parts[0]!, newOid: parts[1]!, ref: parts.slice(2).join(' ') });
    }
    offset += length;
  }
  if (malformed) {
    sendJson(res, 400, { message: 'Malformed git request' });
    return;
  }
  const packBytes = body.subarray(packOffset);
  if (packBytes.length > 0) {
    try {
      if (!verifyPackChecksum(packBytes)) throw new Error('pack trailer checksum mismatch');
      const read = readPack(packBytes, { resolveBase: store.lookup(repo) });
      for (const [oid, object] of read.objects) {
        if (!repo.objects.has(oid)) repo.objects.set(oid, object);
      }
    } catch (err) {
      // Unpack failure: report it per-ref and keep the server up.
      const results = commands.map((command) => ({
        ref: command.ref,
        ok: false,
        reason: err instanceof Error ? err.message : 'unpacker error',
      }));
      sendBuffer(
        res,
        200,
        pushReport(results, `unpack ${err instanceof Error ? err.message : 'error'}\n`),
        RECEIVE_RESULT,
      );
      return;
    }
  }
  const results: Array<{ ref: string; ok: boolean; reason: string }> = [];
  for (const command of commands) {
    const current = repo.refs.get(command.ref);
    if (command.newOid === ZERO_OID) {
      // Delete.
      if (current === undefined) {
        results.push({ ref: command.ref, ok: false, reason: 'remote ref does not exist' });
      } else if (current !== command.oldOid) {
        results.push({ ref: command.ref, ok: false, reason: 'non-fast-forward' });
      } else {
        repo.refs.delete(command.ref);
        results.push({ ref: command.ref, ok: true, reason: '' });
      }
      continue;
    }
    if (!repo.objects.has(command.newOid)) {
      results.push({ ref: command.ref, ok: false, reason: 'object not found' });
      continue;
    }
    if (current === undefined) {
      if (command.oldOid !== ZERO_OID) {
        results.push({ ref: command.ref, ok: false, reason: 'non-fast-forward' });
        continue;
      }
      repo.refs.set(command.ref, command.newOid);
      results.push({ ref: command.ref, ok: true, reason: '' });
      continue;
    }
    if (current !== command.oldOid) {
      results.push({ ref: command.ref, ok: false, reason: 'non-fast-forward' });
      continue;
    }
    if (!store.isFastForward(repo, command.oldOid, command.newOid)) {
      results.push({ ref: command.ref, ok: false, reason: 'non-fast-forward' });
      continue;
    }
    repo.refs.set(command.ref, command.newOid);
    results.push({ ref: command.ref, ok: true, reason: '' });
  }
  sendBuffer(res, 200, pushReport(results), RECEIVE_RESULT);
}
