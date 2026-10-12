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
/// Trailing spaces stay attached to their word; a word wider than a
/// whole row is hard-split at whatever cell boundary fits. Joining
/// the rows is not guaranteed to reproduce the input byte-for-byte
/// (dropped breaks), but concatenating the rows' characters does.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        // Degenerate — nowhere to render. One row, untouched.
        return vec![text.to_string()];
    }

    let mut rows: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;

    // `split_inclusive(' ')` keeps each word's trailing space, so a
    // normal wrap never loses the space between words.
    for word in text.split_inclusive(' ') {
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
            // Hard-split an oversized word cell by cell — a wide char
            // still refuses to straddle the edge.
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
    rows
}

/// Byte offset of each row's first character under the same greedy
/// walk `wrap` uses. Rows are a contiguous partition of `text`
/// (concatenating `wrap`'s rows reproduces its characters), so the
/// ranges never need building explicitly — row *i* spans
/// `starts[i]..starts[i+1]`, the last row to the end of the text.
pub fn wrap_row_starts(text: &str, width: usize) -> Vec<usize> {
    if width == 0 {
        return vec![0];
    }
    let mut starts = vec![0usize];
    let mut current_width = 0usize;
    // Byte offset of the current word. A row's start is exactly the
    // offset where it opens — pushed the moment the walk opens it.
    let mut offset = 0usize;
    for word in text.split_inclusive(' ') {
        let word_width = display_width(word);
        if current_width + word_width <= width {
            current_width += word_width;
        } else if word_width > width {
            // The partial row closes here (its start was recorded
            // when it opened); this word's first char opens the next
            // row. Then hard-split cell by cell — a wide char still
            // refuses to straddle the edge.
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
/// on one; belt and suspenders for rendering).
pub fn cursor_pos(text: &str, cursor: usize, width: usize) -> (usize, usize) {
    let starts = wrap_row_starts(text, width);
    let mut cursor = cursor.min(text.len());
    while cursor > 0 && !text.is_char_boundary(cursor) {
        cursor -= 1;
    }
    // A cursor at a row boundary belongs to the previous row's end —
    // the caret sits after "hello ", not on "brave"'s first cell.
    let row = if cursor == 0 {
        0
    } else {
        starts
            .iter()
            .rposition(|&s| s < cursor)
            .expect("starts[0] == 0")
    };
    (row, display_width(&text[starts[row]..cursor]))
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
}
