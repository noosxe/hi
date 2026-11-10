//! Process-group plumbing shared by the task manager and the bash
//! tool (#30): children spawn into a fresh group so one `killpg`
//! takes the whole tree — TERM, a grace beat, KILL. Windows gets
//! Job Objects in #31; until then only the group helpers are gated.
//!
//! Exit (#175): [`BashGroups`] is the registry of live foreground
//! `bash` groups. The tool registers each group for the child's
//! lifetime (RAII). [`BashGroups::kill_all`] takes them all down at
//! exit and closes the registry, so a later `spawn` is refused and no
//! group can slip in behind the sweep. A `bash` call abandoned
//! mid-turn (Ctrl-C, or Esc followed by a quit) therefore does not
//! outlive `hi`. Esc itself does not kill the call — the guarantee
//! covers exits only. [`terminate_groups`] is the TERM, grace, KILL
//! sweep shared with `TaskManager::shutdown`.

use std::collections::HashSet;
use std::process::{Child, Command};
use std::sync::{Arc, Mutex, OnceLock};
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

/// How long an exit sweep waits for TERM to empty the groups before
/// KILL.
pub(crate) const EXIT_GRACE: Duration = Duration::from_millis(250);

/// TERM every group, wait up to `grace` for them to empty, KILL what
/// remains. No groups, no signals and no wait.
pub(crate) fn terminate_groups(pids: &[u32], grace: Duration) {
    if pids.is_empty() {
        return;
    }
    for pid in pids {
        kill_group(*pid, libc::SIGTERM);
    }
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline && pids.iter().any(|pid| group_alive(*pid)) {
        std::thread::sleep(Duration::from_millis(10));
    }
    for pid in pids {
        kill_group(*pid, libc::SIGKILL);
    }
}

/// The live foreground `bash` process groups (#175). Cloneable; the
/// process-wide instance is [`BashGroups::global`], tests build their
/// own with [`BashGroups::new`] and must never `kill_all` the global
/// one (parallel tests would lose their children).
///
/// `kill_all` also **closes** the registry: [`BashGroups::spawn`] then
/// refuses, so an agent thread still running tool rounds after a
/// mid-turn quit cannot start a group the sweep missed. Spawn checks
/// `closed` and registers the pid under one lock, and `kill_all`
/// closes and snapshots under the same lock — no window between them.
#[derive(Clone, Default)]
pub struct BashGroups(Arc<Mutex<GroupState>>);

#[derive(Default)]
struct GroupState {
    closed: bool,
    pids: HashSet<u32>,
}

/// Unregisters its group on drop — every return path of the tool.
pub(crate) struct GroupGuard {
    groups: BashGroups,
    pid: u32,
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        self.groups.0.lock().unwrap().pids.remove(&self.pid);
    }
}

impl BashGroups {
    /// A fresh, empty registry (tests, injection).
    pub fn new() -> Self {
        Self::default()
    }

    /// The process-wide registry every `BashTool::new` uses.
    pub fn global() -> Self {
        static GLOBAL: OnceLock<BashGroups> = OnceLock::new();
        GLOBAL.get_or_init(BashGroups::new).clone()
    }

    /// Spawn `cmd` and register its group, atomically with respect to
    /// `kill_all`: a closed registry spawns nothing.
    pub(crate) fn spawn(&self, cmd: &mut Command) -> Result<(Child, GroupGuard), String> {
        let mut state = self.0.lock().unwrap();
        if state.closed {
            return Err("hi is exiting; not starting a new process".into());
        }
        let child = cmd.spawn().map_err(|e| format!("cannot spawn bash: {e}"))?;
        let pid = child.id();
        state.pids.insert(pid);
        drop(state);
        Ok((
            child,
            GroupGuard {
                groups: self.clone(),
                pid,
            },
        ))
    }

    /// How many groups are registered right now.
    pub fn len(&self) -> usize {
        self.0.lock().unwrap().pids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Close the registry, then TERM every registered group, wait a
    /// grace beat, KILL what remains. Idempotent; nothing registered
    /// means no wait. Later `spawn`s are refused.
    pub fn kill_all(&self) {
        let pids: Vec<u32> = {
            let mut state = self.0.lock().unwrap();
            state.closed = true;
            state.pids.iter().copied().collect()
        };
        terminate_groups(&pids, EXIT_GRACE);
    }
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

    impl BashGroups {
        /// Register an already-spawned pid (test helper).
        fn adopt(&self, pid: u32) -> GroupGuard {
            self.0.lock().unwrap().pids.insert(pid);
            GroupGuard {
                groups: self.clone(),
                pid,
            }
        }
    }

    fn pid_alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[test]
    fn kill_all_kills_a_registered_running_group() {
        let dir = temp("killall");
        let pidf = dir.join("pid");
        let mut child = spawn(&format!("echo $$ > {}; sleep 60", pidf.display()));
        let pid = wait_for_pid(&pidf);
        let groups = BashGroups::new();
        let guard = groups.adopt(child.id());
        assert_eq!(groups.len(), 1);
        groups.kill_all();
        let _ = child.wait();
        let dead = gone(pid);
        if !dead {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        assert!(dead, "registered group survived kill_all");
        // Idempotent, and no wait when nothing is left registered.
        drop(guard);
        assert!(groups.is_empty());
        let started = Instant::now();
        groups.kill_all();
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn a_spawn_after_kill_all_starts_nothing() {
        let dir = temp("closed");
        let pidf = dir.join("pid");
        let groups = BashGroups::new();
        groups.kill_all();
        let mut cmd = Command::new("bash");
        cmd.arg("-c")
            .arg(format!("echo $$ > {}; sleep 60", pidf.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        own_group(&mut cmd);
        let err = groups
            .spawn(&mut cmd)
            .err()
            .expect("closed registry spawned");
        assert!(err.contains("exiting"), "{err}");
        std::thread::sleep(Duration::from_millis(300));
        assert!(!pidf.exists(), "a process started after kill_all");
        assert!(groups.is_empty());
    }

    #[test]
    fn kill_all_ignores_unregistered_groups() {
        let dir = temp("unreg");
        let pidf = dir.join("pid");
        let mut child = spawn(&format!("echo $$ > {}; sleep 60", pidf.display()));
        let pid = wait_for_pid(&pidf);
        BashGroups::new().kill_all();
        let alive = pid_alive(pid);
        unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
        let _ = child.wait();
        assert!(alive, "an unregistered group was killed");
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
