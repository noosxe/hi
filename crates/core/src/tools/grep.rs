//! `grep` — regex search over file contents.
//!
//! Walks with the shared walker (full gitignore semantics via the
//! `ignore` crate, `.git` skipped), reads
//! text files line by line, and renders rg-style: `path:12: match`,
//! `path-12- context`, `--` between groups. Non-UTF-8 files are skipped
//! silently; match lines are capped at 500 chars and the whole result at
//! the standard contract.

use super::truncate;
use super::walk;
use super::{args_object, opt_bool, opt_str, opt_usize, resolve_path};
use crate::tool::{Tool, ToolAnnotations};
use regex::Regex;
use std::path::{Path, PathBuf};

const DEFAULT_LIMIT: usize = 100;
const MAX_LINE_CHARS: usize = 500;

pub struct GrepTool {
    cwd: PathBuf,
}

impl GrepTool {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self { cwd: cwd.into() }
    }
}

impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents for a regex pattern. Returns matching lines as \
         path:line:text (context lines with dashes), honoring gitignore rules. \
         Clamped to 100 matches (or `limit`), 500 chars per line, and the standard \
         50KB output cap. Use `literal: true` when the pattern contains regex \
         metacharacters."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regular expression to search for"
                },
                "path": {
                    "type": "string",
                    "description": "Directory or file to search (default: current directory)"
                },
                "glob": {
                    "type": "string",
                    "description": "Filter files by glob pattern, e.g. '*.ts'"
                },
                "ignoreCase": {
                    "type": "boolean",
                    "description": "Case-insensitive search (default: false)"
                },
                "literal": {
                    "type": "boolean",
                    "description": "Treat pattern as literal text instead of regex (default: false)"
                },
                "context": {
                    "type": "number",
                    "description": "Lines of context around each match"
                },
                "limit": {
                    "type": "number",
                    "description": "Maximum number of matches (default: 100)"
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
        Some("grep searches contents (regex); find searches filenames (glob).".into())
    }

    fn execute(&self, arguments: &str) -> Result<String, String> {
        let args = args_object(arguments)?;
        let Some(pattern) = opt_str(&args, "pattern")? else {
            return Err("grep needs a pattern".into());
        };
        let search = opt_str(&args, "path")?.unwrap_or_else(|| ".".into());
        let glob = opt_str(&args, "glob")?;
        let ignore_case = opt_bool(&args, "ignoreCase")?.unwrap_or(false);
        let literal = opt_bool(&args, "literal")?.unwrap_or(false);
        let context = opt_usize(&args, "context")?.unwrap_or(0);
        let limit = opt_usize(&args, "limit")?.unwrap_or(DEFAULT_LIMIT);
        if limit == 0 {
            return Err("limit must be at least 1".into());
        }
        let mut source = if literal {
            regex::escape(&pattern)
        } else {
            pattern
        };
        if ignore_case {
            source = format!("(?i){source}");
        }
        let re = Regex::new(&source).map_err(|e| format!("invalid pattern: {e}"))?;
        let filter = match &glob {
            Some(pattern) => Some(walk::user_glob(pattern)?),
            None => None,
        };
        let root = resolve_path(&self.cwd, &search);
        if !root.exists() {
            return Err(format!("path not found: {}", root.display()));
        }

        let mut out: Vec<String> = Vec::new();
        let mut matched_total = 0usize;
        let mut hit_cap = false;
        if root.is_file() {
            let (lines, capped, _) = search_one(&re, &root, &search, context, limit);
            hit_cap = capped;
            out.extend(lines);
        } else {
            for rel in walk::collect(&root, filter.as_ref(), usize::MAX) {
                if matched_total >= limit {
                    hit_cap = true;
                    break;
                }
                let path = root.join(&rel);
                let remaining = limit - matched_total;
                let (lines, capped, count) =
                    search_one(&re, &path, &rel.to_string_lossy(), context, remaining);
                matched_total += count;
                out.extend(lines);
                if capped {
                    hit_cap = true;
                    break;
                }
            }
        }

        if out.is_empty() {
            return Ok("[no matches]".into());
        }
        if hit_cap {
            out.push(format!("[stopped at {limit} matches]"));
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

/// Search one file. Returns the rendered lines, whether the budget was hit
/// with matches still remaining, and how many matches were counted.
fn search_one(
    re: &Regex,
    path: &Path,
    rel: &str,
    context: usize,
    budget: usize,
) -> (Vec<String>, bool, usize) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return (Vec::new(), false, 0);
    };
    let lines: Vec<&str> = content.lines().collect();
    let mut matched: Vec<usize> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if re.is_match(line) {
            matched.push(i);
            if matched.len() >= budget {
                break;
            }
        }
    }
    if matched.is_empty() {
        return (Vec::new(), false, 0);
    }
    let capped = matched.len() >= budget
        && lines[matched[matched.len() - 1] + 1..]
            .iter()
            .any(|line| re.is_match(line));

    // Context ranges around each match, merged when they touch.
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for &m in &matched {
        let start = m.saturating_sub(context);
        let end = (m + context).min(lines.len() - 1);
        match ranges.last_mut() {
            Some(last) if start <= last.1 + 1 => last.1 = end,
            _ => ranges.push((start, end)),
        }
    }

    let mut out = Vec::new();
    for (index, (start, end)) in ranges.iter().enumerate() {
        if index > 0 && context > 0 {
            out.push("--".into());
        }
        for (i, line) in lines[*start..=*end].iter().enumerate() {
            let n = start + i + 1;
            let marker = if matched.contains(&(n - 1)) {
                format!("{rel}:{n}:")
            } else {
                format!("{rel}-{n}-")
            };
            out.push(format!("{marker} {}", clip_line(line)));
        }
    }
    (out, capped, matched.len())
}

fn clip_line(line: &str) -> String {
    if line.chars().count() <= MAX_LINE_CHARS {
        line.to_string()
    } else {
        let mut s: String = line.chars().take(MAX_LINE_CHARS).collect();
        s.push('…');
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hi-grep-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A rendered output line that reports a match, not context/notes:
    /// match lines render `path:12: …`, context lines `path-12- …`.
    fn is_match_line(line: &str) -> bool {
        let Some(colon) = line.find(':') else {
            return false;
        };
        line[colon + 1..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
    }

    fn tool(root: &Path) -> GrepTool {
        GrepTool::new(root)
    }

    #[test]
    fn finds_matches_with_paths_and_line_numbers() {
        let root = temp_root("basic");
        fs::write(root.join("a.txt"), "hello\nworld\nhello again\n").unwrap();
        let out = tool(&root).execute(r#"{"pattern":"hello"}"#).unwrap();
        assert_eq!(out, "a.txt:1: hello\na.txt:3: hello again");
    }

    #[test]
    fn regex_and_flags_work() {
        let root = temp_root("flags");
        fs::write(root.join("f.rs"), "let x = 1;\nLET Y = 2;\n").unwrap();
        let out = tool(&root)
            .execute(r#"{"pattern":"let y","ignoreCase":true}"#)
            .unwrap();
        assert!(out.contains("f.rs:2: LET Y = 2;"), "got: {out}");
        let literal = tool(&root)
            .execute(r#"{"pattern":"x = (1)","literal":true}"#)
            .unwrap();
        assert!(literal.is_empty() || literal.contains("[no matches]"));
        let regex = tool(&root).execute(r#"{"pattern":"x = \\d"}"#).unwrap();
        assert!(regex.contains("f.rs:1: let x = 1;"), "got: {regex}");
    }

    #[test]
    fn invalid_regex_is_an_error_result() {
        let root = temp_root("badre");
        let err = tool(&root)
            .execute(r#"{"pattern":"(unclosed"}"#)
            .unwrap_err();
        assert!(err.contains("invalid pattern"), "got: {err}");
    }

    #[test]
    fn glob_filter_narrows_files() {
        let root = temp_root("glob");
        fs::write(root.join("a.rs"), "target\n").unwrap();
        fs::write(root.join("a.md"), "target\n").unwrap();
        let out = tool(&root)
            .execute(r#"{"pattern":"target","glob":"*.rs"}"#)
            .unwrap();
        assert_eq!(out, "a.rs:1: target");
    }

    #[test]
    fn context_renders_groups_with_separators() {
        let root = temp_root("context");
        fs::write(root.join("f.txt"), "one\ntwo\nthree\nfour\nfive\n").unwrap();
        let out = tool(&root)
            .execute(r#"{"pattern":"three","context":1}"#)
            .unwrap();
        assert_eq!(out, "f.txt-2- two\nf.txt:3: three\nf.txt-4- four");
    }

    #[test]
    fn limit_caps_matches_across_files() {
        let root = temp_root("limit");
        for i in 0..3 {
            fs::write(root.join(format!("f{i}.txt")), "hit\n".repeat(3)).unwrap();
        }
        let out = tool(&root)
            .execute(r#"{"pattern":"hit","limit":4}"#)
            .unwrap();
        assert!(out.contains("[stopped at 4 matches]"), "got: {out}");
        assert_eq!(out.lines().filter(|l| is_match_line(l)).count(), 4);
    }

    #[test]
    fn gitignore_and_binary_files_are_skipped() {
        let root = temp_root("skip");
        fs::write(root.join("keep.txt"), "needle\n").unwrap();
        fs::write(root.join("skip.txt"), "needle\n").unwrap();
        fs::write(root.join(".gitignore"), "skip.txt\n").unwrap();
        fs::write(root.join("blob.bin"), [0x00, 0x01]).unwrap();
        let out = tool(&root).execute(r#"{"pattern":"needle"}"#).unwrap();
        assert_eq!(out, "keep.txt:1: needle");
    }

    #[test]
    fn long_lines_are_clamped_with_an_ellipsis() {
        let root = temp_root("longline");
        fs::write(root.join("f.txt"), "x".repeat(700)).unwrap();
        let out = tool(&root).execute(r#"{"pattern":"x+"}"#).unwrap();
        assert!(out.contains('…'), "got: {out}");
        assert!(out.lines().next().unwrap().chars().count() < 600);
    }
}
