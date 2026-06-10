use std::io;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc::UnboundedSender;

use crate::app::event::AppEvent;

/// Loopback callback listener for browser SSO (RFC 8252 native-app flow).
/// The backend 303-redirects the browser to `http://127.0.0.1:{port}/callback`
/// with the exchange code; the listener delivers it as an AppEvent and exits.
pub struct SsoListener {
    pub port: u16,
    handle: tokio::task::JoinHandle<()>,
}

impl SsoListener {
    pub fn abort(&self) {
        self.handle.abort();
    }
}

pub fn start(tx: UnboundedSender<AppEvent>) -> io::Result<SsoListener> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let handle = tokio::spawn(async move {
        let result = match serve_one(listener).await {
            Ok(result) => result,
            Err(err) => Err(format!("callback listener failed: {err}")),
        };
        let _ = tx.send(AppEvent::SsoCallback(result));
    });
    Ok(SsoListener { port, handle })
}

async fn serve_one(listener: std::net::TcpListener) -> io::Result<Result<String, String>> {
    let listener = tokio::net::TcpListener::from_std(listener)?;
    loop {
        let (mut stream, _) = listener.accept().await?;
        let request_line = read_request_line(&mut stream).await?;
        let Some(result) = parse_request_line(&request_line) else {
            // Stray request (favicon, prefetch) — keep waiting for the code.
            let _ = stream.write_all(&response(404, "Not Found")).await;
            continue;
        };
        let page = match &result {
            Ok(_) => "Sign-in complete. You can close this window and return to the terminal.",
            Err(_) => "Sign-in failed. Return to the terminal to see the error.",
        };
        let _ = stream.write_all(&response(200, page)).await;
        let _ = stream.shutdown().await;
        return Ok(result);
    }
}

async fn read_request_line(stream: &mut tokio::net::TcpStream) -> io::Result<String> {
    let mut buf = vec![0u8; 8192];
    let mut len = 0;
    while len < buf.len() {
        let n = stream.read(&mut buf[len..]).await?;
        if n == 0 {
            break;
        }
        len += n;
        if buf[..len].windows(2).any(|w| w == b"\r\n") {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf[..len]);
    Ok(text.lines().next().unwrap_or_default().to_string())
}

/// `GET /callback?code=furu_mx_... HTTP/1.1` → Ok(code) / Err(error).
/// Values are plain tokens (no percent-encoded characters expected).
fn parse_request_line(line: &str) -> Option<Result<String, String>> {
    let path = line.split_whitespace().nth(1)?;
    let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
    let mut code = None;
    let mut error = None;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        match key {
            "code" if !value.is_empty() => code = Some(value.to_string()),
            "error" if !value.is_empty() => error = Some(value.to_string()),
            _ => {}
        }
    }
    if let Some(error) = error {
        return Some(Err(format!("SSO failed: {error}")));
    }
    code.map(Ok)
}

fn response(status: u16, body: &str) -> Vec<u8> {
    let reason = if status == 200 { "OK" } else { "Not Found" };
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>furumi</title></head>\
         <body style=\"font-family:sans-serif;background:#101114;color:#f5f2ea;\
         display:grid;place-items:center;min-height:100vh;margin:0\"><p>{body}</p></body></html>"
    );
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_code_from_request_line() {
        assert_eq!(
            parse_request_line("GET /callback?code=furu_mx_abc HTTP/1.1"),
            Some(Ok("furu_mx_abc".to_string()))
        );
    }

    #[test]
    fn parses_error_from_request_line() {
        assert_eq!(
            parse_request_line("GET /callback?error=provider_denied HTTP/1.1"),
            Some(Err("SSO failed: provider_denied".to_string()))
        );
    }

    #[test]
    fn ignores_unrelated_requests() {
        assert_eq!(parse_request_line("GET /favicon.ico HTTP/1.1"), None);
        assert_eq!(parse_request_line("GET /callback HTTP/1.1"), None);
    }
}
