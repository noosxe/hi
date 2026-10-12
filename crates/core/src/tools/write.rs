//! `write` — create or fully overwrite a file.

use super::{args_object, is_probably_binary, opt_str, resolve_path};
use crate::tool::{Tool, ToolAnnotations};
use std::path::PathBuf;

pub struct WriteTool {
    cwd: PathBuf,
}

impl WriteTool {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self { cwd: cwd.into() }
    }
}

impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Write content to a file. Creates the file if it doesn't exist, overwrites it \
         if it does, and creates parent directories as needed. For targeted changes to \
         an existing file, prefer edit."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File to write (relative or absolute)"
                },
                "content": {
                    "type": "string",
                    "description": "Content to write to the file"
                }
            },
            "required": ["path", "content"]
        })
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: false,
            destructive: true,
        }
    }

    fn prompt_note(&self) -> Option<String> {
        Some(
            "Use write only for new files or complete rewrites; prefer edit for targeted changes."
                .into(),
        )
    }

    fn execute(&self, arguments: &str) -> Result<String, String> {
        let args = args_object(arguments)?;
        let Some(path) = opt_str(&args, "path")? else {
            return Err("write needs a path".into());
        };
        let Some(content) = opt_str(&args, "content")? else {
            return Err("write needs content".into());
        };
        if is_probably_binary(content.as_bytes()) {
            return Err("content looks binary; write handles text only".into());
        }
        let path = resolve_path(&self.cwd, &path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        std::fs::write(&path, &content)
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        Ok(format!(
            "Wrote {} bytes to {}",
            content.len(),
            path.display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hi-write-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn creates_new_files_and_parent_directories() {
        let root = temp_root("create");
        let out = WriteTool::new(&root)
            .execute(r#"{"path":"deep/nested/f.txt","content":"hello"}"#)
            .unwrap();
        assert!(out.contains("5 bytes"), "got: {out}");
        assert_eq!(
            fs::read_to_string(root.join("deep/nested/f.txt")).unwrap(),
            "hello"
        );
    }

    #[test]
    fn overwrites_existing_files() {
        let root = temp_root("overwrite");
        fs::write(root.join("f.txt"), "old").unwrap();
        WriteTool::new(&root)
            .execute(r#"{"path":"f.txt","content":"new"}"#)
            .unwrap();
        assert_eq!(fs::read_to_string(root.join("f.txt")).unwrap(), "new");
    }

    #[test]
    fn missing_fields_are_error_results() {
        let root = temp_root("badargs");
        let t = WriteTool::new(&root);
        assert!(
            t.execute(r#"{"content":"x"}"#)
                .unwrap_err()
                .contains("path")
        );
        assert!(
            t.execute(r#"{"path":"f"}"#)
                .unwrap_err()
                .contains("content")
        );
    }
}
