//! The session picker: row composition, filtering, and the standalone
//! pre-TUI chooser (pi's startup-TUI pattern). `/resume` reuses the
//! same helpers inside the running TUI (see `app.rs`).

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyModifiers};
use ratatui::text::{Line, Text};
use ratatui::widgets::Paragraph;
use ratatui::{TerminalOptions, Viewport};

use hi_core::session::SessionSummary;

use crate::theme::{Fg, THEME};

/// `3m`-style relative age for a picker row — seconds, minutes, hours,
/// then days. A future mtime (clock skew) reads as `0s`.
pub fn age(modified: SystemTime, now: SystemTime) -> String {
    let secs = now
        .duration_since(modified)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

/// Case-insensitive substring match over the fields a row shows: the
/// name, the id, and the first-message preview.
pub fn filter_matches(filter: &str, summary: &SessionSummary, preview: Option<&str>) -> bool {
    let needle = filter.to_lowercase();
    if needle.is_empty() {
        return true;
    }
    let haystack = format!(
        "{} {} {}",
        summary.header.id,
        summary.header.name.as_deref().unwrap_or(""),
        preview.unwrap_or("")
    );
    haystack.to_lowercase().contains(&needle)
}

/// One picker row: `age · name-or-id-prefix · first user message`.
/// The name wins when set; otherwise the id's first eight characters
/// identify the session. Rendering crops overflowing rows.
pub fn row_text(summary: &SessionSummary, preview: Option<&str>, now: SystemTime) -> String {
    let label = summary
        .header
        .name
        .clone()
        .unwrap_or_else(|| summary.header.id.chars().take(8).collect());
    let mut text = format!("{} · {}", age(summary.modified, now), label);
    if let Some(first) = preview
        .unwrap_or("")
        .lines()
        .next()
        .filter(|line| !line.is_empty())
    {
        text.push_str(" · ");
        text.push_str(first);
    }
    text
}

/// First user message text — the preview column. `None` when the file
/// won't load or carries no user message yet.
pub fn preview_of(path: &Path) -> Option<String> {
    hi_core::session::load(path)
        .ok()?
        .messages
        .into_iter()
        .find_map(|message| match message {
            hi_core::Message::User(text) => Some(text),
            _ => None,
        })
}

/// The picker's lines: title, the visible rows around `selected`,
/// filter, and hint. Shared by the standalone chooser and `/resume` —
/// one composition, so both look and test the same.
pub fn picker_lines(
    summaries: &[SessionSummary],
    previews: &[Option<String>],
    filter: &str,
    selected: usize,
    height: usize,
    now: SystemTime,
) -> Vec<Line<'static>> {
    let visible: Vec<usize> = (0..summaries.len())
        .filter(|index| filter_matches(filter, &summaries[*index], previews[*index].as_deref()))
        .collect();
    let selected = selected.min(visible.len().saturating_sub(1));
    let slots = height.saturating_sub(3); // title + filter + hint
    let start = selected.saturating_sub(slots.saturating_sub(1));

    let mut lines = vec![Line::styled("resume a session", THEME.fg(Fg::Dim))];
    for (offset, index) in visible.iter().skip(start).take(slots).enumerate() {
        let row = row_text(&summaries[*index], previews[*index].as_deref(), now);
        let chosen = start + offset == selected;
        let style = if chosen {
            THEME.fg(Fg::ToolTitle)
        } else {
            THEME.fg(Fg::Muted)
        };
        let marker = if chosen { "❯ " } else { "  " };
        lines.push(Line::styled(format!("{marker}{row}"), style));
    }
    if visible.is_empty() {
        lines.push(Line::styled("no sessions match", THEME.fg(Fg::Dim)));
    }
    lines.push(Line::styled(format!("› {filter}"), THEME.fg(Fg::UserText)));
    lines.push(Line::styled(
        "type to filter · enter switch · esc cancel",
        THEME.fg(Fg::Dim),
    ));
    lines
}

/// Show the picker and return the chosen session file, if any. Owns
/// its terminal lifetime — a short inline viewport restored before
/// returning — so the main TUI boots fresh after selection (pi's
/// startup-TUI pattern). Esc or Ctrl-C cancels.
pub fn pick_session(summaries: &[SessionSummary]) -> Option<PathBuf> {
    let previews: Vec<Option<String>> = summaries.iter().map(|s| preview_of(&s.path)).collect();
    let (_, rows) = ratatui::crossterm::terminal::size().unwrap_or((80, 24));
    // Title + list + filter + hint rows, bounded by the real terminal.
    let height = (summaries.len() as u16 + 3)
        .clamp(6, rows.saturating_sub(2))
        .min(20);
    let mut filter = String::new();
    let mut selected: usize = 0;

    let terminal = ratatui::try_init_with_options(TerminalOptions {
        viewport: Viewport::Inline(height),
    });
    let mut terminal = match terminal {
        Ok(terminal) => terminal,
        Err(err) => {
            eprintln!("hi: {err}");
            return None;
        }
    };
    let choice = loop {
        let visible: Vec<usize> = (0..summaries.len())
            .filter(|index| {
                filter_matches(&filter, &summaries[*index], previews[*index].as_deref())
            })
            .collect();
        selected = selected.min(visible.len().saturating_sub(1));
        let lines = picker_lines(
            summaries,
            &previews,
            &filter,
            selected,
            height as usize,
            SystemTime::now(),
        );
        terminal
            .draw(|frame| {
                frame.render_widget(Paragraph::new(Text::from(lines.clone())), frame.area());
            })
            .ok();

        let Ok(true) = event::poll(std::time::Duration::from_millis(50)) else {
            continue;
        };
        let Ok(Event::Key(key)) = event::read() else {
            continue;
        };
        match key.code {
            KeyCode::Esc => break None,
            KeyCode::Enter => {
                break visible
                    .get(selected)
                    .map(|index| summaries[*index].path.clone());
            }
            KeyCode::Up => selected = selected.saturating_sub(1),
            KeyCode::Down => {
                if selected + 1 < visible.len() {
                    selected += 1;
                }
            }
            KeyCode::Backspace => {
                let _ = filter.pop();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                filter.push(c);
            }
            _ => {}
        }
    };
    ratatui::restore();
    choice
}

#[cfg(test)]
mod tests {
    use super::*;
    use hi_core::session::SessionHeader;
    use std::time::Duration;

    fn summary(id: &str, name: Option<&str>, modified_secs: u64) -> SessionSummary {
        SessionSummary {
            path: PathBuf::from(format!("/tmp/{id}.jsonl")),
            header: SessionHeader {
                version: 1,
                id: id.into(),
                cwd: "/tmp".into(),
                created_at: 0,
                provider: "p".into(),
                model: "m".into(),
                thinking: None,
                name: name.map(str::to_string),
                forked_from: None,
            },
            modified: SystemTime::UNIX_EPOCH + Duration::from_secs(modified_secs),
        }
    }

    #[test]
    fn age_buckets_by_magnitude() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000_000);
        assert_eq!(age(now - Duration::from_secs(5), now), "5s");
        assert_eq!(age(now - Duration::from_secs(300), now), "5m");
        assert_eq!(age(now - Duration::from_secs(7200), now), "2h");
        assert_eq!(age(now - Duration::from_secs(3 * 86400), now), "3d");
        // Clock skew reads as 0s, not a panic.
        assert_eq!(age(now + Duration::from_secs(60), now), "0s");
    }

    #[test]
    fn rows_prefer_the_name_and_append_the_preview() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(120);
        let named = summary("aaaaaaaa-bbbb", Some("auth refactor"), 0);
        assert_eq!(row_text(&named, None, now), "2m · auth refactor");
        let unnamed = summary("12345678-abcd", None, 0);
        assert_eq!(
            row_text(&unnamed, Some("fix the bug\nsecond line"), now),
            "2m · 12345678 · fix the bug"
        );
    }

    #[test]
    fn filters_match_id_name_and_preview_case_insensitively() {
        let s = summary("abcdef12-3456", Some("Auth Work"), 0);
        assert!(filter_matches("", &s, None));
        assert!(filter_matches("AUTH", &s, None));
        assert!(filter_matches("cdef", &s, None));
        assert!(filter_matches("needle", &s, Some("hayneedle stack")));
        assert!(!filter_matches("zzz", &s, Some("nothing here")));
    }

    #[test]
    fn picker_lines_mark_the_selection_and_show_the_filter() {
        let summaries = vec![
            summary("11111111-aaaa", Some("keeper"), 0),
            summary("22222222-bbbb", None, 0),
        ];
        let previews = vec![Some("keep me".into()), None];
        let lines = picker_lines(&summaries, &previews, "", 0, 8, SystemTime::UNIX_EPOCH);
        assert_eq!(lines.len(), 5); // title + 2 rows + filter + hint
        assert_eq!(lines[0].spans[0].content, "resume a session");
        assert_eq!(lines[1].spans[0].content, "❯ 0s · keeper · keep me");
        assert_eq!(lines[2].spans[0].content, "  0s · 22222222");
        assert_eq!(lines[1].style, THEME.fg(Fg::ToolTitle));
        assert_eq!(lines[2].style, THEME.fg(Fg::Muted));
        assert_eq!(lines[3].spans[0].content, "› ");
        // Selected position beyond the list clamps; the marker follows.
        let lines = picker_lines(&summaries, &previews, "", 9, 8, SystemTime::UNIX_EPOCH);
        assert_eq!(lines[2].spans[0].content, "❯ 0s · 22222222");
        // A filter matching nothing says so instead of hiding rows silently.
        let lines = picker_lines(&summaries, &previews, "zzz", 0, 8, SystemTime::UNIX_EPOCH);
        assert_eq!(lines[1].spans[0].content, "no sessions match");
    }
}
