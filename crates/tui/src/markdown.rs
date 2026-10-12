//! Markdown → themed terminal rows, rendering with pi's conventions.
//!
//! Parses with `pulldown-cmark` (CommonMark plus pi's `marked` extension
//! set: GFM tables, strikethrough, tasklists — no math, by ruling) and
//! renders to `Line`s the way pi-tui's markdown component does:
//!
//! - h1 bold+underlined, h2 bold, h3+ keep their `### ` prefix, all in the
//!   heading color; blank line after headings, paragraphs, code, quotes,
//!   rules, and tables (pi's inter-block spacing)
//! - fenced code restated literally: border-colored ``` lines, body
//!   indented two spaces (no syntax highlighting — a known divergence)
//! - blockquotes with a `│ ` border, body italic
//! - lists with `- `/`N. ` markers in the list-bullet color, task-list
//!   `[x]`/`[ ] ` markers, four-space nesting
//! - links as underlined text with ` (url)` appended when the text differs
//!   from the href (the no-OSC-8 fallback pi uses; hi never emits OSC 8)
//! - tables drawn as box-drawing grids with per-column wrapping and a
//!   proportional shrink when the natural widths don't fit
//!
//! Inline content is collected as styled segments and wrapped by
//! [`wrap_segments`], which carries styles across line breaks — the styled
//! sibling of `wrap.rs`'s plain wrapping.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::theme::{Fg, THEME, Theme};
use crate::wrap::display_width;
use unicode_width::UnicodeWidthChar;

/// A run of text sharing one style — the unit inline content is collected
/// and wrapped in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Seg {
    text: String,
    style: Style,
}

impl Seg {
    pub(crate) fn new(text: impl Into<String>, style: Style) -> Self {
        Seg {
            text: text.into(),
            style,
        }
    }

    fn plain(text: impl Into<String>) -> Self {
        Seg::new(text, Style::new())
    }

    fn width(&self) -> usize {
        display_width(&self.text)
    }
}

/// Render markdown to themed rows at `width`.
pub fn markdown_rows(text: &str, width: u16, base: Style) -> Vec<Line<'static>> {
    markdown_rows_with(text, width.max(1) as usize, base, &THEME)
}

fn markdown_rows_with(text: &str, width: usize, base: Style, theme: &Theme) -> Vec<Line<'static>> {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let events: Vec<Event> = Parser::new_ext(text, options).collect();
    let mut rows = Vec::new();
    let mut i = 0;
    blocks(&events, &mut i, None, base, theme, width, &mut rows);
    rows
}

/// Wrap styled segments to `width` (greedy, word boundaries, hard-splits
/// oversized words). A [`Seg`] whose text is exactly `"\n"` forces a break.
pub(crate) fn wrap_segments(segs: &[Seg], width: usize) -> Vec<Vec<Seg>> {
    let width = width.max(1);
    let mut lines: Vec<Vec<Seg>> = Vec::new();
    let mut cur: Vec<Seg> = Vec::new();
    let mut cur_w = 0usize;

    let flush = |lines: &mut Vec<Vec<Seg>>, cur: &mut Vec<Seg>| {
        if !cur.is_empty() {
            lines.push(std::mem::take(cur));
        }
    };

    // Hard-split an oversized word by display columns, pushing full rows
    // straight into `lines` and the remainder into `cur`.
    let hard_split = |lines: &mut Vec<Vec<Seg>>,
                      cur: &mut Vec<Seg>,
                      cur_w: &mut usize,
                      piece: &str,
                      style: Style| {
        let mut chunk = String::new();
        let mut chunk_w = 0usize;
        for ch in piece.chars() {
            let cw = ch.width().unwrap_or(0);
            if chunk_w + cw > width && chunk_w > 0 {
                lines.push(vec![Seg::new(std::mem::take(&mut chunk), style)]);
                chunk_w = 0;
            }
            chunk.push(ch);
            chunk_w += cw;
        }
        if !chunk.is_empty() {
            *cur_w += chunk_w;
            cur.push(Seg::new(chunk, style));
        }
    };

    for seg in segs {
        if seg.text == "\n" {
            flush(&mut lines, &mut cur);
            cur_w = 0;
            continue;
        }
        // Split into space-keeping pieces so breaks land on word boundaries.
        for piece in split_words(&seg.text) {
            let pw = display_width(&piece);
            if pw > width && cur_w == 0 {
                hard_split(&mut lines, &mut cur, &mut cur_w, &piece, seg.style);
                continue;
            }
            if cur_w + pw > width && cur_w > 0 {
                flush(&mut lines, &mut cur);
                cur_w = 0;
                // Don't carry a leading space onto a wrapped line.
                let piece = piece.strip_prefix(' ').unwrap_or(&piece);
                let pw = display_width(piece);
                if pw > width {
                    hard_split(&mut lines, &mut cur, &mut cur_w, piece, seg.style);
                    continue;
                }
                if !piece.is_empty() {
                    cur.push(Seg::new(piece, seg.style));
                    cur_w += pw;
                }
                continue;
            }
            cur.push(Seg::new(piece, seg.style));
            cur_w += pw;
        }
    }
    flush(&mut lines, &mut cur);
    if lines.is_empty() {
        lines.push(Vec::new());
    }
    lines
}

/// Split into pieces that keep their trailing space (so "word " stays one
/// piece), mirroring `wrap.rs`'s `split_inclusive(' ')`.
fn split_words(text: &str) -> Vec<String> {
    // Pieces keep their trailing space, so "word " stays one unit and
    // breaks land between words — mirroring `wrap.rs`.
    text.split_inclusive(' ').map(str::to_string).collect()
}

pub(crate) fn segs_into_line(segs: Vec<Seg>) -> Line<'static> {
    Line::from(
        segs.into_iter()
            .map(|s| Span::styled(s.text, s.style))
            .collect::<Vec<_>>(),
    )
}

/// Block-level walk. Consumes events until `stop` (exclusive) or EOF,
/// pushing rendered rows. `base` is the default style for inline text
/// (quotes layer italic + quote color onto it).
fn blocks(
    events: &[Event],
    i: &mut usize,
    stop: Option<TagEnd>,
    base: Style,
    theme: &Theme,
    width: usize,
    rows: &mut Vec<Line<'static>>,
) {
    let mut gap = false;
    while *i < events.len() {
        if let Some(stop) = stop
            && matches!(&events[*i], Event::End(tag) if *tag == stop)
        {
            *i += 1;
            return;
        }
        let blank_before = gap && !matches!(events[*i], Event::Start(Tag::List(_)));
        gap = false;
        match events[*i].clone() {
            Event::Start(Tag::Paragraph) => {
                *i += 1;
                if blank_before {
                    rows.push(Line::raw(""));
                }
                let segs = inline(events, i, TagEnd::Paragraph, base, theme);
                rows.extend(wrap_segments(&segs, width).into_iter().map(segs_into_line));
                gap = true;
            }
            Event::Start(Tag::Heading { level, .. }) => {
                *i += 1;
                if blank_before {
                    rows.push(Line::raw(""));
                }
                let mut segs = inline(
                    events,
                    i,
                    TagEnd::Heading(level),
                    heading_base(level, base, theme),
                    theme,
                );
                if level >= HeadingLevel::H3 {
                    segs.insert(
                        0,
                        Seg::new(
                            format!("{} ", "#".repeat(level as usize)),
                            heading_base(level, base, theme),
                        ),
                    );
                }
                rows.extend(wrap_segments(&segs, width).into_iter().map(segs_into_line));
                gap = true;
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                *i += 1;
                if blank_before {
                    rows.push(Line::raw(""));
                }
                let lang = match kind {
                    CodeBlockKind::Fenced(lang) => lang.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                let border = theme.fg(Fg::CodeBlockBorder);
                rows.push(Line::from(Span::styled(format!("```{lang}"), border)));
                while *i < events.len() {
                    match events[*i].clone() {
                        Event::Text(code) => {
                            *i += 1;
                            for line in code.lines() {
                                rows.push(Line::from(Span::styled(
                                    format!("  {line}"),
                                    theme.fg(Fg::CodeBlock),
                                )));
                            }
                        }
                        Event::End(TagEnd::CodeBlock) => {
                            *i += 1;
                            break;
                        }
                        _ => *i += 1,
                    }
                }
                rows.push(Line::from(Span::styled("```", border)));
                gap = true;
            }
            Event::Start(Tag::List(start)) => {
                *i += 1;
                if blank_before {
                    rows.push(Line::raw(""));
                }
                list(
                    events,
                    i,
                    start,
                    TagEnd::List(start.is_some()),
                    base,
                    theme,
                    width,
                    rows,
                );
            }
            Event::Start(Tag::BlockQuote(kind)) => {
                *i += 1;
                if blank_before {
                    rows.push(Line::raw(""));
                }
                // Inner content renders one nesting level narrower and gets
                // the `│ ` border afterward (pi quotes the border last).
                let inner_width = width.saturating_sub(2).max(1);
                let mut inner = Vec::new();
                blocks(
                    events,
                    i,
                    Some(TagEnd::BlockQuote(kind)),
                    quote_base(base, theme),
                    theme,
                    inner_width,
                    &mut inner,
                );
                let border = theme.fg(Fg::QuoteBorder);
                for row in inner {
                    let mut spans = vec![Span::styled("│ ", border)];
                    spans.extend(row.spans);
                    rows.push(Line::from(spans));
                }
                gap = true;
            }
            Event::Rule => {
                *i += 1;
                if blank_before {
                    rows.push(Line::raw(""));
                }
                rows.push(Line::from(Span::styled(
                    "─".repeat(width.min(80)),
                    theme.fg(Fg::Hr),
                )));
                gap = true;
            }
            Event::Start(Tag::Table(aligns)) => {
                *i += 1;
                if blank_before {
                    rows.push(Line::raw(""));
                }
                let table = table_cells(events, i, theme, base);
                rows.extend(table_rows(&table, &aligns, width));
                gap = true;
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                *i += 1;
                for line in html.lines() {
                    rows.push(Line::from(Span::raw(line.to_string())));
                }
            }
            Event::Text(_) | Event::Code(_) => {
                // Bare text outside a paragraph (loose list remnants).
                let segs = inline_bare(events, i, base, theme);
                rows.extend(wrap_segments(&segs, width).into_iter().map(segs_into_line));
            }
            // Space, bare End tags (never our stop — checked above), and
            // anything unparsed: step over.
            _ => *i += 1,
        }
    }
}

fn heading_base(level: HeadingLevel, base: Style, theme: &Theme) -> Style {
    let mut style = base.patch(theme.fg(Fg::Heading)).bold();
    if level == HeadingLevel::H1 {
        style = style.underlined();
    }
    style
}

fn quote_base(base: Style, theme: &Theme) -> Style {
    base.patch(theme.fg(Fg::Quote)).italic()
}

/// One list level. `start` is `Some(n)` for ordered lists.
#[allow(clippy::too_many_arguments)]
fn list(
    events: &[Event],
    i: &mut usize,
    start: Option<u64>,
    stop: TagEnd,
    base: Style,
    theme: &Theme,
    width: usize,
    rows: &mut Vec<Line<'static>>,
) {
    let mut index = start.unwrap_or(1);
    while *i < events.len() {
        match events[*i].clone() {
            Event::End(tag) if tag == stop => {
                *i += 1;
                return;
            }
            Event::Start(Tag::Item) => {
                *i += 1;
                let mut task = String::new();
                if let Some(Event::TaskListMarker(checked)) = events.get(*i) {
                    task = if *checked { "[x] " } else { "[ ] " }.to_string();
                    *i += 1;
                }
                let bullet = match start {
                    Some(_) => format!("{index}. "),
                    None => "- ".to_string(),
                };
                index += 1;
                let marker = format!("{bullet}{task}");
                let marker_width = display_width(&marker);
                let first_prefix = Span::styled(marker.clone(), theme.fg(Fg::ListBullet));
                let continuation = " ".repeat(marker_width);
                let item_width = width.saturating_sub(marker_width).max(1);

                // First row prefix lands on the first rendered line.
                let mut prefixed = 0usize;
                let mut emit = |rows: &mut Vec<Line<'static>>, line: Line<'static>| {
                    let mut spans = Vec::with_capacity(2);
                    if prefixed == 0 {
                        spans.push(first_prefix.clone());
                    } else {
                        spans.push(Span::raw(continuation.clone()));
                    }
                    prefixed += 1;
                    spans.extend(line.spans);
                    rows.push(Line::from(spans));
                };

                while *i < events.len() {
                    match events[*i].clone() {
                        Event::End(TagEnd::Item) => {
                            *i += 1;
                            break;
                        }
                        Event::Start(Tag::List(nested)) => {
                            *i += 1;
                            // Nested lists: four-space indent per level (pi),
                            // no parent marker prefix — pi recurses by depth.
                            let mut tmp = Vec::new();
                            list(
                                events,
                                i,
                                nested,
                                TagEnd::List(nested.is_some()),
                                base,
                                theme,
                                width.saturating_sub(4).max(1),
                                &mut tmp,
                            );
                            for row in tmp {
                                let mut spans = vec![Span::raw("    ".to_string())];
                                spans.extend(row.spans);
                                rows.push(Line::from(spans));
                            }
                        }
                        Event::Start(Tag::Paragraph) => {
                            *i += 1;
                            let segs = inline(events, i, TagEnd::Paragraph, base, theme);
                            for line in wrap_segments(&segs, item_width) {
                                emit(rows, segs_into_line(line));
                            }
                        }
                        Event::Start(Tag::CodeBlock(_)) => {
                            // Reuse the block walker for the fence body.
                            let mut tmp = Vec::new();
                            blocks(
                                events,
                                i,
                                Some(TagEnd::CodeBlock),
                                base,
                                theme,
                                item_width,
                                &mut tmp,
                            );
                            for row in tmp {
                                emit(rows, row);
                            }
                        }
                        Event::Text(_) => {
                            let segs = inline_bare(events, i, base, theme);
                            for line in wrap_segments(&segs, item_width) {
                                emit(rows, segs_into_line(line));
                            }
                        }
                        _ => *i += 1,
                    }
                }
            }
            _ => *i += 1,
        }
    }
}

/// Inline walk: collects styled segments until the matching `stop` End
/// (consumed) or EOF. Nested containers recurse with a patched base.
fn inline(events: &[Event], i: &mut usize, stop: TagEnd, base: Style, theme: &Theme) -> Vec<Seg> {
    let mut segs: Vec<Seg> = Vec::new();
    while *i < events.len() {
        match events[*i].clone() {
            Event::End(tag) if tag == stop => {
                *i += 1;
                return segs;
            }
            Event::Text(text) => {
                *i += 1;
                segs.push(Seg::new(text.to_string(), base));
            }
            Event::Code(code) => {
                *i += 1;
                segs.push(Seg::new(code.to_string(), theme.fg(Fg::Code)));
            }
            Event::SoftBreak => {
                *i += 1;
                segs.push(Seg::new(" ", base));
            }
            Event::HardBreak => {
                *i += 1;
                segs.push(Seg::new("\n", base));
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                *i += 1;
                segs.push(Seg::new(html.to_string(), base));
            }
            Event::Start(Tag::Strong) => {
                *i += 1;
                segs.extend(inline(events, i, TagEnd::Strong, base.bold(), theme));
            }
            Event::Start(Tag::Emphasis) => {
                *i += 1;
                segs.extend(inline(events, i, TagEnd::Emphasis, base.italic(), theme));
            }
            Event::Start(Tag::Strikethrough) => {
                *i += 1;
                segs.extend(inline(
                    events,
                    i,
                    TagEnd::Strikethrough,
                    base.crossed_out(),
                    theme,
                ));
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                *i += 1;
                let link_base = base.patch(theme.fg(Fg::Link)).underlined();
                let inner = inline(events, i, TagEnd::Link, link_base, theme);
                // No OSC 8: pi's fallback prints the URL when the visible
                // text differs from it (mailto: prefixes don't count).
                let text: String = inner.iter().map(|s| s.text.as_str()).collect();
                let href = dest_url.strip_prefix("mailto:").unwrap_or(&dest_url);
                if text != href {
                    segs.extend(inner);
                    segs.push(Seg::new(format!(" ({href})"), theme.fg(Fg::LinkUrl)));
                } else {
                    segs.extend(inner);
                }
            }
            Event::Start(Tag::Image { .. }) => {
                // pi drops images in inline rendering; so does hi (alt text
                // and all — a recorded divergence).
                let mut depth = 0usize;
                while *i < events.len() {
                    match &events[*i] {
                        Event::Start(Tag::Image { .. }) => depth += 1,
                        Event::End(TagEnd::Image) => {
                            depth -= 1;
                            if depth == 0 {
                                *i += 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                    *i += 1;
                }
            }
            _ => *i += 1,
        }
    }
    segs
}

/// Consume bare text/code events (used outside paragraphs) as inline
/// content.
fn inline_bare(events: &[Event], i: &mut usize, base: Style, theme: &Theme) -> Vec<Seg> {
    let mut segs = Vec::new();
    while *i < events.len() {
        match &events[*i] {
            Event::Text(text) => {
                segs.push(Seg::new(text.to_string(), base));
                *i += 1;
            }
            Event::Code(code) => {
                segs.push(Seg::new(code.to_string(), theme.fg(Fg::Code)));
                *i += 1;
            }
            Event::SoftBreak => {
                segs.push(Seg::new(" ", base));
                *i += 1;
            }
            _ => break,
        }
    }
    segs
}

/// A table's cells, collected as styled segments.
struct TableAcc {
    header: Vec<Vec<Seg>>,
    rows: Vec<Vec<Vec<Seg>>>,
}

fn table_cells(events: &[Event], i: &mut usize, theme: &Theme, base: Style) -> TableAcc {
    let mut acc = TableAcc {
        header: Vec::new(),
        rows: Vec::new(),
    };
    let mut row: Vec<Vec<Seg>> = Vec::new();
    let mut in_head = false;
    while *i < events.len() {
        match events[*i].clone() {
            Event::Start(Tag::TableHead) => {
                *i += 1;
                in_head = true;
                row = Vec::new();
            }
            Event::Start(Tag::TableRow) => {
                *i += 1;
                if in_head {
                    acc.header = std::mem::take(&mut row);
                    in_head = false;
                } else {
                    row = Vec::new();
                }
            }
            Event::Start(Tag::TableCell) => {
                *i += 1;
                let segs = inline(events, i, TagEnd::TableCell, base, theme);
                row.push(segs);
            }
            Event::End(TagEnd::TableRow) => {
                *i += 1;
                acc.rows.push(std::mem::take(&mut row));
            }
            Event::End(TagEnd::Table) => {
                *i += 1;
                if in_head {
                    acc.header = row;
                } else if !row.is_empty() {
                    acc.rows.push(row);
                }
                return acc;
            }
            _ => *i += 1,
        }
    }
    acc
}

/// Longest word in a cell, capped like pi (`maxUnbrokenWordWidth = 30`).
fn min_word_width(segs: &[Seg]) -> usize {
    segs.iter()
        .flat_map(|s| s.text.split_whitespace())
        .map(display_width)
        .max()
        .unwrap_or(1)
        .clamp(1, 30)
}

/// pi's table layout: `┌─┬─┐` borders, bold header, wrapped cells,
/// separators between all rows; proportional column shrink when the
/// natural widths overflow.
fn table_rows(acc: &TableAcc, aligns: &[Alignment], width: usize) -> Vec<Line<'static>> {
    let n = acc.header.len();
    if n == 0 {
        return Vec::new();
    }
    let overhead = 3 * n + 1;
    let avail = width.saturating_sub(overhead);
    if avail < n {
        // Too narrow for a stable grid: fall back to pipe-joined rows.
        let mut rows = Vec::new();
        for cells in std::iter::once(&acc.header).chain(acc.rows.iter()) {
            let mut segs = Vec::new();
            for (i, cell) in cells.iter().enumerate() {
                if i > 0 {
                    segs.push(Seg::plain(" | "));
                }
                segs.extend(cell.iter().cloned());
            }
            rows.extend(wrap_segments(&segs, width).into_iter().map(segs_into_line));
        }
        return rows;
    }

    let mut natural: Vec<usize> = Vec::with_capacity(n);
    let mut mins: Vec<usize> = Vec::with_capacity(n);
    for col in 0..n {
        let mut w = acc
            .header
            .get(col)
            .map(|c| c.iter().map(|s| s.width()).sum())
            .unwrap_or(0);
        let mut min = acc.header.get(col).map(|c| min_word_width(c)).unwrap_or(1);
        for row in &acc.rows {
            if let Some(cell) = row.get(col) {
                w = w.max(cell.iter().map(|s| s.width()).sum());
                min = min.max(min_word_width(cell));
            }
        }
        natural.push(w.max(1));
        mins.push(min);
    }

    let widths = if natural.iter().sum::<usize>() <= avail {
        natural
    } else {
        // Shrink to minimums, then grow proportionally toward natural.
        let min_total: usize = mins.iter().sum();
        if min_total >= avail {
            mins
        } else {
            let extra = avail - min_total;
            let potential: usize = natural
                .iter()
                .zip(&mins)
                .map(|(nat, min)| nat.saturating_sub(*min))
                .sum();
            let mut widths: Vec<usize> = mins
                .iter()
                .zip(&natural)
                .map(|(min, nat)| {
                    let delta = nat.saturating_sub(*min);
                    // `potential > 0` implied: delta > 0 only when natural
                    // exceeds the min, which requires potential > 0.
                    min + delta.checked_mul(extra).unwrap_or(0) / potential.max(1)
                })
                .collect();
            let allocated: usize = widths.iter().sum();
            let mut leftover = avail - allocated;
            let mut col = 0;
            while leftover > 0 && !widths.is_empty() {
                widths[col % n] += 1;
                leftover -= 1;
                col += 1;
            }
            widths
        }
    };

    let pad_cell = |segs: &[Seg], col: usize| -> Vec<Vec<Seg>> {
        let col_w = widths[col];
        match aligns.get(col) {
            Some(Alignment::Right) => {
                let w: usize = segs.iter().map(|s| s.width()).sum();
                vec![padded_line(segs, col_w.saturating_sub(w), col_w)]
            }
            Some(Alignment::Center) => {
                let w: usize = segs.iter().map(|s| s.width()).sum();
                vec![padded_line(segs, (col_w.saturating_sub(w)) / 2, col_w)]
            }
            _ => wrap_segments(segs, col_w),
        }
    };
    /// One line: `lead` spaces, then the cell, padded to `col_w`.
    fn padded_line(segs: &[Seg], lead: usize, col_w: usize) -> Vec<Seg> {
        let mut out: Vec<Seg> = Vec::with_capacity(segs.len() + 2);
        if lead > 0 {
            out.push(Seg::new(" ".repeat(lead), Style::new()));
        }
        let mut w = lead;
        for s in segs {
            out.push(s.clone());
            w += s.width();
        }
        if w < col_w {
            out.push(Seg::new(" ".repeat(col_w - w), Style::new()));
        }
        out
    }

    let draw_row = |cells: &[Vec<Seg>], header: bool| -> Vec<Line<'static>> {
        let wrapped: Vec<Vec<Vec<Seg>>> = (0..n)
            .map(|col| match cells.get(col) {
                Some(cell) => {
                    let mut prepared = cell.clone();
                    if header {
                        for seg in &mut prepared {
                            seg.style = seg.style.bold();
                        }
                    }
                    pad_cell(&prepared, col)
                }
                None => vec![vec![Seg::new(" ".repeat(widths[col]), Style::new())]],
            })
            .collect();
        let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
        (0..height)
            .map(|line| {
                let mut spans = vec![Span::raw("│ ")];
                for (col, cell_lines) in wrapped.iter().enumerate() {
                    if col > 0 {
                        spans.push(Span::raw(" │ "));
                    }
                    let segs = cell_lines
                        .get(line)
                        .cloned()
                        .unwrap_or_else(|| vec![Seg::new(" ".repeat(widths[col]), Style::new())]);
                    let used: usize = segs.iter().map(|s| s.width()).sum();
                    spans.extend(segs.into_iter().map(|s| Span::styled(s.text, s.style)));
                    if used < widths[col] {
                        spans.push(Span::raw(" ".repeat(widths[col] - used)));
                    }
                }
                spans.push(Span::raw(" │"));
                Line::from(spans)
            })
            .collect()
    };

    let border = |left: char, mid: char, right: char| -> Line<'static> {
        let cells: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
        Line::raw(format!(
            "{left}─{}─{right}",
            cells.join(&format!("─{mid}─"))
        ))
    };

    let mut out = vec![border('┌', '┬', '┐')];
    out.extend(draw_row(&acc.header, true));
    out.push(border('├', '┼', '┤'));
    for (idx, row) in acc.rows.iter().enumerate() {
        out.extend(draw_row(row, false));
        if idx + 1 < acc.rows.len() {
            out.push(border('├', '┼', '┤'));
        }
    }
    out.push(border('└', '┴', '┘'));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Modifier;

    fn plain(rows: &[Line<'static>]) -> Vec<String> {
        rows.iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn paragraph_wraps_at_word_boundaries() {
        let rows = markdown_rows("alpha beta gamma", 12, Style::new());
        assert_eq!(plain(&rows), vec!["alpha beta ", "gamma"]);
    }

    #[test]
    fn h1_is_bold_underlined_heading_color() {
        let rows = markdown_rows("# Title", 40, Style::new());
        let span = &rows[0].spans[0];
        assert_eq!(span.content, "Title");
        assert!(span.style.add_modifier.contains(Modifier::BOLD));
        assert!(span.style.add_modifier.contains(Modifier::UNDERLINED));
        assert_eq!(span.style.fg, THEME.fg(Fg::Heading).fg);
    }

    #[test]
    fn h3_keeps_its_hash_prefix() {
        let rows = markdown_rows("### deep", 40, Style::new());
        assert_eq!(plain(&rows), vec!["### deep"]);
        assert!(rows[0].spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn code_block_restates_fences_and_indents() {
        let rows = markdown_rows("```rust\nlet x = 1;\nlet y = 2;\n```", 40, Style::new());
        assert_eq!(
            plain(&rows),
            vec!["```rust", "  let x = 1;", "  let y = 2;", "```",]
        );
        assert_eq!(rows[0].spans[0].style.fg, THEME.fg(Fg::CodeBlockBorder).fg);
        assert_eq!(rows[1].spans[0].style.fg, THEME.fg(Fg::CodeBlock).fg);
    }

    #[test]
    fn blank_line_between_blocks() {
        let rows = markdown_rows("one\n\ntwo", 40, Style::new());
        assert_eq!(plain(&rows), vec!["one", "", "two"]);
    }

    #[test]
    fn inline_code_takes_code_role() {
        let rows = markdown_rows("run `hi --help` now", 40, Style::new());
        let code_fg = THEME.fg(Fg::Code).fg;
        let coded: String = rows[0]
            .spans
            .iter()
            .filter(|s| s.style.fg == code_fg)
            .map(|s| s.content.to_string())
            .collect();
        assert_eq!(coded, "hi --help");
    }

    #[test]
    fn bold_italic_strikethrough_carry_modifiers() {
        let rows = markdown_rows("**b** *i* ~~s~~", 40, Style::new());
        // Inline styles fragment per word — assert by filtering spans.
        let with = |m: Modifier| -> String {
            rows[0]
                .spans
                .iter()
                .filter(|s| s.style.add_modifier.contains(m))
                .map(|s| s.content.to_string())
                .collect()
        };
        assert_eq!(with(Modifier::BOLD), "b");
        assert_eq!(with(Modifier::ITALIC), "i");
        assert_eq!(with(Modifier::CROSSED_OUT), "s");
    }

    #[test]
    fn link_prints_url_when_text_differs() {
        let rows = markdown_rows("[docs](https://x.example)", 40, Style::new());
        assert_eq!(plain(&rows), vec!["docs (https://x.example)"]);
        assert!(
            rows[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::UNDERLINED)
        );
        assert_eq!(rows[0].spans[1].style.fg, THEME.fg(Fg::LinkUrl).fg);
    }

    #[test]
    fn autolink_stays_unsuffixed() {
        let rows = markdown_rows("<https://x.example>", 40, Style::new());
        assert_eq!(plain(&rows), vec!["https://x.example"]);
    }

    #[test]
    fn unordered_list_markers_and_continuation() {
        let rows = markdown_rows("- alpha beta\ngamma", 14, Style::new());
        assert_eq!(plain(&rows), vec!["- alpha beta ", "  gamma"]);
        assert_eq!(rows[0].spans[0].style.fg, THEME.fg(Fg::ListBullet).fg);
    }

    #[test]
    fn ordered_lists_number_from_start() {
        let rows = markdown_rows("3. c\n4. d", 40, Style::new());
        assert_eq!(plain(&rows), vec!["3. c", "4. d"]);
    }

    #[test]
    fn task_lists_render_checkboxes() {
        let rows = markdown_rows("- [x] done\n- [ ] todo", 40, Style::new());
        assert_eq!(plain(&rows), vec!["- [x] done", "- [ ] todo"]);
    }

    #[test]
    fn nested_lists_indent_four_per_level() {
        let rows = markdown_rows("- a\n    - b", 40, Style::new());
        assert_eq!(plain(&rows), vec!["- a", "    - b"]);
    }

    #[test]
    fn blockquote_gets_border_and_italic() {
        let rows = markdown_rows("> quoted words", 40, Style::new());
        assert_eq!(plain(&rows), vec!["│ quoted words"]);
        assert!(
            rows[0].spans[1]
                .style
                .add_modifier
                .contains(Modifier::ITALIC)
        );
        assert_eq!(rows[0].spans[0].style.fg, THEME.fg(Fg::QuoteBorder).fg);
    }

    #[test]
    fn hr_fills_width_capped_at_eighty() {
        assert_eq!(
            plain(&markdown_rows("---", 10, Style::new())),
            vec!["──────────"]
        );
        assert_eq!(
            plain(&markdown_rows("---", 200, Style::new()))[0]
                .chars()
                .count(),
            80
        );
    }

    #[test]
    fn table_renders_box_grid_with_bold_header() {
        let md = "| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |";
        let rows = markdown_rows(md, 40, Style::new());
        assert_eq!(
            plain(&rows),
            vec![
                "┌───┬───┐",
                "│ a │ b │",
                "├───┼───┤",
                "│ 1 │ 2 │",
                "├───┼───┤",
                "│ 3 │ 4 │",
                "└───┴───┘",
            ]
        );
        assert!(rows[1].spans[1].style.add_modifier.contains(Modifier::BOLD));
        assert!(!rows[3].spans[1].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn wide_cells_shrink_and_wrap() {
        let md = "| head | x |\n|------|---|\n| alpha beta gamma | y |";
        let rows = markdown_rows(md, 24, Style::new());
        let text = plain(&rows);
        assert!(text[0].starts_with('┌'));
        assert!(text.iter().all(|l| display_width(l) <= 24), "{text:?}");
        // The long cell wrapped rather than overflowing.
        assert!(text.join(" ").contains("alpha"));
        assert!(text.join(" ").contains("gamma"));
    }

    #[test]
    fn degenerate_width_still_yields_rows() {
        let rows = markdown_rows("some words here", 0, Style::new());
        assert!(!rows.is_empty());
    }

    #[test]
    fn styled_wrap_hard_splits_oversized_words() {
        let segs = vec![Seg::plain("abcdefgh")];
        let lines = wrap_segments(&segs, 3);
        let joined: Vec<String> = lines
            .iter()
            .map(|l| l.iter().map(|s| s.text.as_str()).collect())
            .collect();
        assert_eq!(joined, vec!["abc", "def", "gh"]);
    }
}
