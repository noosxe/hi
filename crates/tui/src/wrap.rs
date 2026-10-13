//! Display-width math for the input box and the streaming tail.
//!
//! Terminal columns ≠ characters: CJK glyphs and most emoji are two
//! cells wide, combining marks are zero. Everything that positions a
//! cursor or trims a tail must count columns, so it lives here.

use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

/// Terminal columns a string occupies.
pub fn display_width(text: &str) -> usize {
    text.width()
}

/// Greedy word-wrap into rows of at most `width` display columns.
/// Newlines break first — every line wraps independently and earns
/// at least one row. Within a line, trailing spaces stay attached to
/// their word; a word wider than a whole row is hard-split at
/// whatever cell boundary fits. The rows partition the input's bytes
/// (see `wrap_row_starts`) — a row's *text* is its span with the
/// trailing newline that closed the line stripped off, since the
/// break is the row boundary, not a glyph.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        // Degenerate — nowhere to render. One row, untouched.
        return vec![text.to_string()];
    }

    let mut rows: Vec<String> = Vec::new();
    for line in text.split('\n') {
        // Every line yields at least one row — an empty line is a
        // visibly blank row, which is the point of a newline.
        let mut current = String::new();
        let mut current_width = 0usize;

        // `split_inclusive(' ')` keeps each word's trailing space, so
        // a normal wrap never loses the space between words.
        for word in line.split_inclusive(' ') {
            let word_width = display_width(word);
            if current_width + word_width <= width {
                current.push_str(word);
                current_width += word_width;
                continue;
            }
            // Doesn't fit on this row: flush what's there first.
            if current_width > 0 {
                rows.push(std::mem::take(&mut current));
                current_width = 0;
            }
            if word_width > width {
                // Hard-split an oversized word cell by cell — a wide
                // char still refuses to straddle the edge.
                for ch in word.chars() {
                    let ch_width = ch.width().unwrap_or(0);
                    if current_width + ch_width > width {
                        rows.push(std::mem::take(&mut current));
                        current_width = 0;
                    }
                    current.push(ch);
                    current_width += ch_width;
                }
            } else {
                current.push_str(word);
                current_width = word_width;
            }
        }
        rows.push(current);
    }
    rows
}

/// Byte offset of each row's first character under the same greedy
/// walk `wrap` uses. Rows are a contiguous partition of `text`
/// (concatenating the rows' spans reproduces its bytes, newlines
/// riding at the end of the row that closed the line), so the ranges
/// never need building explicitly — row *i* spans
/// `starts[i]..starts[i+1]`, the last row to the end of the text.
/// Every line opens at least one row, so each line's first byte is a
/// row start.
pub fn wrap_row_starts(text: &str, width: usize) -> Vec<usize> {
    if width == 0 {
        return vec![0];
    }
    let mut starts = vec![0usize];
    let mut offset = 0usize;
    let lines: Vec<&str> = text.split('\n').collect();
    let last = lines.len() - 1;
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            // This line opens its own first row.
            starts.push(offset);
        }
        let mut current_width = 0usize;
        // Byte offset of the current word. A row's start is exactly
        // the offset where it opens — pushed the moment the walk
        // opens it.
        for word in line.split_inclusive(' ') {
            let word_width = display_width(word);
            if current_width + word_width <= width {
                current_width += word_width;
            } else if word_width > width {
                // The partial row closes here (its start was recorded
                // when it opened); this word's first char opens the
                // next row. Then hard-split cell by cell — a wide
                // char still refuses to straddle the edge.
                if current_width > 0 {
                    current_width = 0;
                    starts.push(offset);
                }
                for ch in word.chars() {
                    let ch_width = ch.width().unwrap_or(0);
                    if current_width + ch_width > width {
                        starts.push(offset);
                        current_width = 0;
                    }
                    current_width += ch_width;
                    offset += ch.len_utf8();
                }
                continue;
            } else {
                // This word opens the next row.
                starts.push(offset);
                current_width = word_width;
            }
            offset += word.len();
        }
        // The newline byte rides at the end of this line's last row's
        // span — it is the line break, not a glyph.
        if i < last {
            offset += 1;
        }
    }
    starts
}

/// How many rows the greedy wrap produces — the number the input
/// area needs to grow by.
pub fn wrapped_row_count(text: &str, width: usize) -> usize {
    wrap_row_starts(text, width).len()
}

/// Where the input's cursor lands in the wrapped rows: the row index
/// and the cursor's display column inside it — the coordinates the
/// real terminal caret parks at. `cursor` is clamped down to the
/// nearest char boundary first (the invariant says it already sits
/// on one; belt and suspenders for rendering). A caret just after a
/// newline belongs to the row that *opens* there — the blank line it
/// sits on — while a caret at a mere wrap boundary belongs to the
/// previous row's end.
pub fn cursor_pos(text: &str, cursor: usize, width: usize) -> (usize, usize) {
    let starts = wrap_row_starts(text, width);
    let mut cursor = cursor.min(text.len());
    while cursor > 0 && !text.is_char_boundary(cursor) {
        cursor -= 1;
    }
    let after_newline = cursor > 0 && text.as_bytes()[cursor - 1] == b'\n';
    let row = if cursor == 0 {
        0
    } else if after_newline {
        starts
            .iter()
            .position(|&s| s >= cursor)
            .expect("a newline always opens a row")
    } else {
        // A cursor at a row boundary belongs to the previous row's
        // end — the caret sits after "hello ", not on "brave"'s
        // first cell.
        starts
            .iter()
            .rposition(|&s| s < cursor)
            .expect("starts[0] == 0")
    };
    (row, display_width(&text[starts[row]..cursor]))
}

/// The inverse of `cursor_pos` for one row: the byte offset of
/// visual row `row`'s cell column `col` — the nearest char boundary
/// at or before that cell (a wide char owns both of its cells, so
/// the caret never splits one). Clamped to the row's last cell; the
/// caret never parks on the newline byte itself.
pub fn row_col_to_offset(text: &str, row: usize, col: usize, width: usize) -> usize {
    let starts = wrap_row_starts(text, width);
    let r = row.min(starts.len() - 1);
    let end = starts.get(r + 1).copied().unwrap_or(text.len());
    let mut cell = 0usize;
    let mut offset = starts[r];
    for ch in text[starts[r]..end].chars() {
        if ch == '\n' {
            break;
        }
        let ch_width = ch.width().unwrap_or(0);
        if cell + ch_width > col {
            break;
        }
        cell += ch_width;
        offset += ch.len_utf8();
    }
    offset
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_width_counts_columns_not_chars() {
        assert_eq!(display_width("a"), 1);
        assert_eq!(display_width("中"), 2);
        assert_eq!(display_width("🙂"), 2);
        assert_eq!(display_width("a🙂中"), 5);
    }

    #[test]
    fn row_starts_partition_exactly_what_wrap_renders() {
        for (text, width) in [
            ("hello brave world", 6),
            ("", 10),
            ("word", 80),
            ("supercalifragilistic", 5),
            ("a\u{4e2d}b wide", 4),
            ("trailing space ", 3),
        ] {
            let rows = wrap(text, width);
            let starts = wrap_row_starts(text, width);
            assert_eq!(rows.len(), starts.len(), "{text:?}/{width}");
            let bounds = starts.iter().copied().skip(1).chain([text.len()]);
            for (row, (s, e)) in rows.iter().zip(starts.iter().copied().zip(bounds)) {
                assert_eq!(*row, &text[s..e], "{text:?}/{width}");
            }
        }
    }

    #[test]
    fn cursor_pos_lands_on_the_right_row_and_column() {
        // "hello brave world" at width 6 wraps ["hello ", "brave ", "world"].
        assert_eq!(cursor_pos("hello brave world", 0, 6), (0, 0));
        assert_eq!(cursor_pos("hello brave world", 6, 6), (0, 6)); // after "hello "
        assert_eq!(cursor_pos("hello brave world", 8, 6), (1, 2)); // "br|"
        assert_eq!(cursor_pos("hello brave world", 17, 6), (2, 5)); // end
        // Wide char: the column counts display cells, not chars.
        assert_eq!(cursor_pos("a\u{4e2d}b", 4, 80), (0, 3)); // after "a中"
        // Out-of-range and mid-char cursors clamp.
        assert_eq!(cursor_pos("ab", 99, 80), (0, 2));
        assert_eq!(cursor_pos("a\u{4e2d}b", 2, 80), (0, 1));
    }

    #[test]
    fn ascii_wraps_on_words() {
        assert_eq!(
            wrap("hello brave world", 6),
            vec!["hello ", "brave ", "world"]
        );
    }

    #[test]
    fn wide_chars_wrap_on_cell_boundaries() {
        // Each 🙂 is two columns: two per 4-column row.
        assert_eq!(wrap("🙂🙂🙂", 4), vec!["🙂🙂", "🙂"]);
        assert_eq!(wrap("中文中文", 4), vec!["中文", "中文"]);
    }

    #[test]
    fn oversized_words_hard_split() {
        assert_eq!(wrap("abcdefgh", 3), vec!["abc", "def", "gh"]);
    }

    #[test]
    fn empty_input_is_one_empty_row() {
        assert_eq!(wrap("", 10), vec![""]);
    }

    #[test]
    fn one_row_when_it_fits() {
        assert_eq!(wrap("short", 80), vec!["short"]);
        assert_eq!(wrapped_row_count("short", 80), 1);
    }

    #[test]
    fn newlines_break_before_wrapping_and_earn_blank_rows() {
        assert_eq!(wrap("ab\ncd", 10), vec!["ab", "cd"]);
        // A newline between newlines is a visibly blank row.
        assert_eq!(wrap("a\n\nb", 10), vec!["a", "", "b"]);
        // A trailing newline opens one last empty row.
        assert_eq!(wrap("a\n", 10), vec!["a", ""]);
        // Each line wraps on its own budget.
        assert_eq!(
            wrap("hello brave\nworld", 6),
            vec!["hello ", "brave", "world"]
        );
        assert_eq!(wrapped_row_count("a\nb\nc", 80), 3);
    }

    #[test]
    fn row_starts_stay_a_byte_partition_with_newlines() {
        for (text, width) in [("a\n\nb", 10), ("a\n", 10), ("one two\nthree four", 8)] {
            let starts = wrap_row_starts(text, width);
            assert_eq!(*starts.first().unwrap(), 0, "{text:?}");
            let bounds = starts.iter().copied().skip(1).chain([text.len()]);
            for (s, e) in starts.iter().copied().zip(bounds) {
                assert!(s <= e && e <= text.len(), "{text:?} {s}..{e}");
            }
            // Contiguous: each row starts where the previous ended.
            for pair in starts.windows(2) {
                assert!(pair[0] <= pair[1], "{text:?}");
            }
        }
        // "a\n\nb": the newline bytes ride at the end of the rows
        // that closed their lines.
        assert_eq!(wrap_row_starts("a\n\nb", 10), vec![0, 2, 3]);
    }

    #[test]
    fn cursor_pos_after_a_newline_opens_the_next_row() {
        // A caret just after a newline sits on the row that opens
        // there — the blank line it is on — unlike a wrap boundary.
        assert_eq!(cursor_pos("a\n\nb", 1, 10), (0, 1)); // after "a"
        assert_eq!(cursor_pos("a\n\nb", 2, 10), (1, 0)); // on the blank row
        assert_eq!(cursor_pos("a\n\nb", 3, 10), (2, 0)); // after the second break
        assert_eq!(cursor_pos("a\n\nb", 4, 10), (2, 1)); // after "b"
        // A wrap boundary still belongs to the previous row's end.
        assert_eq!(cursor_pos("hello brave world", 6, 6), (0, 6));
    }

    #[test]
    fn row_col_to_offset_round_trips_cursor_pos() {
        for (text, width) in [
            ("a\n\nb", 10),
            ("one two\nthree four", 8),
            ("hello brave world", 6),
            ("\u{4e2d}\u{6587}x\nab", 10),
        ] {
            for cursor in 0..=text.len() {
                if !text.is_char_boundary(cursor) {
                    continue;
                }
                let (row, col) = cursor_pos(text, cursor, width);
                let back = row_col_to_offset(text, row, col, width);
                // Round-trips, except a caret parked after a wrap
                // break (it belongs to the previous row's end, whose
                // offset maps back one row).
                let back_pos = cursor_pos(text, back, width);
                assert!(
                    (back, back_pos.1) == (row, col) || cursor_pos(text, back, width).0 == row,
                    "{text:?}/{width} cursor {cursor}: ({row},{col}) -> {back} -> {back_pos:?}"
                );
            }
        }
        // Column clamps to the row's cells; a wide char is never split.
        assert_eq!(row_col_to_offset("\u{4e2d}x", 0, 1, 10), 0);
        assert_eq!(row_col_to_offset("\u{4e2d}x", 0, 2, 10), 3);
        // Far columns clamp to the row's end — before the newline.
        assert_eq!(row_col_to_offset("ab\ncd", 0, 99, 10), 2);
    }
}
