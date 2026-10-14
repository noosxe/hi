//! The keybind registry (#95): one table answering "what does Alt+D
//! do again?" — `/keybinds` prints it, and a test holds it against
//! the chords actually bound in `app.rs`, so a new binding without a
//! row is a failing test, not a doc drift.

/// Every modified key the TUI binds: the chord as printed and a
/// one-line description of what it does. Display order — movement,
/// kills, yank, undo/redo, then session-level — is the doc order.
pub const KEYBINDS: &[(&str, &str)] = &[
    // movement
    ("Ctrl+A", "cursor to line start"),
    ("Ctrl+E", "cursor to line end"),
    ("Ctrl+B", "cursor back one character"),
    ("Ctrl+F", "cursor forward one character"),
    ("Alt+B", "cursor back one word"),
    ("Alt+F", "cursor forward one word"),
    // kills
    ("Ctrl+D", "delete the character ahead"),
    ("Ctrl+K", "kill to end of line"),
    ("Ctrl+U", "kill to start of line"),
    ("Ctrl+W", "kill the word behind"),
    ("Alt+Backspace", "kill the word behind (same as ctrl+w)"),
    ("Alt+D", "kill the word ahead"),
    ("Alt+Delete", "kill the word ahead (same as alt+d)"),
    // yank
    ("Ctrl+Y", "yank the newest kill"),
    ("Alt+Y", "cycle the yank to older kills"),
    // undo / redo
    ("Ctrl+Z", "undo the last composer edit"),
    ("Alt+Z", "redo"),
    // session-level
    ("Ctrl+J", "insert a newline (legacy shift+enter)"),
    ("Ctrl+C", "clear the draft; quit when it is already empty"),
    ("Ctrl+V", "paste from the clipboard (cmd+v works too)"),
    ("Ctrl+S", "save in the model picker and the settings board"),
    (
        "Ctrl+R",
        "rename the selected session (in the resume picker)",
    ),
];

/// The `/keybinds` listing: a header with the count, then the table.
pub fn listing() -> String {
    if KEYBINDS.is_empty() {
        return "no keybinds registered".into();
    }
    let mut out = format!("keybinds ({})", KEYBINDS.len());
    for (chord, description) in KEYBINDS {
        out.push_str(&format!("\n  {chord:<15} {description}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The source of `app.rs`, scanned so the table cannot drift
    /// from the bindings: every modified chord bound there must have
    /// a row, and every row must be a chord bound there (#95).
    #[test]
    fn every_bound_chord_has_a_row_and_every_row_is_bound() {
        let src = include_str!("app.rs");
        let mut bound: Vec<String> = Vec::new();
        let lines: Vec<&str> = src.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            // Arms of the ctrl chord match are bare `'x' =>`.
            if let Some(rest) = line.trim_start().strip_prefix('\'')
                && let Some(letter) = rest.chars().next()
                && letter.is_ascii_lowercase()
                && rest[1..].starts_with("' =>")
            {
                bound.push(format!("Ctrl+{}", letter.to_ascii_uppercase()));
                continue;
            }
            let lookahead = lines.get(i + 1..i + 3).unwrap_or(&[]);
            // Direct gated arms carry their modifiers on the arm line
            // or within the next two lines (paste_key, alt+backspace).
            let context = {
                let mut c = line.to_string();
                for l in lookahead {
                    c.push(' ');
                    c.push_str(l);
                }
                c
            };
            for (token, name) in [
                ("KeyModifiers::CONTROL", "Ctrl"),
                ("KeyModifiers::ALT", "Alt"),
            ] {
                if !context.contains(token) {
                    continue;
                }
                if let Some(pos) = context.find("KeyCode::Char('")
                    && let Some(letter) = context[pos + 15..].chars().next()
                    && letter.is_ascii_lowercase()
                {
                    bound.push(format!("{name}+{}", letter.to_ascii_uppercase()));
                }
                for key in ["KeyCode::Backspace", "KeyCode::Delete"] {
                    if line.contains(key) {
                        bound.push(format!("{name}+{}", key.trim_start_matches("KeyCode::")));
                    }
                }
            }
        }
        bound.sort();
        bound.dedup();
        let rows: std::collections::BTreeSet<&str> =
            KEYBINDS.iter().map(|(chord, _)| *chord).collect();
        let scanned: std::collections::BTreeSet<&str> = bound.iter().map(String::as_str).collect();
        let unbound: Vec<_> = rows.difference(&scanned).collect();
        let undescribed: Vec<_> = scanned.difference(&rows).collect();
        assert!(
            unbound.is_empty() && undescribed.is_empty(),
            "table and bindings drifted — unbound rows: {unbound:?}, undescribed chords: {undescribed:?}"
        );
    }

    #[test]
    fn the_listing_carries_every_row() {
        let out = listing();
        assert!(out.starts_with(&format!("keybinds ({})", KEYBINDS.len())));
        for (chord, description) in KEYBINDS {
            assert!(out.contains(chord), "listing missing {chord}");
            assert!(out.contains(description), "listing missing {description:?}");
        }
    }
}
