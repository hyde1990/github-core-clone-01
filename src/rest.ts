// rest.ts — GitHub REST v3 under /api/v3. Repositories, branches, commits and
// contents; issues, comments and labels; pull requests, their files and merges.
// Every reply is application/json with a non-empty X-GitHub-Request-Id; errors
// carry a JSON body with a string `message`; unknown routes are JSON 404s;
// a malformed JSON body is a 400. Auth: the trial's bearer token, or 401.

import type { IncomingMessage, ServerResponse } from 'node:http';
import { isObjectId, mergeBase, parseCommit, parseTree } from './git.ts';
import { isRecord, nextRequestId, parseJsonBody, sendJson } from './http-util.ts';
import { shopToken } from './git-http.ts';
import {
  store,
  VIEWER_EMAIL,
  VIEWER_ID,
  VIEWER_LOGIN,
  VIEWER_NAME,
  type Issue,
  type PullRequest,
  type Repo,
} from './store.ts';

export interface RestContext {
  method: string;
  segments: string[];
  query: URLSearchParams;
  body: Buffer;
  req: IncomingMessage;
  res: ServerResponse;
}

function viewerUser(): Record<string, unknown> {
  return { login: VIEWER_LOGIN, id: VIEWER_ID, type: 'User', name: VIEWER_NAME, email: VIEWER_EMAIL };
}

function repoJson(repo: Repo): Record<string, unknown> {
  const stamp = new Date((1735689600 + repo.databaseId) * 1000).toISOString();
  return {
    id: repo.databaseId,
    node_id: `R_${repo.databaseId}`,
    name: repo.name,
    full_name: `${repo.owner}/${repo.name}`,
    owner: viewerUser(),
    private: repo.isPrivate,
    description: repo.description,
    default_branch: repo.defaultBranch,
    has_issues: true,
    has_pull_requests: true,
    created_at: stamp,
    updated_at: stamp,
    pushed_at: stamp,
    url: `/api/v3/repos/${repo.owner}/${repo.name}`,
    html_url: `https://github.com/${repo.owner}/${repo.name}`,
  };
}

function issueJson(repo: Repo, issue: Issue): Record<string, unknown> {
  return {
    id: issue.id,
    node_id: issue.globalId,
    number: issue.number,
    title: issue.title,
    body: issue.body,
    state: issue.state,
    labels: issue.labels.map((name) => {
      const label = repo.labels.get(name);
      return { name, color: label?.color ?? 'ededed' };
    }),
    user: viewerUser(),
    created_at: issue.createdAt,
    updated_at: issue.updatedAt,
    comments: issue.comments.length,
    url: `/api/v3/repos/${repo.owner}/${repo.name}/issues/${issue.number}`,
    html_url: `https://github.com/${repo.owner}/${repo.name}/issues/${issue.number}`,
  };
}

function pullJson(repo: Repo, pull: PullRequest): Record<string, unknown> {
  const headSha = store.refTip(repo, `refs/heads/${pull.head.ref}`) ?? pull.head.sha;
  const baseSha = store.refTip(repo, `refs/heads/${pull.base.ref}`) ?? pull.base.sha;
  return {
    id: pull.id,
    node_id: pull.globalId,
    number: pull.number,
    title: pull.title,
    body: pull.body,
    state: pull.state,
    merged: pull.merged,
    merged_at: pull.mergedAt,
    merge_commit_sha: pull.mergeCommitSha,
    head: { ref: pull.head.ref, sha: headSha, label: `${repo.owner}:${pull.head.ref}`, user: viewerUser() },
    base: { ref: pull.base.ref, sha: baseSha, label: `${repo.owner}:${pull.base.ref}`, user: viewerUser() },
    user: viewerUser(),
    created_at: pull.createdAt,
    updated_at: pull.updatedAt,
    url: `/api/v3/repos/${repo.owner}/${repo.name}/pulls/${pull.number}`,
    html_url: `https://github.com/${repo.owner}/${repo.name}/pull/${pull.number}`,
  };
}

function authorOf(identity: string): { name: string; email: string } {
  const match = /^(.*?) <([^<>]*)>$/.exec(identity.trim());
  if (match === null) return { name: identity.trim(), email: '' };
  return { name: match[1]!, email: match[2]! };
}

function commitJson(repo: Repo, sha: string): Record<string, unknown> | null {
  const object = repo.objects.get(sha);
  if (object === undefined || object.type !== 'commit') return null;
  const commit = parseCommit(object.data);
  const parseWho = (who: string): { name: string; email: string; date: string } => {
    const parts = who.split(' ');
    const ts = Number(parts[parts.length - 2]);
    const identity = parts.slice(0, -2).join(' ');
    const { name, email } = authorOf(identity);
    return { name, email, date: Number.isFinite(ts) ? new Date(ts * 1000).toISOString() : new Date(0).toISOString() };
  };
  return {
    sha,
    commit: {
      // GitHub returns the message without trailing newlines.
      message: commit.message.replace(/\n+$/, ''),
      author: parseWho(commit.author),
      committer: parseWho(commit.committer),
      tree: { sha: commit.tree },
    },
    author: viewerUser(),
    committer: viewerUser(),
    parents: commit.parents.map((parent) => ({ sha: parent })),
    url: `/api/v3/repos/${repo.owner}/${repo.name}/commits/${sha}`,
  };
}

/** Absolute URL for a Link header, from the request's Host. */
function absoluteUrl(ctx: RestContext, path: string, query: Record<string, string | number>): string {
  const host = ctx.req.headers.host ?? '127.0.0.1';
  const params = new URLSearchParams();
  for (const [key, value] of Object.entries(query)) params.set(key, String(value));
  const suffix = params.toString() === '' ? '' : `?${params.toString()}`;
  return `http://${host}${path}${suffix}`;
}

function linkHeader(ctx: RestContext, path: string, page: number, perPage: number, total: number): string | null {
  const lastPage = Math.max(1, Math.ceil(total / perPage));
  if (lastPage <= 1) return null;
  const links: string[] = [];
  if (page < lastPage) {
    links.push(`<${absoluteUrl(ctx, path, { per_page: perPage, page: page + 1 })}>; rel="next"`);
    links.push(`<${absoluteUrl(ctx, path, { per_page: perPage, page: lastPage })}>; rel="last"`);
  }
  if (page > 1) {
    links.push(`<${absoluteUrl(ctx, path, { per_page: perPage, page: 1 })}>; rel="first"`);
    links.push(`<${absoluteUrl(ctx, path, { per_page: perPage, page: page - 1 })}>; rel="prev"`);
  }
  return links.join(', ');
}

function paginate<T>(items: T[], ctx: RestContext): { page: T[]; link: string | null; total: number } {
  const perPage = Math.max(1, Math.min(100, Number(ctx.query.get('per_page') ?? 30) || 30));
  const page = Math.max(1, Number(ctx.query.get('page') ?? 1) || 1);
  const start = (page - 1) * perPage;
  const path = `/api/v3${ctx.segments.map((s) => `/${encodeURIComponent(s)}`).join('')}`;
  return {
    page: items.slice(start, start + perPage),
    link: linkHeader(ctx, path, page, perPage, items.length),
    total: items.length,
  };
}

function notFound(res: ServerResponse): void {
  sendJson(res, 404, { message: 'Not Found' });
}

function badJson(res: ServerResponse): void {
  sendJson(res, 400, { message: 'Problems parsing JSON' });
}

/** The repo at /repos/{owner}/{name}, or null (with the 404 already sent). */
function resolveRepo(ctx: RestContext, owner: string, name: string): Repo | null {
  if (owner !== VIEWER_LOGIN) {
    notFound(ctx.res);
    return null;
  }
  const repo = store.getRepo(owner, name);
  if (repo === undefined) {
    notFound(ctx.res);
    return null;
  }
  return repo;
}

/** Parse the request JSON body; 400 on malformed input, {} on an empty body. */
function jsonBody(ctx: RestContext): Record<string, unknown> | null {
  if (ctx.body.length === 0) return {};
  const { value, error } = parseJsonBody(ctx.body);
  if (error !== null) {
    badJson(ctx.res);
    return null;
  }
  if (!isRecord(value)) {
    badJson(ctx.res);
    return null;
  }
  return value;
}

function decodePathParam(segment: string): string {
  try {
    return decodeURIComponent(segment);
  } catch {
    return segment;
  }
}

// -------------------------------------------------------------------------------------------
// contents
// -------------------------------------------------------------------------------------------

function contentsFileJson(repo: Repo, path: string, blobOid: string): Record<string, unknown> {
  const object = repo.objects.get(blobOid);
  const data = object?.data ?? Buffer.alloc(0);
  return {
    type: 'file',
    encoding: 'base64',
    size: data.length,
    name: path.split('/').pop() ?? path,
    path,
    content: data.toString('base64'),
    sha: blobOid,
    url: `/api/v3/repos/${repo.owner}/${repo.name}/contents/${path}`,
    html_url: `https://github.com/${repo.owner}/${repo.name}/blob/${repo.defaultBranch}/${path}`,
    download_url: `/api/v3/repos/${repo.owner}/${repo.name}/contents/${path}`,
  };
}

function putContents(ctx: RestContext, repo: Repo, filePath: string): void {
  const body = jsonBody(ctx);
  if (body === null) return;
  const message = typeof body.message === 'string' && body.message !== '' ? body.message : `Update ${filePath}`;
  const contentB64 = typeof body.content === 'string' ? body.content : null;
  if (contentB64 === null) {
    sendJson(ctx.res, 422, { message: 'content is required' });
    return;
  }
  let content: Buffer;
  try {
    content = Buffer.from(contentB64.replace(/[\r\n\t ]+/g, ''), 'base64');
  } catch {
    sendJson(ctx.res, 422, { message: 'content is not valid base64' });
    return;
  }
  const branch = typeof body.branch === 'string' && body.branch !== '' ? body.branch : repo.defaultBranch;
  const sha = typeof body.sha === 'string' ? body.sha : null;
  const refName = `refs/heads/${branch}`;
  let tip = store.refTip(repo, refName);
  if (tip === undefined && branch !== repo.defaultBranch) {
    // A contents write on a new branch creates it from the default branch tip.
    const base = store.headTip(repo);
    if (base !== undefined) {
      repo.refs.set(refName, base);
      tip = base;
    }
  }
  const files = tip === undefined ? new Map() : store.filesAtTip(repo, refName);
  const existing = files.get(filePath);
  if (sha === null) {
    if (existing !== undefined) {
      sendJson(ctx.res, 422, { message: 'file already exists' });
      return;
    }
  } else {
    if (existing === undefined || existing.oid !== sha) {
      sendJson(ctx.res, 409, { message: 'sha does not match the current file' });
      return;
    }
  }
  const blobOid = store.putObject(repo, 'blob', content);
  files.set(filePath, { mode: '100644', oid: blobOid });
  const treeOid = store.buildTreeFromFiles(repo, files);
  const parents = tip === undefined ? [] : [tip];
  const commitOid = store.makeCommit(repo, { tree: treeOid, parents, message: message.endsWith('\n') ? message : `${message}\n` });
  repo.refs.set(refName, commitOid);
  sendJson(
    ctx.res,
    sha === null ? 201 : 200,
    {
      content: { ...contentsFileJson(repo, filePath, blobOid), encoding: 'base64' },
      commit: { sha: commitOid, message },
    },
  );
}

function getContents(ctx: RestContext, repo: Repo, filePath: string): void {
  const ref = ctx.query.get('ref') ?? repo.defaultBranch;
  const tip = store.refTip(repo, `refs/heads/${ref}`);
  if (tip === undefined) {
    // An unknown ref, or an unborn repo: 404.
    notFound(ctx.res);
    return;
  }
  const commit = repo.objects.get(tip);
  if (commit === undefined || commit.type !== 'commit') {
    notFound(ctx.res);
    return;
  }
  const treeOid = parseCommit(commit.data).tree;
  const files = store.flattenTree(repo, treeOid);
  if (filePath === '' || filePath === '/') {
    // Root listing.
    sendJson(ctx.res, 200, directoryListing(repo, treeOid, ''));
    return;
  }
  const file = files.get(filePath);
  if (file !== undefined) {
    sendJson(ctx.res, 200, contentsFileJson(repo, filePath, file.oid));
    return;
  }
  // A directory: find its tree by walking the path.
  const dirTree = treeAtPath(repo, treeOid, filePath);
  if (dirTree !== null) {
    sendJson(ctx.res, 200, directoryListing(repo, dirTree, filePath));
    return;
  }
  notFound(ctx.res);
}

function treeAtPath(repo: Repo, treeOid: string, path: string): string | null {
  let current = treeOid;
  for (const segment of path.split('/')) {
    const object = repo.objects.get(current);
    if (object === undefined || object.type !== 'tree') return null;
    const entry = parseTree(object.data).find((candidate) => candidate.name === segment);
    if (entry === undefined || entry.mode !== '40000') return null;
    current = entry.oid;
  }
  return current;
}

function directoryListing(repo: Repo, treeOid: string, dirPath: string): Array<Record<string, unknown>> {
  const object = repo.objects.get(treeOid);
  if (object === undefined || object.type !== 'tree') return [];
  const entries = parseTree(object.data);
  return entries.map((entry) => {
    const path = dirPath === '' ? entry.name : `${dirPath}/${entry.name}`;
    if (entry.mode === '40000') {
      return { type: 'dir', name: entry.name, path, sha: entry.oid, size: 0 };
    }
    const blob = repo.objects.get(entry.oid);
    return { type: 'file', name: entry.name, path, sha: entry.oid, size: blob?.data.length ?? 0 };
  });
}

function deleteContents(ctx: RestContext, repo: Repo, filePath: string): void {
  const body = jsonBody(ctx);
  if (body === null) return;
  const message = typeof body.message === 'string' && body.message !== '' ? body.message : `Delete ${filePath}`;
  const sha = typeof body.sha === 'string' ? body.sha : null;
  const branch = typeof body.branch === 'string' && body.branch !== '' ? body.branch : repo.defaultBranch;
  const refName = `refs/heads/${branch}`;
  const tip = store.refTip(repo, refName);
  if (tip === undefined) {
    notFound(ctx.res);
    return;
  }
  const files = store.filesAtTip(repo, refName);
  const existing = files.get(filePath);
  if (existing === undefined) {
    notFound(ctx.res);
    return;
  }
  if (sha !== null && existing.oid !== sha) {
    sendJson(ctx.res, 409, { message: 'sha does not match the current file' });
    return;
  }
  files.delete(filePath);
  const treeOid = store.buildTreeFromFiles(repo, files);
  const commitOid = store.makeCommit(repo, { tree: treeOid, parents: [tip], message: message.endsWith('\n') ? message : `${message}\n` });
  repo.refs.set(refName, commitOid);
  sendJson(ctx.res, 200, { commit: { sha: commitOid, message } });
}

// -------------------------------------------------------------------------------------------
// router
// -------------------------------------------------------------------------------------------

export function handleRest(ctx: RestContext): void {
  const { method, segments } = ctx;
  const token = shopToken();
  const auth = ctx.req.headers.authorization ?? '';
  if (token === '' || auth !== `Bearer ${token}`) {
    sendJson(ctx.res, 401, { message: 'Requires authentication' });
    return;
  }

  // GET /api/v3/ — the API root.
  if (segments.length === 0) {
    if (method !== 'GET') {
      notFound(ctx.res);
      return;
    }
    sendJson(
      ctx.res,
      200,
      { current_user_url: '/api/v3/user', rate_limit_url: '/api/v3/rate_limit' },
      { 'x-oauth-scopes': 'repo, read:org' },
    );
    return;
  }

  // GET /api/v3/meta
  if (segments.length === 1 && segments[0] === 'meta') {
    if (method !== 'GET') {
      notFound(ctx.res);
      return;
    }
    sendJson(ctx.res, 200, { installed_version: '1.0.0' });
    return;
  }

  // GET /api/v3/user
  if (segments.length === 1 && segments[0] === 'user') {
    if (method !== 'GET') {
      notFound(ctx.res);
      return;
    }
    sendJson(ctx.res, 200, { ...viewerUser(), html_url: `https://github.com/${VIEWER_LOGIN}` });
    return;
  }

  // POST/GET /api/v3/user/repos
  if (segments.length === 2 && segments[0] === 'user' && segments[1] === 'repos') {
    if (method === 'POST') {
      const body = jsonBody(ctx);
      if (body === null) return;
      const name = typeof body.name === 'string' ? body.name : '';
      if (name === '' || !/^[A-Za-z0-9._-]+$/.test(name)) {
        sendJson(ctx.res, 422, { message: 'name is invalid' });
        return;
      }
      const repo = store.createRepo(VIEWER_LOGIN, name, {
        isPrivate: body.private === true,
        description: typeof body.description === 'string' ? body.description : '',
      });
      if (repo === null) {
        sendJson(ctx.res, 422, { message: 'Repository already exists' });
        return;
      }
      sendJson(ctx.res, 201, repoJson(repo));
      return;
    }
    if (method === 'GET') {
      const all = store.listRepos(VIEWER_LOGIN);
      const { page, link } = paginate(all, ctx);
      sendJson(ctx.res, 200, page.map((repo) => repoJson(repo)), link === null ? {} : { link });
      return;
    }
    notFound(ctx.res);
    return;
  }

  // /api/v3/repos/{owner}/{name}[...]
  if (segments.length >= 3 && segments[0] === 'repos') {
    const owner = decodePathParam(segments[1]!);
    const name = decodePathParam(segments[2]!);
    const rest = segments.slice(3);
    const repo = resolveRepo(ctx, owner, name);
    if (repo === null) return;

    // GET/PATCH/DELETE /repos/{o}/{r}
    if (rest.length === 0) {
      if (method === 'GET') {
        sendJson(ctx.res, 200, repoJson(repo));
        return;
      }
      if (method === 'PATCH') {
        const body = jsonBody(ctx);
        if (body === null) return;
        if (typeof body.description === 'string') repo.description = body.description;
        if (typeof body.private === 'boolean') repo.isPrivate = body.private;
        sendJson(ctx.res, 200, repoJson(repo));
        return;
      }
      if (method === 'DELETE') {
        store.deleteRepo(owner, name);
        ctx.res.writeHead(204, { 'x-github-request-id': nextRequestId() });
        ctx.res.end();
        return;
      }
      notFound(ctx.res);
      return;
    }

    const head = rest[0]!;

    // GET /repos/{o}/{r}/branches and /branches/{branch}
    if (head === 'branches') {
      if (rest.length === 1 && method === 'GET') {
        const branches = [...repo.refs.keys()]
          .filter((ref) => ref.startsWith('refs/heads/'))
          .sort()
          .map((ref) => ({ name: ref.slice('refs/heads/'.length), commit: { sha: repo.refs.get(ref)! }, protected: false }));
        sendJson(ctx.res, 200, branches);
        return;
      }
      if (rest.length === 2 && method === 'GET') {
        const branch = decodePathParam(rest[1]!);
        const sha = repo.refs.get(`refs/heads/${branch}`);
        if (sha === undefined) {
          notFound(ctx.res);
          return;
        }
        sendJson(ctx.res, 200, { name: branch, commit: { sha }, protected: false });
        return;
      }
      notFound(ctx.res);
      return;
    }

    // POST /repos/{o}/{r}/git/refs ; GET /repos/{o}/{r}/git/refs/heads/{branch}
    if (head === 'git' && rest[1] === 'refs') {
      if (method === 'POST' && rest.length === 2) {
        const body = jsonBody(ctx);
        if (body === null) return;
        const ref = typeof body.ref === 'string' ? body.ref : '';
        const sha = typeof body.sha === 'string' ? body.sha : '';
        if (!ref.startsWith('refs/') || !isObjectId(sha)) {
          sendJson(ctx.res, 422, { message: 'ref or sha is invalid' });
          return;
        }
        const object = repo.objects.get(sha);
        if (object === undefined || object.type !== 'commit') {
          sendJson(ctx.res, 422, { message: 'sha is not a commit' });
          return;
        }
        if (repo.refs.has(ref)) {
          sendJson(ctx.res, 422, { message: 'Reference already exists' });
          return;
        }
        repo.refs.set(ref, sha);
        sendJson(ctx.res, 201, { ref, object: { sha, type: 'commit' } });
        return;
      }
      if (method === 'GET' && rest.length === 4 && rest[2] === 'heads') {
        const branch = decodePathParam(rest[3]!);
        const sha = repo.refs.get(`refs/heads/${branch}`);
        if (sha === undefined) {
          notFound(ctx.res);
          return;
        }
        sendJson(ctx.res, 200, { ref: `refs/heads/${branch}`, object: { sha, type: 'commit' } });
        return;
      }
      notFound(ctx.res);
      return;
    }

    // GET /repos/{o}/{r}/commits[?sha=]
    if (head === 'commits') {
      if (method !== 'GET' || rest.length !== 1) {
        notFound(ctx.res);
        return;
      }
      const shaParam = ctx.query.get('sha');
      let tip: string | undefined;
      if (shaParam === null) {
        tip = store.headTip(repo);
      } else if (isObjectId(shaParam)) {
        tip = shaParam;
      } else {
        tip = store.refTip(repo, `refs/heads/${shaParam}`) ?? store.refTip(repo, shaParam);
      }
      if (tip === undefined || !repo.objects.has(tip)) {
        if (shaParam !== null) {
          notFound(ctx.res);
          return;
        }
        sendJson(ctx.res, 200, []);
        return;
      }
      const commits = store.commitsFrom(repo, tip);
      const out: Array<Record<string, unknown>> = [];
      for (const sha of commits) {
        const json = commitJson(repo, sha);
        if (json !== null) out.push(json);
      }
      sendJson(ctx.res, 200, out);
      return;
    }

    // /repos/{o}/{r}/contents/{path...}
    if (head === 'contents') {
      const filePath = rest.slice(1).map(decodePathParam).join('/');
      if (filePath.includes('..')) {
        sendJson(ctx.res, 422, { message: 'path is invalid' });
        return;
      }
      if (method === 'PUT') {
        putContents(ctx, repo, filePath);
        return;
      }
      if (method === 'GET') {
        getContents(ctx, repo, filePath);
        return;
      }
      if (method === 'DELETE') {
        deleteContents(ctx, repo, filePath);
        return;
      }
      notFound(ctx.res);
      return;
    }

    // POST /repos/{o}/{r}/labels
    if (head === 'labels' && rest.length === 1) {
      if (method !== 'POST') {
        notFound(ctx.res);
        return;
      }
      const body = jsonBody(ctx);
      if (body === null) return;
      const name = typeof body.name === 'string' ? body.name : '';
      if (name === '') {
        sendJson(ctx.res, 422, { message: 'name is required' });
        return;
      }
      const color = typeof body.color === 'string' && body.color !== '' ? body.color : 'ededed';
      repo.labels.set(name, { name, color });
      sendJson(ctx.res, 201, { name, color });
      return;
    }

    // /repos/{o}/{r}/issues[/{n}[/comments]]
    if (head === 'issues') {
      if (rest.length === 1) {
        if (method === 'POST') {
          const body = jsonBody(ctx);
          if (body === null) return;
          const title = typeof body.title === 'string' ? body.title : '';
          if (title === '') {
            sendJson(ctx.res, 422, { message: 'title is required' });
            return;
          }
          const labels = Array.isArray(body.labels) ? body.labels.filter((l): l is string => typeof l === 'string') : [];
          for (const label of labels) {
            if (!repo.labels.has(label)) {
              sendJson(ctx.res, 422, { message: `label ${label} does not exist` });
              return;
            }
          }
          const issue = store.createIssue(repo, title, typeof body.body === 'string' ? body.body : '', labels);
          sendJson(ctx.res, 201, issueJson(repo, issue));
          return;
        }
        if (method === 'GET') {
          const state = ctx.query.get('state') ?? 'open';
          const sort = ctx.query.get('sort') ?? 'created';
          const direction = ctx.query.get('direction') ?? 'desc';
          let list = [...repo.issues];
          if (state !== 'all') list = list.filter((issue) => issue.state === state);
          if (sort === 'created') {
            list.sort((a, b) => (direction === 'asc' ? a.seq - b.seq : b.seq - a.seq));
          }
          const { page, link } = paginate(list, ctx);
          sendJson(ctx.res, 200, page.map((issue) => issueJson(repo, issue)), link === null ? {} : { link });
          return;
        }
        notFound(ctx.res);
        return;
      }
      const number = Number(rest[1]);
      if (!Number.isInteger(number) || number <= 0) {
        notFound(ctx.res);
        return;
      }
      const issue = store.getIssue(repo, number);
      if (rest.length === 2) {
        if (issue === undefined) {
          notFound(ctx.res);
          return;
        }
        if (method === 'GET') {
          sendJson(ctx.res, 200, issueJson(repo, issue));
          return;
        }
        if (method === 'PATCH') {
          const body = jsonBody(ctx);
          if (body === null) return;
          if (body.state === 'open' || body.state === 'closed') {
            issue.state = body.state;
            issue.updatedAt = store.tick().iso;
          }
          if (typeof body.title === 'string') issue.title = body.title;
          if (typeof body.body === 'string') issue.body = body.body;
          sendJson(ctx.res, 200, issueJson(repo, issue));
          return;
        }
        notFound(ctx.res);
        return;
      }
      if (rest.length === 3 && rest[2] === 'comments') {
        if (issue === undefined) {
          notFound(ctx.res);
          return;
        }
        if (method === 'POST') {
          const body = jsonBody(ctx);
          if (body === null) return;
          const text = typeof body.body === 'string' ? body.body : '';
          const comment = store.addComment(repo, issue, text);
          sendJson(ctx.res, 201, {
            id: comment.id,
            node_id: comment.globalId,
            body: comment.body,
            user: viewerUser(),
            created_at: comment.createdAt,
            updated_at: comment.createdAt,
          });
          return;
        }
        if (method === 'GET') {
          sendJson(
            ctx.res,
            200,
            issue.comments.map((comment) => ({
              id: comment.id,
              node_id: comment.globalId,
              body: comment.body,
              user: viewerUser(),
              created_at: comment.createdAt,
              updated_at: comment.createdAt,
            })),
          );
          return;
        }
        notFound(ctx.res);
        return;
      }
      notFound(ctx.res);
      return;
    }

    // /repos/{o}/{r}/pulls[/{n}[/files|/merge]]
    if (head === 'pulls') {
      if (rest.length === 1) {
        if (method === 'POST') {
          const body = jsonBody(ctx);
          if (body === null) return;
          const title = typeof body.title === 'string' ? body.title : '';
          const headRef = typeof body.head === 'string' ? body.head : '';
          const baseRef = typeof body.base === 'string' ? body.base : repo.defaultBranch;
          if (title === '' || headRef === '') {
            sendJson(ctx.res, 422, { message: 'title and head are required' });
            return;
          }
          if (store.refTip(repo, `refs/heads/${headRef}`) === undefined || store.refTip(repo, `refs/heads/${baseRef}`) === undefined) {
            sendJson(ctx.res, 422, { message: 'head or base branch does not exist' });
            return;
          }
          const open = repo.pulls.find((pull) => pull.head.ref === headRef && pull.state === 'open');
          if (open !== undefined) {
            sendJson(ctx.res, 422, { message: 'a pull request is already open for this head' });
            return;
          }
          const pull = store.createPull(repo, title, typeof body.body === 'string' ? body.body : '', headRef, baseRef);
          if (pull === null) {
            sendJson(ctx.res, 422, { message: 'head or base branch does not exist' });
            return;
          }
          sendJson(ctx.res, 201, pullJson(repo, pull));
          return;
        }
        if (method === 'GET') {
          const state = ctx.query.get('state') ?? 'open';
          let list = [...repo.pulls];
          if (state !== 'all') list = list.filter((pull) => pull.state === state);
          list.sort((a, b) => b.seq - a.seq);
          const { page, link } = paginate(list, ctx);
          sendJson(ctx.res, 200, page.map((pull) => pullJson(repo, pull)), link === null ? {} : { link });
          return;
        }
        notFound(ctx.res);
        return;
      }
      const number = Number(rest[1]);
      if (!Number.isInteger(number) || number <= 0) {
        notFound(ctx.res);
        return;
      }
      const pull = store.getPull(repo, number);
      if (rest.length === 2) {
        if (pull === undefined) {
          notFound(ctx.res);
          return;
        }
        if (method === 'GET') {
          sendJson(ctx.res, 200, pullJson(repo, pull));
          return;
        }
        notFound(ctx.res);
        return;
      }
      if (rest.length === 3 && rest[2] === 'files') {
        if (pull === undefined || method !== 'GET') {
          notFound(ctx.res);
          return;
        }
        const baseTip = store.refTip(repo, `refs/heads/${pull.base.ref}`);
        const headTip = store.refTip(repo, `refs/heads/${pull.head.ref}`);
        if (baseTip === undefined || headTip === undefined) {
          notFound(ctx.res);
          return;
        }
        // Diff merge-base -> head tip.
        const mb = mergeBase(store.lookup(repo), baseTip, headTip) ?? baseTip;
        const files = store.diffFiles(repo, mb, headTip);
        sendJson(
          ctx.res,
          200,
          files.map((file) => ({
            filename: file.path,
            status: file.status,
            additions: file.additions,
            deletions: file.deletions,
            changes: file.additions + file.deletions,
          })),
        );
        return;
      }
      if (rest.length === 3 && rest[2] === 'merge') {
        if (method !== 'PUT') {
          notFound(ctx.res);
          return;
        }
        if (pull === undefined) {
          notFound(ctx.res);
          return;
        }
        if (pull.merged) {
          sendJson(ctx.res, 405, { message: 'Pull request has already been merged' });
          return;
        }
        if (pull.state !== 'open') {
          sendJson(ctx.res, 405, { message: 'Pull request is closed' });
          return;
        }
        const body = jsonBody(ctx);
        if (body === null) return;
        const mergedSha = store.squashMerge(repo, pull);
        if (mergedSha === null) {
          sendJson(ctx.res, 405, { message: 'Merge conflict' });
          return;
        }
        sendJson(ctx.res, 200, {
          sha: mergedSha,
          merged: true,
          message: `Merge pull request #${pull.number} from ${repo.owner}/${pull.head.ref}`,
        });
        return;
      }
      notFound(ctx.res);
      return;
    }
  }

  notFound(ctx.res);
}
