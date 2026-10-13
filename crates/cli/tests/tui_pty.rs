//! A real pseudo-terminal drives the real binary, and `avt` — the
//! terminal emulator asciinema replays sessions with — renders what a
//! real terminal would show. The parts of the TUI that do not survive
//! a mock: the event loop's 50ms poll clock, frame rendering into an
//! actual terminal (diff-based cell updates, scrolling regions,
//! background-color-erase), and teardown ordering (raw mode disable,
//! kitty pop, viewport restore). The kitty one-shot handshake gets no
//! flags over a plain pty; booting anyway is the fail-open contract
//! (AD-0010) and this test would hang if it regressed.
//!
//! The emulator is the assertion surface: needles match the rendered
//! grid (never the raw byte stream — ratatui emits each changed cell
//! with a cursor move between neighbors, so rendered words are never
//! contiguous in the bytes), rows pushed off the top of the screen are
//! per-cell backgrounds expose tint leaks that plain text cannot see.
//! (One limit is recorded honestly: avt models a flush by extending
//! its buffer — an LF at the bottom appends rather than pushing
//! scrollback — so the pushed rows' *content* is not assertable here;
//! what the rig asserts is that the live area gave the rows up.)
//! Fixed sleeps are banned: every wait is a predicate over rendered
//! frames, sampled until it holds — and holds stable.
//!
//! `portable-pty` and `avt` are dev-dependencies: the shipped `hi`
//! never links them (AD-0004 stays about runtime deps).

use avt::Vt;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const COLS: u16 = 80;
const ROWS: u16 = 24;
const POLL: Duration = Duration::from_millis(25);
const STABLE: Duration = Duration::from_millis(150);

/// The boot handshake, query → reply. crossterm blocks on each of
/// these until the terminal answers; a real emulator would. (The
/// kitty query gets the zero-flags reply — the fail-open path.)
const QUERIES: &[(&[u8], &[u8])] = &[
    (b"\x1b[6n", b"\x1b[1;1R"),    // cursor position (DSR)
    (b"\x1b[c", b"\x1b[?1;2c"),    // primary device attributes
    (b"\x1b[>c", b"\x1b[>0;0;0c"), // secondary device attributes
    (b"\x1b[?u", b"\x1b[?0u"),     // kitty keyboard query
];

struct Tui {
    master: Box<dyn MasterPty + Send>,
    journal: Arc<Mutex<String>>,
    child: Arc<Mutex<Box<dyn portable_pty::Child + Send + Sync>>>,
    emu: Arc<Mutex<Vt>>,
    /// Shared with the reader thread, which also writes handshake
    /// replies; `take_writer` can only be called once.
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
}

impl Drop for Tui {
    // A panicking assert unwinds through here: without the kill, the
    // spawned `hi` outlives the test as an orphan burning a core (the
    // incident that earned this guard).
    fn drop(&mut self) {
        let mut child = self.child.lock().unwrap();
        let _ = child.clone_killer().kill();
        let _ = child.wait();
    }
}

impl Tui {
    /// Spawn `hi` inside a fresh pty in an empty temp cwd (no config,
    /// no sessions), pumping its output through the emulator on a
    /// reader thread that answers the handshake queries every time
    /// they appear (crossterm re-queries the cursor position on
    /// resize; one missed reply is a 2s stall then a boot error).
    fn spawn(args: &[&str]) -> Self {
        Self::spawn_sized(args, ROWS, COLS)
    }

    fn spawn_sized(args: &[&str], rows: u16, cols: u16) -> Self {
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
                rows,
                cols,
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
                rows,
                cols,
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
        let emu = Arc::new(Mutex::new(
            Vt::builder()
                .size(cols as usize, rows as usize)
                .scrollback_limit(10_000)
                .build(),
        ));
        let emu_reader = Arc::clone(&emu);
        let reply_writer = Arc::clone(&writer);
        let journal: Arc<Mutex<String>> = Arc::default();
        let journal_reader = Arc::clone(&journal);
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            // Bytes that arrived split across reads mid-UTF-8-character.
            let mut carry: Vec<u8> = Vec::new();
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        pending_replies(&mut carry, &buf[..n], &reply_writer);
                        let text = complete_utf8(&mut carry);
                        {
                            let mut j = journal_reader.lock().unwrap();
                            j.push_str(&format!(
                                "== chunk {}B carry {}B fed {}B\n",
                                n,
                                carry.len(),
                                text.len()
                            ));
                            j.push_str(&String::from_utf8_lossy(&buf[..n]));
                            j.push('\n');
                        }
                        if text.is_empty() {
                            continue;
                        }
                        emu_reader.lock().unwrap().feed_str(&text);
                    }
                }
            }
        });
        Self {
            master: pty.master,
            journal,
            child: Arc::new(Mutex::new(child)),
            emu,
            writer,
        }
    }

    fn write(&self, keys: &str) {
        let mut writer = self.writer.lock().unwrap();
        writer.write_all(keys.as_bytes()).unwrap();
        writer.flush().unwrap();
    }

    /// Type, then wait until hi's composer echoes the text — raw mode
    /// is on and the event loop consumed the bytes. In cooked mode the
    /// pty line discipline echoes keystrokes itself and rewrites `\r`
    /// to `\n` (ICRNL), so keys sent before this gate are lost to the
    /// tty, not to hi.
    fn type_keys(&mut self, keys: &str) {
        self.write(keys);
        self.sees(keys);
    }

    /// The rendered frame: the emulator's grid, one trimmed row per
    /// terminal row. Needles match this, never the byte stream.
    fn frame(&self) -> String {
        let emu = self.emu.lock().unwrap();
        emu.text()
            .iter()
            .map(|row| row.trim_end())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn scrollback(&self) -> Vec<String> {
        let emu = self.emu.lock().unwrap();
        let rows = emu.size().1;
        let all: Vec<String> = emu.lines().map(|line| line.text()).collect();
        all[..all.len().saturating_sub(rows)].to_vec()
    }

    /// Poll rendered frames until `pred` holds and keeps holding — a
    /// settled screen, not one moment inside a streaming redraw. On
    /// timeout the panic carries the last frame and the scrollback:
    /// the diagnostics are the test's output, not a side channel.
    fn until(&self, what: &str, timeout: Duration, mut pred: impl FnMut(&str) -> bool) {
        let deadline = Instant::now() + timeout;
        let mut stable_since: Option<Instant> = None;
        let mut samples = 0usize;
        let mut last_verdict;
        loop {
            samples += 1;
            last_verdict = pred(&self.frame());
            if last_verdict {
                match stable_since {
                    None => stable_since = Some(Instant::now()),
                    Some(then) if then.elapsed() >= STABLE => return,
                    Some(_) => {}
                }
            } else {
                stable_since = None;
            }
            if Instant::now() >= deadline {
                let status = self.child.lock().unwrap().try_wait().unwrap();
                let _ = std::fs::write("/tmp/hi-pty-journal.log", &*self.journal.lock().unwrap());
                panic!(
                    "never settled: {what} (child: {status:?}, samples: {samples}, last verdict: {last_verdict})\n--- frame ---\n{}\n--- scrollback ---\n{}",
                    self.frame(),
                    self.scrollback().join("\n")
                );
            }
            std::thread::sleep(POLL);
        }
    }

    fn sees(&mut self, needle: &str) {
        self.until(needle, TEXT_TIMEOUT, |frame| frame.contains(needle));
    }

    /// The composer is the last content on screen: the status line at
    /// the bottom of the live area, an input border within a few rows
    /// above it. Bottom of the VIEWPORT, not of the screen — after a
    /// resize-grow hi keeps its startup height, so the live area may
    /// sit above blank rows.
    fn assert_composer_visible(&self) {
        self.until(
            "composer at the bottom of the live area",
            TEXT_TIMEOUT,
            |f| {
                let rows: Vec<&str> = f.lines().collect();
                let Some(last) = rows.iter().rposition(|row| !row.trim().is_empty()) else {
                    return false;
                };
                rows[last].contains("(mock)")
                    && rows[..=last]
                        .iter()
                        .rev()
                        .skip(1)
                        .take(3)
                        .any(|row| row.contains('\u{2500}'))
            },
        );
    }

    /// No background tint on the composer rows — a flush that leaks a
    /// block's fill color (background-color-erase through a scroll)
    /// paints exactly here first. Sampled with the same settle rule as
    /// every other wait.
    fn assert_composer_untinted(&self) {
        let emu = Arc::clone(&self.emu);
        let bad = std::rc::Rc::new(std::cell::Cell::default());
        let bad_set = std::rc::Rc::clone(&bad);
        self.until("untinted composer rows", TEXT_TIMEOUT, move |f| {
            let _ = f;
            bad_set.set(None);
            let emu = emu.lock().unwrap();
            let rows = emu.size().1;
            for row in rows.saturating_sub(4)..rows {
                let bgs: Vec<String> = emu
                    .line(row)
                    .cells()
                    .iter()
                    .filter_map(|cell| cell.pen().background().map(|bg| format!("{bg:?}")))
                    .collect();
                if !bgs.is_empty() {
                    bad_set.set(Some((row, bgs)));
                    return false;
                }
            }
            true
        });
        if let Some((row, bgs)) = bad.take() {
            panic!(
                "composer row {row} carries background {bgs:?}\n--- frame ---\n{}",
                self.frame()
            );
        }
    }

    /// Wait for the settled end-of-turn frame: the reply's tail is on
    /// screen, the spinner is gone, the composer is empty.
    fn settled_turn(&mut self, reply_tail: &str) {
        self.until(reply_tail, TEXT_TIMEOUT, |frame| {
            frame.contains(reply_tail) && !frame.chars().any(char_is_spinner)
        });
    }

    /// Wait until the emulator's buffer grew past the screen height —
    /// avt models a flush by extending the buffer (its LF at the bottom
    /// appends rather than pushes scrollback, so the pushed rows'
    /// *content* is a real-terminal question, not an assertable one
    /// here), and growth past `rows` is exactly the flush's footprint.
    fn wait_for_flush(&self) {
        self.until("the flush to extend the buffer", TEXT_TIMEOUT, move |_| {
            self.emu.lock().unwrap().lines().count() > ROWS as usize
        });
    }

    /// Type `/exit` + Enter and poll for a clean exit — the teardown
    /// ordering is exactly what this wait covers: any hang (kitty pop,
    /// viewport restore, a poll loop that never quits) fails the test.
    fn exits_cleanly(self, timeout: Duration) -> bool {
        self.write("/exit\r");
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.lock().unwrap().try_wait().unwrap() {
                Some(status) => {
                    return if status.success() {
                        true
                    } else {
                        panic!("/exit exited with {status:?}");
                    };
                }
                None if Instant::now() >= deadline => {
                    let child = self.child.lock().unwrap();
                    let _ = child.clone_killer().kill();
                    panic!(
                        "never exited\n--- frame ---\n{}\n--- scrollback ---\n{}",
                        self.frame(),
                        self.scrollback().join("\n")
                    );
                }
                None => std::thread::sleep(POLL),
            }
        }
    }

    /// Resize the pty and the emulator together.
    fn resize(&self, rows: u16, cols: u16) {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        self.emu
            .lock()
            .unwrap()
            .resize(cols as usize, rows as usize);
    }
}

const TEXT_TIMEOUT: Duration = Duration::from_secs(20);

/// Braille spinner cells — present only mid-turn.
fn char_is_spinner(c: char) -> bool {
    ('\u{2800}'..='\u{28ff}').contains(&c)
}

/// Answer every handshake query occurrence, then hand back the bytes
/// after the last answered query (they belong to the emulator).
fn pending_replies(carry: &mut Vec<u8>, chunk: &[u8], writer: &Mutex<Box<dyn Write + Send>>) {
    carry.extend_from_slice(chunk);
    for (query, reply) in QUERIES {
        while let Some(at) = carry.windows(query.len()).position(|w| w == *query) {
            carry.drain(..at + query.len());
            {
                let mut w = writer.lock().unwrap();
                let _ = w.write_all(reply);
                let _ = w.flush();
            }
        }
    }
}

/// Longest complete UTF-8 prefix of `carry`; keeps incomplete tails
/// for the next chunk and drops genuinely invalid bytes.
fn complete_utf8(carry: &mut Vec<u8>) -> String {
    match std::str::from_utf8(carry) {
        Ok(text) => {
            let text = text.to_string();
            carry.clear();
            text
        }
        Err(e) => {
            let valid = e.valid_up_to();
            let keep_from = valid + e.error_len().unwrap_or(0);
            let text = String::from_utf8_lossy(&carry[..valid]).into_owned();
            carry.drain(..keep_from);
            text
        }
    }
}

#[test]
fn the_tui_boots_takes_keys_streams_and_exits_cleanly() {
    let mut tui = Tui::spawn(&["--mock"]);
    // The first rendered frame gates typing: until raw mode is on,
    // keystrokes are lost to the cooked-mode tty (see type_keys).
    tui.sees("\u{2500}");
    tui.type_keys("hello mock");
    tui.write("\r");
    // The mock streams its reply live above the input.
    tui.settled_turn("no API key required");
    tui.assert_composer_visible();
    assert!(
        tui.exits_cleanly(Duration::from_secs(20)),
        "/exit did not exit cleanly"
    );
}

#[test]
fn a_full_window_flush_pushes_rows_to_scrollback_and_keeps_the_composer() {
    let mut tui = Tui::spawn(&["--mock"]);
    tui.sees("\u{2500}");
    // Turn past the window: each turn's block must eventually leave
    // the live window through a flush, landing in scrollback — the
    // path whose terminal semantics (scrolling regions, BCE) broke
    // real terminals. The composer must survive every flush.
    for n in 1..=4 {
        tui.type_keys(&format!("message number {n}"));
        tui.write("\r");
        tui.settled_turn("no API key required");
        tui.assert_composer_visible();
        tui.assert_composer_untinted();
    }
    tui.wait_for_flush();
    assert!(
        tui.exits_cleanly(Duration::from_secs(20)),
        "/exit did not exit cleanly"
    );
}

#[test]
fn a_resize_mid_session_does_not_break_the_loop() {
    let mut tui = Tui::spawn(&["--mock"]);
    tui.sees("\u{2500}");
    tui.type_keys("still here");
    tui.write("\r");
    tui.settled_turn("no API key required");
    // Grow: hi's viewport keeps its startup height, so the terminal
    // is now taller than the viewport — a different flush path in the
    // renderer (gap-scroll above the viewport). Turn past the taller
    // window so a flush actually runs; the frame must stay coherent,
    // the composer intact.
    tui.resize(40, COLS);
    for n in 1..=3 {
        tui.type_keys(&format!("grown {n}"));
        tui.write("\r");
        tui.settled_turn("no API key required");
    }
    tui.assert_composer_visible();
    // Shrink: the loop must survive and take keys (ratatui re-anchors
    // the inline viewport by cursor offset here, so the composer's
    // glass position is not asserted — the settled turn is the
    // contract), then out.
    tui.resize(12, COLS);
    tui.type_keys("shrunk");
    tui.write("\r");
    tui.settled_turn("no API key required");
    assert!(
        tui.exits_cleanly(Duration::from_secs(20)),
        "/exit did not exit cleanly"
    );
}
