//! forge: a GitHub-compatible git forge in pure-std Rust.
//!
//! Round 15: a rewrite of the Round 10 entry (same reactor, same store,
//! new GraphQL and REST layers written from the kit spec). A
//! single-threaded non-blocking epoll reactor serves every connection
//! (see `http::serve`). The only other thread is the done-watcher, which
//! sleeps and polls `$MAKEFUN_ENV_URL/done` every 500 ms; it never
//! touches the store or the serving path.
//!
//! Round 15 hardening: `register_env` returns at once when
//! `MAKEFUN_ENV_URL` is set but empty (main.ts passes `""` when the
//! variable is unset, and the old code spent ~15 s retrying a connect to
//! an empty host before giving up).

mod git;
mod git_http;
mod graphql;
mod http;
mod json;
mod mcp;
mod rest;
mod scenario;
mod sha1;
mod store;
mod util;
mod zlib;

use http::{Request, Response};
use std::net::TcpListener;
use std::sync::{Arc, RwLock};

/// Register this instance with the harness env server. The shop token is
/// NOT read from the register response: the trial puts the same token in
/// the `MAKEFUN_SHOP_TOKEN` environment variable, and that is the
/// authoritative source (the register response carries no token).
fn register_env(port: u16) {
    let Some(env_url) = std::env::var("MAKEFUN_ENV_URL").ok() else {
        return;
    };
    if env_url.trim().is_empty() {
        // Set-but-empty: there is nothing to register with. Return at
        // once instead of retrying a connect to an empty host for ~15 s.
        return;
    }
    let env_url = env_url.trim_end_matches('/');
    let host = env_url.trim_start_matches("http://");
    let body = format!("{{\"port\": {port}}}");
    for _ in 0..150 {
        match std::net::TcpStream::connect(host) {
            Ok(mut stream) => {
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(2000)));
                let _ = stream.set_write_timeout(Some(std::time::Duration::from_millis(2000)));
                let request = format!(
                    "POST /register HTTP/1.1\r\nhost: {host}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                if std::io::Write::write_all(&mut stream, request.as_bytes()).is_ok() {
                    let mut raw = Vec::new();
                    use std::io::Read;
                    let _ = stream.read_to_end(&mut raw);
                    break;
                }
            }
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(100)),
        }
    }
}

fn poll_done(env_url: &str) -> bool {
    let host = env_url.trim_start_matches("http://");
    let Ok(mut stream) = std::net::TcpStream::connect(host) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(2000)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_millis(2000)));
    let request = format!("GET /done HTTP/1.1\r\nhost: {host}\r\nconnection: close\r\n\r\n");
    use std::io::{Read, Write};
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw);
    text.contains("\"done\":true") || text.contains("\"done\": true")
}

fn main() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    let env_url = std::env::var("MAKEFUN_ENV_URL").unwrap_or_default();
    let token = std::env::var("MAKEFUN_SHOP_TOKEN").unwrap_or_default();
    register_env(port);
    // The done-watcher only sleeps and polls; it never serves or locks.
    if !env_url.is_empty() {
        let env_url = env_url.trim_end_matches('/').to_string();
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_millis(500));
            if poll_done(&env_url) {
                std::process::exit(0);
            }
        });
    }
    let store = Arc::new(RwLock::new(store::Store::new(
        scenario::build_scenario().viewer,
    )));
    let handler_store = store.clone();
    let token = Arc::new(token);
    let handler = Arc::new(move |req: &Request| -> Response {
        let mut response = route(&handler_store, &token, req);
        // Every reply carries a request id (the protocol checks require it
        // on REST replies, including the 401s; adding it everywhere is
        // harmless).
        if !response
            .headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("x-github-request-id"))
        {
            response
                .headers
                .push(("x-github-request-id".to_string(), rest::next_request_id()));
        }
        response
    });
    http::serve(listener, handler);
}

fn route(store: &Arc<RwLock<store::Store>>, token: &str, req: &Request) -> Response {
    if req.path == "/healthz" {
        return Response::new(200, b"{\"ok\":true}".to_vec())
            .header("content-type", "application/json; charset=utf-8");
    }
    // Bearer auth: when a token is configured, every route except the
    // health check needs the exact `Bearer <token>` header.
    if !token.is_empty() {
        let ok = req
            .header("authorization")
            .is_some_and(|value| value == format!("Bearer {token}"));
        if !ok {
            return Response::new(401, b"{\"message\":\"Requires authentication\"}".to_vec())
                .header("content-type", "application/json; charset=utf-8");
        }
    }
    if req.path.starts_with("/api/v3") {
        rest::handle(store, req)
    } else if req.path == "/api/graphql" {
        graphql::handle(store, req)
    } else if req.path == "/mcp" {
        mcp::handle(store, req)
    } else if req.path.contains(".git") {
        git_http::handle(store, req)
    } else {
        Response::new(404, b"{\"message\":\"Not Found\"}".to_vec())
            .header("content-type", "application/json; charset=utf-8")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_env_empty_url_returns_fast() {
        // Set-but-empty MAKEFUN_ENV_URL used to block startup ~15 s
        // (150 retries x 100 ms against an empty host). It must now
        // return immediately.
        std::env::set_var("MAKEFUN_ENV_URL", "");
        let start = std::time::Instant::now();
        register_env(1);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "register_env took {:?} with an empty URL",
            start.elapsed()
        );
        std::env::remove_var("MAKEFUN_ENV_URL");
    }
}
