//! A real pseudo-terminal drives the real binary — the parts of the
//! TUI that do not survive a mock: the event loop's 50ms poll clock,
//! frame rendering into an actual terminal, and teardown ordering
//! (raw mode disable, kitty pop, viewport restore) must all run for
//! the process to exit at all. The kitty one-shot handshake gets no
//! flags over a plain pty; booting anyway is the fail-open contract
//! (AD-0010) and this test would hang if it regressed.
//!
//! `portable-pty` is a dev-dependency: the shipped `hi` never links
//! it (AD-0004 stays about runtime deps).

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The boot handshake, query → reply. crossterm blocks on each of
/// these until the terminal answers; a real emulator would. (The
/// kitty query gets the zero-flags reply — the fail-open path.)
const QUERIES: &[(&[u8], &[u8])] = &[
    (b"\x1b[6n", b"\x1b[1;1R"),    // cursor position (DSR)
    (b"\x1b[c", b"\x1b[?1;2c"),    // primary device attributes
    (b"\x1b[>c", b"\x1b[>0;0;0c"), // secondary device attributes
    (b"\x1b[?u", b"\x1b[?0u"),     // kitty keyboard query
];

/// Drop CSI/OSC/escape sequences, keeping printable frame text.
fn strip_escapes(raw: &[u8]) -> String {
    let mut out = Vec::new();
    let mut rest = raw;
    while let Some(at) = rest.iter().position(|b| *b == 0x1b) {
        out.extend_from_slice(&rest[..at]);
        rest = &rest[at + 1..];
        rest = match rest.first() {
            Some(b'[') => {
                let end = rest
                    .iter()
                    .skip(1)
                    .position(|b| (0x40..=0x7e).contains(b))
                    .map_or(rest.len(), |at| at + 2);
                &rest[end.min(rest.len())..]
            }
            Some(b']') => {
                let end = rest
                    .iter()
                    .position(|b| *b == 0x07)
                    .map_or(rest.len(), |at| at + 1);
                &rest[end.min(rest.len())..]
            }
            Some(_) => &rest[1..],
            None => rest,
        };
    }
    out.extend_from_slice(rest);
    String::from_utf8_lossy(&out).into_owned()
}

struct Tui {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    /// Escape-stripped frames: ratatui emits each changed cell with a
    /// cursor move between neighbors, so a rendered word is never
    /// contiguous in the raw stream. Needles match this instead.
    plain: Arc<Mutex<String>>,
    /// Shared with the reader thread, which also writes handshake
    /// replies; `take_writer` can only be called once.
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
}

impl Drop for Tui {
    // A panicking assert unwinds through here: without the kill, the
    // spawned `hi` outlives the test as an orphan burning a core (the
    // incident that earned this guard).
    fn drop(&mut self) {
        let _ = self.child.clone_killer().kill();
        let _ = self.child.wait();
    }
}

impl Tui {
    /// Spawn `hi` inside a fresh 80×24 pty in an empty temp cwd (no
    /// config, no sessions), pumping its output on a reader thread
    /// that answers the handshake queries every time they appear.
    fn spawn(args: &[&str]) -> Self {
        // Unique per spawn: the tests run in parallel and must not
        // share (or delete) each other's cwd.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let cwd = std::env::temp_dir().join(format!(
            "hi-pty-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&cwd);
        std::fs::create_dir_all(&cwd).unwrap();

        let pty = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_hi"));
        cmd.args(args);
        cmd.cwd(&cwd);
        let child = pty.slave.spawn_command(cmd).unwrap();
        // Belt and suspenders: some platforms do not honor the size
        // given at openpty; crossterm renders nothing into a 0x0 pty.
        pty.master
            .resize(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        // Dropping the slave closes the child's end; the master reader
        // then sees EOF once the process exits.
        drop(pty.slave);

        let mut reader = pty.master.try_clone_reader().unwrap();
        let writer: Arc<Mutex<Box<dyn Write + Send>>> =
            Arc::new(Mutex::new(pty.master.take_writer().unwrap()));
        let plain: Arc<Mutex<String>> = Arc::default();
        let lines = Arc::clone(&plain);
        let reply_writer = Arc::clone(&writer);
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            let mut pending: Vec<u8> = Vec::new();
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        // Answer every occurrence: crossterm re-queries
                        // the cursor position on resize, and one missed
                        // reply is a 2s stall then a boot error.
                        pending.extend_from_slice(&buf[..n]);
                        for (query, reply) in QUERIES {
                            while let Some(at) =
                                pending.windows(query.len()).position(|w| w == *query)
                            {
                                pending.drain(..at + query.len());
                                let _ = reply_writer.lock().unwrap().write_all(reply);
                                let _ = reply_writer.lock().unwrap().flush();
                            }
                        }
                        if pending.len() > 8192 {
                            pending.clear();
                        }
                        let mut plain = lines.lock().unwrap();
                        let from = plain.len().saturating_sub(4096);
                        plain.replace_range(..from, "");
                        plain.push_str(&strip_escapes(&buf[..n]));
                    }
                }
            }
        });
        Self {
            master: pty.master,
            child,
            plain,
            writer,
        }
    }

    fn write(&self, keys: &str) {
        let mut writer = self.writer.lock().unwrap();
        writer.write_all(keys.as_bytes()).unwrap();
        writer.flush().unwrap();
    }

    /// The rendered frame — after escape stripping — contains the
    /// needle, or the test panics naming what never appeared.
    fn sees(&self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.plain.lock().unwrap().contains(needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("never saw {needle:?}");
    }

    /// Type `/exit` + Enter and poll for a clean exit — the teardown
    /// ordering is exactly what this wait covers: any hang (kitty pop,
    /// viewport restore, a poll loop that never quits) fails the test.
    fn exits_cleanly(mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait().unwrap() {
                Some(status) => {
                    return if status.success() {
                        true
                    } else {
                        panic!("/exit exited with {status:?}");
                    };
                }
                None if Instant::now() >= deadline => {
                    let _ = self.child.clone_killer().kill();
                    let plain = self.plain.lock().unwrap();
                    panic!("never exited; screen: {plain}");
                }
                None => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    }
}

#[test]
fn the_tui_boots_takes_keys_streams_and_exits_cleanly() {
    let tui = Tui::spawn(&["--mock"]);
    // Wait for the first rendered frame (the input border) before
    // typing: until raw mode is on, the pty line discipline echoes
    // keystrokes and rewrites \r to \n (ICRNL), so keys sent during
    // boot are lost to the cooked-mode tty, not to hi.
    tui.sees("\u{2500}", Duration::from_secs(20));
    // Typed characters render live in the composer (the transcript
    // itself over-flushes into scrollback by design — AD-0016 — so it
    // is never a needle the master stream can see).
    // Space-free needles: a space cell is byte-identical to the
    // empty background cell, so ratatui's diff never emits it — a
    // spaced needle can never appear in the stream even though the
    // composer shows it.
    tui.write("hellomock");
    tui.sees("hellomock", Duration::from_secs(20));
    // Enter submits; the mock streams its reply live above the input.
    tui.write("\r");
    tui.sees("streaming", Duration::from_secs(20));
    // The finalize window (turn end, transcript flush) drops bytes
    // written into the pty mid-drain; let it settle before typing.
    std::thread::sleep(Duration::from_millis(500));
    tui.write("/exit\r");
    assert!(
        tui.exits_cleanly(Duration::from_secs(20)),
        "/exit did not exit cleanly"
    );
}

#[test]
fn a_resize_mid_session_does_not_break_the_loop() {
    let tui = Tui::spawn(&["--mock"]);
    tui.sees("\u{2500}", Duration::from_secs(20));
    // Event::Resize carries no payload hi listens for; the redraw is
    // the per-frame loop's job. Surviving it is the assertion.
    tui.master
        .resize(PtySize {
            rows: 12,
            cols: 60,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    // The resize triggers ratatui's inline re-anchor, which re-queries
    // the cursor position; keys typed while that reply is in flight
    // are swallowed by the wait. Let it settle, then type.
    std::thread::sleep(Duration::from_millis(300));
    tui.write("stillhere");
    tui.sees("stillhere", Duration::from_secs(20));
    tui.write("\r");
    tui.sees("streaming", Duration::from_secs(20));
    // The finalize window (turn end, transcript flush) drops bytes
    // written into the pty mid-drain; let it settle before typing.
    std::thread::sleep(Duration::from_millis(500));
    tui.write("/exit\r");
    assert!(
        tui.exits_cleanly(Duration::from_secs(20)),
        "/exit did not exit cleanly"
    );
}
