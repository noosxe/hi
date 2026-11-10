//! `--mode json`: the real binary streams JSONL records (#18, AD-0038).
//!
//! Plain pipes, no pty — headless runs never touch a terminal. Each
//! run gets its own XDG root so session files and config land in a
//! temp tree (never the real home)
//! tree and the session header is genuinely exercised.

use std::process::Command;

const REPLY: &str = "Hello! I am the mock provider — streaming works, no API key required.";

fn run_json(args: &[&str]) -> std::process::Output {
    let root = std::env::temp_dir().join(format!("hi-json-{}-{}", args.len(), std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_hi"))
        .args(args)
        // Never the runner's stdin: print mode reads it to EOF.
        .stdin(std::process::Stdio::null())
        .env("XDG_DATA_HOME", &root)
        // Config too: the maintainer's real settings.json (taskTurns
        // on, a thinking level) must not leak into the run's behavior.
        .env("XDG_CONFIG_HOME", &root)
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&root);
    out
}

/// Every stdout line must be one JSON record — strict JSONL framing.
fn records(stdout: &str) -> Vec<serde_json::Value> {
    stdout
        .lines()
        .map(|line| serde_json::from_str(line).expect("every json-mode line parses"))
        .collect()
}

#[test]
fn json_mode_opens_with_a_session_header_and_ends_with_done() {
    let out = run_json(&["--mock", "--mode", "json", "say something"]);
    assert!(out.status.success());
    assert!(out.stderr.is_empty(), "diagnostics belong on stderr");

    let records = records(&String::from_utf8(out.stdout).unwrap());
    let first = records.first().unwrap();
    assert_eq!(first["type"], "session");
    assert!(first["id"].is_string());
    let cwd = std::env::current_dir()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(first["cwd"], cwd);

    assert!(records.iter().any(|r| r["type"] == "token"));
    assert!(records.iter().any(|r| r["type"] == "message"));
    let last = records.last().unwrap();
    assert_eq!(last["type"], "done");
    assert_eq!(last["text"], REPLY);
}

#[test]
fn json_mode_without_a_session_starts_at_the_user_message() {
    let out = run_json(&["--mock", "--mode", "json", "--no-session", "hi"]);
    assert!(out.status.success());

    let records = records(&String::from_utf8(out.stdout).unwrap());
    assert_eq!(records.first().unwrap()["type"], "message");
    assert_eq!(records.last().unwrap()["type"], "done");
}

#[test]
fn text_print_mode_still_prints_only_the_reply() {
    let out = run_json(&["--mock", "-p", "hi"]);
    assert!(out.status.success());

    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(stdout, format!("{REPLY}\n"));
}

/// #200: the reader going away (`hi --mode json … | head`) used to
/// panic `println!` (exit 101) before the turn was recorded.
#[test]
fn a_closed_stdout_pipe_does_not_panic_or_lose_the_session() {
    let root = std::env::temp_dir().join(format!("hi-json-pipe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_hi"))
        .args(["--mock", "--mode", "json", "say something"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env("XDG_DATA_HOME", &root)
        .env("XDG_CONFIG_HOME", &root)
        .spawn()
        .unwrap();
    drop(child.stdout.take()); // the reader leaves before any output
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    assert!(!stderr.contains("panicked"), "stderr: {stderr}");
    // The turn still ran to the end and was recorded.
    let mut recorded = String::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "jsonl") {
                recorded.push_str(&std::fs::read_to_string(path).unwrap());
            }
        }
    }
    assert!(recorded.contains(REPLY), "session file: {recorded}");
    let _ = std::fs::remove_dir_all(&root);
}

/// #200 review: a failed write that is not a closed pipe loses output,
/// so it must not look like success — but the turn still finishes and
/// is recorded. `/dev/full` fails every write with ENOSPC (Linux).
#[cfg(target_os = "linux")]
#[test]
fn a_full_disk_on_stdout_fails_loudly_but_records_the_turn() {
    let root = std::env::temp_dir().join(format!("hi-json-full-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let full = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_hi"))
        .args(["--mock", "--mode", "json", "say something"])
        .stdin(std::process::Stdio::null())
        .stdout(full)
        .stderr(std::process::Stdio::piped())
        .env("XDG_DATA_HOME", &root)
        .env("XDG_CONFIG_HOME", &root)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(
        stderr.contains("hi: cannot write output:"),
        "stderr: {stderr}"
    );
    assert_eq!(stderr.matches("cannot write output").count(), 1, "{stderr}");
    assert!(!stderr.contains("panicked"), "stderr: {stderr}");
    let mut recorded = String::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "jsonl") {
                recorded.push_str(&std::fs::read_to_string(path).unwrap());
            }
        }
    }
    assert!(recorded.contains(REPLY), "session file: {recorded}");
    let _ = std::fs::remove_dir_all(&root);
}
