//! The shared output clamp: 2000 lines or 50 KiB, whichever hits first.
//!
//! Every built-in tool pushes its result through this contract before it
//! becomes a [`ToolResult`](crate::tool::ToolResult), so no tool can flood
//! the context window. Head-keeping is the default (read/grep/find/ls — you
//! want the top of a file or the first matches); bash tail-keeps instead,
//! because a failed command's diagnosis lives at the bottom of its output.
//!
//! The clipper never returns partial lines, with one documented exception:
//! [`tail_with`] falls back to a byte-suffix of the final line when no whole
//! line fits — the model still sees the tail of a pathological one-line dump.

/// Maximum lines any tool result may carry.
pub const MAX_LINES: usize = 2000;
/// Maximum bytes any tool result may carry (50 KiB).
pub const MAX_BYTES: usize = 50 * 1024;

/// The outcome of clamping one tool result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clip {
    /// The clamped text (empty when nothing fit).
    pub text: String,
    /// Lines kept after clamping.
    pub kept_lines: usize,
    /// Lines the input had before clamping.
    pub total_lines: usize,
    /// True when content was dropped.
    pub truncated: bool,
}

impl Clip {
    /// The model-facing note appended after clamped output, or `None` when
    /// nothing was dropped.
    pub fn note(&self, tail_kept: bool) -> Option<String> {
        if !self.truncated {
            return None;
        }
        let side = if tail_kept { "last" } else { "first" };
        Some(format!(
            "[truncated: showing {side} {} of {} lines]",
            self.kept_lines, self.total_lines
        ))
    }
}

/// Clamp to the built-in contract, keeping the head.
pub fn head(content: &str) -> Clip {
    head_with(content, MAX_LINES, MAX_BYTES)
}

/// Clamp to the built-in contract, keeping the tail.
pub fn tail(content: &str) -> Clip {
    tail_with(content, MAX_LINES, MAX_BYTES)
}

/// Head-clamp with explicit limits (tests use smaller ones).
pub fn head_with(content: &str, max_lines: usize, max_bytes: usize) -> Clip {
    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();
    let mut kept: Vec<&str> = Vec::new();
    let mut bytes = 0usize;
    for line in &lines {
        if kept.len() >= max_lines || bytes + line.len() + 1 > max_bytes {
            break;
        }
        bytes += line.len() + 1;
        kept.push(line);
    }
    Clip {
        text: kept.join("\n"),
        kept_lines: kept.len(),
        total_lines,
        truncated: kept.len() < total_lines,
    }
}

/// Tail-keep with explicit limits. Falls back to a partial final line —
/// cut on a char boundary — when even one line exceeds `max_bytes`.
pub fn tail_with(content: &str, max_lines: usize, max_bytes: usize) -> Clip {
    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();
    let mut kept: Vec<&str> = Vec::new();
    let mut bytes = 0usize;
    for line in lines.iter().rev() {
        if kept.len() >= max_lines || bytes + line.len() + 1 > max_bytes {
            break;
        }
        bytes += line.len() + 1;
        kept.push(line);
    }
    kept.reverse();
    if !kept.is_empty() {
        return Clip {
            text: kept.join("\n"),
            kept_lines: kept.len(),
            total_lines,
            truncated: kept.len() < total_lines,
        };
    }
    // No whole line fits: keep the tail bytes of the final line.
    if total_lines == 0 || max_bytes == 0 {
        return Clip {
            text: String::new(),
            kept_lines: 0,
            total_lines,
            truncated: total_lines > 0,
        };
    }
    let last = lines[total_lines - 1];
    let start = char_floor(last, last.len().saturating_sub(max_bytes));
    Clip {
        text: last[start..].to_string(),
        kept_lines: 1,
        total_lines,
        truncated: true,
    }
}

/// Largest index `<= i` that is a char boundary of `s`.
fn char_floor(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_keeps_everything_under_the_contract() {
        let clip = head("one\ntwo\nthree");
        assert_eq!(clip.text, "one\ntwo\nthree");
        assert!(!clip.truncated);
        assert_eq!(clip.total_lines, 3);
        assert_eq!(clip.note(false), None);
    }

    #[test]
    fn head_clamps_by_lines() {
        let content = (0..10)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let clip = head_with(&content, 4, MAX_BYTES);
        assert_eq!(clip.text, "0\n1\n2\n3");
        assert!(clip.truncated);
        assert_eq!(clip.kept_lines, 4);
        assert_eq!(
            clip.note(false).unwrap(),
            "[truncated: showing first 4 of 10 lines]"
        );
    }

    #[test]
    fn head_clamps_by_bytes_and_never_splits_a_line() {
        let content = "short\n".repeat(3) + &"x".repeat(100);
        let clip = head_with(&content, MAX_LINES, 20);
        // "short\n" x3 = 18 bytes; the long line would overshoot.
        assert_eq!(clip.text, "short\nshort\nshort");
        assert!(clip.truncated);
    }

    #[test]
    fn head_drops_everything_when_line_one_alone_overshoots() {
        let clip = head_with(&"y".repeat(40), MAX_LINES, 10);
        assert_eq!(clip.text, "");
        assert_eq!(clip.kept_lines, 0);
        assert!(clip.truncated);
    }

    #[test]
    fn tail_keeps_the_end() {
        let content = (0..10)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let clip = tail_with(&content, 3, MAX_BYTES);
        assert_eq!(clip.text, "7\n8\n9");
        assert_eq!(
            clip.note(true).unwrap(),
            "[truncated: showing last 3 of 10 lines]"
        );
    }

    #[test]
    fn tail_falls_back_to_a_byte_suffix_on_a_char_boundary() {
        let content = "é".repeat(30); // 2 bytes each, one line
        let clip = tail_with(&content, MAX_LINES, 10);
        assert!(clip.truncated);
        // 10 bytes of suffix cut on a char boundary = 5 'é's.
        assert_eq!(clip.text, "é".repeat(5));
        assert_eq!(clip.kept_lines, 1);
    }

    #[test]
    fn empty_input_stays_empty_and_untruncated() {
        let clip = head("");
        assert_eq!(clip.text, "");
        assert!(!clip.truncated);
        assert_eq!(clip.total_lines, 0);
    }
}
