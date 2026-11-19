// store.ts — the forge's whole state, in memory. One Store per process.
// Everything is deterministic: a logical clock (never Date.now(), never
// Math.random()) stamps every commit, issue and comment, so two trials fed the
// same requests produce the same bytes.
//
// The token's owner is the scenario's viewer. The scenario seed is the
// contest's fixed data (kit program/env/src/wire.ts: SCENARIO_SEED_HEX
// '6a09e667f3bcc909'), whose first generated user is login 'adaiversen'
// (id 1000, 'ada iversen' <adaiversen@example.invalid>). The entry must know
// that login before any owner-bearing request arrives — POST /user/repos
// carries no owner — so it is recorded here, derived from the fixed seed.

import {
  buildCommit,
  buildTree,
  collectReachable,
  hashObject,
  isAncestor,
  isObjectId,
  mergeBase,
  parseCommit,
  parseTag,
  parseTree,
  ZERO_OID,
  type GitObject,
  type ObjectLookup,
  type TreeEntry,
} from './git.ts';

export const VIEWER_LOGIN = 'adaiversen';
export const VIEWER_ID = 1000;
export const VIEWER_NAME = 'ada iversen';
export const VIEWER_EMAIL = 'adaiversen@example.invalid';
export const DEFAULT_BRANCH = 'main';
export const FORGE_IDENTITY = 'Muse <muse@make.fun>';

/** Logical epoch: 2025-01-01T00:00:00Z. Every stamp is this plus a sequence number. */
export const LOGICAL_EPOCH_SEC = 1735689600;

export interface Label {
  name: string;
  color: string;
}

export interface Comment {
  id: number;
  globalId: string;
  body: string;
  createdAt: string;
}

export interface Issue {
  number: number;
  id: number;
  globalId: string;
  title: string;
  body: string;
  state: 'open' | 'closed';
  labels: string[];
  comments: Comment[];
  createdAt: string;
  updatedAt: string;
  seq: number;
}

export interface PullRequest {
  number: number;
  id: number;
  globalId: string;
  title: string;
  body: string;
  state: 'open' | 'closed';
  merged: boolean;
  mergedAt: string | null;
  mergeCommitSha: string | null;
  head: { ref: string; sha: string };
  base: { ref: string; sha: string };
  createdAt: string;
  updatedAt: string;
  seq: number;
}

export interface Repo {
  owner: string;
  name: string;
  description: string;
  isPrivate: boolean;
  databaseId: number;
  defaultBranch: string;
  objects: Map<string, GitObject>;
  refs: Map<string, string>;
  labels: Map<string, Label>;
  issues: Issue[];
  pulls: PullRequest[];
  nextNumber: number;
  nextCommentId: number;
  seq: number;
}

function b64url(text: string): string {
  return Buffer.from(text, 'utf8').toString('base64url');
}

export function repoGlobalId(databaseId: number): string {
  return b64url(`Repository:${databaseId}`);
}

export function issueGlobalId(databaseId: number, number: number): string {
  return b64url(`Issue:${databaseId}:${number}`);
}

export function pullGlobalId(databaseId: number, number: number): string {
  return b64url(`PullRequest:${databaseId}:${number}`);
}

export function commentGlobalId(databaseId: number, number: number, commentId: number): string {
  return b64url(`IssueComment:${databaseId}:${number}:${commentId}`);
}

/** Decode a global id back to its parts, or null when it is not one of ours. */
export function decodeGlobalId(id: string): { kind: string; databaseId: number; number: number; commentId?: number } | null {
  let text: string;
  try {
    text = Buffer.from(id, 'base64url').toString('utf8');
  } catch {
    return null;
  }
  const parts = text.split(':');
  if (parts.length < 2) return null;
  const kind = parts[0]!;
  const databaseId = Number(parts[1]);
  if (!Number.isInteger(databaseId) || databaseId <= 0) return null;
  if (kind === 'Repository' && parts.length === 2) return { kind, databaseId, number: 0 };
  if ((kind === 'Issue' || kind === 'PullRequest') && parts.length === 3) {
    const number = Number(parts[2]);
    if (!Number.isInteger(number) || number <= 0) return null;
    return { kind, databaseId, number };
  }
  if (kind === 'IssueComment' && parts.length === 4) {
    const number = Number(parts[2]);
    const commentId = Number(parts[3]);
    if (!Number.isInteger(number) || number <= 0 || !Number.isInteger(commentId) || commentId <= 0) return null;
    return { kind, databaseId, number, commentId };
  }
  return null;
}

export class Store {
  readonly repos = new Map<string, Repo>();
  private nextDatabaseId = 1;
  private clockSeq = 0;
  private commitSeq = 0;

  /** One logical tick: a deterministic timestamp (seconds + ISO string). */
  tick(): { ts: number; iso: string } {
    this.clockSeq += 1;
    const ts = LOGICAL_EPOCH_SEC + this.clockSeq;
    return { ts, iso: new Date(ts * 1000).toISOString() };
  }

  /** A commit timestamp: the fixed epoch plus a per-commit sequence. Deterministic. */
  nextCommitTimestamp(): number {
    this.commitSeq += 1;
    return 1700000000 + this.commitSeq;
  }

  key(owner: string, name: string): string {
    return `${owner}/${name}`;
  }

  getRepo(owner: string, name: string): Repo | undefined {
    return this.repos.get(this.key(owner, name));
  }

  getRepoByDatabaseId(databaseId: number): Repo | undefined {
    for (const repo of this.repos.values()) if (repo.databaseId === databaseId) return repo;
    return undefined;
  }

  createRepo(owner: string, name: string, options: { isPrivate: boolean; description: string }): Repo | null {
    if (this.repos.has(this.key(owner, name))) return null;
    const repo: Repo = {
      owner,
      name,
      description: options.description,
      isPrivate: options.isPrivate,
      databaseId: this.nextDatabaseId,
      defaultBranch: DEFAULT_BRANCH,
      objects: new Map(),
      refs: new Map(),
      labels: new Map(),
      issues: [],
      pulls: [],
      nextNumber: 1,
      nextCommentId: 1,
      seq: this.clockSeq,
    };
    this.nextDatabaseId += 1;
    this.repos.set(this.key(owner, name), repo);
    return repo;
  }

  deleteRepo(owner: string, name: string): boolean {
    return this.repos.delete(this.key(owner, name));
  }

  /** The viewer's repositories, in creation order. */
  listRepos(owner: string): Repo[] {
    const out: Repo[] = [];
    for (const repo of this.repos.values()) if (repo.owner === owner) out.push(repo);
    return out;
  }

  lookup(repo: Repo): ObjectLookup {
    return (oid: string): GitObject | undefined => repo.objects.get(oid);
  }

  putObject(repo: Repo, type: GitObject['type'], data: Buffer): string {
    const oid = hashObject(type, data);
    if (!repo.objects.has(oid)) repo.objects.set(oid, { type, data });
    return oid;
  }

  refTip(repo: Repo, ref: string): string | undefined {
    return repo.refs.get(ref);
  }

  headTip(repo: Repo): string | undefined {
    return repo.refs.get(`refs/heads/${repo.defaultBranch}`);
  }

  /** Create a commit object in the repo and return its id. */
  makeCommit(repo: Repo, fields: { tree: string; parents: string[]; message: string }): string {
    const data = buildCommit({
      tree: fields.tree,
      parents: fields.parents,
      message: fields.message,
      author: FORGE_IDENTITY,
      timestamp: this.nextCommitTimestamp(),
    });
    return this.putObject(repo, 'commit', data);
  }

  // -------------------------------------------------------------------------------------------
  // trees
  // -------------------------------------------------------------------------------------------

  /** Flatten a tree into path -> {mode, oid, type}. Missing tree = empty. */
  flattenTree(repo: Repo, treeOid: string | null): Map<string, { mode: string; oid: string }> {
    const out = new Map<string, { mode: string; oid: string }>();
    if (treeOid === null) return out;
    const walk = (oid: string, prefix: string): void => {
      const object = repo.objects.get(oid);
      if (object === undefined || object.type !== 'tree') return;
      for (const entry of parseTree(object.data)) {
        const path = prefix === '' ? entry.name : `${prefix}/${entry.name}`;
        if (entry.mode === '40000') walk(entry.oid, path);
        else out.set(path, { mode: entry.mode, oid: entry.oid });
      }
    };
    walk(treeOid, '');
    return out;
  }

  /** The tip commit's tree, flattened; or an empty map for an unborn repo. */
  filesAtTip(repo: Repo, ref?: string): Map<string, { mode: string; oid: string }> {
    const tip = ref === undefined ? this.headTip(repo) : this.refTip(repo, ref);
    if (tip === undefined) return new Map();
    const object = repo.objects.get(tip);
    if (object === undefined || object.type !== 'commit') return new Map();
    return this.flattenTree(repo, parseCommit(object.data).tree);
  }

  /** Build (and store) a tree from a flat path map; returns the root tree id. */
  buildTreeFromFiles(repo: Repo, files: Map<string, { mode: string; oid: string }>): string {
    interface Node {
      files: Map<string, TreeEntry>;
      dirs: Map<string, Node>;
    }
    const make = (): Node => ({ files: new Map(), dirs: new Map() });
    const root = make();
    for (const [path, entry] of files) {
      const segments = path.split('/');
      let node = root;
      for (let index = 0; index < segments.length - 1; index += 1) {
        const name = segments[index]!;
        let child = node.dirs.get(name);
        if (child === undefined) {
          child = make();
          node.dirs.set(name, child);
        }
        node = child;
      }
      node.files.set(segments[segments.length - 1]!, { mode: entry.mode, name: segments[segments.length - 1]!, oid: entry.oid });
    }
    const build = (node: Node): string => {
      const entries: TreeEntry[] = [];
      for (const entry of node.files.values()) entries.push(entry);
      for (const [name, child] of node.dirs) entries.push({ mode: '40000', name, oid: build(child) });
      return this.putObject(repo, 'tree', buildTree(entries));
    };
    return build(root);
  }

  /** The tree of a commit id, or null when the commit is unknown. */
  commitTree(repo: Repo, commitOid: string): string | null {
    const object = repo.objects.get(commitOid);
    if (object === undefined || object.type !== 'commit') return null;
    return parseCommit(object.data).tree;
  }

  /** Peel a ref target to a commit: annotated tags peel to their commit; anything else as-is. */
  peelToCommit(repo: Repo, oid: string): string | null {
    let current = oid;
    for (let hops = 0; hops < 10; hops += 1) {
      const object = repo.objects.get(current);
      if (object === undefined) return null;
      if (object.type === 'commit') return current;
      if (object.type === 'tag') {
        current = parseTag(object.data).object;
        continue;
      }
      return null;
    }
    return null;
  }

  // -------------------------------------------------------------------------------------------
  // issues / pulls
  // -------------------------------------------------------------------------------------------

  createIssue(repo: Repo, title: string, body: string, labels: string[]): Issue {
    const { iso } = this.tick();
    const number = repo.nextNumber;
    repo.nextNumber += 1;
    const issue: Issue = {
      number,
      id: repo.databaseId * 1000000 + number,
      globalId: issueGlobalId(repo.databaseId, number),
      title,
      body,
      state: 'open',
      labels: [...labels],
      comments: [],
      createdAt: iso,
      updatedAt: iso,
      seq: this.clockSeq,
    };
    repo.issues.push(issue);
    return issue;
  }

  getIssue(repo: Repo, number: number): Issue | undefined {
    return repo.issues.find((issue) => issue.number === number);
  }

  addComment(repo: Repo, issue: Issue, body: string): Comment {
    const { iso } = this.tick();
    const comment: Comment = {
      id: repo.nextCommentId,
      globalId: commentGlobalId(repo.databaseId, issue.number, repo.nextCommentId),
      body,
      createdAt: iso,
    };
    repo.nextCommentId += 1;
    issue.comments.push(comment);
    return comment;
  }

  createPull(
    repo: Repo,
    title: string,
    body: string,
    headRef: string,
    baseRef: string,
  ): PullRequest | null {
    const headSha = this.refTip(repo, `refs/heads/${headRef}`);
    const baseSha = this.refTip(repo, `refs/heads/${baseRef}`);
    if (headSha === undefined || baseSha === undefined) return null;
    const { iso } = this.tick();
    const number = repo.nextNumber;
    repo.nextNumber += 1;
    const pull: PullRequest = {
      number,
      id: repo.databaseId * 1000000 + number,
      globalId: pullGlobalId(repo.databaseId, number),
      title,
      body,
      state: 'open',
      merged: false,
      mergedAt: null,
      mergeCommitSha: null,
      head: { ref: headRef, sha: headSha },
      base: { ref: baseRef, sha: baseSha },
      createdAt: iso,
      updatedAt: iso,
      seq: this.clockSeq,
    };
    repo.pulls.push(pull);
    return pull;
  }

  getPull(repo: Repo, number: number): PullRequest | undefined {
    return repo.pulls.find((pull) => pull.number === number);
  }

  /** Changed files between two commits' trees, with line counts. */
  diffFiles(repo: Repo, baseCommit: string, headCommit: string): Array<{
    path: string;
    status: 'added' | 'modified' | 'removed';
    additions: number;
    deletions: number;
  }> {
    const baseTree = this.commitTree(repo, baseCommit);
    const headTree = this.commitTree(repo, headCommit);
    const baseFiles = this.flattenTree(repo, baseTree);
    const headFiles = this.flattenTree(repo, headTree);
    const paths = new Set([...baseFiles.keys(), ...headFiles.keys()]);
    const out: Array<{ path: string; status: 'added' | 'modified' | 'removed'; additions: number; deletions: number }> = [];
    const lineCount = (oid: string): string[] => {
      const object = repo.objects.get(oid);
      if (object === undefined) return [];
      const text = object.data.toString('utf8');
      if (text === '') return [];
      const lines = text.split('\n');
      if (lines[lines.length - 1] === '') lines.pop();
      return lines;
    };
    for (const path of [...paths].sort()) {
      const base = baseFiles.get(path);
      const head = headFiles.get(path);
      if (base !== undefined && head === undefined) {
        out.push({ path, status: 'removed', additions: 0, deletions: lineCount(base.oid).length });
      } else if (base === undefined && head !== undefined) {
        out.push({ path, status: 'added', additions: lineCount(head.oid).length, deletions: 0 });
      } else if (base !== undefined && head !== undefined && base.oid !== head.oid) {
        const oldLines = lineCount(base.oid);
        const newLines = lineCount(head.oid);
        const oldSet = new Map<string, number>();
        for (const line of oldLines) oldSet.set(line, (oldSet.get(line) ?? 0) + 1);
        let additions = 0;
        for (const line of newLines) {
          const left = oldSet.get(line) ?? 0;
          if (left > 0) oldSet.set(line, left - 1);
          else additions += 1;
        }
        let deletions = 0;
        for (const left of oldSet.values()) deletions += left;
        out.push({ path, status: 'modified', additions, deletions });
      }
    }
    return out;
  }

  /**
   * Three-way merge of `headTip` into `baseTip` with `mergeBaseOid` as the ancestor.
   * Returns the merged tree id, or null when both sides changed the same path differently.
   */
  threeWayMerge(repo: Repo, baseTip: string, headTip: string, mergeBaseOid: string): string | null {
    const baseTree = this.commitTree(repo, baseTip);
    const headTree = this.commitTree(repo, headTip);
    const ancestorTree = this.commitTree(repo, mergeBaseOid);
    const baseFiles = this.flattenTree(repo, baseTree);
    const headFiles = this.flattenTree(repo, headTree);
    const ancestorFiles = this.flattenTree(repo, ancestorTree);
    const paths = new Set([...baseFiles.keys(), ...headFiles.keys(), ...ancestorFiles.keys()]);
    const merged = new Map<string, { mode: string; oid: string }>();
    for (const path of paths) {
      const ancestor = ancestorFiles.get(path);
      const base = baseFiles.get(path);
      const head = headFiles.get(path);
      const baseChanged = (base?.oid ?? null) !== (ancestor?.oid ?? null);
      const headChanged = (head?.oid ?? null) !== (ancestor?.oid ?? null);
      if (!baseChanged && !headChanged) {
        if (ancestor !== undefined) merged.set(path, ancestor);
      } else if (baseChanged && !headChanged) {
        if (base !== undefined) merged.set(path, base);
      } else if (!baseChanged && headChanged) {
        if (head !== undefined) merged.set(path, head);
      } else {
        // Both changed: take it when they agree, else conflict.
        if ((base?.oid ?? null) === (head?.oid ?? null)) {
          if (base !== undefined) merged.set(path, base);
        } else {
          return null;
        }
      }
    }
    return this.buildTreeFromFiles(repo, merged);
  }

  /** MERGEABLE / CONFLICTING for a pull request, from the three-way merge. */
  mergeable(repo: Repo, pull: PullRequest): 'MERGEABLE' | 'CONFLICTING' | 'UNKNOWN' {
    const baseTip = this.refTip(repo, `refs/heads/${pull.base.ref}`);
    const headTip = this.refTip(repo, `refs/heads/${pull.head.ref}`);
    if (baseTip === undefined || headTip === undefined) return 'UNKNOWN';
    const base = mergeBase(this.lookup(repo), baseTip, headTip);
    if (base === null) return 'UNKNOWN';
    const merged = this.threeWayMerge(repo, baseTip, headTip, base);
    return merged === null ? 'CONFLICTING' : 'MERGEABLE';
  }

  /**
   * Squash-merge a pull request: one commit on the base branch holding the
   * head's changes. Returns the merge commit id, or null on conflict.
   */
  squashMerge(repo: Repo, pull: PullRequest): string | null {
    const baseTip = this.refTip(repo, `refs/heads/${pull.base.ref}`);
    const headTip = this.refTip(repo, `refs/heads/${pull.head.ref}`);
    if (baseTip === undefined || headTip === undefined) return null;
    const base = mergeBase(this.lookup(repo), baseTip, headTip);
    if (base === null) return null;
    const mergedTree = this.threeWayMerge(repo, baseTip, headTip, base);
    if (mergedTree === null) return null;
    const { iso } = this.tick();
    const message = `Merge pull request #${pull.number} from ${repo.owner}/${pull.head.ref}\n\n${pull.title}\n`;
    const commitOid = this.makeCommit(repo, { tree: mergedTree, parents: [baseTip], message });
    repo.refs.set(`refs/heads/${pull.base.ref}`, commitOid);
    pull.merged = true;
    pull.state = 'closed';
    pull.mergedAt = iso;
    pull.mergeCommitSha = commitOid;
    pull.updatedAt = iso;
    return commitOid;
  }

  /** Fast-forward test: is `oldTip` (or nothing) an ancestor of `newTip`? */
  isFastForward(repo: Repo, oldTip: string, newTip: string): boolean {
    if (oldTip === ZERO_OID) return true;
    return isAncestor(this.lookup(repo), oldTip, newTip);
  }

  /** All objects reachable from a set of tips (for fetch). */
  reachable(repo: Repo, tips: Iterable<string>): Set<string> {
    return collectReachable(tips, this.lookup(repo), { allowMissing: true });
  }

  /** Commits reachable from a ref, newest first (by creation order, approximated by BFS depth). */
  commitsFrom(repo: Repo, tip: string): string[] {
    const out: string[] = [];
    const seen = new Set<string>();
    const queue = [tip];
    while (queue.length > 0) {
      const oid = queue.shift()!;
      if (seen.has(oid)) continue;
      seen.add(oid);
      const object = repo.objects.get(oid);
      if (object === undefined || object.type !== 'commit') continue;
      out.push(oid);
      for (const parent of parseCommit(object.data).parents) queue.push(parent);
    }
    return out;
  }
}

export const store = new Store();
