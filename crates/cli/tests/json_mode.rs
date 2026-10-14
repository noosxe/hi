//! `--mode json`: the real binary streams JSONL records (#18, AD-0038).
//!
//! Plain pipes, no pty — headless runs never touch a terminal. Each
//! run gets its own `XDG_DATA_HOME` so session files land in a temp
//! tree and the session header is genuinely exercised.

use std::process::Command;

const REPLY: &str = "Hello! I am the mock provider — streaming works, no API key required.";

fn run_json(args: &[&str]) -> std::process::Output {
    let root = std::env::temp_dir().join(format!("hi-json-{}-{}", args.len(), std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_hi"))
        .args(args)
        .env("XDG_DATA_HOME", &root)
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
