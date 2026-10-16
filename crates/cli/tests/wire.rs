//! Wire-level CLI integration (#21): the real binary against a
//! recording fake API. Unit tests exercise `with_system_prompt()` and
//! friends directly; they cannot see the binary's construction path —
//! the #20 bug class, where the startup provider never attached
//! SYSTEM.md and every unit test still passed. Here the request is
//! read off the wire and asserted, alongside stdout, exit code, and
//! session files. Plain pipes and a localhost TcpListener; the only
//! dep is serde_json (already a dev-dep since #18).

use std::collections::VecDeque;
use std::io::Read;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// The turn's scripted text, concatenated from two deltas.
const REPLY: &str = "Hello";

// ── the fake API ──────────────────────────────────────────────────

/// One request, read off the wire: path, auth header, parsed body.
struct Recorded {
    path: String,
    authorization: Option<String>,
    body: Value,
}

/// What the server answers with, per request, in order.
#[derive(Clone)]
enum Script {
    /// HTTP 200 carrying an SSE body (the caller builds it with
    /// [`sse`]).
    Sse(String),
    /// A bare status line with an empty body — the failure path.
    Status(u16),
}

/// A recording fake OpenAI-compatible endpoint. Binds 127.0.0.1:0,
/// answers each request with the next scripted response (panic on
/// exhaustion — a test issuing more requests than it scripted is a
/// bug in the test), and records every request body for wire-level
/// assertions.
struct FakeApi {
    base_url: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
    script: Arc<Mutex<VecDeque<Script>>>,
}

impl FakeApi {
    fn start(script: Vec<Script>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests: Arc<Mutex<Vec<Recorded>>> = Arc::default();
        let script: Arc<Mutex<VecDeque<Script>>> =
            Arc::new(Mutex::new(script.into_iter().collect()));
        let listener_requests = Arc::clone(&requests);
        let listener_script = Arc::clone(&script);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                answer(stream, &listener_requests, &listener_script);
            }
        });
        Self {
            base_url: format!("http://127.0.0.1:{port}/v1"),
            requests,
            script,
        }
    }

    fn url(&self) -> &str {
        &self.base_url
    }

    fn requests(&self) -> std::sync::MutexGuard<'_, Vec<Recorded>> {
        self.requests.lock().unwrap()
    }

    /// How many scripted responses remain — a test can assert it hit
    /// zero (every request was answered by the script, none by panic).
    #[allow(dead_code)]
    fn script_remaining(&self) -> usize {
        self.script.lock().unwrap().len()
    }
}

/// Read one HTTP request off the connection, record it, answer with
/// the next scripted response, close. `Connection: close` keeps the
/// protocol at request/response — the client opens a fresh
/// connection for its next request.
fn answer(
    mut stream: std::net::TcpStream,
    requests: &Mutex<Vec<Recorded>>,
    script: &Mutex<VecDeque<Script>>,
) {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
        match stream.read(&mut chunk) {
            Ok(0) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => return,
        }
    };
    let headers = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = headers.lines();
    let request_line = lines.next().unwrap_or_default().to_string();
    let mut content_length = 0usize;
    let mut authorization = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        if name == "content-length" {
            content_length = value.trim().parse().unwrap_or(0);
        }
        if name == "authorization" {
            authorization = Some(value.trim().to_string());
        }
    }
    while buf.len() < header_end + content_length {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    let body = serde_json::from_slice(&buf[header_end..]).unwrap_or(Value::Null);
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_string();
    requests.lock().unwrap().push(Recorded {
        path,
        authorization,
        body,
    });
    let response = match script.lock().unwrap().pop_front() {
        Some(Script::Sse(sse)) => format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            sse.len(),
            sse
        ),
        Some(Script::Status(code)) => {
            format!("HTTP/1.1 {code} nope\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        }
        None => panic!("script exhausted — more requests than scripted"),
    };
    let _ = std::io::Write::write_all(&mut stream, response.as_bytes());
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// An SSE body: one `data:` frame per chunk, then `[DONE]`.
fn sse(chunks: &[Value]) -> String {
    let mut body = String::new();
    for chunk in chunks {
        body.push_str(&format!("data: {chunk}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    body
}

fn token_chunk(text: &str) -> Value {
    json!({"choices": [{"delta": {"content": text}}]})
}

fn usage_chunk(prompt: u64, completion: u64) -> Value {
    json!({
        "choices": [],
        "usage": {
            "prompt_tokens": prompt,
            "completion_tokens": completion,
            "total_tokens": prompt + completion,
        },
    })
}

// ── the environment ───────────────────────────────────────────────

static NEXT: AtomicU64 = AtomicU64::new(0);

/// An isolated XDG root: `config/hi/` for models.json, SYSTEM.md,
/// settings.json; `data/hi/sessions/` for session files. Nothing the
/// tests write can touch the user's real config or sessions.
struct Env {
    root: PathBuf,
}

impl Env {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "hi-wire-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("config/hi")).unwrap();
        std::fs::create_dir_all(root.join("data")).unwrap();
        Self { root }
    }

    fn config_dir(&self) -> PathBuf {
        self.root.join("config/hi")
    }

    fn sessions_dir(&self) -> PathBuf {
        self.root.join("data/hi/sessions")
    }

    /// A fresh project directory to run `hi` in.
    fn project(&self, name: &str) -> PathBuf {
        let dir = self.root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// models.json pointing `fake` at the running fake API.
    fn write_models_json(&self, url: &str, model: Value) {
        let config = json!({
            "providers": {
                "fake": {
                    "baseUrl": url,
                    "apiKey": "test-key",
                    "models": [model],
                },
            },
        });
        std::fs::write(self.config_dir().join("models.json"), config.to_string()).unwrap();
    }

    fn write_settings_json(&self, settings: Value) {
        std::fs::write(
            self.config_dir().join("settings.json"),
            settings.to_string(),
        )
        .unwrap();
    }
}

/// Run the real binary in `project` with the env's XDG dirs. Both
/// streams on pipes; a deadline loop kills a child that wedges (the
/// test fails with a name, not a CI timeout).
fn run(project: &Path, env: &Env, args: &[&str]) -> (i32, String, String) {
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_hi"))
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env("XDG_CONFIG_HOME", env.root.join("config"))
        .env("XDG_DATA_HOME", env.root.join("data"))
        .current_dir(project)
        .spawn()
        .unwrap();

    let stdout = std::thread::spawn({
        let mut pipe = child.stdout.take().unwrap();
        move || {
            let mut text = String::new();
            let _ = std::io::Read::read_to_string(&mut pipe, &mut text);
            text
        }
    });
    let stderr = std::thread::spawn({
        let mut pipe = child.stderr.take().unwrap();
        move || {
            let mut text = String::new();
            let _ = std::io::Read::read_to_string(&mut pipe, &mut text);
            text
        }
    });

    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        match child.try_wait().unwrap() {
            Some(status) => break status,
            None => {
                if Instant::now() > deadline {
                    let _ = child.kill();
                    panic!("hi wedged until the deadline (args: {args:?})");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    };

    (
        status.code().unwrap_or(-1),
        stdout.join().unwrap(),
        stderr.join().unwrap(),
    )
}

fn plain_model() -> Value {
    json!({"id": "fake-mini"})
}

/// The default scripted turn: two deltas that join to [`REPLY`].
fn reply_script() -> Vec<Script> {
    vec![Script::Sse(sse(&[token_chunk("Hel"), token_chunk("lo")]))]
}

// ── the wire tests ────────────────────────────────────────────────

/// Every session transcript on disk: `sessions/<project-key>/<ts>_<id>.jsonl`
/// — sessions are stored per-project, one subdirectory each.
fn session_files(env: &Env) -> Vec<PathBuf> {
    let sessions = env.sessions_dir();
    let mut files = Vec::new();
    for project in std::fs::read_dir(&sessions)
        .unwrap()
        .map(|entry| entry.unwrap().path())
    {
        for file in std::fs::read_dir(project)
            .unwrap()
            .map(|entry| entry.unwrap().path())
        {
            files.push(file);
        }
    }
    files.sort();
    files
}

#[test]
fn every_request_carries_the_system_prompt_chain() {
    let env = Env::new("chain");
    // Both levels, both shapes: user config dir, project `.hi/`.
    std::fs::write(env.config_dir().join("SYSTEM.md"), "USER BASE\n").unwrap();
    std::fs::write(env.config_dir().join("APPEND_SYSTEM.md"), "USER APPEND\n").unwrap();
    let project = env.project("chain-proj");
    std::fs::create_dir_all(project.join(".hi")).unwrap();
    std::fs::write(project.join(".hi/SYSTEM.md"), "PROJECT BASE\n").unwrap();
    std::fs::write(project.join(".hi/APPEND_SYSTEM.md"), "PROJECT APPEND\n").unwrap();

    let api = FakeApi::start(reply_script());
    env.write_models_json(api.url(), plain_model());

    // `--tools none` keeps the tool-notes section out of the prompt,
    // so the chain assertion is exact; the default-bundle variant is
    // covered in json_mode_streams_the_wire_events.
    let (code, stdout, stderr) = run(
        &project,
        &env,
        &[
            "-p",
            "--tools",
            "none",
            "--provider",
            "fake",
            "--model",
            "fake-mini",
            "hi",
        ],
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(stdout, format!("{REPLY}\n"));

    let requests = api.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].path.ends_with("/chat/completions"));
    // The credential travels.
    assert_eq!(
        requests[0].authorization.as_deref(),
        Some("Bearer test-key")
    );
    // The #20 guard: the system prompt chain rides every request —
    // project SYSTEM.md replaced the user's, both appendices follow,
    // blank-line separated, ahead of the user message.
    let messages = requests[0].body["messages"].as_array().unwrap();
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(
        messages[0]["content"],
        "PROJECT BASE\n\nUSER APPEND\n\nPROJECT APPEND"
    );
    assert_eq!(messages[1], json!({"role": "user", "content": "hi"}),);
}

#[test]
fn thinking_kwargs_follow_the_models_level_map() {
    let env = Env::new("thinking");
    let api = FakeApi::start([reply_script(), reply_script()].concat());
    // The map entry is what turns a settings level into wire kwargs.
    env.write_models_json(
        api.url(),
        json!({
            "id": "fake-mini",
            "thinkingLevelMap": {"high": {"reasoning_effort": "high"}},
        }),
    );

    // With settings.thinking = high: the mapped kwargs ride the body.
    env.write_settings_json(json!({"thinking": "high"}));
    let project = env.project("thinking-proj");
    let (code, _, stderr) = run(
        &project,
        &env,
        &["-p", "--provider", "fake", "--model", "fake-mini", "hi"],
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(
        api.requests()[0].body["reasoning_effort"],
        json!("high"),
        "settings thinking must reach the wire through the model's map"
    );

    // Without a configured level: no kwargs, map or not.
    env.write_settings_json(json!({}));
    let (code, _, stderr) = run(
        &project,
        &env,
        &["-p", "--provider", "fake", "--model", "fake-mini", "hi"],
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    let body = &api.requests()[1].body;
    assert!(
        body.get("reasoning_effort").is_none(),
        "no configured level, no kwargs: {body}"
    );
}

#[test]
fn continue_restores_the_sessions_recorded_thinking_level() {
    // AD-0045 ruling 1: the session header's level outranks the
    // settings default on resume — a session recorded at high stays
    // at high even after settings moved on.
    let env = Env::new("think-resume");
    let api = FakeApi::start([reply_script(), reply_script()].concat());
    env.write_models_json(
        api.url(),
        json!({
            "id": "fake-mini",
            "thinkingLevelMap": {
                "high": {"reasoning_effort": "high"},
                "low": {"reasoning_effort": "low"},
            },
        }),
    );
    let project = env.project("think-resume-proj");

    // Session one runs at high: the header records it.
    env.write_settings_json(json!({"thinking": "high"}));
    let (code, _, stderr) = run(
        &project,
        &env,
        &["-p", "--provider", "fake", "--model", "fake-mini", "first"],
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(api.requests()[0].body["reasoning_effort"], json!("high"));

    // Settings move to low; --continue must put the session back at
    // its recorded high, not the new default.
    env.write_settings_json(json!({"thinking": "low"}));
    let (code, _, stderr) = run(
        &project,
        &env,
        &[
            "--continue",
            "-p",
            "--provider",
            "fake",
            "--model",
            "fake-mini",
            "second",
        ],
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(
        api.requests()[1].body["reasoning_effort"],
        json!("high"),
        "the recorded level must outrank the settings default on resume"
    );
}

#[test]
fn continue_appends_and_no_session_writes_nothing() {
    let env = Env::new("sessions");
    let project = env.project("sessions-proj");
    let api = FakeApi::start([reply_script(), reply_script(), reply_script()].concat());
    env.write_models_json(api.url(), plain_model());

    let (code, _, stderr) = run(
        &project,
        &env,
        &["-p", "--provider", "fake", "--model", "fake-mini", "first"],
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    let files = session_files(&env);
    assert_eq!(files.len(), 1, "one turn, one session file");
    let path = files[0].clone();
    let lines_after_first = std::fs::read_to_string(&path).unwrap().lines().count();

    // `--continue` resumes this directory's session: same file, more
    // transcript.
    let (code, _, stderr) = run(
        &project,
        &env,
        &[
            "--continue",
            "-p",
            "--provider",
            "fake",
            "--model",
            "fake-mini",
            "second",
        ],
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(
        session_files(&env),
        vec![path.clone()],
        "same file, no fork"
    );
    let lines_after_second = std::fs::read_to_string(&path).unwrap().lines().count();
    assert!(lines_after_second > lines_after_first);

    // `--no-session` writes nothing at all.
    let throwaway = env.project("throwaway-proj");
    let (code, _, stderr) = run(
        &throwaway,
        &env,
        &[
            "--no-session",
            "-p",
            "--provider",
            "fake",
            "--model",
            "fake-mini",
            "third",
        ],
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(
        session_files(&env),
        vec![path.clone()],
        "--no-session touched nothing"
    );
}

#[test]
fn a_failed_turn_fails_with_stdout_empty() {
    let env = Env::new("failure");
    let api = FakeApi::start(vec![Script::Status(500)]);
    env.write_models_json(api.url(), plain_model());
    let project = env.project("failure-proj");

    let (code, stdout, stderr) = run(
        &project,
        &env,
        &["-p", "--provider", "fake", "--model", "fake-mini", "hi"],
    );
    assert_eq!(code, 1);
    assert_eq!(stdout, "", "a failed turn leaves stdout empty");
    assert!(
        stderr.starts_with("hi: "),
        "the reason names itself on stderr: {stderr:?}"
    );
}

#[test]
fn json_mode_streams_the_wire_events() {
    let env = Env::new("json");
    let api = FakeApi::start(vec![Script::Sse(sse(&[
        token_chunk("Hel"),
        token_chunk("lo"),
        usage_chunk(10, 5),
    ]))]);
    env.write_models_json(api.url(), plain_model());
    let project = env.project("json-proj");

    let (code, stdout, stderr) = run(
        &project,
        &env,
        &[
            "--mode",
            "json",
            "--provider",
            "fake",
            "--model",
            "fake-mini",
            "hi",
        ],
    );
    assert_eq!(code, 0, "stderr: {stderr}");

    let records: Vec<Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let types: Vec<&str> = records
        .iter()
        .map(|record| record["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        types,
        vec![
            "session", "message", "token", "token", "usage", "message", "done"
        ],
        "the wire's ordering becomes the record ordering"
    );
    // The header names the resolved selection, straight from the wire.
    assert_eq!(records[0]["provider"], "fake");
    assert_eq!(records[0]["model"], "fake-mini");
    // Default bundle: the tool notes append to the prompt chain —
    // the exact-chain variant lives in the --tools none test.
    let requests = api.requests();
    assert_eq!(requests[0].body["messages"][0]["role"], "system");
    assert!(
        requests[0].body["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("# Tool notes"),
        "the default bundle appends its tool notes"
    );
    let deltas: String = records
        .iter()
        .filter_map(|record| record["delta"].as_str())
        .collect();
    assert_eq!(deltas, REPLY);
    assert_eq!(records[4]["usage"]["promptTokens"], 10);
    assert_eq!(records[4]["usage"]["completionTokens"], 5);
    assert_eq!(records.last().unwrap()["text"], REPLY);

    // Wire-level: json mode still asks for a stream with usage —
    // the guard from the tool-notes assertions above still holds.
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].body["stream"], json!(true));
    assert_eq!(
        requests[0].body["stream_options"]["include_usage"],
        json!(true)
    );
}
