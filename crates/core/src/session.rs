//! Session persistence: JSONL transcripts that survive restarts.
//!
//! Layout, pi-style but under hi's own data directory:
//!
//! ```text
//! $XDG_DATA_HOME/hi/sessions/  (else ~/.local/share/hi/sessions/)
//!   └── --Users-mechsoull-Projects-hi--/      ← project key (cwd-encoded)
//!        └── 1760000000000_7c9e…-.jsonl       ← <millis>_<uuid>.jsonl
//! ```
//!
//! One session is one file, written **append-only**: the first line is a
//! session header, every following line is one finalized `Message`. Only
//! turn-end state is persisted — streamed tokens never touch disk — so a
//! crash costs at most the in-flight turn, and a torn final line is
//! tolerated on read. Everything is camelCase with unknown fields
//! ignored, matching the config files' forward-compatibility rule.
//!
//! Files are keyed by project directory because resume is a *project*
//! gesture: "continue what I was doing here". Cross-project resume is a
//! deliberate non-goal for now.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::config::data_dir;
use crate::message::Message;

/// The first line of a session file — flat, self-describing metadata.
///
/// `version` exists so a future format migration can be detected by
/// reading one line instead of parsing a whole transcript. Unknown
/// fields are ignored on purpose, like every other hi file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionHeader {
    /// Format version; `1` until something needs a migration.
    #[serde(default = "default_version")]
    pub version: u32,
    /// UUID identifying the session (the filename carries it too).
    pub id: String,
    /// Where the session happened — the project directory.
    pub cwd: String,
    /// Creation time, milliseconds since the Unix epoch. No date
    /// formatting anywhere in core; rendering is a UI concern.
    pub created_at: u64,
    /// Provider name the session started with (informational — resume
    /// re-resolves from today's flags/config).
    pub provider: String,
    /// Model id the session started with (informational, same rule).
    pub model: String,
    /// Thinking level, if one was set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
}

fn default_version() -> u32 {
    1
}

/// A session file reopened for reading: header plus the transcript.
#[derive(Debug, Clone)]
pub struct LoadedSession {
    pub header: SessionHeader,
    pub messages: Vec<Message>,
    pub path: PathBuf,
}

/// An open session file, ready to append finalized messages.
///
/// The `File` handle is held in append mode for the lifetime of the
/// session; `append` writes one JSONL line per message and flushes, so
/// the on-disk transcript is never behind by more than one turn.
pub struct Session {
    file: std::fs::File,
    path: PathBuf,
    header: SessionHeader,
}

impl Session {
    /// Start a new session for the current project (cwd) and the given
    /// provider/model/thinking snapshot.
    pub fn start(provider: &str, model: &str, thinking: Option<&str>) -> Result<Session, String> {
        let cwd = std::env::current_dir()
            .map_err(|err| format!("cannot determine current directory: {err}"))?;
        let root = sessions_dir().ok_or("could not determine data directory")?;
        Self::start_in(&root, &cwd, provider, model, thinking)
    }

    /// The path-independent half of [`start`], so tests can use temp
    /// directories (the `settings.rs` pattern).
    pub fn start_in(
        root: &Path,
        cwd: &Path,
        provider: &str,
        model: &str,
        thinking: Option<&str>,
    ) -> Result<Session, String> {
        let created_at = epoch_millis();
        let header = SessionHeader {
            version: 1,
            id: uuid::Uuid::new_v4().to_string(),
            cwd: cwd.to_string_lossy().into_owned(),
            created_at,
            provider: provider.to_string(),
            model: model.to_string(),
            thinking: thinking.map(str::to_string),
        };
        let dir = root.join(project_key(cwd));
        std::fs::create_dir_all(&dir)
            .map_err(|err| format!("cannot create {}: {err}", dir.display()))?;
        let path = dir.join(format!("{created_at}_{}.jsonl", header.id));
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)
            .map_err(|err| format!("cannot create {}: {err}", path.display()))?;
        writeln!(file, "{}", header_line(&header)?)
            .map_err(|err| format!("cannot write {}: {err}", path.display()))?;
        file.flush()
            .map_err(|err| format!("cannot write {}: {err}", path.display()))?;
        Ok(Session { file, path, header })
    }

    /// Reopen an existing session file for appending — the `--continue`
    /// path. One session stays one file; the directory listing reflects
    /// continued activity through the file's modification time.
    pub fn resume(path: &Path) -> Result<Session, String> {
        let header = read_header(path)?;
        let file = std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .map_err(|err| format!("cannot open {}: {err}", path.display()))?;
        Ok(Session {
            file,
            path: path.to_path_buf(),
            header,
        })
    }

    /// The file this session writes to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// This session's header — id, creation time, the provider/model
    /// snapshot it started with.
    pub fn header(&self) -> &SessionHeader {
        &self.header
    }

    /// Append finalized messages — called at turn end. One line per
    /// message, flushed immediately: the crash window is one message,
    /// not one session.
    pub fn append(&mut self, messages: &[Message]) -> Result<(), String> {
        for message in messages {
            writeln!(self.file, "{}", message_line(message)?)
                .map_err(|err| format!("cannot write {}: {err}", self.path.display()))?;
        }
        self.file
            .flush()
            .map_err(|err| format!("cannot write {}: {err}", self.path.display()))
    }
}

/// Load the most recent session for `cwd` — the `--continue` half.
///
/// Files that fail to load are skipped rather than fatal: they may be
/// mid-write from another process, and resume should pick the newest
/// session it can actually read. `Ok(None)` means nothing resumable.
pub fn latest(cwd: &Path) -> Result<Option<LoadedSession>, String> {
    let root = sessions_dir().ok_or("could not determine data directory")?;
    let dir = root.join(project_key(cwd));
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(None); // no directory yet — nothing to resume, not an error
    };
    let mut files: Vec<(SystemTime, PathBuf)> = entries
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let modified = std::fs::metadata(&path).ok()?.modified().ok()?;
            (path.extension()?.to_str()? == "jsonl").then_some((modified, path))
        })
        .collect();
    files.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    for (_, path) in files {
        if let Ok(loaded) = load(&path) {
            return Ok(Some(loaded));
        }
    }
    Ok(None)
}

/// Parse one session file: header line, then one message per line.
///
/// Tolerates a torn final line (crash mid-append) — an unparsable line
/// is fatal anywhere *except* at the end of the file, where it is
/// treated as the aftermath of an interrupted write and dropped.
pub fn load(path: &Path) -> Result<LoadedSession, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
    let mut header: Option<SessionHeader> = None;
    let mut messages = Vec::new();
    let lines: Vec<&str> = raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    for (index, line) in lines.iter().enumerate() {
        match parse_line(line) {
            Ok(Entry::Header(parsed)) => {
                if header.is_some() || !messages.is_empty() {
                    return Err(format!(
                        "{}: unexpected second session header",
                        path.display()
                    ));
                }
                header = Some(parsed);
            }
            Ok(Entry::Message(message)) => {
                if header.is_none() {
                    return Err(format!("{}: message before session header", path.display()));
                }
                messages.push(message);
            }
            Err(err) => {
                let is_torn_tail = index == lines.len() - 1;
                if is_torn_tail {
                    break; // crash mid-append; everything before it stands
                }
                return Err(format!("{}: {err}", path.display()));
            }
        }
    }
    let header = header.ok_or_else(|| format!("{}: no session header", path.display()))?;
    Ok(LoadedSession {
        header,
        messages,
        path: path.to_path_buf(),
    })
}

/// hi's session directory: `<data-dir>/sessions`.
pub fn sessions_dir() -> Option<PathBuf> {
    data_dir().map(|dir| dir.join("sessions"))
}

/// pi-style project key: `/Users/x/Projects/hi` → `--Users-x-Projects-hi--`.
///
/// Encoded into one filesystem-safe component (no nested directories),
/// unique per absolute path, and stable across machines with the same
/// layout — which is exactly what a synced home directory needs.
pub(crate) fn project_key(cwd: &Path) -> String {
    let text = cwd.to_string_lossy();
    let trimmed = text.strip_prefix('/').unwrap_or(&text);
    format!("--{}--", trimmed.replace('/', "-"))
}

/// One parsed line of a session file.
enum Entry {
    Header(SessionHeader),
    Message(Message),
}

fn parse_line(line: &str) -> Result<Entry, String> {
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|err| format!("malformed line: {err}"))?;
    match value.get("type").and_then(|kind| kind.as_str()) {
        // SessionHeader ignores the unknown "type" key, like every hi file.
        Some("session") => Ok(Entry::Header(
            serde_json::from_value(value).map_err(|err| format!("malformed header: {err}"))?,
        )),
        Some("message") => {
            let message = value.get("message").ok_or("message line missing payload")?;
            Ok(Entry::Message(
                serde_json::from_value(message.clone())
                    .map_err(|err| format!("malformed message: {err}"))?,
            ))
        }
        other => Err(format!(
            "unknown line type: {:?}",
            other.unwrap_or("<none>")
        )),
    }
}

fn header_line(header: &SessionHeader) -> Result<String, String> {
    let mut value =
        serde_json::to_value(header).map_err(|err| format!("cannot serialize header: {err}"))?;
    value["type"] = serde_json::json!("session");
    Ok(value.to_string())
}

fn message_line(message: &Message) -> Result<String, String> {
    let payload =
        serde_json::to_value(message).map_err(|err| format!("cannot serialize message: {err}"))?;
    Ok(serde_json::json!({ "type": "message", "message": payload }).to_string())
}

fn epoch_millis() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn read_header(path: &Path) -> Result<SessionHeader, String> {
    load(path).map(|loaded| loaded.header)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{ToolCall, ToolResult};

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hi-session-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn sample_messages() -> Vec<Message> {
        vec![
            Message::User("look at src".into()),
            Message::Assistant("Checking. ".into()),
            Message::ToolCall(ToolCall {
                id: "call_1".into(),
                name: "read".into(),
                arguments: r#"{"path":"src"}"#.into(),
            }),
            Message::ToolResult(ToolResult {
                call_id: "call_1".into(),
                name: "read".into(),
                content: "fn main() {}".into(),
                is_error: false,
            }),
            Message::ToolResult(ToolResult {
                call_id: "call_9".into(),
                name: "write".into(),
                content: "disk full".into(),
                is_error: true,
            }),
            Message::Error("local annotation — persisted, never sent to the model".into()),
            Message::System("dim note".into()),
            Message::Assistant("All done.".into()),
        ]
    }

    #[test]
    fn project_key_encodes_cwd_pi_style() {
        assert_eq!(
            project_key(Path::new("/Users/x/Projects/hi")),
            "--Users-x-Projects-hi--"
        );
        assert_eq!(project_key(Path::new("/tmp")), "--tmp--");
    }

    #[test]
    fn start_append_resume_round_trips_every_message_kind() {
        let root = temp_root("roundtrip");
        let cwd = Path::new("/tmp/proj");
        let messages = sample_messages();

        let mut session = Session::start_in(&root, cwd, "zai", "glm-5.3", Some("high")).unwrap();
        session.append(&messages).unwrap();
        let path = session.path().to_path_buf();

        // Resume reopens the same file for appending — one session, one file.
        let mut reopened = Session::resume(&path).unwrap();
        assert_eq!(reopened.path(), path.as_path());
        reopened
            .append(&[Message::User("one more".into())])
            .unwrap();

        let loaded = load(&path).unwrap();
        assert_eq!(loaded.header.version, 1);
        assert_eq!(loaded.header.provider, "zai");
        assert_eq!(loaded.header.model, "glm-5.3");
        assert_eq!(loaded.header.thinking.as_deref(), Some("high"));
        assert_eq!(loaded.header.cwd, "/tmp/proj");
        let mut expected = messages;
        expected.push(Message::User("one more".into()));
        assert_eq!(loaded.messages, expected);
        assert!(
            loaded
                .path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with(".jsonl")
        );
    }

    #[test]
    fn lines_are_flat_camel_case_json() {
        let root = temp_root("format");
        let mut session = Session::start_in(&root, Path::new("/p"), "prov", "mod", None).unwrap();
        session
            .append(&[Message::ToolResult(ToolResult {
                call_id: "c1".into(),
                name: "t".into(),
                content: "ok".into(),
                is_error: true,
            })])
            .unwrap();
        let raw = std::fs::read_to_string(session.path()).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 2);
        // Header: flat, camelCase, with the discriminator key.
        let header: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(header["type"], "session");
        assert_eq!(header["version"], 1);
        assert!(header.get("createdAt").is_some());
        assert!(header.get("created_at").is_none());
        // Message line: envelope + camelCase payload fields.
        let line: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(line["type"], "message");
        assert_eq!(line["message"]["toolResult"]["callId"], "c1");
        assert_eq!(line["message"]["toolResult"]["isError"], true);
    }

    #[test]
    fn torn_final_line_is_tolerated_mid_file_line_is_not() {
        let root = temp_root("torn");
        let mut session = Session::start_in(&root, Path::new("/p"), "prov", "mod", None).unwrap();
        session.append(&[Message::User("survives".into())]).unwrap();
        let path = session.path().to_path_buf();

        // Simulate a crash mid-append: a truncated final line.
        let mut raw = std::fs::read_to_string(&path).unwrap();
        raw.push_str("{\"type\":\"message\",\"message\":{\"assi");
        std::fs::write(&path, raw).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.messages, vec![Message::User("survives".into())]);

        // The same malformation mid-file is a loud error instead.
        let mut raw = std::fs::read_to_string(&path).unwrap();
        raw.push_str("\n{\"type\":\"message\",\"message\":{\"assi\"}\n");
        raw.push_str("{\"type\":\"message\",\"message\":{\"user\":\"end\"}}");
        std::fs::write(&path, raw).unwrap();
        assert!(load(&path).is_err());
    }

    #[test]
    fn latest_picks_the_newest_readable_session_for_the_project() {
        let root = temp_root("latest");
        let cwd = Path::new("/tmp/proj");
        let mut first = Session::start_in(&root, cwd, "p", "old", None).unwrap();
        first.append(&[Message::User("first".into())]).unwrap();
        // A few milliseconds later → later mtime, later filename.
        std::thread::sleep(std::time::Duration::from_millis(5));
        let mut second = Session::start_in(&root, cwd, "p", "new", None).unwrap();
        second.append(&[Message::User("second".into())]).unwrap();

        // A different project's session must not interfere.
        let mut other = Session::start_in(&root, Path::new("/tmp/other"), "p", "o", None).unwrap();
        other.append(&[Message::User("elsewhere".into())]).unwrap();

        let loaded = latest_for_root(&root, cwd).unwrap().unwrap();
        assert_eq!(loaded.header.model, "new");
        assert_eq!(loaded.messages, vec![Message::User("second".into())]);
    }

    #[test]
    fn latest_with_no_sessions_is_none_not_an_error() {
        let root = temp_root("empty");
        assert!(
            latest_for_root(&root, Path::new("/nowhere"))
                .unwrap()
                .is_none()
        );
    }

    /// `latest` against a chosen root — the testable half (the real
    /// `latest` derives the root from the environment).
    fn latest_for_root(root: &Path, cwd: &Path) -> Result<Option<LoadedSession>, String> {
        let dir = root.join(project_key(cwd));
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Ok(None);
        };
        let mut files: Vec<(SystemTime, PathBuf)> = entries
            .filter_map(|entry| {
                let path = entry.ok()?.path();
                let modified = std::fs::metadata(&path).ok()?.modified().ok()?;
                (path.extension()?.to_str()? == "jsonl").then_some((modified, path))
            })
            .collect();
        files.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
        for (_, path) in files {
            if let Ok(loaded) = load(&path) {
                return Ok(Some(loaded));
            }
        }
        Ok(None)
    }
}
