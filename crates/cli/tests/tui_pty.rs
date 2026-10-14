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
//! (One limit is recorded honestly: whether avt keeps the rows a
//! flush scrolls off the top depends on how they were scrolled, and
//! real scrollback is a real terminal's business — the rig asserts
//! that the live area gave the rows up, not where they went.)
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

    /// The rendered frame: the emulator's *view* — the live screen's
    /// rows, one trimmed row per terminal row — never the byte stream
    /// and never the scrollback above the view (a flush grows the
    /// buffer; only the view is what a user sees).
    fn frame(&self) -> String {
        let emu = self.emu.lock().unwrap();
        emu.view()
            .map(|line| line.text().trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn scrollback(&self) -> Vec<String> {
        let emu = self.emu.lock().unwrap();
        let rows = emu.size().1;
        let all: Vec<String> = emu.lines().map(|line| line.text()).collect();
        all[..all.len().saturating_sub(rows)].to_vec()
    }

    /// The caret column the TUI painted: ratatui positions the
    /// terminal cursor under the composer caret, and avt tracks
    /// where it went.
    fn cursor_col(&self) -> usize {
        self.emu.lock().unwrap().cursor().col
    }

    /// Sample the caret column until it holds one value for STABLE
    /// and return it — the caret lands with its frame, so a settled
    /// column is a settled screen.
    fn stable_col(&self, what: &str) -> usize {
        let deadline = Instant::now() + TEXT_TIMEOUT;
        let mut col = self.cursor_col();
        let mut stable_since: Option<Instant> = None;
        loop {
            let now = self.cursor_col();
            if now == col {
                match stable_since {
                    None => stable_since = Some(Instant::now()),
                    Some(then) if then.elapsed() >= STABLE => return col,
                    Some(_) => {}
                }
            } else {
                col = now;
                stable_since = None;
            }
            assert!(
                Instant::now() < deadline,
                "caret never settled: {what} (last col {col})"
            );
            std::thread::sleep(POLL);
        }
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

    /// The composer is the last content on screen: the status board's
    /// two resident rows at the bottom of the live area (location bar
    /// on the edge, model directly above it — AD-0025a), an input
    /// border within a few rows above that. Bottom of the VIEWPORT,
    /// not of the screen — after a resize-grow hi keeps its startup
    /// height, so the live area may sit above blank rows.
    fn assert_composer_visible(&self) {
        self.until(
            "composer at the bottom of the live area",
            TEXT_TIMEOUT,
            |f| {
                let rows: Vec<&str> = f.lines().collect();
                let Some(last) = rows.iter().rposition(|row| !row.trim().is_empty()) else {
                    return false;
                };
                let model_row = last.saturating_sub(1);
                // The location row is whatever cwd prints (a pty test
                // runs outside HOME, so no `~`) — being the bottom
                // edge is its whole assertion. The model anchors the
                // board.
                rows[model_row].contains("(mock)")
                    && rows[..=model_row]
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

/// Answer every query occurrence, removing only the query bytes —
/// never the bytes around them. The DSR hi sends once per flush
/// (`clear()`'s position snapshot) rides in the middle of real
/// content, so discarding a query's prefix used to eat transcript
/// rows out of the emulator's feed while hi had emitted them fine.
fn pending_replies(carry: &mut Vec<u8>, chunk: &[u8], writer: &Mutex<Box<dyn Write + Send>>) {
    carry.extend_from_slice(chunk);
    for (query, reply) in QUERIES {
        let mut i = 0;
        while i + query.len() <= carry.len() {
            if &carry[i..i + query.len()] == *query {
                carry.drain(i..i + query.len());
                {
                    let mut w = writer.lock().unwrap();
                    let _ = w.write_all(reply);
                    let _ = w.flush();
                }
            } else {
                i += 1;
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
fn a_terminal_still_opens_the_tui_without_flags() {
    // #19 is a gap-filler, not a takeover: with both streams a real
    // terminal and no mode flag, the TUI opens exactly as before. A
    // wrongly auto-printing binary would emit one reply line and exit
    // — this frame would never render.
    let mut tui = Tui::spawn(&["--mock"]);
    tui.sees("\u{2500}");
    assert!(tui.exits_cleanly(Duration::from_secs(20)));
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
fn a_full_window_gives_its_rows_up_when_it_overflows_and_keeps_the_composer() {
    let mut tui = Tui::spawn(&["--mock"]);
    tui.sees("\u{2500}");
    // Turn well past the window: the stream area holds roughly
    // height - input - status rows and turn height varies with the
    // runner (reply wrapping, separator compaction), so drive enough
    // turns to overflow with margin on the most compact layout. Each
    // turn the composer must survive, and at the end turn 1's block
    // must have left the live area through a flush.
    // Turn 1 gets a needle no later turn can match as a substring.
    tui.type_keys("alpha message");
    tui.write("\r");
    tui.settled_turn("no API key required");
    tui.assert_composer_visible();
    tui.assert_composer_untinted();
    for n in 2..=12 {
        tui.type_keys(&format!("message number {n}"));
        tui.write("\r");
        tui.settled_turn("no API key required");
        tui.assert_composer_visible();
        tui.assert_composer_untinted();
    }
    // The flush's footprint, user-visible: turn 1's block no longer
    // holds any row of the live area. (What the flushed rows become in
    // a real terminal's scrollback is outside the emulator's model.)
    tui.until("turn 1 to leave the live area", TEXT_TIMEOUT, |f| {
        !f.contains("alpha message")
    });
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

/// #63: Ghostty's macOS defaults send option+left/right as the
/// readline word motions — ESC b / ESC f — which crossterm decodes
/// as Alt+B / Alt+F. Terminals that report the chord itself send
/// the legacy CSI encodings instead. Every wire form must move the
/// composer caret by a word; the assertion is the painted caret
/// column relative to the typed tail, so the prompt width cancels.
#[test]
fn option_arrows_word_motions_arrive_through_their_wire_encodings() {
    let mut tui = Tui::spawn(&["--mock"]);
    tui.sees("\u{2500}");
    tui.type_keys("alpha bravo charlie");
    let base = tui.stable_col("typed input at the tail");

    // Ghostty's actual bytes for option+left.
    tui.write("\x1bb");
    assert_eq!(tui.stable_col("ESC b once"), base - 7); // over "charlie"
    tui.write("\x1bb");
    assert_eq!(tui.stable_col("ESC b twice"), base - 13); // over "bravo "

    // Terminals that report the chord itself.
    tui.write("\x1b[1;3D");
    assert_eq!(tui.stable_col("alt+left CSI"), base - 19); // to the start
    tui.write("\x1b[1;5D");
    assert_eq!(tui.stable_col("ctrl+left CSI at the start"), base - 19); // clamped

    // The forward twins walk back out to the tail.
    tui.write("\x1bf");
    assert_eq!(tui.stable_col("ESC f once"), base - 14); // end of "alpha"
    tui.write("\x1bf");
    assert_eq!(tui.stable_col("ESC f twice"), base - 8); // end of "bravo"
    tui.write("\x1bf");
    assert_eq!(tui.stable_col("ESC f thrice"), base);
    tui.write("\x1b[1;3C");
    assert_eq!(tui.stable_col("alt+right CSI at the tail"), base);

    tui.type_keys("/exit");
    tui.write("\r");
    assert!(
        tui.exits_cleanly(Duration::from_secs(20)),
        "/exit did not exit cleanly"
    );
}

/// The `/` completion menu renders over a real pty — typing the slash
/// opens the command list under the input, navigating moves the
/// selection, and the menu yields when the input leaves command space.
#[test]
fn slash_opens_the_completion_menu_over_a_real_pty() {
    let mut tui = Tui::spawn(&["--mock"]);
    tui.sees("\u{2500}");
    tui.type_keys("/");
    tui.until("the command list", TEXT_TIMEOUT, |frame| {
        frame.contains("exit") && frame.contains("model")
    });

    // Down moves the selection marker; the marker row changes.
    let before = tui.frame();
    tui.write("\x1b[B"); // down arrow
    tui.until("selection moves", TEXT_TIMEOUT, |f| {
        f != before && f.contains("exit")
    });

    // A space leaves command space: the menu yields to the status row.
    tui.type_keys(" x");
    tui.until("menu yields", TEXT_TIMEOUT, |f| {
        !f.contains("quit the session")
    });

    tui.type_keys("/exit");
    tui.write("\r");
    assert!(
        tui.exits_cleanly(Duration::from_secs(20)),
        "/exit did not exit cleanly"
    );
}

/// #66: Shift+Enter opens a newline in the composer through its real
/// wire form — the kitty encoding Ghostty sends (`CSI 13;2u`) — the
/// composer grows a row per line, Ctrl+J's raw byte (the legacy
/// chord, no kitty needed) adds another, and plain Enter submits the
/// whole multi-line draft, which then echoes into the transcript.
#[test]
fn shift_enter_multiline_input_lifecycle_over_a_real_pty() {
    let mut tui = Tui::spawn(&["--mock"]);
    tui.sees("\u{2500}");
    tui.type_keys("top line");
    tui.write("\x1b[13;2u"); // Shift+Enter, kitty-encoded
    tui.type_keys("mid line");
    tui.until("the composer grows a second row", TEXT_TIMEOUT, |f| {
        f.contains("top line") && f.contains("mid line")
    });
    tui.write("\x0a"); // Ctrl+J's raw byte — no kitty needed
    tui.type_keys("low line");
    tui.until("the composer grows a third row", TEXT_TIMEOUT, |f| {
        f.contains("low line")
    });

    // Plain Enter submits the whole draft; the prompt echoes into the
    // transcript and the mock turn settles beneath it.
    tui.write("\r");
    tui.settled_turn("no API key required");
    tui.until(
        "the multi-line prompt in the transcript",
        TEXT_TIMEOUT,
        |f| f.contains("top line") && f.contains("low line"),
    );

    tui.type_keys("/exit");
    tui.write("\r");
    assert!(
        tui.exits_cleanly(Duration::from_secs(20)),
        "/exit did not exit cleanly"
    );
}
