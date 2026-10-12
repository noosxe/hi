use hi_core::{Message, ToolCall, ToolResult};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Borders, List, ListItem, ListState, Paragraph, Widget};
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

use crate::app::App;
use crate::markdown::markdown_rows;
use crate::theme::{Bg, Fg, THEME};

/// Render the managed region: streaming area, input, and the status
/// row. The transcript is NOT drawn here — it lives in the terminal's
/// scrollback (see `message_into`).
pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    // The `/resume` session picker takes over the live area — input,
    // menus, and stream all yield; the status row stays for context.
    if app.session_picker.is_some() {
        let picker_area = Rect {
            height: area.height.saturating_sub(1),
            ..area
        };
        draw_session_picker(frame, picker_area, app);
        draw_status(
            frame,
            Rect {
                y: area.y + area.height - 1,
                height: 1,
                ..area
            },
            app,
        );
        return;
    }
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
    let mut block = ratatui::widgets::Block::default().borders(Borders::TOP | Borders::BOTTOM);
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
            ratatui::widgets::Block::default()
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
            ratatui::widgets::Block::default()
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
    // A running tool replaces the streaming text with its live pending
    // block — pi inline: the pending execution occupies the live region
    // and lands in history complete, colored by its outcome.
    if let Some(exec) = pending_tool(&app.messages) {
        render_tail(
            &block_rows(&TranscriptBlock::Tool(exec), area.width),
            area,
            frame,
        );
        return;
    }
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
    let mut rows: Vec<Line<'static>> = Vec::new();
    if !turn.text.is_empty() {
        // pi streams markdown in place, undimmed.
        rows.extend(markdown_rows(&turn.text[start..], area.width, Style::new()));
    }
    if let Some(err) = &turn.error {
        rows.extend(markdown_rows(err, area.width, THEME.fg(Fg::Error)));
    }
    render_tail(&rows, area, frame);
}

/// Draw the last rows of pre-wrapped content into the live area.
fn render_tail(rows: &[Line<'static>], area: Rect, frame: &mut Frame) {
    let height = area.height as usize;
    let skip = rows.len().saturating_sub(height);
    let visible: Vec<Line<'static>> = rows.iter().skip(skip).take(height).cloned().collect();
    // No `Wrap`: the rows are already wrapped to the area's width.
    frame.render_widget(Paragraph::new(Text::from(visible)), area);
}

/// Tool result bodies preview this many source lines, then a marker
/// row. Without the cap, a full tool result (up to 2000 lines under
/// the truncation contract) would paste thousands of rows into
/// scrollback. Full output lives in the session file; expansion is a
/// fullscreen-mode feature hi hasn't built (pi's inline mode also
/// shows the collapsed preview).
const TOOL_PREVIEW_LINES: usize = 10;

/// Call arguments collapse to this many chars (pi's COLLAPSED_ARGS_CHARS).
const ARGS_PREVIEW_CHARS: usize = 100;

/// A transcript display unit. Messages render one way each; a tool
/// execution merges its call and result into one pi-style block.
#[derive(Clone, Debug)]
pub enum TranscriptBlock {
    Message(Message),
    Tool(ToolExec),
}

/// One tool execution: the call plus, once it finishes, its result. A
/// block with a call but no result is still running — the live pending
/// surface in the managed region.
#[derive(Clone, Debug)]
pub struct ToolExec {
    pub call: Option<ToolCall>,
    pub result: Option<ToolResult>,
}

/// Derive display blocks from the message stream. Calls pair with their
/// results (the agent loop guarantees call-then-result order); a
/// trailing unpaired call comes back separately as the live pending
/// execution instead of a block. Resumed sessions pair up the same way.
pub fn derive_blocks(messages: &[Message]) -> (Vec<TranscriptBlock>, Option<ToolExec>) {
    let mut blocks: Vec<TranscriptBlock> = Vec::new();
    let mut open: Option<ToolExec> = None;
    for message in messages {
        match message {
            Message::ToolCall(call) => {
                if let Some(exec) = open.take() {
                    // An abandoned call followed by a new one: flush it.
                    blocks.push(TranscriptBlock::Tool(exec));
                }
                open = Some(ToolExec {
                    call: Some(call.clone()),
                    result: None,
                });
            }
            Message::ToolResult(result) => {
                if let Some(mut exec) = open.take() {
                    exec.result = Some(result.clone());
                    blocks.push(TranscriptBlock::Tool(exec));
                } else {
                    // Orphan result (denied before the call surfaced, or
                    // a crash): self-contained — the result carries the
                    // tool's name and is_error.
                    blocks.push(TranscriptBlock::Tool(ToolExec {
                        call: None,
                        result: Some(result.clone()),
                    }));
                }
            }
            other => blocks.push(TranscriptBlock::Message(other.clone())),
        }
    }
    (blocks, open)
}

/// The still-running execution, if the transcript ends in an unpaired
/// call.
pub fn pending_tool(messages: &[Message]) -> Option<ToolExec> {
    derive_blocks(messages).1
}

/// Compose a transcript block into wrapped, styled rows — one `Line`
/// per scrollback row, in the pi-dark palette. Measuring and rendering
/// share this function, so the row count a caller passes to
/// `Terminal::insert_before` always matches what `rows_into` draws.
pub fn block_rows(block: &TranscriptBlock, width: u16) -> Vec<Line<'static>> {
    let width = width.max(1) as usize;
    match block {
        // Filled blocks pad in their own fill (padded_block / tool_rows).
        TranscriptBlock::Message(Message::User(text)) => padded_block(
            markdown_rows(text, width as u16, THEME.fg(Fg::UserText)),
            width,
            THEME.bg(Bg::UserMessage),
        ),
        // Transparent blocks pad with plain blank rows. Either way every
        // block carries one blank row above and below (pi's paddingY):
        // two blank rows separate any adjacent content rows, some tinted
        // by their block's background, some not.
        TranscriptBlock::Message(Message::Assistant(text)) => {
            // Plain markdown, terminal-default text, no fill (pi).
            pad_plain(markdown_rows(text, width as u16, Style::new()))
        }
        TranscriptBlock::Message(Message::System(text)) => {
            pad_plain(wrap_plain(text, width, THEME.fg(Fg::Dim)))
        }
        TranscriptBlock::Message(Message::Error(text)) => {
            pad_plain(wrap_plain(text, width, THEME.fg(Fg::Error)))
        }
        // derive_blocks routes tool messages to TranscriptBlock::Tool;
        // if one ever lands here, render defensively instead of panicking.
        TranscriptBlock::Message(Message::ToolCall(call)) => pad_plain(wrap_plain(
            &format!("{}({})", call.name, call.arguments),
            width,
            THEME.fg(Fg::Dim),
        )),
        TranscriptBlock::Message(Message::ToolResult(result)) => {
            pad_plain(wrap_plain(&result.content, width, THEME.fg(Fg::Dim)))
        }
        TranscriptBlock::Tool(exec) => tool_rows(exec, width),
    }
}

/// Render pre-wrapped transcript rows into a scrollback buffer. Called
/// via `Terminal::insert_before`; after this runs, the rows are
/// terminal-owned history — scrolling them is the emulator's job.
pub fn rows_into(rows: Vec<Line<'static>>, buf: &mut Buffer) {
    // The rows are already wrapped and styled to the buffer's width, so
    // this is a plain placement pass — no `Wrap`, no reflow.
    Paragraph::new(Text::from(rows)).render(buf.area, buf);
}

/// Plain-text wrap in one style (system and error lines are not
/// markdown; pi renders diagnostics as colored text, no labels).
fn wrap_plain(text: &str, width: usize, style: Style) -> Vec<Line<'static>> {
    let mut rows: Vec<Line<'static>> = Vec::new();
    for source in text.split('\n') {
        for piece in crate::wrap::wrap(source, width.max(1)) {
            rows.push(Line::from(Span::styled(piece, style)));
        }
    }
    if rows.is_empty() {
        rows.push(Line::from(Span::styled(String::new(), style)));
    }
    rows
}

/// Extend rows into a solid pi-style block: every span patched with the
/// fill (so every cell carries the background), each row padded to the
/// full width inside it, one blank fill row above and below (pi's
/// paddingY), one space of side padding (paddingX).
fn padded_block(rows: Vec<Line<'static>>, width: usize, fill: Style) -> Vec<Line<'static>> {
    let mut out = Vec::with_capacity(rows.len() + 2);
    out.push(fill_row(width, fill));
    for row in rows {
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(row.spans.len() + 2);
        spans.push(Span::styled(" ".to_string(), fill));
        spans.extend(row.spans);
        let used: usize = spans.iter().map(|s| s.content.width()).sum();
        if used < width {
            spans.push(Span::styled(" ".repeat(width - used), fill));
        }
        for span in &mut spans {
            span.style = span.style.patch(fill);
        }
        out.push(Line::from(spans));
    }
    out.push(fill_row(width, fill));
    out
}

/// One plain blank row above and below — the transparent block's
/// paddingY, mirroring what `padded_block` does with a fill.
fn pad_plain(rows: Vec<Line<'static>>) -> Vec<Line<'static>> {
    let mut out = Vec::with_capacity(rows.len() + 2);
    out.push(Line::raw(""));
    out.extend(rows);
    out.push(Line::raw(""));
    out
}

fn fill_row(width: usize, fill: Style) -> Line<'static> {
    Line::from(Span::styled(" ".repeat(width), fill))
}

/// Tool block: bold tool name with the collapsed args preview over the
/// output preview, in a fill tinted by execution state — pi's
/// ToolExecutionComponent: pending while running, then success or error.
fn tool_rows(exec: &ToolExec, width: usize) -> Vec<Line<'static>> {
    let fill = match &exec.result {
        None => THEME.bg(Bg::ToolPending),
        Some(result) if result.is_error => THEME.bg(Bg::ToolError),
        Some(_) => THEME.bg(Bg::ToolSuccess),
    };
    let name = exec
        .call
        .as_ref()
        .map(|call| call.name.clone())
        .or_else(|| exec.result.as_ref().map(|r| r.name.clone()))
        .unwrap_or_default();
    let mut title_segs = vec![crate::markdown::Seg::new(
        name,
        THEME.fg(Fg::ToolTitle).bold(),
    )];
    if let Some(args) = exec.call.as_ref().map(|call| args_preview(&call.arguments))
        && !args.is_empty()
    {
        title_segs.push(crate::markdown::Seg::new(
            format!(" {args}"),
            THEME.fg(Fg::Muted),
        ));
    }
    let inner_width = width.saturating_sub(2).max(1);
    let mut inner: Vec<Line<'static>> = crate::markdown::wrap_segments(&title_segs, inner_width)
        .into_iter()
        .map(crate::markdown::segs_into_line)
        .collect();
    if let Some(result) = &exec.result {
        let total = result.content.lines().count();
        for line in result.content.lines().take(TOOL_PREVIEW_LINES) {
            for piece in crate::wrap::wrap(line, inner_width) {
                inner.push(Line::from(Span::styled(piece, THEME.fg(Fg::ToolOutput))));
            }
        }
        if total > TOOL_PREVIEW_LINES {
            let hidden = total - TOOL_PREVIEW_LINES;
            inner.push(Line::from(Span::styled(
                format!("... ({hidden} more lines)"),
                THEME.fg(Fg::Muted),
            )));
        }
        inner.extend(image_notes(result));
    }
    padded_block(inner, width, fill)
}

/// One dim line per attached image: mime type plus decoded size. The
/// terminal renders no pixels — the kitty graphics protocol is issue
/// #42 — so this note is the honest placeholder.
fn image_notes(result: &ToolResult) -> Vec<Line<'static>> {
    result
        .images
        .iter()
        .map(|image| {
            let bytes = image.data.len() as u64 * 3 / 4;
            let size = if bytes < 1024 * 1024 {
                format!("{:.1} KB", bytes as f64 / 1024.0)
            } else {
                format!("{:.1} MB", bytes as f64 / 1024.0 / 1024.0)
            };
            Line::from(Span::styled(
                format!("· image: {}, {size}", image.mime_type),
                THEME.fg(Fg::Muted),
            ))
        })
        .collect()
}

/// Collapse call arguments to pi's `key=value key2=value2` preview,
/// truncated with `...` at [`ARGS_PREVIEW_CHARS`]. Malformed JSON falls
/// back to the trimmed raw text; empty arguments render no preview.
/// (pi preserves source order via `marked`; serde's maps sort keys — a
/// cosmetic divergence.)
fn args_preview(arguments: &str) -> String {
    let collapse = |text: String| -> String {
        if text.chars().count() > ARGS_PREVIEW_CHARS {
            let cut: String = text.chars().take(ARGS_PREVIEW_CHARS - 3).collect();
            format!("{cut}...")
        } else {
            text
        }
    };
    match serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(arguments) {
        Ok(map) if !map.is_empty() => {
            let joined = map
                .into_iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join(" ");
            collapse(joined)
        }
        _ => {
            let raw = arguments.trim().to_string();
            if raw.is_empty() {
                String::new()
            } else {
                collapse(raw)
            }
        }
    }
}

/// One-row welcome banner, printed above the viewport on startup.
/// Doubles as visual separation from whatever the terminal was
/// showing before the app launched.
pub fn welcome_into(buf: &mut Buffer) {
    let line = Line::from(vec!["hi".bold(), " · say something, ctrl-c to quit".dim()]);
    Paragraph::new(line).render(buf.area, buf);
}

/// The `/resume` chooser: title, the visible session rows around the
/// selection, the filter line, and the key hint. Composition is
/// shared with the standalone `--resume` picker (`picker_lines`), so
/// both look — and test — the same.
fn draw_session_picker(frame: &mut Frame, area: Rect, app: &App) {
    let Some(picker) = &app.session_picker else {
        return;
    };
    let lines = crate::picker::picker_lines(
        &picker.summaries,
        &picker.previews,
        &picker.filter,
        picker.selected,
        area.height as usize,
        std::time::SystemTime::now(),
    );
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;

    /// All cell symbols of one buffer row, trailing blanks trimmed.
    fn text_of(buf: &Buffer, y: u16, width: u16) -> String {
        (0..width)
            .map(|x| buf[(x, y)].symbol().to_string())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    fn tool_call(args: &str) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            arguments: args.into(),
        }
    }

    #[test]
    fn image_attachments_render_as_dim_notes() {
        // No pixels in a terminal (#42) — the note is the placeholder.
        let mut result = tool_result("Read image file [image/png] (attached)", false);
        result.images.push(hi_core::tool::ImageBlock {
            data: "aW1n".into(),
            mime_type: "image/png".into(),
        });
        let exec = ToolExec {
            call: None,
            result: Some(result),
        };
        let rows = tool_rows(&exec, 60);
        let text: Vec<String> = rows
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.to_string())
                    .collect()
            })
            .collect();
        // `aW1n` decodes to 3 bytes — the size renders honest and small.
        assert!(
            text.iter()
                .any(|t| t.contains("· image: image/png, ") && t.contains(" KB")),
            "rows: {text:?}"
        );
    }

    fn tool_result(content: &str, is_error: bool) -> ToolResult {
        ToolResult {
            call_id: "c1".into(),
            name: "bash".into(),
            content: content.into(),
            is_error,
            images: Vec::new(),
        }
    }

    #[test]
    fn user_block_fills_its_background() {
        let rows = block_rows(
            &TranscriptBlock::Message(Message::User("hi there".into())),
            20,
        );
        // paddingY: one fill row above and below the content.
        assert_eq!(rows.len(), 3);
        let buf = &mut Buffer::empty(Rect::new(0, 0, 20, 3));
        rows_into(rows, buf);
        let fill = THEME.bg(Bg::UserMessage).bg.unwrap();
        for x in 0..20 {
            // Top and bottom rows are solid fill; the content row carries
            // the fill in its padding columns too.
            assert_eq!(buf[(x, 0)].bg, fill, "row 0 col {x}");
            assert_eq!(buf[(x, 2)].bg, fill, "row 2 col {x}");
            assert_eq!(buf[(x, 1)].bg, fill, "row 1 col {x}");
        }
        assert_eq!(text_of(buf, 1, 20), " hi there");
    }

    #[test]
    fn assistant_block_has_no_fill_and_its_own_padding() {
        let rows = block_rows(
            &TranscriptBlock::Message(Message::Assistant("plain reply".into())),
            30,
        );
        // paddingY: one plain blank row above and below the content.
        assert_eq!(rows.len(), 3);
        let buf = &mut Buffer::empty(Rect::new(0, 0, 30, 3));
        rows_into(rows, buf);
        assert_eq!(text_of(buf, 0, 30), "");
        assert_eq!(text_of(buf, 1, 30), "plain reply");
        assert_eq!(text_of(buf, 2, 30), "");
        for y in 0..3 {
            assert_eq!(buf[(0, y)].bg, ratatui::style::Color::Reset);
        }
    }

    #[test]
    fn assistant_markdown_renders_headings() {
        let rows = block_rows(
            &TranscriptBlock::Message(Message::Assistant("# Plan".into())),
            30,
        );
        assert!(
            rows[1].spans[0]
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
    }

    #[test]
    fn system_and_error_render_without_labels() {
        let system = block_rows(
            &TranscriptBlock::Message(Message::System("note".into())),
            20,
        );
        assert_eq!(system.len(), 3); // paddingY rows flank the content
        assert_eq!(system[1].spans[0].style.fg, THEME.fg(Fg::Dim).fg);
        let error = block_rows(&TranscriptBlock::Message(Message::Error("boom".into())), 20);
        assert_eq!(error.len(), 3);
        assert_eq!(error[1].spans[0].style.fg, THEME.fg(Fg::Error).fg);
    }

    #[test]
    fn calls_pair_with_results_in_order() {
        let messages = vec![
            Message::User("run it".into()),
            Message::ToolCall(tool_call("{}")),
            Message::ToolResult(tool_result("out", false)),
        ];
        let (blocks, pending) = derive_blocks(&messages);
        assert!(pending.is_none());
        assert_eq!(blocks.len(), 2);
        assert!(matches!(
            blocks[0],
            TranscriptBlock::Message(Message::User(_))
        ));
        match &blocks[1] {
            TranscriptBlock::Tool(exec) => {
                assert_eq!(exec.call.as_ref().unwrap().name, "bash");
                assert_eq!(exec.result.as_ref().unwrap().content, "out");
            }
            _ => panic!("expected tool block"),
        }
    }

    #[test]
    fn trailing_unpaired_call_is_the_pending_execution() {
        let messages = vec![Message::ToolCall(tool_call("{}"))];
        let (blocks, pending) = derive_blocks(&messages);
        assert!(blocks.is_empty());
        let pending = pending.unwrap();
        assert_eq!(pending.call.unwrap().name, "bash");
        assert!(pending.result.is_none());
    }

    #[test]
    fn orphan_result_renders_self_contained() {
        let (blocks, pending) = derive_blocks(&[Message::ToolResult(tool_result("nope", true))]);
        assert!(pending.is_none());
        match &blocks[0] {
            TranscriptBlock::Tool(exec) => {
                assert!(exec.call.is_none());
                assert!(exec.result.as_ref().unwrap().is_error);
            }
            _ => panic!("expected tool block"),
        }
    }

    #[test]
    fn tool_state_selects_the_fill() {
        let pending = block_rows(
            &TranscriptBlock::Tool(ToolExec {
                call: Some(tool_call("{}")),
                result: None,
            }),
            20,
        );
        assert_eq!(pending[0].spans[0].style.bg, THEME.bg(Bg::ToolPending).bg);
        let success = block_rows(
            &TranscriptBlock::Tool(ToolExec {
                call: Some(tool_call("{}")),
                result: Some(tool_result("ok", false)),
            }),
            20,
        );
        assert_eq!(success[0].spans[0].style.bg, THEME.bg(Bg::ToolSuccess).bg);
        let failure = block_rows(
            &TranscriptBlock::Tool(ToolExec {
                call: Some(tool_call("{}")),
                result: Some(tool_result("bad", true)),
            }),
            20,
        );
        assert_eq!(failure[0].spans[0].style.bg, THEME.bg(Bg::ToolError).bg);
    }

    #[test]
    fn tool_title_carries_name_and_collapsed_args() {
        let rows = block_rows(
            &TranscriptBlock::Tool(ToolExec {
                call: Some(tool_call("{\"cmd\":\"ls -la\"}")),
                result: Some(tool_result("file.txt", false)),
            }),
            40,
        );
        // paddingY row, title row, body row, paddingY row.
        assert_eq!(rows.len(), 4);
        let title: String = rows[1]
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert_eq!(title.trim(), "bash cmd=\"ls -la\"");
        let body: String = rows[2]
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert_eq!(body.trim(), "file.txt");
    }

    #[test]
    fn long_args_collapse_with_ellipsis() {
        let long = "x".repeat(300);
        let preview = args_preview(&format!("{{\"path\":\"{long}\"}}"));
        assert_eq!(preview.chars().count(), ARGS_PREVIEW_CHARS);
        assert!(preview.ends_with("..."));
        assert_eq!(args_preview(""), "");
    }

    #[test]
    fn tool_result_previews_ten_lines_with_marker() {
        let content: String = (0..50).map(|i| format!("line{i}\n")).collect();
        let rows = block_rows(
            &TranscriptBlock::Tool(ToolExec {
                call: Some(tool_call("{}")),
                result: Some(tool_result(&content, false)),
            }),
            40,
        );
        // pad, title, 10 preview lines, marker, pad.
        assert_eq!(rows.len(), 1 + 1 + TOOL_PREVIEW_LINES + 1 + 1);
        let marker: String = rows[rows.len() - 2]
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(marker.contains("... (40 more lines)"), "marker: {marker}");
    }

    #[test]
    fn degenerate_width_yields_rows() {
        let rows = block_rows(&TranscriptBlock::Message(Message::User("hello".into())), 0);
        assert!(!rows.is_empty());
    }
}
