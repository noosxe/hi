//! The session picker: row composition, filtering, and the standalone
//! pre-TUI chooser (pi's startup-TUI pattern). `/resume` reuses the
//! same helpers inside the running TUI (see `app.rs`).

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyModifiers};
use ratatui::text::{Line, Span, Text};
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

/// One picker row's words: `age · lead` plus the id to demote right.
/// The lead is the words that recognize the session — its name when
/// it has one, else its first prompt, else the id prefix (pi's row
/// shape, #89). `None` id means the lead IS the id — nothing to
/// demote, no duplicate.
pub fn row_text(
    summary: &SessionSummary,
    preview: Option<&str>,
    now: SystemTime,
) -> (String, Option<String>) {
    let lead = summary.header.name.clone().or_else(|| {
        preview
            .unwrap_or("")
            .lines()
            .next()
            .filter(|line| !line.is_empty())
            .map(str::to_string)
    });
    let id = summary.header.id.chars().take(8).collect::<String>();
    match lead {
        Some(lead) => (
            format!("{} · {}", age(summary.modified, now), lead),
            Some(id),
        ),
        None => (format!("{} · {id}", age(summary.modified, now)), None),
    }
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
#[allow(clippy::too_many_arguments)] // the params ARE the composition
pub fn picker_lines(
    summaries: &[SessionSummary],
    previews: &[Option<String>],
    filter: &str,
    selected: usize,
    height: usize,
    now: SystemTime,
    width: usize,
    rename: Option<&str>,
) -> Vec<Line<'static>> {
    let visible: Vec<usize> = (0..summaries.len())
        .filter(|index| filter_matches(filter, &summaries[*index], previews[*index].as_deref()))
        .collect();
    let selected = selected.min(visible.len().saturating_sub(1));
    let slots = height.saturating_sub(3); // title + filter + hint
    // The same SelectList as every hi menu: when the matches overflow
    // the window, one row is spent on the muted `(n/m)` count.
    let overflowing = visible.len() > slots;
    let take = slots.saturating_sub(usize::from(overflowing));
    let start = selected.saturating_sub(take.saturating_sub(1));

    let mut lines = vec![Line::styled("resume a session", THEME.fg(Fg::Dim))];
    for (offset, index) in visible.iter().skip(start).take(take).enumerate() {
        let (main, id) = row_text(&summaries[*index], previews[*index].as_deref(), now);
        let chosen = start + offset == selected;
        // The selected row colors whole — accent, pi's selectedText;
        // unselected rows ride muted.
        let style = if chosen {
            THEME.fg(Fg::Accent)
        } else {
            THEME.fg(Fg::Muted)
        };
        let marker = if chosen { "→ " } else { "  " };
        // The words lead; the id — the `--session` key — demotes to
        // the dim right and survives any width (the lead truncates
        // first, at ten columns minimum, pi's floor).
        let mut spans = Vec::new();
        if let Some(id) = id {
            let marker_cols = marker.chars().count();
            let budget = width.saturating_sub(marker_cols + id.len() + 1).max(10);
            let main: String = if main.chars().count() > budget {
                let cut: String = main.chars().take(budget.saturating_sub(1)).collect();
                format!("{cut}…")
            } else {
                main
            };
            let pad = width.saturating_sub(marker_cols + main.chars().count() + id.len());
            spans.push(Span::styled(format!("{marker}{main}"), style));
            spans.push(Span::styled(
                format!("{}{id}", " ".repeat(pad)),
                THEME.fg(Fg::Dim),
            ));
        } else {
            spans.push(Span::styled(format!("{marker}{main}"), style));
        }
        lines.push(Line::from(spans));
    }
    if overflowing {
        lines.push(Line::styled(
            format!("  ({}/{})", selected + 1, visible.len()),
            THEME.fg(Fg::Muted),
        ));
    }
    if visible.is_empty() {
        lines.push(Line::styled("no sessions match", THEME.fg(Fg::Dim)));
    }
    match rename {
        Some(buffer) => {
            lines.push(Line::styled(
                format!("name: {buffer}"),
                THEME.fg(Fg::UserText),
            ));
            lines.push(Line::styled("enter save · esc cancel", THEME.fg(Fg::Dim)));
        }
        None => {
            lines.push(Line::styled(format!("› {filter}"), THEME.fg(Fg::UserText)));
            lines.push(Line::styled(
                "type to filter · ^r rename · enter switch · esc cancel",
                THEME.fg(Fg::Dim),
            ));
        }
    }
    lines
}

/// Show the picker and return the chosen session file, if any. Owns
/// its terminal lifetime — a short inline viewport restored before
/// returning — so the main TUI boots fresh after selection (pi's
/// startup-TUI pattern). Esc or Ctrl-C cancels.
pub fn pick_session(summaries: &[SessionSummary]) -> Option<PathBuf> {
    let previews: Vec<Option<String>> = summaries.iter().map(|s| preview_of(&s.path)).collect();
    let (columns, rows) = ratatui::crossterm::terminal::size().unwrap_or((80, 24));
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
            columns as usize,
            None,
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
                kind: None,
                parent: None,
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
    fn the_words_lead_the_row() {
        // pi's row shape (#89): name ?? first prompt ?? id, and the id
        // demoted to the dim right — never the identity slot.
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(120);
        let named = summary("aaaaaaaa-bbbb", Some("auth refactor"), 0);
        let (main, id) = row_text(&named, None, now);
        assert_eq!(main, "2m · auth refactor");
        assert_eq!(id.as_deref(), Some("aaaaaaaa"));
        let unnamed = summary("12345678-abcd", None, 0);
        let (main, id) = row_text(&unnamed, Some("fix the bug\nsecond line"), now);
        assert_eq!(main, "2m · fix the bug");
        assert_eq!(id.as_deref(), Some("12345678"));
        // Nothing to say yet: the id leads, and there is nothing to
        // demote — no duplicate id on the right.
        let (main, id) = row_text(&unnamed, None, now);
        assert_eq!(main, "2m · 12345678");
        assert_eq!(id, None);
    }

    #[test]
    fn the_row_leads_with_words_and_demotes_the_id_right() {
        // The composed line: words left, id right-aligned, one span
        // dimmer than the row.
        let summaries = vec![summary("abcdef12-3456", None, 0)];
        let previews = vec![Some("fix the flaky test".into())];
        let lines = picker_lines(
            &summaries,
            &previews,
            "",
            0,
            6,
            SystemTime::UNIX_EPOCH,
            40,
            None,
        );
        let row = &lines[1];
        assert_eq!(row.spans[0].content, "→ 0s · fix the flaky test");
        assert_eq!(row.spans[1].content, "       abcdef12");
        assert_eq!(row.spans[1].style, THEME.fg(Fg::Dim));
    }

    #[test]
    fn a_long_lead_truncates_before_the_id() {
        // The words bow to the width budget before the id does — the
        // id is the `--session` key and must survive any width.
        let summaries = vec![summary("abcdef12-3456", None, 0)];
        let previews = vec![Some(String::from("w").repeat(80))];
        let lines = picker_lines(
            &summaries,
            &previews,
            "",
            0,
            6,
            SystemTime::UNIX_EPOCH,
            40,
            None,
        );
        let row = &lines[1];
        let main = row.spans[0].content.clone();
        assert!(main.ends_with('…'), "{main}");
        assert!(main.chars().count() < 40 - "abcdef12".len(), "{main}");
        assert!(row.spans[1].content.ends_with("abcdef12"));
    }

    #[test]
    fn rename_takes_over_the_prompt_line() {
        // While a rename is in flight the picker says so: the buffer
        // rides the prompt line, the hint narrows to the two keys.
        let summaries = vec![summary("abcdef12-3456", None, 0)];
        let previews = vec![Some("hello".into())];
        let lines = picker_lines(
            &summaries,
            &previews,
            "",
            0,
            6,
            SystemTime::UNIX_EPOCH,
            40,
            Some("par"),
        );
        assert_eq!(lines[lines.len() - 2].spans[0].content, "name: par");
        assert_eq!(
            lines[lines.len() - 1].spans[0].content,
            "enter save · esc cancel"
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
        let lines = picker_lines(
            &summaries,
            &previews,
            "",
            0,
            8,
            SystemTime::UNIX_EPOCH,
            60,
            None,
        );
        assert_eq!(lines.len(), 5); // title + 2 rows + filter + hint
        assert_eq!(lines[0].spans[0].content, "resume a session");
        // The name replaces the preview — pi's `??`, one lead or the
        // other, never both.
        assert_eq!(lines[1].spans[0].content, "→ 0s · keeper");
        assert_eq!(
            lines[1].spans[1].content,
            format!("{}11111111", " ".repeat(39))
        );
        assert_eq!(lines[2].spans[0].content, "  0s · 22222222");
        assert_eq!(lines[1].spans[0].style, THEME.fg(Fg::Accent));
        assert_eq!(lines[2].spans[0].style, THEME.fg(Fg::Muted));
        assert_eq!(lines[3].spans[0].content, "› ");
        // Selected position beyond the list clamps; the marker follows.
        let lines = picker_lines(
            &summaries,
            &previews,
            "",
            9,
            8,
            SystemTime::UNIX_EPOCH,
            60,
            None,
        );
        assert_eq!(lines[2].spans[0].content, "→ 0s · 22222222");
        // Overflowing matches spend a row on the muted count.
        let many: Vec<SessionSummary> = (0..9)
            .map(|i| summary(&format!("0000000{i}-aaaa"), Some(&format!("s{i}")), 0))
            .collect();
        let previews: Vec<Option<String>> = (0..9).map(|_| None).collect();
        let lines = picker_lines(&many, &previews, "", 0, 8, SystemTime::UNIX_EPOCH, 60, None);
        assert!(
            lines.iter().any(|l| l.spans[0].content == "  (1/9)"),
            "count row on overflow"
        );
        // A filter matching nothing says so instead of hiding rows silently.
        let lines = picker_lines(
            &summaries,
            &previews,
            "zzz",
            0,
            8,
            SystemTime::UNIX_EPOCH,
            60,
            None,
        );
        assert_eq!(lines[1].spans[0].content, "no sessions match");
    }
}
