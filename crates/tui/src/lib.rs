//! The terminal front end: an inline-viewport ratatui interface.
//!
//! A *library*, not a binary: the `hi` binary lives in `crates/cli`
//! and hands a chosen provider to [`run`]. A future headless mode
//! simply never links any of this.

mod app;
mod clipboard;
mod commands;
mod editor;
mod kitty;
mod markdown;
mod picker;
mod scroll;
mod theme;
mod ui;
mod wrap;

pub mod catalog;
pub mod keybinds;
pub mod status;

/// The standalone session chooser (`--resume`); `/resume` reuses the
/// helpers from `picker` inside the running TUI.
pub use picker::pick_session;

pub use catalog::Catalog;
pub use status::Status;

use std::sync::Arc;
use std::time::Duration;

use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::{TerminalOptions, Viewport};

use crate::app::App;
use hi_core::event::Usage;
use hi_core::{Message, ModelsConfig, Provider, Session, ThinkingLevel, ToolRegistry};

/// Everything a session carries beyond provider/config: the pieces that
/// exist before the first keypress. One struct instead of a nine-argument
/// constructor — and the natural home for whatever the extension track
/// adds next (subagent config, hook registries, …).
pub struct SessionInit {
    /// Resolved system prompt (`None` sends none).
    pub system_prompt: Option<String>,
    /// Session thinking level.
    pub thinking: Option<ThinkingLevel>,
    /// Tools the model may call.
    pub tools: Arc<ToolRegistry>,
    /// Transcript to start from — empty, or the resumed session's.
    pub messages: Vec<Message>,
    /// The replayed bill (#85): usage merged across the session file's
    /// recorded rounds, and the last recorded round's prompt size —
    /// the context-share fact. Both are zero until the first turn,
    /// live or recorded.
    pub usage: Usage,
    pub usage_last_prompt: u64,
    /// Open session file for turn-end appends (`None` = not persisting).
    pub session: Option<Session>,
    /// The shared image-attachment gate (the same Arc inside `read`).
    /// The app flips it on model switches — the registry outlives
    /// them, the policy follows (AD-0012).
    pub images: hi_core::ImageGate,
    /// The `settings.images` master switch, resolved at startup;
    /// re-resolution on a switch must honor it.
    pub images_setting: bool,
    /// Main-loop tool-round cap, resolved at startup: the
    /// `settings.maxToolRounds` override or the built-in default.
    pub max_tool_rounds: usize,
    /// The background-task manager shared with the registry's `task_*`
    /// tools. The app drains its completion channel per frame and shows
    /// the running count in the status row (`None` = no task tools).
    /// The nested-agent runner (#4): the `subagent` tool's handle. The
    /// app restamps it on every provider rebuild so inherited runs
    /// follow /model and /resume.
    pub subagent: hi_core::subagent::SubagentRunner,
    pub tasks: Option<hi_core::tasks::TaskManager>,
    /// The `settings.taskTurns` opt-in, resolved at startup (#72,
    /// AD-0022): a finished task may start a turn. The app owns the
    /// trigger; core only resolves the human's choice.
    pub task_turns: bool,
}

/// Run the interactive interface until the user quits.
pub fn run(
    provider: Arc<dyn Provider>,
    status: Status,
    catalog: Catalog,
    config: ModelsConfig,
    init: SessionInit,
    telemetry: bool,
) -> std::io::Result<()> {
    // Inline mode: NO alternate screen. The terminal keeps its normal
    // buffer and scrollback; the viewport is the whole screen (the
    // inline height is frozen at construction, so it is sized once
    // here — a mid-run resize keeps the startup height, clamped).
    // The scrollback is the flushed prefix of the transcript (AD-0016)
    // and the viewport shows its live tail down to the input — pi's
    // "regular mode" works the same way. Native scrolling, search, and
    // copy come for free.
    let rows = ratatui::crossterm::terminal::size()
        .map(|(_, h)| h)
        .unwrap_or(24)
        .max(1);
    let options = TerminalOptions {
        viewport: Viewport::Inline(rows),
    };
    // try_init_with_options: enables raw mode, skips the alternate
    // screen, and installs a panic hook that restores the terminal.
    let mut terminal = ratatui::try_init_with_options(options)?;
    // ratatui's hook (ratatui 0.30.2 `init.rs`, `set_panic_hook`) runs
    // `restore()` then the previous hook, on ANY thread. A tool panic on
    // the agent thread is caught and becomes an error result (#172), but
    // the hook fires before the catch: left alone it would turn raw mode
    // off under a live TUI and print over the screen. Only the UI thread's
    // panics may touch the terminal.
    guard_panic_hook(std::thread::current().id());
    // Bracketed paste: without it the terminal sends a paste as raw
    // keystrokes, and every newline in the pasted text submits the
    // input mid-paste. With it, the whole paste arrives as one
    // `Event::Paste` (AD-0011). The panic hook restores the terminal
    // but not this mode — a panic mid-paste may leak a stray `200~`
    // into the shell, the standard crossterm trade-off.
    execute!(std::io::stdout(), EnableBracketedPaste)?;
    // Kitty keyboard protocol, minimal enhancement (AD-0013): under
    // `disambiguate`, every key event that does not generate text —
    // Cmd+V among them — arrives as CSI u with its modifier bits, so
    // supporting terminals (Ghostty, kitty, WezTerm) can let Cmd+V
    // through to the app. Non-supporting terminals ignore the
    // sequence and nothing changes. Enter/Tab/Backspace keep their
    // legacy bytes (the spec's explicit exception), plain typing is
    // untouched; Ctrl+C stops hitting SIGINT and arrives as an event
    // instead, which raw mode had already made harmless. Pair-push
    // with the pop below; the panic-hook caveat above applies.
    execute!(
        std::io::stdout(),
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )?;
    let mut app = App::new(provider, status, catalog, config, init, telemetry);
    let result = event_loop(&mut terminal, &mut app);

    // Teardown, in order:
    // 1. wipe the managed region — clear() erases from the viewport
    //    origin to the end of the screen and then restores the cursor,
    //    so the last drawn frame doesn't linger under the shell
    // 2. restore terminal modes (raw mode off)
    //
    // Quitting mid-turn: `app` drops here, dropping the turn's
    // Receiver. The provider thread's next send fails, so it returns
    // instead of streaming into a dead channel.
    terminal.clear()?;
    // Mirror the enable: leave the terminal the way we found it.
    // Best-effort — teardown must not mask the run's result.
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    // restore() reports its own errors to stderr and returns () —
    // the try_restore() variant is the Result-returning one.
    ratatui::restore();
    result
}

/// Wrap the current panic hook (ratatui's, restore-then-print) so it
/// only runs for panics on the `ui` thread. Panics anywhere else —
/// agent, provider, task-monitor threads — neither restore the terminal
/// nor write to it; their failures surface as error results / `Error`
/// events instead (#172). Must be installed after ratatui's init.
fn guard_panic_hook(ui: std::thread::ThreadId) {
    let inner = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().id() == ui {
            inner(info);
        }
    }));
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> std::io::Result<()> {
    while !app.quit {
        // The frame clock: wait up to 50ms for a key, then continue
        // either way. `poll` (unlike the blocking `read` we had
        // before) is what lets provider events arrive and render with
        // zero keyboard input — roughly 20 frames per second.
        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key) => app.handle_key(key),
                Event::Paste(text) => app.handle_paste(&text),
                // Resize redraws every frame anyway; mouse and focus
                // events carry nothing hi listens for.
                _ => {}
            }
        }

        // Deliver whatever the provider thread streamed this frame;
        // finished turns are finalized into `app.messages`.
        app.pump();

        // The flushed-prefix window (AD-0016): fold any new messages
        // into the cached transcript, then push the rows the live
        // window cannot show into the scrollback above the viewport —
        // exactly once, in order, images included. One chunk per
        // frame; the seam between scrollback and window stays
        // invisible because both are projections of the same block
        // stream.
        let size = terminal.size()?;
        let (width, height) = (size.width, size.height);
        app.width = width;
        app.scroll
            .sync(&app.messages, width, kitty::detect(), app.turn.is_some());
        let stream_h = ui::stream_window_height(
            height,
            ui::input_height(app, width),
            app.menu_height(),
            app.status_height(),
        );
        let live_follows = app.turn.is_some();
        if let Some((rows, segments)) = app.scroll.take_flush(stream_h as usize, live_follows) {
            terminal.insert_before(rows, |buf| ui::segments_into(segments, buf))?;
        }

        // Redraw the live region: the transcript window + streaming
        // tail above the input.
        retry_lost_cursor_reply(|| terminal.draw(|frame| ui::draw(frame, app)).map(|_| ()))?;
    }

    Ok(())
}

/// How many times a frame op is retried after crossterm loses a cursor
/// reply before the error is allowed to end the session.
const CURSOR_REPLY_RETRIES: usize = 3;

/// Is this crossterm's "the terminal never answered `ESC [ 6 n`" error?
fn is_cursor_reply_timeout(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::Other
        && err
            .to_string()
            .contains("cursor position could not be read")
}

/// Run a frame op that may query the cursor position (ratatui's inline
/// viewport queries it when the terminal size changes), retrying when crossterm
/// reports the reply never came (#223).
///
/// crossterm 0.29's event source is edge-triggered (mio) and handles
/// one readiness token at a time: when a SIGWINCH and the terminal's
/// answer become ready in the same wakeup, it returns the resize first
/// and drops the answer's notification. The answer sits unread in the
/// tty and crossterm waits its full two seconds for a notification that
/// never comes, then errors — which used to end the session
/// (`hi: The cursor position could not be read within a normal
/// duration`) when a resize landed while a query was in flight. A retry
/// sends a fresh query, whose answer raises a fresh edge; the stale
/// answer is swallowed as an internal event. The op failed before it
/// drew anything, so repeating it is safe.
fn retry_lost_cursor_reply<T>(mut op: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    for _ in 0..CURSOR_REPLY_RETRIES {
        match op() {
            Err(err) if is_cursor_reply_timeout(&err) => continue,
            other => return other,
        }
    }
    op()
}

/// The panic hook is process-global: tests that swap it take this lock.
#[cfg(test)]
pub(crate) static HOOK_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn a_lost_cursor_reply_is_retried_and_other_errors_are_not() {
        let timeout = || {
            std::io::Error::other("The cursor position could not be read within a normal duration")
        };
        let mut calls = 0;
        let out = retry_lost_cursor_reply(|| {
            calls += 1;
            if calls < 3 { Err(timeout()) } else { Ok(calls) }
        });
        assert_eq!(out.unwrap(), 3, "two lost replies, then the frame lands");

        // A reply that never comes still ends the session — after the
        // bounded retries, not forever.
        let mut calls = 0;
        let out: std::io::Result<()> = retry_lost_cursor_reply(|| {
            calls += 1;
            Err(timeout())
        });
        assert!(out.is_err());
        assert_eq!(calls, CURSOR_REPLY_RETRIES + 1);

        // Any other failure surfaces at once.
        let mut calls = 0;
        let out: std::io::Result<()> = retry_lost_cursor_reply(|| {
            calls += 1;
            Err(std::io::Error::other("broken pipe"))
        });
        assert!(out.is_err());
        assert_eq!(calls, 1);
    }

    #[test]
    fn the_hook_runs_for_the_ui_thread_only() {
        // Stand-in for ratatui's restore-then-print hook: counts calls.
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        let _lock = HOOK_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let original = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {
            CALLS.fetch_add(1, Ordering::SeqCst);
        }));
        guard_panic_hook(std::thread::current().id());

        let off_ui = std::thread::spawn(|| {
            let _ = std::panic::catch_unwind(|| panic!("worker"));
        });
        off_ui.join().unwrap();
        assert_eq!(CALLS.load(Ordering::SeqCst), 0, "worker panic is silent");

        let _ = std::panic::catch_unwind(|| panic!("ui"));
        assert_eq!(CALLS.load(Ordering::SeqCst), 1, "UI panic still restores");

        let _ = std::panic::take_hook();
        std::panic::set_hook(original);
    }
}
