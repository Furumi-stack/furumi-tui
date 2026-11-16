//! Minimal HTTP/1.1 server on 127.0.0.1 for podcast tests (no internet).
//! One request per connection (`Connection: close`).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

#[derive(Clone)]
pub(super) struct Route {
    pub status: u16,
    pub body: Arc<Vec<u8>>,
    pub content_type: String,
    pub etag: Option<String>,
    /// Honour `Range` requests with 206 responses.
    pub ranges: bool,
    /// Send `Content-Length` (otherwise the body is close-delimited).
    pub send_length: bool,
    /// Close the connection once the body reaches this absolute file offset,
    /// on the first `fail_times` responses (simulates a flaky network).
    pub cut_after: Option<usize>,
    pub fail_times: usize,
}

impl Route {
    pub fn new(body: impl Into<Vec<u8>>, content_type: &str) -> Route {
        Route {
            status: 200,
            body: Arc::new(body.into()),
            content_type: content_type.to_string(),
            etag: None,
            ranges: true,
            send_length: true,
            cut_after: None,
            fail_times: 0,
        }
    }

    pub fn status(mut self, status: u16) -> Route {
        self.status = status;
        self
    }

    pub fn etag(mut self, etag: &str) -> Route {
        self.etag = Some(etag.to_string());
        self
    }

    pub fn no_ranges(mut self) -> Route {
        self.ranges = false;
        self
    }

    pub fn no_length(mut self) -> Route {
        self.send_length = false;
        self
    }

    pub fn cut(mut self, after: usize, times: usize) -> Route {
        self.cut_after = Some(after);
        self.fail_times = times;
        self
    }
}

#[derive(Default)]
struct ServerState {
    routes: HashMap<String, Route>,
    /// Per path: the `Range` header of every request (None if absent).
    requests: HashMap<String, Vec<Option<String>>>,
    failures: HashMap<String, usize>,
}

pub(super) struct TestServer {
    base: String,
    state: Arc<Mutex<ServerState>>,
}

impl TestServer {
    pub fn start() -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let base = format!("http://{}", listener.local_addr().expect("local addr"));
        let state = Arc::new(Mutex::new(ServerState::default()));
        let accept_state = Arc::clone(&state);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let state = Arc::clone(&accept_state);
                std::thread::spawn(move || handle(stream, &state));
            }
        });
        TestServer { base, state }
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    pub fn set(&self, path: &str, route: Route) {
        self.lock().routes.insert(path.to_string(), route);
    }

    pub fn requests(&self, path: &str) -> Vec<Option<String>> {
        self.lock().requests.get(path).cloned().unwrap_or_default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ServerState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn handle(stream: TcpStream, state: &Mutex<ServerState>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(stream) => stream,
        Err(_) => return,
    });
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string();
    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let range_header = headers.get("range").cloned();
    let (route, cut) = {
        let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
        state
            .requests
            .entry(path.clone())
            .or_default()
            .push(range_header.clone());
        let route = state.routes.get(&path).cloned();
        let cut = match &route {
            Some(route) if route.cut_after.is_some() => {
                let failures = state.failures.entry(path.clone()).or_default();
                if *failures < route.fail_times {
                    *failures += 1;
                    route.cut_after
                } else {
                    None
                }
            }
            _ => None,
        };
        (route, cut)
    };
    let mut out = stream;
    let Some(route) = route else {
        let _ = out
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    };
    if let (Some(etag), Some(sent)) = (&route.etag, headers.get("if-none-match"))
        && etag == sent
    {
        let _ = write!(
            out,
            "HTTP/1.1 304 Not Modified\r\nETag: {etag}\r\nConnection: close\r\n\r\n"
        );
        return;
    }
    let total = route.body.len();
    let mut status = route.status;
    let mut start = 0usize;
    let mut end = total;
    let mut extra = String::new();
    if route.status == 200
        && route.ranges
        && let Some(range) = range_header
            .as_deref()
            .and_then(|r| r.strip_prefix("bytes="))
        && let Some((from, to)) = range.split_once('-')
        && let Ok(from) = from.trim().parse::<usize>()
    {
        if from >= total {
            let _ = write!(
                out,
                "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{total}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            return;
        }
        let to = to
            .trim()
            .parse::<usize>()
            .map_or(total, |to| (to + 1).min(total));
        status = 206;
        start = from;
        end = to;
        extra.push_str(&format!(
            "Content-Range: bytes {start}-{}/{total}\r\n",
            end - 1
        ));
    }
    if route.ranges {
        extra.push_str("Accept-Ranges: bytes\r\n");
    }
    if let Some(etag) = &route.etag {
        extra.push_str(&format!("ETag: {etag}\r\n"));
    }
    if route.send_length {
        extra.push_str(&format!("Content-Length: {}\r\n", end - start));
    }
    let reason = match status {
        200 => "OK",
        206 => "Partial Content",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Status",
    };
    if write!(
        out,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {}\r\n{extra}Connection: close\r\n\r\n",
        route.content_type
    )
    .is_err()
    {
        return;
    }
    let limit = cut.map_or(end, |cut| cut.clamp(start, end));
    for chunk in route.body[start..limit].chunks(64 * 1024) {
        if out.write_all(chunk).is_err() {
            return;
        }
    }
    let _ = out.flush();
    let _ = out.shutdown(std::net::Shutdown::Both);
}

/// Deterministic pseudo-random bytes.
pub(super) fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    out
}
