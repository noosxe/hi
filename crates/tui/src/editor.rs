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

/// Byte offset of the current logical line's first byte — the line
/// the caret is on; a caret just after a newline belongs to the line
/// that opens there. The whole input while it is single-line.
pub fn line_start(s: &str, pos: usize) -> usize {
    let pos = pos.min(s.len());
    s.as_bytes()[..pos]
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |i| i + 1)
}

/// Byte offset just past the current logical line's last byte —
/// before its newline, or the end of the input.
pub fn line_end(s: &str, pos: usize) -> usize {
    let pos = pos.min(s.len());
    s.as_bytes()[pos..]
        .iter()
        .position(|&b| b == b'\n')
        .map_or(s.len(), |i| pos + i)
}

/// The range a forward word-delete removes at `pos`.
/// Direction of a pending jump-to-char (#67): `Ctrl+]` arms a
/// forward jump, `Ctrl+Alt+]` a backward one.
#[derive(Clone, Copy, PartialEq)]
pub enum Jump {
    Forward,
    Backward,
}

/// Byte offset of the first `target` strictly after `pos` — an
/// occurrence the cursor has not reached yet, so repeated jumps
/// step forward and die at none (pi's jump-to-char rule). The
/// search crosses newlines: the composer is one flat string.
pub fn jump_forward(s: &str, pos: usize, target: char) -> Option<usize> {
    let from = next_boundary(s, pos.min(s.len()))?;
    s[from..].find(target).map(|i| from + i)
}

/// Byte offset of the last `target` before `pos` — the character
/// immediately behind the cursor counts, so repeated jumps step
/// backward and die at none.
pub fn jump_backward(s: &str, pos: usize, target: char) -> Option<usize> {
    s[..pos.min(s.len())].rfind(target)
}

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
    fn line_bounds_scope_to_the_logical_line() {
        let s = "one\ntwo\nthree";
        // Line 1 "one" spans 0..3, line 2 "two" 4..7, line 3 "three" 8..13.
        assert_eq!(line_start(s, 0), 0);
        assert_eq!(line_end(s, 0), 3);
        assert_eq!(line_start(s, 2), 0);
        assert_eq!(line_start(s, 4), 4); // just after the break
        assert_eq!(line_end(s, 4), 7);
        assert_eq!(line_start(s, 8), 8);
        assert_eq!(line_end(s, 9), 13);
        // The whole input while it is single-line.
        assert_eq!(line_start("abc", 2), 0);
        assert_eq!(line_end("abc", 0), 3);
        // Out-of-range cursors clamp.
        assert_eq!(line_start(s, 99), 8);
        assert_eq!(line_end(s, 99), 13);
        // Word motions treat the newline as a separator: from the end,
        // word_left crosses the break into the previous line's word.
        assert_eq!(word_left("one\ntwo", 7), 4);
        assert_eq!(word_right("one\ntwo", 0), 3);
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
    #[test]
    fn jump_forward_steps_strictly_after_the_cursor_and_dies_at_none() {
        // "abca", cursor 0: the first jump lands on the second `a`,
        // because a jump takes you to an occurrence you have not
        // already reached — pi's stepping rule (#67).
        assert_eq!(jump_forward("abca", 0, 'a'), Some(3));
        // Standing on that occurrence, the next press finds none.
        assert_eq!(jump_forward("abca", 3, 'a'), None);
        // A miss in the middle steps to the next one.
        assert_eq!(jump_forward("abca", 1, 'a'), Some(3));
        // At the end there is nothing after the cursor.
        assert_eq!(jump_forward("abc", 3, 'a'), None);
        // Multi-byte targets land on boundaries: "éxé" is
        // é(0..2) x(2..3) é(3..5).
        assert_eq!(jump_forward("éxé", 0, 'é'), Some(3));
        assert_eq!(jump_forward("éxé", 3, 'é'), None);
        // The search crosses newlines — the composer is one flat
        // string and the jump follows it, pi crossing lines the
        // same way.
        assert_eq!(jump_forward("one\ntwo", 0, 't'), Some(4));
        assert_eq!(jump_forward("one\ntwo", 0, '\n'), Some(3));
    }

    #[test]
    fn jump_backward_counts_the_character_behind_the_cursor_and_dies_at_none() {
        // From the end, the nearest `a` behind is the last one.
        assert_eq!(jump_backward("abca", 4, 'a'), Some(3));
        // Standing at an occurrence's start, the next press finds
        // the one before it.
        assert_eq!(jump_backward("abca", 3, 'a'), Some(0));
        // At the very start there is nothing behind.
        assert_eq!(jump_backward("abca", 0, 'a'), None);
        // Multi-byte: "éxé" is é(0..2) x(2..3) é(3..5).
        assert_eq!(jump_backward("éxé", 5, 'é'), Some(3));
        assert_eq!(jump_backward("éxé", 3, 'é'), Some(0));
        // Newlines cross backward the same as forward.
        assert_eq!(jump_backward("one\ntwo", 7, 'n'), Some(1));
    }
}
