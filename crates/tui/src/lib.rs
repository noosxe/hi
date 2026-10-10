//! The terminal front end: an inline-viewport ratatui interface.
//!
//! A *library*, not a binary: the `hi` binary lives in `crates/cli`
//! and hands a chosen provider to [`run`]. A future headless mode
//! simply never links any of this.

mod app;
mod commands;
mod ui;

pub mod catalog;
pub mod status;

pub use catalog::Catalog;
pub use status::Status;

use std::sync::Arc;
use std::time::Duration;

use ratatui::crossterm::event::{self, Event};
use ratatui::{TerminalOptions, Viewport};

use crate::app::App;
use hi_core::{ModelsConfig, Provider};

/// Run the interactive interface until the user quits.
pub fn run(
    provider: Arc<dyn Provider>,
    status: Status,
    catalog: Catalog,
    config: ModelsConfig,
) -> std::io::Result<()> {
    // Inline mode: NO alternate screen. The terminal keeps its normal
    // buffer and scrollback — we manage only the bottom 6 rows (a
    // two-row live streaming area above the 3-row input and the
    // status row), and
    // transcript lines are printed *above* the viewport, owned by the
    // terminal from then on. Native scrolling, search, and copy come
    // for free; pi's "regular mode" works the same way.
    let options = TerminalOptions {
        viewport: Viewport::Inline(6),
    };
    // try_init_with_options: enables raw mode, skips the alternate
    // screen, and installs a panic hook that restores the terminal.
    let mut terminal = ratatui::try_init_with_options(options)?;
    terminal.insert_before(1, |buf| ui::welcome_into(buf))?;

    let mut app = App::new(provider, status, catalog, config);
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
    // restore() reports its own errors to stderr and returns () —
    // the try_restore() variant is the Result-returning one.
    ratatui::restore();
    println!();
    println!();
    result
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
) -> std::io::Result<()> {
    let mut printed = 0;

    while !app.quit {
        // The frame clock: wait up to 50ms for a key, then continue
        // either way. `poll` (unlike the blocking `read` we had
        // before) is what lets provider events arrive and render with
        // zero keyboard input — roughly 20 frames per second.
        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                app.handle_key(key);
            }
        }

        // Deliver whatever the provider thread streamed this frame;
        // finished turns are finalized into `app.messages`.
        app.pump();

        // Anything appended to the transcript but not yet printed goes
        // into the scrollback, above the viewport. Once printed, the
        // terminal owns the line — we never redraw history.
        while printed < app.messages.len() {
            let msg = app.messages[printed].clone();
            terminal.insert_before(1, |buf| ui::message_into(msg, buf))?;
            printed += 1;
        }

        // Redraw the managed region (live streaming area + input).
        terminal.draw(|frame| ui::draw(frame, app))?;
    }

    Ok(())
}
