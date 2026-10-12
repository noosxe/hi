//! `edit` — exact-match text replacement against the original file.
//!
//! The rule that keeps models honest (pi's, and this harness's own hashline
//! discipline): every edit's `oldText` is matched against the **original**
//! file — never against the result of an earlier edit in the same call — and
//! must match exactly once. Overlapping edits are rejected up front; the
//! spans are then applied last-to-first so earlier replacements can't
//! invalidate later offsets.

use super::{args_object, is_probably_binary, opt_str, resolve_path};
use crate::tool::{Tool, ToolAnnotations};
use std::path::PathBuf;

pub struct EditTool {
    cwd: PathBuf,
}

impl EditTool {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self { cwd: cwd.into() }
    }
}

impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Edit a file with exact text replacement. Each element of `edits` carries \
         `oldText` and `newText`; `oldText` must match the ORIGINAL file exactly once \
         (0 or 2+ matches is an error) and edits must not overlap. Match against the \
         original, not incrementally: if two changes touch nearby text, merge them \
         into one edit. Include enough surrounding text to make `oldText` unique."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File to edit (relative or absolute)"
                },
                "edits": {
                    "type": "array",
                    "description": "Replacements, each matched against the original file",
                    "items": {
                        "type": "object",
                        "properties": {
                            "oldText": { "type": "string", "description": "Exact text to replace" },
                            "newText": { "type": "string", "description": "Replacement text" }
                        },
                        "required": ["oldText", "newText"]
                    }
                }
            },
            "required": ["path", "edits"]
        })
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: false,
            destructive: true,
        }
    }

    fn prompt_note(&self) -> Option<String> {
        Some("edit's oldText must match the original file exactly once — include surrounding text for uniqueness.".into())
    }

    fn execute(&self, arguments: &str) -> Result<String, String> {
        let args = args_object(arguments)?;
        let Some(path) = opt_str(&args, "path")? else {
            return Err("edit needs a path".into());
        };
        let Some(edits) = args.get("edits").and_then(|e| e.as_array()) else {
            return Err("edit needs an edits array".into());
        };
        if edits.is_empty() {
            return Err("edits must contain at least one replacement".into());
        }

        let path = resolve_path(&self.cwd, &path);
        let bytes =
            std::fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        if is_probably_binary(&bytes) {
            return Err(format!(
                "{} looks like a binary file; edit handles text only",
                path.display()
            ));
        }
        let mut content = String::from_utf8(bytes)
            .map_err(|_| format!("{} is not valid UTF-8", path.display()))?;
        // The model will never include an invisible BOM in oldText; match on
        // the stripped content and keep the file BOM-free afterwards.
        if content.starts_with('\u{feff}') {
            content.remove(0);
        }

        // Validate everything against the original before touching anything.
        let mut spans: Vec<(usize, usize, &str)> = Vec::new();
        for (i, edit) in edits.iter().enumerate() {
            let Some(old) = edit.get("oldText").and_then(|v| v.as_str()) else {
                return Err(format!("edits[{i}].oldText must be a string"));
            };
            let Some(new) = edit.get("newText").and_then(|v| v.as_str()) else {
                return Err(format!("edits[{i}].newText must be a string"));
            };
            if old.is_empty() {
                return Err(format!("edits[{i}].oldText must not be empty"));
            }
            let mut found = content.match_indices(old).map(|(at, _)| at);
            let Some(start) = found.next() else {
                return Err(format!(
                    "edits[{i}].oldText not found in {}",
                    path.display()
                ));
            };
            if found.next().is_some() {
                return Err(format!(
                    "edits[{i}].oldText matches more than once — include surrounding text to make it unique"
                ));
            }
            spans.push((start, start + old.len(), new));
        }
        spans.sort_by_key(|(start, _, _)| *start);
        for pair in spans.windows(2) {
            if pair[1].0 < pair[0].1 {
                return Err("edits overlap — merge them into one edit".into());
            }
        }

        for (start, end, new) in spans.iter().rev() {
            content.replace_range(start..end, new);
        }
        std::fs::write(&path, &content)
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        Ok(format!(
            "Applied {} edit(s) to {}",
            spans.len(),
            path.display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hi-edit-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn setup(tag: &str, content: &str) -> (PathBuf, EditTool) {
        let root = temp_root(tag);
        fs::write(root.join("f.txt"), content).unwrap();
        let tool = EditTool::new(root.clone());
        (root, tool)
    }

    #[test]
    fn replaces_a_unique_match() {
        let (root, tool) = setup("unique", "hello world\n");
        tool.execute(r#"{"path":"f.txt","edits":[{"oldText":"world","newText":"there"}]}"#)
            .unwrap();
        assert_eq!(
            fs::read_to_string(root.join("f.txt")).unwrap(),
            "hello there\n"
        );
    }

    #[test]
    fn zero_and_multiple_matches_are_errors() {
        let (_, tool) = setup("nomatch", "hello\n");
        let zero = tool
            .execute(r#"{"path":"f.txt","edits":[{"oldText":"zzz","newText":"x"}]}"#)
            .unwrap_err();
        assert!(zero.contains("not found"), "got: {zero}");
        let (_, tool) = setup("multi", "ab ab\n");
        let many = tool
            .execute(r#"{"path":"f.txt","edits":[{"oldText":"ab","newText":"x"}]}"#)
            .unwrap_err();
        assert!(many.contains("more than once"), "got: {many}");
    }

    #[test]
    fn edits_match_against_the_original_not_incrementally() {
        let (root, tool) = setup("original", "a b c\n");
        // Swapping a↔b only works if both match the original.
        tool.execute(
            r#"{"path":"f.txt","edits":[{"oldText":"a b","newText":"b a"},{"oldText":" c","newText":" c!"}]}"#,
        )
        .unwrap();
        assert_eq!(fs::read_to_string(root.join("f.txt")).unwrap(), "b a c!\n");
    }

    #[test]
    fn overlapping_edits_are_rejected() {
        let (_, tool) = setup("overlap", "abcdef\n");
        let err = tool.execute(
            r#"{"path":"f.txt","edits":[{"oldText":"abc","newText":"x"},{"oldText":"cde","newText":"y"}]}"#,
        ).unwrap_err();
        assert!(err.contains("overlap"), "got: {err}");
    }

    #[test]
    fn empty_and_malformed_edits_are_errors() {
        let (_, tool) = setup("malformed", "hello\n");
        assert!(
            tool.execute(r#"{"path":"f.txt","edits":[]}"#)
                .unwrap_err()
                .contains("at least one")
        );
        assert!(
            tool.execute(r#"{"path":"f.txt","edits":[{"oldText":"","newText":"x"}]}"#)
                .unwrap_err()
                .contains("must not be empty")
        );
        assert!(
            tool.execute(r#"{"path":"f.txt"}"#)
                .unwrap_err()
                .contains("edits array")
        );
    }

    #[test]
    fn bom_is_stripped_before_matching() {
        let (root, tool) = setup("bom", "\u{feff}hello\n");
        tool.execute(r#"{"path":"f.txt","edits":[{"oldText":"hello","newText":"hi"}]}"#)
            .unwrap();
        assert_eq!(fs::read_to_string(root.join("f.txt")).unwrap(), "hi\n");
    }
}
