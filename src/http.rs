//! Minimal HTTP/1.1 server: ONE non-blocking epoll reactor thread.
//!
//! Round 10 (H10): no thread per connection, no worker pool, no queue, no
//! dispatch hop. The single reactor thread waits in `epoll_wait`, reads a
//! ready socket, handles the request inline, and writes the response
//! inline. With 50 concurrent clones on a 2-CPU pod, thread-per-connection
//! leaves ~50 threads runnable, and the scheduling delays + allocator
//! contention inflate the tail that sets the round wall; a single thread
//! has zero context switches, zero locks, and zero handoffs. Handlers are
//! microseconds (info/refs ~48 µs, upload-pack ~57 µs), so inline handling
//! cannot starve the loop.
//!
//! epoll itself is reached through raw syscalls (`core::arch::asm!`):
//! pure-std Rust has no epoll binding and this crate takes no dependencies.
//! All other socket operations use `std::net` in non-blocking mode.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::Arc;

pub struct Request {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn query_param(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// The request's host (for absolute pagination URLs).
    pub fn host(&self) -> &str {
        self.header("host").unwrap_or("127.0.0.1")
    }
}

pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16, body: Vec<u8>) -> Self {
        Response {
            status,
            headers: Vec::new(),
            body,
        }
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        422 => "Unprocessable Entity",
        500 => "Internal Server Error",
        _ => "Unknown",
    }
}

/// Percent-decode a path or query component (`+` stays a plus in paths).
pub fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let h = hex_val(bytes[i + 1]);
            let l = hex_val(bytes[i + 2]);
            if let (Some(h), Some(l)) = (h, l) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Split a request target into a decoded path and decoded query pairs.
fn split_target(target: &str) -> (String, Vec<(String, String)>) {
    let (path_part, query_part) = match target.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (target, None),
    };
    let path = percent_decode(path_part);
    let mut query = Vec::new();
    if let Some(q) = query_part {
        for pair in q.split('&') {
            if pair.is_empty() {
                continue;
            }
            match pair.split_once('=') {
                Some((k, v)) => query.push((
                    percent_decode(&k.replace('+', " ")),
                    percent_decode(&v.replace('+', " ")),
                )),
                None => query.push((percent_decode(pair), String::new())),
            }
        }
    }
    (path, query)
}

const MAX_HEADERS: usize = 64 * 1024;
const MAX_BODY: usize = 64 * 1024 * 1024;
/// Generous connection cap: the suite opens ~50 at once; the cap only
/// stops unbounded idle-connection growth.
const MAX_CONNS: usize = 4096;

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// The state of one in-flight request parse.
enum Parse {
    /// Need more bytes.
    Incomplete,
    /// A complete request, plus how many bytes of the buffer it consumed.
    /// `continue_sent` says whether the 100-continue interim reply was
    /// already emitted for these headers.
    Complete(Request, usize),
    /// The request is malformed: drop the connection.
    Invalid,
}

/// Try to parse one request from the head of `buf` (headers + body). The
/// returned bool says whether the interim `100 Continue` reply should be
/// sent now: the headers are complete, the client asked for it, the body
/// is still coming, and it was not sent yet (`continue_sent`).
fn try_parse(buf: &[u8], continue_sent: bool) -> (Parse, bool) {
    let Some(header_end) = find_subslice(buf, b"\r\n\r\n") else {
        if buf.len() > MAX_HEADERS {
            return (Parse::Invalid, false);
        }
        return (Parse::Incomplete, false);
    };
    let header_bytes = &buf[..header_end];
    let mut rest = &buf[header_end + 4..];
    let header_text = String::from_utf8_lossy(header_bytes);
    let mut lines = header_text.lines();
    let Some(request_line) = lines.next() else {
        return (Parse::Invalid, continue_sent);
    };
    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return (Parse::Invalid, continue_sent);
    };
    let method = method.to_string();
    let target = target.to_string();
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    let get_header = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };
    // 100-continue: the client waits for the interim reply before it
    // sends the body. It is only needed when the body has not arrived yet.
    let wants_continue =
        get_header("expect").is_some_and(|v| v.eq_ignore_ascii_case("100-continue"));
    // Body: Content-Length, or chunked.
    let mut body: Vec<u8> = Vec::new();
    let mut consumed = header_end + 4;
    if let Some(len) = get_header("content-length").and_then(|v| v.parse::<usize>().ok()) {
        if len > MAX_BODY {
            return (Parse::Invalid, false);
        }
        if rest.len() < len {
            return (Parse::Incomplete, wants_continue && !continue_sent);
        }
        body = rest[..len].to_vec();
        consumed += len;
    } else if get_header("transfer-encoding")
        .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"))
    {
        loop {
            let Some(line_end) = find_subslice(rest, b"\r\n") else {
                return (Parse::Incomplete, wants_continue && !continue_sent);
            };
            let size_text = String::from_utf8_lossy(&rest[..line_end]).to_string();
            let Ok(size) = usize::from_str_radix(size_text.trim(), 16) else {
                return (Parse::Invalid, false);
            };
            rest = &rest[line_end + 2..];
            consumed += line_end + 2;
            if size == 0 {
                break;
            }
            if body.len() + size > MAX_BODY {
                return (Parse::Invalid, false);
            }
            if rest.len() < size + 2 {
                return (Parse::Incomplete, wants_continue && !continue_sent);
            }
            body.extend_from_slice(&rest[..size]);
            rest = &rest[size + 2..];
            consumed += size + 2;
        }
    }
    let (path, query) = split_target(&target);
    (
        Parse::Complete(
            Request {
                method,
                path,
                query,
                headers,
                body,
            },
            consumed,
        ),
        false,
    )
}

/// Serialize a response (headers + body) into bytes.
fn render_response(req: &Request, res: &Response) -> (Vec<u8>, bool) {
    let close = req
        .header("connection")
        .is_some_and(|v| v.eq_ignore_ascii_case("close"));
    let mut out = Vec::with_capacity(res.body.len() + 256);
    out.extend_from_slice(format!("HTTP/1.1 {} {}\r\n", res.status, reason(res.status)).as_bytes());
    out.extend_from_slice(format!("content-length: {}\r\n", res.body.len()).as_bytes());
    out.extend_from_slice(
        format!(
            "connection: {}\r\n",
            if close { "close" } else { "keep-alive" }
        )
        .as_bytes(),
    );
    for (name, value) in &res.headers {
        out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(&res.body);
    (out, close)
}

// ---------------------------------------------------------------------------
// epoll through raw syscalls (x86_64). No crates, no libc binding.

const SYS_EPOLL_CREATE1: u64 = 291;
const SYS_EPOLL_CTL: u64 = 233;
const SYS_EPOLL_WAIT: u64 = 232;

const EPOLL_CTL_ADD: u64 = 1;
const EPOLL_CTL_MOD: u64 = 3;

const EPOLLIN: u32 = 0x1;
const EPOLLOUT: u32 = 0x4;
const EPOLLERR: u32 = 0x8;
const EPOLLHUP: u32 = 0x10;

#[repr(C)]
#[derive(Clone, Copy)]
struct EpollEvent {
    events: u32,
    data: u64,
}

unsafe fn syscall3(nr: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    let ret: i64;
    // The `syscall` instruction clobbers rcx and r11; both are declared.
    core::arch::asm!(
        "syscall",
        inlateout("rax") nr as i64 => ret,
        in("rdi") a1,
        in("rsi") a2,
        in("rdx") a3,
        lateout("rcx") _,
        lateout("r11") _,
        options(nostack)
    );
    ret
}

unsafe fn syscall4(nr: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> i64 {
    let ret: i64;
    core::arch::asm!(
        "syscall",
        inlateout("rax") nr as i64 => ret,
        in("rdi") a1,
        in("rsi") a2,
        in("rdx") a3,
        in("r10") a4,
        lateout("rcx") _,
        lateout("r11") _,
        options(nostack)
    );
    ret
}

struct Epoll {
    fd: RawFd,
}

impl Epoll {
    fn new() -> Option<Self> {
        let fd = unsafe { syscall3(SYS_EPOLL_CREATE1, 0, 0, 0) };
        if fd < 0 {
            None
        } else {
            Some(Epoll { fd: fd as RawFd })
        }
    }

    fn ctl(&self, op: u64, fd: RawFd, events: u32) -> bool {
        let mut event = EpollEvent {
            events,
            data: fd as u64,
        };
        let ret = unsafe {
            syscall4(
                SYS_EPOLL_CTL,
                self.fd as u64,
                op,
                fd as u64,
                &mut event as *mut EpollEvent as u64,
            )
        };
        ret == 0
    }

    fn add(&self, fd: RawFd, events: u32) -> bool {
        self.ctl(EPOLL_CTL_ADD, fd, events)
    }

    fn modify(&self, fd: RawFd, events: u32) -> bool {
        self.ctl(EPOLL_CTL_MOD, fd, events)
    }

    /// Wait up to `timeout_ms` (-1 = forever); returns (fd, events) pairs.
    fn wait(&self, out: &mut [EpollEvent], timeout_ms: i32) -> usize {
        let ret = unsafe {
            syscall4(
                SYS_EPOLL_WAIT,
                self.fd as u64,
                out.as_mut_ptr() as u64,
                out.len() as u64,
                timeout_ms as u64,
            )
        };
        if ret < 0 {
            0
        } else {
            ret as usize
        }
    }
}

// ---------------------------------------------------------------------------
// The reactor.

struct Conn {
    stream: TcpStream,
    /// Bytes read but not yet parsed into requests.
    in_buf: Vec<u8>,
    /// Response bytes not yet written.
    out_buf: Vec<u8>,
    out_pos: usize,
    /// Whether the current request's 100-continue interim reply went out.
    continue_sent: bool,
    /// Close the connection once `out_buf` drains.
    close_after_write: bool,
    /// EPOLLOUT is currently armed for this fd.
    want_write: bool,
}

impl Conn {
    fn new(stream: TcpStream) -> Self {
        Conn {
            stream,
            in_buf: Vec::new(),
            out_buf: Vec::new(),
            out_pos: 0,
            continue_sent: false,
            close_after_write: false,
            want_write: false,
        }
    }

    /// Flush as much of `out_buf` as the socket takes now. Returns false
    /// when the connection is dead (the reactor must drop it).
    fn flush(&mut self) -> bool {
        while self.out_pos < self.out_buf.len() {
            match self.stream.write(&self.out_buf[self.out_pos..]) {
                Ok(0) => return false,
                Ok(n) => self.out_pos += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => return false,
            }
        }
        if self.out_pos == self.out_buf.len() {
            self.out_buf.clear();
            self.out_pos = 0;
        }
        true
    }
}

/// Serve forever on `listener` from the single reactor thread. At most one
/// request is handled at a time, inline, on a ready socket.
pub fn serve(listener: TcpListener, handler: Arc<dyn Fn(&Request) -> Response + Send + Sync>) {
    let _ = listener.set_nonblocking(true);
    let epoll = match Epoll::new() {
        Some(epoll) => epoll,
        None => {
            eprintln!("forge: epoll_create1 failed");
            std::process::exit(1);
        }
    };
    if !epoll.add(listener.as_raw_fd(), EPOLLIN) {
        eprintln!("forge: epoll_ctl ADD listener failed");
        std::process::exit(1);
    }
    let mut conns: HashMap<RawFd, Conn> = HashMap::new();
    let mut events = vec![EpollEvent { events: 0, data: 0 }; 256];
    let mut read_chunk = [0u8; 16384];
    loop {
        let ready = epoll.wait(&mut events, 1000);
        for i in 0..ready {
            let event = events[i];
            let fd = event.data as RawFd;
            let ev = event.events;
            if fd == listener.as_raw_fd() {
                // Accept until WouldBlock; each new socket joins the reactor.
                while let Ok((stream, _)) = listener.accept() {
                    if conns.len() >= MAX_CONNS {
                        drop(stream);
                        continue;
                    }
                    let _ = stream.set_nodelay(true);
                    let _ = stream.set_nonblocking(true);
                    let fd = stream.as_raw_fd();
                    if epoll.add(fd, EPOLLIN) {
                        conns.insert(fd, Conn::new(stream));
                    }
                }
                continue;
            }
            // A client connection is ready.
            let Some(conn) = conns.get_mut(&fd) else {
                continue;
            };
            let mut dead = false;
            // Readable (or error/hangup: a read tells us for sure).
            if ev & (EPOLLIN | EPOLLERR | EPOLLHUP) != 0 {
                loop {
                    match conn.stream.read(&mut read_chunk) {
                        Ok(0) => {
                            dead = true;
                            break;
                        }
                        Ok(n) => {
                            conn.in_buf.extend_from_slice(&read_chunk[..n]);
                            if conn.in_buf.len() > MAX_HEADERS + MAX_BODY {
                                dead = true;
                                break;
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(_) => {
                            dead = true;
                            break;
                        }
                    }
                }
            }
            // Parse every complete request in the buffer and answer inline.
            if !dead {
                loop {
                    let (parsed, needs_continue) = try_parse(&conn.in_buf, conn.continue_sent);
                    match parsed {
                        Parse::Incomplete => {
                            // Headers complete, body still coming, client
                            // waiting on 100-continue: send the interim now
                            // so it proceeds. Sent at most once per request.
                            if needs_continue {
                                conn.out_buf
                                    .extend_from_slice(b"HTTP/1.1 100 Continue\r\n\r\n");
                                conn.continue_sent = true;
                            }
                            break;
                        }
                        Parse::Invalid => {
                            dead = true;
                            break;
                        }
                        Parse::Complete(req, consumed) => {
                            conn.in_buf.drain(..consumed);
                            conn.continue_sent = false;
                            let res = handler(&req);
                            let (bytes, close) = render_response(&req, &res);
                            conn.out_buf.extend_from_slice(&bytes);
                            if close {
                                conn.close_after_write = true;
                            }
                        }
                    }
                }
            }
            // Flush whatever is queued.
            if !dead && !conn.out_buf.is_empty() && !conn.flush() {
                dead = true;
            }
            // Arm or disarm EPOLLOUT to match the pending output.
            if !dead {
                let pending = !conn.out_buf.is_empty();
                if pending != conn.want_write {
                    let events = if pending { EPOLLIN | EPOLLOUT } else { EPOLLIN };
                    if epoll.modify(fd, events) {
                        conn.want_write = pending;
                    }
                }
                // Writable event with nothing pending: nothing to do.
                if conn.out_buf.is_empty() && conn.close_after_write {
                    dead = true;
                }
            }
            if dead {
                conns.remove(&fd);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decode_roundtrip() {
        assert_eq!(percent_decode("/a%20b/c%2Fd"), "/a b/c/d");
        assert_eq!(percent_decode("a+b"), "a+b");
    }

    #[test]
    fn parse_get_request() {
        let raw = b"GET /api/v3/user?per_page=2&page=1 HTTP/1.1\r\nhost: x\r\n\r\n";
        let (parsed, _) = try_parse(raw, false);
        match parsed {
            Parse::Complete(req, consumed) => {
                assert_eq!(consumed, raw.len());
                assert_eq!(req.method, "GET");
                assert_eq!(req.path, "/api/v3/user");
                assert_eq!(req.query_param("per_page"), Some("2"));
            }
            _ => panic!("expected a complete request"),
        }
    }

    #[test]
    fn parse_post_with_body_and_pipelined() {
        let raw = b"POST /x HTTP/1.1\r\ncontent-length: 5\r\n\r\nhelloGET /y HTTP/1.1\r\n\r\n";
        let (parsed, _) = try_parse(raw, false);
        match parsed {
            Parse::Complete(req, consumed) => {
                assert_eq!(req.body, b"hello");
                // 39 header bytes + 5 body bytes.
                assert_eq!(consumed, 44);
                let (parsed2, _) = try_parse(&raw[consumed..], false);
                assert!(matches!(parsed2, Parse::Complete(_, _)));
            }
            _ => panic!("expected a complete request"),
        }
    }

    #[test]
    fn parse_chunked_body() {
        let raw = b"POST /x HTTP/1.1\r\ntransfer-encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        let (parsed, _) = try_parse(raw, false);
        match parsed {
            Parse::Complete(req, _) => assert_eq!(req.body, b"hello"),
            _ => panic!("expected a complete request"),
        }
    }

    #[test]
    fn parse_incomplete_and_invalid() {
        let (parsed, _) = try_parse(b"GET /x HTTP/1.1\r\nhost: y\r\n", false);
        assert!(matches!(parsed, Parse::Incomplete));
        let (parsed, _) = try_parse(b"GARBAGE\r\n\r\n", false);
        assert!(matches!(parsed, Parse::Invalid));
    }
}
