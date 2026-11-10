//! `bash` — run a shell command in the session's working directory.
//!
//! stdout and stderr are captured separately, rendered with labels, and
//! tail-clamped (a failing command's diagnosis lives at the bottom). A
//! non-zero exit is an **error result with the output inside it** — the
//! model must see what went wrong to adapt. The optional `timeout` is
//! seconds, with no default (pi-parity); on expiry the whole process
//! group is stopped (TERM, then KILL — `process::stop_tree`).
//!
//! Output is read by detached threads into shared buffers. After the
//! leader exits (or the timeout fires) the tool waits at most
//! `DRAIN_GRACE` for both pipes to reach EOF and returns what it has,
//! so a background child (`server &`) that keeps the pipe open cannot
//! hang the tool. Survivors of a normal exit stay alive (#173): the
//! readers keep draining so they never take SIGPIPE, but discard
//! everything after the tool returned. A result whose pipes were still
//! open carries a note saying so. A child that left the group
//! (`setsid`, `set -m`) is not killed by a timeout, but cannot hang it.
//!
//! Exit (#175): each call registers its process group in a
//! [`BashGroups`] registry for the child's lifetime (RAII, so every
//! return path unregisters). `BashTool::new` uses the process-wide
//! registry, which `exit::shutdown` kills on the way out — a call
//! abandoned by Ctrl-C (or by Esc, then a quit) does not outlive `hi`.
//! Esc alone does not kill the call; the guarantee covers exits. The
//! sweep also closes the registry: a call starting afterwards fails with
//! "hi is exiting" and spawns nothing.

use super::truncate;
use super::{args_object, opt_str, opt_usize};
use crate::process::BashGroups;
use crate::tool::{Tool, ToolAnnotations};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// How long to wait for the pipes to hit EOF once the leader is gone.
const DRAIN_GRACE: Duration = Duration::from_millis(500);

const BACKGROUND_NOTE: &str = "[note: a background process still holds the output pipes; \
its later output is not captured. Use task_run for long-lived work.]";

/// Output shared with a detached reader thread: `Some` while capturing,
/// `None` once the main path has taken it (the reader then discards, so
/// post-return growth is structurally impossible).
type Capture = Mutex<Option<Vec<u8>>>;

type Done = Arc<(Mutex<bool>, Condvar)>;

fn spawn_reader<R: Read + Send + 'static>(mut pipe: R, cap: Arc<Capture>, done: Done) {
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Some(buf) = cap.lock().unwrap().as_mut() {
                        buf.extend_from_slice(&chunk[..n]);
                    }
                }
            }
        }
        let (flag, cv) = &*done;
        *flag.lock().unwrap() = true;
        cv.notify_all();
    });
}

/// Wait until every `done` flag is set or `until` passes; true if all set.
fn wait_eof(dones: &[&Done], until: Instant) -> bool {
    for done in dones {
        let (flag, cv) = &***done;
        let mut g = flag.lock().unwrap();
        while !*g {
            let now = Instant::now();
            if now >= until {
                return false;
            }
            g = cv.wait_timeout(g, until - now).unwrap().0;
        }
    }
    true
}

pub struct BashTool {
    cwd: PathBuf,
    groups: BashGroups,
}

impl BashTool {
    /// Registers its groups in the process-wide [`BashGroups`].
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self::with_groups(cwd, BashGroups::global())
    }

    /// Like [`BashTool::new`] with an injected registry (tests).
    pub fn with_groups(cwd: impl Into<PathBuf>, groups: BashGroups) -> Self {
        Self {
            cwd: cwd.into(),
            groups,
        }
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
         timeout in seconds (no default — long-running commands run until finished). \
         A timeout stops the whole process group. Background processes (`cmd &`) are \
         left running when the command exits, but the tool returns after at most a short \
         drain and their later output is not captured; use task_run for long-lived work."
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
        // Spawned and registered under the registry's lock (refused
        // once the exit sweep closed it); unregisters on every return.
        let (mut child, _registered) = self.groups.spawn(&mut cmd)?;
        let Some(out_pipe) = child.stdout.take() else {
            return Err("bash produced no stdout pipe".into());
        };
        let Some(err_pipe) = child.stderr.take() else {
            return Err("bash produced no stderr pipe".into());
        };

        let new_cap = || Arc::new(Mutex::new(Some(Vec::new())));
        let (out_cap, err_cap) = (new_cap(), new_cap());
        let out_done: Done = Arc::new((Mutex::new(false), Condvar::new()));
        let err_done: Done = Arc::new((Mutex::new(false), Condvar::new()));
        spawn_reader(out_pipe, out_cap.clone(), out_done.clone());
        spawn_reader(err_pipe, err_cap.clone(), err_done.clone());

        // A timeout too large for `Instant` to represent is no deadline
        // at all (#172): `checked_add`, never `+`, which panics.
        let deadline =
            timeout.and_then(|secs| Instant::now().checked_add(Duration::from_secs(secs as u64)));
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
        // A timeout no longer discards what was captured, and the tool
        // returns at most `DRAIN_GRACE` after the leader is gone even if
        // something still holds the pipes (#173); output arriving after
        // the grace is not captured.
        let drained = wait_eof(&[&out_done, &err_done], Instant::now() + DRAIN_GRACE);
        // `take()` is both the snapshot and the detach.
        let take = |cap: &Capture| {
            let buf = cap.lock().unwrap().take().unwrap_or_default();
            String::from_utf8_lossy(&buf).into_owned()
        };
        let out = take(&out_cap);
        let err = take(&err_cap);

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
        if !drained {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(BACKGROUND_NOTE);
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
    fn an_unrepresentable_timeout_means_no_deadline() {
        // #172: u64::MAX seconds overflowed `Instant + Duration` and
        // panicked the agent thread. It must simply run the command.
        let root = temp_root("hugetimeout");
        let out = tool(&root)
            .execute(r#"{"command":"echo fine","timeout":18446744073709551615}"#)
            .unwrap();
        assert!(out.contains("fine"), "got: {out}");
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

    #[cfg(unix)]
    fn alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[cfg(unix)]
    fn kill_group_of(pid: i32) {
        // Kill the survivor's whole group (it shares the tool's group),
        // plus the pid itself in case it left.
        unsafe {
            let pgid = libc::getpgid(pid);
            if pgid > 0 {
                libc::kill(-pgid, libc::SIGKILL);
            }
            libc::kill(pid, libc::SIGKILL);
        }
    }

    #[cfg(unix)]
    fn read_pid(file: &Path) -> i32 {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(10) {
            if let Ok(s) = fs::read_to_string(file)
                && let Ok(pid) = s.trim().parse()
            {
                return pid;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("pid file {} never appeared", file.display());
    }

    #[cfg(unix)]
    fn wait_dead(pid: i32) -> bool {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            if !alive(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn run(root: &Path, command: &str, timeout: Option<u64>) -> Result<String, String> {
        let mut args = serde_json::json!({ "command": command });
        if let Some(t) = timeout {
            args["timeout"] = t.into();
        }
        tool(root).execute(&args.to_string())
    }

    #[test]
    #[cfg(unix)]
    fn a_background_survivor_does_not_hang_the_tool_and_stays_alive() {
        let root = temp_root("bgsurvivor");
        let pidf = root.join("pid");
        let cmd = format!("sleep 30 & echo $! > {}; echo started", pidf.display());
        let started = Instant::now();
        let out = run(&root, &cmd, None).unwrap();
        assert!(started.elapsed() < Duration::from_secs(15), "hung");
        assert!(out.contains("started"), "got: {out}");
        assert!(out.contains("background process"), "got: {out}");
        let pid = read_pid(&pidf);
        let still = alive(pid);
        unsafe { libc::kill(pid, libc::SIGKILL) };
        assert!(still, "survivor was killed");
    }

    #[test]
    #[cfg(unix)]
    fn a_survivor_writing_after_return_does_not_take_sigpipe() {
        let root = temp_root("sigpipe");
        let pidf = root.join("pid");
        let outf = root.join("result");
        // Writes after the tool returned (the sleep outlasts the
        // grace), then records that it survived the write.
        let cmd = format!(
            "(echo $BASHPID > {p}; sleep 2; echo late; sleep 1; echo later; echo survived > {o}; sleep 30) & echo hi",
            p = pidf.display(),
            o = outf.display()
        );
        let out = run(&root, &cmd, None).unwrap();
        assert!(out.contains("hi"), "got: {out}");
        let pid = read_pid(&pidf);
        let start = Instant::now();
        while !outf.exists() && start.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(50));
        }
        let survived = outf.exists() && alive(pid);
        kill_group_of(pid);
        assert!(survived, "survivor died writing to the closed-out pipe");
    }

    #[test]
    #[cfg(unix)]
    fn the_note_is_present_even_with_no_other_output() {
        let root = temp_root("quietnote");
        let pidf = root.join("pid");
        let cmd = format!("sleep 30 > /dev/null 2>&1 & echo $! > {}", pidf.display());
        // Redirected survivor holds no pipe: no note expected there.
        let out = run(&root, &cmd, None).unwrap();
        assert_eq!(out, "(no output)");
        unsafe { libc::kill(read_pid(&pidf), libc::SIGKILL) };

        let pidf = root.join("pid2");
        let cmd = format!("sleep 30 & echo $! > {}", pidf.display());
        let out = run(&root, &cmd, None).unwrap();
        assert!(out.contains("background process"), "got: {out}");
        assert!(!out.contains("(no output)"), "got: {out}");
        unsafe { libc::kill(read_pid(&pidf), libc::SIGKILL) };
    }

    #[test]
    #[cfg(unix)]
    fn a_survivor_holding_only_one_pipe_still_gets_the_note() {
        let root = temp_root("onepipe");
        // `> /dev/null` leaves the survivor holding only stderr;
        // `2> /dev/null` only stdout. Either pipe alone must count.
        for (tag, redirect) in [("stderr-only", ">"), ("stdout-only", "2>")] {
            let pidf = root.join(format!("pid-{tag}"));
            let cmd = format!(
                "sleep 30 {redirect} /dev/null & echo $! > {}",
                pidf.display()
            );
            let out = run(&root, &cmd, None).unwrap();
            let pid = read_pid(&pidf);
            unsafe { libc::kill(pid, libc::SIGKILL) };
            assert!(out.contains("background process"), "{tag}: got: {out}");
        }
    }

    #[test]
    #[cfg(unix)]
    fn the_note_is_present_on_error_results() {
        let root = temp_root("errnote");
        let pidf = root.join("pid");
        let cmd = format!("sleep 30 & echo $! > {}; exit 2", pidf.display());
        let err = run(&root, &cmd, None).unwrap_err();
        unsafe { libc::kill(read_pid(&pidf), libc::SIGKILL) };
        assert!(err.starts_with("exit code 2"), "got: {err}");
        assert!(err.contains("background process"), "got: {err}");
    }

    #[test]
    fn a_normal_command_has_no_note() {
        let root = temp_root("nonote");
        let out = run(&root, "echo plain", None).unwrap();
        assert!(!out.contains("background process"), "got: {out}");
        let err = run(&root, "echo plain; exit 1", None).unwrap_err();
        assert!(!err.contains("background process"), "got: {err}");
    }

    #[test]
    fn foreground_output_over_the_pipe_buffer_is_captured_to_the_tail() {
        let root = temp_root("bigout");
        // ~600KB, far over the 64KB pipe buffer and the 50KB clamp.
        let out = run(&root, "seq 1 100000; echo THE-END", None).unwrap();
        assert!(out.contains("THE-END"), "tail lost");
        assert!(out.contains("100000"), "tail lost");
    }

    #[test]
    #[cfg(unix)]
    fn a_term_immune_child_cannot_defeat_the_timeout() {
        let root = temp_root("immune");
        let pidf = root.join("pid");
        // The trap is installed before the announce. The deadline runs
        // from spawn, so the timeout is 2s to leave room for startup.
        let cmd = format!(
            "(trap '' TERM; echo $BASHPID > {p}; echo announced; sleep 30) & while [ ! -s {p} ]; do sleep 0.02; done; wait",
            p = pidf.display()
        );
        let started = Instant::now();
        let err = run(&root, &cmd, Some(2)).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(15), "hung");
        assert!(err.starts_with("timeout after 2s"), "got: {err}");
        assert!(err.contains("announced"), "output lost: {err}");
        let pid = read_pid(&pidf);
        let dead = wait_dead(pid);
        if !dead {
            kill_group_of(pid);
        }
        assert!(dead, "TERM-immune child {pid} survived the timeout");
    }

    #[test]
    #[cfg(unix)]
    fn a_child_that_left_the_group_cannot_hang_a_timeout() {
        let root = temp_root("escapee");
        let pidf = root.join("pid");
        let fgf = root.join("fg");
        // `set -m` gives every job, the foreground one too, its own
        // process group, so neither stop_tree nor the group kill reaches
        // them: the test records and kills both.
        let cmd = format!(
            "set -m; (echo $BASHPID > {p}; sleep 30) & while [ ! -s {p} ]; do sleep 0.02; done; echo before; sleep 30 & echo $! > {f}; wait",
            p = pidf.display(),
            f = fgf.display()
        );
        let started = Instant::now();
        let err = run(&root, &cmd, Some(1)).unwrap_err();
        let pid = read_pid(&pidf);
        let fg = read_pid(&fgf);
        let escaped = unsafe { libc::getpgid(pid) } == pid;
        let was_alive = alive(pid);
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
            libc::kill(-fg, libc::SIGKILL);
            libc::kill(fg, libc::SIGKILL);
        }
        assert!(started.elapsed() < Duration::from_secs(15), "hung");
        assert!(err.starts_with("timeout after 1s"), "got: {err}");
        assert!(err.contains("before"), "got: {err}");
        assert!(
            was_alive && escaped,
            "set -m did not move the job to its own group"
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_running_call_is_registered_and_unregisters_after_it_returns() {
        let root = temp_root("registry");
        let groups = BashGroups::new();
        let t = BashTool::with_groups(&root, groups.clone());
        assert!(groups.is_empty());
        let seen = std::thread::scope(|s| {
            let h = s.spawn(|| t.execute(r#"{"command":"sleep 1; echo done"}"#));
            let start = Instant::now();
            while groups.is_empty() && start.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_millis(10));
            }
            let seen = groups.len();
            assert!(h.join().unwrap().unwrap().contains("done"));
            seen
        });
        assert_eq!(seen, 1, "the running call was not registered");
        assert!(groups.is_empty(), "group left registered after success");
    }

    #[test]
    #[cfg(unix)]
    fn groups_unregister_on_timeout_and_on_error() {
        let root = temp_root("registry-err");
        let groups = BashGroups::new();
        let t = BashTool::with_groups(&root, groups.clone());
        let err = t
            .execute(r#"{"command":"sleep 30","timeout":1}"#)
            .unwrap_err();
        assert!(err.starts_with("timeout after 1s"), "got: {err}");
        assert!(groups.is_empty(), "group left registered after timeout");
        let err = t.execute(r#"{"command":"exit 3"}"#).unwrap_err();
        assert!(err.starts_with("exit code 3"), "got: {err}");
        assert!(groups.is_empty(), "group left registered after error");
    }

    #[test]
    #[cfg(unix)]
    fn kill_all_stops_an_in_flight_call_and_its_children() {
        let root = temp_root("killall");
        let pidf = root.join("pid");
        let groups = BashGroups::new();
        let t = BashTool::with_groups(&root, groups.clone());
        let cmd = serde_json::json!({
            "command": format!("sleep 60 & echo $! > {}; wait", pidf.display())
        })
        .to_string();
        std::thread::scope(|s| {
            let h = s.spawn(|| t.execute(&cmd));
            let pid = read_pid(&pidf);
            groups.kill_all();
            let dead = wait_dead(pid);
            if !dead {
                kill_group_of(pid);
            }
            let _ = h.join();
            assert!(dead, "background child {pid} survived kill_all");
        });
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
