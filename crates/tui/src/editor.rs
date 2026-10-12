//! The composer's text-editing core: pure functions over a flat
//! `String` plus a byte-offset cursor (always on a `char` boundary —
//! the invariant every function here preserves and relies on).
//!
//! AD-0014: this is owned, not a crate. ratatui ships no input widget
//! (render-only by design); tui-textarea is a multi-line widget that
//! renders itself and its "history" is undo history; tui-input covers
//! only this thin slice. The app-shaped parts — prompt history with a
//! draft, the chord set including SUPER under the kitty keyboard
//! protocol — can't come from a crate at all. pi reached the same
//! conclusion with its own 2,000-line editor.
//!
//! Word unit everywhere: maximal runs of `is_alphanumeric() || '_'`,
//! other characters are separators — pi's word-move semantics, and
//! the right granularity for paths (`/tmp/hi-clipboard-x.png` jumps
//! at `-`, `/`, `.`).

/// Byte index of the character before `pos`, or `None` at the start.
pub fn prev_boundary(s: &str, pos: usize) -> Option<usize> {
    s[..pos].chars().next_back().map(|c| pos - c.len_utf8())
}

/// Byte index of the character after `pos`, or `None` at the end.
pub fn next_boundary(s: &str, pos: usize) -> Option<usize> {
    s[pos..].chars().next().map(|c| pos + c.len_utf8())
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Where a word-left lands from `pos`: skip separators backwards,
/// then word characters — emacs `backward-word`, clamped at 0.
pub fn word_left(s: &str, pos: usize) -> usize {
    let mut target = 0usize;
    let mut skipping_sep = true;
    for (i, c) in s[..pos].char_indices().rev() {
        if skipping_sep {
            if is_word_char(c) {
                skipping_sep = false;
                target = i;
            }
        } else {
            if !is_word_char(c) {
                return i + c.len_utf8();
            }
            target = i;
        }
    }
    target
}

/// Where a word-right lands from `pos`: skip separators forwards,
/// then word characters — emacs `forward-word`, clamped at the end.
pub fn word_right(s: &str, pos: usize) -> usize {
    let mut target = s.len();
    let mut skipping_sep = true;
    for (i, c) in s[pos..].char_indices() {
        let i = pos + i;
        if skipping_sep {
            if is_word_char(c) {
                skipping_sep = false;
                target = i + c.len_utf8();
            }
        } else {
            if !is_word_char(c) {
                return i;
            }
            target = i + c.len_utf8();
        }
    }
    target
}

/// The range a word-backspace removes at `pos` — the same unit the
/// move lands on, so Alt+Backspace composes with Alt+Left.
pub fn word_delete_range_back(s: &str, pos: usize) -> std::ops::Range<usize> {
    word_left(s, pos)..pos
}

/// The range a forward word-delete removes at `pos`.
pub fn word_delete_range_fwd(s: &str, pos: usize) -> std::ops::Range<usize> {
    pos..word_right(s, pos)
}

/// Insert one `char` at `pos`, returning the cursor past it.
pub fn insert_char(input: &mut String, pos: usize, c: char) -> usize {
    input.insert(pos, c);
    pos + c.len_utf8()
}

/// Remove `range`, returning the cursor (its start — where the
/// removed text used to begin).
pub fn delete_range(input: &mut String, range: std::ops::Range<usize>) -> usize {
    input.replace_range(range.clone(), "");
    range.start
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boundaries_walk_by_character_not_byte() {
        // `é` is two bytes, `中` three; the cursor never lands
        // inside either.
        let s = "aé中b"; // bytes: a=1, é=2..4, 中=4..7, b=7
        assert_eq!(prev_boundary(s, s.len()), Some(6)); // before b
        assert_eq!(next_boundary(s, 1), Some(3)); // past é
        assert_eq!(prev_boundary(s, 0), None);
        assert_eq!(next_boundary(s, s.len()), None);
    }

    #[test]
    fn word_left_skips_separators_then_word_chars() {
        assert_eq!(word_left("hello world", 11), 6);
        // Paths break at separators — the paste-path use case.
        // Paths break at separators — the paste-path use case.
        // From the end of "/tmp/hi-x.png": back to the start of "png".
        assert_eq!(word_left("/tmp/hi-x.png", 13), 10);
        assert_eq!(word_left("foo bar", 3), 0);
        assert_eq!(word_left("  leading", 9), 2);
        // At the start, clamped.
        assert_eq!(word_left("abc", 0), 0);
        // `_` counts as a word character.
        assert_eq!(word_left("a_b c", 3), 0);
    }

    #[test]
    fn word_right_mirrors_word_left() {
        assert_eq!(word_right("hello world", 0), 5);
        assert_eq!(word_right("/tmp/hi-x.png", 0), 4);
        assert_eq!(word_right("ab cd", 3), 5);
        assert_eq!(word_right("abc", 3), 3);
        assert_eq!(word_right("a_b c", 0), 3);
    }

    #[test]
    fn word_delete_ranges_match_the_move() {
        assert_eq!(word_delete_range_back("hello world", 11), 6..11);
        assert_eq!(word_delete_range_fwd("/tmp/hi", 0), 0..4);
        assert_eq!(word_delete_range_back("abc", 0), 0..0);
        assert_eq!(word_delete_range_fwd("abc", 3), 3..3);
    }

    #[test]
    fn insert_char_places_text_and_moves_the_cursor() {
        let mut s = String::from("acd");
        assert_eq!(insert_char(&mut s, 1, 'b'), 2);
        assert_eq!(s, "abcd");
        assert_eq!(insert_char(&mut s, 4, '中'), 7);
        assert_eq!(s, "abcd中");
        assert_eq!(insert_char(&mut s, 0, 'x'), 1);
        assert_eq!(s, "xabcd中");
    }

    #[test]
    fn delete_range_returns_the_removed_span_start() {
        let mut s = String::from("hello world");
        assert_eq!(delete_range(&mut s, 5..11), 5);
        assert_eq!(s, "hello");
        // Deleting at the start keeps the cursor parked there.
        let mut s = String::from("hello");
        assert_eq!(delete_range(&mut s, 0..1), 0);
        assert_eq!(s, "ello");
        let mut s = String::from("中b");
        assert_eq!(delete_range(&mut s, 0..3), 0);
        assert_eq!(s, "b");
    }
}
