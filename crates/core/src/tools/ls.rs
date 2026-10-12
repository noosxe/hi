//! `ls` — one directory level, sorted, dotfiles included.

use super::truncate;
use super::{args_object, opt_str, opt_usize, resolve_path};
use crate::tool::{Tool, ToolAnnotations};
use std::path::PathBuf;

const DEFAULT_LIMIT: usize = 500;

pub struct LsTool {
    cwd: PathBuf,
}

impl LsTool {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self { cwd: cwd.into() }
    }
}

impl Tool for LsTool {
    fn name(&self) -> &str {
        "ls"
    }

    fn description(&self) -> &str {
        "List directory contents: one entry per line, sorted alphabetically, `/` suffix \
         for directories, dotfiles included. Clamped to 500 entries (or `limit`) and the \
         standard 50KB output cap."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory to list (default: current directory)"
                },
                "limit": {
                    "type": "number",
                    "description": "Maximum number of entries to return"
                }
            }
        })
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }

    fn prompt_note(&self) -> Option<String> {
        Some("ls lists one directory level; find locates files by name across the tree.".into())
    }

    fn execute(&self, arguments: &str) -> Result<String, String> {
        let args = args_object(arguments)?;
        let path = opt_str(&args, "path")?.unwrap_or_else(|| ".".into());
        let limit = opt_usize(&args, "limit")?.unwrap_or(DEFAULT_LIMIT);
        if limit == 0 {
            return Err("limit must be at least 1".into());
        }
        let dir = resolve_path(&self.cwd, &path);
        if !dir.is_dir() {
            return Err(format!("not a directory: {}", dir.display()));
        }
        let mut entries: Vec<String> = std::fs::read_dir(&dir)
            .map_err(|e| format!("cannot list {}: {e}", dir.display()))?
            .filter_map(Result::ok)
            .map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                if dir.join(&name).is_dir() {
                    format!("{name}/")
                } else {
                    name
                }
            })
            .collect();
        entries.sort_by_key(|a| a.to_lowercase());

        let total = entries.len();
        let clipped: Vec<String> = entries.into_iter().take(limit).collect();
        let clip = truncate::head_with(
            &clipped.join("\n"),
            truncate::MAX_LINES,
            truncate::MAX_BYTES,
        );
        let note =
            (total > limit).then(|| format!("[truncated: showing first {limit} of {total} lines]"));
        let mut text = clip.text;
        if let Some(note) = note {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&note);
        } else if total == 0 {
            text.push_str("[empty directory]");
        }
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hi-ls-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn lists_sorted_with_dir_suffixes_and_dotfiles() {
        let root = temp_root("sorted");
        fs::create_dir(root.join("zeta")).unwrap();
        fs::write(root.join("apple.txt"), "x").unwrap();
        fs::write(root.join(".hidden"), "x").unwrap();
        let out = LsTool::new(root.clone()).execute("{}").unwrap();
        assert_eq!(out, ".hidden\napple.txt\nzeta/");
    }

    #[test]
    fn limit_clamps_and_notes() {
        let root = temp_root("limit");
        for i in 0..5 {
            fs::write(root.join(format!("f{i}")), "x").unwrap();
        }
        let out = LsTool::new(root.clone()).execute(r#"{"limit":2}"#).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3); // 2 entries + note
        assert!(out.contains("[truncated: showing first 2 of 5 lines]"));
    }

    #[test]
    fn non_directories_are_refused() {
        let root = temp_root("notdir");
        fs::write(root.join("file.txt"), "x").unwrap();
        let err = LsTool::new(root.clone())
            .execute(r#"{"path":"file.txt"}"#)
            .unwrap_err();
        assert!(err.contains("not a directory"), "got: {err}");
    }
}
