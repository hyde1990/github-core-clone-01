//! GitHub REST v3 (`/api/v3`). Every reply is `application/json` with a
//! non-empty `X-GitHub-Request-Id`; errors carry a string `message`.

use crate::git::{oid_from_hex, oid_hex, parse_commit_full};
use crate::http::Request;
use crate::http::Response;
use crate::json::{obj, Json};
use crate::store::{
    delete_file, diff_trees, pull_mergeable, squash_merge, write_file, Issue, IssueState, Label,
    MergeStatus, Pull, Repo, Store,
};
use crate::util::{base64_decode, base64_encode, iso_from_epoch};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

/// Process-wide request-id counter (no lock needed; ids only need to be
/// non-empty and distinct).
static REQUEST_SEQ: AtomicU64 = AtomicU64::new(0);

/// A fresh `X-GitHub-Request-Id` value.
pub fn next_request_id() -> String {
    let seq = REQUEST_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    format!("req-{seq:012x}")
}

pub fn handle(store: &RwLock<Store>, req: &Request) -> Response {
    let path = req.path.trim_end_matches('/');
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    // /api/v3/...
    if segments.first() != Some(&"api") || segments.get(1) != Some(&"v3") {
        return not_found();
    }
    let rest = &segments[2..];
    match (req.method.as_str(), rest) {
        ("GET", []) => {
            let body = obj(vec![
                ("current_user_url", Json::Str("/api/v3/user".to_string())),
                (
                    "rate_limit_url",
                    Json::Str("/api/v3/rate_limit".to_string()),
                ),
            ]);
            json_response(store, 200, body).header("x-oauth-scopes", "repo, read:org")
        }
        ("GET", ["meta"]) => json_response(
            store,
            200,
            obj(vec![(
                "installed_version",
                Json::Str("forge-1.0.0".to_string()),
            )]),
        ),
        ("GET", ["user"]) => {
            let store = store.read().unwrap();
            json_locked(&store, 200, user_json(&store))
        }
        ("GET", ["user", "repos"]) => list_user_repos(store, req),
        ("POST", ["user", "repos"]) => create_repo(store, req),
        ("GET", ["repos", owner, name]) => get_repo(store, owner, name),
        ("PATCH", ["repos", owner, name]) => patch_repo(store, req, owner, name),
        ("DELETE", ["repos", owner, name]) => delete_repo(store, owner, name),
        ("GET", ["repos", owner, name, "branches"]) => list_branches(store, owner, name),
        ("GET", ["repos", owner, name, "branches", branch @ ..]) => {
            get_branch(store, owner, name, branch)
        }
        ("GET", ["repos", owner, name, "commits"]) => list_commits(store, req, owner, name),
        ("GET", ["repos", owner, name, "git", "refs", "heads", branch @ ..]) => {
            get_ref(store, owner, name, branch)
        }
        ("POST", ["repos", owner, name, "git", "refs"]) => create_ref(store, req, owner, name),
        ("PUT", ["repos", owner, name, "contents", path @ ..]) => {
            put_contents(store, req, owner, name, path)
        }
        ("GET", ["repos", owner, name, "contents", path @ ..]) => {
            get_contents(store, req, owner, name, path)
        }
        ("DELETE", ["repos", owner, name, "contents", path @ ..]) => {
            delete_contents(store, req, owner, name, path)
        }
        ("POST", ["repos", owner, name, "labels"]) => create_label(store, req, owner, name),
        ("POST", ["repos", owner, name, "issues"]) => create_issue(store, req, owner, name),
        ("GET", ["repos", owner, name, "issues"]) => list_issues(store, req, owner, name),
        ("GET", ["repos", owner, name, "issues", number]) => get_issue(store, owner, name, number),
        ("PATCH", ["repos", owner, name, "issues", number]) => {
            patch_issue(store, req, owner, name, number)
        }
        ("POST", ["repos", owner, name, "issues", number, "comments"]) => {
            create_comment(store, req, owner, name, number)
        }
        ("GET", ["repos", owner, name, "issues", number, "comments"]) => {
            list_comments(store, owner, name, number)
        }
        ("POST", ["repos", owner, name, "pulls"]) => create_pull(store, req, owner, name),
        ("GET", ["repos", owner, name, "pulls"]) => list_pulls(store, req, owner, name),
        ("GET", ["repos", owner, name, "pulls", number]) => get_pull(store, owner, name, number),
        ("GET", ["repos", owner, name, "pulls", number, "files"]) => {
            list_pull_files(store, owner, name, number)
        }
        ("PUT", ["repos", owner, name, "pulls", number, "merge"]) => {
            merge_pull(store, req, owner, name, number)
        }
        _ => not_found(),
    }
}

// ---------------------------------------------------------------------------
// Response helpers.

pub fn json_response(_store: &RwLock<Store>, status: u16, body: Json) -> Response {
    Response::new(status, body.stringify().into_bytes())
        .header("content-type", "application/json; charset=utf-8")
        .header("x-github-request-id", &next_request_id())
}

/// A JSON response when the caller already holds the store lock.
fn json_locked(_store: &Store, status: u16, body: Json) -> Response {
    Response::new(status, body.stringify().into_bytes())
        .header("content-type", "application/json; charset=utf-8")
        .header("x-github-request-id", &next_request_id())
}

pub fn error_response(store: &RwLock<Store>, status: u16, message: &str) -> Response {
    json_response(
        store,
        status,
        obj(vec![("message", Json::Str(message.to_string()))]),
    )
}

fn not_found() -> Response {
    Response::new(404, br#"{"message":"Not Found"}"#.to_vec())
        .header("content-type", "application/json; charset=utf-8")
}

/// Parse a JSON request body; an empty body is `{}`; malformed JSON is 400.
fn parse_body(req: &Request) -> Result<Json, Response> {
    if req.body.is_empty() {
        return Ok(Json::Obj(vec![]));
    }
    let text = String::from_utf8_lossy(&req.body);
    crate::json::parse(&text).map_err(|_| {
        Response::new(400, br#"{"message":"malformed JSON body"}"#.to_vec())
            .header("content-type", "application/json; charset=utf-8")
    })
}

fn body_str<'a>(body: &'a Json, key: &str) -> Option<&'a str> {
    body.get(key)?.as_str()
}

fn body_bool(body: &Json, key: &str) -> Option<bool> {
    body.get(key)?.as_bool()
}

fn number_param(text: &str) -> Option<u32> {
    text.parse::<u32>().ok()
}

// ---------------------------------------------------------------------------
// Shapes.

pub fn user_json(store: &Store) -> Json {
    obj(vec![
        ("login", Json::Str(store.viewer.login.clone())),
        ("id", Json::Int(store.viewer.id as i64)),
        ("name", Json::Str(store.viewer.name.clone())),
        ("email", Json::Str(store.viewer.email.clone())),
        ("type", Json::Str("User".to_string())),
    ])
}

pub fn repo_json(repo: &Repo) -> Json {
    obj(vec![
        ("id", Json::Int(repo.db_id as i64)),
        ("name", Json::Str(repo.name.clone())),
        ("full_name", Json::Str(repo.full_name())),
        (
            "owner",
            obj(vec![
                ("login", Json::Str(repo.owner.clone())),
                ("id", Json::Int(1000)),
            ]),
        ),
        ("private", Json::Bool(repo.private)),
        ("description", Json::Str(repo.description.clone())),
        ("default_branch", Json::Str(repo.default_branch.clone())),
        ("has_issues", Json::Bool(true)),
        (
            "created_at",
            Json::Str(iso_from_epoch(crate::scenario::SCENARIO_EPOCH_SEC)),
        ),
    ])
}

fn label_json(label: &Label) -> Json {
    obj(vec![
        ("name", Json::Str(label.name.clone())),
        ("color", Json::Str(label.color.clone())),
    ])
}

pub fn issue_json(viewer_login: &str, repo: &Repo, issue: &Issue) -> Json {
    let labels: Vec<Json> = issue
        .labels
        .iter()
        .map(|name| {
            let color = repo
                .labels
                .iter()
                .find(|l| &l.name == name)
                .map(|l| l.color.clone())
                .unwrap_or_else(|| "ededed".to_string());
            obj(vec![
                ("name", Json::Str(name.clone())),
                ("color", Json::Str(color)),
            ])
        })
        .collect();
    obj(vec![
        ("id", Json::Int(issue.id as i64)),
        ("number", Json::Int(issue.number as i64)),
        ("title", Json::Str(issue.title.clone())),
        ("body", Json::Str(issue.body.clone())),
        ("state", Json::Str(issue.state.as_str().to_string())),
        ("labels", Json::Arr(labels)),
        (
            "user",
            obj(vec![("login", Json::Str(viewer_login.to_string()))]),
        ),
        (
            "created_at",
            Json::Str(iso_from_epoch(
                crate::scenario::SCENARIO_EPOCH_SEC + issue.created_seq,
            )),
        ),
        (
            "updated_at",
            Json::Str(iso_from_epoch(
                crate::scenario::SCENARIO_EPOCH_SEC + issue.created_seq,
            )),
        ),
    ])
}

pub fn pull_json(viewer_login: &str, repo: &Repo, pull: &Pull) -> Json {
    let head_sha = repo
        .branch_tip(&pull.head_ref)
        .map(|oid| oid_hex(&oid))
        .unwrap_or_default();
    let base_sha = oid_hex(&pull.base_sha);
    let state = if pull.merged {
        "closed"
    } else {
        pull.state.as_str()
    };
    let mut fields: Vec<(&str, Json)> = vec![
        ("id", Json::Int(pull.id as i64)),
        ("number", Json::Int(pull.number as i64)),
        ("title", Json::Str(pull.title.clone())),
        ("body", Json::Str(pull.body.clone())),
        ("state", Json::Str(state.to_string())),
        ("merged", Json::Bool(pull.merged)),
        (
            "mergeable",
            Json::Bool(pull_mergeable(repo, pull) == MergeStatus::CanMerge),
        ),
        (
            "head",
            obj(vec![
                ("ref", Json::Str(pull.head_ref.clone())),
                ("sha", Json::Str(head_sha)),
                (
                    "label",
                    Json::Str(format!("{viewer_login}:{}", pull.head_ref)),
                ),
            ]),
        ),
        (
            "base",
            obj(vec![
                ("ref", Json::Str(pull.base_ref.clone())),
                ("sha", Json::Str(base_sha)),
                (
                    "label",
                    Json::Str(format!("{viewer_login}:{}", pull.base_ref)),
                ),
            ]),
        ),
        (
            "user",
            obj(vec![("login", Json::Str(viewer_login.to_string()))]),
        ),
        (
            "created_at",
            Json::Str(iso_from_epoch(
                crate::scenario::SCENARIO_EPOCH_SEC + pull.created_seq,
            )),
        ),
    ];
    if pull.merged {
        fields.push((
            "merged_at",
            Json::Str(pull.merged_at.clone().unwrap_or_default()),
        ));
        fields.push((
            "merge_commit_sha",
            Json::Str(
                pull.merge_commit_sha
                    .map(|oid| oid_hex(&oid))
                    .unwrap_or_default(),
            ),
        ));
    }
    obj(fields)
}

fn comment_json(id: u64, body: &str) -> Json {
    obj(vec![
        ("id", Json::Int(id as i64)),
        ("body", Json::Str(body.to_string())),
        (
            "created_at",
            Json::Str(iso_from_epoch(crate::scenario::SCENARIO_EPOCH_SEC)),
        ),
    ])
}

// ---------------------------------------------------------------------------
// Repositories.

fn list_user_repos(store: &RwLock<Store>, req: &Request) -> Response {
    let store = store.read().unwrap();
    let per_page = req
        .query_param("per_page")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(30)
        .clamp(1, 100);
    let page = req
        .query_param("page")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(1)
        .max(1);
    let repos = store.viewer_repos();
    let total = repos.len();
    let start = (page - 1) * per_page;
    let items: Vec<Json> = repos
        .iter()
        .skip(start)
        .take(per_page)
        .map(|repo| repo_json(repo))
        .collect();
    let mut res = json_locked(&store, 200, Json::Arr(items));
    let pages = total.div_ceil(per_page);
    if page < pages {
        let next = format!(
            "http://{}/api/v3/user/repos?per_page={per_page}&page={}",
            req.host(),
            page + 1
        );
        let last = format!(
            "http://{}/api/v3/user/repos?per_page={per_page}&page={pages}",
            req.host()
        );
        res = res.header(
            "link",
            &format!("<{next}>; rel=\"next\", <{last}>; rel=\"last\""),
        );
    }
    res
}

fn create_repo(store: &RwLock<Store>, req: &Request) -> Response {
    let body = match parse_body(req) {
        Ok(body) => body,
        Err(res) => return res,
    };
    let Some(name) = body_str(&body, "name") else {
        return error_response(store, 422, "name is required");
    };
    let name = name.to_string();
    if name.is_empty() || name.len() > 100 {
        return error_response(store, 422, "invalid repository name");
    }
    let description = body_str(&body, "description").unwrap_or("").to_string();
    let private = body_bool(&body, "private").unwrap_or(false);
    let viewer = {
        let store = store.read().unwrap();
        store.viewer.login.clone()
    };
    let mut store = store.write().unwrap();
    match store.create_repo(&name, &description, private) {
        Ok(_) => match store.get_repo(&viewer, &name) {
            Some(repo) => json_locked(&store, 201, repo_json(repo)),
            None => json_locked(
                &store,
                500,
                obj(vec![("message", Json::Str("internal error".to_string()))]),
            ),
        },
        Err(_) => json_locked(
            &store,
            422,
            obj(vec![(
                "message",
                Json::Str("Repository already exists".to_string()),
            )]),
        ),
    }
}

fn with_repo<F>(store: &RwLock<Store>, owner: &str, name: &str, f: F) -> Response
where
    F: FnOnce(&Store, &Repo) -> Response,
{
    let store = store.read().unwrap();
    match store.get_repo(owner, name) {
        Some(repo) => f(&store, repo),
        None => json_locked(
            &store,
            404,
            obj(vec![("message", Json::Str("Not Found".to_string()))]),
        ),
    }
}

fn get_repo(store: &RwLock<Store>, owner: &str, name: &str) -> Response {
    with_repo(store, owner, name, |store, repo| {
        json_locked(store, 200, repo_json(repo))
    })
}

fn patch_repo(store: &RwLock<Store>, req: &Request, owner: &str, name: &str) -> Response {
    let body = match parse_body(req) {
        Ok(body) => body,
        Err(res) => return res,
    };
    let mut store = store.write().unwrap();
    let Some(repo) = store.get_repo_mut(owner, name) else {
        return json_locked(
            &store,
            404,
            obj(vec![("message", Json::Str("Not Found".to_string()))]),
        );
    };
    if let Some(description) = body_str(&body, "description") {
        repo.description = description.to_string();
    }
    if let Some(private) = body_bool(&body, "private") {
        repo.private = private;
    }
    let json = repo_json(repo);
    json_locked(&store, 200, json)
}

fn delete_repo(store: &RwLock<Store>, owner: &str, name: &str) -> Response {
    let mut store = store.write().unwrap();
    if store.delete_repo(owner, name) {
        Response::new(204, Vec::new())
            .header("content-type", "application/json; charset=utf-8")
            .header("x-github-request-id", &next_request_id())
    } else {
        json_locked(
            &store,
            404,
            obj(vec![("message", Json::Str("Not Found".to_string()))]),
        )
    }
}

fn list_branches(store: &RwLock<Store>, owner: &str, name: &str) -> Response {
    with_repo(store, owner, name, |store, repo| {
        let mut branches: Vec<Json> = repo
            .sorted_refs()
            .into_iter()
            .filter(|(name, _)| name.starts_with("refs/heads/"))
            .map(|(name, oid)| {
                obj(vec![
                    (
                        "name",
                        Json::Str(name.trim_start_matches("refs/heads/").to_string()),
                    ),
                    ("commit", obj(vec![("sha", Json::Str(oid_hex(&oid)))])),
                ])
            })
            .collect();
        branches.sort_by(|a, b| {
            let an = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let bn = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
            an.cmp(bn)
        });
        json_locked(store, 200, Json::Arr(branches))
    })
}

fn get_branch(store: &RwLock<Store>, owner: &str, name: &str, branch: &[&str]) -> Response {
    let branch_name = branch.join("/");
    with_repo(store, owner, name, |store, repo| {
        match repo.branch_tip(&branch_name) {
            Some(oid) => json_locked(
                store,
                200,
                obj(vec![
                    ("name", Json::Str(branch_name)),
                    ("commit", obj(vec![("sha", Json::Str(oid_hex(&oid)))])),
                ]),
            ),
            None => json_locked(
                store,
                404,
                obj(vec![("message", Json::Str("Not Found".to_string()))]),
            ),
        }
    })
}

fn get_ref(store: &RwLock<Store>, owner: &str, name: &str, branch: &[&str]) -> Response {
    let branch_name = branch.join("/");
    with_repo(store, owner, name, |store, repo| {
        match repo.branch_tip(&branch_name) {
            Some(oid) => json_locked(
                store,
                200,
                obj(vec![
                    ("ref", Json::Str(format!("refs/heads/{branch_name}"))),
                    ("object", obj(vec![("sha", Json::Str(oid_hex(&oid)))])),
                ]),
            ),
            None => json_locked(
                store,
                404,
                obj(vec![("message", Json::Str("Not Found".to_string()))]),
            ),
        }
    })
}

fn create_ref(store: &RwLock<Store>, req: &Request, owner: &str, name: &str) -> Response {
    let body = match parse_body(req) {
        Ok(body) => body,
        Err(res) => return res,
    };
    let Some(ref_name) = body_str(&body, "ref").map(|s| s.to_string()) else {
        return error_response(store, 422, "ref is required");
    };
    let Some(sha) = body_str(&body, "sha").and_then(oid_from_hex) else {
        return error_response(store, 422, "sha is required");
    };
    if !ref_name.starts_with("refs/") {
        return error_response(store, 422, "invalid ref");
    }
    let mut store = store.write().unwrap();
    let Some(repo) = store.get_repo_mut(owner, name) else {
        return json_locked(
            &store,
            404,
            obj(vec![("message", Json::Str("Not Found".to_string()))]),
        );
    };
    if repo.refs.contains_key(&ref_name) {
        return json_locked(
            &store,
            422,
            obj(vec![(
                "message",
                Json::Str("Reference already exists".to_string()),
            )]),
        );
    }
    if repo.objects.get(&sha).is_none() {
        return json_locked(
            &store,
            422,
            obj(vec![(
                "message",
                Json::Str("Object does not exist".to_string()),
            )]),
        );
    }
    repo.set_ref(&ref_name, Some(sha));
    json_locked(
        &store,
        201,
        obj(vec![
            ("ref", Json::Str(ref_name)),
            ("object", obj(vec![("sha", Json::Str(oid_hex(&sha)))])),
        ]),
    )
}

fn list_commits(store: &RwLock<Store>, req: &Request, owner: &str, name: &str) -> Response {
    let sha_param = req.query_param("sha").map(|s| s.to_string());
    with_repo(store, owner, name, |store, repo| {
        let tip = match sha_param {
            Some(ref_name) => repo.resolve_ref(&ref_name),
            None => repo.default_tip(),
        };
        let Some(tip) = tip else {
            return json_locked(store, 200, Json::Arr(vec![]));
        };
        let commits: Vec<Json> = repo
            .commits_from(tip)
            .into_iter()
            .filter_map(|oid| {
                let object = repo.objects.get(&oid)?;
                let full = parse_commit_full(&object.data)?;
                Some(obj(vec![
                    ("sha", Json::Str(oid_hex(&oid))),
                    (
                        "commit",
                        obj(vec![
                            ("message", Json::Str(full.message)),
                            (
                                "author",
                                obj(vec![
                                    ("name", Json::Str(full.author.clone())),
                                    ("date", Json::Str(iso_from_epoch(full.timestamp))),
                                ]),
                            ),
                            (
                                "committer",
                                obj(vec![
                                    ("name", Json::Str(full.committer.clone())),
                                    ("date", Json::Str(iso_from_epoch(full.timestamp))),
                                ]),
                            ),
                        ]),
                    ),
                ]))
            })
            .collect();
        json_locked(store, 200, Json::Arr(commits))
    })
}

// ---------------------------------------------------------------------------
// Contents.

fn put_contents(
    store: &RwLock<Store>,
    req: &Request,
    owner: &str,
    name: &str,
    path: &[&str],
) -> Response {
    let body = match parse_body(req) {
        Ok(body) => body,
        Err(res) => return res,
    };
    let path_str = path.join("/");
    if path_str.is_empty() {
        return error_response(store, 422, "path is required");
    }
    let Some(message) = body_str(&body, "message").map(|s| s.to_string()) else {
        return error_response(store, 422, "message is required");
    };
    let Some(content_b64) = body_str(&body, "content").map(|s| s.to_string()) else {
        return error_response(store, 422, "content is required");
    };
    let Some(content) = base64_decode(&content_b64) else {
        return error_response(store, 422, "content is not base64");
    };
    let expected_sha = body_str(&body, "sha").and_then(oid_from_hex);
    let branch = body_str(&body, "branch").map(|s| s.to_string());
    let mut store = store.write().unwrap();
    // Scalars first: the repo borrow below is taken from `store`.
    let identity = store.identity();
    let timestamp = store.next_commit_timestamp();
    let Some(repo) = store.get_repo_mut(owner, name) else {
        return json_locked(
            &store,
            404,
            obj(vec![("message", Json::Str("Not Found".to_string()))]),
        );
    };
    match write_file(
        repo,
        &identity,
        timestamp,
        &path_str,
        &content,
        &message,
        branch.as_deref(),
        expected_sha,
    ) {
        Ok((blob_oid, commit_oid, created)) => {
            let body = obj(vec![
                (
                    "content",
                    obj(vec![
                        ("name", Json::Str(path.last().unwrap_or(&"").to_string())),
                        ("path", Json::Str(path_str.clone())),
                        ("sha", Json::Str(oid_hex(&blob_oid))),
                        ("size", Json::Int(content.len() as i64)),
                        ("encoding", Json::Str("base64".to_string())),
                        ("content", Json::Str(content_b64)),
                    ]),
                ),
                (
                    "commit",
                    obj(vec![
                        ("sha", Json::Str(oid_hex(&commit_oid))),
                        ("message", Json::Str(message)),
                    ]),
                ),
            ]);
            json_locked(&store, if created { 201 } else { 200 }, body)
        }
        Err("stale sha") => json_locked(
            &store,
            409,
            obj(vec![(
                "message",
                Json::Str("sha does not match".to_string()),
            )]),
        ),
        Err("missing sha") => json_locked(
            &store,
            422,
            obj(vec![(
                "message",
                Json::Str("sha is required to update a file".to_string()),
            )]),
        ),
        Err(_) => json_locked(
            &store,
            422,
            obj(vec![(
                "message",
                Json::Str("cannot write file".to_string()),
            )]),
        ),
    }
}

fn get_contents(
    store: &RwLock<Store>,
    req: &Request,
    owner: &str,
    name: &str,
    path: &[&str],
) -> Response {
    let path_str = path.join("/");
    let ref_param = req.query_param("ref").map(|s| s.to_string());
    with_repo(store, owner, name, |store, repo| {
        let tip = match ref_param {
            Some(ref_name) => repo.resolve_ref(&ref_name),
            None => repo.default_tip(),
        };
        let Some(tip) = tip else {
            return json_locked(
                store,
                404,
                obj(vec![("message", Json::Str("Not Found".to_string()))]),
            );
        };
        if path_str.is_empty() {
            // The root directory.
            return match repo.list_dir(tip, "") {
                Some(entries) => json_locked(store, 200, dir_json("", &entries)),
                None => json_locked(
                    store,
                    404,
                    obj(vec![("message", Json::Str("Not Found".to_string()))]),
                ),
            };
        }
        if let Some((blob_oid, bytes)) = repo.read_path(tip, &path_str) {
            return json_locked(
                store,
                200,
                obj(vec![
                    ("type", Json::Str("file".to_string())),
                    ("encoding", Json::Str("base64".to_string())),
                    ("content", Json::Str(base64_encode(&bytes))),
                    ("sha", Json::Str(oid_hex(&blob_oid))),
                    ("size", Json::Int(bytes.len() as i64)),
                    ("name", Json::Str(path.last().unwrap_or(&"").to_string())),
                    ("path", Json::Str(path_str)),
                ]),
            );
        }
        match repo.list_dir(tip, &path_str) {
            Some(entries) => json_locked(store, 200, dir_json(&path_str, &entries)),
            None => json_locked(
                store,
                404,
                obj(vec![("message", Json::Str("Not Found".to_string()))]),
            ),
        }
    })
}

fn dir_json(dir: &str, entries: &[crate::git::TreeEntry]) -> Json {
    let mut items: Vec<Json> = entries
        .iter()
        .map(|entry| {
            let path = if dir.is_empty() {
                entry.name.clone()
            } else {
                format!("{dir}/{}", entry.name)
            };
            let is_dir = entry.mode == 0o40000;
            obj(vec![
                (
                    "type",
                    Json::Str(if is_dir { "dir" } else { "file" }.to_string()),
                ),
                ("name", Json::Str(entry.name.clone())),
                ("path", Json::Str(path)),
                ("sha", Json::Str(oid_hex(&entry.oid))),
            ])
        })
        .collect();
    items.sort_by(|a, b| {
        let an = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let bn = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
        an.cmp(bn)
    });
    Json::Arr(items)
}

fn delete_contents(
    store: &RwLock<Store>,
    req: &Request,
    owner: &str,
    name: &str,
    path: &[&str],
) -> Response {
    let body = match parse_body(req) {
        Ok(body) => body,
        Err(res) => return res,
    };
    let path_str = path.join("/");
    let message = body_str(&body, "message")
        .unwrap_or("delete file")
        .to_string();
    let branch = body_str(&body, "branch").map(|s| s.to_string());
    let mut store = store.write().unwrap();
    let identity = store.identity();
    let timestamp = store.next_commit_timestamp();
    let Some(repo) = store.get_repo_mut(owner, name) else {
        return json_locked(
            &store,
            404,
            obj(vec![("message", Json::Str("Not Found".to_string()))]),
        );
    };
    match delete_file(
        repo,
        &identity,
        timestamp,
        &path_str,
        &message,
        branch.as_deref(),
    ) {
        Ok(commit_oid) => json_locked(
            &store,
            200,
            obj(vec![
                (
                    "commit",
                    obj(vec![
                        ("sha", Json::Str(oid_hex(&commit_oid))),
                        ("message", Json::Str(message)),
                    ]),
                ),
                ("content", Json::Null),
            ]),
        ),
        Err(_) => json_locked(
            &store,
            404,
            obj(vec![("message", Json::Str("Not Found".to_string()))]),
        ),
    }
}

// ---------------------------------------------------------------------------
// Labels, issues, comments.

fn create_label(store: &RwLock<Store>, req: &Request, owner: &str, name: &str) -> Response {
    let body = match parse_body(req) {
        Ok(body) => body,
        Err(res) => return res,
    };
    let Some(label_name) = body_str(&body, "name").map(|s| s.to_string()) else {
        return error_response(store, 422, "name is required");
    };
    let color = body_str(&body, "color").unwrap_or("ededed").to_string();
    let mut store = store.write().unwrap();
    let Some(repo) = store.get_repo_mut(owner, name) else {
        return json_locked(
            &store,
            404,
            obj(vec![("message", Json::Str("Not Found".to_string()))]),
        );
    };
    if let Some(existing) = repo.labels.iter().find(|l| l.name == label_name) {
        let json = label_json(existing);
        return json_locked(&store, 201, json);
    }
    repo.labels.push(Label {
        name: label_name,
        color,
    });
    let json = match repo.labels.last() {
        Some(label) => label_json(label),
        None => obj(vec![("message", Json::Str("internal error".to_string()))]),
    };
    json_locked(&store, 201, json)
}

fn create_issue(store: &RwLock<Store>, req: &Request, owner: &str, name: &str) -> Response {
    let body = match parse_body(req) {
        Ok(body) => body,
        Err(res) => return res,
    };
    let Some(title) = body_str(&body, "title").map(|s| s.to_string()) else {
        return error_response(store, 422, "title is required");
    };
    let body_text = body_str(&body, "body").unwrap_or("").to_string();
    let labels: Vec<String> = body
        .get("labels")
        .and_then(|v| v.as_arr())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let mut store = store.write().unwrap();
    // Find the first unknown label (if any) before borrowing the repo mutably.
    let bad_label: Option<String> = match store.get_repo(owner, name) {
        Some(repo) => labels
            .iter()
            .find(|label| !repo.labels.iter().any(|l| l.name == **label))
            .cloned(),
        None => {
            return json_locked(
                &store,
                404,
                obj(vec![("message", Json::Str("Not Found".to_string()))]),
            )
        }
    };
    if let Some(label) = bad_label {
        return json_locked(
            &store,
            422,
            obj(vec![(
                "message",
                Json::Str(format!("Label '{label}' does not exist")),
            )]),
        );
    }
    let id = store.next_issue_id;
    store.next_issue_id += 1;
    store.created_seq += 1;
    let created_seq = store.created_seq;
    let viewer_login = store.viewer.login.clone();
    let Some(repo) = store.get_repo_mut(owner, name) else {
        return json_locked(
            &store,
            404,
            obj(vec![("message", Json::Str("Not Found".to_string()))]),
        );
    };
    repo.number_seq += 1;
    let number = repo.number_seq;
    let issue = Issue {
        id,
        number,
        title,
        body: body_text,
        state: IssueState::Open,
        labels,
        comments: Vec::new(),
        created_seq,
    };
    repo.issues.push(issue);
    let json = match repo.issues.last() {
        Some(issue) => issue_json(&viewer_login, repo, issue),
        None => obj(vec![("message", Json::Str("internal error".to_string()))]),
    };
    json_locked(&store, 201, json)
}

fn list_issues(store: &RwLock<Store>, req: &Request, owner: &str, name: &str) -> Response {
    let state = req.query_param("state").unwrap_or("open").to_string();
    let direction = req.query_param("direction").unwrap_or("desc").to_string();
    let sort = req.query_param("sort").unwrap_or("created").to_string();
    let _ = sort;
    with_repo(store, owner, name, |store, repo| {
        let mut issues: Vec<&Issue> = repo
            .issues
            .iter()
            .filter(|issue| match state.as_str() {
                "all" => true,
                "closed" => issue.state == IssueState::Closed,
                _ => issue.state == IssueState::Open,
            })
            .collect();
        // Created order is the number order.
        issues.sort_by_key(|issue| issue.number);
        if direction == "desc" {
            issues.reverse();
        }
        let items: Vec<Json> = issues
            .into_iter()
            .map(|issue| issue_json(&store.viewer.login, repo, issue))
            .collect();
        json_locked(store, 200, Json::Arr(items))
    })
}

fn find_issue(repo: &Repo, number: u32) -> Option<&Issue> {
    repo.issues.iter().find(|issue| issue.number == number)
}

fn get_issue(store: &RwLock<Store>, owner: &str, name: &str, number: &str) -> Response {
    let Some(number) = number_param(number) else {
        return error_response(store, 404, "Not Found");
    };
    with_repo(store, owner, name, |store, repo| {
        match find_issue(repo, number) {
            Some(issue) => json_locked(store, 200, issue_json(&store.viewer.login, repo, issue)),
            None => json_locked(
                store,
                404,
                obj(vec![("message", Json::Str("Not Found".to_string()))]),
            ),
        }
    })
}

fn patch_issue(
    store: &RwLock<Store>,
    req: &Request,
    owner: &str,
    name: &str,
    number: &str,
) -> Response {
    let body = match parse_body(req) {
        Ok(body) => body,
        Err(res) => return res,
    };
    let Some(number) = number_param(number) else {
        return error_response(store, 404, "Not Found");
    };
    let mut store = store.write().unwrap();
    let viewer_login = store.viewer.login.clone();
    let Some(repo) = store.get_repo_mut(owner, name) else {
        return json_locked(
            &store,
            404,
            obj(vec![("message", Json::Str("Not Found".to_string()))]),
        );
    };
    let Some(issue) = repo.issues.iter_mut().find(|issue| issue.number == number) else {
        return json_locked(
            &store,
            404,
            obj(vec![("message", Json::Str("Not Found".to_string()))]),
        );
    };
    if let Some(state) = body_str(&body, "state") {
        issue.state = if state == "closed" {
            IssueState::Closed
        } else {
            IssueState::Open
        };
    }
    if let Some(title) = body_str(&body, "title") {
        issue.title = title.to_string();
    }
    if let Some(body_text) = body_str(&body, "body") {
        issue.body = body_text.to_string();
    }
    // Clone the mutated issue first: `issue` holds a mutable borrow of
    // `repo`, so the JSON must be built from the snapshot.
    let snapshot = issue.clone();
    let json = issue_json(&viewer_login, repo, &snapshot);
    json_locked(&store, 200, json)
}

fn create_comment(
    store: &RwLock<Store>,
    req: &Request,
    owner: &str,
    name: &str,
    number: &str,
) -> Response {
    let body = match parse_body(req) {
        Ok(body) => body,
        Err(res) => return res,
    };
    let Some(number) = number_param(number) else {
        return error_response(store, 404, "Not Found");
    };
    let Some(comment_body) = body_str(&body, "body").map(|s| s.to_string()) else {
        return error_response(store, 422, "body is required");
    };
    let mut store = store.write().unwrap();
    let Some(repo) = store.get_repo_mut(owner, name) else {
        return json_locked(
            &store,
            404,
            obj(vec![("message", Json::Str("Not Found".to_string()))]),
        );
    };
    // Take the comment id before borrowing the issue (both live in `repo`).
    repo.comment_seq += 1;
    let id = repo.comment_seq;
    let Some(issue) = repo.issues.iter_mut().find(|issue| issue.number == number) else {
        return json_locked(
            &store,
            404,
            obj(vec![("message", Json::Str("Not Found".to_string()))]),
        );
    };
    issue.comments.push(crate::store::Comment {
        id,
        body: comment_body.clone(),
    });
    json_locked(&store, 201, comment_json(id, &comment_body))
}

fn list_comments(store: &RwLock<Store>, owner: &str, name: &str, number: &str) -> Response {
    let Some(number) = number_param(number) else {
        return error_response(store, 404, "Not Found");
    };
    with_repo(store, owner, name, |store, repo| {
        match find_issue(repo, number) {
            Some(issue) => {
                let items: Vec<Json> = issue
                    .comments
                    .iter()
                    .map(|c| comment_json(c.id, &c.body))
                    .collect();
                json_locked(store, 200, Json::Arr(items))
            }
            None => json_locked(
                store,
                404,
                obj(vec![("message", Json::Str("Not Found".to_string()))]),
            ),
        }
    })
}

// ---------------------------------------------------------------------------
// Pull requests.

fn create_pull(store: &RwLock<Store>, req: &Request, owner: &str, name: &str) -> Response {
    let body = match parse_body(req) {
        Ok(body) => body,
        Err(res) => return res,
    };
    let Some(title) = body_str(&body, "title").map(|s| s.to_string()) else {
        return error_response(store, 422, "title is required");
    };
    let body_text = body_str(&body, "body").unwrap_or("").to_string();
    let Some(head) = body_str(&body, "head").map(|s| s.to_string()) else {
        return error_response(store, 422, "head is required");
    };
    let Some(base) = body_str(&body, "base").map(|s| s.to_string()) else {
        return error_response(store, 422, "base is required");
    };
    let mut store = store.write().unwrap();
    // Validate the branches and take the ids before borrowing the repo mutably.
    let (base_sha, head_ok) = match store.get_repo(owner, name) {
        Some(repo) => (repo.branch_tip(&base), repo.branch_tip(&head).is_some()),
        None => {
            return json_locked(
                &store,
                404,
                obj(vec![("message", Json::Str("Not Found".to_string()))]),
            )
        }
    };
    let Some(base_sha) = base_sha else {
        return json_locked(
            &store,
            422,
            obj(vec![(
                "message",
                Json::Str("base branch does not exist".to_string()),
            )]),
        );
    };
    if !head_ok {
        return json_locked(
            &store,
            422,
            obj(vec![(
                "message",
                Json::Str("head branch does not exist".to_string()),
            )]),
        );
    }
    let id = store.next_pull_id;
    store.next_pull_id += 1;
    store.created_seq += 1;
    let created_seq = store.created_seq;
    let viewer_login = store.viewer.login.clone();
    let Some(repo) = store.get_repo_mut(owner, name) else {
        return json_locked(
            &store,
            404,
            obj(vec![("message", Json::Str("Not Found".to_string()))]),
        );
    };
    repo.number_seq += 1;
    let number = repo.number_seq;
    let pull = Pull {
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
    };
    repo.pulls.push(pull);
    let json = match repo.pulls.last() {
        Some(pull) => pull_json(&viewer_login, repo, pull),
        None => obj(vec![("message", Json::Str("internal error".to_string()))]),
    };
    json_locked(&store, 201, json)
}

fn list_pulls(store: &RwLock<Store>, req: &Request, owner: &str, name: &str) -> Response {
    let state = req.query_param("state").unwrap_or("open").to_string();
    with_repo(store, owner, name, |store, repo| {
        let pulls: Vec<&Pull> = repo
            .pulls
            .iter()
            .filter(|pull| match state.as_str() {
                "all" => true,
                "closed" => pull.state == IssueState::Closed,
                _ => pull.state == IssueState::Open,
            })
            .collect();
        let items: Vec<Json> = pulls
            .into_iter()
            .map(|pull| pull_json(&store.viewer.login, repo, pull))
            .collect();
        json_locked(store, 200, Json::Arr(items))
    })
}

fn find_pull(repo: &Repo, number: u32) -> Option<&Pull> {
    repo.pulls.iter().find(|pull| pull.number == number)
}

fn get_pull(store: &RwLock<Store>, owner: &str, name: &str, number: &str) -> Response {
    let Some(number) = number_param(number) else {
        return error_response(store, 404, "Not Found");
    };
    with_repo(store, owner, name, |store, repo| {
        match find_pull(repo, number) {
            Some(pull) => json_locked(store, 200, pull_json(&store.viewer.login, repo, pull)),
            None => json_locked(
                store,
                404,
                obj(vec![("message", Json::Str("Not Found".to_string()))]),
            ),
        }
    })
}

fn list_pull_files(store: &RwLock<Store>, owner: &str, name: &str, number: &str) -> Response {
    let Some(number) = number_param(number) else {
        return error_response(store, 404, "Not Found");
    };
    with_repo(store, owner, name, |store, repo| {
        let Some(pull) = find_pull(repo, number) else {
            return json_locked(
                store,
                404,
                obj(vec![("message", Json::Str("Not Found".to_string()))]),
            );
        };
        let Some(head_tip) = repo.branch_tip(&pull.head_ref) else {
            return json_locked(
                store,
                404,
                obj(vec![("message", Json::Str("Not Found".to_string()))]),
            );
        };
        let Some(base_commit) = repo.objects.commit_of(&pull.base_sha) else {
            return json_locked(
                store,
                404,
                obj(vec![("message", Json::Str("Not Found".to_string()))]),
            );
        };
        let Some(head_commit) = repo.objects.commit_of(&head_tip) else {
            return json_locked(
                store,
                404,
                obj(vec![("message", Json::Str("Not Found".to_string()))]),
            );
        };
        let files: Vec<Json> = diff_trees(repo, base_commit.tree, head_commit.tree)
            .into_iter()
            .filter(|change| change.status != "removed")
            .map(|change| {
                let changes = change.changes();
                obj(vec![
                    ("filename", Json::Str(change.path)),
                    ("status", Json::Str(change.status.to_string())),
                    ("additions", Json::Int(change.additions as i64)),
                    ("deletions", Json::Int(change.deletions as i64)),
                    ("changes", Json::Int(changes as i64)),
                ])
            })
            .collect();
        json_locked(store, 200, Json::Arr(files))
    })
}

fn merge_pull(
    store: &RwLock<Store>,
    req: &Request,
    owner: &str,
    name: &str,
    number: &str,
) -> Response {
    let _body = match parse_body(req) {
        Ok(body) => body,
        Err(res) => return res,
    };
    let Some(number) = number_param(number) else {
        return error_response(store, 404, "Not Found");
    };
    let mut store = store.write().unwrap();
    let exists = store
        .get_repo(owner, name)
        .is_some_and(|repo| repo.pulls.iter().any(|p| p.number == number));
    if !exists {
        return json_locked(
            &store,
            404,
            obj(vec![("message", Json::Str("Not Found".to_string()))]),
        );
    }
    let identity = store.identity();
    let timestamp = store.next_commit_timestamp();
    let merged_at = iso_from_epoch(crate::scenario::SCENARIO_EPOCH_SEC + store.commit_seq);
    let Some(repo) = store.get_repo_mut(owner, name) else {
        return json_locked(
            &store,
            404,
            obj(vec![("message", Json::Str("Not Found".to_string()))]),
        );
    };
    match squash_merge(repo, &identity, timestamp, merged_at, number) {
        Ok(sha) => {
            let pull = repo.pulls.iter().find(|p| p.number == number);
            let message = match pull {
                Some(pull) => format!("Merge pull request #{number} from {}", pull.head_ref),
                None => format!("Merge pull request #{number}"),
            };
            json_locked(
                &store,
                200,
                obj(vec![
                    ("merged", Json::Bool(true)),
                    ("sha", Json::Str(oid_hex(&sha))),
                    ("message", Json::Str(message)),
                ]),
            )
        }
        Err("conflict") => json_locked(
            &store,
            405,
            obj(vec![("message", Json::Str("Merge conflict".to_string()))]),
        ),
        Err(_) => json_locked(
            &store,
            404,
            obj(vec![("message", Json::Str("Not Found".to_string()))]),
        ),
    }
}
