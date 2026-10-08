use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Stylize;
use ratatui::widgets::{Block, Borders, Paragraph};

use hi_core::Message;
use ratatui::text::Line;
use crate::app::App;

/// Render one full frame: a pure function of the app state.
/// Same state in, same pixels out — this function never mutates anything.
pub fn draw(frame: &mut Frame, app: &App) {
    // Split the screen vertically: transcript takes everything
    // that's left, input always gets exactly 3 rows at the bottom.
    let [transcript_area, input_area] = Layout::vertical([
        Constraint::Min(0),    // "give me at least 0 rows, i.e. the rest"
        Constraint::Length(3), // border row + one text row + border row
    ])
    .areas(frame.area());

    draw_transcript(frame, transcript_area, app);
    draw_input(frame, input_area, app);
}

/// The scrolling conversation: user messages, assistant messages,
/// tool calls — everything the agent does will render in here.
fn draw_transcript(frame: &mut Frame, area: Rect, app: &App) {
    let paragraph = if app.messages.is_empty() {
        Paragraph::new("no messages yet".dim())
    } else {
        let lines: Vec<_> = app
            .messages
            .iter()
            .map(|msg| match msg {
                Message::User(text) => {
                    Line::from(vec!["you ".dim(), text.clone().into()])
                }
                Message::Assistant(text) => {
                    Line::from(vec!["assistant ".dim(), text.clone().into()])
                }
            })
            .collect();
        Paragraph::new(lines)
    };
    frame.render_widget(paragraph, area);
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
