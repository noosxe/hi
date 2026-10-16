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
//! session header, every following line is one finalized `Message` or one
//! round's `usage` record (the bill, #85). Only turn-end state is
//! persisted — streamed tokens never touch disk — so a crash costs at
//! most the in-flight turn, and a torn final line is tolerated on read.
//! Everything is camelCase with unknown fields ignored, matching the
//! config files' forward-compatibility rule.
//!
//! Files are keyed by project directory because resume is a *project*
//! gesture: "continue what I was doing here". Cross-project resume is a
//! deliberate non-goal for now.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::config::data_dir;
use crate::event::Usage;
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
    /// Display name (`--name` / `-n`), shown in the session picker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// For forks: the source file's absolute path. The id, cwd, and
    /// timestamp of a forked session are its own; this is the lineage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
    /// What produced the session (`None` = an ordinary interactive
    /// session). Only subagents record today (#116); serde-default and
    /// skipped when absent, so old binaries read new files and vice
    /// versa.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// For subagent sessions: the parent session's id. Informational
    /// lineage, same rule as `forked_from`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}

fn default_version() -> u32 {
    1
}

/// A session file reopened for reading: header plus the transcript
/// and the replayed bill (#85) — `usage` merged across every recorded
/// round, `usage_last_prompt` the last recorded round's prompt size
/// (the context-share fact; the latest round's prompt IS the context).
#[derive(Debug, Clone)]
pub struct LoadedSession {
    pub header: SessionHeader,
    pub messages: Vec<Message>,
    pub usage: Usage,
    pub usage_last_prompt: u64,
    pub path: PathBuf,
}

/// The shared file-creation tail of `start_in` and
/// `start_subagent_in`: one project directory, one fresh file, the
/// header line first.
fn create(
    root: &Path,
    cwd: &Path,
    created_at: u64,
    header: SessionHeader,
) -> Result<Session, String> {
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
    /// provider/model/thinking snapshot, optionally named.
    pub fn start(
        provider: &str,
        model: &str,
        thinking: Option<&str>,
        name: Option<&str>,
    ) -> Result<Session, String> {
        let cwd = std::env::current_dir()
            .map_err(|err| format!("cannot determine current directory: {err}"))?;
        let root = sessions_dir().ok_or("could not determine data directory")?;
        Self::start_in(&root, &cwd, provider, model, thinking, name)
    }

    /// The path-independent half of [`start`], so tests can use temp
    /// directories (the `settings.rs` pattern).
    pub fn start_in(
        root: &Path,
        cwd: &Path,
        provider: &str,
        model: &str,
        thinking: Option<&str>,
        name: Option<&str>,
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
            name: name.map(str::to_string),
            forked_from: None,
            kind: None,
            parent: None,
        };
        create(root, cwd, created_at, header)
    }

    /// Open a subagent transcript (#116): an ordinary session file
    /// under the parent project's directory, marked `kind: "subagent"`
    /// with the parent session's id in the header. Same on-disk
    /// contract as every session — inspectable, resumable, pruned by
    /// the same retention rules.
    pub fn start_subagent_in(
        root: &Path,
        cwd: &Path,
        provider: &str,
        model: &str,
        thinking: Option<&str>,
        parent: Option<&str>,
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
            name: None,
            forked_from: None,
            kind: Some("subagent".to_string()),
            parent: parent.map(str::to_string),
        };
        create(root, cwd, created_at, header)
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

    /// The session's recorded thinking level, if any — the resume
    /// restore reads this (AD-0045 ruling 1).
    pub fn thinking(&self) -> Option<&str> {
        self.header.thinking.as_deref()
    }

    /// The recorded provider — the model-restore half of AD-0046
    /// reads this pair.
    pub fn provider(&self) -> &str {
        &self.header.provider
    }

    /// The recorded model (AD-0046).
    pub fn model(&self) -> &str {
        &self.header.model
    }

    /// Restamp the live header's `thinking` (AD-0045): the session
    /// file's first line is rewritten in place so the record tracks
    /// the level actually in use, and a later resume can restore it.
    /// The transcript below the header is untouched; the append
    /// handle is reopened against the rewritten file.
    pub fn set_thinking(&mut self, thinking: Option<&str>) -> Result<(), String> {
        self.header.thinking = thinking.map(str::to_string);
        self.rewrite_header_line()
    }

    /// Rewrite line one from the live header: read, replace, write,
    /// reopen the append handle. The header is always line one
    /// (`create` wrote it); an empty file means something external
    /// truncated it — fail loudly.
    fn rewrite_header_line(&mut self) -> Result<(), String> {
        let body = std::fs::read_to_string(&self.path)
            .map_err(|err| format!("cannot read {}: {err}", self.path.display()))?;
        let mut lines = body.splitn(2, '\n');
        if lines.next().is_none() || body.is_empty() || body.starts_with('\n') {
            return Err(format!("{} has no header line", self.path.display()));
        }
        let rest = lines.next().unwrap_or("");
        let mut out = String::with_capacity(body.len() + 16);
        out.push_str(&header_line(&self.header)?);
        out.push('\n');
        out.push_str(rest);
        std::fs::write(&self.path, out)
            .map_err(|err| format!("cannot write {}: {err}", self.path.display()))?;
        self.file = std::fs::OpenOptions::new()
            .append(true)
            .open(&self.path)
            .map_err(|err| format!("cannot reopen {}: {err}", self.path.display()))?;
        Ok(())
    }

    /// Restamp the live header's `provider`/`model` (AD-0046): the
    /// session file's first line is rewritten so the identity fields
    /// track a mid-session `/model` switch, and a later resume can
    /// restore the model the session actually ended on.
    pub fn set_model(&mut self, provider: &str, model: &str) -> Result<(), String> {
        self.header.provider = provider.to_string();
        self.header.model = model.to_string();
        self.rewrite_header_line()
    }

    /// Copy an existing session into a fresh file for `cwd` — the
    /// `--fork` path. The source is only ever read: the copy gets a new
    /// id and timestamp, the forking directory in its header, a
    /// `forkedFrom` pointer to the source, and re-serialized copies of
    /// the messages. An explicit name wins over the inherited one.
    pub fn fork_from(source: &Path, cwd: &Path, name: Option<&str>) -> Result<Session, String> {
        let root = sessions_dir().ok_or("could not determine data directory")?;
        Self::fork_from_in(&root, source, cwd, name)
    }

    /// The root-parameterized half of [`fork_from`].
    pub fn fork_from_in(
        root: &Path,
        source: &Path,
        cwd: &Path,
        name: Option<&str>,
    ) -> Result<Session, String> {
        let loaded = load(source)?;
        let created_at = epoch_millis();
        let header = SessionHeader {
            version: 1,
            id: uuid::Uuid::new_v4().to_string(),
            cwd: cwd.to_string_lossy().into_owned(),
            created_at,
            provider: loaded.header.provider.clone(),
            model: loaded.header.model.clone(),
            thinking: loaded.header.thinking.clone(),
            name: name.map(str::to_string).or(loaded.header.name.clone()),
            forked_from: Some(source.to_string_lossy().into_owned()),
            // A fork is an ordinary session: a human picked it up. The
            // subagent lineage rides `forkedFrom`, not `kind`.
            kind: None,
            parent: None,
        };
        let mut session = create(root, cwd, created_at, header)?;
        session.append(&loaded.messages)?;
        Ok(session)
    }

    /// The file this session writes to.
    /// The session's id — also the prompt-cache routing key on the
    /// wire, so every turn of one session routes to the same cache.
    pub fn id(&self) -> &str {
        &self.header.id
    }

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

    /// The session's effective name — the header's `--name` with any
    /// rename records applied (last wins).
    pub fn name(&self) -> Option<&str> {
        self.header.name.as_deref()
    }

    /// Append the session's name — the `name` record (#89). Names are
    /// appended, never rewritten: the file keeps every choice, replay
    /// takes the last, an empty name clears. Newlines collapse to
    /// spaces and the edges trim (pi's normalization) — a name is one
    /// line by construction. Flushed like every append.
    pub fn rename(&mut self, name: &str) -> Result<(), String> {
        let sanitized = name.replace(['\r', '\n'], " ").trim().to_string();
        writeln!(
            self.file,
            "{}",
            serde_json::json!({ "type": "name", "name": sanitized })
        )
        .map_err(|err| format!("cannot write {}: {err}", self.path.display()))?;
        self.header.name = if sanitized.is_empty() {
            None
        } else {
            Some(sanitized)
        };
        self.file
            .flush()
            .map_err(|err| format!("cannot write {}: {err}", self.path.display()))
    }

    /// Append one round's bill — the `usage` record (#85). One line
    /// per round, flushed like the messages; replay merges the
    /// records, and the last one's prompt size is the context fact.
    pub fn append_usage(&mut self, usage: &Usage) -> Result<(), String> {
        let payload =
            serde_json::to_value(usage).map_err(|err| format!("cannot serialize usage: {err}"))?;
        writeln!(
            self.file,
            "{}",
            serde_json::json!({ "type": "usage", "usage": payload })
        )
        .map_err(|err| format!("cannot write {}: {err}", self.path.display()))?;
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
    latest_in_root(&root, cwd)
}

/// The root-parameterized half of [`latest`] (the `settings.rs` pattern).
pub fn latest_in_root(root: &Path, cwd: &Path) -> Result<Option<LoadedSession>, String> {
    Ok(sessions_in_root(root, cwd)?
        .into_iter()
        .find_map(|summary| load(&summary.path).ok()))
}

/// A session file's selection facts: where it is, what its header says,
/// when it was last written.
#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub path: PathBuf,
    pub header: SessionHeader,
    pub modified: SystemTime,
}

/// List this project's sessions, newest first — the `--resume` picker's
/// menu. Files whose header won't parse are skipped, not fatal: one
/// broken file must not hide the rest.
pub fn sessions_in(cwd: &Path) -> Result<Vec<SessionSummary>, String> {
    let root = sessions_dir().ok_or("could not determine data directory")?;
    sessions_in_root(&root, cwd)
}

/// The root-parameterized half of [`sessions_in`].
pub fn sessions_in_root(root: &Path, cwd: &Path) -> Result<Vec<SessionSummary>, String> {
    let dir = root.join(project_key(cwd));
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(Vec::new()); // no directory yet — no sessions, not an error
    };
    let mut summaries: Vec<SessionSummary> = entries
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            if path.extension()?.to_str()? != "jsonl" {
                return None;
            }
            let modified = std::fs::metadata(&path).ok()?.modified().ok()?;
            Some(SessionSummary {
                header: read_summary_header(&path)?,
                path,
                modified,
            })
        })
        .collect();
    summaries.sort_by_key(|summary| std::cmp::Reverse(summary.modified));
    Ok(summaries)
}

/// List every project's sessions, newest first — the global half of
/// `--session <id>` matching.
pub fn sessions_all() -> Result<Vec<SessionSummary>, String> {
    let root = sessions_dir().ok_or("could not determine data directory")?;
    sessions_all_root(&root)
}

/// The root-parameterized half of [`sessions_all`].
pub fn sessions_all_root(root: &Path) -> Result<Vec<SessionSummary>, String> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Ok(Vec::new());
    };
    let mut summaries = Vec::new();
    for entry in entries.flatten() {
        if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            summaries.extend(sessions_in_root(root, &project_dir_of(&entry.path()))?);
        }
    }
    summaries.sort_by_key(|summary| std::cmp::Reverse(summary.modified));
    Ok(summaries)
}

/// Opt-in session retention (#23): delete `<project>/*.jsonl` files
/// whose mtime is older than `max_age`. Sessions are user history, so
/// nothing is ever pruned unless the `sessionRetention` setting is
/// set (pi-parity verified: pi never deletes session files). Age-only
/// by ruling: a session being appended to refreshes its mtime, so a
/// live file is never a candidate — mtime *is* the liveness signal,
/// and no byte ceiling exists to evict fresh files. Only plain
/// `.jsonl` files are touched (a directory named like one survives);
/// every error is swallowed; a missing root is a silent no-op.
/// Returns the removal count.
pub fn prune_sessions(root: &Path, max_age: Duration) -> usize {
    let Ok(projects) = std::fs::read_dir(root) else {
        return 0;
    };
    let mut removed = 0;
    for project in projects.flatten() {
        if !project.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Ok(files) = std::fs::read_dir(project.path()) else {
            continue;
        };
        for file in files.flatten() {
            let is_our_kind = file.file_name().to_string_lossy().ends_with(".jsonl")
                && file.file_type().is_ok_and(|t| t.is_file());
            if !is_our_kind {
                continue;
            }
            let aged = file
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|m| m.elapsed().ok())
                .is_some_and(|d| d > max_age);
            if aged && std::fs::remove_file(file.path()).is_ok() {
                removed += 1;
            }
        }
    }
    removed
}

/// Invert [`project_key`]: the encoded directory component back to the
/// path it names (best-effort — a decoded cwd is display information).
fn project_dir_of(dir: &Path) -> PathBuf {
    let text = dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    PathBuf::from(text.trim_matches('-').replace('-', "/"))
}

/// Where a `--session` / `--fork` argument points. Path-shaped
/// arguments stay paths; ids are matched in the current project first,
/// then across every project.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionRef {
    /// A session for the current project (or an explicit file path).
    Local(PathBuf),
    /// A session from another project — forking territory.
    Global { path: PathBuf, cwd: String },
    /// No path or id matches, here or anywhere.
    NotFound,
}

/// Resolve a `--session` / `--fork` argument: path shape wins, then
/// exact id, then id prefix in this project, then exact/prefix across
/// all projects (pi's order).
pub fn resolve(cwd: &Path, arg: &str) -> Result<SessionRef, String> {
    let root = sessions_dir().ok_or("could not determine data directory")?;
    resolve_in_root(&root, cwd, arg)
}

/// The root-parameterized half of [`resolve`].
pub fn resolve_in_root(root: &Path, cwd: &Path, arg: &str) -> Result<SessionRef, String> {
    // Path-shaped: `/x/y.jsonl`, `sub/dir`, `x.jsonl` — opened relative
    // to the process cwd, existence checked by the opener.
    if arg.contains('/') || arg.contains('\\') || arg.ends_with(".jsonl") {
        return Ok(SessionRef::Local(PathBuf::from(arg)));
    }
    let local = sessions_in_root(root, cwd)?;
    if let Some(hit) = local
        .iter()
        .find(|s| s.header.id == arg)
        .or_else(|| local.iter().find(|s| s.header.id.starts_with(arg)))
    {
        return Ok(SessionRef::Local(hit.path.clone()));
    }
    let all = sessions_all_root(root)?;
    if let Some(hit) = all
        .iter()
        .find(|s| s.header.id == arg)
        .or_else(|| all.iter().find(|s| s.header.id.starts_with(arg)))
    {
        return Ok(SessionRef::Global {
            path: hit.path.clone(),
            cwd: hit.header.cwd.clone(),
        });
    }
    Ok(SessionRef::NotFound)
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
    let mut usage = Usage::default();
    let mut usage_last_prompt = 0u64;
    let mut name = None;
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
                // The header's --name seeds the effective name; name
                // records then win, last one standing.
                name = parsed.name.clone();
                header = Some(parsed);
            }
            Ok(Entry::Message(message)) => {
                if header.is_none() {
                    return Err(format!("{}: message before session header", path.display()));
                }
                messages.push(message);
            }
            Ok(Entry::Usage(recorded)) => {
                if header.is_none() {
                    return Err(format!("{}: usage before session header", path.display()));
                }
                usage_last_prompt = recorded.prompt_tokens;
                usage.merge(recorded);
            }
            Ok(Entry::Name(recorded)) => {
                if header.is_none() {
                    return Err(format!("{}: name before session header", path.display()));
                }
                // Last wins; an empty record clears (the header's
                // --name included).
                name = if recorded.is_empty() {
                    None
                } else {
                    Some(recorded)
                };
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
    let mut header = header.ok_or_else(|| format!("{}: no session header", path.display()))?;
    header.name = name;
    Ok(LoadedSession {
        header,
        messages,
        usage,
        usage_last_prompt,
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
pub fn project_key(cwd: &Path) -> String {
    let text = cwd.to_string_lossy();
    let trimmed = text.strip_prefix('/').unwrap_or(&text);
    format!("--{}--", trimmed.replace('/', "-"))
}

/// One parsed line of a session file.
enum Entry {
    Header(SessionHeader),
    Message(Message),
    Usage(Usage),
    Name(String),
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
        Some("usage") => {
            let usage = value.get("usage").ok_or("usage line missing payload")?;
            Ok(Entry::Usage(
                serde_json::from_value(usage.clone())
                    .map_err(|err| format!("malformed usage: {err}"))?,
            ))
        }
        Some("name") => {
            let name = value
                .get("name")
                .and_then(|name| name.as_str())
                .ok_or("name line missing payload")?;
            Ok(Entry::Name(name.to_string()))
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

/// Parse just the header line — the listing path, which never needs the
/// transcript bodies. `None` for anything unreadable or headerless.
/// The session's effective header — the first line with any `name`
/// records applied (last wins). The summary pass scans the whole file
/// once per listing, not per frame; only lines carrying the record
/// key are parsed.
fn read_summary_header(path: &Path) -> Option<SessionHeader> {
    let raw = std::fs::read_to_string(path).ok()?;
    let mut header: Option<SessionHeader> = None;
    let mut name = None;
    for line in raw.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if !line.contains("\"type\"") {
            continue;
        }
        match parse_line(line) {
            Ok(Entry::Header(parsed)) => {
                name = parsed.name.clone();
                header = Some(parsed);
            }
            Ok(Entry::Name(recorded)) => {
                name = if recorded.is_empty() {
                    None
                } else {
                    Some(recorded)
                };
            }
            // Records other than names don't move the summary; a
            // malformed name line is skipped here (the summary pass is
            // best-effort) but still rejected by `load`.
            _ => {}
        }
    }
    let mut header = header?;
    header.name = name;
    Some(header)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{ImageBlock, ToolCall, ToolResult};

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
                images: Vec::new(),
            }),
            Message::ToolResult(ToolResult {
                call_id: "call_9".into(),
                name: "write".into(),
                content: "disk full".into(),
                is_error: true,
                images: Vec::new(),
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
    fn subagent_sessions_carry_kind_and_parent() {
        let root = std::env::temp_dir().join(format!("hi-sess-sub-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let session = Session::start_subagent_in(
            &root,
            Path::new("/p"),
            "prov",
            "mod",
            None,
            Some("parent-1"),
        )
        .unwrap();
        assert_eq!(session.header().kind.as_deref(), Some("subagent"));
        assert_eq!(session.header().parent.as_deref(), Some("parent-1"));
        let loaded = load(session.path()).unwrap();
        assert_eq!(loaded.header.kind.as_deref(), Some("subagent"));
        assert_eq!(loaded.header.parent.as_deref(), Some("parent-1"));
        // Ordinary sessions stay unmarked.
        let plain = Session::start_in(&root, Path::new("/p"), "prov", "mod", None, None).unwrap();
        assert_eq!(plain.header().kind, None);
        assert_eq!(plain.header().parent, None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn start_append_resume_round_trips_every_message_kind() {
        let root = temp_root("roundtrip");
        let cwd = Path::new("/tmp/proj");
        let messages = sample_messages();

        let mut session =
            Session::start_in(&root, cwd, "zai", "glm-5.3", Some("high"), None).unwrap();
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
    fn set_thinking_rewrites_the_header_in_place() {
        let root = temp_root("restamp");
        let mut session =
            Session::start_in(&root, Path::new("/p"), "zai", "glm-5.3", Some("high"), None)
                .unwrap();
        session.append(&[Message::User("turn one".into())]).unwrap();
        let path = session.path().to_path_buf();

        // AD-0045: a mid-session switch restamps the live header so
        // the record tracks reality.
        session.set_thinking(Some("low")).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.header.thinking.as_deref(), Some("low"));
        // The transcript survives the rewrite.
        assert_eq!(loaded.messages, vec![Message::User("turn one".into())]);

        // And the session keeps accepting records.
        session.append(&[Message::User("turn two".into())]).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.header.thinking.as_deref(), Some("low"));

        // Back to auto clears the field.
        session.set_thinking(None).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.header.thinking, None);
        assert_eq!(loaded.messages.len(), 2);
    }

    #[test]
    fn set_model_restamps_provider_and_model_together() {
        let root = temp_root("restamp-model");
        let mut session =
            Session::start_in(&root, Path::new("/p"), "zai", "glm-5.3", Some("high"), None)
                .unwrap();
        session
            .append(&[Message::User("before the switch".into())])
            .unwrap();
        let path = session.path().to_path_buf();

        // AD-0046: a mid-session /model switch restamps the identity
        // fields so resume restores the model the session ended on.
        session.set_model("openai", "gpt-6.2").unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.header.provider, "openai");
        assert_eq!(loaded.header.model, "gpt-6.2");
        assert_eq!(loaded.header.thinking.as_deref(), Some("high"));
        assert_eq!(
            loaded.messages,
            vec![Message::User("before the switch".into())]
        );
        // The file keeps accepting records.
        session.append(&[Message::User("after".into())]).unwrap();
        assert_eq!(load(&path).unwrap().messages.len(), 2);
    }

    #[test]
    fn set_thinking_works_on_headers_without_the_field() {
        // An old binary's session: the header predates `thinking`
        // (serde default None). Restamping must not require it.
        let root = temp_root("restamp-old");
        let session =
            Session::start_in(&root, Path::new("/p"), "zai", "glm-5.3", None, None).unwrap();
        let path = session.path().to_path_buf();
        let mut reopened = Session::resume(&path).unwrap();
        reopened.set_thinking(Some("max")).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.header.thinking.as_deref(), Some("max"));
    }

    #[test]
    fn lines_are_flat_camel_case_json() {
        let root = temp_root("format");
        let mut session =
            Session::start_in(&root, Path::new("/p"), "prov", "mod", None, None).unwrap();
        session
            .append(&[Message::ToolResult(ToolResult {
                call_id: "c1".into(),
                name: "t".into(),
                content: "ok".into(),
                is_error: true,
                images: Vec::new(),
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
        let mut session =
            Session::start_in(&root, Path::new("/p"), "prov", "mod", None, None).unwrap();
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
        let mut first = Session::start_in(&root, cwd, "p", "old", None, None).unwrap();
        first.append(&[Message::User("first".into())]).unwrap();
        // A few milliseconds later → later mtime, later filename.
        std::thread::sleep(std::time::Duration::from_millis(5));
        let mut second = Session::start_in(&root, cwd, "p", "new", None, None).unwrap();
        second.append(&[Message::User("second".into())]).unwrap();

        // A different project's session must not interfere.
        let mut other =
            Session::start_in(&root, Path::new("/tmp/other"), "p", "o", None, None).unwrap();
        other.append(&[Message::User("elsewhere".into())]).unwrap();

        let loaded = latest_in_root(&root, cwd).unwrap().unwrap();
        assert_eq!(loaded.header.model, "new");
        assert_eq!(loaded.messages, vec![Message::User("second".into())]);
    }

    #[test]
    fn latest_with_no_sessions_is_none_not_an_error() {
        let root = temp_root("empty");
        assert!(
            latest_in_root(&root, Path::new("/nowhere"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn sessions_in_lists_newest_first_and_skips_unreadable_files() {
        let root = temp_root("list");
        let cwd = Path::new("/tmp/proj");
        let mut first = Session::start_in(&root, cwd, "p", "old", None, None).unwrap();
        first.append(&[Message::User("first".into())]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let mut second = Session::start_in(&root, cwd, "p", "new", None, None).unwrap();
        second.append(&[Message::User("second".into())]).unwrap();
        // Garbage sharing the extension must not break the listing.
        std::fs::write(
            root.join(project_key(cwd)).join("garbage.jsonl"),
            "not json",
        )
        .unwrap();

        let listed = sessions_in_root(&root, cwd).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].header.model, "new");
        assert_eq!(listed[1].header.model, "old");
        assert!(listed[0].modified >= listed[1].modified);
        assert_eq!(listed[0].path, second.path());
        // Empty project: a list, not an error.
        assert!(
            sessions_in_root(&root, Path::new("/nowhere"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn resolve_prefers_path_then_local_id_then_global() {
        let root = temp_root("resolve");
        let here = Path::new("/tmp/proj");
        let there = Path::new("/tmp/other");
        let mut local = Session::start_in(&root, here, "p", "m", None, None).unwrap();
        local.append(&[Message::User("local".into())]).unwrap();
        let mut away = Session::start_in(&root, there, "p", "m", None, None).unwrap();
        away.append(&[Message::User("away".into())]).unwrap();
        let local_id = local.header().id.clone();
        let away_id = away.header().id.clone();

        // Path shape stays a path, un-exist-checked (the opener reports).
        assert_eq!(
            resolve_in_root(&root, here, "some/where.jsonl"),
            Ok(SessionRef::Local(PathBuf::from("some/where.jsonl")))
        );
        // Exact and prefix ids match within the project first.
        assert_eq!(
            resolve_in_root(&root, here, &local_id),
            Ok(SessionRef::Local(local.path().to_path_buf()))
        );
        assert_eq!(
            resolve_in_root(&root, here, &local_id[..8]),
            Ok(SessionRef::Local(local.path().to_path_buf()))
        );
        // Other projects come back as Global, carrying their cwd.
        assert_eq!(
            resolve_in_root(&root, here, &away_id),
            Ok(SessionRef::Global {
                path: away.path().to_path_buf(),
                cwd: "/tmp/other".into(),
            })
        );
        assert_eq!(
            resolve_in_root(&root, here, &away_id[..8]),
            Ok(SessionRef::Global {
                path: away.path().to_path_buf(),
                cwd: "/tmp/other".into(),
            })
        );
        assert_eq!(
            resolve_in_root(&root, here, "zzz"),
            Ok(SessionRef::NotFound)
        );
    }

    #[test]
    fn fork_copies_messages_and_never_touches_the_source() {
        let root = temp_root("fork");
        let origin = Path::new("/tmp/origin");
        let mut source = Session::start_in(
            &root,
            origin,
            "prov",
            "glm",
            Some("high"),
            Some("origin name"),
        )
        .unwrap();
        source.append(&sample_messages()).unwrap();
        let source_path = source.path().to_path_buf();
        let before = std::fs::read_to_string(&source_path).unwrap();

        let fork_cwd = Path::new("/tmp/forked");
        let session = Session::fork_from_in(&root, &source_path, fork_cwd, None).unwrap();
        assert_eq!(
            before,
            std::fs::read_to_string(&source_path).unwrap(),
            "the source file is only ever read"
        );

        let header = session.header();
        assert_ne!(header.id, source.header().id);
        assert_eq!(header.cwd, "/tmp/forked");
        assert_eq!(
            header.forked_from.as_deref(),
            Some(source_path.to_string_lossy().as_ref())
        );
        assert_eq!(header.name.as_deref(), Some("origin name"));
        assert_eq!(header.provider, "prov");
        assert_eq!(header.thinking.as_deref(), Some("high"));

        let loaded = load(session.path()).unwrap();
        assert_eq!(loaded.messages, sample_messages());

        // An explicit name wins over the inherited one.
        let renamed = Session::fork_from_in(&root, &source_path, fork_cwd, Some("mine")).unwrap();
        assert_eq!(renamed.header().name.as_deref(), Some("mine"));
        assert_eq!(
            renamed.header().forked_from.as_deref(),
            Some(source_path.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn name_and_forked_from_are_optional_camel_case_fields() {
        let root = temp_root("nameheader");
        let session =
            Session::start_in(&root, Path::new("/p"), "prov", "mod", None, Some("my work"))
                .unwrap();
        let raw = std::fs::read_to_string(session.path()).unwrap();
        let header: serde_json::Value = serde_json::from_str(raw.lines().next().unwrap()).unwrap();
        assert_eq!(header["name"], "my work");
        assert!(header.get("forkedFrom").is_none());

        // Headers written before the fields existed still parse.
        let legacy: SessionHeader = serde_json::from_str(
            r#"{"type":"session","version":1,"id":"i","cwd":"/p","createdAt":1,"provider":"x","model":"y"}"#,
        )
        .unwrap();
        assert_eq!(legacy.name, None);
        assert_eq!(legacy.forked_from, None);
    }
    #[test]
    fn usage_records_round_trip_and_replay_the_bill() {
        let root = temp_root("usagereplay");
        let cwd = Path::new("/p");
        let mut session = Session::start_in(&root, cwd, "prov", "mod", None, None).unwrap();
        session.append(&[Message::User("turn one".into())]).unwrap();
        session
            .append_usage(&crate::event::Usage {
                prompt_tokens: 1000,
                completion_tokens: 250,
                cached_tokens: Some(700),
                reasoning_tokens: Some(100),
            })
            .unwrap();
        session.append(&[Message::User("turn two".into())]).unwrap();
        session
            .append_usage(&crate::event::Usage {
                prompt_tokens: 2000,
                completion_tokens: 300,
                cached_tokens: None,
                reasoning_tokens: Some(50),
            })
            .unwrap();
        let path = session.path().to_path_buf();

        // On disk: the record envelope, camelCase, absent details omitted.
        let raw = std::fs::read_to_string(&path).unwrap();
        let line = raw
            .lines()
            .find(|l| l.contains(r#""type":"usage""#))
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(value["usage"]["promptTokens"], 1000);
        assert_eq!(value["usage"]["cachedTokens"], 700);
        assert!(value["usage"].get("reasoning_tokens").is_none());
        assert!(value["usage"].get("prompt_tokens").is_none());
        assert!(
            !raw.lines().any(|l| l.contains(r#""cachedTokens":null"#)),
            "absent details are omitted, not null: {raw}"
        );

        // Replay: the records merge, the last prompt is the context fact.
        let loaded = load(&path).unwrap();
        assert_eq!(
            loaded.usage,
            crate::event::Usage {
                prompt_tokens: 3000,
                completion_tokens: 550,
                cached_tokens: Some(700),
                reasoning_tokens: Some(150),
            }
        );
        assert_eq!(loaded.usage_last_prompt, 2000);
    }

    #[test]
    fn sessions_without_usage_records_load_with_an_empty_bill() {
        // The forward-compat seam: a session written before #85 loads
        // with a zero bill, and a usage record before the header is as
        // loud as a message before one.
        let root = temp_root("nousage");
        let session = Session::start_in(&root, Path::new("/p"), "prov", "mod", None, None).unwrap();
        let header = std::fs::read_to_string(session.path()).unwrap();
        std::fs::write(
            session.path(),
            format!("{header}{{\"type\":\"message\",\"message\":{{\"user\":\"old turn\"}}}}\n"),
        )
        .unwrap();
        let loaded = load(session.path()).unwrap();
        assert_eq!(loaded.messages, vec![Message::User("old turn".into())]);
        assert_eq!(loaded.usage, crate::event::Usage::default());
        assert_eq!(loaded.usage_last_prompt, 0);

        let raw = std::fs::read_to_string(session.path()).unwrap();
        std::fs::write(
            session.path(),
            format!(
            "{raw}{{\"type\":\"usage\",\"usage\":{{\"promptTokens\":1,\"completionTokens\":2}}}}\n"
        ),
        )
        .unwrap();
        // ...but this file has no header problem — the usage line comes
        // after the header, so it replays fine.
        let loaded = load(session.path()).unwrap();
        assert_eq!(loaded.usage.prompt_tokens, 1);
        assert_eq!(loaded.usage.completion_tokens, 2);

        // A usage record before any header IS an error, like messages.
        let orphan = root.join("orphan.jsonl");
        std::fs::write(
            &orphan,
            "{\"type\":\"usage\",\"usage\":{\"promptTokens\":1,\"completionTokens\":2}}\n",
        )
        .unwrap();
        assert!(load(&orphan).is_err());
    }

    #[test]
    fn fork_starts_with_an_empty_bill() {
        // A fork is a new session: it inherits the transcript, never
        // the economics.
        let root = temp_root("forkbill");
        let mut source =
            Session::start_in(&root, Path::new("/p"), "prov", "mod", None, None).unwrap();
        source.append(&[Message::User("history".into())]).unwrap();
        source
            .append_usage(&crate::event::Usage {
                prompt_tokens: 5000,
                completion_tokens: 900,
                cached_tokens: None,
                reasoning_tokens: None,
            })
            .unwrap();
        let source_path = source.path().to_path_buf();

        let fork = Session::fork_from_in(&root, &source_path, Path::new("/q"), None).unwrap();
        let loaded = load(fork.path()).unwrap();
        assert_eq!(loaded.messages, vec![Message::User("history".into())]);
        assert_eq!(loaded.usage, crate::event::Usage::default());
    }

    #[test]
    fn name_records_replay_last_wins_and_empty_clears() {
        // Names are appended, never rewritten in place (#89): the file
        // keeps every choice, replay takes the last, an empty record
        // clears back to the header's (or no) name.
        let root = temp_root("names");
        let mut session =
            Session::start_in(&root, Path::new("/p"), "prov", "mod", None, Some("initial"))
                .unwrap();
        session.rename("the real fix").unwrap();
        session.rename("renamed again").unwrap();
        let raw = std::fs::read_to_string(session.path()).unwrap();
        assert!(
            raw.contains("{\"name\":\"the real fix\",\"type\":\"name\"}"),
            "{raw}"
        );
        let loaded = load(session.path()).unwrap();
        assert_eq!(loaded.header.name.as_deref(), Some("renamed again"));

        session.rename("").unwrap();
        let loaded = load(session.path()).unwrap();
        assert_eq!(loaded.header.name, None);
    }

    #[test]
    fn a_session_without_name_records_keeps_its_header_name() {
        // The forward-compat seam again: files written before #89 load
        // untouched, --name riding the header as always.
        let root = temp_root("nonames");
        let session =
            Session::start_in(&root, Path::new("/p"), "prov", "mod", None, Some("kept")).unwrap();
        let loaded = load(session.path()).unwrap();
        assert_eq!(loaded.header.name.as_deref(), Some("kept"));

        let session = Session::start_in(&root, Path::new("/q"), "prov", "mod", None, None).unwrap();
        let loaded = load(session.path()).unwrap();
        assert_eq!(loaded.header.name, None);
    }

    #[test]
    fn summaries_carry_the_effective_name() {
        // The picker lists summaries, not loads: the summary pass must
        // see past the first line to the rename records.
        let root = temp_root("sumnames");
        let mut session = Session::start_in(
            &root,
            Path::new("/p"),
            "prov",
            "mod",
            None,
            Some("header name"),
        )
        .unwrap();
        session.rename("picked").unwrap();
        let summaries = sessions_in_root(&root, Path::new("/p")).unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].header.name.as_deref(), Some("picked"));
    }

    #[test]
    fn a_name_record_before_the_header_is_an_error() {
        let root = temp_root("orphanname");
        let session = Session::start_in(&root, Path::new("/p"), "prov", "mod", None, None).unwrap();
        let header = std::fs::read_to_string(session.path()).unwrap();
        std::fs::write(
            session.path(),
            format!("{{\"type\":\"name\",\"name\":\"early\"}}\n{header}"),
        )
        .unwrap();
        assert!(load(session.path()).is_err());
    }

    #[test]
    fn images_round_trip_through_the_session_file() {
        let root = temp_root("images");
        let cwd = Path::new("/p");
        let mut session = Session::start_in(&root, cwd, "prov", "mod", None, None).unwrap();
        session
            .append(&[Message::ToolResult(ToolResult {
                call_id: "c1".into(),
                name: "read".into(),
                content: "Read image file [image/png] (attached)".into(),
                is_error: false,
                images: vec![ImageBlock {
                    data: "aW1n".into(),
                    mime_type: "image/png".into(),
                }],
            })])
            .unwrap();
        // On disk: camelCase, and only when non-empty.
        let raw = std::fs::read_to_string(session.path()).unwrap();
        assert!(
            raw.contains("\"images\":[{\"data\":\"aW1n\",\"mimeType\":\"image/png\"}]"),
            "raw: {raw}"
        );
        // Back in memory through the loader.
        let loaded = latest_in_root(&root, cwd).unwrap().unwrap();
        let Message::ToolResult(result) = &loaded.messages[0] else {
            panic!("expected the tool result");
        };
        assert_eq!(result.images.len(), 1);
        assert_eq!(result.images[0].mime_type, "image/png");
    }

    #[test]
    fn pre_image_session_files_load_without_images() {
        // serde(default) on `images` is the forward-compat seam: a
        // session written before images existed loads with none.
        let root = temp_root("preimage");
        let cwd = Path::new("/p");
        let session = Session::start_in(&root, cwd, "prov", "mod", None, None).unwrap();
        let header = std::fs::read_to_string(session.path()).unwrap();
        let legacy = format!(
            "{header}{{\"type\":\"message\",\"message\":{{\"toolResult\":{{\"callId\":\"c1\",\"name\":\"read\",\"content\":\"text only\",\"isError\":false}}}}}}\n"
        );
        std::fs::write(session.path(), legacy).unwrap();
        let loaded = latest_in_root(&root, cwd).unwrap().unwrap();
        assert_eq!(loaded.messages.len(), 1, "got: {:?}", loaded.messages);
        let Message::ToolResult(result) = &loaded.messages[0] else {
            panic!("expected the tool result");
        };
        assert!(result.images.is_empty());
        assert_eq!(result.content, "text only");
    }

    #[test]
    fn prune_removes_only_aged_session_files() {
        let root = temp_root("retention");
        let cwd = Path::new("/ret-demo");
        Session::start_in(&root, cwd, "p", "m", None, None).unwrap();
        Session::start_in(&root, cwd, "p", "m", None, None).unwrap();
        let dir = root.join(project_key(cwd));
        let paths: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .collect();
        assert_eq!(paths.len(), 2);
        backdate(&paths[0], Duration::from_secs(31 * 24 * 3600));
        backdate(&paths[1], Duration::from_secs(8 * 24 * 3600));
        let removed = prune_sessions(&root, Duration::from_secs(30 * 24 * 3600));
        assert_eq!(removed, 1);
        assert!(!paths[0].exists());
        assert!(paths[1].exists());
    }

    #[test]
    fn prune_walks_every_project_dir() {
        let root = temp_root("retention-multi");
        Session::start_in(&root, Path::new("/a"), "p", "m", None, None).unwrap();
        Session::start_in(&root, Path::new("/b"), "p", "m", None, None).unwrap();
        // Don't hardcode the project-key encoding — walk whatever
        // subdirs exist.
        for entry in std::fs::read_dir(&root).unwrap().flatten() {
            let file = std::fs::read_dir(entry.path())
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            backdate(&file, Duration::from_secs(40 * 24 * 3600));
        }
        assert_eq!(
            prune_sessions(&root, Duration::from_secs(30 * 24 * 3600)),
            2
        );
    }

    #[test]
    fn prune_never_touches_non_session_entries() {
        let root = temp_root("retention-stray");
        let dir = root.join(project_key(Path::new("/stray")));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("notes.txt"), "user data").unwrap();
        std::fs::write(dir.join("keep.jsonl"), "fresh session").unwrap();
        std::fs::create_dir_all(dir.join("9c9e9999-0000-0000-0000-000000000000.jsonl")).unwrap();
        // A jsonl so old it would go if it were a file.
        let old = dir.join("1000000000000_deadbeef-0000-0000-0000-000000000000.jsonl");
        std::fs::write(&old, "x").unwrap();
        backdate(&old, Duration::from_secs(400 * 24 * 3600));
        let removed = prune_sessions(&root, Duration::from_secs(30 * 24 * 3600));
        assert_eq!(removed, 1);
        assert!(dir.join("notes.txt").exists());
        assert!(dir.join("keep.jsonl").exists());
        assert!(
            dir.join("9c9e9999-0000-0000-0000-000000000000.jsonl")
                .exists()
        );
        // A missing root is a silent no-op.
        assert_eq!(prune_sessions(&root.join("nope"), Duration::ZERO), 0);
    }

    #[test]
    fn latest_still_finds_the_newest_after_prune() {
        let root = temp_root("retention-latest");
        let cwd = Path::new("/ret-latest");
        Session::start_in(&root, cwd, "p", "old", None, None).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        Session::start_in(&root, cwd, "p", "new", None, None).unwrap();
        let dir = root.join(project_key(cwd));
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .collect();
        paths.sort();
        backdate(&paths[0], Duration::from_secs(31 * 24 * 3600));
        prune_sessions(&root, Duration::from_secs(30 * 24 * 3600));
        let found = latest_in_root(&root, cwd)
            .unwrap()
            .expect("newest survives");
        assert!(found.path.file_name().unwrap() == paths[1].file_name().unwrap());
    }

    /// Backdate a file's mtime (the tasks.rs retention-test pattern).
    fn backdate(path: &Path, age: Duration) {
        let f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::now() - age))
            .unwrap();
    }
}
