//! Model Context Protocol endpoint (`POST /mcp`): JSON-RPC 2.0 over HTTP.
//! Supports both the modern `server/discover` era (2026-07-28) and the
//! legacy `initialize` era (2025-11-25 and earlier).

use crate::http::{Request, Response};
use crate::json::{obj, Json};
use crate::rest::next_request_id;
use crate::store::{squash_merge, IssueState, Store};
use crate::util::iso_from_epoch;
use std::sync::RwLock;

const SERVER_NAME: &str = "forge";
const SERVER_VERSION: &str = "1.0.0";
const SUPPORTED_VERSIONS: &[&str] = &[
    "2026-07-28",
    "2025-11-25",
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
];

pub fn handle(store: &RwLock<Store>, req: &Request) -> Response {
    let text = String::from_utf8_lossy(&req.body);
    let body = match crate::json::parse(&text) {
        Ok(body) => body,
        Err(_) => {
            // A parse error: HTTP 400 with -32700, echoing the request id
            // when it can still be read from the raw body.
            let id = extract_id(&text);
            let error = obj(vec![
                ("jsonrpc", Json::Str("2.0".to_string())),
                ("id", id),
                (
                    "error",
                    obj(vec![
                        ("code", Json::Int(-32700)),
                        ("message", Json::Str("Parse error".to_string())),
                    ]),
                ),
            ]);
            return Response::new(400, error.stringify().into_bytes())
                .header("content-type", "application/json; charset=utf-8")
                .header("x-github-request-id", &next_request_id());
        }
    };
    // A request without an id is a notification: 202 with no body.
    let Some(id) = body.get("id").cloned() else {
        return Response::new(202, Vec::new()).header("x-github-request-id", &next_request_id());
    };
    if id == Json::Null {
        return Response::new(202, Vec::new()).header("x-github-request-id", &next_request_id());
    }
    let method = body
        .get("method")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let params = body.get("params").cloned().unwrap_or(Json::Null);
    let result = dispatch(store, &method, &params);
    let response = match result {
        Ok(result) => obj(vec![
            ("jsonrpc", Json::Str("2.0".to_string())),
            ("id", id),
            ("result", result),
        ]),
        Err((code, message)) => obj(vec![
            ("jsonrpc", Json::Str("2.0".to_string())),
            ("id", id),
            (
                "error",
                obj(vec![
                    ("code", Json::Int(code)),
                    ("message", Json::Str(message)),
                ]),
            ),
        ]),
    };
    Response::new(200, response.stringify().into_bytes())
        .header("content-type", "application/json; charset=utf-8")
        .header("x-github-request-id", &next_request_id())
}

/// Best-effort id extraction from a (possibly truncated) JSON body.
fn extract_id(text: &str) -> Json {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 4 <= bytes.len() {
        if &text[i..i + 4] == "\"id\"" {
            let mut j = i + 4;
            while j < bytes.len()
                && (bytes[j] == b' ' || bytes[j] == b'\t' || bytes[j] == b'\n' || bytes[j] == b'\r')
            {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b':' {
                j += 1;
                while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                    j += 1;
                }
                let start = j;
                if j < bytes.len() && bytes[j] == b'"' {
                    j += 1;
                    while j < bytes.len() && bytes[j] != b'"' {
                        j += 1;
                    }
                    return Json::Str(text[start + 1..j].to_string());
                }
                while j < bytes.len() && (bytes[j] as char).is_ascii_digit() {
                    j += 1;
                }
                if j > start {
                    if let Ok(n) = text[start..j].parse::<i64>() {
                        return Json::Int(n);
                    }
                }
            }
        }
        i += 1;
    }
    Json::Null
}

fn server_info() -> Json {
    obj(vec![
        ("name", Json::Str(SERVER_NAME.to_string())),
        ("version", Json::Str(SERVER_VERSION.to_string())),
    ])
}

fn dispatch(store: &RwLock<Store>, method: &str, params: &Json) -> Result<Json, (i64, String)> {
    match method {
        "server/discover" => Ok(obj(vec![
            (
                "supportedVersions",
                Json::Arr(
                    SUPPORTED_VERSIONS
                        .iter()
                        .map(|v| Json::Str(v.to_string()))
                        .collect(),
                ),
            ),
            ("capabilities", obj(vec![("tools", obj(vec![]))])),
            ("serverInfo", server_info()),
            (
                "_meta",
                obj(vec![("io.modelcontextprotocol/serverInfo", server_info())]),
            ),
        ])),
        "initialize" => {
            let requested = params
                .get("protocolVersion")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| "2025-11-25".to_string());
            let version = if SUPPORTED_VERSIONS.contains(&requested.as_str()) {
                requested
            } else {
                "2025-11-25".to_string()
            };
            Ok(obj(vec![
                ("protocolVersion", Json::Str(version)),
                ("capabilities", obj(vec![("tools", obj(vec![]))])),
                ("serverInfo", server_info()),
                (
                    "_meta",
                    obj(vec![("io.modelcontextprotocol/serverInfo", server_info())]),
                ),
            ]))
        }
        "ping" => Ok(obj(vec![])),
        "tools/list" => Ok(obj(vec![("tools", Json::Arr(tool_definitions()))])),
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let arguments = params.get("arguments").cloned().unwrap_or(Json::Null);
            call_tool(store, &name, &arguments)
        }
        _ => Err((-32601, format!("method not found: {method}"))),
    }
}

// ---------------------------------------------------------------------------
// Tools.

fn tool_definitions() -> Vec<Json> {
    let string = |description: &str| {
        obj(vec![
            ("type", Json::Str("string".to_string())),
            ("description", Json::Str(description.to_string())),
        ])
    };
    vec![
        obj(vec![
            ("name", Json::Str("create_repo".to_string())),
            (
                "description",
                Json::Str("Create a repository for the authenticated user.".to_string()),
            ),
            (
                "inputSchema",
                obj(vec![
                    ("type", Json::Str("object".to_string())),
                    (
                        "properties",
                        obj(vec![
                            ("name", string("The repository name.")),
                            (
                                "description",
                                string("A short description of the repository."),
                            ),
                            (
                                "private",
                                obj(vec![("type", Json::Str("boolean".to_string()))]),
                            ),
                        ]),
                    ),
                    ("required", Json::Arr(vec![Json::Str("name".to_string())])),
                ]),
            ),
        ]),
        obj(vec![
            ("name", Json::Str("get_repo".to_string())),
            (
                "description",
                Json::Str("Get a repository by owner and name.".to_string()),
            ),
            (
                "inputSchema",
                obj(vec![
                    ("type", Json::Str("object".to_string())),
                    (
                        "properties",
                        obj(vec![
                            ("owner", string("The repository owner.")),
                            ("repo", string("The repository name.")),
                        ]),
                    ),
                    (
                        "required",
                        Json::Arr(vec![
                            Json::Str("owner".to_string()),
                            Json::Str("repo".to_string()),
                        ]),
                    ),
                ]),
            ),
        ]),
        obj(vec![
            ("name", Json::Str("create_issue".to_string())),
            (
                "description",
                Json::Str("Create an issue in a repository.".to_string()),
            ),
            (
                "inputSchema",
                obj(vec![
                    ("type", Json::Str("object".to_string())),
                    (
                        "properties",
                        obj(vec![
                            ("owner", string("The repository owner.")),
                            ("repo", string("The repository name.")),
                            ("title", string("The issue title.")),
                            ("body", string("The issue body.")),
                        ]),
                    ),
                    (
                        "required",
                        Json::Arr(vec![
                            Json::Str("owner".to_string()),
                            Json::Str("repo".to_string()),
                            Json::Str("title".to_string()),
                        ]),
                    ),
                ]),
            ),
        ]),
        obj(vec![
            ("name", Json::Str("list_issues".to_string())),
            (
                "description",
                Json::Str("List issues in a repository.".to_string()),
            ),
            (
                "inputSchema",
                obj(vec![
                    ("type", Json::Str("object".to_string())),
                    (
                        "properties",
                        obj(vec![
                            ("owner", string("The repository owner.")),
                            ("repo", string("The repository name.")),
                            ("state", string("Filter by state: open, closed, or all.")),
                        ]),
                    ),
                    (
                        "required",
                        Json::Arr(vec![
                            Json::Str("owner".to_string()),
                            Json::Str("repo".to_string()),
                        ]),
                    ),
                ]),
            ),
        ]),
        obj(vec![
            ("name", Json::Str("create_pull_request".to_string())),
            (
                "description",
                Json::Str("Create a pull request in a repository.".to_string()),
            ),
            (
                "inputSchema",
                obj(vec![
                    ("type", Json::Str("object".to_string())),
                    (
                        "properties",
                        obj(vec![
                            ("owner", string("The repository owner.")),
                            ("repo", string("The repository name.")),
                            ("title", string("The pull request title.")),
                            ("head", string("The head branch.")),
                            ("base", string("The base branch.")),
                        ]),
                    ),
                    (
                        "required",
                        Json::Arr(vec![
                            Json::Str("owner".to_string()),
                            Json::Str("repo".to_string()),
                            Json::Str("title".to_string()),
                            Json::Str("head".to_string()),
                            Json::Str("base".to_string()),
                        ]),
                    ),
                ]),
            ),
        ]),
        obj(vec![
            ("name", Json::Str("merge_pull_request".to_string())),
            (
                "description",
                Json::Str("Merge a pull request with a squash merge.".to_string()),
            ),
            (
                "inputSchema",
                obj(vec![
                    ("type", Json::Str("object".to_string())),
                    (
                        "properties",
                        obj(vec![
                            ("owner", string("The repository owner.")),
                            ("repo", string("The repository name.")),
                            (
                                "pull_number",
                                obj(vec![
                                    ("type", Json::Str("integer".to_string())),
                                    (
                                        "description",
                                        Json::Str("The pull request number.".to_string()),
                                    ),
                                ]),
                            ),
                        ]),
                    ),
                    (
                        "required",
                        Json::Arr(vec![
                            Json::Str("owner".to_string()),
                            Json::Str("repo".to_string()),
                            Json::Str("pull_number".to_string()),
                        ]),
                    ),
                ]),
            ),
        ]),
    ]
}

/// Normalize a tool name: strip any server prefix (`github__create_issue`,
/// `server/create_issue`, or `github/create_issue`), lower-case. The kit
/// matches a prefixed name as a suffix, case-insensitively.
fn normalize_tool(name: &str) -> String {
    let after_slash = name.rsplit('/').next().unwrap_or(name);
    let after_prefix = after_slash.rsplit("__").next().unwrap_or(after_slash);
    after_prefix.to_ascii_lowercase()
}

/// A tool result: text content blocks plus the same value as structured data.
fn tool_result(value: Json) -> Json {
    obj(vec![
        (
            "content",
            Json::Arr(vec![obj(vec![
                ("type", Json::Str("text".to_string())),
                ("text", Json::Str(value.stringify())),
            ])]),
        ),
        ("structuredContent", value),
    ])
}

fn tool_error(message: &str) -> Json {
    obj(vec![
        ("isError", Json::Bool(true)),
        (
            "content",
            Json::Arr(vec![obj(vec![
                ("type", Json::Str("text".to_string())),
                ("text", Json::Str(message.to_string())),
            ])]),
        ),
    ])
}

fn arg_str<'a>(args: &'a Json, name: &str) -> Option<&'a str> {
    args.get(name)?.as_str()
}

fn arg_int(args: &Json, name: &str) -> Option<i64> {
    args.get(name)?.as_i64()
}

fn call_tool(store: &RwLock<Store>, name: &str, args: &Json) -> Result<Json, (i64, String)> {
    match normalize_tool(name).as_str() {
        "create_repo" => {
            let Some(repo_name) = arg_str(args, "name").map(|s| s.to_string()) else {
                return Ok(tool_error("missing required argument: name"));
            };
            let description = arg_str(args, "description").unwrap_or("").to_string();
            let private = args
                .get("private")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let mut store = store.write().unwrap();
            match store.create_repo(&repo_name, &description, private) {
                Ok(_) => {
                    let viewer = store.viewer.login.clone();
                    let repo = store.get_repo(&viewer, &repo_name);
                    match repo {
                        Some(repo) => Ok(tool_result(crate::rest::repo_json(repo))),
                        None => Ok(tool_error("repository was not created")),
                    }
                }
                Err(_) => Ok(tool_error("repository already exists")),
            }
        }
        "get_repo" => {
            let (Some(owner), Some(repo_name)) = (arg_str(args, "owner"), arg_str(args, "repo"))
            else {
                return Ok(tool_error("missing required argument: owner or repo"));
            };
            let store = store.read().unwrap();
            match store.get_repo(owner, repo_name) {
                Some(repo) => Ok(tool_result(crate::rest::repo_json(repo))),
                None => Ok(tool_error("repository not found")),
            }
        }
        "create_issue" => {
            let (Some(owner), Some(repo_name), Some(title)) = (
                arg_str(args, "owner"),
                arg_str(args, "repo"),
                arg_str(args, "title"),
            ) else {
                return Ok(tool_error(
                    "missing required argument: owner, repo, or title",
                ));
            };
            let body_text = arg_str(args, "body").unwrap_or("").to_string();
            let mut store = store.write().unwrap();
            let id = store.next_issue_id;
            store.next_issue_id += 1;
            store.created_seq += 1;
            let created_seq = store.created_seq;
            let viewer_login = store.viewer.login.clone();
            let Some(repo) = store.get_repo_mut(owner, repo_name) else {
                return Ok(tool_error("repository not found"));
            };
            repo.number_seq += 1;
            let number = repo.number_seq;
            repo.issues.push(crate::store::Issue {
                id,
                number,
                title: title.to_string(),
                body: body_text,
                state: IssueState::Open,
                labels: Vec::new(),
                comments: Vec::new(),
                created_seq,
            });
            let json = match repo.issues.last() {
                Some(issue) => crate::rest::issue_json(&viewer_login, repo, issue),
                None => obj(vec![("message", Json::Str("internal error".to_string()))]),
            };
            Ok(tool_result(json))
        }
        "list_issues" => {
            let (Some(owner), Some(repo_name)) = (arg_str(args, "owner"), arg_str(args, "repo"))
            else {
                return Ok(tool_error("missing required argument: owner or repo"));
            };
            let state = arg_str(args, "state").unwrap_or("open");
            let store = store.read().unwrap();
            let Some(repo) = store.get_repo(owner, repo_name) else {
                return Ok(tool_error("repository not found"));
            };
            let issues: Vec<Json> = repo
                .issues
                .iter()
                .filter(|issue| match state {
                    "all" => true,
                    "closed" => issue.state == IssueState::Closed,
                    _ => issue.state == IssueState::Open,
                })
                .map(|issue| crate::rest::issue_json(&store.viewer.login, repo, issue))
                .collect();
            Ok(tool_result(Json::Arr(issues)))
        }
        "create_pull_request" => {
            let (Some(owner), Some(repo_name), Some(title), Some(head), Some(base)) = (
                arg_str(args, "owner"),
                arg_str(args, "repo"),
                arg_str(args, "title"),
                arg_str(args, "head"),
                arg_str(args, "base"),
            ) else {
                return Ok(tool_error(
                    "missing required argument: owner, repo, title, head, or base",
                ));
            };
            let mut store = store.write().unwrap();
            let (base_sha, head_ok) = match store.get_repo(owner, repo_name) {
                Some(repo) => (repo.branch_tip(base), repo.branch_tip(head).is_some()),
                None => return Ok(tool_error("repository not found")),
            };
            let Some(base_sha) = base_sha else {
                return Ok(tool_error("base branch does not exist"));
            };
            if !head_ok {
                return Ok(tool_error("head branch does not exist"));
            }
            let id = store.next_pull_id;
            store.next_pull_id += 1;
            store.created_seq += 1;
            let created_seq = store.created_seq;
            let viewer_login = store.viewer.login.clone();
            let Some(repo) = store.get_repo_mut(owner, repo_name) else {
                return Ok(tool_error("repository not found"));
            };
            repo.number_seq += 1;
            let number = repo.number_seq;
            repo.pulls.push(crate::store::Pull {
                id,
                number,
                title: title.to_string(),
                body: String::new(),
                head_ref: head.to_string(),
                base_ref: base.to_string(),
                base_sha,
                state: IssueState::Open,
                merged: false,
                merged_at: None,
                merge_commit_sha: None,
                created_seq,
            });
            let json = match repo.pulls.last() {
                Some(pull) => crate::rest::pull_json(&viewer_login, repo, pull),
                None => obj(vec![("message", Json::Str("internal error".to_string()))]),
            };
            Ok(tool_result(json))
        }
        "merge_pull_request" => {
            let (Some(owner), Some(repo_name), Some(number)) = (
                arg_str(args, "owner"),
                arg_str(args, "repo"),
                arg_int(args, "pull_number"),
            ) else {
                return Ok(tool_error(
                    "missing required argument: owner, repo, or pull_number",
                ));
            };
            let number = number as u32;
            let mut store = store.write().unwrap();
            let exists = store
                .get_repo(owner, repo_name)
                .is_some_and(|repo| repo.pulls.iter().any(|p| p.number == number));
            if !exists {
                return Ok(tool_error("pull request not found"));
            }
            let identity = store.identity();
            let timestamp = store.next_commit_timestamp();
            let merged_at = iso_from_epoch(crate::scenario::SCENARIO_EPOCH_SEC + store.commit_seq);
            let Some(repo) = store.get_repo_mut(owner, repo_name) else {
                return Ok(tool_error("repository not found"));
            };
            match squash_merge(repo, &identity, timestamp, merged_at, number) {
                Ok(sha) => Ok(tool_result(obj(vec![
                    ("merged", Json::Bool(true)),
                    ("sha", Json::Str(crate::git::oid_hex(&sha))),
                    ("number", Json::Int(number as i64)),
                ]))),
                Err("conflict") => Ok(tool_error("merge conflict")),
                Err(_) => Ok(tool_error("pull request not found")),
            }
        }
        _ => Err((-32602, format!("unknown tool: {name}"))),
    }
}
