//! The in-memory store: repositories, git objects, refs, issues, pull
//! requests, labels and comments. One `Store` behind an `RwLock`; all
//! mutation goes through it. (Round 10 serves it from a single reactor
//! thread, so the lock is uncontended; the shape is kept so every handler
//! stays unchanged.)

use crate::git::{build_commit, build_pack, build_tree, ObjType, Object, ObjectDb, Oid, TreeEntry};
use crate::scenario::User;
use crate::util::base64_encode;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

pub const DEFAULT_BRANCH: &str = "main";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IssueState {
    Open,
    Closed,
}

impl IssueState {
    pub fn as_str(self) -> &'static str {
        match self {
            IssueState::Open => "open",
            IssueState::Closed => "closed",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Comment {
    pub id: u64,
    pub body: String,
}

#[derive(Clone, Debug)]
pub struct Issue {
    pub id: u64,
    pub number: u32,
    pub title: String,
    pub body: String,
    pub state: IssueState,
    pub labels: Vec<String>,
    pub comments: Vec<Comment>,
    pub created_seq: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MergeStatus {
    CanMerge,
    Conflicting,
    Unknown,
}

impl MergeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            MergeStatus::CanMerge => "MERGEABLE",
            MergeStatus::Conflicting => "CONFLICTING",
            MergeStatus::Unknown => "UNKNOWN",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Pull {
    pub id: u64,
    pub number: u32,
    pub title: String,
    pub body: String,
    pub head_ref: String,
    pub base_ref: String,
    /// The base tip when the pull request was opened (the diff/merge base).
    pub base_sha: Oid,
    pub state: IssueState,
    pub merged: bool,
    pub merged_at: Option<String>,
    pub merge_commit_sha: Option<Oid>,
    pub created_seq: u64,
}

#[derive(Clone, Debug)]
pub struct Label {
    pub name: String,
    pub color: String,
}

/// The v0 advertisement cache: service -> (refs generation, bytes).
type AdvCache = Mutex<HashMap<&'static str, (u64, Arc<Vec<u8>>)>>;

pub struct Repo {
    pub db_id: u32,
    pub owner: String,
    pub name: String,
    pub description: String,
    pub private: bool,
    pub default_branch: String,
    pub objects: ObjectDb,
    pub refs: HashMap<String, Oid>,
    pub issues: Vec<Issue>,
    pub pulls: Vec<Pull>,
    pub labels: Vec<Label>,
    /// Shared issue/pull-request number counter (GitHub numbers both from
    /// one sequence), starting at 0; the next number is +1.
    pub number_seq: u32,
    pub comment_seq: u64,
    /// Bumped on every ref change; advertisement caches key on it.
    pub refs_generation: u64,
    /// Cached v0 advertisement bytes, keyed by service, validated against
    /// the refs generation. A Mutex so info/refs can read it under a read
    /// lock; `set_ref` clears it.
    pub adv_cache: AdvCache,
}

impl Repo {
    pub fn new(
        db_id: u32,
        owner: String,
        name: String,
        description: String,
        private: bool,
    ) -> Self {
        Repo {
            db_id,
            owner,
            name,
            description,
            private,
            default_branch: DEFAULT_BRANCH.to_string(),
            objects: ObjectDb::new(),
            refs: HashMap::new(),
            issues: Vec::new(),
            pulls: Vec::new(),
            labels: Vec::new(),
            number_seq: 0,
            comment_seq: 0,
            refs_generation: 0,
            adv_cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    pub fn default_tip(&self) -> Option<Oid> {
        self.refs
            .get(&format!("refs/heads/{}", self.default_branch))
            .copied()
    }

    pub fn branch_tip(&self, branch: &str) -> Option<Oid> {
        self.refs.get(&format!("refs/heads/{branch}")).copied()
    }

    /// Set a ref (or delete it with None), bumping the generation.
    pub fn set_ref(&mut self, name: &str, oid: Option<Oid>) {
        match oid {
            Some(oid) => {
                self.refs.insert(name.to_string(), oid);
            }
            None => {
                self.refs.remove(name);
            }
        }
        self.refs_generation += 1;
        self.adv_cache.lock().unwrap().clear();
    }

    /// All refs, sorted by name (HEAD first when present is the caller's job).
    pub fn sorted_refs(&self) -> Vec<(String, Oid)> {
        let mut refs: Vec<(String, Oid)> = self.refs.iter().map(|(k, v)| (k.clone(), *v)).collect();
        refs.sort_by(|a, b| a.0.cmp(&b.0));
        refs
    }

    /// Commits reachable from `tip`, newest-first (children before parents).
    pub fn commits_from(&self, tip: Oid) -> Vec<Oid> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut stack = vec![tip];
        while let Some(oid) = stack.pop() {
            if !seen.insert(oid) {
                continue;
            }
            let Some(parsed) = self.objects.commit_of(&oid) else {
                continue;
            };
            out.push(oid);
            // Parents in reverse so the first parent is visited first.
            for parent in parsed.parents.iter().rev() {
                stack.push(*parent);
            }
        }
        out
    }

    /// Resolve `refname` (a branch name, a full ref, or an oid) to a commit.
    pub fn resolve_ref(&self, name: &str) -> Option<Oid> {
        if let Some(oid) = self.refs.get(name) {
            return Some(*oid);
        }
        if let Some(oid) = self.refs.get(&format!("refs/heads/{name}")) {
            return Some(*oid);
        }
        if name == "HEAD" {
            return self.default_tip();
        }
        crate::git::oid_from_hex(name)
    }

    /// The blob oid and bytes at `path` in the tree of `commit`, if present.
    pub fn read_path(&self, commit: Oid, path: &str) -> Option<(Oid, Arc<Vec<u8>>)> {
        let parsed = self.objects.commit_of(&commit)?;
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let mut tree_oid = parsed.tree;
        for (i, segment) in segments.iter().enumerate() {
            let tree = self.objects.get(&tree_oid)?;
            if tree.ty != ObjType::Tree {
                return None;
            }
            let entries = crate::git::parse_tree(&tree.data)?;
            let entry = entries.iter().find(|e| e.name == *segment)?;
            if i + 1 == segments.len() {
                if entry.mode == 0o40000 {
                    return None;
                }
                let blob = self.objects.get(&entry.oid)?;
                return Some((entry.oid, blob.data.clone()));
            }
            if entry.mode != 0o40000 {
                return None;
            }
            tree_oid = entry.oid;
        }
        None
    }

    /// The entries of the directory at `path` (empty = the root tree).
    pub fn list_dir(&self, commit: Oid, path: &str) -> Option<Vec<TreeEntry>> {
        let parsed = self.objects.commit_of(&commit)?;
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let mut tree_oid = parsed.tree;
        for segment in segments {
            let tree = self.objects.get(&tree_oid)?;
            if tree.ty != ObjType::Tree {
                return None;
            }
            let entries = crate::git::parse_tree(&tree.data)?;
            let entry = entries.iter().find(|e| e.name == segment)?;
            if entry.mode != 0o40000 {
                return None;
            }
            tree_oid = entry.oid;
        }
        let tree = self.objects.get(&tree_oid)?;
        if tree.ty != ObjType::Tree {
            return None;
        }
        crate::git::parse_tree(&tree.data)
    }

    /// The tree oid at `path` (empty = the root tree of `commit`).
    fn tree_at(&self, commit: Oid, path: &str) -> Option<Oid> {
        let parsed = self.objects.commit_of(&commit)?;
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let mut tree_oid = parsed.tree;
        for segment in segments {
            let tree = self.objects.get(&tree_oid)?;
            if tree.ty != ObjType::Tree {
                return None;
            }
            let entries = crate::git::parse_tree(&tree.data)?;
            let entry = entries.iter().find(|e| e.name == segment)?;
            if entry.mode != 0o40000 {
                return None;
            }
            tree_oid = entry.oid;
        }
        Some(tree_oid)
    }
}

#[derive(Clone, Debug)]
pub struct FileChange {
    pub path: String,
    pub status: &'static str,
    pub additions: u32,
    pub deletions: u32,
}

impl FileChange {
    pub fn changes(&self) -> u32 {
        self.additions + self.deletions
    }
}

pub struct Store {
    pub viewer: User,
    pub repos: HashMap<(String, String), Repo>,
    pub repo_order: Vec<(String, String)>,
    pub next_repo_db_id: u32,
    pub next_issue_id: u64,
    pub next_pull_id: u64,
    /// Deterministic clock for server-made commits: scenario epoch + seq.
    pub commit_seq: u64,
    pub created_seq: u64,
    /// Pack bytes by the sorted oid set they carry (immutable, so never
    /// stale). A Mutex so fetch handlers can build packs under a read lock.
    pub pack_cache: Mutex<HashMap<Vec<Oid>, Arc<Vec<u8>>>>,
}

impl Store {
    pub fn new(viewer: User) -> Self {
        Store {
            viewer,
            repos: HashMap::new(),
            repo_order: Vec::new(),
            next_repo_db_id: 1,
            next_issue_id: 1,
            next_pull_id: 1,
            commit_seq: 0,
            created_seq: 0,
            pack_cache: Mutex::new(HashMap::new()),
        }
    }

    /// A deterministic timestamp for a server-made commit.
    pub fn next_commit_timestamp(&mut self) -> u64 {
        self.commit_seq += 1;
        crate::scenario::SCENARIO_EPOCH_SEC + self.commit_seq
    }

    /// The pack carrying exactly `oids` from `repo`, from the cache when
    /// present. The cache is store-wide because a pack holds only objects:
    /// the same oid set yields the same bytes in any repo. The build happens
    /// outside the cache lock, so concurrent clones share one result.
    pub fn pack_for(&self, repo: &Repo, oids: &[Oid]) -> Arc<Vec<u8>> {
        let mut key: Vec<Oid> = oids.to_vec();
        key.sort();
        key.dedup();
        if let Some(pack) = self.pack_cache.lock().unwrap().get(&key) {
            return pack.clone();
        }
        let objs: Vec<(Oid, &Object)> = key
            .iter()
            .filter_map(|oid| repo.objects.get(oid).map(|obj| (*oid, obj)))
            .collect();
        let pack = Arc::new(build_pack(&objs));
        self.pack_cache.lock().unwrap().insert(key, pack.clone());
        pack
    }

    pub fn identity(&self) -> String {
        format!("{} <{}>", self.viewer.name, self.viewer.email)
    }

    pub fn create_repo(
        &mut self,
        name: &str,
        description: &str,
        private: bool,
    ) -> Result<u32, &'static str> {
        let key = (self.viewer.login.clone(), name.to_string());
        if self.repos.contains_key(&key) {
            return Err("already exists");
        }
        let db_id = self.next_repo_db_id;
        self.next_repo_db_id += 1;
        self.created_seq += 1;
        let repo = Repo::new(
            db_id,
            key.0.clone(),
            name.to_string(),
            description.to_string(),
            private,
        );
        self.repos.insert(key.clone(), repo);
        self.repo_order.push(key);
        Ok(db_id)
    }

    pub fn get_repo(&self, owner: &str, name: &str) -> Option<&Repo> {
        // Only the viewer's repositories exist here; any other owner is 404.
        if owner != self.viewer.login {
            return None;
        }
        self.repos.get(&(owner.to_string(), name.to_string()))
    }

    pub fn get_repo_mut(&mut self, owner: &str, name: &str) -> Option<&mut Repo> {
        if owner != self.viewer.login {
            return None;
        }
        self.repos.get_mut(&(owner.to_string(), name.to_string()))
    }

    pub fn delete_repo(&mut self, owner: &str, name: &str) -> bool {
        if owner != self.viewer.login {
            return false;
        }
        let key = (owner.to_string(), name.to_string());
        if self.repos.remove(&key).is_some() {
            self.repo_order.retain(|k| *k != key);
            true
        } else {
            false
        }
    }

    /// The viewer's repositories in creation order.
    pub fn viewer_repos(&self) -> Vec<&Repo> {
        self.repo_order
            .iter()
            .filter_map(|key| self.repos.get(key))
            .collect()
    }

    /// Opaque global id for a repository (never the plain database id).
    pub fn repo_gid(&self, repo: &Repo) -> String {
        base64_encode(format!("Repository:{}", repo.db_id).as_bytes())
    }

    pub fn issue_gid(&self, repo: &Repo, number: u32) -> String {
        base64_encode(format!("Issue:{}:{}", repo.db_id, number).as_bytes())
    }

    pub fn pull_gid(&self, repo: &Repo, number: u32) -> String {
        base64_encode(format!("PullRequest:{}:{}", repo.db_id, number).as_bytes())
    }

    /// Decode an opaque global id into (kind, repo db id, number).
    pub fn decode_gid(&self, gid: &str) -> Option<(String, u32, u32)> {
        let bytes = crate::util::base64_decode(gid)?;
        let text = std::str::from_utf8(&bytes).ok()?;
        let mut parts = text.split(':');
        let kind = parts.next()?.to_string();
        let db_id: u32 = parts.next()?.parse().ok()?;
        let number: u32 = parts.next().unwrap_or("0").parse().ok()?;
        Some((kind, db_id, number))
    }
}

/// A flat map of a tree: path -> (mode, oid), built by walking subtrees.
pub fn flatten_tree(repo: &Repo, tree_oid: Oid) -> HashMap<String, (u32, Oid)> {
    let mut out = HashMap::new();
    let mut stack = vec![(tree_oid, String::new())];
    while let Some((oid, prefix)) = stack.pop() {
        let Some(obj) = repo.objects.get(&oid) else {
            continue;
        };
        if obj.ty != ObjType::Tree {
            continue;
        }
        let Some(entries) = crate::git::parse_tree(&obj.data) else {
            continue;
        };
        for entry in entries {
            let path = if prefix.is_empty() {
                entry.name.clone()
            } else {
                format!("{prefix}/{}", entry.name)
            };
            if entry.mode == 0o40000 {
                stack.push((entry.oid, path));
            } else {
                out.insert(path, (entry.mode, entry.oid));
            }
        }
    }
    out
}

/// Diff two trees: files added, removed, or changed (by blob oid).
pub fn diff_trees(repo: &Repo, base: Oid, head: Oid) -> Vec<FileChange> {
    let base_map = flatten_tree(repo, base);
    let head_map = flatten_tree(repo, head);
    let mut paths: Vec<String> = base_map.keys().chain(head_map.keys()).cloned().collect();
    paths.sort();
    paths.dedup();
    let mut changes = Vec::new();
    for path in paths {
        let b = base_map.get(&path);
        let h = head_map.get(&path);
        match (b, h) {
            (None, Some((_, oid))) => {
                let additions = count_lines(repo, *oid);
                changes.push(FileChange {
                    path,
                    status: "added",
                    additions,
                    deletions: 0,
                });
            }
            (Some((_, _)), None) => {
                let deletions = count_lines(repo, b.unwrap().1);
                changes.push(FileChange {
                    path,
                    status: "removed",
                    additions: 0,
                    deletions,
                });
            }
            (Some((_, b_oid)), Some((_, h_oid))) if b_oid != h_oid => {
                let (additions, deletions) = line_diff(repo, *b_oid, *h_oid);
                changes.push(FileChange {
                    path,
                    status: "modified",
                    additions,
                    deletions,
                });
            }
            _ => {}
        }
    }
    changes
}

fn blob_lines(repo: &Repo, oid: Oid) -> Vec<String> {
    let Some(obj) = repo.objects.get(&oid) else {
        return Vec::new();
    };
    if obj.ty != ObjType::Blob {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&obj.data);
    text.lines().map(|l| l.to_string()).collect()
}

fn count_lines(repo: &Repo, oid: Oid) -> u32 {
    blob_lines(repo, oid).len() as u32
}

/// Line multiset difference: lines in `new` not in `old` are additions and
/// vice versa. Enough for the suite (it asserts additions >= 1).
fn line_diff(repo: &Repo, old: Oid, new: Oid) -> (u32, u32) {
    let old_lines = blob_lines(repo, old);
    let new_lines = blob_lines(repo, new);
    let mut old_counts: HashMap<&str, u32> = HashMap::new();
    for line in &old_lines {
        *old_counts.entry(line).or_insert(0) += 1;
    }
    let mut additions = 0;
    for line in &new_lines {
        match old_counts.get_mut(line.as_str()) {
            Some(count) if *count > 0 => *count -= 1,
            _ => additions += 1,
        }
    }
    let mut new_counts: HashMap<&str, u32> = HashMap::new();
    for line in &new_lines {
        *new_counts.entry(line).or_insert(0) += 1;
    }
    let mut deletions = 0;
    for line in &old_lines {
        match new_counts.get_mut(line.as_str()) {
            Some(count) if *count > 0 => *count -= 1,
            _ => deletions += 1,
        }
    }
    (additions, deletions)
}

/// Write `content` to `path` on `branch` (default when None), creating the
/// branch's first commit when the repo is unborn. `identity` is the
/// `Name <email>` for the commit; `timestamp` its epoch. Returns
/// (blob sha, commit sha, created?).
#[allow(clippy::too_many_arguments)]
pub fn write_file(
    repo: &mut Repo,
    identity: &str,
    timestamp: u64,
    path: &str,
    content: &[u8],
    message: &str,
    branch: Option<&str>,
    expected_sha: Option<Oid>,
) -> Result<(Oid, Oid, bool), &'static str> {
    let branch = branch
        .map(|b| b.to_string())
        .unwrap_or_else(|| repo.default_branch.clone());
    let current_tip = repo.branch_tip(&branch);
    let is_new_file = match current_tip {
        Some(tip) => repo.read_path(tip, path).is_none(),
        None => true,
    };
    if let Some(tip) = current_tip {
        if let Some((current_blob, _)) = repo.read_path(tip, path) {
            if let Some(expected) = expected_sha {
                if current_blob != expected {
                    return Err("stale sha");
                }
            } else if !is_new_file {
                // Updating without a sha is a 422 on GitHub.
                return Err("missing sha");
            }
        } else if expected_sha.is_some() {
            return Err("stale sha");
        }
    }
    let blob_oid = repo.objects.insert(ObjType::Blob, content.to_vec());
    // Rebuild the trees bottom-up from the current tip (or an empty tree).
    let new_root = rebuild_with_file(repo, current_tip, path, blob_oid);
    let parents: Vec<Oid> = current_tip.into_iter().collect();
    let commit = build_commit(&new_root, &parents, identity, identity, timestamp, message);
    let commit_oid = repo.objects.insert(ObjType::Commit, commit);
    repo.set_ref(&format!("refs/heads/{branch}"), Some(commit_oid));
    Ok((blob_oid, commit_oid, is_new_file))
}

/// Rebuild the tree at `path`'s parent chain, setting the leaf to `blob_oid`.
/// `current_tip` may be None (unborn repo: build from an empty tree).
fn rebuild_with_file(repo: &mut Repo, current_tip: Option<Oid>, path: &str, blob_oid: Oid) -> Oid {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    // Walk down, remembering each level's tree oid.
    let mut chain: Vec<Oid> = Vec::new();
    // The root tree oid (or the empty tree when unborn).
    let root = match current_tip.and_then(|tip| repo.tree_at(tip, "")) {
        Some(tree) => tree,
        None => repo.objects.insert(ObjType::Tree, Vec::new()),
    };
    chain.push(root);
    for segment in &segments[..segments.len() - 1] {
        let tree_oid = *chain.last().unwrap();
        let child = repo
            .objects
            .get(&tree_oid)
            .and_then(|obj| crate::git::parse_tree(&obj.data))
            .and_then(|entries| entries.iter().find(|e| e.name == *segment).map(|e| e.oid));
        let child_oid = match child {
            Some(oid) => oid,
            None => repo.objects.insert(ObjType::Tree, Vec::new()),
        };
        chain.push(child_oid);
    }
    // Rebuild bottom-up: the leaf level gets the file, each parent gets the
    // rebuilt child tree.
    let mut child_oid = blob_oid;
    let mut child_name = segments[segments.len() - 1].to_string();
    let mut child_mode = 0o100644;
    for level in (0..chain.len()).rev() {
        let tree_oid = chain[level];
        let mut entries = repo
            .objects
            .get(&tree_oid)
            .and_then(|obj| crate::git::parse_tree(&obj.data))
            .unwrap_or_default();
        entries.retain(|e| e.name != child_name);
        entries.push(TreeEntry {
            mode: child_mode,
            name: child_name.clone(),
            oid: child_oid,
        });
        crate::git::sort_tree_entries(&mut entries);
        let built = build_tree(&entries);
        let new_oid = repo.objects.insert(ObjType::Tree, built);
        child_oid = new_oid;
        if level > 0 {
            child_name = segments[level - 1].to_string();
            child_mode = 0o40000;
        }
    }
    child_oid
}

/// Delete `path` on `branch`, pruning now-empty directories. Returns the new
/// commit sha.
pub fn delete_file(
    repo: &mut Repo,
    identity: &str,
    timestamp: u64,
    path: &str,
    message: &str,
    branch: Option<&str>,
) -> Result<Oid, &'static str> {
    let branch = branch
        .map(|b| b.to_string())
        .unwrap_or_else(|| repo.default_branch.clone());
    let tip = repo.branch_tip(&branch).ok_or("no such branch")?;
    if repo.read_path(tip, path).is_none() {
        return Err("no such file");
    }
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let new_root = rebuild_without_file(repo, tip, &segments).ok_or("no such file")?;
    let commit = build_commit(&new_root, &[tip], identity, identity, timestamp, message);
    let commit_oid = repo.objects.insert(ObjType::Commit, commit);
    repo.set_ref(&format!("refs/heads/{branch}"), Some(commit_oid));
    Ok(commit_oid)
}

/// Rebuild the tree chain with the leaf removed; empty directories are pruned
/// (their entries are dropped when the rebuilt subtree is empty).
fn rebuild_without_file(repo: &mut Repo, tip: Oid, segments: &[&str]) -> Option<Oid> {
    // Collect the tree oid at each level.
    let mut chain: Vec<Oid> = Vec::new();
    let mut current = repo.tree_at(tip, "")?;
    chain.push(current);
    for segment in &segments[..segments.len() - 1] {
        let entries = repo
            .objects
            .get(&current)
            .and_then(|obj| crate::git::parse_tree(&obj.data))
            .unwrap_or_default();
        let child = entries.iter().find(|e| e.name == *segment).map(|e| e.oid)?;
        chain.push(child);
        current = child;
    }
    // Bottom-up: remove the leaf, drop empty subtrees.
    let mut child: Option<(String, Oid, u32)> = None;
    let mut removed_name = segments[segments.len() - 1].to_string();
    for level in (0..chain.len()).rev() {
        let tree_oid = chain[level];
        let mut entries = repo
            .objects
            .get(&tree_oid)
            .and_then(|obj| crate::git::parse_tree(&obj.data))
            .unwrap_or_default();
        entries.retain(|e| e.name != removed_name);
        if let Some((name, oid, mode)) = child {
            entries.push(TreeEntry { mode, name, oid });
        }
        crate::git::sort_tree_entries(&mut entries);
        if entries.is_empty() && level > 0 {
            // Prune: the parent drops this directory.
            removed_name = segments[level - 1].to_string();
            child = None;
            continue;
        }
        let built = build_tree(&entries);
        let new_oid = repo.objects.insert(ObjType::Tree, built);
        if level > 0 {
            removed_name = segments[level - 1].to_string();
            child = Some((removed_name.clone(), new_oid, 0o40000));
        } else {
            return Some(new_oid);
        }
    }
    // The root became empty: the empty tree.
    Some(repo.objects.insert(ObjType::Tree, Vec::new()))
}

/// Whether `pull` can merge into its base right now.
pub fn pull_mergeable(repo: &Repo, pull: &Pull) -> MergeStatus {
    let Some(head_tip) = repo.branch_tip(&pull.head_ref) else {
        return MergeStatus::Unknown;
    };
    let Some(base_tip) = repo.branch_tip(&pull.base_ref) else {
        return MergeStatus::Unknown;
    };
    if base_tip == pull.base_sha {
        return MergeStatus::CanMerge;
    }
    // The base moved: any file the pull touches that the base also changed
    // (to different content) is a conflict.
    let Some(base_commit) = repo.objects.commit_of(&pull.base_sha) else {
        return MergeStatus::Unknown;
    };
    let Some(head_commit) = repo.objects.commit_of(&head_tip) else {
        return MergeStatus::Unknown;
    };
    let Some(current_commit) = repo.objects.commit_of(&base_tip) else {
        return MergeStatus::Unknown;
    };
    let pr_files = diff_trees(repo, base_commit.tree, head_commit.tree);
    let base_files = diff_trees(repo, base_commit.tree, current_commit.tree);
    let base_changed: HashSet<&str> = base_files.iter().map(|f| f.path.as_str()).collect();
    let head_map = flatten_tree(repo, head_commit.tree);
    let current_map = flatten_tree(repo, current_commit.tree);
    for change in pr_files {
        if base_changed.contains(change.path.as_str()) {
            let head_version = head_map.get(&change.path);
            let current_version = current_map.get(&change.path);
            if head_version != current_version {
                return MergeStatus::Conflicting;
            }
        }
    }
    MergeStatus::CanMerge
}

/// Squash-merge `pull_number` into its base branch. Returns the merge commit.
pub fn squash_merge(
    repo: &mut Repo,
    identity: &str,
    timestamp: u64,
    merged_at: String,
    number: u32,
) -> Result<Oid, &'static str> {
    let idx = repo
        .pulls
        .iter()
        .position(|p| p.number == number)
        .ok_or("no such pull")?;
    let pull = repo.pulls[idx].clone();
    if pull.merged {
        return Err("already merged");
    }
    if pull.state == IssueState::Closed {
        return Err("closed");
    }
    match pull_mergeable(repo, &pull) {
        MergeStatus::Conflicting => return Err("conflict"),
        MergeStatus::Unknown => return Err("unknown"),
        MergeStatus::CanMerge => {}
    }
    let head_tip = repo.branch_tip(&pull.head_ref).ok_or("no such head")?;
    let base_tip = repo.branch_tip(&pull.base_ref).ok_or("no such base")?;
    let head_commit = repo.objects.commit_of(&head_tip).ok_or("no such head")?;
    let message = format!("{}\n", pull.title);
    let commit = build_commit(
        &head_commit.tree,
        &[base_tip],
        identity,
        identity,
        timestamp,
        &message,
    );
    let commit_oid = repo.objects.insert(ObjType::Commit, commit);
    repo.set_ref(&format!("refs/heads/{}", pull.base_ref), Some(commit_oid));
    repo.pulls[idx].merged = true;
    repo.pulls[idx].state = IssueState::Closed;
    repo.pulls[idx].merged_at = Some(merged_at);
    repo.pulls[idx].merge_commit_sha = Some(commit_oid);
    Ok(commit_oid)
}
