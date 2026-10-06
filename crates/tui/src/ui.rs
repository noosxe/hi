use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Stylize;
use ratatui::widgets::{Block, Paragraph};

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
    draw_input(frame, input_area);
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
            .map(|msg| msg.clone().into())
            .collect();
        Paragraph::new(lines)
    };
    frame.render_widget(paragraph, area);
}

/// The input box: bordered so it reads as an interactive element,
/// one line for now — multi-line editing comes later.
fn draw_input(frame: &mut Frame, area: Rect) {
    let block = Block::bordered().title(" input ");
    frame.render_widget(Paragraph::new("").block(block), area);
}
