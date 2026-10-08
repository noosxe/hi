use hi_core::Message;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph, Widget};
use ratatui::Frame;

use crate::app::App;

/// Render the managed region: the input area, and nothing else.
/// The transcript is NOT drawn here — it lives in the terminal's
/// scrollback (see `message_into`).
pub fn draw(frame: &mut Frame, app: &App) {
    // The frame IS the 3-row inline viewport — no layout solving needed.
    draw_input(frame, frame.area(), app);
}

/// The input area: top/bottom rules only, no side walls — pi-style.
/// Shows whatever is currently being typed, with the real terminal
/// cursor parked right after the text.
fn draw_input(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::new().borders(Borders::TOP | Borders::BOTTOM);
    // Borrow what we need from `block` BEFORE the render call:
    // `.block(block)` below moves ownership of block into the
    // paragraph, and a moved value can't be used again.
    let inner = block.inner(area);

    // as_str borrows the text — no clone needed for rendering.
    frame.render_widget(Paragraph::new(app.input.as_str()).block(block), area);

    // Park the real terminal cursor just past the typed text.
    // inner.x is already the first writable column — inner() excludes
    // the border, so no extra +1 (that double-counted it).
    frame.set_cursor_position(Position::new(
        inner.x + app.input.chars().count() as u16,
        inner.y,
    ));
}

/// Format one transcript message and render it into a scrollback row.
/// Called via `Terminal::insert_before`; after this runs, the line is
/// terminal-owned history — scrolling it is the emulator's job.
pub fn message_into(msg: Message, buf: &mut Buffer) {
    // We own the message, so its String moves straight into the line —
    // no clones. The dim label marks who spoke.
    let line = match msg {
        Message::User(text) => Line::from(vec!["you ".dim(), text.into()]),
        Message::Assistant(text) => Line::from(vec!["assistant ".dim(), text.into()]),
    };
    Paragraph::new(line).render(buf.area, buf);
}

/// One-row welcome banner, printed above the viewport on startup.
/// Doubles as visual separation from whatever the terminal was
/// showing before the app launched.
pub fn welcome_into(buf: &mut Buffer) {
    let line = Line::from(vec![
        "hi".bold(),
        " · say something, ctrl-c to quit".dim(),
    ]);
    Paragraph::new(line).render(buf.area, buf);
}
