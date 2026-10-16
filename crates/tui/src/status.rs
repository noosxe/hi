//! Status-bar facts: where am I, which repo, which model.
//!
//! Everything here is plain filesystem reading — no `git` subprocess,
//! no dependencies. The branch comes from `.git/HEAD`, found by
//! walking up from the working directory: the same thing
//! `git rev-parse --abbrev-ref HEAD` does, minus the process spawn.

use std::path::{Path, PathBuf};

use hi_core::event::Usage;

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

/// The session's usage line (AD-0026), the board's top-left corner
/// (row 1's left section) —
/// `↑8.6M ↓1.9M R250M CH99.2% 10.7%/1M`. The counts are
/// session-cumulative (↑ output, ↓ input net of the cache, R
/// reasoning, CH cache-hit share of the prompt); the context share is
/// the *latest* round's prompt against the configured window — a
/// point-in-time fact, not a sum. Empty until the first bill arrives,
/// and every segment hides when its input is absent: no cache detail
/// means no CH and a gross ↓, no reasoning means no R, no configured
/// window means no share.
pub fn usage_line(
    usage: &Usage,
    last_prompt: u64,
    context_window: Option<u64>,
    pricing: Option<&hi_core::catalog::ModelInfo>,
) -> String {
    if usage.prompt_tokens == 0 && usage.completion_tokens == 0 && last_prompt == 0 {
        return String::new();
    }
    let mut parts = Vec::new();
    parts.push(format!("↑{}", compact_tokens(usage.completion_tokens)));
    let net_input = usage.cached_tokens.map_or(usage.prompt_tokens, |cached| {
        usage.prompt_tokens.saturating_sub(cached)
    });
    parts.push(format!("↓{}", compact_tokens(net_input)));
    if let Some(reasoning) = usage.reasoning_tokens.filter(|r| *r > 0) {
        parts.push(format!("R{}", compact_tokens(reasoning)));
    }
    if let (Some(cached), false) = (
        usage.cached_tokens.filter(|c| *c > 0),
        usage.prompt_tokens == 0,
    ) {
        parts.push(format!(
            "CH{:.1}%",
            cached as f64 * 100.0 / usage.prompt_tokens as f64
        ));
    }
    if let (Some(window), false) = (context_window.filter(|w| *w > 0), last_prompt == 0) {
        parts.push(format!(
            "{:.1}%/{}",
            last_prompt as f64 * 100.0 / window as f64,
            compact_tokens(window)
        ));
    }
    // The dollar estimate (AD-0044 ruling 6): net prompt + cached-read +
    // completion against the vendor's rates. Cache *writes* are
    // session-shape-dependent and stay out; absent rates, absent segment.
    let has_tokens = usage.prompt_tokens > 0 || usage.completion_tokens > 0;
    if let Some(info) = pricing
        && has_tokens
        && let (Some(prompt_rate), Some(completion_rate)) = (info.prompt_rate, info.completion_rate)
    {
        let cached = usage.cached_tokens.unwrap_or(0).min(usage.prompt_tokens);
        let cost = usage.prompt_tokens.saturating_sub(cached) as f64 * prompt_rate
            + cached as f64 * info.cache_read_rate.unwrap_or(prompt_rate)
            + usage.completion_tokens as f64 * completion_rate;
        parts.push(format!("~${cost:.2}"));
    }
    parts.join(" ")
}

/// Compact token count: `8.6M`, `250M`, `86.4k`, `999` — one decimal,
/// trailing `.0` trimmed.
fn compact_tokens(tokens: u64) -> String {
    let (value, suffix) = if tokens >= 1_000_000 {
        (tokens as f64 / 1_000_000.0, "M")
    } else if tokens >= 1_000 {
        (tokens as f64 / 1_000.0, "k")
    } else {
        return tokens.to_string();
    };
    let rendered = format!("{value:.1}");
    let rendered = rendered.strip_suffix(".0").unwrap_or(&rendered);
    format!("{rendered}{suffix}")
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

    #[test]
    fn usage_line_is_empty_until_the_first_bill() {
        let usage = Usage::default();
        assert_eq!(usage_line(&usage, 0, None, None), "");
        // A window alone is not a bill either.
        assert_eq!(usage_line(&usage, 0, Some(1_000_000), None), "");
    }

    #[test]
    fn usage_line_renders_the_whole_shape() {
        let usage = Usage {
            prompt_tokens: 9_500_000, // 1.9M net of 7.6M cached
            completion_tokens: 8_600_000,
            cached_tokens: Some(7_600_000),
            reasoning_tokens: Some(250_000_000),
        };
        assert_eq!(
            usage_line(&usage, 107_000, Some(1_000_000), None),
            "↑8.6M ↓1.9M R250M CH80.0% 10.7%/1M"
        );
    }

    #[test]
    fn usage_line_hides_what_the_wire_never_reported() {
        let usage = Usage {
            prompt_tokens: 500,
            completion_tokens: 20,
            cached_tokens: None,
            reasoning_tokens: None,
        };
        // No details: gross ↓, no R, no CH, no share without a window.
        assert_eq!(usage_line(&usage, 500, None, None), "↑20 ↓500");
    }

    #[test]
    fn usage_line_estimates_cost_when_pricing_is_known() {
        let usage = Usage {
            prompt_tokens: 1_000_000,
            completion_tokens: 100_000,
            cached_tokens: Some(800_000),
            reasoning_tokens: None,
        };
        let info = hi_core::catalog::ModelInfo {
            name: None,
            context_window: None,
            max_output: None,
            efforts: None,
            default_effort: None,
            reasoning_mandatory: false,
            // $5/M prompt, $25/M completion, $0.50/M cache read.
            prompt_rate: Some(5e-6),
            completion_rate: Some(2.5e-5),
            cache_read_rate: Some(5e-7),
            cache_write_rate: None,
            vision: None,
            tools: true,
        };
        // Net prompt 200k*$5 + cached 800k*$0.50 + out 100k*$25
        // = $1.00 + $0.40 + $2.50 = $3.90.
        let line = usage_line(&usage, 0, None, Some(&info));
        assert_eq!(line, "↑100k ↓200k CH80.0% ~$3.90");
        // No pricing: no dollar segment (the whole-shape test pins that).
    }
}
