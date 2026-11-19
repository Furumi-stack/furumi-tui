//! Minimal HTTP/1.1 server on 127.0.0.1 for podcast tests (no internet).
//! One request per connection (`Connection: close`). Besides feeds it serves
//! audio like a publisher: `Range` requests (206 / 416) unless disabled,
//! optionally without `Content-Length`, and connections cut after a number
//! of bytes to simulate outages.

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
    pub last_modified: Option<String>,
    /// `Location` of a redirect status.
    pub location: Option<String>,
    pub retry_after: Option<String>,
    /// Honour `Range` requests with 206 responses.
    pub ranges: bool,
    /// Send `Content-Length` (and the total in `Content-Range`).
    pub send_length: bool,
    /// Cut the body after this many bytes (absolute file offset)...
    pub cut_after: Option<usize>,
    /// ...for this many requests; later requests are served in full.
    pub fail_times: usize,
}

impl Route {
    pub fn new(body: impl Into<Vec<u8>>, content_type: &str) -> Route {
        Route {
            status: 200,
            body: Arc::new(body.into()),
            content_type: content_type.to_string(),
            etag: None,
            last_modified: None,
            location: None,
            retry_after: None,
            ranges: true,
            send_length: true,
            cut_after: None,
            fail_times: 0,
        }
    }

    /// A server that ignores `Range` and always answers 200 with the file.
    pub fn no_ranges(mut self) -> Route {
        self.ranges = false;
        self
    }

    /// No `Content-Length`: the client cannot know the file size.
    pub fn no_length(mut self) -> Route {
        self.send_length = false;
        self
    }

    /// The first `times` responses stop after byte `after`.
    pub fn cut(mut self, after: usize, times: usize) -> Route {
        self.cut_after = Some(after);
        self.fail_times = times;
        self
    }

    pub fn redirect(status: u16, location: &str) -> Route {
        Route {
            status,
            location: Some(location.to_string()),
            ..Route::new(Vec::new(), "text/plain")
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

    pub fn last_modified(mut self, value: &str) -> Route {
        self.last_modified = Some(value.to_string());
        self
    }

    pub fn retry_after(mut self, value: &str) -> Route {
        self.retry_after = Some(value.to_string());
        self
    }
}

/// Headers of one received request (names lowercased).
pub(super) type RequestHeaders = HashMap<String, String>;

#[derive(Default)]
struct ServerState {
    routes: HashMap<String, Route>,
    requests: HashMap<String, Vec<RequestHeaders>>,
    /// Cut responses served so far, per path.
    cuts: HashMap<String, usize>,
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

    /// Headers of every request received for `path`, in order.
    pub fn requests(&self, path: &str) -> Vec<RequestHeaders> {
        self.lock().requests.get(path).cloned().unwrap_or_default()
    }

    /// The `Range` header of every request for `path` (`None` if absent).
    pub fn ranges(&self, path: &str) -> Vec<Option<String>> {
        self.requests(path)
            .into_iter()
            .map(|headers| headers.get("range").cloned())
            .collect()
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
    let mut headers = RequestHeaders::new();
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
    let (route, cut) = {
        let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
        state
            .requests
            .entry(path.clone())
            .or_default()
            .push(headers.clone());
        let route = state.routes.get(&path).cloned();
        let cut = match &route {
            Some(route) if route.cut_after.is_some() => {
                let served = state.cuts.entry(path.clone()).or_default();
                if *served < route.fail_times {
                    *served += 1;
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
    let etag_matches = route
        .etag
        .as_ref()
        .is_some_and(|etag| headers.get("if-none-match") == Some(etag));
    let date_matches = route
        .last_modified
        .as_ref()
        .is_some_and(|date| headers.get("if-modified-since") == Some(date));
    if route.status == 200 && (etag_matches || date_matches) {
        let _ = write!(
            out,
            "HTTP/1.1 304 Not Modified\r\nConnection: close\r\n\r\n"
        );
        return;
    }
    let total = route.body.len();
    let mut status = route.status;
    let (mut start, mut end) = (0usize, total);
    let mut extra = String::new();
    if route.status == 200
        && route.ranges
        && let Some(range) = headers
            .get("range")
            .and_then(|range| range.strip_prefix("bytes="))
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
        status = 206;
        start = from;
        end = to
            .trim()
            .parse::<usize>()
            .map_or(total, |to| (to + 1).min(total));
        let size = if route.send_length {
            total.to_string()
        } else {
            "*".to_string()
        };
        extra.push_str(&format!(
            "Content-Range: bytes {start}-{}/{size}\r\n",
            end - 1
        ));
    }
    if route.ranges {
        extra.push_str("Accept-Ranges: bytes\r\n");
    }
    if route.send_length {
        extra.push_str(&format!("Content-Length: {}\r\n", end - start));
    }
    for (name, value) in [
        ("ETag", &route.etag),
        ("Last-Modified", &route.last_modified),
        ("Location", &route.location),
        ("Retry-After", &route.retry_after),
    ] {
        if let Some(value) = value {
            extra.push_str(&format!("{name}: {value}\r\n"));
        }
    }
    let reason = match status {
        200 => "OK",
        206 => "Partial Content",
        301 => "Moved Permanently",
        302 => "Found",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        404 => "Not Found",
        503 => "Service Unavailable",
        _ => "Status",
    };
    if write!(
        out,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {}\r\n{extra}Connection: close\r\n\r\n",
        route.content_type,
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

/// Sample rate of [`wav`].
pub(super) const WAV_RATE: u32 = 8_000;

/// A mono 16-bit PCM WAV file of `seconds` (8 kHz). Every sample of second
/// `n` has the value `n * 40` (up to 13 minutes), so a decoded sample tells where playback
/// is.
pub(super) fn wav(seconds: u32) -> Vec<u8> {
    const RATE: u32 = WAV_RATE;
    let samples = RATE * seconds;
    let data_len = samples * 2;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for n in 0..samples {
        let second = (n / RATE) as i16;
        out.extend_from_slice(&second.saturating_mul(40).to_le_bytes());
    }
    out
}
