//! Test-only helpers shared across hi-core's test modules.
//!
//! `FakeApi` is a minimal HTTP server on an ephemeral port: it reads
//! each request fully, records it, answers with one canned response
//! (or one per request, via `start_sequence`), and closes. One accept
//! loop per server, one connection per request (every response says
//! `Connection: close`).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;

use crate::event::Event;
use crate::message::Message;
use crate::provider::Provider;

pub struct FakeApi {
    pub base_url: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl FakeApi {
    pub fn start(response: impl Into<String>) -> Self {
        Self::start_sequence(vec![response.into()])
    }

    /// Like `start`, but each request consumes the next response; the
    /// last one repeats for any further requests. The agent loop's
    /// multi-round conversations need round-specific answers.
    pub fn start_sequence(responses: Vec<String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let responses = Arc::new(Mutex::new(responses));
        thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let request = read_request(&mut stream);
                recorded.lock().unwrap().push(request);
                let response = {
                    let mut responses = responses.lock().unwrap();
                    match responses.len() {
                        0 | 1 => responses.first().cloned().unwrap_or_default(),
                        _ => responses.remove(0),
                    }
                };
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

    /// The `index`-th recorded request (0-based), if it happened.
    pub fn request(&self, index: usize) -> Option<String> {
        self.requests.lock().unwrap().get(index).cloned()
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
    // A gzip'd request body (AD-0052 tests) is stored decoded, so
    // assertions see the payload a decompressing server would hand
    // its JSON parser. Headers stay exactly as they went out.
    let mut request = String::from_utf8_lossy(&bytes[..header_end]).to_string();
    let gzipped = lowered.lines().any(|line| {
        line.strip_prefix("content-encoding:")
            .is_some_and(|value| value.trim() == "gzip")
    });
    let body = &bytes[header_end..];
    if gzipped {
        let mut decoded = Vec::new();
        flate2::read::GzDecoder::new(body)
            .read_to_end(&mut decoded)
            .expect("test request body is valid gzip");
        request.push_str(&String::from_utf8_lossy(&decoded));
    } else {
        request.push_str(&String::from_utf8_lossy(body));
    }
    request
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// A provider that replays scripted rounds — the test double for the
/// agent loop. Each call to `respond` consumes the next scripted round
/// and records the history it was handed, so tests can assert on the
/// replayed conversation the same way `FakeApi` records raw requests.
pub(crate) struct ScriptedProvider {
    rounds: Vec<Vec<Event>>,
    /// What each round received, in order. Index `i` is round `i`'s
    /// input history.
    pub received: std::sync::Mutex<Vec<Vec<Message>>>,
}

impl ScriptedProvider {
    pub fn new(rounds: Vec<Vec<Event>>) -> Self {
        Self {
            rounds,
            received: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl Provider for ScriptedProvider {
    fn respond(&self, history: &[Message], tx: Sender<Event>) {
        let index = {
            let mut received = self.received.lock().unwrap();
            received.push(history.to_vec());
            received.len() - 1
        };
        let Some(round) = self.rounds.get(index) else {
            // A test that consumes more rounds than scripted is a bug
            // in the test — fail loudly instead of hanging.
            let _ = tx.send(Event::Error("scripted provider exhausted".into()));
            let _ = tx.send(Event::TurnEnd);
            return;
        };
        for event in round {
            if tx.send(event.clone()).is_err() {
                return;
            }
        }
    }
}

/// A tool that answers with its own name plus whatever arguments it
/// received — makes mutated vs. original arguments visible in loop and
/// wire tests.
pub(crate) struct EchoTool;

/// Always attaches one tiny PNG — the image-path stand-in for agent
/// and wire tests. `data` is base64 of `img`.
pub(crate) struct ImageTool;

impl crate::tool::Tool for ImageTool {
    fn name(&self) -> &str {
        "imgtool"
    }
    fn description(&self) -> &str {
        "attaches a tiny png"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    fn execute(&self, _arguments: &str) -> Result<String, String> {
        Ok("Read image file [image/png] (attached)".into())
    }
    fn execute_with_images(&self, _arguments: &str) -> Result<crate::tool::ToolOutput, String> {
        Ok(crate::tool::ToolOutput {
            content: "Read image file [image/png] (attached)".into(),
            images: vec![crate::tool::ImageBlock {
                data: "aW1n".into(),
                mime_type: "image/png".into(),
            }],
        })
    }
}

impl crate::tool::Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "echoes its arguments"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    fn execute(&self, arguments: &str) -> Result<String, String> {
        Ok(format!("echo{arguments}"))
    }
}
