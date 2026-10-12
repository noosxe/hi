//! The system clipboard, read off-thread (AD-0011). Paste stages a
//! *reference*, never a send: an image becomes a temp file whose path
//! lands in the input as inert text; the image itself reaches the
//! model only when `read` runs on it — the same gated, budgeted,
//! honestly-noted path as any attached file. pi works exactly this way
//! (temp file + path insert; no submit-time parsing).
//!
//! The read runs on a one-shot thread and reports through a channel
//! the frame loop drains: arboard's X11 backend can stall on a broken
//! session, and the UI must never block on the clipboard.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, channel};

/// What one clipboard read produced.
#[derive(Debug, Clone, PartialEq)]
pub enum ClipboardOutcome {
    /// An image, staged at a temp file; carries the path for the input.
    Image { path: String },
    /// Plain text, ready to insert into the input verbatim.
    Text(String),
    /// The clipboard is reachable but holds nothing usable.
    Empty,
    /// The clipboard is unreachable, or the image could not be staged.
    Unavailable(String),
}

/// Read the clipboard on a fresh thread and report once. Arboard's
/// connection is per-use (cheap; no daemon state to hold across the
/// session), so every Ctrl-V is self-contained.
pub fn spawn_clipboard_read() -> Receiver<ClipboardOutcome> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let _ = tx.send(read_clipboard());
    });
    rx
}

fn read_clipboard() -> ClipboardOutcome {
    let mut clipboard = match arboard::Clipboard::new() {
        Ok(clipboard) => clipboard,
        Err(e) => return ClipboardOutcome::Unavailable(e.to_string()),
    };
    // Image first, then text — pi's order: a screenshot in the
    // clipboard must not be shadowed by stale X11 text selections.
    match clipboard.get_image() {
        Ok(image) => {
            let Some(png) = hi_core::image_util::from_rgba(
                image.width as u32,
                image.height as u32,
                &image.bytes,
            ) else {
                return ClipboardOutcome::Unavailable(
                    "clipboard image could not be encoded".into(),
                );
            };
            let path = temp_png_path();
            if let Err(e) = std::fs::write(&path, &png) {
                return ClipboardOutcome::Unavailable(format!(
                    "could not stage clipboard image: {e}"
                ));
            }
            return ClipboardOutcome::Image {
                path: path.display().to_string(),
            };
        }
        // Nothing image-shaped — fall through to text. Any other
        // error is the platform saying "no clipboard" (headless CI,
        // a lost X11 session), which is Unavailable, not Empty.
        Err(arboard::Error::ContentNotAvailable) => {}
        Err(e) => return ClipboardOutcome::Unavailable(e.to_string()),
    }
    match clipboard.get_text() {
        Ok(text) if !text.trim().is_empty() => ClipboardOutcome::Text(text),
        _ => ClipboardOutcome::Empty,
    }
}

/// A unique staging path for one pasted image: `hi-clipboard-*` in the
/// OS temp dir. Never auto-deleted — pi leaves its files too; OS temp
/// hygiene owns cleanup, and the model may legitimately re-read the
/// path later in the session.
pub fn temp_png_path() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("hi-clipboard-{nanos:x}-{n:x}.png"))
}

/// Append pasted text to the input the way pi pads an insert: one
/// leading space when it would glue onto a word, no trailing space
/// (the insertion point is the end of the input). Line endings
/// normalize to `\n` — terminals send `\r\n`, and a raw `\r` would
/// mangle the composer's wrapping.
/// Paste `text` at the cursor: `before` is everything left of it,
/// `after` everything from it onward. Newlines normalize to `\n`;
/// pad with a space against a non-whitespace neighbor on either side
/// (pi's cursor-neighborhood rule — with a real cursor the "after"
/// half of it finally applies too).
pub fn insert_pasted(before: &str, text: &str, after: &str) -> String {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::with_capacity(before.len() + after.len() + text.len() + 2);
    out.push_str(before);
    if !text.is_empty() {
        if before
            .chars()
            .next_back()
            .is_some_and(|c| !c.is_whitespace())
        {
            out.push(' ');
        }
        out.push_str(&text);
        if after.chars().next().is_some_and(|c| !c.is_whitespace()) {
            out.push(' ');
        }
    }
    out.push_str(after);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_pasted_pads_against_words_on_both_sides() {
        assert_eq!(insert_pasted("", "/tmp/x.png", ""), "/tmp/x.png");
        assert_eq!(insert_pasted("look", "/tmp/x.png", ""), "look /tmp/x.png");
        // Whitespace neighbors mean no pad, either side.
        assert_eq!(insert_pasted("look ", "/tmp/x.png", ""), "look /tmp/x.png");
        // Mid-word: pad both sides so the paste stands alone.
        assert_eq!(
            insert_pasted("see", "/tmp/x.png", "now"),
            "see /tmp/x.png now"
        );
        // Empty text is a pure no-op — no pad invented.
        assert_eq!(insert_pasted("see", "", "now"), "seenow");
    }

    #[test]
    fn insert_pasted_normalizes_line_endings() {
        assert_eq!(insert_pasted("a", "x\r\ny\rz", ""), "a x\ny\nz");
        assert_eq!(insert_pasted("a", "\r\n", "b"), "a \n b");
    }

    #[test]
    fn temp_png_paths_are_unique_and_shaped() {
        let a = temp_png_path();
        let b = temp_png_path();
        assert_ne!(a, b);
        let name = a.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("hi-clipboard-"), "{name}");
        assert!(name.ends_with(".png"), "{name}");
        assert_eq!(a.parent(), Some(std::env::temp_dir().as_path()));
    }
}
