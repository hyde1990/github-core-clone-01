//! GitHub REST v3 (`/api/v3`): repositories, branches, commits, git refs,
//! contents, labels, issues, comments and pull requests.
//!
//! Round 15 rewrite, written from the kit's conformance suite
//! (`sdk/conformance/checks-rest.ts`, `checks-mcp.ts`, `checks-graphql.ts`)
//! and the other modules' public interfaces. The design is my own:
//! one `handle` entry that dispatches on path segments, small per-resource
//! handlers that take the store lock, and shared serializers
//! (`repo_json`, `issue_json`, `pull_json`) that the MCP module reuses so
//! the two surfaces can never drift apart.
//!
//! Contract notes (all from the suite):
//! * every reply is `application/json` with a non-empty
//!   `x-github-request-id`; unknown routes are JSON 404s; a malformed JSON
//!   body is a 400 with `{"message":"malformed JSON body"}`.
//! * a repository belongs to the token's login: the same name under another
//!   owner is a 404, never a 403 (enforced by `Store::get_repo`).
//! * lists paginate on `page`/`per_page` (default 30) and carry a `Link`
//!   header with `rel="next"` while more pages remain.
//! * issues and pull requests share one number sequence per repository.
//! * a duplicate repository name is a 422; an unknown label on issue
//!   create is a 422; a stale contents sha is a 409; a conflicted merge
//!   is a 405; an unknown pull request merge is a 404.

use crate::git::{oid_from_hex, oid_hex, ObjType};
use crate::http::{Request, Response};
use crate::json::{obj, Json};
use crate::scenario::SCENARIO_EPOCH_SEC;
use crate::store::{
    delete_file, diff_trees, pull_mergeable, squash_merge, Issue, IssueState, Label, MergeStatus,
    Pull, Repo, Store,
};
use crate::util::{base64_decode, base64_encode, iso_from_epoch};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

/// Monotonic request-id counter; every reply gets `req-<hex>`.
static REQUEST_SEQ: AtomicU64 = AtomicU64::new(1);

pub fn next_request_id() -> String {
    format!("req-{:016x}", REQUEST_SEQ.fetch_add(1, Ordering::Relaxed))
}

// ---------------------------------------------------------------------------
// Reply helpers.

fn json_reply(status: u16, value: Json) -> Response {
    Response::new(status, value.stringify().into_bytes())
        .header("content-type", "application/json; charset=utf-8")
        .header("x-github-request-id", &next_request_id())
}

fn error_reply(status: u16, message: &str) -> Response {
    json_reply(
        status,
        obj(vec![("message", Json::Str(message.to_string()))]),
    )
}

/// Parse the request body as JSON. An empty body counts as an empty object;
/// anything present but unparseable is the protocol 400.
fn parse_body(req: &Request) -> Result<Json, Response> {
    let text = String::from_utf8_lossy(&req.body);
    let text = text.trim();
    if text.is_empty() {
        return Ok(obj(vec![]));
    }
    match crate::json::parse(text) {
        Ok(value) => Ok(value),
        Err(_) => Err(error_reply(400, "malformed JSON body")),
    }
}

fn body_str<'a>(body: &'a Json, field: &str) -> Option<&'a str> {
    body.get(field)?.as_str()
}

fn body_bool(body: &Json, field: &str, default: bool) -> bool {
    body.get(field).and_then(|v| v.as_bool()).unwrap_or(default)
}

/// `page`/`per_page` from the query string (defaults 1 and 30).
fn page_params(req: &Request) -> (usize, usize) {
    let page = req
        .query_param("page")
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|p| *p >= 1)
        .unwrap_or(1);
    let per_page = req
        .query_param("per_page")
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|p| *p >= 1)
        .unwrap_or(30);
    (page, per_page)
}

/// Slice `items` to the requested page and build the `Link` header value
/// (`rel="next"` only when a later page exists). The URL is absolute, on
/// the request's own host, so the suite's same-origin check passes.
fn page_link(req: &Request, base: &str, page: usize, per_page: usize, total: usize) -> String {
    let mut links: Vec<String> = Vec::new();
    let host = req.host();
    if page * per_page < total {
        links.push(format!(
            "<http://{host}{base}?per_page={per_page}&page={}>; rel=\"next\"",
            page + 1
        ));
    }
    if page > 1 {
        links.push(format!(
            "<http://{host}{base}?per_page={per_page}&page={}>; rel=\"prev\"",
            page - 1
        ));
        links.push(format!(
            "<http://{host}{base}?per_page={per_page}&page=1>; rel=\"first\""
        ));
    }
    let last = total.div_ceil(per_page).max(1);
    if last > 1 {
        links.push(format!(
            "<http://{host}{base}?per_page={per_page}&page={last}>; rel=\"last\""
        ));
    }
    links.join(", ")
}

/// Paginate an already-ordered list of JSON values.
fn paginate(req: &Request, base: &str, items: Vec<Json>) -> (Vec<Json>, Option<String>) {
    let (page, per_page) = page_params(req);
    let total = items.len();
    let start = (page - 1) * per_page;
    let page_items: Vec<Json> = items.into_iter().skip(start).take(per_page).collect();
    let link = if total > per_page || page > 1 {
        Some(page_link(req, base, page, per_page, total))
    } else {
        None
    };
    (page_items, link)
}

fn list_reply(req: &Request, base: &str, items: Vec<Json>) -> Response {
    let (page_items, link) = paginate(req, base, items);
    let mut response = json_reply(200, Json::Arr(page_items));
    if let Some(link) = link {
        response.headers.push(("link".to_string(), link));
    }
    response
}

// ---------------------------------------------------------------------------
// Serializers. These are the canonical shapes; the MCP module calls the
// same three functions so both surfaces render identical objects.

fn user_json(login: &str) -> Json {
    obj(vec![
        ("login", Json::Str(login.to_string())),
        ("id", Json::Int(1000)),
        (
            "node_id",
            Json::Str(base64_encode(format!("User:{login}").as_bytes())),
        ),
        ("type", Json::Str("User".to_string())),
        ("site_admin", Json::Bool(false)),
        (
            "avatar_url",
            Json::Str(format!("https://avatars.githubusercontent.com/{login}")),
        ),
        ("html_url", Json::Str(format!("https://github.com/{login}"))),
        (
            "url",
            Json::Str(format!("https://api.github.com/users/{login}")),
        ),
    ])
}

pub fn repo_json(repo: &Repo) -> Json {
    let open_issues = repo
        .issues
        .iter()
        .filter(|issue| issue.state == IssueState::Open)
        .count() as i64;
    obj(vec![
        ("id", Json::Int(repo.db_id as i64)),
        (
            "node_id",
            Json::Str(base64_encode(
                format!("Repository:{}", repo.db_id).as_bytes(),
            )),
        ),
        ("name", Json::Str(repo.name.clone())),
        ("full_name", Json::Str(repo.full_name())),
        ("private", Json::Bool(repo.private)),
        ("owner", user_json(&repo.owner)),
        ("description", Json::Str(repo.description.clone())),
        ("default_branch", Json::Str(repo.default_branch.clone())),
        (
            "html_url",
            Json::Str(format!("https://github.com/{}", repo.full_name())),
        ),
        (
            "url",
            Json::Str(format!("https://api.github.com/repos/{}", repo.full_name())),
        ),
        (
            "clone_url",
            Json::Str(format!("https://github.com/{}.git", repo.full_name())),
        ),
        (
            "ssh_url",
            Json::Str(format!("git@github.com:{}.git", repo.full_name())),
        ),
        ("created_at", Json::Str(iso_from_epoch(SCENARIO_EPOCH_SEC))),
        ("updated_at", Json::Str(iso_from_epoch(SCENARIO_EPOCH_SEC))),
        ("pushed_at", Json::Str(iso_from_epoch(SCENARIO_EPOCH_SEC))),
        ("homepage", Json::Str(String::new())),
        ("language", Json::Null),
        ("has_issues", Json::Bool(true)),
        ("has_projects", Json::Bool(true)),
        ("has_wiki", Json::Bool(true)),
        ("has_pages", Json::Bool(false)),
        ("forks_count", Json::Int(0)),
        ("stargazers_count", Json::Int(0)),
        ("watchers_count", Json::Int(0)),
        ("open_issues_count", Json::Int(open_issues)),
        ("license", Json::Null),
        ("topics", Json::Arr(vec![])),
        (
            "visibility",
            Json::Str(if repo.private { "private" } else { "public" }.to_string()),
        ),
        ("archived", Json::Bool(false)),
        ("disabled", Json::Bool(false)),
    ])
}

fn label_json(label: &Label) -> Json {
    obj(vec![
        ("name", Json::Str(label.name.clone())),
        ("color", Json::Str(label.color.clone())),
        ("description", Json::Str(String::new())),
        ("default", Json::Bool(false)),
    ])
}

pub fn issue_json(viewer_login: &str, repo: &Repo, issue: &Issue) -> Json {
    let labels: Vec<Json> = issue
        .labels
        .iter()
        .filter_map(|name| {
            repo.labels
                .iter()
                .find(|label| &label.name == name)
                .map(label_json)
        })
        .collect();
    obj(vec![
        ("id", Json::Int(issue.id as i64)),
        (
            "node_id",
            Json::Str(base64_encode(
                format!("Issue:{}:{}", repo.db_id, issue.number).as_bytes(),
            )),
        ),
        ("number", Json::Int(issue.number as i64)),
        ("title", Json::Str(issue.title.clone())),
        ("body", Json::Str(issue.body.clone())),
        ("state", Json::Str(issue.state.as_str().to_string())),
        ("user", user_json(viewer_login)),
        ("labels", Json::Arr(labels)),
        ("comments", Json::Int(issue.comments.len() as i64)),
        (
            "created_at",
            Json::Str(iso_from_epoch(SCENARIO_EPOCH_SEC + issue.created_seq)),
        ),
        (
            "updated_at",
            Json::Str(iso_from_epoch(SCENARIO_EPOCH_SEC + issue.created_seq)),
        ),
        (
            "url",
            Json::Str(format!(
                "https://api.github.com/repos/{}/issues/{}",
                repo.full_name(),
                issue.number
            )),
        ),
        (
            "html_url",
            Json::Str(format!(
                "https://github.com/{}/issues/{}",
                repo.full_name(),
                issue.number
            )),
        ),
        (
            "repository_url",
            Json::Str(format!("https://api.github.com/repos/{}", repo.full_name())),
        ),
        ("assignee", Json::Null),
        ("assignees", Json::Arr(vec![])),
        ("milestone", Json::Null),
        ("locked", Json::Bool(false)),
    ])
}

fn comment_json(viewer_login: &str, repo: &Repo, comment: &crate::store::Comment) -> Json {
    obj(vec![
        ("id", Json::Int(comment.id as i64)),
        (
            "node_id",
            Json::Str(base64_encode(
                format!("IssueComment:{}:{}", repo.db_id, comment.id).as_bytes(),
            )),
        ),
        ("body", Json::Str(comment.body.clone())),
        ("user", user_json(viewer_login)),
        ("created_at", Json::Str(iso_from_epoch(SCENARIO_EPOCH_SEC))),
        ("updated_at", Json::Str(iso_from_epoch(SCENARIO_EPOCH_SEC))),
        (
            "url",
            Json::Str(format!(
                "https://api.github.com/repos/{}/issues/comments/{}",
                repo.full_name(),
                comment.id
            )),
        ),
        (
            "html_url",
            Json::Str(format!(
                "https://github.com/{}/issues/{}#issuecomment-{}",
                repo.full_name(),
                comment.id,
                comment.id
            )),
        ),
    ])
}

pub fn pull_json(viewer_login: &str, repo: &Repo, pull: &Pull) -> Json {
    let head_sha = repo
        .branch_tip(&pull.head_ref)
        .map(|oid| Json::Str(oid_hex(&oid)))
        .unwrap_or(Json::Null);
    let base_sha = repo
        .branch_tip(&pull.base_ref)
        .map(|oid| Json::Str(oid_hex(&oid)))
        .unwrap_or(Json::Null);
    let mergeable = match pull_mergeable(repo, pull) {
        MergeStatus::CanMerge => Json::Bool(true),
        MergeStatus::Conflicting => Json::Bool(false),
        MergeStatus::Unknown => Json::Null,
    };
    obj(vec![
        ("id", Json::Int(pull.id as i64)),
        (
            "node_id",
            Json::Str(base64_encode(
                format!("PullRequest:{}:{}", repo.db_id, pull.number).as_bytes(),
            )),
        ),
        ("number", Json::Int(pull.number as i64)),
        ("title", Json::Str(pull.title.clone())),
        ("body", Json::Str(pull.body.clone())),
        ("state", Json::Str(pull.state.as_str().to_string())),
        ("merged", Json::Bool(pull.merged)),
        (
            "merged_at",
            pull.merged_at
                .as_ref()
                .map(|at| Json::Str(at.clone()))
                .unwrap_or(Json::Null),
        ),
        (
            "merge_commit_sha",
            pull.merge_commit_sha
                .as_ref()
                .map(|oid| Json::Str(oid_hex(oid)))
                .unwrap_or(Json::Null),
        ),
        ("mergeable", mergeable),
        (
            "head",
            obj(vec![
                ("ref", Json::Str(pull.head_ref.clone())),
                ("sha", head_sha),
                (
                    "label",
                    Json::Str(format!("{}:{}", repo.owner, pull.head_ref)),
                ),
                ("user", user_json(viewer_login)),
                ("repo", repo_json(repo)),
            ]),
        ),
        (
            "base",
            obj(vec![
                ("ref", Json::Str(pull.base_ref.clone())),
                ("sha", base_sha),
                (
                    "label",
                    Json::Str(format!("{}:{}", repo.owner, pull.base_ref)),
                ),
                ("user", user_json(viewer_login)),
                ("repo", repo_json(repo)),
            ]),
        ),
        ("user", user_json(viewer_login)),
        (
            "created_at",
            Json::Str(iso_from_epoch(SCENARIO_EPOCH_SEC + pull.created_seq)),
        ),
        (
            "updated_at",
            Json::Str(iso_from_epoch(SCENARIO_EPOCH_SEC + pull.created_seq)),
        ),
        (
            "url",
            Json::Str(format!(
                "https://api.github.com/repos/{}/pulls/{}",
                repo.full_name(),
                pull.number
            )),
        ),
        (
            "html_url",
            Json::Str(format!(
                "https://github.com/{}/pull/{}",
                repo.full_name(),
                pull.number
            )),
        ),
        ("comments", Json::Int(0)),
        ("commits", Json::Int(0)),
        ("additions", Json::Int(0)),
        ("deletions", Json::Int(0)),
        ("changed_files", Json::Int(0)),
    ])
}

/// One commit in a list, from the parsed commit header.
fn commit_json(repo: &Repo, oid: &crate::git::Oid) -> Json {
    let Some(object) = repo.objects.get(oid) else {
        return obj(vec![("sha", Json::Str(oid_hex(oid)))]);
    };
    let Some(full) = crate::git::parse_commit_full(&object.data) else {
        return obj(vec![("sha", Json::Str(oid_hex(oid)))]);
    };
    let (author_name, author_email) = split_ident(&full.author);
    let (committer_name, committer_email) = split_ident(&full.committer);
    let parents: Vec<Json> = full
        .parents
        .iter()
        .map(|parent| obj(vec![("sha", Json::Str(oid_hex(parent)))]))
        .collect();
    obj(vec![
        ("sha", Json::Str(oid_hex(oid))),
        (
            "node_id",
            Json::Str(base64_encode(
                format!("Commit:{oid_hex}", oid_hex = oid_hex(oid)).as_bytes(),
            )),
        ),
        (
            "commit",
            obj(vec![
                ("message", Json::Str(full.message.clone())),
                (
                    "author",
                    obj(vec![
                        ("name", Json::Str(author_name.clone())),
                        ("email", Json::Str(author_email)),
                        ("date", Json::Str(iso_from_epoch(full.timestamp))),
                    ]),
                ),
                (
                    "committer",
                    obj(vec![
                        ("name", Json::Str(committer_name.clone())),
                        ("email", Json::Str(committer_email)),
                        ("date", Json::Str(iso_from_epoch(full.timestamp))),
                    ]),
                ),
                ("tree", obj(vec![("sha", Json::Str(oid_hex(&full.tree)))])),
            ]),
        ),
        (
            "author",
            obj(vec![("login", Json::Str(author_name.clone()))]),
        ),
        (
            "committer",
            obj(vec![("login", Json::Str(committer_name.clone()))]),
        ),
        ("parents", Json::Arr(parents)),
        (
            "url",
            Json::Str(format!(
                "https://api.github.com/repos/{}/commits/{}",
                repo.full_name(),
                oid_hex(oid)
            )),
        ),
    ])
}

/// Split a `Name <email>` identity line into its parts.
fn split_ident(ident: &str) -> (String, String) {
    match ident.split_once('<') {
        Some((name, rest)) => (
            name.trim().to_string(),
            rest.trim_end_matches('>').trim().to_string(),
        ),
        None => (ident.to_string(), String::new()),
    }
}

// ---------------------------------------------------------------------------
// Dispatcher.

pub fn handle(store: &RwLock<Store>, req: &Request) -> Response {
    let segments: Vec<&str> = req.path.split('/').filter(|s| !s.is_empty()).collect();
    // Everything here lives under /api/v3 (main.rs already routed it).
    if segments.len() < 2 || segments[0] != "api" || segments[1] != "v3" {
        return error_reply(404, "Not Found");
    }
    let tail = &segments[2..];
    let method = req.method.as_str();
    match (method, tail) {
        ("GET", []) => root(req),
        ("GET", ["meta"]) => json_reply(
            200,
            obj(vec![("installed_version", Json::Str("1.0.0".to_string()))]),
        ),
        ("GET", ["user"]) => user(store),
        ("GET", ["user", "repos"]) => list_user_repos(store, req),
        ("POST", ["user", "repos"]) => create_user_repo(store, req),
        ("GET", ["users", login]) => user_by_login(store, login),
        ("GET", ["repos", owner, name]) => get_repo(store, owner, name),
        ("PATCH", ["repos", owner, name]) => patch_repo(store, req, owner, name),
        ("DELETE", ["repos", owner, name]) => delete_repo(store, owner, name),
        ("GET", ["repos", owner, name, "branches"]) => list_branches(store, owner, name),
        ("GET", ["repos", owner, name, "branches", branch]) => {
            get_branch(store, owner, name, branch)
        }
        ("GET", ["repos", owner, name, "commits"]) => list_commits(store, req, owner, name),
        ("GET", ["repos", owner, name, "commits", sha]) => get_commit(store, owner, name, sha),
        ("GET", ["repos", owner, name, "git", "refs"]) => list_refs(store, owner, name),
        ("POST", ["repos", owner, name, "git", "refs"]) => create_ref(store, req, owner, name),
        ("GET", ["repos", owner, name, "git", "refs", rest @ ..]) => {
            get_ref(store, owner, name, rest)
        }
        ("GET", ["repos", owner, name, "contents"]) => get_contents(store, req, owner, name, &[]),
        ("GET", ["repos", owner, name, "contents", rest @ ..]) => {
            get_contents(store, req, owner, name, rest)
        }
        ("PUT", ["repos", owner, name, "contents", rest @ ..]) => {
            put_contents(store, req, owner, name, rest)
        }
        ("DELETE", ["repos", owner, name, "contents", rest @ ..]) => {
            delete_contents(store, req, owner, name, rest)
        }
        ("GET", ["repos", owner, name, "labels"]) => list_labels(store, owner, name),
        ("POST", ["repos", owner, name, "labels"]) => create_label(store, req, owner, name),
        ("GET", ["repos", owner, name, "issues"]) => list_issues(store, req, owner, name),
        ("POST", ["repos", owner, name, "issues"]) => create_issue(store, req, owner, name),
        ("GET", ["repos", owner, name, "issues", number]) => get_issue(store, owner, name, number),
        ("PATCH", ["repos", owner, name, "issues", number]) => {
            patch_issue(store, req, owner, name, number)
        }
        ("GET", ["repos", owner, name, "issues", number, "comments"]) => {
            list_comments(store, owner, name, number)
        }
        ("POST", ["repos", owner, name, "issues", number, "comments"]) => {
            create_comment(store, req, owner, name, number)
        }
        ("GET", ["repos", owner, name, "pulls"]) => list_pulls(store, req, owner, name),
        ("POST", ["repos", owner, name, "pulls"]) => create_pull(store, req, owner, name),
        ("GET", ["repos", owner, name, "pulls", number]) => get_pull(store, owner, name, number),
        ("GET", ["repos", owner, name, "pulls", number, "files"]) => {
            list_pull_files(store, owner, name, number)
        }
        ("GET", ["repos", owner, name, "pulls", number, "commits"]) => {
            list_pull_commits(store, owner, name, number)
        }
        ("PUT", ["repos", owner, name, "pulls", number, "merge"]) => {
            merge_pull(store, req, owner, name, number)
        }
        _ => error_reply(404, "Not Found"),
    }
}

fn root(_req: &Request) -> Response {
    let mut response = json_reply(
        200,
        obj(vec![
            (
                "current_user_url",
                Json::Str("https://api.github.com/user".to_string()),
            ),
            (
                "repository_url",
                Json::Str("https://api.github.com/repos/{owner}/{repo}".to_string()),
            ),
            ("message", Json::Str("GitHub API".to_string())),
        ]),
    );
    response
        .headers
        .push(("x-oauth-scopes".to_string(), "repo, read:org".to_string()));
    response
}

fn user(store: &RwLock<Store>) -> Response {
    let store = store.read().unwrap();
    json_reply(200, user_json(&store.viewer.login))
}

fn user_by_login(store: &RwLock<Store>, login: &str) -> Response {
    let store = store.read().unwrap();
    if login == store.viewer.login {
        json_reply(200, user_json(login))
    } else {
        error_reply(404, "Not Found")
    }
}

// ---------------------------------------------------------------------------
// Repositories.

fn list_user_repos(store: &RwLock<Store>, req: &Request) -> Response {
    let store = store.read().unwrap();
    let items: Vec<Json> = store
        .viewer_repos()
        .iter()
        .map(|repo| repo_json(repo))
        .collect();
    list_reply(req, "/api/v3/user/repos", items)
}

fn create_user_repo(store: &RwLock<Store>, req: &Request) -> Response {
    let body = match parse_body(req) {
        Ok(body) => body,
        Err(response) => return response,
    };
    let Some(name) = body_str(&body, "name").map(|s| s.to_string()) else {
        return error_reply(422, "name is required");
    };
    if name.is_empty() || name.contains('/') {
        return error_reply(422, "invalid repository name");
    }
    let description = body_str(&body, "description").unwrap_or("").to_string();
    let private = body_bool(&body, "private", false);
    let mut store = store.write().unwrap();
    match store.create_repo(&name, &description, private) {
        Ok(_) => {
            let viewer = store.viewer.login.clone();
            let repo = store.get_repo(&viewer, &name).unwrap();
            json_reply(201, repo_json(repo))
        }
        Err(_) => error_reply(422, "Repository already exists"),
    }
}

fn get_repo(store: &RwLock<Store>, owner: &str, name: &str) -> Response {
    let store = store.read().unwrap();
    match store.get_repo(owner, name) {
        Some(repo) => json_reply(200, repo_json(repo)),
        None => error_reply(404, "Not Found"),
    }
}

fn patch_repo(store: &RwLock<Store>, req: &Request, owner: &str, name: &str) -> Response {
    let body = match parse_body(req) {
        Ok(body) => body,
        Err(response) => return response,
    };
    let mut store = store.write().unwrap();
    let Some(repo) = store.get_repo_mut(owner, name) else {
        return error_reply(404, "Not Found");
    };
    if let Some(description) = body_str(&body, "description") {
        repo.description = description.to_string();
    }
    if let Some(private) = body.get("private").and_then(|v| v.as_bool()) {
        repo.private = private;
    }
    json_reply(200, repo_json(repo))
}

fn delete_repo(store: &RwLock<Store>, owner: &str, name: &str) -> Response {
    let mut store = store.write().unwrap();
    if store.delete_repo(owner, name) {
        Response::new(204, Vec::new())
            .header("content-type", "application/json; charset=utf-8")
            .header("x-github-request-id", &next_request_id())
    } else {
        error_reply(404, "Not Found")
    }
}

// ---------------------------------------------------------------------------
// Branches and commits.

fn list_branches(store: &RwLock<Store>, owner: &str, name: &str) -> Response {
    let store = store.read().unwrap();
    let Some(repo) = store.get_repo(owner, name) else {
        return error_reply(404, "Not Found");
    };
    let mut branches: Vec<Json> = repo
        .refs
        .iter()
        .filter_map(|(ref_name, oid)| {
            ref_name.strip_prefix("refs/heads/").map(|branch| {
                obj(vec![
                    ("name", Json::Str(branch.to_string())),
                    ("commit", obj(vec![("sha", Json::Str(oid_hex(oid)))])),
                    ("protected", Json::Bool(false)),
                ])
            })
        })
        .collect();
    branches.sort_by(|a, b| {
        a.get("name")
            .and_then(|v| v.as_str())
            .cmp(&b.get("name").and_then(|v| v.as_str()))
    });
    json_reply(200, Json::Arr(branches))
}

fn get_branch(store: &RwLock<Store>, owner: &str, name: &str, branch: &str) -> Response {
    let store = store.read().unwrap();
    let Some(repo) = store.get_repo(owner, name) else {
        return error_reply(404, "Not Found");
    };
    match repo.branch_tip(branch) {
        Some(oid) => json_reply(
            200,
            obj(vec![
                ("name", Json::Str(branch.to_string())),
                ("commit", obj(vec![("sha", Json::Str(oid_hex(&oid)))])),
                ("protected", Json::Bool(false)),
            ]),
        ),
        None => error_reply(404, "Not Found"),
    }
}

fn list_commits(store: &RwLock<Store>, req: &Request, owner: &str, name: &str) -> Response {
    let store = store.read().unwrap();
    let Some(repo) = store.get_repo(owner, name) else {
        return error_reply(404, "Not Found");
    };
    let tip = match req.query_param("sha") {
        Some(reference) => repo.resolve_ref(reference),
        None => repo.default_tip(),
    };
    let Some(tip) = tip else {
        return json_reply(200, Json::Arr(vec![]));
    };
    let items: Vec<Json> = repo
        .commits_from(tip)
        .iter()
        .map(|oid| commit_json(repo, oid))
        .collect();
    list_reply(req, &format!("/api/v3/repos/{owner}/{name}/commits"), items)
}

fn get_commit(store: &RwLock<Store>, owner: &str, name: &str, sha: &str) -> Response {
    let store = store.read().unwrap();
    let Some(repo) = store.get_repo(owner, name) else {
        return error_reply(404, "Not Found");
    };
    let Some(oid) = oid_from_hex(sha).filter(|oid| repo.objects.get(oid).is_some()) else {
        return error_reply(404, "Not Found");
    };
    if repo.objects.get(&oid).map(|o| o.ty) != Some(ObjType::Commit) {
        return error_reply(404, "Not Found");
    }
    json_reply(200, commit_json(repo, &oid))
}

// ---------------------------------------------------------------------------
// Git refs.

fn list_refs(store: &RwLock<Store>, owner: &str, name: &str) -> Response {
    let store = store.read().unwrap();
    let Some(repo) = store.get_repo(owner, name) else {
        return error_reply(404, "Not Found");
    };
    let items: Vec<Json> = repo
        .sorted_refs()
        .iter()
        .map(|(ref_name, oid)| {
            obj(vec![
                ("ref", Json::Str(ref_name.clone())),
                (
                    "object",
                    obj(vec![
                        ("sha", Json::Str(oid_hex(oid))),
                        ("type", Json::Str("commit".to_string())),
                    ]),
                ),
            ])
        })
        .collect();
    json_reply(200, Json::Arr(items))
}

fn create_ref(store: &RwLock<Store>, req: &Request, owner: &str, name: &str) -> Response {
    let body = match parse_body(req) {
        Ok(body) => body,
        Err(response) => return response,
    };
    let Some(ref_name) = body_str(&body, "ref").map(|s| s.to_string()) else {
        return error_reply(422, "ref is required");
    };
    let Some(sha_text) = body_str(&body, "sha").map(|s| s.to_string()) else {
        return error_reply(422, "sha is required");
    };
    if !ref_name.starts_with("refs/") {
        return error_reply(422, "ref must start with refs/");
    }
    let Some(oid) = oid_from_hex(&sha_text) else {
        return error_reply(422, "invalid sha");
    };
    let mut store = store.write().unwrap();
    let Some(repo) = store.get_repo_mut(owner, name) else {
        return error_reply(404, "Not Found");
    };
    if repo.refs.contains_key(&ref_name) {
        return error_reply(422, "Reference already exists");
    }
    if repo.objects.get(&oid).is_none() {
        return error_reply(422, "sha does not exist");
    }
    repo.set_ref(&ref_name, Some(oid));
    json_reply(
        201,
        obj(vec![
            ("ref", Json::Str(ref_name)),
            (
                "object",
                obj(vec![
                    ("sha", Json::Str(oid_hex(&oid))),
                    ("type", Json::Str("commit".to_string())),
                ]),
            ),
        ]),
    )
}

fn get_ref(store: &RwLock<Store>, owner: &str, name: &str, rest: &[&str]) -> Response {
    let ref_name = format!("refs/{}", rest.join("/"));
    let store = store.read().unwrap();
    let Some(repo) = store.get_repo(owner, name) else {
        return error_reply(404, "Not Found");
    };
    match repo.refs.get(&ref_name) {
        Some(oid) => json_reply(
            200,
            obj(vec![
                ("ref", Json::Str(ref_name)),
                (
                    "object",
                    obj(vec![
                        ("sha", Json::Str(oid_hex(oid))),
                        ("type", Json::Str("commit".to_string())),
                    ]),
                ),
            ]),
        ),
        None => error_reply(404, "Not Found"),
    }
}

// ---------------------------------------------------------------------------
// Contents.

/// Resolve the `ref` query parameter (or the default branch) to a commit.
fn contents_tip(repo: &Repo, req: &Request) -> Option<crate::git::Oid> {
    match req.query_param("ref") {
        Some(reference) => repo.resolve_ref(reference),
        None => repo.default_tip(),
    }
}

fn get_contents(
    store: &RwLock<Store>,
    req: &Request,
    owner: &str,
    name: &str,
    rest: &[&str],
) -> Response {
    let path = rest.join("/");
    let store = store.read().unwrap();
    let Some(repo) = store.get_repo(owner, name) else {
        return error_reply(404, "Not Found");
    };
    let Some(tip) = contents_tip(repo, req) else {
        return error_reply(404, "Not Found");
    };
    if path.is_empty() {
        return match repo.list_dir(tip, "") {
            Some(entries) => json_reply(200, Json::Arr(dir_entries(repo, "", &entries))),
            None => error_reply(404, "Not Found"),
        };
    }
    if let Some((oid, bytes)) = repo.read_path(tip, &path) {
        let file_name = path.rsplit('/').next().unwrap_or(&path).to_string();
        return json_reply(
            200,
            obj(vec![
                ("type", Json::Str("file".to_string())),
                ("encoding", Json::Str("base64".to_string())),
                ("size", Json::Int(bytes.len() as i64)),
                ("name", Json::Str(file_name)),
                ("path", Json::Str(path)),
                ("content", Json::Str(base64_encode(&bytes))),
                ("sha", Json::Str(oid_hex(&oid))),
            ]),
        );
    }
    match repo.list_dir(tip, &path) {
        Some(entries) => json_reply(200, Json::Arr(dir_entries(repo, &path, &entries))),
        None => error_reply(404, "Not Found"),
    }
}

fn dir_entries(_repo: &Repo, dir: &str, entries: &[crate::git::TreeEntry]) -> Vec<Json> {
    let mut out: Vec<Json> = entries
        .iter()
        .map(|entry| {
            let entry_path = if dir.is_empty() {
                entry.name.clone()
            } else {
                format!("{dir}/{}", entry.name)
            };
            let is_dir = entry.mode == 0o40000;
            let mut fields = vec![
                (
                    "type",
                    Json::Str(if is_dir { "dir" } else { "file" }.to_string()),
                ),
                ("name", Json::Str(entry.name.clone())),
                ("path", Json::Str(entry_path)),
                ("sha", Json::Str(oid_hex(&entry.oid))),
            ];
            if !is_dir {
                fields.push(("size", Json::Int(0)));
            }
            obj(fields)
        })
        .collect();
    out.sort_by(|a, b| {
        a.get("name")
            .and_then(|v| v.as_str())
            .cmp(&b.get("name").and_then(|v| v.as_str()))
    });
    out
}

fn put_contents(
    store: &RwLock<Store>,
    req: &Request,
    owner: &str,
    name: &str,
    rest: &[&str],
) -> Response {
    let path = rest.join("/");
    let body = match parse_body(req) {
        Ok(body) => body,
        Err(response) => return response,
    };
    if path.is_empty() {
        return error_reply(422, "path is required");
    }
    let Some(message) = body_str(&body, "message").map(|s| s.to_string()) else {
        return error_reply(422, "message is required");
    };
    let Some(content_b64) = body_str(&body, "content") else {
        return error_reply(422, "content is required");
    };
    let Some(content) = base64_decode(content_b64) else {
        return error_reply(422, "content is not valid base64");
    };
    let branch = body_str(&body, "branch").map(|s| s.to_string());
    let expected_sha = body_str(&body, "sha").and_then(oid_from_hex);
    let mut store = store.write().unwrap();
    let identity = store.identity();
    let timestamp = store.next_commit_timestamp();
    let Some(repo) = store.get_repo_mut(owner, name) else {
        return error_reply(404, "Not Found");
    };
    match crate::store::write_file(
        repo,
        &identity,
        timestamp,
        &path,
        &content,
        &message,
        branch.as_deref(),
        expected_sha,
    ) {
        Ok((blob_oid, commit_oid, created)) => {
            let file_name = path.rsplit('/').next().unwrap_or(&path).to_string();
            json_reply(
                if created { 201 } else { 200 },
                obj(vec![
                    (
                        "content",
                        obj(vec![
                            ("type", Json::Str("file".to_string())),
                            ("encoding", Json::Str("base64".to_string())),
                            ("size", Json::Int(content.len() as i64)),
                            ("name", Json::Str(file_name)),
                            ("path", Json::Str(path)),
                            ("sha", Json::Str(oid_hex(&blob_oid))),
                        ]),
                    ),
                    (
                        "commit",
                        obj(vec![
                            ("sha", Json::Str(oid_hex(&commit_oid))),
                            ("message", Json::Str(message)),
                        ]),
                    ),
                ]),
            )
        }
        Err("stale sha") => error_reply(409, "sha does not match the current file"),
        Err("missing sha") => error_reply(422, "sha is required to update a file"),
        Err(_) => error_reply(422, "cannot write file"),
    }
}

fn delete_contents(
    store: &RwLock<Store>,
    req: &Request,
    owner: &str,
    name: &str,
    rest: &[&str],
) -> Response {
    let path = rest.join("/");
    let body = match parse_body(req) {
        Ok(body) => body,
        Err(response) => return response,
    };
    if path.is_empty() {
        return error_reply(422, "path is required");
    }
    let message = body_str(&body, "message")
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("Delete {path}"));
    let branch = body_str(&body, "branch").map(|s| s.to_string());
    let expected_sha = body_str(&body, "sha").and_then(oid_from_hex);
    let mut store = store.write().unwrap();
    let identity = store.identity();
    let timestamp = store.next_commit_timestamp();
    let Some(repo) = store.get_repo_mut(owner, name) else {
        return error_reply(404, "Not Found");
    };
    // A sha that does not match the live blob is a 409, like the PUT path.
    if let Some(expected) = expected_sha {
        let tip = match branch
            .as_deref()
            .map(|b| repo.branch_tip(b))
            .unwrap_or_else(|| repo.default_tip())
        {
            Some(tip) => tip,
            None => return error_reply(404, "Not Found"),
        };
        match repo.read_path(tip, &path) {
            Some((current, _)) if current != expected => {
                return error_reply(409, "sha does not match the current file");
            }
            None => return error_reply(404, "Not Found"),
            _ => {}
        }
    }
    match delete_file(
        repo,
        &identity,
        timestamp,
        &path,
        &message,
        branch.as_deref(),
    ) {
        Ok(commit_oid) => json_reply(
            200,
            obj(vec![(
                "commit",
                obj(vec![
                    ("sha", Json::Str(oid_hex(&commit_oid))),
                    ("message", Json::Str(message)),
                ]),
            )]),
        ),
        Err("no such file") => error_reply(404, "Not Found"),
        Err("no such branch") => error_reply(404, "Not Found"),
        Err(_) => error_reply(422, "cannot delete file"),
    }
}

// ---------------------------------------------------------------------------
// Labels.

fn list_labels(store: &RwLock<Store>, owner: &str, name: &str) -> Response {
    let store = store.read().unwrap();
    let Some(repo) = store.get_repo(owner, name) else {
        return error_reply(404, "Not Found");
    };
    json_reply(200, Json::Arr(repo.labels.iter().map(label_json).collect()))
}

fn create_label(store: &RwLock<Store>, req: &Request, owner: &str, name: &str) -> Response {
    let body = match parse_body(req) {
        Ok(body) => body,
        Err(response) => return response,
    };
    let Some(label_name) = body_str(&body, "name").map(|s| s.to_string()) else {
        return error_reply(422, "name is required");
    };
    let color = body_str(&body, "color")
        .unwrap_or("ededed")
        .trim_start_matches('#')
        .to_string();
    let mut store = store.write().unwrap();
    let Some(repo) = store.get_repo_mut(owner, name) else {
        return error_reply(404, "Not Found");
    };
    if repo.labels.iter().any(|label| label.name == label_name) {
        return error_reply(422, "Label already exists");
    }
    repo.labels.push(Label {
        name: label_name,
        color,
    });
    let label = repo.labels.last().unwrap();
    json_reply(201, label_json(label))
}

// ---------------------------------------------------------------------------
// Issues and comments.

fn parse_number(text: &str) -> Option<u32> {
    text.parse::<u32>().ok()
}

fn create_issue(store: &RwLock<Store>, req: &Request, owner: &str, name: &str) -> Response {
    let body = match parse_body(req) {
        Ok(body) => body,
        Err(response) => return response,
    };
    let Some(title) = body_str(&body, "title").map(|s| s.to_string()) else {
        return error_reply(422, "title is required");
    };
    if title.is_empty() {
        return error_reply(422, "title is required");
    }
    let body_text = body_str(&body, "body").unwrap_or("").to_string();
    let wanted_labels: Vec<String> = body
        .get("labels")
        .and_then(|v| v.as_arr())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let mut store = store.write().unwrap();
    let Some(repo) = store.get_repo_mut(owner, name) else {
        return error_reply(404, "Not Found");
    };
    for wanted in &wanted_labels {
        if !repo.labels.iter().any(|label| &label.name == wanted) {
            return error_reply(422, &format!("unknown label: {wanted}"));
        }
    }
    let id = store.next_issue_id;
    store.next_issue_id += 1;
    store.created_seq += 1;
    let created_seq = store.created_seq;
    let viewer_login = store.viewer.login.clone();
    let Some(repo) = store.get_repo_mut(owner, name) else {
        return error_reply(404, "Not Found");
    };
    repo.number_seq += 1;
    let number = repo.number_seq;
    repo.issues.push(Issue {
        id,
        number,
        title,
        body: body_text,
        state: IssueState::Open,
        labels: wanted_labels,
        comments: Vec::new(),
        created_seq,
    });
    let issue = repo.issues.last().unwrap();
    json_reply(201, issue_json(&viewer_login, repo, issue))
}

fn list_issues(store: &RwLock<Store>, req: &Request, owner: &str, name: &str) -> Response {
    let store = store.read().unwrap();
    let Some(repo) = store.get_repo(owner, name) else {
        return error_reply(404, "Not Found");
    };
    let state = req.query_param("state").unwrap_or("open");
    let direction = req.query_param("direction").unwrap_or("desc");
    let mut issues: Vec<&Issue> = repo
        .issues
        .iter()
        .filter(|issue| match state {
            "all" => true,
            "closed" => issue.state == IssueState::Closed,
            _ => issue.state == IssueState::Open,
        })
        .collect();
    // Sort by creation order (the `sort` parameter is `created` in the
    // suite; any other value keeps creation order too).
    if direction == "asc" {
        issues.sort_by_key(|issue| issue.created_seq);
    } else {
        issues.sort_by_key(|issue| std::cmp::Reverse(issue.created_seq));
    }
    let items: Vec<Json> = issues
        .iter()
        .map(|issue| issue_json(&store.viewer.login, repo, issue))
        .collect();
    list_reply(req, &format!("/api/v3/repos/{owner}/{name}/issues"), items)
}

fn get_issue(store: &RwLock<Store>, owner: &str, name: &str, number: &str) -> Response {
    let Some(number) = parse_number(number) else {
        return error_reply(404, "Not Found");
    };
    let store = store.read().unwrap();
    let Some(repo) = store.get_repo(owner, name) else {
        return error_reply(404, "Not Found");
    };
    match repo.issues.iter().find(|issue| issue.number == number) {
        Some(issue) => json_reply(200, issue_json(&store.viewer.login, repo, issue)),
        None => error_reply(404, "Not Found"),
    }
}

fn patch_issue(
    store: &RwLock<Store>,
    req: &Request,
    owner: &str,
    name: &str,
    number: &str,
) -> Response {
    let Some(number) = parse_number(number) else {
        return error_reply(404, "Not Found");
    };
    let body = match parse_body(req) {
        Ok(body) => body,
        Err(response) => return response,
    };
    if let Some(state) = body_str(&body, "state") {
        if state != "open" && state != "closed" {
            return error_reply(422, "invalid state");
        }
    }
    let mut store = store.write().unwrap();
    let viewer_login = store.viewer.login.clone();
    let Some(repo) = store.get_repo_mut(owner, name) else {
        return error_reply(404, "Not Found");
    };
    let Some(issue) = repo
        .issues
        .iter_mut()
        .find(|issue| issue.number == number)
        .map(|issue| {
            if let Some(state) = body_str(&body, "state") {
                issue.state = if state == "open" {
                    IssueState::Open
                } else {
                    IssueState::Closed
                };
            }
            if let Some(title) = body_str(&body, "title") {
                issue.title = title.to_string();
            }
            if let Some(body_text) = body_str(&body, "body") {
                issue.body = body_text.to_string();
            }
            issue.clone()
        })
    else {
        return error_reply(404, "Not Found");
    };
    json_reply(200, issue_json(&viewer_login, repo, &issue))
}

fn list_comments(store: &RwLock<Store>, owner: &str, name: &str, number: &str) -> Response {
    let Some(number) = parse_number(number) else {
        return error_reply(404, "Not Found");
    };
    let store = store.read().unwrap();
    let Some(repo) = store.get_repo(owner, name) else {
        return error_reply(404, "Not Found");
    };
    let Some(issue) = repo.issues.iter().find(|issue| issue.number == number) else {
        return error_reply(404, "Not Found");
    };
    json_reply(
        200,
        Json::Arr(
            issue
                .comments
                .iter()
                .map(|comment| comment_json(&store.viewer.login, repo, comment))
                .collect(),
        ),
    )
}

fn create_comment(
    store: &RwLock<Store>,
    req: &Request,
    owner: &str,
    name: &str,
    number: &str,
) -> Response {
    let Some(number) = parse_number(number) else {
        return error_reply(404, "Not Found");
    };
    let body = match parse_body(req) {
        Ok(body) => body,
        Err(response) => return response,
    };
    let Some(text) = body_str(&body, "body").map(|s| s.to_string()) else {
        return error_reply(422, "body is required");
    };
    let mut store = store.write().unwrap();
    let viewer_login = store.viewer.login.clone();
    let Some(repo) = store.get_repo_mut(owner, name) else {
        return error_reply(404, "Not Found");
    };
    repo.comment_seq += 1;
    let id = repo.comment_seq;
    let Some(issue) = repo.issues.iter_mut().find(|issue| issue.number == number) else {
        return error_reply(404, "Not Found");
    };
    issue
        .comments
        .push(crate::store::Comment { id, body: text });
    let comment = issue.comments.last().unwrap().clone();
    json_reply(201, comment_json(&viewer_login, repo, &comment))
}

// ---------------------------------------------------------------------------
// Pull requests.

/// A branch name from the request body, tolerating an `owner:` prefix.
fn clean_branch(raw: &str) -> String {
    raw.rsplit(':').next().unwrap_or(raw).to_string()
}

fn create_pull(store: &RwLock<Store>, req: &Request, owner: &str, name: &str) -> Response {
    let body = match parse_body(req) {
        Ok(body) => body,
        Err(response) => return response,
    };
    let Some(title) = body_str(&body, "title").map(|s| s.to_string()) else {
        return error_reply(422, "title is required");
    };
    let Some(head_raw) = body_str(&body, "head").map(|s| s.to_string()) else {
        return error_reply(422, "head is required");
    };
    let Some(base_raw) = body_str(&body, "base").map(|s| s.to_string()) else {
        return error_reply(422, "base is required");
    };
    let head = clean_branch(&head_raw);
    let base = clean_branch(&base_raw);
    let body_text = body_str(&body, "body").unwrap_or("").to_string();
    let mut store = store.write().unwrap();
    let (base_sha, head_ok) = match store.get_repo(owner, name) {
        Some(repo) => (repo.branch_tip(&base), repo.branch_tip(&head).is_some()),
        None => return error_reply(404, "Not Found"),
    };
    let Some(base_sha) = base_sha else {
        return error_reply(422, "base branch does not exist");
    };
    if !head_ok {
        return error_reply(422, "head branch does not exist");
    }
    if head == base {
        return error_reply(422, "head and base are the same branch");
    }
    let viewer_login = store.viewer.login.clone();
    let Some(repo) = store.get_repo_mut(owner, name) else {
        return error_reply(404, "Not Found");
    };
    if repo
        .pulls
        .iter()
        .any(|pull| pull.head_ref == head && pull.state == IssueState::Open)
    {
        return error_reply(422, "a pull request from this head branch is already open");
    }
    let id = store.next_pull_id;
    store.next_pull_id += 1;
    store.created_seq += 1;
    let created_seq = store.created_seq;
    let Some(repo) = store.get_repo_mut(owner, name) else {
        return error_reply(404, "Not Found");
    };
    repo.number_seq += 1;
    let number = repo.number_seq;
    repo.pulls.push(Pull {
        id,
        number,
        title,
        body: body_text,
        head_ref: head,
        base_ref: base,
        base_sha,
        state: IssueState::Open,
        merged: false,
        merged_at: None,
        merge_commit_sha: None,
        created_seq,
    });
    let pull = repo.pulls.last().unwrap();
    json_reply(201, pull_json(&viewer_login, repo, pull))
}

fn list_pulls(store: &RwLock<Store>, req: &Request, owner: &str, name: &str) -> Response {
    let store = store.read().unwrap();
    let Some(repo) = store.get_repo(owner, name) else {
        return error_reply(404, "Not Found");
    };
    let state = req.query_param("state").unwrap_or("open");
    let mut pulls: Vec<&Pull> = repo
        .pulls
        .iter()
        .filter(|pull| match state {
            "all" => true,
            "closed" => pull.state == IssueState::Closed,
            _ => pull.state == IssueState::Open,
        })
        .collect();
    pulls.sort_by_key(|pull| std::cmp::Reverse(pull.created_seq));
    let items: Vec<Json> = pulls
        .iter()
        .map(|pull| pull_json(&store.viewer.login, repo, pull))
        .collect();
    list_reply(req, &format!("/api/v3/repos/{owner}/{name}/pulls"), items)
}

fn get_pull(store: &RwLock<Store>, owner: &str, name: &str, number: &str) -> Response {
    let Some(number) = parse_number(number) else {
        return error_reply(404, "Not Found");
    };
    let store = store.read().unwrap();
    let Some(repo) = store.get_repo(owner, name) else {
        return error_reply(404, "Not Found");
    };
    match repo.pulls.iter().find(|pull| pull.number == number) {
        Some(pull) => json_reply(200, pull_json(&store.viewer.login, repo, pull)),
        None => error_reply(404, "Not Found"),
    }
}

fn list_pull_files(store: &RwLock<Store>, owner: &str, name: &str, number: &str) -> Response {
    let Some(number) = parse_number(number) else {
        return error_reply(404, "Not Found");
    };
    let store = store.read().unwrap();
    let Some(repo) = store.get_repo(owner, name) else {
        return error_reply(404, "Not Found");
    };
    let Some(pull) = repo.pulls.iter().find(|pull| pull.number == number) else {
        return error_reply(404, "Not Found");
    };
    let Some(head_tip) = repo.branch_tip(&pull.head_ref) else {
        return error_reply(404, "Not Found");
    };
    let Some(base_commit) = repo.objects.commit_of(&pull.base_sha) else {
        return error_reply(404, "Not Found");
    };
    let Some(head_commit) = repo.objects.commit_of(&head_tip) else {
        return error_reply(404, "Not Found");
    };
    let changes = diff_trees(repo, base_commit.tree, head_commit.tree);
    let items: Vec<Json> = changes
        .iter()
        .map(|change| {
            obj(vec![
                ("filename", Json::Str(change.path.clone())),
                ("status", Json::Str(change.status.to_string())),
                ("additions", Json::Int(change.additions as i64)),
                ("deletions", Json::Int(change.deletions as i64)),
                ("changes", Json::Int(change.changes() as i64)),
            ])
        })
        .collect();
    json_reply(200, Json::Arr(items))
}

fn list_pull_commits(store: &RwLock<Store>, owner: &str, name: &str, number: &str) -> Response {
    let Some(number) = parse_number(number) else {
        return error_reply(404, "Not Found");
    };
    let store = store.read().unwrap();
    let Some(repo) = store.get_repo(owner, name) else {
        return error_reply(404, "Not Found");
    };
    let Some(pull) = repo.pulls.iter().find(|pull| pull.number == number) else {
        return error_reply(404, "Not Found");
    };
    let Some(head_tip) = repo.branch_tip(&pull.head_ref) else {
        return error_reply(404, "Not Found");
    };
    let base_closure = repo.objects.closure(&[pull.base_sha]);
    let items: Vec<Json> = repo
        .commits_from(head_tip)
        .iter()
        .filter(|oid| !base_closure.contains(*oid))
        .map(|oid| commit_json(repo, oid))
        .collect();
    json_reply(200, Json::Arr(items))
}

fn merge_pull(
    store: &RwLock<Store>,
    req: &Request,
    owner: &str,
    name: &str,
    number: &str,
) -> Response {
    let Some(number) = parse_number(number) else {
        return error_reply(404, "Not Found");
    };
    // The body is optional (merge_method defaults to squash here); a
    // malformed body is still a 400.
    if let Err(response) = parse_body(req) {
        return response;
    }
    let mut store = store.write().unwrap();
    let exists = store
        .get_repo(owner, name)
        .is_some_and(|repo| repo.pulls.iter().any(|pull| pull.number == number));
    if !exists {
        return error_reply(404, "Not Found");
    }
    let identity = store.identity();
    let timestamp = store.next_commit_timestamp();
    let merged_at = iso_from_epoch(SCENARIO_EPOCH_SEC + store.commit_seq);
    let Some(repo) = store.get_repo_mut(owner, name) else {
        return error_reply(404, "Not Found");
    };
    match squash_merge(repo, &identity, timestamp, merged_at, number) {
        Ok(sha) => json_reply(
            200,
            obj(vec![
                ("merged", Json::Bool(true)),
                ("sha", Json::Str(oid_hex(&sha))),
                (
                    "message",
                    Json::Str("Pull request successfully merged".to_string()),
                ),
            ]),
        ),
        Err("conflict") => error_reply(405, "merge conflict"),
        Err("already merged") => error_reply(405, "Pull request is already merged"),
        Err("closed") => error_reply(405, "Pull request is closed"),
        Err(_) => error_reply(405, "Pull request cannot be merged"),
    }
}
