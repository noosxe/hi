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

/// Kill the whole tree: TERM the group, give it a moment, KILL.
pub(crate) fn stop_tree(child: &mut Child) {
    let pid = child.id();
    kill_group(pid, libc::SIGTERM);
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => std::thread::sleep(POLL),
        }
    }
    kill_group(pid, libc::SIGKILL);
    let _ = child.wait();
}
