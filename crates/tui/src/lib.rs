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
    /// Open session file for turn-end appends (`None` = not persisting).
    pub session: Option<Session>,
    /// The shared image-attachment gate (the same Arc inside `read`).
    /// The app flips it on model switches — the registry outlives
    /// them, the policy follows (AD-0012).
    pub images: hi_core::ImageGate,
    /// The `settings.images` master switch, resolved at startup;
    /// re-resolution on a switch must honor it.
    pub images_setting: bool,
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
    // 3. two newlines: the cursor was parked on the viewport's middle
    //    row, so this lands on the first line *below* the old viewport
    //    — a fresh row for the shell prompt
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
    println!();
    println!();
    result
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
        app.scroll.sync(&app.messages, width, kitty::detect());
        let stream_h =
            ui::stream_window_height(height, ui::input_height(app, width), app.menu_height());
        let live_follows = app.turn.is_some() || ui::pending_tool(&app.messages).is_some();
        if let Some((rows, segments)) = app.scroll.take_flush(stream_h as usize, live_follows) {
            terminal.insert_before(rows, |buf| ui::segments_into(segments, buf))?;
        }

        // Redraw the live region: the transcript window + streaming
        // tail above the input.
        terminal.draw(|frame| ui::draw(frame, app))?;
    }

    Ok(())
}
