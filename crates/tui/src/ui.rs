use hi_core::Message;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Stylize, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List, ListItem, ListState, Block, Borders, Paragraph, Widget, Wrap};
use unicode_width::UnicodeWidthChar;
use ratatui::Frame;

use crate::app::App;

/// Render the managed region: streaming area, input, and the status
/// row. The transcript is NOT drawn here — it lives in the terminal's
/// scrollback (see `message_into`).
pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    // A fixed 12-row viewport with two elastic consumers. Bottom-up:
    // the status row is pinned, the input grows upward with its
    // wrapped text (3 rows resting, up to 8), the menu takes rows
    // above the input (0 when closed), and the streaming area keeps
    // whatever remains — never fewer than 2 rows.
    //
    //   resting:    [stream 8] [input 3] [status 1]
    //   menu open:  [stream 5] [menu 3] [input 3] [status 1]
    //   long input: [stream 2] [input 8] [status 1]
    let input_rows = crate::wrap::wrapped_row_count(&app.input, area.width as usize)
        .clamp(1, 6) as u16;
    let input_height = input_rows + 2; // rules
    let menu_h = app.menu_height().min(9 - input_height);
    let stream_h = 11 - input_height - menu_h;
    let stream_area = Rect { height: stream_h, ..area };
    let menu_area = Rect {
        y: area.y + stream_h,
        height: menu_h,
        ..area
    };
    let input_area = Rect {
        y: area.y + 11 - input_height,
        height: input_height,
        ..area
    };
    let status_area = Rect {
        y: area.y + 11,
        height: 1,
        ..area
    };
    draw_stream(frame, stream_area, app);
    draw_input(frame, input_area, app);
    if menu_h > 0 {
        if app.picker.is_some() {
            draw_picker(frame, menu_area, app);
        } else {
            draw_completion(frame, menu_area, app);
        }
    }
    draw_status(frame, status_area, app);
}

/// The input area: top/bottom rules only, no side walls — pi-style.
/// Shows whatever is currently being typed, with the real terminal
/// cursor parked right after the text.
fn draw_input(frame: &mut Frame, area: Rect, app: &App) {
    let mut block = Block::new().borders(Borders::TOP | Borders::BOTTOM);
    // While work is in flight, the top rule carries a right-aligned
    // activity title; quiet sessions render a plain rule.
    if let Some(activity) = app.activity_label() {
        // Left margin, content, right margin — then two unstyled
        // rule characters: they overwrite the border seamlessly, so
        // the rule reads as continuing past the menu's right edge.
        block = block.title_top(
            Line::from(vec![
                Span::from(" "),
                Span::from(activity).yellow(),
                Span::from(" "),
                Span::from("──"),
            ])
            .right_aligned(),
        );
    }
    // Borrow what we need from `block` BEFORE the render call:
    // `.block(block)` below moves ownership of block into the
    // paragraph, and a moved value can't be used again.
    let inner = block.inner(area);

    // Word-wrap the input into the elastic text rows and park the
    // real cursor after the last visible cell of the last row.
    // Widths are display columns — emoji count double.
    let rows = crate::wrap::wrap(&app.input, inner.width as usize);
    let text: Text = rows.iter().map(|row| Line::from(row.as_str())).collect();
    frame.render_widget(Paragraph::new(text).block(block), area);
    let last_row = rows.last().map(String::as_str).unwrap_or("");
    frame.set_cursor_position(Position::new(
        inner.x + crate::wrap::display_width(last_row) as u16,
        inner.y + rows.len() as u16 - 1,
    ));
}


/// The status row: cwd (branch) on the left, (provider) model on the
/// right. All dim — decoration is never the star of the frame.
fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    // The thinking level rides on the right label when set.
    let right = match app.thinking {
        Some(level) => format!("{} · thinking:{}", app.status.label(), level.name()),
        None => app.status.label(),
    };
    let text = crate::status::compose_status(&app.location, &right, area.width);
    frame.render_widget(Paragraph::new(text).style(Style::new().dim()), area);
}

/// The `/model` picker, drawn in the menu zone directly above the
/// input. Five rows for the common case (a handful of models); the
/// `List` scrolls when a server offers more.
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

/// The `/` completion menu, drawn in the menu zone between the
/// streaming area and the input. Echoes the typed text in its title;
/// commands grow over time, and the list scrolls.
fn draw_completion(frame: &mut Frame, area: Rect, app: &App) {
    let Some(completion) = app.completion.as_ref() else {
        return;
    };
    // Narrower than the viewport reads as a menu, not a pane.
    let popup = Rect {
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
/// signal it is not final. Only the tail fits in the elastic stream
/// rows — the complete text is printed to scrollback when the turn
/// ends.
fn draw_stream(frame: &mut Frame, area: Rect, app: &App) {
    let Some(turn) = app.turn.as_ref() else {
        return;
    };
    if turn.text.is_empty() && turn.error.is_none() {
        return;
    }
    // Trim from the front until the tail fits the visible columns —
    // counted in display columns (emoji are two), not characters.
    let capacity = area.width as usize * area.height as usize;
    let mut start = 0;
    let mut used = 0usize;
    for (index, ch) in turn.text.char_indices().rev() {
        let width = ch.width().unwrap_or(0);
        if used + width > capacity {
            start = index + ch.len_utf8();
            break;
        }
        used += width;
    }
    let mut lines: Vec<Line> = Vec::new();
    if !turn.text.is_empty() {
        let tail: String = turn.text[start..].to_string();
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
