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

/// How many rows `wrap` would produce — the number the input area
/// needs to grow by.
pub fn wrapped_row_count(text: &str, width: usize) -> usize {
    wrap(text, width).len()
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
