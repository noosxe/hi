use hi_core::{Message, ToolCall, ToolResult};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Borders, Paragraph, Widget};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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
        let status_h = app.status_height();
        let picker_area = Rect {
            height: area.height.saturating_sub(status_h),
            ..area
        };
        draw_session_picker(frame, picker_area, app);
        draw_status(
            frame,
            Rect {
                y: area.y + area.height - status_h,
                height: status_h,
                ..area
            },
            app,
        );
        return;
    }
    // The live viewport is the whole terminal (AD-0015/AD-0016),
    // bottom-up: the input grows upward with its wrapped text (4 rows
    // resting, up to 9 — one breathing row rides on top, AD-0020); an
    // open menu renders UNDER the input —
    // pi-style, taking the bottom edge while the status row yields
    // for its brief lifetime (ruled: menus outrank status); and the
    // stream area keeps every row above it: the transcript window
    // (AD-0016), then live content, flowing down to the input. Never
    // a blank band.
    //
    //   50 rows, resting:   [stream 45] [input 4] [status 1]
    //   50 rows, menu open: [stream 43] [input 4] [menu 3]
    let input_height = input_height(app, area.width);
    let menu_h = app.menu_height();
    let (stream_area, menu_area, input_area, status_area) =
        layout(area, input_height, menu_h, app.status_height());
    draw_stream(frame, stream_area, app);
    draw_input(frame, input_area, app);
    if menu_h > 0 {
        if app.tasks_view.is_some() {
            draw_tasks(frame, menu_area, app);
        } else if app.picker.is_some() {
            draw_picker(frame, menu_area, app);
        } else if app.settings_board.is_some() {
            draw_settings(frame, menu_area, app);
        } else {
            draw_completion(frame, menu_area, app);
        }
    } else {
        draw_status(frame, status_area, app);
    }
}

/// The `/tasks` pane (AD-0021): the list as a SelectList with state
/// descriptions, or one task's output watched live. Both read the
/// manager fresh every frame — the frame clock is the poll; nothing
/// caches, nothing blocks.
fn draw_tasks(frame: &mut Frame, area: Rect, app: &App) {
    let Some(view) = app.tasks_view.as_ref() else {
        return;
    };
    let Some(manager) = app.tasks.as_ref() else {
        return;
    };
    match view {
        crate::app::TasksView::List { selected } => {
            let summaries = manager.list();
            let items: Vec<(String, String)> = summaries
                .iter()
                .map(|summary| {
                    (
                        format!("#{} {}", summary.id, summary.name),
                        format!(
                            "{} · {}",
                            summary.state.label(),
                            hi_core::tasks::format_duration(summary.wall)
                        ),
                    )
                })
                .collect();
            let mut rows = if items.is_empty() {
                vec![Line::from(Span::styled(
                    "no background tasks",
                    THEME.fg(Fg::Muted),
                ))]
            } else {
                let refs: Vec<(&str, &str)> = items
                    .iter()
                    .map(|(name, description)| (name.as_str(), description.as_str()))
                    .collect();
                select_list_rows(refs, *selected, area.width as usize)
            };
            rows.push(Line::from(Span::styled(
                "enter output · esc close",
                THEME.fg(Fg::Dim),
            )));
            frame.render_widget(Paragraph::new(Text::from(rows)), area);
        }
        crate::app::TasksView::Output { id } => {
            let summary = manager.status(*id);
            let finished = summary.as_ref().is_some_and(|s| s.state.finished());
            let header = match &summary {
                Some(summary) => format!(
                    "#{} '{}' — {} · {}",
                    summary.id,
                    summary.name,
                    summary.state.label(),
                    hi_core::tasks::format_duration(summary.wall)
                ),
                None => format!("#{id}"),
            };
            let content_rows = area.height.saturating_sub(2) as usize; // header + footer
            // The tail's newest rows win: the pane bottom-anchors like
            // the stream, and a longer tail trims from the top.
            let tail = manager.output_tail(*id, 8192).unwrap_or_default();
            let tail_lines: Vec<&str> = tail.lines().collect();
            let start = tail_lines.len().saturating_sub(content_rows);
            let mut rows = Vec::with_capacity(content_rows + 2);
            rows.push(Line::from(Span::styled(
                header,
                THEME.fg(if finished { Fg::Muted } else { Fg::Accent }),
            )));
            rows.extend(
                tail_lines[start..]
                    .iter()
                    .map(|line| Line::from(line.to_string())),
            );
            rows.push(Line::from(Span::styled("esc back", THEME.fg(Fg::Dim))));
            frame.render_widget(Paragraph::new(Text::from(rows)), area);
        }
    }
}

/// The input box's row count at this width — wrapped rows (1–6), its
/// two rules, and one breathing row above the box (AD-0020): the gap
/// between transcript and composer is part of the input's footprint,
/// so draw and the flush pass measure it with one formula.
pub fn input_height(app: &App, width: u16) -> u16 {
    let rows = crate::wrap::wrapped_row_count(&app.input, width.max(1) as usize).clamp(1, 6);
    rows as u16 + 3
}

/// How many rows the stream window gets at this frame's geometry —
/// the flush pass targets exactly this many live rows.
pub fn stream_window_height(height: u16, input_height: u16, menu_h: u16, status_h: u16) -> u16 {
    layout(Rect::new(0, 0, 1, height), input_height, menu_h, status_h)
        .0
        .height
}

/// The live-area partition (AD-0015/AD-0016), bottom-up: the input
/// sits above the bottom edge; an open menu renders UNDER the input
/// and takes the bottom edge for its brief lifetime — the status row
/// yields while it is open (ruled: menus outrank status). The stream
/// keeps every row from the top. Saturating by construction — each
/// consumer subtracts from the one below it, so on absurdly short
/// terminals the stream shrinks first, to nothing, and nothing
/// panics. The scrollback above the viewport is the flushed prefix
/// of the transcript; the stream area is its window — never blank.
fn layout(area: Rect, input_height: u16, menu_h: u16, status_h: u16) -> (Rect, Rect, Rect, Rect) {
    let bottom_row = area.bottom().saturating_sub(1);
    // The bottommost consumer is the open menu, or — when no menu is
    // open — the status board. Exactly one of them owns the edge.
    let edge_h = if menu_h > 0 { menu_h } else { status_h };
    let edge_y = bottom_row.saturating_sub(edge_h.saturating_sub(1));
    let input_area = Rect {
        y: edge_y.saturating_sub(input_height),
        height: input_height,
        ..area
    };
    let menu_area = Rect {
        y: input_area.y.saturating_sub(0) + input_height,
        height: menu_h,
        ..area
    };
    let status_area = Rect {
        y: bottom_row.saturating_sub(status_h.saturating_sub(1)),
        height: if menu_h == 0 { status_h } else { 0 },
        ..area
    };
    let stream_area = Rect {
        height: input_area.y.saturating_sub(area.y),
        ..area
    };
    (stream_area, menu_area, input_area, status_area)
}

/// The input area: top/bottom rules only, no side walls — pi-style.
/// Shows whatever is currently being typed, with the real terminal
/// cursor parked right after the text.
fn draw_input(frame: &mut Frame, area: Rect, app: &App) {
    // The footprint's first row is the margin (AD-0020) — owned and
    // cleared, so transcript content that scrolled through it never
    // lingers above the box.
    frame.render_widget(ratatui::widgets::Clear, Rect { height: 1, ..area });
    let area = Rect {
        y: area.y + 1,
        height: area.height.saturating_sub(1),
        ..area
    };
    let mut block = ratatui::widgets::Block::default().borders(Borders::TOP | Borders::BOTTOM);
    // While work is in flight, the top rule carries a right-aligned
    // activity title; quiet sessions render a plain rule.
    if let Some(note) = &app.note {
        // Transient paste note (AD-0011): left-aligned on the same
        // rule, error-tinted — the honest red a failed clipboard read
        // deserves, gone at the next keypress.
        block = block.title_top(
            Line::from(vec![
                Span::from(" "),
                Span::from(format!("· {note}")).style(THEME.fg(Fg::Error)),
                Span::from(" "),
                Span::from("──"),
            ])
            .left_aligned(),
        );
    }
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
    // real terminal caret at the logical cursor (AD-0014) — mid-text
    // while editing, not just after the last cell. Widths are display
    // columns — emoji count double.
    let starts = crate::wrap::wrap_row_starts(&app.input, inner.width as usize);
    let ends = starts.iter().copied().skip(1).chain([app.input.len()]);
    let rows: Vec<&str> = starts
        .iter()
        .copied()
        .zip(ends)
        // The newline that closed a line rides at the end of its
        // last row's byte span — it is the break, not a glyph.
        .map(|(s, e)| &app.input[s..e])
        .map(|row| row.strip_suffix('\n').unwrap_or(row))
        .collect();
    let (cursor_row, cursor_col) =
        crate::wrap::cursor_pos(&app.input, app.cursor, inner.width as usize);
    let text: Text = rows.iter().map(|row| Line::from(*row)).collect();
    frame.render_widget(Paragraph::new(text).block(block), area);
    frame.set_cursor_position(Position::new(
        inner.x + cursor_col as u16,
        inner.y + cursor_row as u16,
    ));
}

/// The status board (AD-0025): `status_rows` returns rows bottom-up
/// (row 0 is the classic bar — cwd (branch) left, (provider) model
/// right); earned rows stack above it. All dim — decoration is never
/// the star of the frame.
fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    let rows: Vec<Line<'static>> = app
        .status_rows()
        .into_iter()
        .rev()
        .map(|(left, right)| Line::from(crate::status::compose_status(&left, &right, area.width)))
        .collect();
    frame.render_widget(Paragraph::new(rows).style(Style::new().dim()), area);
}

/// The `/model` picker, drawn in the menu zone directly above the
/// input. Five rows for the common case (a handful of models); the
/// `List` scrolls when a server offers more.
fn draw_picker(frame: &mut Frame, area: Rect, app: &App) {
    let Some(picker) = app.picker.as_ref() else {
        return;
    };
    // The same SelectList as the slash menu (AD: deprecated box): the
    // bare model rows carry no descriptions, so this is the
    // name-only path; the key hints the box title used to carry
    // become a muted footer row.
    let items: Vec<(&str, &str)> = picker
        .items
        .iter()
        .map(|item| (item.as_str(), ""))
        .collect();
    let mut rows = select_list_rows(items, picker.selected, area.width as usize);
    if picker.items.is_empty() {
        rows.push(Line::from(Span::styled(
            "no models available",
            THEME.fg(Fg::Muted),
        )));
    }
    rows.push(Line::from(Span::styled(
        "enter use · ctrl-s default · esc close",
        THEME.fg(Fg::Dim),
    )));
    frame.render_widget(Paragraph::new(Text::from(rows)), area);
}

/// The `/settings` board (AD-0023), drawn in the menu zone: the
/// curated policy rows with their live `on|off` state, plus the
/// key-hint footer. Reads `settings_rows()` fresh each frame, so an
/// Enter flip is visible immediately — the board stays open.
fn draw_settings(frame: &mut Frame, area: Rect, app: &App) {
    if app.settings_board.is_none() {
        return;
    }
    let rows = app.settings_rows();
    let items: Vec<(&str, &str)> = rows.iter().map(|row| (row.as_str(), "")).collect();
    let selected = app
        .settings_board
        .as_ref()
        .map(|board| board.selected)
        .unwrap_or(0);
    let mut lines = select_list_rows(items, selected, area.width as usize);
    lines.push(Line::from(Span::styled(
        "enter toggle · ctrl-s save · esc close",
        THEME.fg(Fg::Dim),
    )));
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

/// The `/` completion menu, drawn in the menu zone between the
/// streaming area and the input. Echoes the typed text in its title;
/// commands grow over time, and the list scrolls.
fn draw_completion(frame: &mut Frame, area: Rect, app: &App) {
    let Some(completion) = app.completion.as_ref() else {
        return;
    };
    let items: Vec<(&str, &str)> = completion
        .items
        .iter()
        .map(|(name, description)| (*name, *description))
        .collect();
    let rows = select_list_rows(items, completion.selected, area.width as usize);
    frame.render_widget(Paragraph::new(Text::from(rows)), area);
}

/// pi's SelectList row set (select-list.js, the one renderer behind
/// every hi menu): plain rows, `→ ` on the selected row, a primary
/// column sized to the widest item (clamped 12–32) with an optional
/// muted description in a second column, and a `(n/m)` count row
/// whenever the items overflow the five visible slots. The visible
/// window is centered on the selection; the selected row colors
/// whole — accent, pi's selectedText — while unselected descriptions
/// ride muted. Descriptions render only with room to spare: a wide
/// terminal and at least MIN_DESCRIPTION_WIDTH columns left.
fn select_list_rows<'a>(
    items: impl IntoIterator<Item = (&'a str, &'a str)>,
    selected: usize,
    width: usize,
) -> Vec<Line<'static>> {
    const VISIBLE: usize = 5;
    let items: Vec<(&str, &str)> = items.into_iter().collect();
    let total = items.len();
    let start = selected
        .saturating_sub(VISIBLE / 2)
        .min(total.saturating_sub(VISIBLE));
    let end = (start + VISIBLE).min(total);
    let widest = items
        .iter()
        .map(|(name, _)| name.width() + 2) // PRIMARY_COLUMN_GAP
        .max()
        .unwrap_or(0);
    let column = (widest as u16).clamp(12, 32) as usize;
    let desc_start = 2 + column;
    let desc_width = width.saturating_sub(desc_start + 2);
    let mut rows: Vec<Line<'static>> = Vec::new();
    for (index, (name, description)) in items.iter().enumerate().skip(start).take(end - start) {
        let chosen = index == selected;
        let prefix = if chosen { "→ " } else { "  " };
        let mut spans: Vec<Span<'static>> = vec![Span::raw(prefix.to_string())];
        if width > 40 && desc_width > 10 && !description.is_empty() {
            let shown = truncate_cols(name, column - 2);
            let gap = " ".repeat(column - 2 - shown.width());
            spans.push(Span::raw(shown.to_string()));
            spans.push(Span::raw(gap));
            spans.push(Span::styled(
                truncate_cols(description, desc_width).to_string(),
                THEME.fg(Fg::Muted),
            ));
        } else {
            spans.push(Span::raw(
                truncate_cols(name, width.saturating_sub(2)).to_string(),
            ));
        }
        let mut line = Line::from(spans);
        if chosen {
            line = line.style(THEME.fg(Fg::Accent));
        }
        rows.push(line);
    }
    if total > VISIBLE {
        let info = format!("  ({}/{total})", selected + 1);
        rows.push(Line::from(Span::styled(
            truncate_cols(&info, width.saturating_sub(2)).to_string(),
            THEME.fg(Fg::Muted),
        )));
    }
    rows
}

/// Cut `text` to at most `max` display columns (emoji are two), on a
/// char boundary — pi's `truncateToWidth`.
fn truncate_cols(text: &str, max: usize) -> &str {
    let mut used = 0usize;
    for (index, ch) in text.char_indices() {
        let w = ch.width().unwrap_or(0);
        if used + w > max {
            return &text[..index];
        }
        used += w;
    }
    text
}
/// The in-progress reply, rendered live in the viewport, dimmed to
/// signal it is not final. Only the tail fits in the elastic stream
/// rows — the complete text is printed to scrollback when the turn
/// ends.
fn draw_stream(frame: &mut Frame, area: Rect, app: &App) {
    // The window (AD-0016): the unflushed tail of the transcript —
    // the conversation flows straight down to the input, and the
    // scrollback above the viewport is its flushed prefix. Live
    // content continues below it: a running tool's pending block
    // replaces the streaming text (pi inline: the pending execution
    // occupies the live region and lands in history complete), then
    // the turn's streaming markdown, undimmed.
    let mut rows = app.scroll.window_rows();
    if let Some(exec) = pending_tool(&app.messages) {
        rows.extend(block_rows(&TranscriptBlock::Tool(exec), area.width));
    } else if let Some(turn) = app.turn.as_ref() {
        // The live reply renders with the exact padding and wrap width
        // its flushed self will get (AD-0024): the same pad_plain and
        // the same `width - 2` the assistant block uses — completing a
        // turn is visually a no-op, not a reflow jump.
        let inner = area.width.saturating_sub(2).max(1);
        if !turn.text.is_empty() {
            rows.extend(pad_plain(markdown_rows(&turn.text, inner, Style::new())));
        }
        if let Some(err) = &turn.error {
            rows.extend(pad_plain(markdown_rows(err, inner, THEME.fg(Fg::Error))));
        }
    }
    // The newest rows win, and they hug the input: a short
    // conversation sits directly above the composer, not floating at
    // the top of the window — pi bottom-anchors its live region too.
    let visible = rows.len().min(area.height as usize);
    let anchored = Rect {
        y: area.y + area.height - visible as u16,
        height: visible as u16,
        ..area
    };
    render_tail(&rows, anchored, frame);
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
/// `Terminal::insert_before` always matches what `segments_into` draws.
pub fn block_rows(block: &TranscriptBlock, width: u16) -> Vec<Line<'static>> {
    text_only(block_segments(block, width, None))
}

/// Flatten segments to their text rows, dropping image segments. Used
/// by the managed region (which cannot host images — it re-renders
/// every frame) and by the no-protocol path, where `block_segments`
/// emits no image segments at all.
fn text_only(segments: Vec<Segment>) -> Vec<Line<'static>> {
    segments
        .into_iter()
        .flat_map(|segment| match segment {
            Segment::Text(rows) => rows,
            Segment::Image { .. } => Vec::new(),
        })
        .collect()
}

/// One printed chunk of a transcript block. Text rows flow through the
/// ordinary Paragraph path; an image is a raw kitty sequence plus the
/// number of scrollback rows it overlays.
#[derive(Debug, Clone)]
pub enum Segment {
    Text(Vec<Line<'static>>),
    Image { sequence: String, rows: u16 },
}

/// Build the printable segments of a block. `caps` is the terminal's
/// image protocol: `Some` lets `read`-attached images render as real
/// graphics (falling back to the text note per corrupt image), `None`
/// reproduces the text-only transcript exactly.
pub fn block_segments(
    block: &TranscriptBlock,
    width: u16,
    caps: Option<crate::kitty::Protocol>,
) -> Vec<Segment> {
    let width = width.max(1) as usize;
    // paddingX: every block's content is inset one column on each side
    // (ruled: mirror the paddingY rows — fill-tinted for filled blocks,
    // whose fill still runs the full width; plain for transparent ones).
    let inner = width.saturating_sub(2).max(1);
    match block {
        // Filled blocks pad in their own fill (padded_block / tool_rows).
        TranscriptBlock::Message(Message::User(text)) => vec![Segment::Text(padded_block(
            markdown_rows(text, inner as u16, THEME.fg(Fg::UserText)),
            width,
            THEME.bg(Bg::UserMessage),
        ))],
        // Transparent blocks pad with plain blank rows. Either way every
        // block carries one blank row above and below (pi's paddingY)
        // and one blank column each side (paddingX): two blank rows
        // separate any adjacent content rows, some tinted by their
        // block's background, some not.
        TranscriptBlock::Message(Message::Assistant(text)) => {
            // Plain markdown, terminal-default text, no fill (pi).
            vec![Segment::Text(pad_plain(markdown_rows(
                text,
                inner as u16,
                Style::new(),
            )))]
        }
        TranscriptBlock::Message(Message::System(text)) => vec![Segment::Text(pad_plain(
            wrap_plain(text, inner, THEME.fg(Fg::Dim)),
        ))],
        TranscriptBlock::Message(Message::Error(text)) => vec![Segment::Text(pad_plain(
            wrap_plain(text, inner, THEME.fg(Fg::Error)),
        ))],
        // derive_blocks routes tool messages to TranscriptBlock::Tool;
        // if one ever lands here, render defensively instead of panicking.
        TranscriptBlock::Message(Message::ToolCall(call)) => {
            vec![Segment::Text(pad_plain(wrap_plain(
                &format!("{}({})", call.name, call.arguments),
                inner,
                THEME.fg(Fg::Dim),
            )))]
        }
        TranscriptBlock::Message(Message::ToolResult(result)) => vec![Segment::Text(pad_plain(
            wrap_plain(&result.content, inner, THEME.fg(Fg::Dim)),
        ))],
        TranscriptBlock::Tool(exec) => tool_segments(exec, width, caps),
    }
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
/// fill (so every cell — including the padding columns — carries the
/// background at full width), one blank fill row above and below (pi's
/// paddingY), and the content inset one column on each side (paddingX):
/// callers wrap at `width - 2`, so one fill column borders the content
/// left and right.
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

/// One plain blank row above and below (paddingY) and one plain blank
/// column each side (paddingX) — the transparent block's mirror of
/// what `padded_block` does with a fill. Callers wrap at `width - 2`.
fn pad_plain(rows: Vec<Line<'static>>) -> Vec<Line<'static>> {
    let mut out = Vec::with_capacity(rows.len() + 2);
    out.push(Line::raw(""));
    for row in rows {
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(row.spans.len() + 2);
        spans.push(Span::raw(" "));
        spans.extend(row.spans);
        spans.push(Span::raw(" "));
        out.push(Line::from(spans));
    }
    out.push(Line::raw(""));
    out
}

fn fill_row(width: usize, fill: Style) -> Line<'static> {
    Line::from(Span::styled(" ".repeat(width), fill))
}

/// Tool block: bold tool name with the collapsed args preview over the
/// output preview, in a fill tinted by execution state — pi's
/// ToolExecutionComponent: pending while running, then success or error.
/// Attached images render as kitty graphics after the text band (ruling
/// #42.2); a corrupt or undecodable image falls back to its text note,
/// never to nothing.
fn tool_segments(
    exec: &ToolExec,
    width: usize,
    caps: Option<crate::kitty::Protocol>,
) -> Vec<Segment> {
    let mut segments = vec![Segment::Text(tool_rows(exec, width))];
    let Some(result) = &exec.result else {
        return segments;
    };
    if result.images.is_empty() {
        return segments;
    }
    let kitty = caps == Some(crate::kitty::Protocol::Kitty);
    let inner_width = width.saturating_sub(2).max(1);
    for image in &result.images {
        // One note format serves both stories: no image protocol, or a
        // specific image the renderer had to reject.
        if let (true, Some((sequence, rows))) =
            (kitty, crate::kitty::render(image, inner_width as u16))
        {
            segments.push(Segment::Image { sequence, rows });
        } else {
            segments.push(Segment::Text(vec![image_note(image)]));
        }
    }
    segments
}

/// Total scrollback rows a set of segments will occupy.
pub fn segments_height(segments: &[Segment]) -> u16 {
    segments
        .iter()
        .map(|segment| match segment {
            Segment::Text(rows) => rows.len().max(1) as u16,
            Segment::Image { rows, .. } => *rows,
        })
        .sum()
}

/// Render print segments into a scrollback buffer. Text rows go
/// through the ordinary Paragraph path; an image's kitty sequence
/// rides in one cell at the block's side padding, overlaying the
/// `rows` untouched cells below it (kitty z-order draws the image over
/// default-background cells, so the empty spaces stay invisible).
pub fn segments_into(segments: Vec<Segment>, buf: &mut Buffer) {
    const PAD_X: u16 = 1; // padded_block's side padding
    let mut y = 0u16;
    for segment in segments {
        match segment {
            Segment::Text(rows) => {
                let height = rows.len().max(1) as u16;
                Paragraph::new(Text::from(rows)).render(
                    Rect {
                        x: 0,
                        y,
                        width: buf.area.width,
                        height,
                    },
                    buf,
                );
                y += height;
            }
            Segment::Image { sequence, rows } => {
                if let Some(cell) = buf.cell_mut((PAD_X, y)) {
                    cell.set_symbol(sequence.as_str());
                }
                y += rows;
            }
        }
    }
}

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
    }
    padded_block(inner, width, fill)
}

/// One dim line per image: mime type plus decoded size. The text note
/// is the honest placeholder whenever pixels cannot render — no image
/// protocol, or a specific image the renderer rejected (issue #42).
fn image_note(image: &hi_core::tool::ImageBlock) -> Line<'static> {
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
        area.width as usize,
        picker.renaming.as_deref(),
    );
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::{Backend, TestBackend};
    use ratatui::layout::Rect;

    #[test]
    fn the_window_hugs_the_input_when_the_transcript_is_short() {
        use crate::app::test_app;
        let mut app = test_app();
        app.messages
            .push(hi_core::Message::User("where did my transcript go".into()));
        app.scroll.sync(&app.messages, 40, None);
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let buf = terminal.backend_mut().buffer().clone();
        let row_text = |y: u16| {
            (0..40)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
                .trim_end()
                .to_string()
        };
        // Input box rows 14-17 (margin 14, rules 15/17), status rows
        // 18-19; the stream window is rows 0..14 — and the user block
        // (3 rows: pad, text, pad) sits at its BOTTOM, rows 11-13,
        // above the composer's breathing row (AD-0020).
        assert!(row_text(12).contains("where did my transcript go"));
        assert!(row_text(13).trim().is_empty()); // the block's lower pad
        assert!(row_text(14).trim().is_empty()); // the margin row
        assert!(row_text(15).contains("─")); // input top rule
    }

    #[test]
    fn live_streaming_block_carries_the_block_padding() {
        use crate::app::{Turn, test_app};
        let mut app = test_app();
        // A flushed block above: the live reply must keep the same
        // one-blank-row separation every block gets — and then the
        // flush is visually a no-op instead of a reflow jump.
        app.messages
            .push(hi_core::Message::User("earlier question".into()));
        app.scroll.sync(&app.messages, 40, None);
        app.turn = Some(Turn::with_text("streamed padding probe"));
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let buf = terminal.backend_mut().buffer().clone();
        let row_text = |y: u16| {
            (0..40)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
                .trim_end()
                .to_string()
        };
        let content = (0..20u16)
            .find(|&y| row_text(y).contains("streamed padding probe"))
            .expect("streaming text visible");
        // paddingX: the live content sits one column in, like every
        // flushed block.
        assert!(
            row_text(content).starts_with(' '),
            "live text hugs the left edge: {:?}",
            row_text(content)
        );
        // paddingY: the live block opens with its own blank row — two
        // blank rows now separate the user text from the live text
        // (the user block's bottom pad plus this one).
        assert!(
            row_text(content - 2).trim().is_empty(),
            "no top pad above the live block: {:?}",
            row_text(content - 2)
        );
    }

    #[test]
    fn the_margin_row_survives_a_full_stream() {
        use crate::app::test_app;
        let mut app = test_app();
        // Long enough to fill the stream window completely: under the
        // old geometry the newest transcript row would sit flush
        // against the input's top rule.
        for i in 0..30 {
            app.messages
                .push(hi_core::Message::User(format!("filler message {i}")));
        }
        app.scroll.sync(&app.messages, 40, None);
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let buf = terminal.backend_mut().buffer().clone();
        let row_text = |y: u16| {
            (0..40)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
                .trim_end()
                .to_string()
        };
        assert!(row_text(15).contains("─")); // input top rule
        // The newest text row sits at 12, with the block's own pad on
        // 13: the stream's bottom edge is the margin row — one row
        // higher than the old flush-against-the-rule geometry.
        assert!(row_text(12).contains("filler message 29"));
        assert!(row_text(13).trim().is_empty()); // block pad, stream's last row
        // The breathing row itself stays empty.
        assert!(row_text(14).trim().is_empty(), "{:?}", row_text(14));
    }

    #[test]
    fn settings_board_lists_rows_and_shows_the_flip_live() {
        use crate::app::test_app;
        let mut app = test_app();
        app.run_command("settings", "");
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = frame_text(terminal.backend_mut().buffer(), 60, 20);
        assert!(text.contains("task turns: off"), "{text}");
        assert!(text.contains("images: on"), "{text}");
        assert!(
            text.contains("enter toggle · ctrl-s save · esc close"),
            "{text}"
        );
        // The board stays open on Enter and the row re-renders — the
        // flip is visible without reopening (AD-0023).
        app.handle_key(ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Enter,
            ratatui::crossterm::event::KeyModifiers::NONE,
        ));
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = frame_text(terminal.backend_mut().buffer(), 60, 20);
        assert!(text.contains("task turns: on"), "{text}");
        assert!(app.settings_board.is_some());
    }

    #[test]
    fn tasks_pane_lists_states_and_watches_the_output() {
        use crate::app::{TasksView, test_app};
        let mut app = test_app();
        let tasks =
            hi_core::tasks::TaskManager::new(std::env::temp_dir().join("hi-tui-tasks-pane"));
        app.tasks = Some(tasks.clone());
        let id = tasks
            .spawn(hi_core::tasks::TaskSpec::process(
                "echo pane-echo-marker",
                std::env::temp_dir(),
            ))
            .unwrap();
        // Watch means draw reads live: wait on the fact, then render.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !tasks.status(id).is_some_and(|s| s.state.finished()) {
            assert!(
                std::time::Instant::now() < deadline,
                "echo task never finished"
            );
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        app.run_command("tasks", "");
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = frame_text(terminal.backend_mut().buffer(), 60, 20);
        assert!(text.contains("#1 echo"), "{text}");
        assert!(text.contains("finished (exit 0)"), "{text}");
        assert!(text.contains("enter output · esc close"), "{text}");
        // The output view: header carries the state phrase, the tail
        // shows the task's actual bytes, the footer offers Esc.
        app.tasks_view = Some(TasksView::Output { id });
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = frame_text(terminal.backend_mut().buffer(), 60, 20);
        assert!(
            text.contains("#1 'echo pane-echo-marker' — finished (exit 0)"),
            "{text}"
        );
        assert!(text.contains("pane-echo-marker"), "{text}");
        assert!(text.contains("esc back"), "{text}");
    }

    #[test]
    fn tasks_output_pane_bottom_anchors_long_tails() {
        use crate::app::{TasksView, test_app};
        let mut app = test_app();
        let tasks =
            hi_core::tasks::TaskManager::new(std::env::temp_dir().join("hi-tui-tasks-long"));
        app.tasks = Some(tasks.clone());
        let id = tasks
            .spawn(hi_core::tasks::TaskSpec::process(
                "seq -f '%05.0f' 1 40",
                std::env::temp_dir(),
            ))
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let Some(status) = tasks.status(id) else {
                panic!("task vanished");
            };
            if status.state.finished() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "seq task never finished"
            );
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        app.tasks_view = Some(TasksView::Output { id });
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = frame_text(terminal.backend_mut().buffer(), 60, 20);
        // Six content rows fit: the newest six lines win (35..40),
        // the head of the tail is trimmed.
        assert!(text.contains("00040"), "{text}");
        assert!(!text.contains("00001"), "{text}");
    }

    /// All non-blank frame content as one string, row by row.
    fn frame_text(buf: &Buffer, width: u16, height: u16) -> String {
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_model_picker_uses_the_same_select_list() {
        use crate::app::test_app;
        let mut app = test_app();
        app.picker = Some(crate::app::Picker {
            items: (0..6).map(|i| format!("provider{i}/model-{i}")).collect(),
            selected: 1,
        });
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let buf = terminal.backend_mut().buffer().clone();
        let rows: Vec<String> = (0..20u16)
            .map(|y| {
                (0..60)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();
        // The deprecated box is gone; the SelectList marker, count row
        // and hint footer take its place.
        assert!(!rows.iter().any(|r| r.contains("┌")));
        assert!(rows.iter().any(|r| r.contains("→ provider1")));
        // The count row follows the last visible item, not the marker.
        assert!(rows.iter().any(|r| r.contains("  (2/6)")));
        assert!(rows.iter().any(|r| r.contains("ctrl-s default")));
    }

    #[test]
    fn the_menu_rolls_with_the_selection_and_counts_its_rows() {
        use crate::app::test_app;
        let mut app = test_app();
        app.completion = Some(crate::app::Completion {
            items: vec![
                ("alpha", "first"),
                ("beta", "second"),
                ("gamma", "third"),
                ("delta", "fourth"),
                ("epsilon", "fifth"),
                ("zeta", "sixth"),
                ("eta", "seventh"),
            ],
            selected: 0,
        });
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let buf = terminal.backend_mut().buffer().clone();
        let row_text = |y: u16| {
            (0..60)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
                .trim_end()
                .to_string()
        };
        let rows: Vec<String> = (0..20u16).map(row_text).collect();
        // Five visible slots plus the count row; the window shows the
        // head of the list, not the tail.
        assert!(rows.iter().any(|r| r.contains("  (1/7)")));
        assert!(rows.iter().any(|r| r.contains("alpha")));
        assert!(!rows.iter().any(|r| r.contains("zeta")));
        assert!(!rows.iter().any(|r| r.contains("sixth")));
    }

    #[test]
    fn menus_render_under_the_input_and_status_yields() {
        use crate::app::test_app;
        let mut app = test_app();
        app.handle_key(ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Char('/'),
            ratatui::crossterm::event::KeyModifiers::NONE,
        ));
        assert!(app.completion.is_some(), "slash opens the completion");
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let buf = terminal.backend_mut().buffer().clone();
        let row_text = |y: u16| {
            (0..60)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
                .trim_end()
                .to_string()
        };
        // Geometry, not coordinates: the input line (with the typed
        // "/") must sit ABOVE the completion items, the items must
        // reach the bottom edge, and the status line must have
        // yielded — no "mock" anywhere in the frame. SelectList
        // style: the marker rides the selected row, the description
        // sits in its own column.
        let input_row = (0..20u16).find(|&y| row_text(y) == "/").expect("input row");
        let menu_row = (0..20u16)
            .find(|&y| row_text(y).contains("pick a model"))
            .expect("completion description column");
        assert!(input_row < menu_row, "menu renders under the input");
        assert!(row_text(menu_row - 1).contains("→ "), "selected row marker");
        assert!(
            !row_text(19).contains("mock"),
            "status yields while a menu is open"
        );
    }

    #[test]
    fn the_viewport_is_the_whole_terminal() {
        // 40 rows resting: the stream keeps everything but the input
        // and the status row — the old fixed 12-row sketch gave this
        // terminal 8 stream rows.
        let (stream, menu, input, status) = layout(Rect::new(0, 0, 80, 40), 3, 0, 1);
        assert_eq!((stream.height, menu.height), (36, 0));
        assert_eq!((input.y, input.height), (36, 3)); // rows 36-38
        assert_eq!((status.y, status.height), (39, 1));
        // A menu renders UNDER the input and owns the bottom edge;
        // the status board yields for its lifetime.
        let (stream, menu, input, status) = layout(Rect::new(0, 0, 80, 40), 3, 4, 1);
        assert_eq!(stream.height, 33);
        assert_eq!(input.y, 33); // rows 33-35
        assert_eq!((menu.y, menu.height), (36, 4)); // rows 36-39
        assert_eq!((status.y, status.height), (39, 0));
    }

    #[test]
    fn short_terminals_degrade_without_panicking() {
        // The stream shrinks first, to nothing.
        let (stream, _, input, status) = layout(Rect::new(0, 0, 80, 6), 3, 0, 1);
        assert_eq!(stream.height, 2);
        assert_eq!(input.y, 2);
        assert_eq!(status.y, 5);
        let (stream, _, input, _) = layout(Rect::new(0, 0, 80, 4), 3, 0, 1);
        assert_eq!(stream.height, 0);
        assert_eq!(input.y, 0); // rows 0-2, status at 3
        // Absurd — an input taller than the terminal: saturated, no
        // panic, the status board still claims the bottom edge.
        let (_, _, input, status) = layout(Rect::new(0, 0, 80, 2), 8, 0, 1);
        assert_eq!(status.y, 1);
        assert_eq!(input.y, 0);
    }

    #[test]
    fn the_status_board_stacks_its_rows_above_the_bar() {
        // Two earned rows: the board grows upward from the bottom
        // edge, the stream pays for it.
        let (stream, menu, input, status) = layout(Rect::new(0, 0, 80, 40), 3, 0, 2);
        assert_eq!((stream.height, menu.height), (35, 0));
        assert_eq!((input.y, input.height), (35, 3)); // rows 35-37
        assert_eq!((status.y, status.height), (38, 2)); // rows 38-39
        // A menu still outranks the whole board (AD-0025 keeps the
        // ruled yield): when one is open the board yields entirely.
        let (stream, menu, input, status) = layout(Rect::new(0, 0, 80, 40), 3, 4, 2);
        assert_eq!(stream.height, 33);
        assert_eq!((menu.y, menu.height), (36, 4));
        assert_eq!((input.y, input.height), (33, 3));
        assert_eq!(status.height, 0);
    }

    #[test]
    fn the_status_board_renders_its_resident_rows() {
        let app = crate::app::test_app();
        assert_eq!(app.status_height(), 2, "both rows are permanent now");
        // Wide enough that the location doesn't clip the right label.
        let mut terminal = Terminal::new(TestBackend::new(72, 20)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let buf = terminal.backend_mut().buffer().clone();
        let row_text = |y: u16| {
            (0..72)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
                .trim_end()
                .to_string()
        };
        // Two permanent rows: the model on row 18, the location bar
        // on the bottom edge — row 0's right stays empty.
        assert!(row_text(19).contains("~"), "{:?}", row_text(19));
        assert!(!row_text(19).contains("(mock)"), "{:?}", row_text(19));
        assert!(row_text(18).ends_with("(mock) mock"), "{:?}", row_text(18));
    }

    #[test]
    fn the_caret_tracks_the_logical_cursor() {
        let mut app = crate::app::test_app();
        app.input = "hello world".into();
        app.cursor = 5;
        // Narrow frame: "hello world" wraps into two rows here.
        let backend = TestBackend::new(10, 11);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        // "hello world" wraps to two input rows here: the box on an
        // 11-row frame starts at y = 11 - 2(status) - 5(input), inner
        // text at y = 6.
        assert_eq!(
            terminal.backend_mut().get_cursor_position().unwrap(),
            Position { x: 5, y: 6 }
        );
        // Mid-word wrap: cursor inside the second wrapped row.
        app.input = "hello brave world".into();
        app.cursor = 8; // after "brave"'s first two chars
        terminal.draw(|f| draw(f, &app)).unwrap();
        // Three wrapped rows: the box starts at y = 11 - 2 - 6 = 3,
        // inner text at 5 — and the cursor sits on wrapped row 1,
        // two columns into "brave", so 6.
        assert_eq!(
            terminal.backend_mut().get_cursor_position().unwrap(),
            Position { x: 2, y: 6 }
        );
    }

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
        let rows = block_rows(&TranscriptBlock::Tool(exec), 60);
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

    fn segment_text(segments: &[Segment]) -> String {
        segments
            .iter()
            .flat_map(|s| match s {
                Segment::Text(rows) => rows
                    .iter()
                    .map(|r| {
                        r.spans
                            .iter()
                            .map(|sp| sp.content.to_string())
                            .collect::<Vec<_>>()
                            .join("")
                    })
                    .collect::<Vec<_>>(),
                Segment::Image { .. } => Vec::new(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    const TINY_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

    #[test]
    fn kitty_caps_render_images_as_segments_not_notes() {
        let mut result = tool_result("done", false);
        result.images = vec![hi_core::tool::ImageBlock {
            data: TINY_PNG_B64.into(),
            mime_type: "image/png".into(),
        }];
        let segments = block_segments(
            &TranscriptBlock::Tool(ToolExec {
                call: None,
                result: Some(result),
            }),
            60,
            Some(crate::kitty::Protocol::Kitty),
        );
        let text = segment_text(&segments);
        assert!(!text.contains("· image:"), "note must be gone: {text}");
        let images: Vec<&Segment> = segments
            .iter()
            .filter(|s| matches!(s, Segment::Image { .. }))
            .collect();
        assert_eq!(images.len(), 1);
        let Segment::Image { sequence, rows } = images[0] else {
            unreachable!()
        };
        assert!(sequence.starts_with("\x1b_Ga=T,f=100,q=2,C=1,"));
        assert!(*rows >= 1);
        // Height accounting covers the image's overlay rows too.
        assert!(segments_height(&segments) as usize >= 3 + *rows as usize);
    }

    #[test]
    fn corrupt_images_fall_back_to_an_honest_note() {
        let mut result = tool_result("done", false);
        result.images = vec![hi_core::tool::ImageBlock {
            data: "aW1n".into(), // decodes, but is not an image
            mime_type: "image/png".into(),
        }];
        let segments = block_segments(
            &TranscriptBlock::Tool(ToolExec {
                call: None,
                result: Some(result),
            }),
            60,
            Some(crate::kitty::Protocol::Kitty),
        );
        assert!(!segments.iter().any(|s| matches!(s, Segment::Image { .. })));
        let text = segment_text(&segments);
        assert!(text.contains("· image: image/png, 0.0 KB"));
    }

    #[test]
    fn image_segments_place_their_sequence_and_rows_exactly() {
        // One text segment (2 rows), then an image (3 rows), then text.
        let segments = vec![
            Segment::Text(vec![Line::from("one"), Line::from("two")]),
            Segment::Image {
                sequence: "\x1b_Gfake\x1b\\".into(),
                rows: 3,
            },
            Segment::Text(vec![Line::from("after")]),
        ];
        assert_eq!(segments_height(&segments), 6);
        let buf = &mut Buffer::empty(Rect::new(0, 0, 20, 6));
        segments_into(segments, buf);
        // The sequence lands in one cell at the side padding, exactly
        // where the image band starts.
        assert_eq!(buf[(1, 2)].symbol(), "\x1b_Gfake\x1b\\");
        // Text resumes below the image band.
        assert_eq!(text_of(buf, 5, 20), "after");
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
        segments_into(vec![Segment::Text(rows)], buf);
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
    fn every_block_insets_its_content_one_column_each_side() {
        // Transparent: plain spaces flank the content.
        let rows = block_rows(
            &TranscriptBlock::Message(Message::Assistant("hello".into())),
            30,
        );
        assert_eq!(rows[1].spans.len(), 3);
        assert_eq!(rows[1].spans[0].content, " ");
        assert_eq!(rows[1].spans[1].content, "hello");
        assert_eq!(rows[1].spans[2].content, " ");
        // Filled: the fill still runs the FULL width (ruled), with the
        // content inset one column inside it. "hello" fills its row:
        // 1 pad + 28 content + 1 pad = 30.
        let rows = block_rows(&TranscriptBlock::Message(Message::User("x".repeat(28))), 30);
        assert_eq!(rows[1].spans[0].content, " ");
        let total: usize = rows[1].spans.iter().map(|s| s.content.width()).sum();
        assert_eq!(total, 30);
        assert_eq!(rows[1].spans[1].content, "x".repeat(28));
    }

    #[test]
    fn assistant_block_has_no_fill_and_its_own_padding() {
        let rows = block_rows(
            &TranscriptBlock::Message(Message::Assistant("plain reply".into())),
            30,
        );
        // paddingY: one plain blank row above and below; paddingX:
        // one plain column each side of the content.
        assert_eq!(rows.len(), 3);
        let buf = &mut Buffer::empty(Rect::new(0, 0, 30, 3));
        segments_into(vec![Segment::Text(rows)], buf);
        assert_eq!(text_of(buf, 0, 30), "");
        assert_eq!(text_of(buf, 1, 30), " plain reply"); // trailing pad trimmed
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
        // rows[1] spans: [paddingX " ", heading, paddingX " "].
        assert!(
            rows[1].spans[1]
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
        assert_eq!(system[1].spans[1].style.fg, THEME.fg(Fg::Dim).fg); // [pad, text, pad]
        let error = block_rows(&TranscriptBlock::Message(Message::Error("boom".into())), 20);
        assert_eq!(error.len(), 3);
        assert_eq!(error[1].spans[1].style.fg, THEME.fg(Fg::Error).fg);
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
