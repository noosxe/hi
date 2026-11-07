//! Process-group plumbing shared by the task manager and the bash
//! tool (#30): children spawn into a fresh group so one `killpg`
//! takes the whole tree — TERM, a grace beat, KILL. Windows gets
//! Job Objects in #31; until then only the group helpers are gated.

use std::process::{Child, Command};
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(50);

/// Put the command into its own process group (unix): the whole
/// tree dies with one killpg. Nothing to do elsewhere yet — #31.
pub(crate) fn own_group(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(not(unix))]
    let _ = cmd;
}

/// Kill a process group (negative pid targets the group). Best
/// effort: a vanished group is fine, permissions are not expected
/// here.
#[cfg(unix)]
pub(crate) fn kill_group(pid: u32, sig: i32) {
    unsafe {
        libc::kill(-(pid as i32), sig);
    }
}

#[cfg(not(unix))]
pub(crate) fn kill_group(pid: u32, sig: i32) {
    let _ = (pid, sig);
    // Windows has no process groups in std; nothing to do here.
}

/// Is anything still in the process group `pid` (negative pid probes
/// the group)? ESRCH means empty; EPERM means members exist. An
/// unreaped zombie leader still counts, so reap before probing.
/// Non-unix reports false until #31.
#[cfg(unix)]
pub(crate) fn group_alive(pid: u32) -> bool {
    if unsafe { libc::kill(-(pid as i32), 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(not(unix))]
pub(crate) fn group_alive(pid: u32) -> bool {
    let _ = pid;
    false
}

/// Kill the whole tree (#173): TERM the group, then poll until the
/// leader is reaped AND the group is empty; at the 2s deadline KILL
/// whatever remains and reap. A group that dies at TERM returns at
/// once; a TERM-immune member of a dead leader's group still gets KILL.
pub(crate) fn stop_tree(child: &mut Child) {
    let pid = child.id();
    kill_group(pid, libc::SIGTERM);
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut reaped = false;
    while Instant::now() < deadline {
        // A `try_wait` error falls through to KILL below.
        match child.try_wait() {
            Ok(Some(_)) => reaped = true,
            Ok(None) => {}
            Err(_) => break,
        }
        if reaped && !group_alive(pid) {
            return;
        }
        std::thread::sleep(POLL);
    }
    kill_group(pid, libc::SIGKILL);
    let _ = child.wait();
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::Stdio;

    fn temp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("hi-proc-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn spawn(script: &str) -> Child {
        let mut cmd = Command::new("bash");
        cmd.arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        own_group(&mut cmd);
        cmd.spawn().unwrap()
    }

    fn wait_for_pid(file: &std::path::Path) -> i32 {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(10) {
            if let Ok(s) = std::fs::read_to_string(file)
                && let Ok(pid) = s.trim().parse()
            {
                return pid;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("pid file never appeared");
    }

    fn gone(pid: i32) -> bool {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            if unsafe { libc::kill(pid, 0) } != 0 {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn stop_tree_kills_a_term_immune_member_after_the_leader_died() {
        let dir = temp("immune");
        let pidf = dir.join("pid");
        let ready = dir.join("ready");
        // The member traps TERM, announces its pid once the trap is
        // live, and outlives the leader (which exits on TERM).
        let script = format!(
            "(trap '' TERM; echo $BASHPID > {p}; sleep 60) & while [ ! -s {p} ]; do sleep 0.02; done; touch {r}; sleep 60",
            p = pidf.display(),
            r = ready.display()
        );
        let mut child = spawn(&script);
        let pid = wait_for_pid(&pidf);
        let start = Instant::now();
        while !ready.exists() {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "leader never got ready"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        stop_tree(&mut child);
        let dead = gone(pid);
        if !dead {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        assert!(dead, "TERM-immune member {pid} survived stop_tree");
    }

    #[test]
    fn stop_tree_returns_promptly_when_the_group_dies_at_term() {
        let mut child = spawn("sleep 60 & sleep 60");
        std::thread::sleep(Duration::from_millis(300));
        let started = Instant::now();
        stop_tree(&mut child);
        let took = started.elapsed();
        assert!(took < Duration::from_millis(1500), "took {took:?}");
    }
}
