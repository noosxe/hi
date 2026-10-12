//! Directory walking for `grep` and `find`.
//!
//! Recursive, deterministic (sorted per directory), `.git` always skipped,
//! and the **root-level** `.gitignore` honored. That last part is a
//! deliberately documented subset (ruling on #26): simple glob patterns,
//! no `!` negation, no nested `.gitignore` files. Full semantics would
//! mean the `ignore` crate — issue #28 holds that decision.
//!
//! Paths come back **relative to the walk root**, so callers render them
//! directly (`find`) or re-join to read (`grep`).

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use std::fs;
use std::path::{Path, PathBuf};

/// Parse the root `.gitignore` into a matcher, or `None` when there is
/// nothing to honor. Malformed or unsupported lines (`!`, comments, blanks)
/// are skipped, not fatal.
pub fn root_gitignore(root: &Path) -> Option<GlobSet> {
    let text = fs::read_to_string(root.join(".gitignore")).ok()?;
    let mut builder = GlobSetBuilder::new();
    let mut any = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
            continue;
        }
        let pattern = line.trim_end_matches('/');
        // Patterns with an interior slash are anchored to the root and match
        // the relative path; bare names match a path component anywhere.
        let built = if pattern.contains('/') {
            GlobBuilder::new(pattern).literal_separator(true).build()
        } else {
            GlobBuilder::new(pattern).build()
        };
        if let Ok(glob) = built {
            builder.add(glob);
            any = true;
        }
    }
    if any { builder.build().ok() } else { None }
}

/// A user-supplied `glob` filter (`grep`'s `glob`, `find`'s `pattern`).
/// `*` is allowed to cross separators, matching rg's `--glob` feel.
pub fn user_glob(pattern: &str) -> Result<GlobSet, String> {
    let glob = GlobBuilder::new(pattern)
        .build()
        .map_err(|e| format!("invalid glob pattern {pattern:?}: {e}"))?;
    GlobSetBuilder::new()
        .add(glob)
        .build()
        .map_err(|e| format!("invalid glob pattern {pattern:?}: {e}"))
}

/// Collect file paths (relative to `root`), up to `limit` of them.
/// `ignore` prunes paths before `filter` selects; both may be `None`.
pub fn collect(
    root: &Path,
    ignore: Option<&GlobSet>,
    filter: Option<&GlobSet>,
    limit: usize,
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk_dir(root, Path::new(""), ignore, filter, limit, &mut out);
    out
}

fn walk_dir(
    root: &Path,
    rel: &Path,
    ignore: Option<&GlobSet>,
    filter: Option<&GlobSet>,
    limit: usize,
    out: &mut Vec<PathBuf>,
) {
    if out.len() >= limit {
        return;
    }
    let Ok(entries) = fs::read_dir(root.join(rel)) else {
        return;
    };
    let mut entries: Vec<std::fs::DirEntry> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        if out.len() >= limit {
            return;
        }
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str == ".git" {
            continue;
        }
        let child_rel = rel.join(&name);
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        // Never descend into symlinked directories (cycle safety).
        if file_type.is_symlink()
            && fs::metadata(root.join(&child_rel))
                .map(|m| m.is_dir())
                .unwrap_or(false)
        {
            continue;
        }
        if let Some(set) = ignore
            && (set.is_match(&child_rel) || set.is_match(&*name_str))
        {
            continue;
        }
        if file_type.is_dir() {
            walk_dir(root, &child_rel, ignore, filter, limit, out);
        } else if filter.is_none_or(|f| f.is_match(&child_rel) || f.is_match(&*name_str)) {
            out.push(child_rel);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(root: &Path, rel: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "x").unwrap();
    }

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hi-walk-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn walks_recursively_sorted_and_relative() {
        let root = temp_root("basic");
        touch(&root, "b.txt");
        touch(&root, "a.txt");
        touch(&root, "sub/z.txt");
        touch(&root, "sub/y.txt");
        let got = collect(&root, None, None, 100);
        assert_eq!(
            got,
            vec![
                PathBuf::from("a.txt"),
                PathBuf::from("b.txt"),
                PathBuf::from("sub/y.txt"),
                PathBuf::from("sub/z.txt"),
            ]
        );
    }

    #[test]
    fn skips_git_and_prunes_ignored_dirs() {
        let root = temp_root("ignore");
        touch(&root, "keep.rs");
        touch(&root, "target/debug/junk.rs");
        touch(&root, ".git/objects/ab");
        std::fs::write(
            root.join(".gitignore"),
            "target/\n# comment\n!not-a-negation\n",
        )
        .unwrap();
        let ignore = root_gitignore(&root);
        let got = collect(&root, ignore.as_ref(), None, 100);
        assert_eq!(
            got,
            vec![PathBuf::from(".gitignore"), PathBuf::from("keep.rs")]
        );
    }

    #[test]
    fn bare_gitignore_names_match_at_any_depth() {
        let root = temp_root("bare");
        touch(&root, "top.log");
        touch(&root, "deep/nested/low.log");
        touch(&root, "deep/keep.txt");
        std::fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        let ignore = root_gitignore(&root);
        let got = collect(&root, ignore.as_ref(), None, 100);
        assert_eq!(
            got,
            vec![PathBuf::from(".gitignore"), PathBuf::from("deep/keep.txt")]
        );
    }

    #[test]
    fn anchored_gitignore_patterns_stay_at_the_root() {
        let root = temp_root("anchored");
        touch(&root, "build/staging.rs");
        touch(&root, "src/build/staging.rs");
        std::fs::write(root.join(".gitignore"), "build/*.rs\n").unwrap();
        let ignore = root_gitignore(&root);
        let got = collect(&root, ignore.as_ref(), None, 100);
        assert_eq!(
            got,
            vec![
                PathBuf::from(".gitignore"),
                PathBuf::from("src/build/staging.rs")
            ]
        );
    }

    #[test]
    fn filter_matches_rel_path_or_basename() {
        let root = temp_root("filter");
        touch(&root, "src/main.rs");
        touch(&root, "src/lib.rs");
        touch(&root, "notes.md");
        let filter = user_glob("*.rs").unwrap();
        let got = collect(&root, None, Some(&filter), 100);
        assert_eq!(
            got,
            vec![PathBuf::from("src/lib.rs"), PathBuf::from("src/main.rs")]
        );
    }

    #[test]
    fn limit_stops_the_walk_early() {
        let root = temp_root("limit");
        for i in 0..10 {
            touch(&root, &format!("f{i}.txt"));
        }
        let got = collect(&root, None, None, 3);
        assert_eq!(got.len(), 3);
    }

    #[test]
    fn symlinked_directories_are_not_followed() {
        let root = temp_root("symlink");
        touch(&root, "real/inside.txt");
        std::os::unix::fs::symlink(root.join("real"), root.join("loop")).unwrap();
        let got = collect(&root, None, None, 100);
        assert_eq!(got, vec![PathBuf::from("real/inside.txt")]);
    }
}
