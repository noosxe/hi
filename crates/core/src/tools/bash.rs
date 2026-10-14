//! `bash` — run a shell command in the session's working directory.
//!
//! stdout and stderr are captured separately, rendered with labels, and
//! tail-clamped (a failing command's diagnosis lives at the bottom). A
//! non-zero exit is an **error result with the output inside it** — the
//! model must see what went wrong to adapt. The optional `timeout` is
//! seconds, with no default (pi-parity); on expiry the child is killed.
//! Known v1 limitation (issue #30): grandchildren survive — there is no
//! process-group kill.

use super::truncate;
use super::{args_object, opt_str, opt_usize};
use crate::tool::{Tool, ToolAnnotations};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_millis(50);

pub struct BashTool {
    cwd: PathBuf,
}

impl BashTool {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self { cwd: cwd.into() }
    }
}

impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Execute a bash command in the current working directory. Returns stdout and \
         stderr (labeled), tail-truncated to 2000 lines / 50KB. A non-zero exit comes \
         back as an error result with the output included. Optionally provide a \
         timeout in seconds (no default — long-running commands run until finished)."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The bash command to run"
                },
                "timeout": {
                    "type": "number",
                    "description": "Timeout in seconds (optional, no default timeout)"
                }
            },
            "required": ["command"]
        })
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: false,
            destructive: true,
        }
    }

    fn prompt_note(&self) -> Option<String> {
        Some("bash runs with the session's permissions — prefer read/grep/find for inspection and reserve commands for what they can't do.".into())
    }

    fn execute(&self, arguments: &str) -> Result<String, String> {
        let args = args_object(arguments)?;
        let Some(command) = opt_str(&args, "command")? else {
            return Err("bash needs a command".into());
        };
        let timeout = opt_usize(&args, "timeout")?;

        let mut cmd = Command::new("bash");
        cmd.arg("-c")
            .arg(&command)
            .current_dir(&self.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Own process group (#30): the timeout kill takes the whole
        // tree — the task manager's rule, shared plumbing.
        crate::process::own_group(&mut cmd);
        let mut child = cmd.spawn().map_err(|e| format!("cannot spawn bash: {e}"))?;
        let Some(mut out_pipe) = child.stdout.take() else {
            return Err("bash produced no stdout pipe".into());
        };
        let Some(mut err_pipe) = child.stderr.take() else {
            return Err("bash produced no stderr pipe".into());
        };

        let (out, err, status, timed_out) = std::thread::scope(|scope| {
            let out_handle = scope.spawn(move || read_lossy(&mut out_pipe));
            let err_handle = scope.spawn(move || read_lossy(&mut err_pipe));
            let deadline = timeout.map(|secs| Instant::now() + Duration::from_secs(secs as u64));
            let mut timed_out = false;
            let status: Option<ExitStatus> = loop {
                match child.try_wait() {
                    Ok(Some(status)) => break Some(status),
                    Ok(None) => {}
                    Err(_) => break None,
                }
                if let Some(deadline) = deadline
                    && Instant::now() >= deadline
                {
                    crate::process::stop_tree(&mut child);
                    timed_out = true;
                    break None;
                }
                std::thread::sleep(POLL_INTERVAL);
            };
            let out = out_handle.join().unwrap_or_default();
            let err = err_handle.join().unwrap_or_default();
            (out, err, status, timed_out)
        });

        let mut text = String::new();
        if !out.is_empty() {
            text.push_str("--- stdout ---\n");
            text.push_str(out.trim_end());
        }
        if !err.is_empty() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str("--- stderr ---\n");
            text.push_str(err.trim_end());
        }
        let clip = truncate::tail(&text);
        let note = clip.note(true);
        let mut text = clip.text;
        if let Some(note) = note {
            text.push('\n');
            text.push_str(&note);
        }

        if timed_out {
            return Err(format!("timeout after {}s\n{text}", timeout.unwrap_or(0)));
        }
        let Some(status) = status else {
            return Err(format!("bash failed to run\n{text}"));
        };
        match status.code() {
            Some(0) => {
                if text.is_empty() {
                    Ok("(no output)".into())
                } else {
                    Ok(text)
                }
            }
            Some(code) => Err(format!("exit code {code}\n{text}")),
            None => Err(format!("terminated by signal\n{text}")),
        }
    }
}

fn read_lossy<R: Read>(reader: &mut R) -> String {
    let mut buf = Vec::new();
    let _ = reader.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hi-bash-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn tool(root: &Path) -> BashTool {
        BashTool::new(root)
    }

    #[test]
    fn runs_in_the_session_cwd() {
        let root = temp_root("cwd");
        fs::write(root.join("marker.txt"), "x").unwrap();
        let out = tool(&root).execute(r#"{"command":"ls"}"#).unwrap();
        assert!(out.contains("marker.txt"), "got: {out}");
    }

    #[test]
    fn captures_stdout_and_stderr_labeled() {
        let root = temp_root("pipes");
        let out = tool(&root)
            .execute(r#"{"command":"echo out-line; echo err-line 1>&2"}"#)
            .unwrap();
        assert!(out.contains("--- stdout ---\nout-line"), "got: {out}");
        assert!(out.contains("--- stderr ---\nerr-line"), "got: {out}");
    }

    #[test]
    fn non_zero_exit_is_an_error_with_output_inside() {
        let root = temp_root("exitcode");
        let err = tool(&root)
            .execute(r#"{"command":"echo before-fail; exit 3"}"#)
            .unwrap_err();
        assert!(err.starts_with("exit code 3"), "got: {err}");
        assert!(err.contains("before-fail"), "got: {err}");
    }

    #[test]
    fn timeout_kills_the_child() {
        let root = temp_root("timeout");
        let started = std::time::Instant::now();
        let err = tool(&root)
            .execute(r#"{"command":"sleep 30","timeout":1}"#)
            .unwrap_err();
        assert!(err.starts_with("timeout after 1s"), "got: {err}");
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }

    #[test]
    #[cfg(unix)]
    fn timeout_kills_the_whole_tree_not_just_the_child() {
        let root = temp_root("tree");
        let pid_file = root.join("gc.pid");
        // The direct child is the tool's `bash`; the quoted `sh` is
        // its grandchild and announces its own pid before sleeping.
        let cmd = format!(
            r#"{{"command":"sh -c 'echo $$ > {}; sleep 30' & sleep 30","timeout":1}}"#,
            pid_file.display()
        );
        let started = std::time::Instant::now();
        let err = tool(&root).execute(&cmd).unwrap_err();
        assert!(err.starts_with("timeout after 1s"), "got: {err}");
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .expect("grandchild never started")
            .trim()
            .parse()
            .expect("pid file held a pid");
        // The grandchild must die with the tree: poll for ESRCH —
        // before #30 it survived its parent's kill and kept the
        // signal slot alive for a few seconds here.
        let mut dead = false;
        while started.elapsed() < std::time::Duration::from_secs(5) {
            if unsafe { libc::kill(pid, 0) } != 0 {
                dead = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(dead, "grandchild {pid} outlived the timeout kill");
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }

    #[test]
    fn missing_command_is_an_error_result() {
        let root = temp_root("badargs");
        assert!(
            tool(&root)
                .execute(r#"{}"#)
                .unwrap_err()
                .contains("command")
        );
    }
}
