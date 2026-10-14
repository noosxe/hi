//! Directory walking for `grep` and `find`.
//!
//! Built on the [`ignore`] crate (#28) — full gitignore semantics:
//! nested `.gitignore` files at every depth, `!` negation, `dir/`
//! directory-only patterns, and anchored patterns, honored even
//! outside a git repository (`require_git(false)`; session dirs often
//! aren't repos, and the v1 walker behaved the same way). Gitignores
//! above the walk root apply too (`parents(true)`, git semantics — a
//! deliberate v2 change). Deterministic: single-threaded, sorted per
//! directory; `.git` is always skipped; symlinked directories are
//! never descended (cycle safety). `WalkBuilder::build` is the
//! single-threaded walker; `sort_by_file_name` orders each directory's
//! entries, which is what makes the output deterministic.
//!
//! Paths come back **relative to the walk root**, so callers render them
//! directly (`find`) or re-join to read (`grep`).

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use std::path::{Path, PathBuf};

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
/// Gitignores prune before `filter` selects; `filter` may be `None`.
pub fn collect(root: &Path, filter: Option<&GlobSet>, limit: usize) -> Vec<PathBuf> {
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .require_git(false)
        .parents(true)
        .sort_by_file_name(|a, b| a.cmp(b))
        .filter_entry(|entry| entry.file_name() != ".git")
        .build();
    let mut out = Vec::new();
    for entry in walker {
        let Ok(entry) = entry else { continue };
        if entry.depth() == 0 || entry.file_type().is_some_and(|t| t.is_dir()) {
            continue;
        }
        // Symlinked directories are never descended *or listed* (v1
        // cycle safety); symlinked files stay. `file_type` here is
        // symlink-aware (no follow), so a link to a dir reads as
        // symlink; `metadata` follows to answer "is the target a dir".
        let symlinks_to_dir = entry.file_type().is_some_and(|t| t.is_symlink())
            && std::fs::metadata(entry.path()).is_ok_and(|m| m.is_dir());
        if symlinks_to_dir {
            continue;
        }
        if out.len() >= limit {
            break;
        }
        let rel = match entry.path().strip_prefix(root) {
            Ok(rel) => rel,
            Err(_) => continue,
        };
        if filter.is_none_or(|f| f.is_match(rel) || f.is_match(entry.file_name())) {
            out.push(rel.to_path_buf());
        }
    }
    out
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
        let got = collect(&root, None, 100);
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
        let got = collect(&root, None, 100);
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
        let got = collect(&root, None, 100);
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
        let got = collect(&root, None, 100);
        assert_eq!(
            got,
            vec![
                PathBuf::from(".gitignore"),
                PathBuf::from("src/build/staging.rs")
            ]
        );
    }

    /// The #28 gap, closed: a negation un-hides within the same file.
    #[test]
    fn negation_unhides_within_the_same_gitignore() {
        let root = temp_root("negation");
        touch(&root, "noise.log");
        touch(&root, "keep.log");
        std::fs::write(root.join(".gitignore"), "*.log\n!keep.log\n").unwrap();
        let got = collect(&root, None, 100);
        assert_eq!(
            got,
            vec![PathBuf::from(".gitignore"), PathBuf::from("keep.log")]
        );
    }

    /// Nested `.gitignore` files apply at their depth and can override
    /// the root file — the v1 walker never read them.
    #[test]
    fn nested_gitignore_overrides_the_root() {
        let root = temp_root("nested");
        touch(&root, "deep/low.log");
        touch(&root, "deep/keep.txt");
        std::fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        std::fs::write(root.join("deep/.gitignore"), "!low.log\n").unwrap();
        let got = collect(&root, None, 100);
        assert_eq!(
            got,
            vec![
                PathBuf::from(".gitignore"),
                PathBuf::from("deep/.gitignore"),
                PathBuf::from("deep/keep.txt"),
                PathBuf::from("deep/low.log"),
            ]
        );
    }

    /// `build/` matches only directories; a *file* named `build`
    /// survives (v1 trimmed the slash and hid it).
    #[test]
    fn trailing_slash_is_directory_only() {
        let root = temp_root("dironly");
        touch(&root, "build/out.rs");
        touch(&root, "other/build");
        std::fs::write(root.join(".gitignore"), "build/\n").unwrap();
        let got = collect(&root, None, 100);
        assert_eq!(
            got,
            vec![PathBuf::from(".gitignore"), PathBuf::from("other/build")]
        );
    }

    /// v2 behavior change: gitignores above the walk root apply, like
    /// git itself (the v1 walker only ever read the root's own file).
    #[test]
    fn parent_gitignores_apply_above_the_root() {
        let outer = temp_root("parents");
        touch(&outer, "sub/x.log");
        std::fs::write(outer.join(".gitignore"), "*.log\n").unwrap();
        let got = collect(&outer.join("sub"), None, 100);
        assert_eq!(got, Vec::<PathBuf>::new());
    }

    #[test]
    fn filter_matches_rel_path_or_basename() {
        let root = temp_root("filter");
        touch(&root, "src/main.rs");
        touch(&root, "src/lib.rs");
        touch(&root, "notes.md");
        let filter = user_glob("*.rs").unwrap();
        let got = collect(&root, Some(&filter), 100);
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
        let got = collect(&root, None, 3);
        assert_eq!(got.len(), 3);
    }

    #[test]
    fn symlinked_directories_are_not_followed() {
        let root = temp_root("symlink");
        touch(&root, "real/inside.txt");
        std::os::unix::fs::symlink(root.join("real"), root.join("loop")).unwrap();
        let got = collect(&root, None, 100);
        assert_eq!(got, vec![PathBuf::from("real/inside.txt")]);
    }
}
