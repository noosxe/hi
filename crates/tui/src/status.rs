//! Status-bar facts: where am I, which repo, which model.
//!
//! Everything here is plain filesystem reading — no `git` subprocess,
//! no dependencies. The branch comes from `.git/HEAD`, found by
//! walking up from the working directory: the same thing
//! `git rev-parse --abbrev-ref HEAD` does, minus the process spawn.

use std::path::{Path, PathBuf};

/// The right side of the status bar: which backend, which model.
/// A plain struct because the `Arc<dyn Provider>` trait object has
/// amnesia — the CLI knows these names, the trait does not carry them.
#[derive(Debug, Clone)]
pub struct Status {
    pub provider: String,
    pub model: String,
    /// How many built-in tools launched with the session (0 = none).
    pub tool_count: usize,
}

impl Status {
    /// The rendered right-hand text, e.g. `(zai) glm-5.3-flash`, with the
    /// tool count when the session launched with a bundle.
    pub fn label(&self) -> String {
        let base = format!("({}) {}", self.provider, self.model);
        if self.tool_count > 0 {
            format!("{base} · {} tools", self.tool_count)
        } else {
            base
        }
    }
}

/// Left side of the status bar: `~/Projects/hi (main)` — the
/// ` (branch)` part only when the directory sits inside a git
/// repository. Computed once at startup; refreshing on mid-session
/// branch switches is polish for later.
pub fn location_label() -> String {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("?"));
    match branch_from(&cwd) {
        Some(branch) => format!("{} ({branch})", home_shortened(&cwd)),
        None => home_shortened(&cwd),
    }
}

/// One row: `left`, filler, `right` — padded to the terminal width.
/// Too-narrow terminals simply clip (the widget truncates); geometry
/// never panics.
pub fn compose_status(left: &str, right: &str, width: u16) -> String {
    let width = width as usize;
    let used = left.chars().count() + right.chars().count();
    let pad = width.saturating_sub(used);
    format!("{left}{}{right}", " ".repeat(pad))
}

/// Replace the home prefix with `~` so the bar stays short.
fn home_shortened(path: &Path) -> String {
    match std::env::home_dir() {
        Some(home) if path.starts_with(&home) => {
            let rest = path.strip_prefix(&home).unwrap_or(path).to_string_lossy();
            if rest.is_empty() {
                "~".into()
            } else {
                format!("~/{rest}")
            }
        }
        _ => path.to_string_lossy().into_owned(),
    }
}

/// Walk up from `start` looking for a `.git` entry, then read the
/// branch out of it. `None` = not inside a git repository.
fn branch_from(start: &Path) -> Option<String> {
    let dot_git = start
        .ancestors()
        .map(|dir| dir.join(".git"))
        .find(|entry| entry.exists())?;
    read_branch(&dot_git)
}

/// `.git` may be a directory (normal repo) or a *file* (worktrees and
/// submodules: `gitdir: <path>`). HEAD holds either
/// `ref: refs/heads/<branch>` or a detached commit sha.
fn read_branch(dot_git: &Path) -> Option<String> {
    let git_dir = if dot_git.is_dir() {
        dot_git.to_path_buf()
    } else {
        let text = std::fs::read_to_string(dot_git).ok()?;
        let target = PathBuf::from(text.strip_prefix("gitdir:")?.trim());
        // The pointer may be absolute, or relative to the directory
        // that holds the `.git` file.
        if target.is_absolute() {
            target
        } else {
            dot_git.parent()?.join(target)
        }
    };
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    head.strip_prefix("ref: refs/heads/")
        .map(str::to_string)
        .or_else(|| Some(head.chars().take(7).collect())) // detached HEAD
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Unique temp dir; tests run in parallel, so names must not clash.
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hi-status-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn branch_comes_from_head_ref() {
        let dir = temp_dir("ref");
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::write(dir.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        assert_eq!(branch_from(&dir).as_deref(), Some("main"));
    }

    #[test]
    fn detached_head_shows_short_sha() {
        let dir = temp_dir("detached");
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::write(
            dir.join(".git/HEAD"),
            "0123456789abcdef0123456789abcdef01234567\n",
        )
        .unwrap();
        assert_eq!(branch_from(&dir).as_deref(), Some("0123456"));
    }

    #[test]
    fn worktree_style_git_file_is_followed() {
        let repo = temp_dir("wt-repo");
        let work = temp_dir("wt-work");
        fs::create_dir_all(repo.join("actual")).unwrap();
        fs::write(
            work.join(".git"),
            format!("gitdir: {}\n", repo.join("actual").display()),
        )
        .unwrap();
        fs::write(repo.join("actual/HEAD"), "ref: refs/heads/feature\n").unwrap();
        assert_eq!(branch_from(&work).as_deref(), Some("feature"));
    }

    #[test]
    fn walks_up_to_find_the_repo_root() {
        let repo = temp_dir("nested");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(repo.join("deep/deeper")).unwrap();
        fs::write(repo.join(".git/HEAD"), "ref: refs/heads/trunk\n").unwrap();
        assert_eq!(
            branch_from(&repo.join("deep/deeper")).as_deref(),
            Some("trunk")
        );
    }

    #[test]
    fn no_git_anywhere_is_none() {
        // Assumes nothing above the temp dir is itself a repo — true
        // for /tmp and /private/tmp on our platforms.
        let dir = temp_dir("plain");
        assert_eq!(branch_from(&dir), None);
    }

    #[test]
    fn status_label_matches_the_pi_style() {
        let status = Status {
            provider: "zai".into(),
            model: "glm-5.3-flash".into(),
            tool_count: 0,
        };
        assert_eq!(status.label(), "(zai) glm-5.3-flash");
    }

    #[test]
    fn status_label_shows_the_tool_count_when_a_bundle_runs() {
        let status = Status {
            provider: "zai".into(),
            model: "glm-5.3-flash".into(),
            tool_count: 7,
        };
        assert_eq!(status.label(), "(zai) glm-5.3-flash · 7 tools");
    }

    #[test]
    fn compose_pads_between_left_and_right() {
        assert_eq!(compose_status("left", "right", 10), "left right");
        assert_eq!(compose_status("left", "right", 9), "leftright");
        // Too narrow: no panic, the widget will clip.
        assert_eq!(compose_status("left", "right", 2), "leftright");
    }
}
