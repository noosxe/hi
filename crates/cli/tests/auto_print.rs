//! Auto-print on a redirected stream (#19, ruling a): when stdin or
//! stdout is not a terminal and no mode is selected, `hi` runs text
//! print mode instead of opening a TUI that cannot work there. Plain
//! pipes here — no pty — because these tests assert the pipe contract
//! itself: exit code, stdout framing, empty stderr.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

/// The mock's one reply (mock.rs) — print mode prints it verbatim.
const REPLY: &str = "Hello! I am the mock provider — streaming works, no API key required.";

static NEXT: AtomicU64 = AtomicU64::new(0);

/// Spawn the real binary with both streams on pipes. XDG points at a
/// throwaway dir (the mock needs neither config nor models.json), and
/// stdin is a pipe that closes after `stdin_text` — EOF, exactly like
/// `git diff | hi …`. A 30 s deadline kills a child that wrongly tries
/// to open the TUI on a pipe instead of hanging the test forever.
fn run_pipe(stdin_text: Option<&str>, args: &[&str]) -> (i32, String, String) {
    let xdg = std::env::temp_dir().join(format!(
        "hi-auto-print-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&xdg);
    std::fs::create_dir_all(&xdg).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_hi"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("XDG_CONFIG_HOME", &xdg)
        .env("XDG_DATA_HOME", &xdg)
        .spawn()
        .unwrap();
    if let Some(text) = stdin_text {
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap();
    }
    drop(child.stdin.take()); // close the pipe → the child sees EOF

    // Drain both streams on threads: a chatty child must not block on
    // a full pipe buffer while we wait for its exit.
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

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let status = loop {
        match child.try_wait().unwrap() {
            Some(status) => break status,
            None => {
                if std::time::Instant::now() > deadline {
                    let _ = child.kill();
                    panic!("hi opened the TUI on a pipe and hung until the deadline");
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    };

    (
        status.code().unwrap_or(-1),
        stdout.join().unwrap(),
        stderr.join().unwrap(),
    )
}

#[test]
fn piped_stdin_becomes_the_prompt_without_flags() {
    // `echo "review this" | hi --mock` — no mode flag anywhere.
    let (code, stdout, stderr) = run_pipe(Some("review this"), &["--mock"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(stdout, format!("{REPLY}\n"));
    assert_eq!(stderr, "");
}

#[test]
fn piped_stdin_plus_positional_prompt_runs_print_mode() {
    // `git diff | hi "review this"` — piped stdin and the positional
    // PROMPT, still no flags. Auto-print, not the [PROMPT] guard.
    let (code, stdout, stderr) = run_pipe(None, &["--mock", "say hi"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(stdout, format!("{REPLY}\n"));
    assert_eq!(stderr, "");
}

#[test]
fn empty_pipe_without_a_prompt_names_the_problem() {
    // Ruling (a): a redirected stream with nothing to say is still
    // print mode — and print mode's honest "no prompt" failure. The
    // TUI must not be attempted on a pipe either way.
    let (code, stdout, stderr) = run_pipe(None, &["--mock"]);
    assert_eq!(code, 1);
    assert_eq!(stdout, "");
    assert_eq!(
        stderr,
        "hi: no prompt: pass one as an argument or pipe it into stdin\n"
    );
}
