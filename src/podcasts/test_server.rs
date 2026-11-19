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
    pub last_modified: Option<String>,
    /// `Location` of a redirect status.
    pub location: Option<String>,
    pub retry_after: Option<String>,
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
        }
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
    let route = {
        let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
        state
            .requests
            .entry(path.clone())
            .or_default()
            .push(headers.clone());
        state.routes.get(&path).cloned()
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
    let mut extra = String::new();
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
    let reason = match route.status {
        200 => "OK",
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
        "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n",
        route.status,
        route.content_type,
        route.body.len()
    )
    .is_err()
    {
        return;
    }
    let _ = out.write_all(&route.body);
    let _ = out.flush();
    let _ = out.shutdown(std::net::Shutdown::Both);
}
