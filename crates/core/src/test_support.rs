//! Test-only helpers shared across hi-core's test modules.
//!
//! `FakeApi` is a minimal HTTP server on an ephemeral port: it reads
//! each request fully, records it, answers with one canned response,
//! and closes. One accept loop per server, one connection per request
//! (every response says `Connection: close`).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

pub struct FakeApi {
    pub base_url: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl FakeApi {
    pub fn start(response: impl Into<String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let response = response.into();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let request = read_request(&mut stream);
                recorded.lock().unwrap().push(request);
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        Self {
            base_url: format!("http://127.0.0.1:{port}/v1"),
            requests,
        }
    }

    pub fn first_request(&self) -> String {
        self.requests
            .lock()
            .unwrap()
            .first()
            .cloned()
            .unwrap_or_default()
    }
}

/// SSE response for the given `data:` frames.
pub fn sse_response(frames: &[&str]) -> String {
    let mut body = String::new();
    for frame in frames {
        body.push_str(&format!("data: {frame}\n\n"));
    }
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

/// JSON response with a computed Content-Length.
pub fn json_response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

/// Read a whole HTTP request: headers through the blank line, then
/// exactly `Content-Length` bytes of body.
fn read_request(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut buf = [0u8; 1024];
    let header_end = loop {
        let n = stream.read(&mut buf).unwrap();
        assert!(n > 0, "client closed before sending headers");
        bytes.extend_from_slice(&buf[..n]);
        if let Some(pos) = find_subsequence(&bytes, b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]).to_string();
    let lowered = headers.to_ascii_lowercase();
    let content_length = lowered
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while bytes.len() < header_end + content_length {
        let n = stream.read(&mut buf).unwrap();
        assert!(n > 0, "client closed before sending body");
        bytes.extend_from_slice(&buf[..n]);
    }
    String::from_utf8_lossy(&bytes).to_string()
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
