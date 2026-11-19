// graphql.ts — GitHub GraphQL v4 for the documents gh and github-mcp-server
// actually send. A small recursive-descent parser (operations, fields, aliases,
// arguments with variables/inline literals/input objects/lists/enums, inline
// fragments) plus an executor over the same store as REST. No schema library.
//
// Rules the suite rests on:
// - an unknown field is HTTP 200 with an `errors` array naming it, never an
//   HTTP error; an unknown repository is `data.repository = null`, no errors;
// - an unauthenticated request is exactly 401;
// - two operation documents in one body (a JSON array) answer two results in order.

import type { IncomingMessage, ServerResponse } from 'node:http';
import { mergeBase } from './git.ts';
import { isRecord, parseJsonBody, sendJson } from './http-util.ts';
import { shopToken } from './git-http.ts';
import {
  decodeGlobalId,
  store,
  VIEWER_LOGIN,
  type Issue,
  type PullRequest,
  type Repo,
} from './store.ts';

// -------------------------------------------------------------------------------------------
// parser
// -------------------------------------------------------------------------------------------

type Value =
  | { kind: 'variable'; name: string }
  | { kind: 'string'; value: string }
  | { kind: 'int'; value: number }
  | { kind: 'float'; value: number }
  | { kind: 'boolean'; value: boolean }
  | { kind: 'null' }
  | { kind: 'enum'; value: string }
  | { kind: 'list'; items: Value[] }
  | { kind: 'object'; fields: Record<string, Value> };

interface FieldNode {
  alias: string | null;
  name: string;
  args: Record<string, Value>;
  selections: SelectionNode[];
}

type SelectionNode =
  | { kind: 'field'; field: FieldNode }
  | { kind: 'inlineFragment'; type: string | null; selections: SelectionNode[] };

interface OperationNode {
  operation: 'query' | 'mutation';
  name: string | null;
  variableDefinitions: Record<string, Value>;
  selections: SelectionNode[];
}

class GraphQLSyntaxError extends Error {}

class Parser {
  private pos = 0;
  private readonly text: string;

  constructor(text: string) {
    this.text = text;
  }

  parse(): OperationNode[] {
    const operations: OperationNode[] = [];
    this.skipIgnored();
    while (this.pos < this.text.length) {
      operations.push(this.parseOperation());
      this.skipIgnored();
    }
    if (operations.length === 0) throw new GraphQLSyntaxError('empty document');
    return operations;
  }

  private peek(): string {
    return this.text[this.pos] ?? '';
  }

  private skipIgnored(): void {
    for (;;) {
      while (this.pos < this.text.length && /[\s,]/u.test(this.text[this.pos]!)) this.pos += 1;
      if (this.text[this.pos] === '#') {
        while (this.pos < this.text.length && this.text[this.pos] !== '\n') this.pos += 1;
        continue;
      }
      break;
    }
  }

  private expect(char: string): void {
    this.skipIgnored();
    if (this.text[this.pos] !== char) throw new GraphQLSyntaxError(`expected ${char} at ${this.pos}`);
    this.pos += 1;
  }

  private readName(): string {
    this.skipIgnored();
    const match = /^[A-Za-z_][A-Za-z0-9_]*/u.exec(this.text.slice(this.pos));
    if (match === null) throw new GraphQLSyntaxError(`expected a name at ${this.pos}`);
    this.pos += match[0].length;
    return match[0];
  }

  private parseOperation(): OperationNode {
    this.skipIgnored();
    let operation: 'query' | 'mutation' = 'query';
    let name: string | null = null;
    if (this.peek() === '{') {
      return { operation: 'query', name: null, variableDefinitions: {}, selections: this.parseSelectionSet() };
    }
    const keyword = this.readName();
    if (keyword !== 'query' && keyword !== 'mutation') throw new GraphQLSyntaxError(`unsupported operation ${keyword}`);
    operation = keyword;
    this.skipIgnored();
    if (/[A-Za-z_]/.test(this.peek())) name = this.readName();
    const variableDefinitions: Record<string, Value> = {};
    this.skipIgnored();
    if (this.peek() === '(') {
      this.pos += 1;
      this.skipIgnored();
      while (this.peek() !== ')') {
        this.expect('$');
        const varName = this.readName();
        this.expect(':');
        // Type: a name, possibly wrapped in [] or suffixed with !. Value ignored.
        this.skipIgnored();
        if (this.peek() === '[') {
          this.pos += 1;
          this.readName();
          this.expect(']');
        } else {
          this.readName();
        }
        this.skipIgnored();
        if (this.peek() === '!') this.pos += 1;
        this.skipIgnored();
        if (this.peek() === '=') {
          this.pos += 1;
          variableDefinitions[varName] = this.parseValue();
        } else {
          variableDefinitions[varName] = { kind: 'null' };
        }
        this.skipIgnored();
      }
      this.expect(')');
    }
    return { operation, name, variableDefinitions, selections: this.parseSelectionSet() };
  }

  private parseSelectionSet(): SelectionNode[] {
    this.expect('{');
    const selections: SelectionNode[] = [];
    for (;;) {
      this.skipIgnored();
      if (this.peek() === '}') {
        this.pos += 1;
        return selections;
      }
      if (this.pos >= this.text.length) throw new GraphQLSyntaxError('unterminated selection set');
      selections.push(this.parseSelection());
    }
  }

  private parseSelection(): SelectionNode {
    this.skipIgnored();
    if (this.text.startsWith('...', this.pos)) {
      this.pos += 3;
      this.skipIgnored();
      if (this.text.startsWith('on', this.pos) && !/[A-Za-z0-9_]/.test(this.text[this.pos + 2] ?? '')) {
        this.pos += 2;
        const type = this.readName();
        return { kind: 'inlineFragment', type, selections: this.parseSelectionSet() };
      }
      // A named fragment spread: unsupported; the suite never sends one.
      const name = this.readName();
      throw new GraphQLSyntaxError(`unsupported fragment spread ...${name}`);
    }
    let alias: string | null = null;
    let name = this.readName();
    this.skipIgnored();
    if (this.peek() === ':') {
      this.pos += 1;
      alias = name;
      name = this.readName();
    }
    const args: Record<string, Value> = {};
    this.skipIgnored();
    if (this.peek() === '(') {
      this.pos += 1;
      this.skipIgnored();
      while (this.peek() !== ')') {
        const argName = this.readName();
        this.expect(':');
        args[argName] = this.parseValue();
        this.skipIgnored();
      }
      this.expect(')');
    }
    // Directives: skip @name(args) if present.
    this.skipIgnored();
    while (this.peek() === '@') {
      this.pos += 1;
      this.readName();
      this.skipIgnored();
      if (this.peek() === '(') {
        let depth = 0;
        while (this.pos < this.text.length) {
          const char = this.text[this.pos]!;
          this.pos += 1;
          if (char === '(') depth += 1;
          else if (char === ')') {
            depth -= 1;
            if (depth === 0) break;
          } else if (char === '"') {
            while (this.pos < this.text.length && this.text[this.pos] !== '"') {
              if (this.text[this.pos] === '\\') this.pos += 1;
              this.pos += 1;
            }
            this.pos += 1;
          }
        }
      }
      this.skipIgnored();
    }
    let selections: SelectionNode[] = [];
    this.skipIgnored();
    if (this.peek() === '{') selections = this.parseSelectionSet();
    return { kind: 'field', field: { alias, name, args, selections } };
  }

  private parseValue(): Value {
    this.skipIgnored();
    const char = this.peek();
    if (char === '$') {
      this.pos += 1;
      return { kind: 'variable', name: this.readName() };
    }
    if (char === '"') return { kind: 'string', value: this.parseString() };
    if (char === '[') {
      this.pos += 1;
      const items: Value[] = [];
      this.skipIgnored();
      while (this.peek() !== ']') {
        items.push(this.parseValue());
        this.skipIgnored();
      }
      this.pos += 1;
      return { kind: 'list', items };
    }
    if (char === '{') {
      this.pos += 1;
      const fields: Record<string, Value> = {};
      this.skipIgnored();
      while (this.peek() !== '}') {
        const key = this.readName();
        this.expect(':');
        fields[key] = this.parseValue();
        this.skipIgnored();
      }
      this.pos += 1;
      return { kind: 'object', fields };
    }
    if (char === '-' || /[0-9]/.test(char)) {
      const match = /^-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?/u.exec(this.text.slice(this.pos));
      if (match === null) throw new GraphQLSyntaxError(`bad number at ${this.pos}`);
      this.pos += match[0].length;
      const num = Number(match[0]);
      return match[0].includes('.') || /[eE]/.test(match[0]) ? { kind: 'float', value: num } : { kind: 'int', value: num };
    }
    const name = this.readName();
    if (name === 'true') return { kind: 'boolean', value: true };
    if (name === 'false') return { kind: 'boolean', value: false };
    if (name === 'null') return { kind: 'null' };
    return { kind: 'enum', value: name };
  }

  private parseString(): string {
    this.expect('"');
    let out = '';
    for (;;) {
      if (this.pos >= this.text.length) throw new GraphQLSyntaxError('unterminated string');
      const char = this.text[this.pos]!;
      this.pos += 1;
      if (char === '"') return out;
      if (char === '\\') {
        const escape = this.text[this.pos]!;
        this.pos += 1;
        if (escape === 'n') out += '\n';
        else if (escape === 't') out += '\t';
        else if (escape === 'r') out += '\r';
        else if (escape === '"') out += '"';
        else if (escape === '\\') out += '\\';
        else if (escape === '/') out += '/';
        else if (escape === 'u') {
          out += String.fromCharCode(Number.parseInt(this.text.slice(this.pos, this.pos + 4), 16));
          this.pos += 4;
        } else out += escape;
      } else {
        out += char;
      }
    }
  }
}

// -------------------------------------------------------------------------------------------
// executor
// -------------------------------------------------------------------------------------------

class GraphQLError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'GraphQLError';
  }
}

type Variables = Record<string, unknown>;

function resolveValue(value: Value, variables: Variables): unknown {
  switch (value.kind) {
    case 'variable':
      return variables[value.name] ?? null;
    case 'string':
    case 'int':
    case 'float':
    case 'boolean':
      return value.value;
    case 'null':
      return null;
    case 'enum':
      return value.value;
    case 'list':
      return value.items.map((item) => resolveValue(item, variables));
    case 'object': {
      const out: Record<string, unknown> = {};
      for (const [key, item] of Object.entries(value.fields)) out[key] = resolveValue(item, variables);
      return out;
    }
  }
}

function argValue(field: FieldNode, name: string, variables: Variables): unknown {
  const raw = field.args[name];
  if (raw === undefined) return undefined;
  return resolveValue(raw, variables);
}

const ISSUE_FIELDS = ['id', 'number', 'title', 'state', 'body', 'createdAt', 'updatedAt', 'author', 'labels', 'comments', 'url', 'repository'];
const PULL_REQUEST_FIELDS = [
  'id', 'number', 'title', 'body', 'state', 'merged', 'mergeable', 'headRefName', 'baseRefName',
  'headRefOid', 'baseRefOid', 'files', 'commits', 'author', 'createdAt', 'updatedAt', 'url', 'repository',
];
const REPOSITORY_FIELDS = [
  'id', 'databaseId', 'name', 'nameWithOwner', 'owner', 'defaultBranchRef', 'viewerPermission',
  'hasIssuesEnabled', 'issue', 'issues', 'pullRequests', 'pullRequest', 'description', 'private',
  'createdAt', 'updatedAt', 'url',
];
const SEARCH_TYPE_VALUES = ['ISSUE', 'PULL_REQUEST', 'REPOSITORY', 'USER', 'DISCUSSION', 'COMMIT', 'CODE', 'TOPIC', 'PACKAGE', 'MILESTONE', 'WIKI'];

function b64url(text: string): string {
  return Buffer.from(text, 'utf8').toString('base64url');
}

function issueCursor(number: number): string {
  return b64url(`issue:${number}`);
}

function cursorNumber(cursor: string): number | null {
  try {
    const text = Buffer.from(cursor, 'base64url').toString('utf8');
    const match = /^issue:(\d+)$/.exec(text);
    return match === null ? null : Number(match[1]);
  } catch {
    return null;
  }
}

interface Ctx {
  variables: Variables;
}

function executeSelections(
  selections: SelectionNode[],
  source: unknown,
  kind: 'Query' | 'Mutation' | 'Repository' | 'Issue' | 'PullRequest' | 'User' | '__Type' | '__TypeField' | 'IssueConnection' | 'PullRequestConnection' | 'PageInfo' | 'BranchRef' | 'CommentEdge' | 'IssueComment' | 'PullRequestChangedFileConnection' | 'PullRequestChangedFile' | 'CreateIssuePayload' | 'CloseIssuePayload' | 'CreatePullRequestPayload' | 'MergePullRequestPayload' | 'AddCommentPayload',
  ctx: Ctx,
): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const selection of selections) {
    if (selection.kind === 'inlineFragment') {
      // Inline fragments apply when the type matches (or is absent); the suite
      // uses `... on Repository` on a Repository node.
      const fragmentOut = executeSelections(selection.selections, source, kind, ctx);
      Object.assign(out, fragmentOut);
      continue;
    }
    const field = selection.field;
    const key = field.alias ?? field.name;
    out[key] = resolveField(field, source, kind, ctx);
  }
  return out;
}

function unknownField(field: string, type: string): never {
  throw new GraphQLError(`Cannot query field "${field}" on type "${type}".`);
}

function resolveField(
  field: FieldNode,
  source: unknown,
  kind: string,
  ctx: Ctx,
): unknown {
  switch (kind) {
    case 'Query':
      return resolveQueryField(field, ctx);
    case 'Mutation':
      return resolveMutationField(field, ctx);
    case 'Repository':
      return resolveRepositoryField(field, source as Repo, ctx);
    case 'Issue':
      return resolveIssueField(field, source as Issue, ctx);
    case 'PullRequest':
      return resolvePullRequestField(field, source as PullRequest, ctx);
    case 'User':
      return resolveUserField(field, source as { login: string }, ctx);
    case '__Type':
      return resolveTypeField(field, source as { name: string }, ctx);
    case '__TypeField': {
      const record = source as Record<string, unknown>;
      const value = record[field.name];
      if (value === undefined) unknownField(field.name, '__Type');
      return value;
    }
    case 'IssueConnection':
      return resolveIssueConnectionField(field, source as { nodes: Issue[]; pageInfo: { hasNextPage: boolean; endCursor: string | null } }, ctx);
    case 'PullRequestConnection':
      return resolvePullRequestConnectionField(field, source as { nodes: PullRequest[] }, ctx);
    case 'PageInfo':
      return resolvePageInfoField(field, source as { hasNextPage: boolean; endCursor: string | null }, ctx);
    case 'BranchRef':
      return resolveBranchRefField(field, source as { name: string }, ctx);
    case 'CreateIssuePayload':
    case 'CloseIssuePayload':
    case 'CreatePullRequestPayload':
    case 'MergePullRequestPayload':
    case 'AddCommentPayload': {
      const record = source as Record<string, unknown>;
      const value = record[field.name];
      if (value === undefined) unknownField(field.name, kind);
      if (field.selections.length > 0 && isRecord(value)) {
        const subKind =
          field.name === 'issue' ? 'Issue' :
          field.name === 'pullRequest' ? 'PullRequest' :
          field.name === 'commentEdge' ? 'CommentEdge' :
          kind;
        return executeSelections(field.selections, value, subKind as never, ctx);
      }
      return value;
    }
    case 'CommentEdge': {
      const record = source as Record<string, unknown>;
      if (field.name === 'node') {
        const node = record.node;
        if (field.selections.length > 0 && isRecord(node)) return executeSelections(field.selections, node, 'IssueComment', ctx);
        return node;
      }
      unknownField(field.name, kind);
      return undefined;
    }
    case 'IssueComment': {
      const record = source as Record<string, unknown>;
      const value = record[field.name];
      if (value === undefined) unknownField(field.name, kind);
      return value;
    }
    case 'PullRequestChangedFileConnection': {
      const record = source as { nodes: Array<{ path: string }> };
      if (field.name === 'nodes') {
        return record.nodes.map((node) => executeSelections(field.selections, node, 'PullRequestChangedFile', ctx));
      }
      unknownField(field.name, kind);
      return undefined;
    }
    case 'PullRequestChangedFile': {
      const record = source as Record<string, unknown>;
      const value = record[field.name];
      if (value === undefined) unknownField(field.name, kind);
      return value;
    }
    default:
      unknownField(field.name, kind);
      return undefined;
  }
}

function resolveQueryField(field: FieldNode, ctx: Ctx): unknown {
  if (field.name === 'viewer') {
    if (field.selections.length === 0) return { login: VIEWER_LOGIN };
    return executeSelections(field.selections, { login: VIEWER_LOGIN }, 'User', ctx);
  }
  if (field.name === 'repository') {
    const owner = argValue(field, 'owner', ctx.variables);
    const name = argValue(field, 'name', ctx.variables);
    if (typeof owner !== 'string' || typeof name !== 'string') return null;
    const repo = owner === VIEWER_LOGIN ? store.getRepo(owner, name) : undefined;
    if (repo === undefined) return null;
    if (field.selections.length === 0) return {};
    return executeSelections(field.selections, repo, 'Repository', ctx);
  }
  if (field.name === 'node') {
    const id = argValue(field, 'id', ctx.variables);
    if (typeof id !== 'string') return null;
    const decoded = decodeGlobalId(id);
    if (decoded === null) return null;
    if (decoded.kind === 'Repository') {
      const repo = store.getRepoByDatabaseId(decoded.databaseId);
      if (repo === undefined) return null;
      if (field.selections.length === 0) return { id };
      return executeSelections(field.selections, repo, 'Repository', ctx);
    }
    if (decoded.kind === 'Issue' || decoded.kind === 'PullRequest') {
      const repo = store.getRepoByDatabaseId(decoded.databaseId);
      if (repo === undefined) return null;
      const target = decoded.kind === 'Issue' ? store.getIssue(repo, decoded.number) : store.getPull(repo, decoded.number);
      if (target === undefined) return null;
      if (field.selections.length === 0) return { id };
      return executeSelections(field.selections, target, decoded.kind, ctx);
    }
    return null;
  }
  if (field.name === '__type') {
    const name = argValue(field, 'name', ctx.variables);
    if (typeof name !== 'string') return null;
    if (!['Issue', 'PullRequest', 'Repository', 'SearchType', 'User'].includes(name)) return null;
    if (field.selections.length === 0) return { name };
    return executeSelections(field.selections, { name }, '__Type', ctx);
  }
  unknownField(field.name, 'Query');
  return undefined;
}

function resolveRepositoryField(field: FieldNode, repo: Repo, ctx: Ctx): unknown {
  switch (field.name) {
    case 'id':
      return b64url(`Repository:${repo.databaseId}`);
    case 'databaseId':
      return repo.databaseId;
    case 'name':
      return repo.name;
    case 'nameWithOwner':
      return `${repo.owner}/${repo.name}`;
    case 'owner':
      if (field.selections.length === 0) return { login: repo.owner };
      return executeSelections(field.selections, { login: repo.owner }, 'User', ctx);
    case 'defaultBranchRef': {
      const tip = store.headTip(repo);
      if (tip === undefined) return null;
      if (field.selections.length === 0) return { name: repo.defaultBranch };
      return executeSelections(field.selections, { name: repo.defaultBranch }, 'BranchRef', ctx);
    }
    case 'viewerPermission':
      return 'ADMIN';
    case 'hasIssuesEnabled':
      return true;
    case 'description':
      return repo.description;
    case 'private':
      return repo.isPrivate;
    case 'url':
      return `https://github.com/${repo.owner}/${repo.name}`;
    case 'createdAt':
    case 'updatedAt':
      return new Date((1735689600 + repo.databaseId) * 1000).toISOString();
    case 'issue': {
      const number = argValue(field, 'number', ctx.variables);
      if (typeof number !== 'number') return null;
      const issue = store.getIssue(repo, number);
      if (issue === undefined) return null;
      if (field.selections.length === 0) return { number: issue.number };
      return executeSelections(field.selections, issue, 'Issue', ctx);
    }
    case 'issues': {
      const first = argValue(field, 'first', ctx.variables);
      const after = argValue(field, 'after', ctx.variables);
      const states = argValue(field, 'states', ctx.variables);
      const orderBy = argValue(field, 'orderBy', ctx.variables);
      let list = [...repo.issues];
      if (Array.isArray(states) && states.length > 0) {
        const wanted = new Set(states.map((s) => String(s).toUpperCase()));
        list = list.filter((issue) => wanted.has(issue.state.toUpperCase()));
      }
      if (isRecord(orderBy)) {
        const direction = String(orderBy.direction ?? 'DESC').toUpperCase();
        list.sort((a, b) => (direction === 'ASC' ? a.seq - b.seq : b.seq - a.seq));
      } else {
        list.sort((a, b) => a.seq - b.seq);
      }
      if (typeof after === 'string' && after !== '') {
        const afterNumber = cursorNumber(after);
        if (afterNumber !== null) {
          const index = list.findIndex((issue) => issue.number === afterNumber);
          if (index >= 0) list = list.slice(index + 1);
        }
      }
      const limit = typeof first === 'number' ? first : list.length;
      const page = list.slice(0, limit);
      const connection = {
        nodes: page,
        pageInfo: {
          hasNextPage: list.length > page.length,
          endCursor: page.length > 0 ? issueCursor(page[page.length - 1]!.number) : null,
        },
      };
      if (field.selections.length === 0) return connection;
      return executeSelections(field.selections, connection, 'IssueConnection', ctx);
    }
    case 'pullRequests': {
      const first = argValue(field, 'first', ctx.variables);
      const list = [...repo.pulls].sort((a, b) => a.seq - b.seq);
      const page = typeof first === 'number' ? list.slice(0, first) : list;
      const connection = { nodes: page };
      if (field.selections.length === 0) return connection;
      return executeSelections(field.selections, connection, 'PullRequestConnection', ctx);
    }
    case 'pullRequest': {
      const number = argValue(field, 'number', ctx.variables);
      if (typeof number !== 'number') return null;
      const pull = store.getPull(repo, number);
      if (pull === undefined) return null;
      if (field.selections.length === 0) return { number: pull.number };
      return executeSelections(field.selections, pull, 'PullRequest', ctx);
    }
    default:
      unknownField(field.name, 'Repository');
      return undefined;
  }
}

function resolveIssueField(field: FieldNode, issue: Issue, ctx: Ctx): unknown {
  switch (field.name) {
    case 'id':
      return issue.globalId;
    case 'number':
      return issue.number;
    case 'title':
      return issue.title;
    case 'state':
      return issue.state.toUpperCase();
    case 'body':
      return issue.body;
    case 'createdAt':
      return issue.createdAt;
    case 'updatedAt':
      return issue.updatedAt;
    case 'url':
      return '';
    default:
      unknownField(field.name, 'Issue');
      return undefined;
  }
}

function resolvePullRequestField(field: FieldNode, pull: PullRequest, ctx: Ctx): unknown {
  // The repo is found through the pull's number space: the executor passes the
  // repo along via a module-level lookup is not possible, so files/mergeable
  // resolve through the store by database id embedded in the global id.
  const decoded = decodeGlobalId(pull.globalId);
  const repo = decoded === null ? undefined : store.getRepoByDatabaseId(decoded.databaseId);
  switch (field.name) {
    case 'id':
      return pull.globalId;
    case 'number':
      return pull.number;
    case 'title':
      return pull.title;
    case 'body':
      return pull.body;
    case 'state':
      return pull.merged ? 'MERGED' : pull.state.toUpperCase();
    case 'merged':
      return pull.merged;
    case 'headRefName':
      return pull.head.ref;
    case 'baseRefName':
      return pull.base.ref;
    case 'headRefOid':
      return pull.head.sha;
    case 'baseRefOid':
      return pull.base.sha;
    case 'mergeable': {
      if (repo === undefined) return 'UNKNOWN';
      return store.mergeable(repo, pull);
    }
    case 'files': {
      if (repo === undefined) return { nodes: [] };
      const first = argValue(field, 'first', ctx.variables);
      const baseTip = store.refTip(repo, `refs/heads/${pull.base.ref}`);
      const headTip = store.refTip(repo, `refs/heads/${pull.head.ref}`);
      if (baseTip === undefined || headTip === undefined) return { nodes: [] };
      const mb = mergeBase(store.lookup(repo), baseTip, headTip) ?? baseTip;
      const files = store.diffFiles(repo, mb, headTip);
      const nodes = (typeof first === 'number' ? files.slice(0, first) : files).map((file) => ({ path: file.path }));
      const connection = { nodes };
      if (field.selections.length === 0) return connection;
      return executeSelections(field.selections, connection, 'PullRequestChangedFileConnection', ctx);
    }
    case 'createdAt':
      return pull.createdAt;
    case 'updatedAt':
      return pull.updatedAt;
    case 'url':
      return '';
    default:
      unknownField(field.name, 'PullRequest');
      return undefined;
  }
}

function resolveUserField(field: FieldNode, user: { login: string }, ctx: Ctx): unknown {
  if (field.name === 'login') return user.login;
  unknownField(field.name, 'User');
  return undefined;
}

function resolveTypeField(field: FieldNode, type: { name: string }, ctx: Ctx): unknown {
  if (field.name === 'name') return type.name;
  if (field.name === 'fields') {
    const names =
      type.name === 'Issue' ? ISSUE_FIELDS :
      type.name === 'PullRequest' ? PULL_REQUEST_FIELDS :
      type.name === 'Repository' ? REPOSITORY_FIELDS :
      type.name === 'User' ? ['login', 'id', 'name', 'email'] : [];
    const list = names.map((name) => ({ name }));
    if (field.selections.length === 0) return list;
    return list.map((entry) => executeSelections(field.selections, entry, '__TypeField' as never, ctx));
  }
  if (field.name === 'enumValues') {
    if (type.name !== 'SearchType') return [];
    const list = SEARCH_TYPE_VALUES.map((name) => ({ name }));
    if (field.selections.length === 0) return list;
    return list.map((entry) => executeSelections(field.selections, entry, '__TypeField' as never, ctx));
  }
  unknownField(field.name, '__Type');
  return undefined;
}

function resolveIssueConnectionField(
  field: FieldNode,
  connection: { nodes: Issue[]; pageInfo: { hasNextPage: boolean; endCursor: string | null } },
  ctx: Ctx,
): unknown {
  if (field.name === 'nodes') {
    return connection.nodes.map((node) => executeSelections(field.selections, node, 'Issue', ctx));
  }
  if (field.name === 'pageInfo') {
    if (field.selections.length === 0) return connection.pageInfo;
    return executeSelections(field.selections, connection.pageInfo, 'PageInfo', ctx);
  }
  unknownField(field.name, 'IssueConnection');
  return undefined;
}

function resolvePullRequestConnectionField(field: FieldNode, connection: { nodes: PullRequest[] }, ctx: Ctx): unknown {
  if (field.name === 'nodes') {
    return connection.nodes.map((node) => executeSelections(field.selections, node, 'PullRequest', ctx));
  }
  unknownField(field.name, 'PullRequestConnection');
  return undefined;
}

function resolvePageInfoField(field: FieldNode, pageInfo: { hasNextPage: boolean; endCursor: string | null }, _ctx: Ctx): unknown {
  if (field.name === 'hasNextPage') return pageInfo.hasNextPage;
  if (field.name === 'endCursor') return pageInfo.endCursor;
  unknownField(field.name, 'PageInfo');
  return undefined;
}

function resolveBranchRefField(field: FieldNode, ref: { name: string }, _ctx: Ctx): unknown {
  if (field.name === 'name') return ref.name;
  unknownField(field.name, 'BranchRef');
  return undefined;
}

function resolveMutationField(field: FieldNode, ctx: Ctx): unknown {
  if (field.name === 'createIssue') {
    const input = argValue(field, 'input', ctx.variables);
    if (!isRecord(input)) throw new GraphQLError('createIssue needs an input object.');
    const repositoryId = input.repositoryId;
    const title = input.title;
    if (typeof repositoryId !== 'string' || typeof title !== 'string') throw new GraphQLError('createIssue needs repositoryId and title.');
    const decoded = decodeGlobalId(repositoryId);
    const repo = decoded === null ? undefined : store.getRepoByDatabaseId(decoded.databaseId);
    if (repo === undefined) throw new GraphQLError(`Could not resolve to a Repository with the id of '${repositoryId}'.`);
    const issue = store.createIssue(repo, title, typeof input.body === 'string' ? input.body : '', []);
    const payload = { issue };
    if (field.selections.length === 0) return payload;
    return executeSelections(field.selections, payload, 'CreateIssuePayload', ctx);
  }
  if (field.name === 'closeIssue') {
    const input = argValue(field, 'input', ctx.variables);
    if (!isRecord(input)) throw new GraphQLError('closeIssue needs an input object.');
    const issueId = input.issueId;
    if (typeof issueId !== 'string') throw new GraphQLError('closeIssue needs issueId.');
    const decoded = decodeGlobalId(issueId);
    const repo = decoded === null ? undefined : store.getRepoByDatabaseId(decoded.databaseId);
    const issue = repo === undefined ? undefined : store.getIssue(repo, decoded!.number);
    if (issue === undefined) throw new GraphQLError(`Could not resolve to an Issue with the id of '${issueId}'.`);
    issue.state = 'closed';
    issue.updatedAt = store.tick().iso;
    const payload = { issue };
    if (field.selections.length === 0) return payload;
    return executeSelections(field.selections, payload, 'CloseIssuePayload', ctx);
  }
  if (field.name === 'createPullRequest') {
    const input = argValue(field, 'input', ctx.variables);
    if (!isRecord(input)) throw new GraphQLError('createPullRequest needs an input object.');
    const repositoryId = input.repositoryId;
    const baseRefName = input.baseRefName;
    const headRefName = input.headRefName;
    const title = input.title;
    if (typeof repositoryId !== 'string' || typeof baseRefName !== 'string' || typeof headRefName !== 'string' || typeof title !== 'string') {
      throw new GraphQLError('createPullRequest needs repositoryId, baseRefName, headRefName and title.');
    }
    const decoded = decodeGlobalId(repositoryId);
    const repo = decoded === null ? undefined : store.getRepoByDatabaseId(decoded.databaseId);
    if (repo === undefined) throw new GraphQLError(`Could not resolve to a Repository with the id of '${repositoryId}'.`);
    if (store.refTip(repo, `refs/heads/${headRefName}`) === undefined || store.refTip(repo, `refs/heads/${baseRefName}`) === undefined) {
      throw new GraphQLError('head or base branch does not exist.');
    }
    const open = repo.pulls.find((pull) => pull.head.ref === headRefName && pull.state === 'open');
    if (open !== undefined) throw new GraphQLError('a pull request is already open for this head.');
    const pull = store.createPull(repo, title, typeof input.body === 'string' ? input.body : '', headRefName, baseRefName);
    if (pull === null) throw new GraphQLError('head or base branch does not exist.');
    const payload = { pullRequest: pull };
    if (field.selections.length === 0) return payload;
    return executeSelections(field.selections, payload, 'CreatePullRequestPayload', ctx);
  }
  if (field.name === 'mergePullRequest') {
    const input = argValue(field, 'input', ctx.variables);
    if (!isRecord(input)) throw new GraphQLError('mergePullRequest needs an input object.');
    const pullRequestId = input.pullRequestId;
    if (typeof pullRequestId !== 'string') throw new GraphQLError('mergePullRequest needs pullRequestId.');
    const decoded = decodeGlobalId(pullRequestId);
    const repo = decoded === null ? undefined : store.getRepoByDatabaseId(decoded.databaseId);
    const pull = repo === undefined ? undefined : store.getPull(repo, decoded!.number);
    if (pull === undefined) throw new GraphQLError(`Could not resolve to a PullRequest with the id of '${pullRequestId}'.`);
    if (pull.merged) throw new GraphQLError('Pull request has already been merged.');
    const mergedSha = store.squashMerge(repo, pull);
    if (mergedSha === null) throw new GraphQLError('Merge conflict.');
    const payload = { pullRequest: pull };
    if (field.selections.length === 0) return payload;
    return executeSelections(field.selections, payload, 'MergePullRequestPayload', ctx);
  }
  if (field.name === 'addComment') {
    const input = argValue(field, 'input', ctx.variables);
    if (!isRecord(input)) throw new GraphQLError('addComment needs an input object.');
    const subjectId = input.subjectId;
    const body = input.body;
    if (typeof subjectId !== 'string' || typeof body !== 'string') throw new GraphQLError('addComment needs subjectId and body.');
    const decoded = decodeGlobalId(subjectId);
    const repo = decoded === null ? undefined : store.getRepoByDatabaseId(decoded.databaseId);
    const issue = repo === undefined || decoded?.kind !== 'Issue' ? undefined : store.getIssue(repo, decoded.number);
    if (issue === undefined) throw new GraphQLError(`Could not resolve to an Issue with the id of '${subjectId}'.`);
    const comment = store.addComment(repo, issue, body);
    const payload = { commentEdge: { node: { id: comment.globalId, body: comment.body } } };
    if (field.selections.length === 0) return payload;
    return executeSelections(field.selections, payload, 'AddCommentPayload', ctx);
  }
  unknownField(field.name, 'Mutation');
  return undefined;
}

// -------------------------------------------------------------------------------------------
// HTTP entry point
// -------------------------------------------------------------------------------------------

function executeOperation(operation: OperationNode, variables: Variables): { data: unknown; errors: Array<{ message: string }> } {
  const ctx: Ctx = { variables };
  try {
    const kind = operation.operation === 'mutation' ? 'Mutation' : 'Query';
    const data = executeSelections(operation.selections, null, kind, ctx);
    return { data, errors: [] };
  } catch (err) {
    if (err instanceof GraphQLError) return { data: null, errors: [{ message: err.message }] };
    throw err;
  }
}

export function handleGraphQL(req: IncomingMessage, res: ServerResponse, body: Buffer): void {
  const token = shopToken();
  const auth = req.headers.authorization ?? '';
  if (token === '' || auth !== `Bearer ${token}`) {
    sendJson(res, 401, { message: 'Requires authentication' });
    return;
  }
  const { value, error } = parseJsonBody(body);
  if (error !== null) {
    sendJson(res, 400, { message: 'Problems parsing JSON' });
    return;
  }
  // Batched operations: a JSON array answers an array, in order.
  if (Array.isArray(value)) {
    const out: Array<{ data: unknown; errors: Array<{ message: string }> }> = [];
    for (const entry of value) {
      if (!isRecord(entry) || typeof entry.query !== 'string') {
        out.push({ data: null, errors: [{ message: 'Each batched operation needs a query string.' }] });
        continue;
      }
      const variables = isRecord(entry.variables) ? (entry.variables as Variables) : {};
      try {
        const operations = new Parser(entry.query).parse();
        const operation = entry.operationName !== undefined && typeof entry.operationName === 'string'
          ? operations.find((op) => op.name === entry.operationName) ?? operations[0]!
          : operations[0]!;
        out.push(executeOperation(operation, variables));
      } catch (err) {
        out.push({ data: null, errors: [{ message: err instanceof Error ? err.message : String(err) }] });
      }
    }
    sendJson(res, 200, out);
    return;
  }
  if (!isRecord(value) || typeof value.query !== 'string') {
    sendJson(res, 400, { message: 'A query string is required' });
    return;
  }
  const variables = isRecord(value.variables) ? (value.variables as Variables) : {};
  try {
    const operations = new Parser(value.query).parse();
    const operation = value.operationName !== undefined && typeof value.operationName === 'string'
      ? operations.find((op) => op.name === value.operationName) ?? operations[0]!
      : operations[0]!;
    const result = executeOperation(operation, variables);
    sendJson(res, 200, result);
  } catch (err) {
    sendJson(res, 200, { data: null, errors: [{ message: err instanceof Error ? err.message : String(err) }] });
  }
}
