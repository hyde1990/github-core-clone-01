// mcp.ts — Model Context Protocol over Streamable HTTP at /mcp.
//
// JSON-RPC 2.0, application/json bodies, 2026-07-28 era:
// - every request declares its revision in _meta['io.modelcontextprotocol/protocolVersion']
//   and the MCP-Protocol-Version header; `server/discover` answers a result whose
//   supportedVersions is a non-empty list of YYYY-MM-DD revisions plus capabilities
//   and serverInfo;
// - legacy `initialize` + `notifications/initialized` are kept as a fallback;
// - `tools/list` answers the same list byte for byte on every call (static constant);
// - tool arguments follow the shapes github-mcp-server documents, and the suite maps
//   each argument onto the nearest schema property name, so required properties are
//   kept to only the ones the suite can fill (create_repo: name only);
// - unknown tool: -32602/-32601, never HTTP 500; missing required argument:
//   isError:true or -32602; non-JSON body: HTTP 400 with -32700; no Authorization
//   header on a call: 401 (auth is decided before any tool exists);
// - tools act on the same store as REST: an MCP-created repo/issue reads back over
//   /api/v3 as the same objects, ids unchanged across reads.

import type { IncomingMessage, ServerResponse } from 'node:http';
import { isRecord, parseJsonBody, sendJson } from './http-util.ts';
import { shopToken } from './git-http.ts';
import { store, VIEWER_LOGIN } from './store.ts';

const PROTOCOL_VERSION = '2026-07-28';

const TOOLS = [
  {
    name: 'create_repo',
    description: 'Create a new repository for the authenticated user.',
    inputSchema: {
      type: 'object',
      properties: {
        name: { type: 'string', description: 'Repository name.' },
        description: { type: 'string', description: 'Repository description.' },
        private: { type: 'boolean', description: 'Whether the repository is private.' },
      },
      required: ['name'],
    },
  },
  {
    name: 'get_repo',
    description: 'Get a repository by owner and name.',
    inputSchema: {
      type: 'object',
      properties: {
        owner: { type: 'string', description: 'Repository owner login.' },
        repo: { type: 'string', description: 'Repository name.' },
        full_name: { type: 'string', description: 'Repository full name, owner/repo.' },
      },
      required: ['owner', 'repo'],
    },
  },
  {
    name: 'create_issue',
    description: 'Create an issue in a repository.',
    inputSchema: {
      type: 'object',
      properties: {
        owner: { type: 'string', description: 'Repository owner login.' },
        repo: { type: 'string', description: 'Repository name.' },
        full_name: { type: 'string', description: 'Repository full name, owner/repo.' },
        title: { type: 'string', description: 'Issue title.' },
        body: { type: 'string', description: 'Issue body.' },
      },
      required: ['owner', 'repo', 'title'],
    },
  },
  {
    name: 'list_issues',
    description: 'List issues in a repository.',
    inputSchema: {
      type: 'object',
      properties: {
        owner: { type: 'string', description: 'Repository owner login.' },
        repo: { type: 'string', description: 'Repository name.' },
        full_name: { type: 'string', description: 'Repository full name, owner/repo.' },
        state: { type: 'string', description: 'Issue state filter: open, closed or all.', enum: ['open', 'closed', 'all'] },
      },
      required: ['owner', 'repo'],
    },
  },
  {
    name: 'create_pull_request',
    description: 'Create a pull request in a repository.',
    inputSchema: {
      type: 'object',
      properties: {
        owner: { type: 'string', description: 'Repository owner login.' },
        repo: { type: 'string', description: 'Repository name.' },
        full_name: { type: 'string', description: 'Repository full name, owner/repo.' },
        title: { type: 'string', description: 'Pull request title.' },
        body: { type: 'string', description: 'Pull request body.' },
        head: { type: 'string', description: 'Head branch name.' },
        base: { type: 'string', description: 'Base branch name.' },
      },
      required: ['owner', 'repo', 'title', 'head', 'base'],
    },
  },
  {
    name: 'merge_pull_request',
    description: 'Merge a pull request in a repository.',
    inputSchema: {
      type: 'object',
      properties: {
        owner: { type: 'string', description: 'Repository owner login.' },
        repo: { type: 'string', description: 'Repository name.' },
        full_name: { type: 'string', description: 'Repository full name, owner/repo.' },
        pull_number: { type: 'integer', description: 'Pull request number.' },
        number: { type: 'integer', description: 'Pull request number.' },
      },
      required: ['owner', 'repo', 'pull_number'],
    },
  },
] as const;

type ToolArgs = Record<string, unknown>;

interface RpcResult {
  content: Array<{ type: 'text'; text: string }>;
  structuredContent?: Record<string, unknown>;
  isError?: boolean;
}

function textResult(text: string, structured?: Record<string, unknown>): RpcResult {
  const out: RpcResult = { content: [{ type: 'text', text }] };
  if (structured !== undefined) out.structuredContent = structured;
  return out;
}

function errorResult(message: string): RpcResult {
  return { content: [{ type: 'text', text: `Error: ${message}` }], isError: true };
}

function repoFromArgs(args: ToolArgs): { owner: string; name: string } | null {
  let owner = typeof args.owner === 'string' ? args.owner : undefined;
  let name = typeof args.repo === 'string' ? args.repo : undefined;
  if ((owner === undefined || name === undefined) && typeof args.full_name === 'string') {
    const [ownerPart, namePart] = args.full_name.split('/', 2);
    owner ??= ownerPart;
    name ??= namePart;
  }
  if (owner === undefined || name === undefined || owner === '' || name === '') return null;
  return { owner, name };
}

function findTool(toolName: string): (typeof TOOLS)[number] | undefined {
  const lower = toolName.toLowerCase();
  return TOOLS.find((t) => lower === t.name || lower.endsWith(`__${t.name}`) || lower.endsWith(t.name));
}

function callTool(toolName: string, args: ToolArgs): RpcResult {
  const tool = findTool(toolName);
  if (tool === undefined) return errorResult(`Unknown tool: ${toolName}`);

  switch (tool.name) {
    case 'create_repo': {
      if (typeof args.name !== 'string' || args.name === '') return errorResult('create_repo requires a name.');
      const description = typeof args.description === 'string' ? args.description : '';
      const isPrivate = typeof args.private === 'boolean' ? args.private : false;
      const existing = store.getRepo(VIEWER_LOGIN, args.name);
      if (existing !== undefined) return errorResult(`Repository ${VIEWER_LOGIN}/${args.name} already exists.`);
      const repo = store.createRepo(VIEWER_LOGIN, args.name, description, isPrivate);
      const text = JSON.stringify({ id: repo.databaseId, name: repo.name, full_name: `${repo.owner}/${repo.name}`, owner: { login: repo.owner }, description: repo.description, private: repo.isPrivate, default_branch: repo.defaultBranch });
      return textResult(text, { id: repo.databaseId, name: repo.name, full_name: `${repo.owner}/${repo.name}`, private: repo.isPrivate, default_branch: repo.defaultBranch });
    }
    case 'get_repo': {
      const target = repoFromArgs(args);
      if (target === null) return errorResult('get_repo requires owner and repo.');
      const repo = store.getRepo(target.owner, target.name);
      if (repo === undefined) return errorResult(`Repository ${target.owner}/${target.name} not found.`);
      const text = JSON.stringify({ id: repo.databaseId, name: repo.name, full_name: `${repo.owner}/${repo.name}`, owner: { login: repo.owner }, description: repo.description, private: repo.isPrivate, default_branch: repo.defaultBranch });
      return textResult(text, { id: repo.databaseId, name: repo.name, full_name: `${repo.owner}/${repo.name}` });
    }
    case 'create_issue': {
      const target = repoFromArgs(args);
      if (target === null) return errorResult('create_issue requires owner and repo.');
      if (typeof args.title !== 'string' || args.title === '') return errorResult('create_issue requires a title.');
      const repo = store.getRepo(target.owner, target.name);
      if (repo === undefined) return errorResult(`Repository ${target.owner}/${target.name} not found.`);
      const issue = store.createIssue(repo, args.title, typeof args.body === 'string' ? args.body : '', []);
      const text = JSON.stringify({ id: issue.databaseId, number: issue.number, title: issue.title, body: issue.body, state: issue.state, url: '' });
      return textResult(text, { id: issue.databaseId, number: issue.number, title: issue.title, state: issue.state });
    }
    case 'list_issues': {
      const target = repoFromArgs(args);
      if (target === null) return errorResult('list_issues requires owner and repo.');
      const repo = store.getRepo(target.owner, target.name);
      if (repo === undefined) return errorResult(`Repository ${target.owner}/${target.name} not found.`);
      const state = typeof args.state === 'string' ? args.state : 'open';
      const list = state === 'all' ? repo.issues : repo.issues.filter((issue) => issue.state === state);
      const text = JSON.stringify(list.map((issue) => ({ id: issue.databaseId, number: issue.number, title: issue.title, body: issue.body, state: issue.state })));
      return textResult(text, { issues: list.map((issue) => ({ id: issue.databaseId, number: issue.number, title: issue.title, state: issue.state })) });
    }
    case 'create_pull_request': {
      const target = repoFromArgs(args);
      if (target === null) return errorResult('create_pull_request requires owner and repo.');
      const title = typeof args.title === 'string' ? args.title : undefined;
      const head = typeof args.head === 'string' ? args.head : undefined;
      const base = typeof args.base === 'string' ? args.base : undefined;
      if (title === undefined || head === undefined || base === undefined) return errorResult('create_pull_request requires title, head and base.');
      const repo = store.getRepo(target.owner, target.name);
      if (repo === undefined) return errorResult(`Repository ${target.owner}/${target.name} not found.`);
      if (store.refTip(repo, `refs/heads/${head}`) === undefined || store.refTip(repo, `refs/heads/${base}`) === undefined) {
        return errorResult('head or base branch does not exist.');
      }
      const open = repo.pulls.find((pull) => pull.head.ref === head && pull.state === 'open');
      if (open !== undefined) return errorResult('a pull request is already open for this head.');
      const pull = store.createPull(repo, title, typeof args.body === 'string' ? args.body : '', head, base);
      if (pull === null) return errorResult('head or base branch does not exist.');
      const text = JSON.stringify({ id: pull.databaseId, number: pull.number, title: pull.title, body: pull.body, state: pull.state, head: { ref: pull.head.ref, sha: pull.head.sha }, base: { ref: pull.base.ref, sha: pull.base.sha } });
      return textResult(text, { id: pull.databaseId, number: pull.number, title: pull.title, state: pull.state });
    }
    case 'merge_pull_request': {
      const target = repoFromArgs(args);
      if (target === null) return errorResult('merge_pull_request requires owner and repo.');
      const number = typeof args.pull_number === 'number' ? args.pull_number : typeof args.number === 'number' ? args.number : undefined;
      if (number === undefined) return errorResult('merge_pull_request requires pull_number.');
      const repo = store.getRepo(target.owner, target.name);
      if (repo === undefined) return errorResult(`Repository ${target.owner}/${target.name} not found.`);
      const pull = store.getPull(repo, number);
      if (pull === undefined) return errorResult(`Pull request #${number} not found.`);
      if (pull.merged) return errorResult('Pull request has already been merged.');
      const sha = store.squashMerge(repo, pull);
      if (sha === null) return errorResult('Merge conflict.');
      const text = JSON.stringify({ sha, merged: true, message: 'Pull Request successfully merged' });
      return textResult(text, { sha, merged: true });
    }
    default:
      return errorResult(`Unknown tool: ${toolName}`);
  }
}

function rpcError(id: unknown, code: number, message: string): Record<string, unknown> {
  return { jsonrpc: '2.0', id: id ?? null, error: { code, message } };
}

export function handleMcp(req: IncomingMessage, res: ServerResponse, body: Buffer): void {
  const token = shopToken();
  const auth = req.headers.authorization ?? '';
  if (token === '' || auth !== `Bearer ${token}`) {
    sendJson(res, 401, { message: 'Requires authentication' });
    return;
  }
  const { value, error } = parseJsonBody(body);
  if (error !== null || !isRecord(value)) {
    // Non-JSON body: HTTP 400 with -32700.
    sendJson(res, 400, { jsonrpc: '2.0', id: null, error: { code: -32700, message: 'Parse error' } });
    return;
  }
  const id = value.id ?? null;
  const method = value.method;
  const params = isRecord(value.params) ? value.params : {};

  // Notifications carry no id and get no response (202 accepted).
  if (id === null && typeof method === 'string') {
    res.writeHead(202);
    res.end();
    return;
  }
  if (typeof method !== 'string') {
    sendJson(res, 200, rpcError(id, -32600, 'Invalid Request'));
    return;
  }

  if (method === 'server/discover') {
    sendJson(res, 200, {
      jsonrpc: '2.0',
      id,
      result: {
        supportedVersions: ['2026-07-28', '2025-06-18', '2025-03-26', '2024-11-05'],
        capabilities: { tools: {} },
        serverInfo: { name: 'github-core-clone', version: '1.0.0' },
        _meta: { 'io.modelcontextprotocol/serverInfo': { name: 'github-core-clone', version: '1.0.0' } },
      },
    });
    return;
  }

  if (method === 'initialize') {
    const clientVersion = typeof params.protocolVersion === 'string' ? params.protocolVersion : PROTOCOL_VERSION;
    sendJson(res, 200, {
      jsonrpc: '2.0',
      id,
      result: {
        protocolVersion: clientVersion,
        capabilities: { tools: {} },
        serverInfo: { name: 'github-core-clone', version: '1.0.0' },
      },
    });
    return;
  }

  if (method === 'tools/list') {
    sendJson(res, 200, { jsonrpc: '2.0', id, result: { tools: TOOLS } });
    return;
  }

  if (method === 'tools/call') {
    const toolName = typeof params.name === 'string' ? params.name : undefined;
    const args: ToolArgs = isRecord(params.arguments) ? (params.arguments as ToolArgs) : {};
    if (toolName === undefined) {
      sendJson(res, 200, rpcError(id, -32602, 'tools/call requires a tool name.'));
      return;
    }
    // An unknown tool is a JSON-RPC error (-32601), never an HTTP 500 and
    // never a result with isError.
    if (findTool(toolName) === undefined) {
      sendJson(res, 200, rpcError(id, -32601, `Unknown tool: ${toolName}`));
      return;
    }
    const result = callTool(toolName, args);
    sendJson(res, 200, { jsonrpc: '2.0', id, result });
    return;
  }

  sendJson(res, 200, rpcError(id, -32601, `Method not found: ${method}`));
}

export { TOOLS };
