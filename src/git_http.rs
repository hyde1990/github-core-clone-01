//! Smart HTTP git transport: `info/refs` (v0 + v2 advertisements),
//! `git-upload-pack` (v2 + v0 fetch) and `git-receive-pack` (v0 push).
//! The v0 advertisement is cached per refs generation; fetch packs are
//! cached by their exact sorted oid set.

use crate::git::{
    decode_pkt_prefix, oid_from_hex, oid_hex, parse_pack, pkt_line, plan_fetch, sideband, Pkt,
    PKT_DELIM, PKT_FLUSH, ZERO_OID,
};
use crate::http::{Request, Response};
use crate::store::{Repo, Store};
use std::sync::{Arc, RwLock};

const CAPS_UPLOAD_V0: &str = "multi_ack thin-pack side-band side-band-64k ofs-delta shallow deepen-since deepen-not deepen-relative no-progress include-tag multi_ack_detailed allow-tip-sha1-in-want allow-reachable-sha1-in-want no-done filter symref=HEAD:refs/heads/main agent=forge/1.0";
const CAPS_RECEIVE_V0: &str = "report-status report-status-v2 delete-refs side-band-64k quiet atomic ofs-delta symref=HEAD:refs/heads/main agent=forge/1.0";

pub fn handle(store: &RwLock<Store>, req: &Request) -> Response {
    let path = req.path.trim_end_matches('/');
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() < 2 {
        return git_not_found();
    }
    let repo_seg = segments[1];
    let Some(repo_name) = repo_seg.strip_suffix(".git") else {
        return git_not_found();
    };
    let owner = segments[0];
    let tail = &segments[2..];
    match (req.method.as_str(), tail) {
        ("GET", ["info", "refs"]) => info_refs(store, req, owner, repo_name),
        ("POST", ["git-upload-pack"]) => upload_pack(store, req, owner, repo_name),
        ("POST", ["git-receive-pack"]) => receive_pack(store, req, owner, repo_name),
        _ => git_not_found(),
    }
}

fn git_not_found() -> Response {
    Response::new(404, b"Not Found\n".to_vec()).header("content-type", "text/plain")
}

fn repo_404() -> Response {
    Response::new(404, b"Not Found\n".to_vec()).header("content-type", "text/plain")
}

// ---------------------------------------------------------------------------
// Pkt-line helpers.

/// Decode one flush-terminated group; on any error, an empty group that
/// consumes the whole input.
fn packets(data: &[u8]) -> (Vec<Pkt>, usize) {
    match decode_pkt_prefix(data) {
        Ok((pkts, consumed)) => (pkts, consumed),
        Err(_) => (Vec::new(), data.len()),
    }
}

/// The data payloads of a packet group, as UTF-8 lines (no trailing \n).
fn data_lines(pkts: &[Pkt]) -> Vec<String> {
    pkts.iter()
        .filter_map(|pkt| match pkt {
            Pkt::Data(bytes) => String::from_utf8(bytes.clone())
                .ok()
                .map(|s| s.trim_end_matches('\n').to_string()),
            _ => None,
        })
        .collect()
}

fn has_delim(pkts: &[Pkt]) -> bool {
    pkts.iter().any(|pkt| matches!(pkt, Pkt::Delim))
}

// ---------------------------------------------------------------------------
// info/refs.

fn info_refs(store: &RwLock<Store>, req: &Request, owner: &str, name: &str) -> Response {
    let service = req.query_param("service").unwrap_or("").to_string();
    let store = store.read().unwrap();
    let Some(repo) = store.get_repo(owner, name) else {
        return repo_404();
    };
    match service.as_str() {
        "git-upload-pack" => {
            let is_v2 = req
                .header("git-protocol")
                .map(|v| v.contains("version=2"))
                .unwrap_or(false);
            if is_v2 {
                let mut body = Vec::new();
                body.extend_from_slice(&pkt_line(b"# service=git-upload-pack\n"));
                body.extend_from_slice(PKT_FLUSH);
                body.extend_from_slice(&pkt_line(b"version 2\n"));
                body.extend_from_slice(&pkt_line(b"agent=forge/1.0\n"));
                body.extend_from_slice(&pkt_line(b"ls-refs=unborn\n"));
                body.extend_from_slice(&pkt_line(b"fetch=shallow filter\n"));
                body.extend_from_slice(&pkt_line(b"object-format=sha1\n"));
                body.extend_from_slice(PKT_FLUSH);
                Response::new(200, body)
                    .header(
                        "content-type",
                        "application/x-git-upload-pack-advertisement",
                    )
                    .header("cache-control", "no-cache")
            } else {
                let body = advertisement_v0(repo, false);
                Response::new(200, body)
                    .header(
                        "content-type",
                        "application/x-git-upload-pack-advertisement",
                    )
                    .header("cache-control", "no-cache")
            }
        }
        "git-receive-pack" => {
            let body = advertisement_v0(repo, true);
            Response::new(200, body)
                .header(
                    "content-type",
                    "application/x-git-receive-pack-advertisement",
                )
                .header("cache-control", "no-cache")
        }
        _ => git_not_found(),
    }
}

/// The v0 advertisement: `# service=...` + refs + capabilities on the first
/// ref line (or the `capabilities^{}` pseudo-ref when there are no refs).
/// Annotated tags get a `^{}` peeled line after their tag line. HEAD is
/// listed first when the default branch exists. Cached per refs generation.
fn advertisement_v0(repo: &Repo, receive: bool) -> Vec<u8> {
    let service = if receive {
        "git-receive-pack"
    } else {
        "git-upload-pack"
    };
    let key: &'static str = if receive { "receive" } else { "upload" };
    if let Some((generation, cached)) = repo.adv_cache.lock().unwrap().get(key) {
        if *generation == repo.refs_generation {
            return (**cached).clone();
        }
    }
    let caps = if receive {
        CAPS_RECEIVE_V0
    } else {
        CAPS_UPLOAD_V0
    };
    let mut body = Vec::new();
    body.extend_from_slice(&pkt_line(format!("# service={service}\n").as_bytes()));
    body.extend_from_slice(PKT_FLUSH);
    let mut refs = repo.sorted_refs();
    if let Some(tip) = repo.default_tip() {
        refs.insert(0, ("HEAD".to_string(), tip));
    }
    let mut first = true;
    for (name, oid) in &refs {
        let mut line = format!("{} {name}", oid_hex(oid));
        if first {
            line.push('\0');
            line.push_str(caps);
            first = false;
        }
        line.push('\n');
        body.extend_from_slice(&pkt_line(line.as_bytes()));
        // Peeled line for annotated tags.
        if name.starts_with("refs/tags/") {
            if let Some(peeled) = repo.objects.peel_to_commit(oid) {
                if peeled != *oid {
                    body.extend_from_slice(&pkt_line(
                        format!("{} {name}^{{}}\n", oid_hex(&peeled)).as_bytes(),
                    ));
                }
            }
        }
    }
    if refs.is_empty() {
        // No refs: capabilities ride the `capabilities^{}` pseudo-ref.
        body.extend_from_slice(&pkt_line(
            format!("{} capabilities^{{}}\0{caps}\n", oid_hex(&ZERO_OID)).as_bytes(),
        ));
    }
    body.extend_from_slice(PKT_FLUSH);
    repo.adv_cache
        .lock()
        .unwrap()
        .insert(key, (repo.refs_generation, Arc::new(body.clone())));
    body
}

// ---------------------------------------------------------------------------
// upload-pack.

fn upload_pack(store: &RwLock<Store>, req: &Request, owner: &str, name: &str) -> Response {
    let is_v2 = req
        .header("git-protocol")
        .map(|v| v.contains("version=2"))
        .unwrap_or(false);
    if is_v2 {
        upload_pack_v2(store, req, owner, name)
    } else {
        upload_pack_v0(store, req, owner, name)
    }
}

fn upload_pack_v2(store: &RwLock<Store>, req: &Request, owner: &str, name: &str) -> Response {
    let store_guard = store.read().unwrap();
    let Some(repo) = store_guard.get_repo(owner, name) else {
        return repo_404();
    };
    let (pkts, _) = packets(&req.body);
    let lines = data_lines(&pkts);
    let command = lines
        .iter()
        .find_map(|line| line.strip_prefix("command=").map(|cmd| cmd.to_string()))
        .unwrap_or_default();
    let args: Vec<String> = if has_delim(&pkts) {
        lines
            .into_iter()
            .skip_while(|line| !line.starts_with("command="))
            .skip(1)
            .collect()
    } else {
        Vec::new()
    };
    match command.as_str() {
        "ls-refs" => {
            let body = ls_refs_response(repo, &args);
            Response::new(200, body).header("content-type", "application/x-git-upload-pack-result")
        }
        "fetch" => {
            let body = fetch_v2_response(&store_guard, repo, &args);
            Response::new(200, body).header("content-type", "application/x-git-upload-pack-result")
        }
        _ => Response::new(400, b"unknown command\n".to_vec()).header("content-type", "text/plain"),
    }
}

fn ls_refs_response(repo: &Repo, args: &[String]) -> Vec<u8> {
    let mut peel = false;
    let mut symrefs = false;
    let mut prefixes: Vec<String> = Vec::new();
    for line in args {
        if line == "peel" {
            peel = true;
        } else if line == "symrefs" {
            symrefs = true;
        } else if let Some(prefix) = line.strip_prefix("ref-prefix ") {
            prefixes.push(prefix.to_string());
        }
    }
    let mut refs = repo.sorted_refs();
    if let Some(tip) = repo.default_tip() {
        refs.insert(0, ("HEAD".to_string(), tip));
    }
    let mut out = Vec::new();
    for (name, oid) in refs {
        if !prefixes.is_empty() && !prefixes.iter().any(|p| name.starts_with(p.as_str())) {
            continue;
        }
        let mut line = format!("{} {name}", oid_hex(&oid));
        if peel {
            if let Some(peeled) = repo.objects.peel_to_commit(&oid) {
                if peeled != oid {
                    line.push_str(&format!(" peeled:{}", oid_hex(&peeled)));
                }
            }
        }
        if symrefs && name == "HEAD" {
            line.push_str(" symref-target:refs/heads/main");
        }
        line.push('\n');
        out.extend_from_slice(&pkt_line(line.as_bytes()));
    }
    out.extend_from_slice(PKT_FLUSH);
    out
}

/// A parsed v2/v0 fetch request.
struct FetchRequest {
    wants: Vec<crate::git::Oid>,
    haves: Vec<crate::git::Oid>,
    shallow: Vec<crate::git::Oid>,
    deepen: Option<u32>,
}

fn parse_fetch_args(lines: &[String]) -> FetchRequest {
    let mut wants = Vec::new();
    let mut haves = Vec::new();
    let mut shallow = Vec::new();
    let mut deepen = None;
    for line in lines {
        if let Some(oid) = line
            .strip_prefix("want ")
            .and_then(|h| oid_from_hex(h.trim()))
        {
            wants.push(oid);
        } else if let Some(oid) = line
            .strip_prefix("have ")
            .and_then(|h| oid_from_hex(h.trim()))
        {
            haves.push(oid);
        } else if let Some(oid) = line
            .strip_prefix("shallow ")
            .and_then(|h| oid_from_hex(h.trim()))
        {
            shallow.push(oid);
        } else if let Some(n) = line
            .strip_prefix("deepen ")
            .and_then(|n| n.trim().parse::<u32>().ok())
        {
            deepen = Some(n);
        }
    }
    FetchRequest {
        wants,
        haves,
        shallow,
        deepen,
    }
}

fn fetch_v2_response(store: &Store, repo: &Repo, args: &[String]) -> Vec<u8> {
    let request = parse_fetch_args(args);
    let plan = plan_fetch(
        &repo.objects,
        &request.wants,
        &request.haves,
        &request.shallow,
        request.deepen,
    );
    let mut out = Vec::new();
    // acknowledgments section.
    out.extend_from_slice(&pkt_line(b"acknowledgments\n"));
    out.extend_from_slice(&pkt_line(b"NAK\n"));
    out.extend_from_slice(PKT_DELIM);
    // shallow-info section, only when there is something to say.
    if !plan.shallow.is_empty() || !plan.unshallow.is_empty() {
        out.extend_from_slice(&pkt_line(b"shallow-info\n"));
        for oid in &plan.shallow {
            out.extend_from_slice(&pkt_line(format!("shallow {}\n", oid_hex(oid)).as_bytes()));
        }
        for oid in &plan.unshallow {
            out.extend_from_slice(&pkt_line(
                format!("unshallow {}\n", oid_hex(oid)).as_bytes(),
            ));
        }
        out.extend_from_slice(PKT_DELIM);
    }
    // packfile section, only when objects are new.
    if !plan.objects.is_empty() {
        out.extend_from_slice(&pkt_line(b"packfile\n"));
        let pack = store.pack_for(repo, &plan.objects);
        out.extend_from_slice(&sideband(1, &pack));
        out.extend_from_slice(PKT_FLUSH);
    }
    out.extend_from_slice(PKT_FLUSH);
    out
}

fn upload_pack_v0(store: &RwLock<Store>, req: &Request, owner: &str, name: &str) -> Response {
    let store_guard = store.read().unwrap();
    let Some(repo) = store_guard.get_repo(owner, name) else {
        return repo_404();
    };
    // Group 1: `want`/`shallow`/`deepen` lines up to the first flush;
    // group 2: `have` lines + `done` up to the second flush.
    let (pkts1, consumed1) = packets(&req.body);
    let (pkts2, _) = packets(&req.body[consumed1..]);
    let mut lines = data_lines(&pkts1);
    lines.extend(data_lines(&pkts2));
    let request = parse_fetch_args(&lines);
    let plan = plan_fetch(
        &repo.objects,
        &request.wants,
        &request.haves,
        &request.shallow,
        request.deepen,
    );
    let mut out = Vec::new();
    // NAK, then shallow/unshallow lines, then the sideband pack.
    out.extend_from_slice(&pkt_line(b"NAK\n"));
    for oid in &plan.shallow {
        out.extend_from_slice(&pkt_line(format!("shallow {}\n", oid_hex(oid)).as_bytes()));
    }
    for oid in &plan.unshallow {
        out.extend_from_slice(&pkt_line(
            format!("unshallow {}\n", oid_hex(oid)).as_bytes(),
        ));
    }
    if !plan.objects.is_empty() {
        let pack = store_guard.pack_for(repo, &plan.objects);
        out.extend_from_slice(&sideband(1, &pack));
    }
    out.extend_from_slice(PKT_FLUSH);
    Response::new(200, out).header("content-type", "application/x-git-upload-pack-result")
}

// ---------------------------------------------------------------------------
// receive-pack.

fn receive_pack(store: &RwLock<Store>, req: &Request, owner: &str, name: &str) -> Response {
    let mut store_guard = store.write().unwrap();
    let Some(repo) = store_guard.get_repo_mut(owner, name) else {
        return repo_404();
    };
    // Parse the command list: `<old> <new> <ref>\0<caps>` lines until flush,
    // then the pack (a zlib stream).
    let (pkts, pack_start) = packets(&req.body);
    let mut commands: Vec<(crate::git::Oid, crate::git::Oid, String)> = Vec::new();
    for line in data_lines(&pkts) {
        let mut parts = line.split(' ');
        let old = parts.next().and_then(oid_from_hex);
        let new = parts.next().and_then(oid_from_hex);
        let ref_name = parts
            .next()
            .map(|s| s.split('\0').next().unwrap_or("").to_string());
        if let (Some(old), Some(new), Some(ref_name)) = (old, new, ref_name) {
            commands.push((old, new, ref_name));
        }
    }
    // Apply the pack (if any) before the ref updates. A packfile is a
    // binary container ("PACK" + objects + trailer), not a zlib stream:
    // each object's body is inflated inside parse_pack.
    let mut unpack_ok = true;
    if pack_start < req.body.len() && apply_pack(repo, &req.body[pack_start..]).is_err() {
        unpack_ok = false;
    }
    // Apply the ref updates and build the report.
    let mut report = String::new();
    if unpack_ok {
        report.push_str("unpack ok\n");
        for (old, new, ref_name) in &commands {
            let current = repo.refs.get(ref_name).copied().unwrap_or(ZERO_OID);
            if *new == ZERO_OID {
                // Delete.
                if current != *old && *old != ZERO_OID {
                    report.push_str(&format!("ng {ref_name} non-fast-forward\n"));
                    continue;
                }
                repo.set_ref(ref_name, None);
                report.push_str(&format!("ok {ref_name}\n"));
                continue;
            }
            // Create or update.
            if *old == ZERO_OID {
                if repo.refs.contains_key(ref_name) {
                    report.push_str(&format!("ng {ref_name} non-fast-forward\n"));
                    continue;
                }
            } else if current != *old || !repo.objects.is_ancestor_of(old, new) {
                report.push_str(&format!("ng {ref_name} non-fast-forward\n"));
                continue;
            }
            repo.set_ref(ref_name, Some(*new));
            report.push_str(&format!("ok {ref_name}\n"));
        }
    } else {
        report.push_str("unpack error\n");
        for (_, _, ref_name) in &commands {
            report.push_str(&format!("ng {ref_name} unpack failed\n"));
        }
    }
    // The report goes on sideband band 1, as pkt-lines, then flush.
    let mut out = Vec::new();
    out.extend_from_slice(&sideband(1, report.as_bytes()));
    out.extend_from_slice(PKT_FLUSH);
    Response::new(200, out).header("content-type", "application/x-git-receive-pack-result")
}

/// Insert the objects from a parsed pack into the repo's object database.
fn apply_pack(repo: &mut Repo, pack: &[u8]) -> Result<(), String> {
    let objects = parse_pack(pack)?;
    for (oid, obj_type, data) in objects {
        repo.objects.insert_typed(obj_type, data, oid);
    }
    Ok(())
}
