//! `find` — locate files by glob pattern across the tree.

use super::truncate;
use super::walk;
use super::{args_object, opt_str, opt_usize, resolve_path};
use crate::tool::{Tool, ToolAnnotations};
use std::path::PathBuf;

const DEFAULT_LIMIT: usize = 1000;

pub struct FindTool {
    cwd: PathBuf,
}

impl FindTool {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self { cwd: cwd.into() }
    }
}

impl Tool for FindTool {
    fn name(&self) -> &str {
        "find"
    }

    fn description(&self) -> &str {
        "Find files by glob pattern (`*` crosses directories), e.g. '**/*.rs' or \
         'Cargo.toml'. Returns paths relative to the search directory, honoring the \
         gitignore rules. Clamped to 1000 results (or `limit`) and the standard 50KB \
         output cap."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern, e.g. '*.rs' or 'src/**/*.ts'"
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search in (default: current directory)"
                },
                "limit": {
                    "type": "number",
                    "description": "Maximum number of results (default: 1000)"
                }
            },
            "required": ["pattern"]
        })
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }

    fn prompt_note(&self) -> Option<String> {
        Some("find locates files by name (glob); grep searches contents (regex).".into())
    }

    fn execute(&self, arguments: &str) -> Result<String, String> {
        let args = args_object(arguments)?;
        let Some(pattern) = opt_str(&args, "pattern")? else {
            return Err("find needs a pattern".into());
        };
        let search = opt_str(&args, "path")?.unwrap_or_else(|| ".".into());
        let limit = opt_usize(&args, "limit")?.unwrap_or(DEFAULT_LIMIT);
        if limit == 0 {
            return Err("limit must be at least 1".into());
        }
        let filter = walk::user_glob(&pattern)?;
        let root = resolve_path(&self.cwd, &search);
        if !root.is_dir() {
            return Err(format!("not a directory: {}", root.display()));
        }
        // One extra fetch so an exact-limit result isn't mislabeled.
        let mut rels = walk::collect(&root, Some(&filter), limit + 1);
        let hit_cap = rels.len() > limit;
        rels.truncate(limit);

        if rels.is_empty() {
            return Ok("[no matches]".into());
        }
        rels.sort();
        let mut out: Vec<String> = rels
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        if hit_cap {
            out.push(format!("[stopped at {limit} results]"));
        }
        let clip = truncate::head(&out.join("\n"));
        let note = clip.note(false);
        let mut text = clip.text;
        if let Some(note) = note {
            text.push('\n');
            text.push_str(&note);
        }
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hi-find-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch(root: &Path, rel: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "x").unwrap();
    }

    #[test]
    fn finds_by_basename_anywhere() {
        let root = temp_root("basename");
        touch(&root, "src/main.rs");
        touch(&root, "docs/x.rs");
        touch(&root, "README.md");
        let out = FindTool::new(root.clone())
            .execute(r#"{"pattern":"*.rs"}"#)
            .unwrap();
        assert_eq!(out, "docs/x.rs\nsrc/main.rs");
    }

    #[test]
    fn directory_patterns_cross_separators() {
        let root = temp_root("cross");
        touch(&root, "src/deep/main.rs");
        touch(&root, "other/main.rs");
        let out = FindTool::new(root.clone())
            .execute(r#"{"pattern":"src/**/*.rs"}"#)
            .unwrap();
        assert_eq!(out, "src/deep/main.rs");
    }

    #[test]
    fn gitignore_hides_and_limit_notes() {
        let root = temp_root("limit");
        for i in 0..5 {
            touch(&root, &format!("hidden_{i}.log"));
        }
        touch(&root, "keep.txt");
        touch(&root, "a.txt");
        touch(&root, "b.txt");
        touch(&root, "c.txt");
        fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        let none = FindTool::new(root.clone())
            .execute(r#"{"pattern":"*.log"}"#)
            .unwrap();
        assert_eq!(none, "[no matches]");
        let out = FindTool::new(root.clone())
            .execute(r#"{"pattern":"*","limit":2}"#)
            .unwrap();
        assert!(out.contains("[stopped at 2 results]"), "got: {out}");
    }

    #[test]
    fn invalid_globs_error_cleanly() {
        let root = temp_root("badglob");
        let err = FindTool::new(root)
            .execute(r#"{"pattern":"[unclosed"}"#)
            .unwrap_err();
        assert!(err.contains("invalid glob"), "got: {err}");
    }
}
