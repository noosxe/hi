use hi_core::Message;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Widget, Wrap};
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

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
    let input_rows =
        crate::wrap::wrapped_row_count(&app.input, area.width as usize).clamp(1, 6) as u16;
    let input_height = input_rows + 2; // rules
    let menu_h = app.menu_height().min(9 - input_height);
    let stream_h = 11 - input_height - menu_h;
    let stream_area = Rect {
        height: stream_h,
        ..area
    };
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
        .block(
            Block::new()
                .borders(Borders::ALL)
                .title(format!(" /{} ", app.input.strip_prefix('/').unwrap_or(""))),
        )
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

/// Tool-call and tool-result bodies are capped to this many wrapped
/// rows plus a marker row. Without the cap, a full tool result (up to
/// 2000 lines under the truncation contract) would paste thousands of
/// rows into scrollback. The full output still lives in the session
/// file; #33 replaces this with expandable blocks.
const TOOL_PREVIEW_ROWS: usize = 10;

/// Compose a transcript message into wrapped terminal rows — one
/// `Line` per scrollback row. The label sits on the first row and
/// shrinks that row's wrap capacity; continuation rows get the full
/// width. Measuring and rendering share this function, so the row
/// count a caller passes to `Terminal::insert_before` always matches
/// what `rows_into` draws.
pub fn message_rows(msg: Message, width: u16) -> Vec<Line<'static>> {
    let width = width.max(1) as usize;
    match msg {
        Message::User(text) => labeled_rows("you ", Style::new().dim(), text, Style::new(), width),
        Message::Assistant(text) => {
            labeled_rows("assistant ", Style::new().dim(), text, Style::new(), width)
        }
        Message::Error(text) => {
            labeled_rows("error ", Style::new().red(), text, Style::new(), width)
        }
        Message::System(text) => labeled_rows("", Style::new(), text, Style::new().dim(), width),
        Message::ToolCall(call) => {
            let body = format!("{}({})", call.name, call.arguments);
            let mut rows = labeled_rows(
                "tool ",
                Style::new().dim(),
                body,
                Style::new().yellow(),
                width,
            );
            cap_tool_rows(&mut rows);
            rows
        }
        Message::ToolResult(result) => {
            let label = if result.is_error {
                Style::new().red()
            } else {
                Style::new().dim()
            };
            let mut rows = labeled_rows("result ", label, result.content, Style::new(), width);
            cap_tool_rows(&mut rows);
            rows
        }
    }
}

/// Render pre-wrapped transcript rows into a scrollback buffer. Called
/// via `Terminal::insert_before`; after this runs, the rows are
/// terminal-owned history — scrolling them is the emulator's job.
pub fn rows_into(rows: Vec<Line<'static>>, buf: &mut Buffer) {
    // The rows are already wrapped to the buffer's width, so this is a
    // plain placement pass — no `Wrap`, no reflow, no surprises.
    Paragraph::new(Text::from(rows)).render(buf.area, buf);
}

/// Word-wrap `body` into row strings: the first row reserves the
/// label's columns, later rows the full width. Source newlines split
/// first, so a message's own line breaks survive wrapping.
fn wrap_body(label: &str, body: &str, width: usize) -> Vec<String> {
    let first_cap = width.saturating_sub(label.width()).max(1);
    let mut rows: Vec<String> = Vec::new();
    for segment in body.split('\n') {
        let cap = if rows.is_empty() { first_cap } else { width };
        rows.extend(crate::wrap::wrap(segment, cap));
    }
    rows
}

/// Build message rows: the label prefixes the first row, the wrapped
/// body follows in `body_style`.
fn labeled_rows(
    label: &'static str,
    label_style: Style,
    body: String,
    body_style: Style,
    width: usize,
) -> Vec<Line<'static>> {
    wrap_body(label, &body, width)
        .into_iter()
        .enumerate()
        .map(|(i, row)| {
            let mut spans = Vec::with_capacity(2);
            if i == 0 && !label.is_empty() {
                spans.push(Span::styled(label.to_string(), label_style));
            }
            spans.push(Span::styled(row, body_style));
            Line::from(spans)
        })
        .collect()
}

/// Keep tool rows to the preview budget, collapsing the rest into one
/// dim marker row — pi's collapsed-preview pattern for scrollback.
fn cap_tool_rows(rows: &mut Vec<Line<'static>>) {
    if rows.len() <= TOOL_PREVIEW_ROWS {
        return;
    }
    let hidden = rows.len() - TOOL_PREVIEW_ROWS;
    rows.truncate(TOOL_PREVIEW_ROWS);
    rows.push(Line::from(format!("\u{2026} {hidden} more lines").dim()));
}

/// One-row welcome banner, printed above the viewport on startup.
/// Doubles as visual separation from whatever the terminal was
/// showing before the app launched.
pub fn welcome_into(buf: &mut Buffer) {
    let line = Line::from(vec!["hi".bold(), " · say something, ctrl-c to quit".dim()]);
    Paragraph::new(line).render(buf.area, buf);
}

#[cfg(test)]
mod tests {
    use super::*;
    use hi_core::ToolResult;

    /// All cell symbols of one buffer row, trailing blanks trimmed.
    fn text_of(buf: &Buffer, y: u16, width: u16) -> String {
        (0..width)
            .map(|x| buf[(x, y)].symbol().to_string())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn long_reply_wraps_at_word_boundaries() {
        // 10 label columns + 12 wrap columns: "alpha beta " fits, "gamma" wraps.
        let width = 22;
        let rows = message_rows(Message::Assistant("alpha beta gamma".into()), width);
        assert_eq!(rows.len(), 2);
        let buf = &mut Buffer::empty(Rect::new(0, 0, width, rows.len() as u16));
        rows_into(rows, buf);
        assert_eq!(text_of(buf, 0, width), "assistant alpha beta");
        assert_eq!(text_of(buf, 1, width), "gamma");
    }

    #[test]
    fn continuation_rows_have_no_label() {
        let width = 20;
        let rows = message_rows(
            Message::User("one two three four five six seven".into()),
            width,
        );
        assert!(rows.len() > 1);
        let buf = &mut Buffer::empty(Rect::new(0, 0, width, rows.len() as u16));
        rows_into(rows, buf);
        assert!(text_of(buf, 0, width).starts_with("you "));
        assert!(!text_of(buf, 1, width).starts_with("you "));
    }

    #[test]
    fn source_newlines_become_row_breaks() {
        let rows = message_rows(Message::User("a\nb".into()), 40);
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn oversized_words_hard_split() {
        let width = 10;
        let rows = message_rows(Message::User("abcdefgh".into()), width);
        assert_eq!(rows.len(), 2);
        let buf = &mut Buffer::empty(Rect::new(0, 0, width, 2));
        rows_into(rows, buf);
        assert_eq!(text_of(buf, 0, width), "you abcdef");
        assert_eq!(text_of(buf, 1, width), "gh");
    }

    #[test]
    fn tool_result_is_capped_with_a_marker_row() {
        let content: String = (0..50).map(|i| format!("line{i}\n")).collect();
        let rows = message_rows(
            Message::ToolResult(ToolResult {
                call_id: "c1".into(),
                name: "bash".into(),
                content,
                is_error: false,
            }),
            40,
        );
        assert_eq!(rows.len(), TOOL_PREVIEW_ROWS + 1);
        let height = rows.len() as u16;
        let buf = &mut Buffer::empty(Rect::new(0, 0, 40, height));
        rows_into(rows, buf);
        let marker = text_of(buf, height - 1, 40);
        assert!(marker.contains("41 more lines"), "marker: {marker}");
    }

    #[test]
    fn error_result_keeps_red_label() {
        let rows = message_rows(
            Message::ToolResult(ToolResult {
                call_id: "c2".into(),
                name: "bash".into(),
                content: "boom".into(),
                is_error: true,
            }),
            40,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].spans[0].style, Style::new().red());
    }

    #[test]
    fn system_rows_have_no_label() {
        let rows = message_rows(Message::System("note".into()), 40);
        assert_eq!(rows.len(), 1);
        let buf = &mut Buffer::empty(Rect::new(0, 0, 40, 1));
        rows_into(rows, buf);
        assert_eq!(text_of(buf, 0, 40), "note");
    }

    #[test]
    fn degenerate_width_yields_one_row() {
        assert_eq!(message_rows(Message::Assistant(String::new()), 0).len(), 1);
    }
}
