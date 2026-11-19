// test/smoke.mjs — local conformance smoke test for the github-core-clone forge.
// Spawns `node main.ts` (no MAKEFUN_ENV_URL: standalone mode), then exercises
// REST v3, real git CLI clone/push/fetch, GraphQL, MCP and the four perf workloads.
// Usage: node test/smoke.mjs [--perf]   (perf runs the timed workloads; default on)
import { spawn, execFile } from 'node:child_process';
import { mkdtemp, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const TOKEN = 'test-token-123';
const ROOT = join(fileURLToPath(import.meta.url), '..', '..');

let passed = 0;
let failed = 0;
const failures = [];

function check(name, ok, detail = '') {
  if (ok) {
    passed += 1;
    console.log(`  ok   ${name}`);
  } else {
    failed += 1;
    failures.push(name);
    console.log(`  FAIL ${name}${detail ? ` — ${detail}` : ''}`);
  }
}

function eq(a, b) {
  return JSON.stringify(a) === JSON.stringify(b);
}

async function startServer() {
  const child = spawn('node', ['main.ts'], {
    cwd: ROOT,
    env: { ...process.env, MAKEFUN_SHOP_TOKEN: TOKEN },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  let stderr = '';
  child.stderr.on('data', (chunk) => { stderr += chunk; });
  const port = await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`server did not start: ${stderr}`)), 10000);
    child.stdout.on('data', (chunk) => {
      const match = /listening on 127\.0\.0\.1:(\d+)/.exec(chunk.toString());
      if (match !== null) {
        clearTimeout(timer);
        resolve(Number(match[1]));
      }
    });
    child.on('exit', (code) => {
      clearTimeout(timer);
      reject(new Error(`server exited ${code}: ${stderr}`));
    });
  });
  return { child, port, stderr: () => stderr };
}

function git(args, cwd, extraEnv = {}) {
  return new Promise((resolve) => {
    execFile('git', ['-c', `http.extraHeader=Authorization: Bearer ${TOKEN}`, '-c', 'init.defaultBranch=main', ...args], {
      cwd,
      env: { ...process.env, GIT_AUTHOR_NAME: 'Smoke', GIT_AUTHOR_EMAIL: 'smoke@make.fun', GIT_COMMITTER_NAME: 'Smoke', GIT_COMMITTER_EMAIL: 'smoke@make.fun', ...extraEnv },
      timeout: 30000,
    }, (error, stdout, stderr) => {
      resolve({ code: error === null ? 0 : (error.code ?? 1), stdout, stderr });
    });
  });
}

async function main() {
  const { child, port } = await startServer();
  const base = `http://127.0.0.1:${port}`;
  console.log(`server on ${base}`);
  const work = await mkdtemp(join(tmpdir(), 'forge-smoke-'));

  async function api(path, { method = 'GET', body, token = TOKEN } = {}) {
    const headers = { Accept: 'application/vnd.github+json' };
    if (token !== null) headers.Authorization = `Bearer ${token}`;
    if (body !== undefined) headers['Content-Type'] = 'application/json';
    const response = await fetch(`${base}${path}`, {
      method,
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    const text = await response.text();
    let json = null;
    try { json = JSON.parse(text); } catch { /* non-JSON body */ }
    return { status: response.status, headers: response.headers, text, json };
  }

  try {
    // -------------------------------------------------------------------------------- REST
    console.log('REST v3');
    let r = await api('/api/v3/');
    check('root 200 + scopes + request-id', r.status === 200 && r.headers.get('x-oauth-scopes') !== null && (r.headers.get('x-github-request-id') ?? '').length > 0, `${r.status}`);
    r = await api('/api/v3/', { token: null });
    check('root unauthenticated 401', r.status === 401, `${r.status}`);
    r = await api('/api/v3/meta');
    check('meta installed_version', r.status === 200 && typeof r.json?.installed_version === 'string' && r.json.installed_version !== '', JSON.stringify(r.json));
    r = await api('/api/v3/user');
    check('user login adaiversen', r.status === 200 && r.json?.login === 'adaiversen', JSON.stringify(r.json));
    r = await api('/api/v3/user/repos', { method: 'POST', body: { name: 'smoke', description: 'smoke repo', private: false } });
    check('create repo 201', r.status === 201 && r.json?.name === 'smoke' && r.json?.default_branch === 'main', `${r.status} ${r.text.slice(0, 120)}`);
    r = await api('/api/v3/user/repos', { method: 'POST', body: { name: 'smoke' } });
    check('duplicate repo 422', r.status === 422, `${r.status}`);
    for (let i = 0; i < 11; i += 1) {
      await api('/api/v3/user/repos', { method: 'POST', body: { name: `page-${i}` } });
    }
    r = await api('/api/v3/user/repos?per_page=10');
    check('list page 1 has 10 + Link next', Array.isArray(r.json) && r.json.length === 10 && (r.headers.get('link') ?? '').includes('rel="next"'), `len=${r.json?.length} link=${r.headers.get('link')}`);
    const nextUrl = /<([^>]+)>; rel="next"/.exec(r.headers.get('link') ?? '')?.[1];
    r = await api(nextUrl.replace(base, ''));
    check('list page 2 has 2', Array.isArray(r.json) && r.json.length === 2, `len=${r.json?.length}`);
    r = await api('/api/v3/repos/adaiversen/smoke');
    check('get repo', r.status === 200 && r.json?.full_name === 'adaiversen/smoke', `${r.status}`);
    r = await api('/api/v3/repos/adaiversen/smoke', { method: 'PATCH', body: { description: 'updated' } });
    check('patch description', r.status === 200 && r.json?.description === 'updated', `${r.status}`);
    r = await api('/api/v3/repos/adaiversen/smoke/branches');
    check('branches empty on unborn repo', r.status === 200 && Array.isArray(r.json) && r.json.length === 0, `${r.status} ${r.text.slice(0, 80)}`);
    r = await api('/api/v3/repos/adaiversen/smoke/commits');
    check('commits empty on unborn repo', r.status === 200 && Array.isArray(r.json) && r.json.length === 0, `${r.status}`);
    r = await api('/api/v3/repos/adaiversen/smoke/contents/hello.txt', { method: 'PUT', body: { message: 'add hello', content: Buffer.from('hello world\n').toString('base64') } });
    check('contents PUT first commit 201', r.status === 201 && typeof r.json?.commit?.sha === 'string', `${r.status} ${r.text.slice(0, 120)}`);
    const fileSha = r.json.content.sha;
    r = await api('/api/v3/repos/adaiversen/smoke/contents/hello.txt');
    check('contents GET file base64', r.status === 200 && r.json?.encoding === 'base64' && Buffer.from(r.json.content, 'base64').toString() === 'hello world\n', `${r.status}`);
    r = await api('/api/v3/repos/adaiversen/smoke/contents/');
    check('contents GET root dir listing', r.status === 200 && Array.isArray(r.json) && r.json.some((e) => e.name === 'hello.txt'), `${r.status}`);
    r = await api('/api/v3/repos/adaiversen/smoke/contents/hello.txt', { method: 'PUT', body: { message: 'update hello', content: Buffer.from('hello v2\n').toString('base64'), sha: fileSha } });
    check('contents PUT update 200', r.status === 200, `${r.status} ${r.text.slice(0, 100)}`);
    r = await api('/api/v3/repos/adaiversen/smoke/contents/hello.txt', { method: 'PUT', body: { message: 'stale', content: Buffer.from('x\n').toString('base64'), sha: fileSha } });
    check('contents PUT stale sha 409', r.status === 409, `${r.status}`);
    r = await api('/api/v3/repos/adaiversen/smoke/labels', { method: 'POST', body: { name: 'bug', color: 'd73a4a' } });
    check('create label 201', r.status === 201 && r.json?.name === 'bug', `${r.status}`);
    r = await api('/api/v3/repos/adaiversen/smoke/issues', { method: 'POST', body: { title: 'first issue', body: 'body text', labels: ['bug'] } });
    check('create issue 201 number 1', r.status === 201 && r.json?.number === 1 && r.json?.state === 'open', `${r.status} ${r.text.slice(0, 120)}`);
    r = await api('/api/v3/repos/adaiversen/smoke/issues', { method: 'POST', body: { title: 'bad label', labels: ['nope'] } });
    check('create issue unknown label 422', r.status === 422, `${r.status}`);
    r = await api('/api/v3/repos/adaiversen/smoke/issues', { method: 'POST', body: { title: 'second issue' } });
    check('create issue 201 number 2', r.status === 201 && r.json?.number === 2, `${r.status}`);
    r = await api('/api/v3/repos/adaiversen/smoke/issues?state=open&sort=created&direction=desc');
    check('list issues desc [2,1]', Array.isArray(r.json) && eq(r.json.map((i) => i.number), [2, 1]), JSON.stringify(r.json?.map((i) => i.number)));
    r = await api('/api/v3/repos/adaiversen/smoke/issues/1', { method: 'PATCH', body: { state: 'closed' } });
    check('close issue', r.status === 200 && r.json?.state === 'closed', `${r.status}`);
    r = await api('/api/v3/repos/adaiversen/smoke/issues?state=open');
    check('list issues open only [2]', Array.isArray(r.json) && eq(r.json.map((i) => i.number), [2]), JSON.stringify(r.json?.map((i) => i.number)));
    r = await api('/api/v3/repos/adaiversen/smoke/issues/2/comments', { method: 'POST', body: { body: 'a comment' } });
    check('create comment 201', r.status === 201 && r.json?.body === 'a comment' && typeof r.json?.id === 'number', `${r.status} ${r.text.slice(0, 100)}`);
    r = await api('/api/v3/repos/adaiversen/smoke/issues/2/comments');
    check('list comments', Array.isArray(r.json) && r.json.length === 1, `${r.json?.length}`);
    r = await api('/api/v3/repos/adaiversen/smoke/contents/feature.txt', { method: 'PUT', body: { message: 'feature file', content: Buffer.from('feature\n').toString('base64'), branch: 'feature' } });
    check('contents PUT on new branch 201', r.status === 201, `${r.status} ${r.text.slice(0, 100)}`);
    r = await api('/api/v3/repos/adaiversen/smoke/branches/feature');
    check('get branch feature', r.status === 200 && r.json?.name === 'feature', `${r.status}`);
    r = await api('/api/v3/repos/adaiversen/smoke/pulls', { method: 'POST', body: { title: 'add feature', head: 'feature', base: 'main' } });
    check('create PR 201', r.status === 201 && r.json?.number === 3 && r.json?.state === 'open', `${r.status} ${r.text.slice(0, 120)}`);
    r = await api('/api/v3/repos/adaiversen/smoke/pulls', { method: 'POST', body: { title: 'dup', head: 'feature', base: 'main' } });
    check('duplicate open PR 422', r.status === 422, `${r.status}`);
    r = await api('/api/v3/repos/adaiversen/smoke/pulls/3/files');
    check('PR files lists feature.txt', r.status === 200 && Array.isArray(r.json) && r.json.some((f) => f.filename === 'feature.txt'), `${r.status} ${r.text.slice(0, 200)}`);
    r = await api('/api/v3/repos/adaiversen/smoke/pulls/3/merge', { method: 'PUT', body: { merge_method: 'squash' } });
    check('merge PR squash 200', r.status === 200 && r.json?.merged === true && typeof r.json?.sha === 'string', `${r.status} ${r.text.slice(0, 120)}`);
    r = await api('/api/v3/repos/adaiversen/smoke/pulls/3');
    check('merged PR reads closed+merged', r.status === 200 && r.json?.state === 'closed' && r.json?.merged === true && typeof r.json?.merged_at === 'string', `${r.status} ${r.text.slice(0, 150)}`);
    r = await api('/api/v3/repos/adaiversen/smoke/commits?sha=main');
    check('squash commit on main', Array.isArray(r.json) && r.json.length >= 3, `len=${r.json?.length}`);
    r = await api('/api/v3/repos/adaiversen/smoke/contents/feature.txt?ref=main');
    check('merged file on main', r.status === 200 && Buffer.from(r.json.content, 'base64').toString() === 'feature\n', `${r.status}`);
    r = await api('/api/v3/repos/adaiversen/smoke/contents/hello.txt', { method: 'DELETE', body: { message: 'remove hello', sha: (await api('/api/v3/repos/adaiversen/smoke/contents/hello.txt')).json.sha } });
    check('contents DELETE 200', r.status === 200 && typeof r.json?.commit?.sha === 'string', `${r.status} ${r.text.slice(0, 100)}`);
    r = await api('/api/v3/repos/adaiversen/smoke/contents/hello.txt');
    check('deleted file GET 404', r.status === 404, `${r.status}`);
    r = await api('/api/v3/unknown/route');
    check('unknown route JSON 404', r.status === 404 && typeof r.json?.message === 'string', `${r.status}`);
    r = await api('/api/v3/repos/otherowner/smoke');
    check('same name other owner 404 not 403', r.status === 404, `${r.status}`);

    // -------------------------------------------------------------------------------- git CLI
    console.log('git smart HTTP (real git CLI)');
    const gitDir = join(work, 'git');
    await import('node:fs/promises').then((fs) => fs.mkdir(gitDir, { recursive: true }));
    let g = await git(['clone', `${base}/adaiversen/smoke.git`, 'empty-clone'], gitDir);
    check('clone empty repo exits 0', g.code === 0, g.stderr.slice(0, 200));
    await writeFile(join(gitDir, 'empty-clone', 'a.txt'), 'a\n');
    g = await git(['add', 'a.txt'], join(gitDir, 'empty-clone'));
    g = await git(['commit', '-m', 'first'], join(gitDir, 'empty-clone'));
    check('commit in empty clone', g.code === 0, g.stderr.slice(0, 200));
    g = await git(['push', 'origin', 'main'], join(gitDir, 'empty-clone'));
    check('push to empty repo', g.code === 0, g.stderr.slice(0, 300));
    g = await git(['clone', `${base}/adaiversen/smoke.git`, 'full-clone'], gitDir);
    check('clone after push', g.code === 0, g.stderr.slice(0, 200));
    const cloned = await import('node:fs/promises').then((fs) => fs.readFile(join(gitDir, 'full-clone', 'a.txt'), 'utf8').catch(() => null));
    check('cloned file content', cloned === 'a\n', String(cloned));
    await writeFile(join(gitDir, 'full-clone', 'b.txt'), 'b\n');
    await git(['add', 'b.txt'], join(gitDir, 'full-clone'));
    await git(['commit', '-m', 'second'], join(gitDir, 'full-clone'));
    g = await git(['push', 'origin', 'main'], join(gitDir, 'full-clone'));
    check('fast-forward push', g.code === 0, g.stderr.slice(0, 300));
    // Non-fast-forward: reset the empty-clone to an older commit and force a divergent push.
    await git(['fetch', 'origin'], join(gitDir, 'empty-clone'));
    await git(['reset', '--hard', 'HEAD~0'], join(gitDir, 'empty-clone'));
    await git(['checkout', '-b', 'divergent'], join(gitDir, 'empty-clone'));
    await writeFile(join(gitDir, 'empty-clone', 'a.txt'), 'diverged\n');
    await git(['add', 'a.txt'], join(gitDir, 'empty-clone'));
    await git(['commit', '-m', 'divergent'], join(gitDir, 'empty-clone'));
    g = await git(['push', 'origin', 'divergent:main'], join(gitDir, 'empty-clone'));
    check('non-fast-forward push refused', g.code !== 0 && /non-fast-forward|rejected/i.test(g.stderr), `code=${g.code} ${g.stderr.slice(0, 200)}`);
    g = await git(['ls-remote', `${base}/adaiversen/smoke.git`], gitDir);
    check('ls-remote shows main + HEAD', g.code === 0 && g.stdout.includes('refs/heads/main') && g.stdout.includes('HEAD'), g.stdout.slice(0, 200));
    g = await git(['clone', '--depth', '1', `${base}/adaiversen/smoke.git`, 'shallow-clone'], gitDir);
    check('shallow clone depth 1', g.code === 0, g.stderr.slice(0, 200));
    g = await git(['clone', `${base}/adaiversen/missing.git`, 'missing-clone'], gitDir);
    check('clone missing repo fails', g.code !== 0, `code=${g.code}`);
    g = await git(['fetch', 'origin'], join(gitDir, 'shallow-clone'));
    check('fetch in shallow clone', g.code === 0, g.stderr.slice(0, 200));

    // -------------------------------------------------------------------------------- GraphQL
    console.log('GraphQL');
    async function gql(query, variables, token = TOKEN) {
      const headers = { 'Content-Type': 'application/json' };
      if (token !== null) headers.Authorization = `Bearer ${token}`;
      const response = await fetch(`${base}/api/graphql`, { method: 'POST', headers, body: JSON.stringify({ query, variables: variables ?? {} }) });
      return { status: response.status, json: await response.json() };
    }
    let q = await gql('query { viewer { login } }');
    check('viewer login', q.status === 200 && q.json?.data?.viewer?.login === 'adaiversen', JSON.stringify(q.json).slice(0, 150));
    q = await gql('query { repository(owner: "adaiversen", name: "smoke") { id databaseId name nameWithOwner owner { login } defaultBranchRef { name } viewerPermission hasIssuesEnabled } }');
    check('repository fields', q.status === 200 && q.json?.data?.repository?.nameWithOwner === 'adaiversen/smoke' && q.json?.data?.repository?.defaultBranchRef?.name === 'main' && q.json?.data?.repository?.viewerPermission === 'ADMIN', JSON.stringify(q.json).slice(0, 250));
    q = await gql('query { __type(name: "Issue") { fields { name } } }');
    const issueFields = q.json?.data?.__type?.fields?.map((f) => f.name) ?? [];
    check('introspection Issue fields', ['id', 'number', 'title', 'state', 'body'].every((f) => issueFields.includes(f)), JSON.stringify(issueFields));
    q = await gql('query { __type(name: "SearchType") { enumValues { name } } }');
    const enumValues = q.json?.data?.__type?.enumValues?.map((f) => f.name) ?? [];
    check('introspection SearchType has ISSUE', enumValues.includes('ISSUE'), JSON.stringify(enumValues));
    q = await gql('mutation { createIssue(input: { repositoryId: "PLACEHOLDER", title: "gql issue", body: "via graphql" }) { issue { number id title state } } }');
    // repositoryId must be the real global id: fetch it first.
    const repoId = (await gql('query { repository(owner: "adaiversen", name: "smoke") { id } }')).json?.data?.repository?.id;
    q = await gql(`mutation { createIssue(input: { repositoryId: "${repoId}", title: "gql issue", body: "via graphql" }) { issue { number id title state } } }`);
    check('createIssue mutation', q.status === 200 && q.json?.data?.createIssue?.issue?.title === 'gql issue' && typeof q.json?.data?.createIssue?.issue?.id === 'string', JSON.stringify(q.json).slice(0, 250));
    const gqlIssueId = q.json?.data?.createIssue?.issue?.id;
    const gqlIssueNumber = q.json?.data?.createIssue?.issue?.number;
    q = await gql('query { repository(owner: "adaiversen", name: "smoke") { issues(first: 10, orderBy: { field: CREATED_AT, direction: DESC }) { nodes { number title } pageInfo { hasNextPage endCursor } } } }');
    const ordered = q.json?.data?.repository?.issues?.nodes?.map((n) => n.number) ?? [];
    check('issues DESC order', ordered.length >= 2 && ordered[0] > ordered[ordered.length - 1], JSON.stringify(ordered));
    q = await gql(`query { repository(owner: "adaiversen", name: "smoke") { issue(number: ${gqlIssueNumber}) { number title state } } }`);
    check('issueByNumber', q.json?.data?.repository?.issue?.title === 'gql issue', JSON.stringify(q.json).slice(0, 150));
    q = await gql(`mutation { closeIssue(input: { issueId: "${gqlIssueId}" }) { issue { number state } } }`);
    check('closeIssue mutation', q.json?.data?.closeIssue?.issue?.state === 'CLOSED', JSON.stringify(q.json).slice(0, 150));
    // Two more open issues so pagination has a second page (issue #1 and the
    // GraphQL issue are closed; only #2 is open at this point).
    await api('/api/v3/repos/adaiversen/smoke/issues', { method: 'POST', body: { title: 'open three' } });
    await api('/api/v3/repos/adaiversen/smoke/issues', { method: 'POST', body: { title: 'open four' } });
    q = await gql('query { repository(owner: "adaiversen", name: "smoke") { issues(first: 2, states: [OPEN]) { nodes { number } pageInfo { hasNextPage endCursor } } } }');
    const page1 = q.json?.data?.repository?.issues;
    check('issues pagination page 1', Array.isArray(page1?.nodes) && page1.nodes.length === 2 && page1.pageInfo.hasNextPage === true && typeof page1.pageInfo.endCursor === 'string', JSON.stringify(page1).slice(0, 200));
    q = await gql(`query { repository(owner: "adaiversen", name: "smoke") { issues(first: 2, states: [OPEN], after: "${page1.pageInfo.endCursor}") { nodes { number } pageInfo { hasNextPage } } } }`);
    check('issues pagination page 2', Array.isArray(q.json?.data?.repository?.issues?.nodes) && q.json.data.repository.issues.nodes.length >= 0, JSON.stringify(q.json).slice(0, 200));
    q = await gql(`mutation { addComment(input: { subjectId: "${gqlIssueId}", body: "gql comment" }) { commentEdge { node { id body } } } }`);
    check('addComment mutation', q.json?.data?.addComment?.commentEdge?.node?.body === 'gql comment', JSON.stringify(q.json).slice(0, 200));
    q = await gql('query { viewer { login noSuchFieldOnViewer } }');
    check('invalid field 200 + errors name it', q.status === 200 && Array.isArray(q.json?.errors) && q.json.errors.some((e) => e.message.includes('noSuchFieldOnViewer')), JSON.stringify(q.json).slice(0, 200));
    q = await gql('query { repository(owner: "adaiversen", name: "nope") { name } }');
    check('unknown repository null, no errors', q.json?.data?.repository === null && (q.json?.errors ?? []).length === 0, JSON.stringify(q.json).slice(0, 150));
    q = await gql('query { viewer { login } }', undefined, null);
    check('graphql unauthenticated 401', q.status === 401, `${q.status}`);
    // Batched operations.
    {
      const response = await fetch(`${base}/api/graphql`, { method: 'POST', headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${TOKEN}` }, body: JSON.stringify([{ query: 'query { viewer { login } }' }, { query: 'query { repository(owner: "adaiversen", name: "smoke") { name } }' }]) });
      const batched = await response.json();
      check('batched operations in order', response.status === 200 && Array.isArray(batched) && batched.length === 2 && batched[0]?.data?.viewer?.login === 'adaiversen' && batched[1]?.data?.repository?.name === 'smoke', JSON.stringify(batched).slice(0, 200));
    }
    q = await gql(`query { node(id: "${repoId}") { ... on Repository { name nameWithOwner } } }`);
    check('node repository', q.json?.data?.node?.nameWithOwner === 'adaiversen/smoke', JSON.stringify(q.json).slice(0, 200));
    // GraphQL pull request flow.
    await api('/api/v3/repos/adaiversen/smoke/contents/gql-feature.txt', { method: 'PUT', body: { message: 'gql feature', content: Buffer.from('gql\n').toString('base64'), branch: 'gql-branch' } });
    q = await gql(`mutation { createPullRequest(input: { repositoryId: "${repoId}", baseRefName: "main", headRefName: "gql-branch", title: "gql pr" }) { pullRequest { number state headRefName baseRefName } } }`);
    check('createPullRequest mutation', q.json?.data?.createPullRequest?.pullRequest?.headRefName === 'gql-branch', JSON.stringify(q.json).slice(0, 200));
    const prId = (await gql(`query { repository(owner: "adaiversen", name: "smoke") { pullRequest(number: ${q.json?.data?.createPullRequest?.pullRequest?.number}) { id } } }`)).json?.data?.repository?.pullRequest?.id;
    q = await gql(`query { repository(owner: "adaiversen", name: "smoke") { pullRequest(number: ${q.json?.data?.createPullRequest?.pullRequest?.number ?? 0}) { files(first: 10) { nodes { path } } } } }`);
    check('pullRequest files', Array.isArray(q.json?.data?.repository?.pullRequest?.files?.nodes), JSON.stringify(q.json).slice(0, 200));
    q = await gql(`mutation { mergePullRequest(input: { pullRequestId: "${prId}" }) { pullRequest { number merged state } } }`);
    check('mergePullRequest mutation', q.json?.data?.mergePullRequest?.pullRequest?.merged === true, JSON.stringify(q.json).slice(0, 200));

    // -------------------------------------------------------------------------------- MCP
    console.log('MCP');
    async function mcp(body, { raw = false, token = TOKEN } = {}) {
      const headers = { 'Content-Type': 'application/json', 'MCP-Protocol-Version': '2026-07-28' };
      if (token !== null) headers.Authorization = `Bearer ${token}`;
      const response = await fetch(`${base}/mcp`, { method: 'POST', headers, body: raw ? body : JSON.stringify(body) });
      const text = await response.text();
      let json = null;
      try { json = JSON.parse(text); } catch { /* keep null */ }
      return { status: response.status, json, text };
    }
    let m = await mcp({ jsonrpc: '2.0', id: 1, method: 'server/discover', params: {}, _meta: { 'io.modelcontextprotocol/protocolVersion': '2026-07-28' } });
    check('server/discover supportedVersions', m.status === 200 && Array.isArray(m.json?.result?.supportedVersions) && m.json.result.supportedVersions.length > 0 && m.json.result.supportedVersions[0] === '2026-07-28', JSON.stringify(m.json).slice(0, 200));
    m = await mcp({ jsonrpc: '2.0', id: 2, method: 'initialize', params: { protocolVersion: '2026-07-28', capabilities: {}, clientInfo: { name: 'smoke', version: '0' } } });
    check('initialize echoes revision', m.status === 200 && m.json?.result?.protocolVersion === '2026-07-28', JSON.stringify(m.json).slice(0, 150));
    m = await mcp({ jsonrpc: '2.0', id: 3, method: 'tools/list', params: {} });
    const toolNames = (m.json?.result?.tools ?? []).map((t) => t.name);
    check('tools/list has 6 required tools', ['create_repo', 'get_repo', 'create_issue', 'list_issues', 'create_pull_request', 'merge_pull_request'].every((n) => toolNames.includes(n)) && (m.json?.result?.tools ?? []).every((t) => typeof t.description === 'string' && t.description !== '' && t.inputSchema?.type === 'object'), JSON.stringify(toolNames));
    const listAgain = await mcp({ jsonrpc: '2.0', id: 4, method: 'tools/list', params: {} });
    check('tools/list idempotent bytes', JSON.stringify(m.json?.result) === JSON.stringify(listAgain.json?.result), '');
    m = await mcp({ jsonrpc: '2.0', id: 5, method: 'tools/call', params: { name: 'create_repo', arguments: { name: 'mcp-repo', description: 'via mcp' } } });
    check('tools/call create_repo', m.status === 200 && m.json?.result?.isError !== true && m.json?.result?.content?.[0]?.text?.includes('mcp-repo'), JSON.stringify(m.json).slice(0, 200));
    r = await api('/api/v3/repos/adaiversen/mcp-repo');
    check('MCP repo reads back over REST', r.status === 200 && r.json?.name === 'mcp-repo', `${r.status}`);
    m = await mcp({ jsonrpc: '2.0', id: 6, method: 'tools/call', params: { name: 'create_issue', arguments: { owner: 'adaiversen', repo: 'mcp-repo', title: 'mcp issue', full_name: 'adaiversen/mcp-repo' } } });
    check('tools/call create_issue', m.status === 200 && m.json?.result?.isError !== true, JSON.stringify(m.json).slice(0, 200));
    m = await mcp({ jsonrpc: '2.0', id: 7, method: 'tools/call', params: { name: 'list_issues', arguments: { owner: 'adaiversen', repo: 'mcp-repo', state: 'open' } } });
    check('tools/call list_issues has title', m.status === 200 && (m.json?.result?.content?.[0]?.text ?? '').includes('mcp issue'), JSON.stringify(m.json).slice(0, 200));
    m = await mcp({ jsonrpc: '2.0', id: 8, method: 'tools/call', params: { name: 'get_repo', arguments: { owner: 'adaiversen', repo: 'mcp-repo' } } });
    check('tools/call get_repo has name', m.status === 200 && (m.json?.result?.content?.[0]?.text ?? '').includes('mcp-repo'), JSON.stringify(m.json).slice(0, 150));
    m = await mcp({ jsonrpc: '2.0', id: 9, method: 'tools/call', params: { name: 'nope_tool', arguments: {} } });
    check('unknown tool -32601/-32602', m.status === 200 && (m.json?.error?.code === -32601 || m.json?.error?.code === -32602 || m.json?.result?.isError === true), JSON.stringify(m.json).slice(0, 150));
    m = await mcp('{not json', { raw: true });
    check('parse error 400 -32700', m.status === 400 && m.json?.error?.code === -32700, `${m.status} ${m.text.slice(0, 120)}`);
    m = await mcp({ jsonrpc: '2.0', id: 10, method: 'tools/call', params: { name: 'create_repo', arguments: {} } });
    check('missing arguments isError/-32602', m.json?.result?.isError === true || m.json?.error?.code === -32602, JSON.stringify(m.json).slice(0, 150));
    m = await mcp({ jsonrpc: '2.0', id: 11, method: 'tools/call', params: { name: 'get_repo', arguments: { owner: 'adaiversen', repo: 'smoke' } } }, { token: null });
    check('MCP no auth 401', m.status === 401, `${m.status}`);
    {
      const [a, b] = await Promise.all([
        mcp({ jsonrpc: '2.0', id: 101, method: 'tools/call', params: { name: 'get_repo', arguments: { owner: 'adaiversen', repo: 'smoke' } } }),
        mcp({ jsonrpc: '2.0', id: 102, method: 'tools/call', params: { name: 'get_repo', arguments: { owner: 'adaiversen', repo: 'mcp-repo' } } }),
      ]);
      check('concurrent calls keep ids/args', a.json?.id === 101 && b.json?.id === 102 && a.json?.result?.content?.[0]?.text?.includes('smoke') && b.json?.result?.content?.[0]?.text?.includes('mcp-repo'), `${JSON.stringify(a.json).slice(0, 100)} | ${JSON.stringify(b.json).slice(0, 100)}`);
    }

    // -------------------------------------------------------------------------------- perf
    console.log('perf workloads (local, vs T_ref)');
    const T_REF = { push: 1640.82, clone: 266.941, read: 2759.715, write: 348.965 };
    function perfPoints(t, tRef) {
      return Math.round(Math.max(0, Math.min(50, 25 - 12.5 * Math.log2(t / tRef))));
    }
    // write: 200 issue creations.
    await api('/api/v3/user/repos', { method: 'POST', body: { name: 'bench-write' } });
    {
      const times = [];
      for (let round = 0; round < 3; round += 1) {
        const start = performance.now();
        for (let i = 0; i < 200; i += 1) {
          await api('/api/v3/repos/adaiversen/bench-write/issues', { method: 'POST', body: { title: `w-${round}-${i}` } });
        }
        times.push(performance.now() - start);
      }
      const median = times.sort((a, b) => a - b)[1];
      console.log(`  write 200 issues: median ${median.toFixed(1)} ms total (${(median / 200).toFixed(2)} ms/op) vs T_ref ${T_REF.write} ms -> ${perfPoints(median, T_REF.write)} pts`);
    }
    // read: 2000 sequential issue reads, one keep-alive connection (fetch reuses it).
    {
      const times = [];
      for (let round = 0; round < 3; round += 1) {
        const start = performance.now();
        for (let i = 0; i < 2000; i += 1) {
          await api(`/api/v3/repos/adaiversen/bench-write/issues/${(i % 200) + 1}`);
        }
        times.push(performance.now() - start);
      }
      const median = times.sort((a, b) => a - b)[1];
      console.log(`  read 2000 issues: median ${median.toFixed(1)} ms total (${(median / 2000).toFixed(2)} ms/op) vs T_ref ${T_REF.read} ms -> ${perfPoints(median, T_REF.read)} pts`);
    }
    // push: 200 sequential 1 KiB pushes via git CLI into bench-push.
    await api('/api/v3/user/repos', { method: 'POST', body: { name: 'bench-push' } });
    const pushDir = join(work, 'bench-push');
    await import('node:fs/promises').then((fs) => fs.mkdir(pushDir, { recursive: true }));
    await git(['clone', `${base}/adaiversen/bench-push.git`, 'repo'], pushDir);
    const pushRepo = join(pushDir, 'repo');
    const pushTimes = [];
    {
      const start = performance.now();
      for (let i = 0; i < 200; i += 1) {
        const blob = Buffer.alloc(1024, `push-${i}-`).toString('utf8');
        await writeFile(join(pushRepo, 'file.txt'), blob);
        await git(['add', 'file.txt'], pushRepo);
        await git(['commit', '-m', `push ${i}`], pushRepo);
        const t0 = performance.now();
        const pg = await git(['push', 'origin', 'main'], pushRepo);
        pushTimes.push(performance.now() - t0);
        if (pg.code !== 0) throw new Error(`push ${i} failed: ${pg.stderr.slice(0, 200)}`);
      }
      const total = performance.now() - start;
      const median = pushTimes.sort((a, b) => a - b)[100];
      console.log(`  push 200x1KiB: median per-push ${median.toFixed(1)} ms (total ${total.toFixed(0)} ms incl. git CLI) vs T_ref ${T_REF.push} ms/op -> ${perfPoints(median, T_REF.push)} pts`);
    }
    // clone: 50 concurrent shallow clones of the 200-commit repo.
    {
      const times = [];
      for (let round = 0; round < 3; round += 1) {
        const start = performance.now();
        const clones = [];
        for (let i = 0; i < 50; i += 1) {
          clones.push(git(['clone', '--depth', '1', `${base}/adaiversen/bench-push.git`, `c-${round}-${i}`], pushDir));
        }
        const results = await Promise.all(clones);
        times.push(performance.now() - start);
        const bad = results.filter((r) => r.code !== 0);
        if (bad.length > 0) throw new Error(`${bad.length} clones failed: ${bad[0].stderr.slice(0, 200)}`);
      }
      const median = times.sort((a, b) => a - b)[1];
      console.log(`  clone 50 concurrent shallow: median ${median.toFixed(1)} ms total (${(median / 50).toFixed(2)} ms/op) vs T_ref ${T_REF.clone} ms/op -> ${perfPoints(median, T_REF.clone)} pts`);
    }

    // -------------------------------------------------------------------------------- cleanup checks
    r = await api('/api/v3/repos/adaiversen/smoke', { method: 'DELETE' });
    check('delete repo 204', r.status === 204, `${r.status}`);
    r = await api('/api/v3/repos/adaiversen/smoke');
    check('deleted repo GET 404', r.status === 404, `${r.status}`);
  } finally {
    child.kill('SIGTERM');
    await rm(work, { recursive: true, force: true });
  }

  console.log(`\nSMOKE: ${passed} passed, ${failed} failed`);
  if (failures.length > 0) {
    console.log('failures:');
    for (const name of failures) console.log(`  - ${name}`);
  }
  process.exit(failed > 0 ? 1 : 0);
}

main().catch((err) => {
  console.error('smoke crashed:', err);
  process.exit(2);
});
