// git.ts — pure git primitives for the forge: object hashing, trees, commits,
// tags, packfiles (read/write, plain objects + OFS/REF deltas) and pkt-lines.
// Node stdlib only (node:crypto, node:zlib). No git binary anywhere in the trial
// pod, so everything the forge serves is built here.

import { createHash } from 'node:crypto';
import { deflateSync, inflateSync } from 'node:zlib';

export type GitObjectType = 'blob' | 'tree' | 'commit' | 'tag';

export interface GitObject {
  type: GitObjectType;
  data: Buffer;
}

export interface TreeEntry {
  mode: string;
  name: string;
  oid: string;
}

export interface CommitView {
  tree: string;
  parents: string[];
  author: string;
  committer: string;
  message: string;
}

export interface TagView {
  object: string;
  type: GitObjectType;
  tag: string;
  tagger: string;
  message: string;
}

export const ZERO_OID = '0'.repeat(40);
const HEX40 = /^[0-9a-f]{40}$/;

export function isObjectId(value: string): boolean {
  return HEX40.test(value);
}

function objectHeader(type: GitObjectType, size: number): Buffer {
  return Buffer.from(`${type} ${size}\0`, 'ascii');
}

/** SHA-1 of `<type> <size>\0<data>` — the object id. */
export function hashObject(type: GitObjectType, data: Uint8Array): string {
  const hash = createHash('sha1');
  hash.update(objectHeader(type, data.length));
  hash.update(data);
  return hash.digest('hex');
}

/** Parse a tree object into entries, in stored order. */
export function parseTree(data: Uint8Array): TreeEntry[] {
  const buffer = Buffer.isBuffer(data) ? data : Buffer.from(data);
  const entries: TreeEntry[] = [];
  let offset = 0;
  while (offset < buffer.length) {
    const space = buffer.indexOf(0x20, offset);
    if (space < 0) throw new Error('tree entry has no mode separator');
    const nul = buffer.indexOf(0x00, space + 1);
    if (nul < 0 || nul + 21 > buffer.length) throw new Error('tree entry is truncated');
    entries.push({
      mode: buffer.subarray(offset, space).toString('ascii'),
      name: buffer.subarray(space + 1, nul).toString('utf8'),
      oid: buffer.subarray(nul + 1, nul + 21).toString('hex'),
    });
    offset = nul + 21;
  }
  return entries;
}

/** git's tree-entry sort: byte order of the name, a directory compared as if it ended in '/'. */
function compareTreeNames(a: { name: string; mode: string }, b: { name: string; mode: string }): number {
  const left = Buffer.from(a.mode === '40000' ? `${a.name}/` : a.name, 'utf8');
  const right = Buffer.from(b.mode === '40000' ? `${b.name}/` : b.name, 'utf8');
  return Buffer.compare(left, right);
}

/** Build a tree object from entries; entries are sorted the way git sorts them. */
export function buildTree(entries: readonly TreeEntry[]): Buffer {
  const sorted = [...entries].sort(compareTreeNames);
  const parts: Buffer[] = [];
  for (const entry of sorted) {
    parts.push(Buffer.from(`${entry.mode} ${entry.name}\0`, 'utf8'));
    parts.push(Buffer.from(entry.oid, 'hex'));
  }
  return Buffer.concat(parts);
}

/** Parse a commit object. */
export function parseCommit(data: Uint8Array): CommitView {
  const buffer = Buffer.isBuffer(data) ? data : Buffer.from(data);
  const blank = buffer.indexOf('\n\n');
  const headerEnd = blank < 0 ? buffer.length : blank;
  const message = blank < 0 ? '' : buffer.subarray(blank + 2).toString('utf8');
  let tree = '';
  const parents: string[] = [];
  let author = '';
  let committer = '';
  for (const line of buffer.subarray(0, headerEnd).toString('utf8').split('\n')) {
    if (!line) continue;
    const space = line.indexOf(' ');
    const key = space < 0 ? line : line.slice(0, space);
    const value = space < 0 ? '' : line.slice(space + 1);
    if (key === 'tree') tree = value;
    else if (key === 'parent') parents.push(value);
    else if (key === 'author') author = value;
    else if (key === 'committer') committer = value;
  }
  if (!isObjectId(tree)) throw new Error('commit has no valid tree header');
  return { tree, parents, author, committer, message };
}

/** Parse an annotated tag object. */
export function parseTag(data: Uint8Array): TagView {
  const buffer = Buffer.isBuffer(data) ? data : Buffer.from(data);
  const blank = buffer.indexOf('\n\n');
  const headerEnd = blank < 0 ? buffer.length : blank;
  const headers = new Map<string, string>();
  for (const line of buffer.subarray(0, headerEnd).toString('utf8').split('\n')) {
    if (!line) continue;
    const space = line.indexOf(' ');
    if (space >= 0) headers.set(line.slice(0, space), line.slice(space + 1));
  }
  return {
    object: headers.get('object') ?? '',
    type: (headers.get('type') ?? 'commit') as GitObjectType,
    tag: headers.get('tag') ?? '',
    tagger: headers.get('tagger') ?? '',
    message: blank < 0 ? '' : buffer.subarray(blank + 2).toString('utf8'),
  };
}

/** Build a commit object's bytes. `timestamp` is a fixed logical clock value, never Date.now(). */
export function buildCommit(fields: {
  tree: string;
  parents?: readonly string[];
  message: string;
  author: string;
  committer?: string;
  timestamp: number;
  timezone?: string;
}): Buffer {
  const timezone = fields.timezone ?? '+0000';
  const who = `${fields.author} ${fields.timestamp} ${timezone}`;
  const committer = `${fields.committer ?? fields.author} ${fields.timestamp} ${timezone}`;
  const lines = [`tree ${fields.tree}`];
  for (const parent of fields.parents ?? []) lines.push(`parent ${parent}`);
  lines.push(`author ${who}`, `committer ${committer}`, '');
  return Buffer.from(`${lines.join('\n')}\n${fields.message}`, 'utf8');
}

/** Build an annotated tag object's bytes. */
export function buildTag(fields: {
  object: string;
  type: GitObjectType;
  tag: string;
  message: string;
  tagger: string;
  timestamp: number;
  timezone?: string;
}): Buffer {
  const timezone = fields.timezone ?? '+0000';
  return Buffer.from(
    `object ${fields.object}\ntype ${fields.type}\ntag ${fields.tag}\ntagger ${fields.tagger} ${fields.timestamp} ${timezone}\n\n${fields.message}`,
    'utf8',
  );
}

// ---------------------------------------------------------------------------------------------
// reachability
// ---------------------------------------------------------------------------------------------

export type ObjectLookup = (oid: string) => GitObject | undefined;

/** Every object reachable from `roots`: commit -> tree + parents, tree -> entries, tag -> target. */
export function collectReachable(
  roots: Iterable<string>,
  lookup: ObjectLookup,
  options: { allowMissing?: boolean } = {},
): Set<string> {
  const seen = new Set<string>();
  const queue: string[] = [];
  for (const root of roots) if (isObjectId(root)) queue.push(root);
  while (queue.length > 0) {
    const oid = queue.pop()!;
    if (seen.has(oid)) continue;
    const object = lookup(oid);
    if (object === undefined) {
      if (options.allowMissing === true) continue;
      throw new Error(`missing object ${oid}`);
    }
    seen.add(oid);
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
  return seen;
}

/** Is `ancestor` reachable from `tip` (or equal to it)? Linear-history fast path included. */
export function isAncestor(lookup: ObjectLookup, ancestor: string, tip: string): boolean {
  if (ancestor === tip) return true;
  const seen = new Set<string>();
  const queue = [tip];
  while (queue.length > 0) {
    const oid = queue.pop()!;
    if (oid === ancestor) return true;
    if (seen.has(oid)) continue;
    seen.add(oid);
    const object = lookup(oid);
    if (object === undefined || object.type !== 'commit') continue;
    for (const parent of parseCommit(object.data).parents) queue.push(parent);
  }
  return false;
}

/** The merge-base of two commits: the first common ancestor found by a two-sided walk. */
export function mergeBase(lookup: ObjectLookup, a: string, b: string): string | null {
  if (a === b) return a;
  const seenA = new Set<string>([a]);
  const seenB = new Set<string>([b]);
  const queueA = [a];
  const queueB = [b];
  // Walk both sides level by level; the first commit seen by both sides is a merge-base.
  // Histories here are small (fixtures and bench chains), so a plain BFS is fast enough.
  while (queueA.length > 0 || queueB.length > 0) {
    if (queueA.length > 0) {
      const oid = queueA.shift()!;
      if (seenB.has(oid)) return oid;
      const object = lookup(oid);
      if (object !== undefined && object.type === 'commit') {
        for (const parent of parseCommit(object.data).parents) {
          if (!seenA.has(parent)) {
            seenA.add(parent);
            queueA.push(parent);
          }
        }
      }
    }
    if (queueB.length > 0) {
      const oid = queueB.shift()!;
      if (seenA.has(oid)) return oid;
      const object = lookup(oid);
      if (object !== undefined && object.type === 'commit') {
        for (const parent of parseCommit(object.data).parents) {
          if (!seenB.has(parent)) {
            seenB.add(parent);
            queueB.push(parent);
          }
        }
      }
    }
  }
  return null;
}

// ---------------------------------------------------------------------------------------------
// packfiles
// ---------------------------------------------------------------------------------------------

const TYPE_CODE: Record<GitObjectType, number> = { commit: 1, tree: 2, blob: 3, tag: 4 };
const CODE_TYPE: Record<number, GitObjectType> = { 1: 'commit', 2: 'tree', 3: 'blob', 4: 'tag' };

function writePackObjectHeader(type: GitObjectType, size: number): Buffer {
  const code = TYPE_CODE[type];
  const bytes: number[] = [];
  let remaining = size;
  let first = (code << 4) | (remaining & 0x0f);
  remaining = Math.floor(remaining / 16);
  while (remaining > 0) {
    bytes.push(first | 0x80);
    first = remaining & 0x7f;
    remaining = Math.floor(remaining / 128);
  }
  bytes.push(first);
  return Buffer.from(bytes);
}

/** Build a packfile from plain objects (no deltas). Trailer is SHA-1 over everything before it. */
export function writePack(objects: Iterable<GitObject>): Buffer {
  const list = [...objects];
  const parts: Buffer[] = [];
  const header = Buffer.allocUnsafe(12);
  header.write('PACK', 0, 4, 'ascii');
  header.writeUInt32BE(2, 4);
  header.writeUInt32BE(list.length, 8);
  parts.push(header);
  for (const object of list) {
    parts.push(writePackObjectHeader(object.type, object.data.length));
    parts.push(deflateSync(object.data));
  }
  const body = Buffer.concat(parts);
  return Buffer.concat([body, createHash('sha1').update(body).digest()]);
}

/** True when the pack's trailer matches its bytes. */
export function verifyPackChecksum(pack: Uint8Array): boolean {
  const buffer = Buffer.isBuffer(pack) ? pack : Buffer.from(pack);
  if (buffer.length < 20) return false;
  const actual = buffer.subarray(buffer.length - 20).toString('hex');
  const expected = createHash('sha1').update(buffer.subarray(0, buffer.length - 20)).digest('hex');
  return actual === expected;
}

function readVarintSize(buffer: Buffer, offset: number): { size: number; first: number; next: number } {
  const first = buffer[offset]!;
  let size = first & 0x0f;
  let shift = 4;
  let next = offset + 1;
  let byte = first;
  while ((byte & 0x80) !== 0) {
    byte = buffer[next]!;
    size |= (byte & 0x7f) << shift;
    shift += 7;
    next += 1;
  }
  return { size, first, next };
}

function readOfsDeltaBase(buffer: Buffer, offset: number): { baseOffset: number; next: number } {
  let byte = buffer[offset]!;
  let baseOffset = byte & 0x7f;
  let next = offset + 1;
  while ((byte & 0x80) !== 0) {
    byte = buffer[next]!;
    baseOffset = ((baseOffset + 1) << 7) | (byte & 0x7f);
    next += 1;
  }
  return { baseOffset, next };
}

/** Apply a git delta (base size, result size, copy/insert ops) to a base buffer. */
export function applyDelta(base: Buffer, delta: Buffer): Buffer {
  let offset = 0;
  const readVarint = (): number => {
    let value = 0;
    let shift = 0;
    for (;;) {
      const byte = delta[offset]!;
      offset += 1;
      value |= (byte & 0x7f) << shift;
      shift += 7;
      if ((byte & 0x80) === 0) break;
    }
    return value;
  };
  const baseSize = readVarint();
  const resultSize = readVarint();
  if (baseSize !== base.length) throw new Error('delta base size mismatch');
  const parts: Buffer[] = [];
  let total = 0;
  while (offset < delta.length) {
    const op = delta[offset]!;
    offset += 1;
    if ((op & 0x80) !== 0) {
      let copyOffset = 0;
      let copySize = 0;
      for (let bit = 0; bit < 4; bit += 1) {
        if ((op & (1 << bit)) !== 0) {
          copyOffset |= delta[offset]! << (bit * 8);
          offset += 1;
        }
      }
      for (let bit = 0; bit < 3; bit += 1) {
        if ((op & (0x10 << bit)) !== 0) {
          copySize |= delta[offset]! << (bit * 8);
          offset += 1;
        }
      }
      if (copySize === 0) copySize = 0x10000;
      parts.push(base.subarray(copyOffset, copyOffset + copySize));
      total += copySize;
    } else if (op !== 0) {
      parts.push(delta.subarray(offset, offset + op));
      offset += op;
      total += op;
    } else {
      throw new Error('delta opcode 0');
    }
  }
  const result = Buffer.concat(parts);
  if (result.length !== resultSize || total !== resultSize) throw new Error('delta result size mismatch');
  return result;
}

export interface PackReadResult {
  version: number;
  declaredCount: number;
  objects: Map<string, GitObject>;
  deltaCount: number;
}

/** Parse a packfile: plain objects plus OFS_DELTA/REF_DELTA (bases in-pack or via resolveBase). */
export function readPack(pack: Uint8Array, options: { resolveBase?: ObjectLookup } = {}): PackReadResult {
  const buffer = Buffer.isBuffer(pack) ? pack : Buffer.from(pack);
  if (buffer.length < 32) throw new Error('pack is too short');
  if (buffer.subarray(0, 4).toString('ascii') !== 'PACK') throw new Error('pack does not start with PACK');
  const version = buffer.readUInt32BE(4);
  if (version !== 2 && version !== 3) throw new Error(`unsupported pack version ${version}`);
  const declaredCount = buffer.readUInt32BE(8);
  const trailerStart = buffer.length - 20;

  interface RawEntry {
    offset: number;
    typeCode: number;
    size: number;
    data: Buffer;
    baseOffset?: number;
    baseOid?: string;
    resolvedType?: GitObjectType;
    resolved?: Buffer;
    oid?: string;
  }
  const raw: RawEntry[] = [];
  const byOffset = new Map<number, RawEntry>();
  let offset = 12;
  for (let index = 0; index < declaredCount; index += 1) {
    const entryOffset = offset;
    const header = readVarintSize(buffer, offset);
    offset = header.next;
    const typeCode = (header.first >> 4) & 0x07;
    const entry: RawEntry = { offset: entryOffset, typeCode, size: header.size, data: Buffer.alloc(0) };
    if (typeCode === 6) {
      const base = readOfsDeltaBase(buffer, offset);
      offset = base.next;
      entry.baseOffset = entryOffset - base.baseOffset;
    } else if (typeCode === 7) {
      if (offset + 20 > trailerStart) throw new Error('ref-delta base id is truncated');
      entry.baseOid = buffer.subarray(offset, offset + 20).toString('hex');
      offset += 20;
    } else if (CODE_TYPE[typeCode] === undefined) {
      throw new Error(`pack entry ${index} has unknown type code ${typeCode}`);
    }
    // Inflate from `offset`; inflateSync needs the exact deflate stream — find its end by
    // inflating a slice and measuring consumed bytes via a streaming Inflater instead.
    const inflated = inflateAt(buffer, offset, trailerStart);
    if (inflated.data.length !== header.size) throw new Error('pack entry size mismatch after inflate');
    entry.data = inflated.data;
    offset = inflated.next;
    raw.push(entry);
    byOffset.set(entryOffset, entry);
  }
  if (offset !== trailerStart) throw new Error('pack has stray bytes before its trailer');

  const objects = new Map<string, GitObject>();
  const resolvedByOid = new Map<string, RawEntry>();
  const resolving = new Set<number>();
  let deltaCount = 0;
  const resolve = (entry: RawEntry): { type: GitObjectType; data: Buffer; oid: string } => {
    if (entry.resolved !== undefined && entry.resolvedType !== undefined && entry.oid !== undefined) {
      return { type: entry.resolvedType, data: entry.resolved, oid: entry.oid };
    }
    if (resolving.has(entry.offset)) throw new Error('delta cycle in pack');
    resolving.add(entry.offset);
    let type: GitObjectType;
    let data: Buffer;
    if (entry.typeCode === 6) {
      const baseEntry = byOffset.get(entry.baseOffset!);
      if (baseEntry === undefined) throw new Error('ofs-delta base is not in the pack');
      const base = resolve(baseEntry);
      type = base.type;
      data = applyDelta(base.data, entry.data);
    } else if (entry.typeCode === 7) {
      const wanted = entry.baseOid!;
      const baseEntry = resolvedByOid.get(wanted);
      if (baseEntry !== undefined) {
        const base = resolve(baseEntry);
        type = base.type;
        data = applyDelta(base.data, entry.data);
      } else {
        const external = options.resolveBase?.(wanted);
        if (external === undefined) throw new Error(`ref-delta base ${wanted} is not available`);
        type = external.type;
        data = applyDelta(external.data, entry.data);
      }
    } else {
      type = CODE_TYPE[entry.typeCode]!;
      data = entry.data;
    }
    const oid = hashObject(type, data);
    entry.resolved = data;
    entry.resolvedType = type;
    entry.oid = oid;
    resolvedByOid.set(oid, entry);
    resolving.delete(entry.offset);
    return { type, data, oid };
  };
  for (const entry of raw) {
    const resolved = resolve(entry);
    if (entry.typeCode === 6 || entry.typeCode === 7) deltaCount += 1;
    if (!objects.has(resolved.oid)) objects.set(resolved.oid, { type: resolved.type, data: resolved.data });
  }
  return { version, declaredCount, objects, deltaCount };
}

// Inflate one deflate stream starting at `offset` inside `buffer` (bounded by `end`),
// returning the inflated bytes and the offset just past the stream. Node's
// inflateSync ignores bytes after a stream's end and throws on a truncated one,
// so the stream's end is the shortest prefix that inflates cleanly: an
// exponential probe finds an upper bound, then a binary search refines it.
function inflateAt(buffer: Buffer, offset: number, end: number): { data: Buffer; next: number } {
  const tryInflate = (to: number): Buffer | null => {
    try {
      return inflateSync(buffer.subarray(offset, to));
    } catch {
      return null;
    }
  };
  let probe = Math.min(end, offset + 64);
  let best: Buffer | null = null;
  let bestEnd = -1;
  for (;;) {
    const data = tryInflate(probe);
    if (data !== null) {
      best = data;
      bestEnd = probe;
      break;
    }
    if (probe === end) break;
    probe = Math.min(end, probe * 2);
  }
  if (best === null || bestEnd < 0) throw new Error('pack entry deflate stream does not terminate');
  let low = offset + 1;
  let high = bestEnd;
  while (low <= high) {
    const mid = Math.floor((low + high) / 2);
    const data = tryInflate(mid);
    if (data !== null) {
      best = data;
      bestEnd = mid;
      high = mid - 1;
    } else {
      low = mid + 1;
    }
  }
  return { data: best, next: bestEnd };
}

// ---------------------------------------------------------------------------------------------
// pkt-lines
// ---------------------------------------------------------------------------------------------

export type PktLine =
  | { kind: 'data'; payload: Buffer }
  | { kind: 'flush' }
  | { kind: 'delim' };

/** Encode one data packet (payload + trailing newline handling is the caller's). */
export function pktLine(payload: string | Buffer): Buffer {
  const body = typeof payload === 'string' ? Buffer.from(payload, 'utf8') : payload;
  const length = body.length + 4;
  if (length > 65520) throw new Error('pkt-line too large');
  const head = Buffer.alloc(4);
  head.write(length.toString(16).padStart(4, '0'), 0, 4, 'ascii');
  return Buffer.concat([head, body]);
}

export const PKT_FLUSH = Buffer.from('0000', 'ascii');
export const PKT_DELIM = Buffer.from('0001', 'ascii');

/** Encode lines ('flush' for a flush packet, strings must already end with '\n' when wanted). */
export function encodePktLines(lines: readonly (string | 'flush' | 'delim')[]): Buffer {
  const parts: Buffer[] = [];
  for (const line of lines) {
    if (line === 'flush') parts.push(PKT_FLUSH);
    else if (line === 'delim') parts.push(PKT_DELIM);
    else parts.push(pktLine(line));
  }
  return Buffer.concat(parts);
}

/** Decode a whole buffer of pkt-lines. Throws on a malformed length prefix. */
export function decodePktLines(buffer: Buffer): PktLine[] {
  const out: PktLine[] = [];
  let offset = 0;
  while (offset < buffer.length) {
    if (offset + 4 > buffer.length) throw new Error('pkt-line truncated');
    const head = buffer.subarray(offset, offset + 4).toString('ascii');
    if (!/^[0-9a-fA-F]{4}$/.test(head)) throw new Error(`bad pkt-line length ${JSON.stringify(head)}`);
    const length = parseInt(head, 16);
    if (length === 0) {
      out.push({ kind: 'flush' });
      offset += 4;
      continue;
    }
    if (length === 1) {
      out.push({ kind: 'delim' });
      offset += 4;
      continue;
    }
    if (length < 4 || offset + length > buffer.length) throw new Error('pkt-line length out of range');
    out.push({ kind: 'data', payload: Buffer.from(buffer.subarray(offset + 4, offset + length)) });
    offset += length;
  }
  return out;
}

/** Incremental pkt-line decoder for streamed request bodies. */
export class PktLineDecoder {
  private buffer: Buffer = Buffer.alloc(0);

  push(chunk: Buffer): void {
    this.buffer = this.buffer.length === 0 ? chunk : Buffer.concat([this.buffer, chunk]);
  }

  /** Next packet, or null when more bytes are needed. Throws on malformed input. */
  next(): PktLine | null {
    if (this.buffer.length < 4) return null;
    const head = this.buffer.subarray(0, 4).toString('ascii');
    if (!/^[0-9a-fA-F]{4}$/.test(head)) throw new Error(`bad pkt-line length ${JSON.stringify(head)}`);
    const length = parseInt(head, 16);
    if (length === 0) {
      this.buffer = this.buffer.subarray(4);
      return { kind: 'flush' };
    }
    if (length === 1) {
      this.buffer = this.buffer.subarray(4);
      return { kind: 'delim' };
    }
    if (length < 4) throw new Error('pkt-line length out of range');
    if (this.buffer.length < length) return null;
    const payload = Buffer.from(this.buffer.subarray(4, length));
    this.buffer = this.buffer.subarray(length);
    return { kind: 'data', payload };
  }

  /** Bytes not yet consumed (the pack after a flush, for receive-pack). */
  rest(): Buffer {
    return this.buffer;
  }
}
