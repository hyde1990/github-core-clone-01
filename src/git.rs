//! Git objects, packs, pkt-lines and fetch planning. Pure functions over
//! bytes; the store (store.rs) owns the maps.
//!
//! Round 15 hardening (C4): `parse_pack` bounds the declared object count
//! by the input size *before* allocating, caps the pre-allocation at 1024,
//! and rejects a declared object size above the 64 MiB inflate cap *before*
//! inflating it — so a count of 0xFFFFFFFF or a 200 MB declaration can
//! never trigger a huge allocation or a giant inflate.

use crate::sha1::{hash_object, Sha1};
use crate::util::{hex_decode_20, hex_encode};
use crate::zlib;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

pub type Oid = [u8; 20];
pub const ZERO_OID: Oid = [0u8; 20];

pub fn oid_hex(oid: &Oid) -> String {
    hex_encode(oid)
}

pub fn oid_from_hex(text: &str) -> Option<Oid> {
    hex_decode_20(text)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ObjType {
    Commit,
    Tree,
    Blob,
    Tag,
}

impl ObjType {
    pub fn name(self) -> &'static str {
        match self {
            ObjType::Commit => "commit",
            ObjType::Tree => "tree",
            ObjType::Blob => "blob",
            ObjType::Tag => "tag",
        }
    }

    /// Pack type code (1 commit, 2 tree, 3 blob, 4 tag).
    pub fn code(self) -> u8 {
        match self {
            ObjType::Commit => 1,
            ObjType::Tree => 2,
            ObjType::Blob => 3,
            ObjType::Tag => 4,
        }
    }

    pub fn from_code(code: u8) -> Option<ObjType> {
        match code {
            1 => Some(ObjType::Commit),
            2 => Some(ObjType::Tree),
            3 => Some(ObjType::Blob),
            4 => Some(ObjType::Tag),
            _ => None,
        }
    }
}

#[derive(Clone)]
pub struct Object {
    pub ty: ObjType,
    pub data: Arc<Vec<u8>>,
}

impl Object {
    pub fn new(ty: ObjType, data: Vec<u8>) -> Self {
        Object {
            ty,
            data: Arc::new(data),
        }
    }
}

// ---------------------------------------------------------------------------
// Commit / tree / tag parsing (lazy: raw bytes are stored, parsed on demand).

#[derive(Clone, Debug)]
pub struct ParsedCommit {
    pub tree: Oid,
    pub parents: Vec<Oid>,
}

/// `tree <hex>` and `parent <hex>` lines, up to the blank line.
pub fn parse_commit(data: &[u8]) -> Option<ParsedCommit> {
    parse_commit_full(data).map(|full| ParsedCommit {
        tree: full.tree,
        parents: full.parents,
    })
}

/// A commit's full header: tree, parents, author/committer lines, timestamp
/// and message (the message has trailing newlines stripped, as GitHub shows).
pub struct CommitFull {
    pub tree: Oid,
    pub parents: Vec<Oid>,
    pub author: String,
    pub committer: String,
    pub timestamp: u64,
    pub message: String,
}

pub fn parse_commit_full(data: &[u8]) -> Option<CommitFull> {
    let text = std::str::from_utf8(data).ok()?;
    let mut tree = None;
    let mut parents = Vec::new();
    let mut author = String::new();
    let mut committer = String::new();
    let mut timestamp = 0u64;
    let mut message = String::new();
    let mut in_message = false;
    for line in text.split('\n') {
        if in_message {
            if !message.is_empty() {
                message.push('\n');
            }
            message.push_str(line);
            continue;
        }
        if line.is_empty() {
            in_message = true;
            continue;
        }
        if let Some(hex) = line.strip_prefix("tree ") {
            tree = oid_from_hex(hex.trim());
        } else if let Some(hex) = line.strip_prefix("parent ") {
            if let Some(oid) = oid_from_hex(hex.trim()) {
                parents.push(oid);
            }
        } else if let Some(rest) = line.strip_prefix("author ") {
            author = rest.to_string();
            timestamp = ident_timestamp(rest).unwrap_or(timestamp);
        } else if let Some(rest) = line.strip_prefix("committer ") {
            committer = rest.to_string();
            if timestamp == 0 {
                timestamp = ident_timestamp(rest).unwrap_or(0);
            }
        }
    }
    while message.ends_with('\n') || message.ends_with('\r') {
        message.pop();
    }
    Some(CommitFull {
        tree: tree?,
        parents,
        author,
        committer,
        timestamp,
        message,
    })
}

/// The epoch in a `Name <email> 1735689600 +0000` identity line.
fn ident_timestamp(line: &str) -> Option<u64> {
    let mut parts = line.rsplit(' ');
    let _tz = parts.next()?;
    parts.next()?.parse().ok()
}

#[derive(Clone, Debug)]
pub struct TreeEntry {
    pub mode: u32,
    pub name: String,
    pub oid: Oid,
}

/// `<mode> <name>\0<20-byte oid>` entries.
pub fn parse_tree(data: &[u8]) -> Option<Vec<TreeEntry>> {
    let mut entries = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let space = data[i..].iter().position(|&b| b == b' ')? + i;
        let mode_text = std::str::from_utf8(&data[i..space]).ok()?;
        let mode = u32::from_str_radix(mode_text, 8).ok()?;
        let nul = data[space + 1..].iter().position(|&b| b == 0)? + space + 1;
        let name = std::str::from_utf8(&data[space + 1..nul]).ok()?.to_string();
        if nul + 21 > data.len() {
            return None;
        }
        let mut oid = [0u8; 20];
        oid.copy_from_slice(&data[nul + 1..nul + 21]);
        entries.push(TreeEntry { mode, name, oid });
        i = nul + 21;
    }
    Some(entries)
}

/// A tag object's target oid (`object <hex>` on the first line).
pub fn parse_tag(data: &[u8]) -> Option<Oid> {
    let text = std::str::from_utf8(data).ok()?;
    let first = text.lines().next()?;
    oid_from_hex(first.strip_prefix("object ")?.trim())
}

// ---------------------------------------------------------------------------
// Builders (for server-made commits and trees).

/// Git's tree sort: entries by name, directories as if named `name/`.
pub fn sort_tree_entries(entries: &mut [TreeEntry]) {
    entries.sort_by(|a, b| {
        let a_key = if a.mode == 0o40000 {
            format!("{}/", a.name)
        } else {
            a.name.clone()
        };
        let b_key = if b.mode == 0o40000 {
            format!("{}/", b.name)
        } else {
            b.name.clone()
        };
        a_key.cmp(&b_key)
    });
}

pub fn build_tree(entries: &[TreeEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    for entry in entries {
        out.extend_from_slice(format!("{:o} {}", entry.mode, entry.name).as_bytes());
        out.push(0);
        out.extend_from_slice(&entry.oid);
    }
    out
}

pub fn build_commit(
    tree: &Oid,
    parents: &[Oid],
    author: &str,
    committer: &str,
    timestamp: u64,
    message: &str,
) -> Vec<u8> {
    let mut out = String::new();
    out.push_str(&format!("tree {}\n", oid_hex(tree)));
    for parent in parents {
        out.push_str(&format!("parent {}\n", oid_hex(parent)));
    }
    out.push_str(&format!("author {author} {timestamp} +0000\n"));
    out.push_str(&format!("committer {committer} {timestamp} +0000\n"));
    out.push('\n');
    out.push_str(message);
    if !message.ends_with('\n') {
        out.push('\n');
    }
    out.into_bytes()
}

#[allow(dead_code)]
pub fn build_tag(
    object: &Oid,
    ty: ObjType,
    tag: &str,
    tagger: &str,
    timestamp: u64,
    message: &str,
) -> Vec<u8> {
    let mut out = String::new();
    out.push_str(&format!("object {}\n", oid_hex(object)));
    out.push_str(&format!("type {}\n", ty.name()));
    out.push_str(&format!("tag {tag}\n"));
    out.push_str(&format!("tagger {tagger} {timestamp} +0000\n"));
    out.push('\n');
    out.push_str(message);
    if !message.ends_with('\n') {
        out.push('\n');
    }
    out.into_bytes()
}

// ---------------------------------------------------------------------------
// Object database: one repo's objects, keyed by oid.

pub struct ObjectDb {
    pub objects: HashMap<Oid, Object>,
}

impl ObjectDb {
    pub fn new() -> Self {
        ObjectDb {
            objects: HashMap::new(),
        }
    }

    /// Store an object, returning its oid (content-addressed).
    pub fn insert(&mut self, ty: ObjType, data: Vec<u8>) -> Oid {
        let oid = hash_object(ty.name(), &data);
        self.objects
            .entry(oid)
            .or_insert_with(|| Object::new(ty, data));
        oid
    }

    pub fn insert_typed(&mut self, ty: ObjType, data: Vec<u8>, oid: Oid) {
        self.objects
            .entry(oid)
            .or_insert_with(|| Object::new(ty, data));
    }

    pub fn get(&self, oid: &Oid) -> Option<&Object> {
        self.objects.get(oid)
    }

    #[allow(dead_code)]
    pub fn contains(&self, oid: &Oid) -> bool {
        self.objects.contains_key(oid)
    }

    pub fn commit_of(&self, oid: &Oid) -> Option<ParsedCommit> {
        let obj = self.objects.get(oid)?;
        if obj.ty != ObjType::Commit {
            return None;
        }
        parse_commit(&obj.data)
    }

    /// The commit a tag (possibly nested) ultimately points at, or None.
    pub fn peel_to_commit(&self, oid: &Oid) -> Option<Oid> {
        let mut current = *oid;
        for _ in 0..16 {
            let obj = self.objects.get(&current)?;
            match obj.ty {
                ObjType::Commit => return Some(current),
                ObjType::Tag => current = parse_tag(&obj.data)?,
                _ => return None,
            }
        }
        None
    }

    /// Whether `ancestor` is reachable from `descendant` (inclusive, after
    /// peeling tags). Used for the receive-pack fast-forward check.
    pub fn is_ancestor_of(&self, ancestor: &Oid, descendant: &Oid) -> bool {
        let Some(ancestor) = self.peel_to_commit(ancestor) else {
            return false;
        };
        let Some(start) = self.peel_to_commit(descendant) else {
            return false;
        };
        if ancestor == start {
            return true;
        }
        let mut seen = HashSet::new();
        let mut stack = vec![start];
        let mut steps = 0usize;
        while let Some(oid) = stack.pop() {
            steps += 1;
            if steps > 1_000_000 {
                return false;
            }
            if !seen.insert(oid) {
                continue;
            }
            let Some(parsed) = self.commit_of(&oid) else {
                continue;
            };
            for parent in parsed.parents {
                if parent == ancestor {
                    return true;
                }
                stack.push(parent);
            }
        }
        false
    }

    /// All objects reachable from `roots` (commits, their trees/blobs, tags
    /// and their targets). Roots that are missing contribute nothing.
    pub fn closure(&self, roots: &[Oid]) -> HashSet<Oid> {
        let mut out = HashSet::new();
        let mut stack: Vec<Oid> = roots.to_vec();
        while let Some(oid) = stack.pop() {
            if !out.insert(oid) {
                continue;
            }
            let Some(obj) = self.objects.get(&oid) else {
                continue;
            };
            match obj.ty {
                ObjType::Commit => {
                    if let Some(parsed) = parse_commit(&obj.data) {
                        stack.push(parsed.tree);
                        stack.extend(parsed.parents);
                    }
                }
                ObjType::Tree => {
                    if let Some(entries) = parse_tree(&obj.data) {
                        stack.extend(entries.iter().map(|e| e.oid));
                    }
                }
                ObjType::Tag => {
                    if let Some(target) = parse_tag(&obj.data) {
                        stack.push(target);
                    }
                }
                ObjType::Blob => {}
            }
        }
        out
    }

    /// A tree's own closure: the tree, its subtrees and its blobs.
    pub fn tree_closure(&self, tree: Oid, out: &mut HashSet<Oid>) {
        let mut stack = vec![tree];
        while let Some(oid) = stack.pop() {
            if !out.insert(oid) {
                continue;
            }
            let Some(obj) = self.objects.get(&oid) else {
                continue;
            };
            if obj.ty != ObjType::Tree {
                continue;
            }
            if let Some(entries) = parse_tree(&obj.data) {
                for entry in entries {
                    if entry.mode == 0o40000 {
                        stack.push(entry.oid);
                    } else {
                        out.insert(entry.oid);
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Fetch planning.

pub struct FetchPlan {
    /// Objects to send, in walk order (commits, then their trees/blobs).
    pub objects: Vec<Oid>,
    /// New shallow boundary commits.
    pub shallow: Vec<Oid>,
    /// Previously shallow commits whose parents are now included.
    pub unshallow: Vec<Oid>,
}

/// Plan a fetch: walk from `wants` (depth-limited when `depth` is set),
/// subtract what the client holds, and compute the shallow boundary.
///
/// `shallow_boundaries` are the client's current shallow commits: a have
/// that is one of them subtracts only its own tree closure, not its
/// ancestors — that is what makes `deepen` deliver the parent.
pub fn plan_fetch(
    db: &ObjectDb,
    wants: &[Oid],
    haves: &[Oid],
    shallow_boundaries: &[Oid],
    depth: Option<u32>,
) -> FetchPlan {
    // 1. Walk commits from wants, generation by generation.
    let mut walked: Vec<Oid> = Vec::new(); // commits, walk order
    let mut seen: HashSet<Oid> = HashSet::new();
    let mut frontier: Vec<Oid> = wants.to_vec();
    let mut generation = 0u32;
    let mut boundary: Vec<Oid> = Vec::new();
    loop {
        if depth.is_some_and(|d| generation >= d) {
            break;
        }
        let mut next: Vec<Oid> = Vec::new();
        for oid in frontier {
            if !seen.insert(oid) {
                continue;
            }
            let Some(parsed) = db.commit_of(&oid) else {
                // A want that is a tag (or a missing object): include the
                // object itself, no parents to walk.
                continue;
            };
            walked.push(oid);
            if depth.is_some_and(|d| generation + 1 >= d) {
                boundary.push(oid);
            } else {
                next.extend(parsed.parents);
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
        generation += 1;
    }
    // Tags among the wants are sent with their targets.
    let mut send: HashSet<Oid> = HashSet::new();
    for oid in wants {
        if let Some(obj) = db.get(oid) {
            if obj.ty == ObjType::Tag {
                send.insert(*oid);
                if let Some(target) = parse_tag(&obj.data) {
                    for extra in db.closure(&[target]) {
                        send.insert(extra);
                    }
                }
            }
        }
    }
    // 2. The walk's object set: each walked commit + its tree closure.
    for oid in &walked {
        send.insert(*oid);
        if let Some(parsed) = db.commit_of(oid) {
            db.tree_closure(parsed.tree, &mut send);
        }
    }
    // 3. Subtract what the client holds.
    let boundary_set: HashSet<Oid> = shallow_boundaries.iter().copied().collect();
    for have in haves {
        if boundary_set.contains(have) {
            // A shallow boundary: only its own commit + tree closure.
            send.remove(have);
            if let Some(parsed) = db.commit_of(have) {
                let mut own = HashSet::new();
                db.tree_closure(parsed.tree, &mut own);
                for oid in own {
                    send.remove(&oid);
                }
            }
        } else {
            for oid in db.closure(&[*have]) {
                send.remove(&oid);
            }
        }
    }
    // 4. Shallow bookkeeping.
    let shallow: Vec<Oid> = boundary
        .into_iter()
        .filter(|oid| send.contains(oid))
        .collect();
    let shallow_set: HashSet<Oid> = shallow.iter().copied().collect();
    let walked_set: HashSet<Oid> = walked.iter().copied().collect();
    let unshallow: Vec<Oid> = shallow_boundaries
        .iter()
        .copied()
        .filter(|oid| walked_set.contains(oid) && !shallow_set.contains(oid))
        .collect();
    // Deterministic order: walked commits first, then the rest sorted.
    let mut objects: Vec<Oid> = walked
        .into_iter()
        .filter(|oid| send.contains(oid))
        .collect();
    let mut rest: Vec<Oid> = send
        .into_iter()
        .filter(|oid| !objects.contains(oid))
        .collect();
    rest.sort();
    objects.extend(rest);
    FetchPlan {
        objects,
        shallow,
        unshallow,
    }
}

// ---------------------------------------------------------------------------
// Packs.

/// Parse a packfile (v2/3, plain objects only) into (oid, type, bytes).
/// Verifies the trailer SHA-1.
///
/// C4 hardening: the declared object count is bounded by the input size
/// *before* the trailer check and *before* any allocation (a well-formed
/// object needs at least ~10 bytes, so `count > (len - 32) / 10` cannot be
/// real); the result vector pre-allocates at most 1024 entries; and each
/// object's declared size is checked against the 64 MiB inflate cap
/// before `inflate_parts` runs.
pub fn parse_pack(data: &[u8]) -> Result<Vec<(Oid, ObjType, Vec<u8>)>, String> {
    if data.len() < 12 + 20 {
        return Err("pack too short".to_string());
    }
    if &data[0..4] != b"PACK" {
        return Err("bad pack magic".to_string());
    }
    let version = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    if version != 2 && version != 3 {
        return Err(format!("unsupported pack version {version}"));
    }
    let count = u32::from_be_bytes([data[8], data[9], data[10], data[11]]) as usize;
    // Bound the count by the input size first: a count of 0xFFFFFFFF on a
    // 137-byte pack is rejected here, before the trailer check and before
    // any allocation could run away.
    let max_objects = data.len().saturating_sub(32) / 10;
    if count > max_objects {
        return Err("pack object count exceeds input size".to_string());
    }
    // Trailer check: cheap and catches truncated/corrupt packs.
    let mut trailer_check = Sha1::new();
    trailer_check.update(&data[..data.len() - 20]);
    let want = trailer_check.finish();
    if want != data[data.len() - 20..] {
        return Err("pack trailer checksum mismatch".to_string());
    }
    let mut out = Vec::with_capacity(count.min(1024));
    let mut pos = 12;
    for _ in 0..count {
        if pos >= data.len() - 20 {
            return Err("pack truncated".to_string());
        }
        let first = data[pos];
        pos += 1;
        let ty_code = (first >> 4) & 0x7;
        let Some(ty) = ObjType::from_code(ty_code) else {
            return Err(format!(
                "pack object type {ty_code} (delta) is not supported"
            ));
        };
        let mut size = (first & 0x0f) as u64;
        let mut shift = 4u32;
        let mut cont = first & 0x80 != 0;
        while cont {
            if pos >= data.len() - 20 {
                return Err("pack truncated in size".to_string());
            }
            let byte = data[pos];
            pos += 1;
            size |= ((byte & 0x7f) as u64) << shift;
            shift += 7;
            cont = byte & 0x80 != 0;
        }
        // The declared size is checked before inflating: a 200 MB
        // declaration never allocates or inflates.
        if size > crate::zlib::MAX_INFLATE_BYTES as u64 {
            return Err("pack object size exceeds limit".to_string());
        }
        let (bytes, consumed) =
            zlib::inflate_parts(&data[pos..]).map_err(|e| format!("zlib: {e}"))?;
        pos += consumed;
        if bytes.len() as u64 != size {
            return Err("pack object size mismatch".to_string());
        }
        let oid = hash_object(ty.name(), &bytes);
        out.push((oid, ty, bytes));
    }
    Ok(out)
}

/// Build a packfile v2 from objects (plain, no deltas).
pub fn build_pack(objects: &[(Oid, &Object)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + objects.len() * 64);
    out.extend_from_slice(b"PACK");
    out.extend_from_slice(&2u32.to_be_bytes());
    out.extend_from_slice(&(objects.len() as u32).to_be_bytes());
    let mut hasher = Sha1::new();
    // Header bytes are hashed too; feed them as we go.
    hasher.update(&out);
    for (_, obj) in objects {
        let mut header = Vec::new();
        let mut size = obj.data.len() as u64;
        let mut first = ((size & 0x0f) as u8) | (obj.ty.code() << 4);
        size >>= 4;
        if size > 0 {
            first |= 0x80;
        }
        header.push(first);
        while size > 0 {
            let mut byte = (size & 0x7f) as u8;
            size >>= 7;
            if size > 0 {
                byte |= 0x80;
            }
            header.push(byte);
        }
        out.extend_from_slice(&header);
        hasher.update(&header);
        let compressed = zlib::deflate(&obj.data);
        out.extend_from_slice(&compressed);
        hasher.update(&compressed);
    }
    let trailer = hasher.finish();
    out.extend_from_slice(&trailer);
    out
}

// ---------------------------------------------------------------------------
// Pkt-lines.

#[derive(Clone, Debug, PartialEq)]
pub enum Pkt {
    Data(Vec<u8>),
    Flush,
    Delim,
    ResponseEnd,
}

pub fn pkt_line(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 4);
    out.extend_from_slice(format!("{:04x}", payload.len() + 4).as_bytes());
    out.extend_from_slice(payload);
    out
}

pub const PKT_FLUSH: &[u8; 4] = b"0000";
pub const PKT_DELIM: &[u8; 4] = b"0001";
#[allow(dead_code)]
pub const PKT_RESPONSE_END: &[u8; 4] = b"0002";

/// Decode pkt-lines from the start of `data`, stopping after the first flush
/// (or at the end). Returns the packets and the bytes consumed.
pub fn decode_pkt_prefix(data: &[u8]) -> Result<(Vec<Pkt>, usize), String> {
    let mut packets = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        if pos + 4 > data.len() {
            return Err("truncated pkt-line length".to_string());
        }
        let len_text = std::str::from_utf8(&data[pos..pos + 4])
            .map_err(|_| "bad pkt-line length".to_string())?;
        let len =
            usize::from_str_radix(len_text, 16).map_err(|_| "bad pkt-line length".to_string())?;
        if len == 0 {
            packets.push(Pkt::Flush);
            return Ok((packets, pos + 4));
        }
        if len == 1 {
            packets.push(Pkt::Delim);
            pos += 4;
            continue;
        }
        if len == 2 {
            packets.push(Pkt::ResponseEnd);
            pos += 4;
            continue;
        }
        if !(4..=65520).contains(&len) || pos + len > data.len() {
            return Err(format!("bad pkt-line length {len}"));
        }
        packets.push(Pkt::Data(data[pos + 4..pos + len].to_vec()));
        pos += len;
    }
    Ok((packets, pos))
}

/// Encode data packets, a delim, or a flush, in order.
#[allow(dead_code)]
pub fn encode_pkts(items: &[Pkt]) -> Vec<u8> {
    let mut out = Vec::new();
    for item in items {
        match item {
            Pkt::Data(payload) => out.extend_from_slice(&pkt_line(payload)),
            Pkt::Flush => out.extend_from_slice(PKT_FLUSH),
            Pkt::Delim => out.extend_from_slice(PKT_DELIM),
            Pkt::ResponseEnd => out.extend_from_slice(PKT_RESPONSE_END),
        }
    }
    out
}

/// Frame `data` as sideband packets of `band` (1 = pack data, 2 = progress,
/// 3 = error), each pkt-line at most 65520 bytes.
pub fn sideband(band: u8, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    if data.is_empty() {
        let payload = vec![band];
        out.extend_from_slice(&pkt_line(&payload));
        return out;
    }
    for chunk in data.chunks(65515) {
        let mut payload = Vec::with_capacity(chunk.len() + 1);
        payload.push(band);
        payload.extend_from_slice(chunk);
        out.extend_from_slice(&pkt_line(&payload));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob(data: &[u8]) -> (Oid, ObjType, Vec<u8>) {
        (hash_object("blob", data), ObjType::Blob, data.to_vec())
    }

    #[test]
    fn pack_roundtrip() {
        let (b1, t1, d1) = blob(b"hello world");
        let (b2, t2, d2) = blob(&vec![b'x'; 5000]);
        let commit = build_commit(
            &b1,
            &[],
            "A U Thor <a@example.invalid>",
            "A U Thor <a@example.invalid>",
            1_700_000_000,
            "first\n",
        );
        let coid = hash_object("commit", &commit);
        let tree = build_tree(&[
            TreeEntry {
                mode: 0o100644,
                name: "a.txt".to_string(),
                oid: b1,
            },
            TreeEntry {
                mode: 0o100644,
                name: "b.txt".to_string(),
                oid: b2,
            },
        ]);
        let toid = hash_object("tree", &tree);
        let objects = [
            (b1, Object::new(t1, d1)),
            (b2, Object::new(t2, d2)),
            (coid, Object::new(ObjType::Commit, commit.clone())),
            (toid, Object::new(ObjType::Tree, tree.clone())),
        ];
        let refs: Vec<(Oid, &Object)> = objects.iter().map(|(oid, obj)| (*oid, obj)).collect();
        let pack = build_pack(&refs);
        let parsed = parse_pack(&pack).unwrap();
        assert_eq!(parsed.len(), 4);
        let mut got: Vec<Oid> = parsed.iter().map(|(oid, _, _)| *oid).collect();
        got.sort();
        let mut want: Vec<Oid> = vec![b1, b2, coid, toid];
        want.sort();
        assert_eq!(got, want);
        // Corrupt one byte: the trailer check must fail.
        let mut bad = pack.clone();
        bad[20] ^= 0xff;
        assert!(parse_pack(&bad).is_err());
    }

    #[test]
    fn pkt_lines_roundtrip() {
        let items = vec![
            Pkt::Data(b"hello\n".to_vec()),
            Pkt::Delim,
            Pkt::Data(b"world".to_vec()),
            Pkt::Flush,
        ];
        let encoded = encode_pkts(&items);
        let (decoded, used) = decode_pkt_prefix(&encoded).unwrap();
        assert_eq!(decoded, items);
        assert_eq!(used, encoded.len());
        assert!(decode_pkt_prefix(b"zzzz").is_err());
        assert!(decode_pkt_prefix(b"000").is_err());
    }

    #[test]
    fn commit_tree_parse() {
        let tree = build_tree(&[TreeEntry {
            mode: 0o100644,
            name: "x".to_string(),
            oid: ZERO_OID,
        }]);
        let parsed = parse_tree(&tree).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "x");
        assert_eq!(parsed[0].mode, 0o100644);
        let commit = build_commit(&ZERO_OID, &[ZERO_OID], "A <a@x>", "A <a@x>", 42, "msg\n");
        let parsed = parse_commit(&commit).unwrap();
        assert_eq!(parsed.tree, ZERO_OID);
        assert_eq!(parsed.parents, vec![ZERO_OID]);
    }

    #[test]
    fn fetch_plan_depth_and_incremental() {
        let mut db = ObjectDb::new();
        let mk = |db: &mut ObjectDb, parent: Option<Oid>, content: &[u8]| -> Oid {
            let blob = db.insert(ObjType::Blob, content.to_vec());
            let tree = db.insert(
                ObjType::Tree,
                build_tree(&[TreeEntry {
                    mode: 0o100644,
                    name: "f.txt".to_string(),
                    oid: blob,
                }]),
            );
            let commit = build_commit(
                &tree,
                &parent.into_iter().collect::<Vec<_>>(),
                "A <a@x>",
                "A <a@x>",
                1_700_000_000,
                "c\n",
            );
            db.insert(ObjType::Commit, commit)
        };
        let c1 = mk(&mut db, None, b"one");
        let c2 = mk(&mut db, Some(c1), b"two");
        let c3 = mk(&mut db, Some(c2), b"three");
        // Depth 1: only the tip + its tree/blobs.
        let plan = plan_fetch(&db, &[c3], &[], &[], Some(1));
        assert!(plan.objects.contains(&c3));
        assert!(!plan.objects.contains(&c2));
        assert_eq!(plan.shallow, vec![c3]);
        // Depth 2 from a shallow clone holding c3: the parent only.
        let plan = plan_fetch(&db, &[c3], &[c3], &[c3], Some(2));
        assert!(plan.objects.contains(&c2));
        assert!(!plan.objects.contains(&c1));
        assert!(!plan.objects.contains(&c3));
        assert_eq!(plan.shallow, vec![c2]);
        assert_eq!(plan.unshallow, vec![c3]);
        // Incremental: wants c3, haves c1 -> c2, c3 and their new objects only.
        let plan = plan_fetch(&db, &[c3], &[c1], &[], None);
        assert!(plan.objects.contains(&c3));
        assert!(plan.objects.contains(&c2));
        assert!(!plan.objects.contains(&c1));
        // Nothing new.
        let plan = plan_fetch(&db, &[c3], &[c3], &[], None);
        assert!(plan.objects.is_empty());
    }

    #[test]
    fn rejects_absurd_pack_count() {
        // A valid small pack with its count field patched to 0xFFFFFFFF:
        // must fail on the count bound, never on a 206 GB allocation.
        // (The count check runs before the trailer check, so the stale
        // trailer does not matter here.)
        let (b1, t1, d1) = blob(b"hello");
        let objects = [(b1, Object::new(t1, d1))];
        let refs: Vec<(Oid, &Object)> = objects.iter().map(|(oid, obj)| (*oid, obj)).collect();
        let mut pack = build_pack(&refs);
        pack[8..12].copy_from_slice(&0xFFFFFFFFu32.to_be_bytes());
        let err = parse_pack(&pack).unwrap_err();
        assert!(
            err.contains("exceeds input size"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_oversize_declared_object() {
        // A hand-built pack: count 1, one blob header declaring 200 MB,
        // a tiny deflate stream, and a correct trailer. The declared size
        // must be rejected before any inflate happens.
        let mut pack: Vec<u8> = Vec::new();
        pack.extend_from_slice(b"PACK");
        pack.extend_from_slice(&2u32.to_be_bytes());
        pack.extend_from_slice(&1u32.to_be_bytes());
        // Object header: type blob (3), size 200_000_000, varint-encoded.
        let mut size: u64 = 200_000_000;
        let mut first = ((size & 0x0f) as u8) | (3u8 << 4);
        size >>= 4;
        if size > 0 {
            first |= 0x80;
        }
        pack.push(first);
        while size > 0 {
            let mut byte = (size & 0x7f) as u8;
            size >>= 7;
            if size > 0 {
                byte |= 0x80;
            }
            pack.push(byte);
        }
        pack.extend_from_slice(&zlib::deflate(b"tiny"));
        let mut hasher = Sha1::new();
        hasher.update(&pack);
        pack.extend_from_slice(&hasher.finish());
        let err = parse_pack(&pack).unwrap_err();
        assert!(err.contains("exceeds limit"), "unexpected error: {err}");
    }
}
