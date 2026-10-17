//! The flushed-prefix transcript window (AD-0016). The scrollback
//! above the viewport holds a prefix of the finalized transcript; the
//! live stream area shows the tail that follows it. Rows move from
//! tail to scrollback exactly once, in order, through
//! [`crate::ui::segments_into`] — images included — so the seam
//! between the two is invisible and the stream area is never blank
//! while the transcript has rows. pi's inline mode works the same way.
//!
//! The flush cursor is segment-granular by design: text segments
//! sub-flush row by row, image segments move atomically (a kitty
//! image cannot be split across the seam), and an image at the very
//! end of the transcript over-flushes past the window target so a
//! final image is never stranded invisible.

use hi_core::Message;
use ratatui::style::Stylize;
use ratatui::text::Line;

use crate::kitty::Protocol;
use crate::ui::{
    Segment, TranscriptBlock, block_rows, block_segments, derive_blocks, segments_height,
};

/// One finalized transcript block, cached at the width it was wrapped.
#[derive(Clone)]
struct CachedBlock {
    /// The source block — kept so a resize can re-wrap unflushed rows.
    source: TranscriptBlock,
    /// Scrollback representation: text rows plus real image segments
    /// when the protocol allows.
    segments: Vec<Segment>,
    /// Viewport rows: the text rows only. The managed region re-renders
    /// every frame and cannot host images (AD-0010); unflushed image
    /// rows are simply not visible until they flush.
    rows: Vec<Line<'static>>,
    /// `segments_height(&segments)` — the block's scrollback rows.
    height: usize,
}

/// Where the next flush continues: everything strictly before this
/// position lives in the terminal's scrollback.
#[derive(Clone, Copy, Default)]
struct Cursor {
    block: usize,
    seg: usize,
    /// Row offset inside the current text segment.
    row: usize,
}

pub(crate) struct Scroll {
    blocks: Vec<CachedBlock>,
    /// How much of `messages` is folded into `blocks` — the old
    /// `printed` watermark. A trailing unpaired call stays live until
    /// its result lands, so it is not folded yet.
    seen: usize,
    /// The width `blocks` were wrapped at.
    width: u16,
    cursor: Cursor,
    /// Rows already handed to `insert_before`.
    flushed: usize,
    /// Sum of every block's height.
    total: usize,
}

impl Scroll {
    /// Seeded with the welcome line: it sits in the window just above
    /// the input until the conversation pushes it into scrollback.
    pub(crate) fn new() -> Self {
        let line = Line::from(vec!["hi".bold(), " · say something, ctrl-c to quit".dim()]);
        let welcome = CachedBlock {
            source: TranscriptBlock::Message(Message::System(String::new())),
            segments: vec![Segment::Text(vec![line.clone()])],
            rows: vec![line],
            height: 1,
        };
        Self {
            blocks: vec![welcome],
            seen: 0,
            width: 0,
            cursor: Cursor::default(),
            flushed: 0,
            total: 1,
        }
    }

    /// Fold any not-yet-cached messages into blocks, re-wrapping
    /// unflushed blocks when the width changed. Flushed rows are
    /// immutable — they are already in the terminal's scrollback —
    /// so a resize may re-print a couple of seam rows once (pi's
    /// behavior too).
    pub(crate) fn sync(&mut self, messages: &[Message], width: u16, caps: Option<Protocol>) {
        if self.width != width {
            self.width = width;
            self.rebuild_from_cursor(messages, caps);
        }
        if self.seen >= messages.len() {
            return;
        }
        let (blocks, pending) = derive_blocks(&messages[self.seen..]);
        for block in blocks {
            self.push_block(block, width, caps);
        }
        self.seen = messages.len() - usize::from(pending.is_some());
    }

    /// A session switch replays the new history below the old
    /// conversation: the flush position stays, the fold pointer
    /// restarts, and the next `sync` appends the new messages as
    /// fresh blocks.
    pub(crate) fn refold(&mut self) {
        self.seen = 0;
    }

    fn push_block(&mut self, block: TranscriptBlock, width: u16, caps: Option<Protocol>) {
        let (segments, rows, height) = Self::cached(&block, width, caps, true);
        self.total += height;
        self.blocks.push(CachedBlock {
            source: block,
            segments,
            rows,
            height,
        });
    }

    /// A block's cached rendering. `separator` prepends the seam row —
    /// exactly one blank row above the block (AD-0051): the seam is
    /// part of the block, so it flushes, scrolls, and survives resizes
    /// with it. Every block after the welcome seed carries one; filled
    /// blocks keep their own fill rows inside, so two backgrounds never
    /// touch and plain blocks end on text rows either way.
    fn cached(
        block: &TranscriptBlock,
        width: u16,
        caps: Option<Protocol>,
        separator: bool,
    ) -> (Vec<Segment>, Vec<Line<'static>>, usize) {
        let mut segments = block_segments(block, width, caps);
        let mut rows = block_rows(block, width);
        if separator {
            segments.insert(0, Segment::Text(vec![Line::raw("")]));
            rows.insert(0, Line::raw(""));
        }
        let height = segments_height(&segments) as usize;
        (segments, rows, height)
    }

    fn rebuild_from_cursor(&mut self, _messages: &[Message], caps: Option<Protocol>) {
        // Snap the cursor to the partial block's start and give those
        // rows back: they will flush again at the new width. The old
        // copies in scrollback re-print — at most one block, once per
        // resize, invisible in practice.
        let back = self.rows_before_cursor_in_block();
        self.flushed = self.flushed.saturating_sub(back);
        self.cursor = Cursor {
            block: self.cursor.block,
            seg: 0,
            row: 0,
        };
        // Index 0 is the welcome seed: synthetic and width-invariant,
        // so it is never re-derived (re-deriving would clobber it into
        // blank `System("")` rows — it lives only here).
        for block in self.blocks.iter_mut().skip(self.cursor.block.max(1)) {
            let (segments, rows, height) = Self::cached(&block.source, self.width, caps, true);
            block.segments = segments;
            block.rows = rows;
            block.height = height;
        }
        self.total = self.blocks.iter().map(|b| b.height).sum();
    }

    fn rows_before_cursor_in_block(&self) -> usize {
        let Some(block) = self.blocks.get(self.cursor.block) else {
            return 0;
        };
        let mut rows = 0;
        for segment in block.segments.iter().take(self.cursor.seg) {
            if let Segment::Text(text) = segment {
                rows += text.len();
            }
        }
        rows + self.cursor.row
    }

    /// Push rows into the scrollback until only `window_rows` of the
    /// transcript remain live, returning the chunk to hand to
    /// `Terminal::insert_before` — `(height, segments)`. Text
    /// segments sub-flush row-wise; image segments move atomically,
    /// except one at the very end of the transcript (with no live
    /// content following) over-flushes so it can never strand
    /// invisible.
    pub(crate) fn take_flush(
        &mut self,
        window_rows: usize,
        live_follows: bool,
    ) -> Option<(u16, Vec<Segment>)> {
        let target = self.total.saturating_sub(window_rows);
        let mut chunk: Vec<Segment> = Vec::new();
        let mut chunk_rows = 0usize;
        while self.flushed + chunk_rows < target {
            let Some(block) = self.blocks.get(self.cursor.block) else {
                break;
            };
            let Some(segment) = block.segments.get(self.cursor.seg) else {
                self.cursor.block += 1;
                self.cursor.seg = 0;
                self.cursor.row = 0;
                continue;
            };
            match segment {
                Segment::Text(rows) => {
                    let remaining = rows.len() - self.cursor.row;
                    let budget = target - self.flushed - chunk_rows;
                    let take = remaining.min(budget);
                    let drained: Vec<Line<'static>> =
                        rows[self.cursor.row..self.cursor.row + take].to_vec();
                    self.cursor.row += take;
                    chunk_rows += take;
                    if self.cursor.row >= rows.len() {
                        self.cursor.seg += 1;
                        self.cursor.row = 0;
                    }
                    chunk.push(Segment::Text(drained));
                }
                Segment::Image { rows, .. } => {
                    let fits = self.flushed + chunk_rows + *rows as usize <= target;
                    let last_content = self.cursor.block + 1 == self.blocks.len()
                        && self.cursor.seg + 1 == block.segments.len();
                    if fits || (last_content && !live_follows) {
                        let copied = segment.clone();
                        chunk_rows += *rows as usize;
                        self.cursor.seg += 1;
                        self.cursor.row = 0;
                        chunk.push(copied);
                    } else {
                        break;
                    }
                }
            }
        }
        if chunk_rows == 0 {
            return None;
        }
        self.flushed += chunk_rows;
        Some((chunk_rows.max(1) as u16, chunk))
    }

    /// The window's rows: every unflushed text row, in order. The
    /// stream area trims to what fits — the newest rows win.
    pub(crate) fn window_rows(&self) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        for (index, block) in self.blocks.iter().enumerate() {
            let skip = if index < self.cursor.block {
                continue;
            } else if index == self.cursor.block {
                self.rows_before_cursor_in_block()
            } else {
                0
            };
            out.extend(block.rows.iter().skip(skip).cloned());
        }
        out
    }

    #[cfg(test)]
    pub(crate) fn totals(&self) -> (usize, usize) {
        (self.flushed, self.total)
    }

    #[cfg(test)]
    pub(crate) fn seen(&self) -> usize {
        self.seen
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> Message {
        Message::User(text.into())
    }

    fn scroll_with(messages: &[Message], width: u16) -> Scroll {
        let mut scroll = Scroll::new();
        scroll.sync(messages, width, None);
        scroll
    }

    #[test]
    fn the_welcome_line_survives_rebuilds() {
        let mut scroll = Scroll::new();
        assert_eq!(scroll.window_rows().len(), 1);
        scroll.sync(&[], 80, None);
        let rows = scroll.window_rows();
        let text: Vec<String> = rows
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.clone())
                    .collect::<String>()
            })
            .collect();
        assert_eq!(
            text,
            vec!["hi · say something, ctrl-c to quit".to_string()],
            "{text:?}"
        );
    }

    /// Window texts, one string per row.
    fn texts(scroll: &Scroll) -> Vec<String> {
        scroll
            .window_rows()
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
            .collect()
    }

    #[test]
    fn welcome_seeds_one_window_row() {
        let scroll = Scroll::new();
        assert_eq!(scroll.totals(), (0, 1));
        assert_eq!(scroll.window_rows().len(), 1);
        assert!(scroll.window_rows()[0].spans[0].content.contains("hi"));
    }

    #[test]
    fn any_two_blocks_are_separated_by_exactly_one_blank_row() {
        let scroll = scroll_with(&[user("hello"), Message::assistant("hi there")], 40);
        // welcome, separator, [fill, hello, fill], separator, hi there
        let rows = texts(&scroll);
        assert_eq!(rows.len(), 7, "{rows:?}");
        assert_eq!(rows[5], "", "exactly one separator row");
        assert_eq!(rows[6].trim(), "hi there");
        assert!(
            scroll.window_rows()[5].spans.is_empty(),
            "the seam is plain"
        );
        // Every block after the seed leads with its separator (so the
        // seam survives flushing — it is part of the block).
        for block in scroll.blocks.iter().skip(1) {
            assert!(block.rows.first().unwrap().spans.is_empty());
        }
        // The filled block keeps its fill rows (rule: bg blocks pad):
        // seam + fill + text + fill.
        assert_eq!(scroll.blocks[1].rows.len(), 4);
    }

    #[test]
    fn two_filled_blocks_get_one_separator_between_their_fills() {
        let call = |id: &str| {
            Message::ToolCall(hi_core::ToolCall {
                id: id.into(),
                name: "read".into(),
                arguments: "{}".into(),
            })
        };
        let result = |id: &str| {
            Message::ToolResult(hi_core::ToolResult {
                call_id: id.into(),
                name: "read".into(),
                content: "ok".into(),
                is_error: false,
                images: Vec::new(),
            })
        };
        let scroll = scroll_with(
            &[user("go"), call("a"), result("a"), call("b"), result("b")],
            40,
        );
        // welcome, user, tool, tool — one block each plus the seed.
        assert_eq!(scroll.blocks.len(), 4);
        let fills = |rows: &[Line<'static>]| {
            rows.iter()
                .filter(|r| r.spans.iter().any(|s| s.style.bg.is_some()))
                .count()
        };
        for i in [1, 2, 3] {
            let rows = &scroll.blocks[i].rows;
            assert_eq!(rows.first().unwrap().spans.len(), 0, "leading seam: {i}");
            // padded_block tints every span, so every row but the seam
            // is fill: the block kept its top and bottom fill rows.
            assert_eq!(fills(rows), rows.len() - 1, "bg blocks stay filled: {i}");
        }
        // tool|tool: fill of the first, seam, fill of the second.
        // Exactly one truly empty row per seam: three seams, no others
        // (fill rows are whitespace, not empty).
        let rows = texts(&scroll);
        let seams = rows.iter().filter(|r| r.is_empty()).count();
        assert_eq!(seams, 3, "{rows:?}");
    }

    #[test]
    fn thinking_and_reply_are_separate_blocks_with_one_blank_row() {
        let message = Message::Assistant(vec![
            hi_core::message::AssistantBlock::Thinking {
                thinking: "weigh it".into(),
                signature: None,
                redacted: false,
            },
            hi_core::message::AssistantBlock::Text("the reply".into()),
        ]);
        let scroll = scroll_with(&[user("go"), message], 40);
        assert_eq!(scroll.blocks.len(), 4); // welcome + user + thinking + reply
        let rows = texts(&scroll);
        let n = rows.len();
        assert_eq!(rows[n - 3].trim(), "weigh it");
        assert_eq!(rows[n - 2], "", "one blank between thinking and reply");
        assert_eq!(rows[n - 1].trim(), "the reply");
    }

    #[test]
    fn folding_caches_blocks_and_advances_seen() {
        let scroll = scroll_with(&[user("hello"), Message::assistant("hi there")], 40);
        // Welcome + two message blocks.
        assert_eq!(scroll.blocks.len(), 3);
        assert_eq!(scroll.seen(), 2);
        assert_eq!(scroll.totals(), (0, scroll.total));
    }

    #[test]
    fn flush_reaches_the_target_and_the_window_shows_the_tail() {
        let mut scroll = scroll_with(
            &[user("hello"), Message::assistant("hi there"), user("again")],
            40,
        );
        let total = scroll.total;
        // Window of 3: everything above the last 3 rows goes to the
        // scrollback in one chunk.
        let (height, _) = scroll.take_flush(3, false).expect("chunk");
        let (flushed, _) = scroll.totals();
        assert_eq!(flushed, total - 3);
        assert_eq!(height as usize, total - 3);
        // The window now holds exactly the tail.
        assert_eq!(scroll.window_rows().len(), 3);
        // Converged: nothing more to flush at this window size.
        assert!(scroll.take_flush(3, false).is_none());
    }

    #[test]
    fn a_trailing_call_stays_unfolded_until_its_result_lands() {
        let call = Message::ToolCall(hi_core::ToolCall {
            id: "call_1".into(),
            name: "read".into(),
            arguments: "{}".into(),
        });
        let mut scroll = scroll_with(&[user("go"), call.clone()], 40);
        assert_eq!(scroll.seen(), 1); // the call is live, not cached
        scroll.sync(
            &[
                user("go"),
                call,
                Message::ToolResult(hi_core::ToolResult {
                    call_id: "call_1".into(),
                    name: "read".into(),
                    content: "done".into(),
                    is_error: false,
                    images: Vec::new(),
                }),
            ],
            40,
            None,
        );
        assert_eq!(scroll.seen(), 3);
    }

    #[test]
    fn width_change_rewraps_unflushed_blocks_and_keeps_the_flushed_prefix() {
        let mut scroll = scroll_with(
            &[user("a longer first message"), Message::assistant("reply")],
            40,
        );
        scroll.take_flush(2, false).expect("flush");
        let (flushed_before, _) = scroll.totals();
        // Same messages at a narrow width: unflushed blocks re-wrap.
        // The cursor snaps back to the partial block's start (at most
        // one block re-prints at the seam) and then converges again.
        scroll.sync(
            &[user("a longer first message"), Message::assistant("reply")],
            12,
            None,
        );
        let (after_resize, _) = scroll.totals();
        assert!(after_resize <= flushed_before);
        scroll.take_flush(2, false);
        let (flushed, total) = scroll.totals();
        assert_eq!(flushed, total - 2);
    }

    #[test]
    fn session_refold_replays_messages_as_new_blocks() {
        let msgs = [user("hello")];
        let mut scroll = scroll_with(&msgs, 40);
        scroll.refold();
        scroll.sync(&msgs, 40, None);
        assert_eq!(scroll.seen(), 1);
        // Welcome + the original block + the replayed one.
        assert_eq!(scroll.blocks.len(), 3);
    }

    #[test]
    fn a_tail_image_over_flushes_so_it_never_strands_invisible() {
        let mut scroll = Scroll::new();
        scroll.sync(&[user("draw")], 40, None);
        // Hand-build a final block whose image is taller than any
        // reasonable window: without the over-flush rule it would
        // never reach the scrollback.
        let block = crate::ui::TranscriptBlock::Tool(crate::ui::ToolExec {
            call: None,
            result: Some(hi_core::ToolResult {
                call_id: "call_1".into(),
                name: "read".into(),
                content: "pic".into(),
                is_error: false,
                images: Vec::new(),
            }),
        });
        let segments = vec![
            crate::ui::Segment::Text(vec![Line::from("tool")]),
            crate::ui::Segment::Image {
                sequence: "<kitty>".to_string(),
                rows: 50,
            },
        ];
        scroll.total += crate::ui::segments_height(&segments) as usize + 1;
        scroll.blocks.push(CachedBlock {
            source: block,
            segments,
            rows: vec![Line::from("tool")],
            height: 51,
        });
        // A window of 10 cannot fit a 50-row image; the tail rule
        // flushes it anyway.
        let (height, chunk) = scroll.take_flush(10, false).expect("over-flush");
        assert!(height as usize >= 50);
        assert!(
            chunk
                .iter()
                .any(|s| matches!(s, crate::ui::Segment::Image { .. }))
        );
    }
}
