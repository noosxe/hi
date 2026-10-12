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
    // buffer and scrollback — we manage only the bottom 12 rows (an
    // 8-row live streaming area that shrinks while a menu is open
    // above the 3-row input, plus the status row), and
    // transcript lines are printed *above* the viewport, owned by the
    // terminal from then on. Native scrolling, search, and copy come
    // for free; pi's "regular mode" works the same way.
    let options = TerminalOptions {
        viewport: Viewport::Inline(12),
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
    terminal.insert_before(1, ui::welcome_into)?;

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

        // Anything appended to the transcript but not yet printed goes
        // into the scrollback above the viewport; once printed, the
        // terminal owns the rows and we never redraw history. Display
        // blocks are derived from the message stream (a call pairs with
        // its result); a trailing unpaired call stays in the live area
        // as the pending execution until its result lands.
        while app.printed < app.messages.len() {
            let (blocks, pending) = ui::derive_blocks(&app.messages[app.printed..]);
            if blocks.is_empty() {
                break; // only the pending execution — wait for its result
            }
            // Re-read the width per batch: a resize mid-burst must not
            // measure rows against the old width.
            let width = terminal.size()?.width;
            // Detect once per batch: env vars do not change mid-burst,
            // and the answer is passed down, not read in the renderer
            // (which would make ui tests depend on the real terminal).
            let caps = kitty::detect();
            for block in &blocks {
                // Spacing is each block's own padding (one blank row above
                // and below, tinted for filled blocks — see `block_rows`),
                // so no cross-block separator bookkeeping exists here.
                let segments = ui::block_segments(block, width, caps);
                let height = ui::segments_height(&segments).max(1);
                terminal.insert_before(height, |buf| ui::segments_into(segments, buf))?;
            }
            app.printed = app.messages.len() - usize::from(pending.is_some());
        }

        // Redraw the managed region (live streaming area + input).
        terminal.draw(|frame| ui::draw(frame, app))?;
    }

    Ok(())
}
