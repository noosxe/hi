use hi_core::Message;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Stylize, Style};
use ratatui::text::Line;
use ratatui::widgets::{List, ListItem, ListState, Block, Borders, Paragraph, Widget, Wrap};
use ratatui::Frame;

use crate::app::App;

/// Render the managed region: streaming area, input, and the status
/// row. The transcript is NOT drawn here — it lives in the terminal's
/// scrollback (see `message_into`).
pub fn draw(frame: &mut Frame, app: &App) {
    // The frame IS the 6-row inline viewport. Three fixed-height
    // areas need no layout solver — plain struct arithmetic. The
    // functional update syntax `..frame.area()` copies every field
    // not named.
    let stream_area = Rect { height: 2, ..frame.area() };
    let input_area = Rect {
        y: frame.area().y + 2,
        height: 3,
        ..frame.area()
    };
    let status_area = Rect {
        y: frame.area().y + 5,
        height: 1,
        ..frame.area()
    };
    if app.picker.is_some() {
        let picker_area = Rect { height: 5, ..frame.area() };
        draw_picker(frame, picker_area, app);
    } else {
        draw_stream(frame, stream_area, app);
        draw_input(frame, input_area, app);
    }
    draw_status(frame, status_area, app);

    // The `/` completion menu overlays the upper rows — never while
    // the model picker owns the screen.
    if app.picker.is_none() {
        draw_completion(frame, frame.area(), app);
    }
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


/// The status row: cwd (branch) on the left, (provider) model on the
/// right. All dim — decoration is never the star of the frame.
fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    let text = crate::status::compose_status(&app.location, &app.status.label(), area.width);
    frame.render_widget(Paragraph::new(text).style(Style::new().dim()), area);
}

/// The `/model` popup: fills the viewport above the status row. The
/// inline viewport is only 6 rows, so the picker lives inside it —
/// the `List` scrolls, so any model count works.
fn draw_picker(frame: &mut Frame, area: Rect, app: &App) {
    let Some(picker) = app.picker.as_ref() else {
        return;
    };
    let items: Vec<ListItem> = picker
        .items
        .iter()
        .map(|item| ListItem::from(item.as_str()))
        .collect();
    let mut state = ListState::default();
    state.select(Some(picker.selected));
    let list = List::new(items)
        .block(
            Block::new()
                .borders(Borders::ALL)
                .title(" model — enter: use · ctrl-s: default · esc: close "),
        )
        .highlight_symbol("› ")
        .highlight_style(Style::new().bold());
    frame.render_stateful_widget(list, area, &mut state);
}

/// The `/` completion menu: anchored to the top of the viewport,
/// echoing the typed text in its title. Commands grow over time; the
/// list scrolls.
fn draw_completion(frame: &mut Frame, area: Rect, app: &App) {
    let Some(completion) = app.completion.as_ref() else {
        return;
    };
    let height = (completion.items.len() as u16 + 2).clamp(3, area.height);
    let popup = Rect {
        height,
        width: area.width.min(64),
        ..area
    };
    let items: Vec<ListItem> = completion
        .items
        .iter()
        .map(|(name, description)| {
            ListItem::from(Line::from(vec![
                format!("/{name} ").bold(),
                (*description).dim(),
            ]))
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(completion.selected));
    let list = List::new(items)
        .block(Block::new().borders(Borders::ALL).title(format!(
            " /{} ",
            app.input.strip_prefix('/').unwrap_or("")
        )))
        .highlight_symbol("› ")
        .highlight_style(Style::new().bold());
    frame.render_stateful_widget(list, popup, &mut state);
}
/// The in-progress reply, rendered live in the viewport, dimmed to
/// signal it is not final. Only the tail fits in two rows — the
/// complete text is printed to scrollback when the turn ends.
fn draw_stream(frame: &mut Frame, area: Rect, app: &App) {
    let Some(turn) = app.turn.as_ref() else {
        return;
    };
    if turn.text.is_empty() && turn.error.is_none() {
        return;
    }
    // Keep only as many trailing characters as fit the two wrapped
    // rows. Counted in chars, not display columns — emoji drift is a
    // known gap, queued for the input-polish milestone.
    let capacity = area.width as usize * area.height as usize;
    let count = turn.text.chars().count();
    let mut lines: Vec<Line> = Vec::new();
    if !turn.text.is_empty() {
        let tail: String = turn
            .text
            .chars()
            .skip(count.saturating_sub(capacity))
            .collect();
        lines.push(Line::from(tail).style(Style::new().dim()));
    }
    if let Some(err) = &turn.error {
        lines.push(Line::from(err.clone()).red());
    }
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
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
        Message::Error(text) => Line::from(vec!["error ".red(), text.into()]),
        Message::System(text) => Line::from(text).style(Style::new().dim()),
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
