//! Built-in agent tools — the capabilities that ship with the harness (#26).
//!
//! Design, in short:
//!
//! - Every tool captures its `cwd` at construction; `execute` stays sync on
//!   the agent-loop thread, per the [`Tool`] trait contract.
//! - Every result passes through [`truncate`] — 2000 lines / 50 KiB, head
//!   for inspection tools, tail for bash.
//! - Each tool can contribute a [`Tool::prompt_note`] to the system prompt;
//!   [`ToolRegistry::prompt_notes`] collects them (weave point: the CLI).
//! - Paths resolve against `cwd` but are **not jailed** — the security
//!   stance (documented in `docs/core.md` alongside pi's) is that tools run
//!   with the session's OS permissions; the boundary is the OS, not us.
//!
//! Bundles ([`ToolBundle`]) are the curated sets: `read-only` for
//! observation, `coding` (the default — hi is a coding harness first) for
//! read/bash/edit/write, `all` for everything.

mod bash;
mod edit;
mod find;
mod grep;
mod ls;
mod read;
mod truncate;
mod walk;
mod write;

pub use bash::BashTool;
pub use edit::EditTool;
pub use find::FindTool;
pub use grep::GrepTool;
pub use ls::LsTool;
pub use read::ReadTool;
pub use truncate::{MAX_BYTES, MAX_LINES};
pub use write::WriteTool;

use crate::tool::{Tool, ToolRegistry};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Parse a bundle name (`none`, `read-only`, `coding`, `all`).
pub fn parse_bundle(raw: &str) -> Result<ToolBundle, String> {
    ToolBundle::parse(raw)
}

/// Which built-in tools a session runs with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolBundle {
    /// No built-ins — the pre-#26 behavior, and `--tools none`.
    None,
    /// read, grep, find, ls — observation only.
    ReadOnly,
    /// read, bash, edit, write — pi's default and hi's (a coding harness
    /// first, per the #26 ruling).
    #[default]
    Coding,
    /// Everything, read-only extras included.
    All,
}

impl ToolBundle {
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "none" => Ok(Self::None),
            "read-only" => Ok(Self::ReadOnly),
            "coding" => Ok(Self::Coding),
            "all" => Ok(Self::All),
            other => Err(format!(
                "unknown tool bundle {other:?} — use none, read-only, coding, or all"
            )),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::ReadOnly => "read-only",
            Self::Coding => "coding",
            Self::All => "all",
        }
    }

    /// Build the registry for this bundle, rooted at `cwd`.
    /// Build the registry. `allow_images` is the startup image
    /// policy (`settings.images` AND the starting model's `vision`
    /// flag) — handed to `read`, the one built-in that produces
    /// attachments.
    pub fn registry(self, cwd: &Path, allow_images: bool) -> ToolRegistry {
        let tools: Vec<Arc<dyn Tool>> = match self {
            Self::None => Vec::new(),
            Self::ReadOnly => vec![
                Arc::new(ReadTool::new(cwd).allow_images(allow_images)),
                Arc::new(GrepTool::new(cwd)),
                Arc::new(FindTool::new(cwd)),
                Arc::new(LsTool::new(cwd)),
            ],
            Self::Coding => vec![
                Arc::new(ReadTool::new(cwd).allow_images(allow_images)),
                Arc::new(BashTool::new(cwd)),
                Arc::new(EditTool::new(cwd)),
                Arc::new(WriteTool::new(cwd)),
            ],
            Self::All => vec![
                Arc::new(ReadTool::new(cwd).allow_images(allow_images)),
                Arc::new(BashTool::new(cwd)),
                Arc::new(EditTool::new(cwd)),
                Arc::new(WriteTool::new(cwd)),
                Arc::new(GrepTool::new(cwd)),
                Arc::new(FindTool::new(cwd)),
                Arc::new(LsTool::new(cwd)),
            ],
        };
        let mut registry = ToolRegistry::new();
        for tool in tools {
            // Duplicate built-in names are a programming error, not a
            // runtime condition.
            registry
                .register(tool)
                .expect("built-in tool names are unique");
        }
        registry
    }
}

/// Parse the raw JSON-arguments text into an object, or explain the failure.
/// Dispatch has already checked JSON validity; this re-checks shape because
/// the tool owns its schema.
pub(crate) fn args_object(arguments: &str) -> Result<serde_json::Value, String> {
    match serde_json::from_str::<serde_json::Value>(arguments) {
        Ok(value @ serde_json::Value::Object(_)) => Ok(value),
        Ok(_) => Err("arguments must be a JSON object".into()),
        Err(e) => Err(format!("invalid JSON arguments: {e}")),
    }
}

pub(crate) fn opt_str(args: &serde_json::Value, key: &str) -> Result<Option<String>, String> {
    match args.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(format!("{key} must be a string, got {other}")),
    }
}

pub(crate) fn opt_usize(args: &serde_json::Value, key: &str) -> Result<Option<usize>, String> {
    match args.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Number(n)) => n
            .as_u64()
            .map(|v| Some(v as usize))
            .ok_or_else(|| format!("{key} must be a non-negative integer")),
        Some(other) => Err(format!("{key} must be a number, got {other}")),
    }
}

pub(crate) fn opt_bool(args: &serde_json::Value, key: &str) -> Result<Option<bool>, String> {
    match args.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Bool(b)) => Ok(Some(*b)),
        Some(other) => Err(format!("{key} must be a boolean, got {other}")),
    }
}

/// Relative paths resolve against the session cwd; absolute paths pass
/// through. Nothing is jailed — that is a documented stance, not an accident.
pub(crate) fn resolve_path(cwd: &Path, raw: &str) -> PathBuf {
    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// Null byte in the first 8 KiB — the classic sniff, good enough here.
pub(crate) fn is_probably_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|&b| b == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_parse_round_trips_and_rejects_unknowns() {
        for name in ["none", "read-only", "coding", "all"] {
            assert_eq!(ToolBundle::parse(name).unwrap().as_str(), name);
        }
        assert!(ToolBundle::parse("everything").is_err());
        assert_eq!(ToolBundle::default(), ToolBundle::Coding);
    }

    #[test]
    fn bundles_build_the_expected_registries() {
        let cwd = std::env::temp_dir();
        let names = |bundle: ToolBundle| -> Vec<String> {
            let reg = bundle.registry(&cwd, false);
            reg.definitions()
                .iter()
                .filter_map(|d| d["function"]["name"].as_str().map(str::to_string))
                .collect()
        };
        assert!(ToolBundle::None.registry(&cwd, false).is_empty());
        assert_eq!(
            names(ToolBundle::ReadOnly),
            vec!["find", "grep", "ls", "read"]
        );
        assert_eq!(
            names(ToolBundle::Coding),
            vec!["bash", "edit", "read", "write"]
        );
        assert_eq!(ToolBundle::All.registry(&cwd, false).len(), 7);
    }

    #[test]
    fn registry_prompt_notes_collect_bullets_in_wire_order() {
        let cwd = std::env::temp_dir();
        let notes = ToolBundle::Coding.registry(&cwd, false).prompt_notes();
        assert!(notes.starts_with("- "), "got: {notes}");
        assert!(notes.contains("Use read to examine files"));
        assert!(notes.contains("complete rewrites"));
        assert_eq!(ToolRegistry::new().prompt_notes(), "");
    }

    #[test]
    fn arg_helpers_explain_bad_shapes() {
        let args = args_object(r#"{"a":"x","n":3,"b":true,"f":1.5,"neg":-2}"#).unwrap();
        assert_eq!(opt_str(&args, "a").unwrap().as_deref(), Some("x"));
        assert_eq!(opt_usize(&args, "n").unwrap(), Some(3));
        assert_eq!(opt_bool(&args, "b").unwrap(), Some(true));
        assert!(opt_str(&args, "n").is_err());
        assert!(opt_usize(&args, "f").is_err());
        assert!(opt_usize(&args, "neg").is_err());
        assert_eq!(opt_str(&args, "missing").unwrap(), None);
        assert!(args_object("[]").unwrap_err().contains("object"));
    }

    #[test]
    fn resolve_path_passes_absolute_through() {
        let cwd = Path::new("/tmp/some-root");
        assert_eq!(resolve_path(cwd, "sub/file"), cwd.join("sub/file"));
        assert_eq!(resolve_path(cwd, "/etc/hosts"), PathBuf::from("/etc/hosts"));
    }
}
