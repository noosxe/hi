mod app;
mod ui;

use ratatui::crossterm::event::{self, Event};
use ratatui::{TerminalOptions, Viewport};

use crate::app::App;

fn main() -> std::io::Result<()> {
    // Inline mode: NO alternate screen. The terminal keeps its normal
    // buffer and scrollback — we manage only the bottom 3 rows (the
    // input area), and transcript lines are printed *above* the
    // viewport, owned by the terminal from then on. Native scrolling,
    // search, and copy come for free; pi's "regular mode" works the
    // same way.
    let options = TerminalOptions {
        viewport: Viewport::Inline(3),
    };
    // try_init_with_options: enables raw mode, skips the alternate
    // screen, and installs a panic hook that restores the terminal.
    let mut terminal = ratatui::try_init_with_options(options)?;

    let mut app = App::new();
    let result = run(&mut terminal, &mut app);

    // restore() reports its own errors to stderr and returns () —
    // the try_restore() variant is the Result-returning one.
    ratatui::restore();
    result
}

fn run(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
) -> std::io::Result<()> {
    let mut printed = 0;

    while !app.quit {
        // Anything appended to the transcript but not yet printed goes
        // into the scrollback, above the viewport. Once printed, the
        // terminal owns the line — we never redraw history.
        while printed < app.messages.len() {
            let msg = app.messages[printed].clone();
            terminal.insert_before(1, |buf| ui::message_into(msg, buf))?;
            printed += 1;
        }

        // Redraw the managed region (input area only).
        terminal.draw(|frame| ui::draw(frame, app))?;

        if let Event::Key(key) = event::read()? {
            app.handle_key(key);
        }
    }

    Ok(())
}
