//! The system prompt: pi-shaped resolution chain.
//!
//!   1. built-in default (one sentence — hi makes no tool promises)
//!   2. `SYSTEM.md` in the user config dir replaces it
//!   3. `SYSTEM.md` in the project's `.hi/` directory replaces that
//!   4. `APPEND_SYSTEM.md` at both levels appends (user first, then
//!      project), blank-line separated
//!
//! The result travels as one leading `role: "system"` message.

use std::path::{Path, PathBuf};

pub const DEFAULT_SYSTEM_PROMPT: &str = "You are hi, a concise terminal assistant.";

/// Resolve the system prompt for the current session: user files
/// come from the config directory, project files from `.hi/` in the
/// working directory.
pub fn resolve() -> String {
    let user_dir = crate::config::config_dir();
    let project_dir = std::env::current_dir().ok();
    resolve_from(user_dir.as_deref(), project_dir.as_deref())
}

/// The path-independent decision, so tests can use temp dirs. Project
/// files live under the project's `.hi/` directory.
pub fn resolve_from(user_dir: Option<&Path>, project_dir: Option<&Path>) -> String {
    let project_hi = project_dir.map(|dir| dir.join(".hi"));
    let mut parts: Vec<String> = Vec::new();

    let mut base = DEFAULT_SYSTEM_PROMPT.to_string();
    if let Some(contents) = read_markdown(user_dir, "SYSTEM.md") {
        base = contents;
    }
    // The project override wins over the user-level one: the closer
    // the file is to the work, the more it knows.
    if let Some(contents) = read_markdown(project_hi.as_deref(), "SYSTEM.md") {
        base = contents;
    }
    parts.push(base);

    if let Some(appendix) = read_markdown(user_dir, "APPEND_SYSTEM.md") {
        parts.push(appendix);
    }
    if let Some(appendix) = read_markdown(project_hi.as_deref(), "APPEND_SYSTEM.md") {
        parts.push(appendix);
    }

    parts.join("\n\n")
}

/// Read `<dir>/<name>` when the directory and file exist, trimmed.
/// Empty (whitespace-only) files read as absent.
fn read_markdown(dir: Option<&Path>, name: &str) -> Option<String> {
    let path: PathBuf = dir?.join(name);
    std::fs::read_to_string(path)
        .ok()
        .map(|contents| contents.trim().to_string())
        .filter(|contents| !contents.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hi-sysprompt-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn default_when_no_files_exist() {
        assert_eq!(resolve_from(None, None), DEFAULT_SYSTEM_PROMPT);
    }

    #[test]
    fn user_system_md_replaces_the_default() {
        let user = temp_dir("user");
        fs::write(user.join("SYSTEM.md"), "custom persona\n").unwrap();
        assert_eq!(resolve_from(Some(&user), None), "custom persona");
    }

    #[test]
    fn project_system_md_wins_over_user() {
        let user = temp_dir("u-sys");
        fs::write(user.join("SYSTEM.md"), "user rules").unwrap();
        let project = temp_dir("p-sys");
        fs::create_dir_all(project.join(".hi")).unwrap();
        fs::write(project.join(".hi/SYSTEM.md"), "project rules").unwrap();
        assert_eq!(resolve_from(Some(&user), Some(&project)), "project rules");
    }

    #[test]
    fn append_files_join_after_the_base_in_order() {
        let user = temp_dir("u-app");
        fs::write(user.join("APPEND_SYSTEM.md"), "house style").unwrap();
        let project = temp_dir("p-app");
        fs::create_dir_all(project.join(".hi")).unwrap();
        fs::write(project.join(".hi/APPEND_SYSTEM.md"), "project style").unwrap();
        assert_eq!(
            resolve_from(Some(&user), Some(&project)),
            format!("{DEFAULT_SYSTEM_PROMPT}\n\nhouse style\n\nproject style")
        );
    }

    #[test]
    fn append_works_alongside_replacement() {
        let user = temp_dir("u-mix");
        fs::write(user.join("SYSTEM.md"), "user rules").unwrap();
        let project = temp_dir("p-mix");
        fs::create_dir_all(project.join(".hi")).unwrap();
        fs::write(project.join(".hi/APPEND_SYSTEM.md"), "project style").unwrap();
        assert_eq!(
            resolve_from(Some(&user), Some(&project)),
            "user rules\n\nproject style"
        );
    }

    #[test]
    fn whitespace_only_files_count_as_absent() {
        let user = temp_dir("u-blank");
        fs::write(user.join("SYSTEM.md"), "   \n").unwrap();
        assert_eq!(resolve_from(Some(&user), None), DEFAULT_SYSTEM_PROMPT);
    }
}
