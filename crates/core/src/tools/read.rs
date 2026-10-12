//! `read` — file contents, line-numbered, paged with `offset`/`limit`.

use super::truncate;
use super::{args_object, is_probably_binary, opt_str, opt_usize, resolve_path};
use crate::tool::{Tool, ToolAnnotations};
use std::path::PathBuf;

pub struct ReadTool {
    cwd: PathBuf,
}

impl ReadTool {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self { cwd: cwd.into() }
    }
}

impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read the contents of a text file. Output is line-numbered and clamped to 2000 \
         lines or 50KB, whichever hits first; page through larger files with offset \
         (1-indexed) and limit. Binary files are refused. When you need more of the \
         file, call again with a later offset."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File to read (relative or absolute)"
                },
                "offset": {
                    "type": "number",
                    "description": "Line number to start from (1-indexed)"
                },
                "limit": {
                    "type": "number",
                    "description": "Maximum number of lines to return"
                }
            },
            "required": ["path"]
        })
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }

    fn prompt_note(&self) -> Option<String> {
        Some("Use read to examine files instead of cat or sed.".into())
    }

    fn execute(&self, arguments: &str) -> Result<String, String> {
        let args = args_object(arguments)?;
        let Some(path) = opt_str(&args, "path")? else {
            return Err("read needs a path".into());
        };
        let offset = opt_usize(&args, "offset")?.unwrap_or(1);
        let limit = opt_usize(&args, "limit")?.unwrap_or(truncate::MAX_LINES);
        if offset == 0 {
            return Err("offset is 1-indexed; it must be at least 1".into());
        }
        if limit == 0 {
            return Err("limit must be at least 1".into());
        }
        let path = resolve_path(&self.cwd, &path);
        let bytes =
            std::fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        if is_probably_binary(&bytes) {
            return Err(format!(
                "{} looks like a binary file; read handles text only",
                path.display()
            ));
        }
        let content = String::from_utf8(bytes)
            .map_err(|_| format!("{} is not valid UTF-8", path.display()))?;

        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();
        if total == 0 {
            return Ok("[empty file]".into());
        }
        if offset > total {
            return Err(format!(
                "offset {offset} is past the end: the file has {total} lines"
            ));
        }
        let selected: Vec<String> = lines[offset - 1..]
            .iter()
            .take(limit)
            .enumerate()
            .map(|(i, line)| format!("{:>5}│{}", offset + i, line))
            .collect();
        let selected_count = selected.len();

        let clip = truncate::head_with(
            &selected.join("\n"),
            truncate::MAX_LINES,
            truncate::MAX_BYTES,
        );
        let mut text = clip.text;
        if clip.truncated {
            text.push_str(&format!(
                "\n[clamped at 50KB — call read again with offset {}]",
                offset + clip.kept_lines
            ));
        }
        let end = offset + selected_count - 1;
        if offset > 1 || end < total {
            text.push_str(&format!("\n[lines {offset}–{end} of {total}]"));
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
        let dir = std::env::temp_dir().join(format!("hi-read-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn tool(root: &Path) -> ReadTool {
        ReadTool::new(root)
    }

    #[test]
    fn reads_numbered_lines() {
        let root = temp_root("numbered");
        fs::write(root.join("f.txt"), "alpha\nbeta\n").unwrap();
        let out = tool(&root).execute(r#"{"path":"f.txt"}"#).unwrap();
        assert_eq!(out, "    1│alpha\n    2│beta");
    }

    #[test]
    fn offset_is_one_indexed_and_pages() {
        let root = temp_root("offset");
        fs::write(root.join("f.txt"), "1\n2\n3\n4\n5").unwrap();
        let out = tool(&root)
            .execute(r#"{"path":"f.txt","offset":3}"#)
            .unwrap();
        assert!(out.contains("    3│3"));
        assert!(out.contains("[lines 3–5 of 5]"));
        let out = tool(&root)
            .execute(r#"{"path":"f.txt","offset":2,"limit":2}"#)
            .unwrap();
        assert!(out.contains("[lines 2–3 of 5]"));
    }

    #[test]
    fn rejects_bad_arguments() {
        let root = temp_root("badargs");
        let t = tool(&root);
        std::fs::write(root.join("f.txt"), "1\n2\n3\n4\n5").unwrap();
        assert!(t.execute(r#"{}"#).unwrap_err().contains("path"));
        assert!(
            t.execute(r#"{"path":"f.txt","offset":0}"#)
                .unwrap_err()
                .contains("1-indexed")
        );
        assert!(
            t.execute(r#"{"path":"f.txt","limit":0}"#)
                .unwrap_err()
                .contains("at least 1")
        );
        assert!(
            t.execute(r#"{"path":"f.txt","offset":9}"#)
                .unwrap_err()
                .contains("past the end")
        );
        assert!(t.execute("not json").unwrap_err().contains("invalid JSON"));
    }

    #[test]
    fn empty_file_reports_itself() {
        let root = temp_root("empty");
        fs::write(root.join("f.txt"), "").unwrap();
        assert_eq!(
            tool(&root).execute(r#"{"path":"f.txt"}"#).unwrap(),
            "[empty file]"
        );
    }

    #[test]
    fn binary_files_are_refused() {
        let root = temp_root("binary");
        fs::write(root.join("f.bin"), [0x68, 0x69, 0x00, 0x62]).unwrap();
        let err = tool(&root).execute(r#"{"path":"f.bin"}"#).unwrap_err();
        assert!(err.contains("binary"), "got: {err}");
    }

    #[test]
    fn invalid_utf8_is_refused_cleanly() {
        let root = temp_root("utf8");
        fs::write(root.join("f.txt"), [0xff, 0xfe]).unwrap();
        let err = tool(&root).execute(r#"{"path":"f.txt"}"#).unwrap_err();
        assert!(err.contains("UTF-8"), "got: {err}");
    }

    #[test]
    fn missing_files_error_with_the_path() {
        let root = temp_root("missing");
        let err = tool(&root).execute(r#"{"path":"nope.txt"}"#).unwrap_err();
        assert!(err.contains("cannot read"), "got: {err}");
    }
}
