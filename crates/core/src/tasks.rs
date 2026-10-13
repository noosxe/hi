//! The background task manager (#5, AD-0019): long-lived work that
//! outlives a turn — dev servers, watchers, builds — without blocking
//! the turn loop or the UI thread.
//!
//! Design, in short (the ruling points of the #5 design):
//!
//! - **Threads, not tokio.** One worker thread per task, polling the
//!   child at 50ms — the bash tool's pattern generalized. No async
//!   runtime anywhere; the Luau bridge (#6) inherits this choice.
//! - **Process groups.** A task spawns in its own group (Unix) and
//!   cancel/timeout/shutdown kill the *group*, so grandchildren (a dev
//!   server's watchers) die with it. This is deliberately stronger
//!   than the `bash` tool, whose #30 grandchild limitation stands.
//! - **Merged output, total order.** The command runs under
//!   `bash -c "exec 2>&1; …"`, so stdout and stderr interleave in one
//!   pipe exactly as a terminal would show them — one reader thread,
//!   no interleaving races.
//! - **Two-tier buffering.** A bounded ring (256 KiB) backs instant
//!   `output_tail` reads; the full stream lands in a log file under
//!   the tasks dir for post-mortems.
//! - **Completion is a fact, not a trigger.** Finishing sends one
//!   [`TaskCompletion`] into a channel the front end drains per frame;
//!   auto-starting a turn is out of scope (that is #4's orchestrator).
//! - **Tasks are runtime objects.** Spawn echoes persist as ordinary
//!   transcript messages; running state is never resurrected; dropping
//!   the last [`TaskManager`] handle kills every running group, so a
//!   clean `/exit` orphans nothing (a crash can — the OS is the
//!   boundary, the same trust stance as every tool).
//!
//! The manager tracks *life and output*; what the worker is, is a
//! [`TaskKind`]. `Process` is the v1 kind; `Agent` arrives with #4 and
//! slots into the same lifecycle.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// In-memory output kept for instant tail reads. The full stream goes
/// to the log file regardless of this bound.
pub const RING_BYTES: usize = 256 * 1024;
/// How many tasks may run at once. Beyond it, `spawn` refuses — an
/// error the caller (tool) turns into content the model can read.
pub const MAX_RUNNING: usize = 16;
/// Child poll cadence — the bash tool's interval.
const POLL: Duration = Duration::from_millis(50);

/// hi's task-log directory: `<data-dir>/tasks`.
pub fn tasks_dir() -> Option<PathBuf> {
    crate::config::data_dir().map(|dir| dir.join("tasks"))
}

/// What a task runs. v1: a shell command — the same trust stance as
/// the `bash` tool (session's OS permissions, no jailing).
#[derive(Debug, Clone, PartialEq)]
pub enum TaskKind {
    /// A shell command run under `bash -c`, with stderr merged into
    /// stdout (terminal-style interleave).
    Process(ProcessSpec),
}

/// A shell command plus where it runs.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessSpec {
    /// The command text, handed to `bash -c` verbatim.
    pub command: String,
    /// Working directory; resolved against the session's cwd.
    pub cwd: PathBuf,
}

/// A task to spawn: what it is, what to call it, how long it may run.
#[derive(Debug, Clone)]
pub struct TaskSpec {
    /// Human label (transcript, status row). Derived from the command
    /// when the caller omits it.
    pub name: String,
    /// The work.
    pub kind: TaskKind,
    /// Optional wall-clock limit. `None` runs until it finishes or is
    /// cancelled — long-lived daemons are the point.
    pub timeout: Option<Duration>,
}

impl TaskSpec {
    /// A shell-command task with a derived name (the command's first
    /// two words, clipped — enough to recognize in a list).
    pub fn process(command: impl Into<String>, cwd: impl Into<PathBuf>) -> Self {
        let command = command.into();
        let name: String = command
            .split_whitespace()
            .take(2)
            .collect::<Vec<_>>()
            .join(" ");
        let name: String = if name.chars().count() > 24 {
            name.chars().take(24).collect::<String>() + "…"
        } else {
            name
        };
        Self {
            name,
            kind: TaskKind::Process(ProcessSpec {
                command,
                cwd: cwd.into(),
            }),
            timeout: None,
        }
    }
}

/// Lifecycle state. The worker thread owns every transition; readers
/// see snapshots.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TaskState {
    /// Still going.
    Running,
    /// Exited zero.
    Succeeded { exit_code: i32 },
    /// Exited non-zero (or was taken down by a signal).
    Failed { exit_code: i32 },
    /// Cancelled by a caller.
    Killed,
    /// Exceeded its timeout and was killed.
    TimedOut,
    /// Never started (the OS refused the spawn).
    SpawnFailed,
}

/// Seconds-and-minutes phrasing for a wall time — compact, one
/// phrasing for the task tools, the transcript notes, and the TUI's
/// tasks pane. Lives here so every consumer spells it the same way.
pub fn format_duration(wall: Duration) -> String {
    let secs = wall.as_secs();
    if secs >= 60 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{secs}.{:03}", wall.subsec_millis())
    }
}

impl TaskState {
    /// Not `Running`.
    pub fn finished(&self) -> bool {
        !matches!(self, TaskState::Running)
    }

    /// The human phrase the transcript note uses.
    pub fn label(&self) -> String {
        match *self {
            TaskState::Running => "is running".into(),
            TaskState::Succeeded { exit_code } => format!("finished (exit {exit_code})"),
            TaskState::Failed { exit_code } => format!("failed (exit {exit_code})"),
            TaskState::Killed => "was cancelled".into(),
            TaskState::TimedOut => "timed out and was killed".into(),
            TaskState::SpawnFailed => "failed to start".into(),
        }
    }
}

/// A point-in-time snapshot for status rows, listings, and the model.
#[derive(Debug, Clone)]
pub struct TaskSummary {
    /// Monotonic per-manager id, rendered `#<n>` at the tool boundary.
    pub id: u64,
    /// Human label.
    pub name: String,
    /// The command text.
    pub command: String,
    /// Lifecycle state.
    pub state: TaskState,
    /// OS pid of the direct child (the `bash -c` wrapper).
    pub pid: u32,
    /// Runtime so far — elapsed while running, total once finished.
    pub wall: Duration,
}

/// One-shot notice that a task left `Running`. Sent exactly once per
/// task, into the channel [`TaskManager::try_recv_completion`] drains.
#[derive(Debug, Clone)]
pub struct TaskCompletion {
    pub id: u64,
    pub name: String,
    pub state: TaskState,
    /// Total runtime.
    pub wall: Duration,
}

/// Bounded shared output ring.
#[derive(Clone)]
struct Ring(Arc<Mutex<VecDeque<u8>>>);

impl Ring {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(VecDeque::with_capacity(8192))))
    }

    fn push(&self, bytes: &[u8]) {
        let mut ring = self.0.lock().unwrap();
        ring.extend(bytes.iter().copied());
        let overflow = ring.len().saturating_sub(RING_BYTES);
        drop(ring.drain(..overflow));
    }

    /// The last `max_bytes` bytes, lossily decoded (a split multibyte
    /// char at the seam becomes a replacement char — display-only).
    fn tail(&self, max_bytes: usize) -> String {
        let ring = self.0.lock().unwrap();
        let start = ring.len().saturating_sub(max_bytes);
        let bytes: Vec<u8> = ring.iter().skip(start).copied().collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

/// One registry row.
struct TaskEntry {
    name: String,
    command: String,
    state: TaskState,
    pid: u32,
    started: Instant,
    ring: Ring,
    /// Set by `cancel`; the worker's poll loop observes it.
    cancel: Arc<std::sync::atomic::AtomicBool>,
}

struct Inner {
    next_id: AtomicU64,
    tasks: Arc<Mutex<HashMap<u64, TaskEntry>>>,
    tx: Arc<Mutex<Sender<TaskCompletion>>>,
    completions: Mutex<Receiver<TaskCompletion>>,
    logs_dir: PathBuf,
}

/// A cloneable handle to the background task manager. Drop the last
/// handle and every running task's process group is killed.
#[derive(Clone)]
pub struct TaskManager(Arc<Inner>);

impl TaskManager {
    /// Logs land in `logs_dir/<id>.log` (created on demand).
    pub fn new(logs_dir: impl Into<PathBuf>) -> Self {
        let (tx, rx) = channel();
        Self(Arc::new(Inner {
            next_id: AtomicU64::new(1),
            tasks: Arc::new(Mutex::new(HashMap::new())),
            tx: Arc::new(Mutex::new(tx)),
            completions: Mutex::new(rx),
            logs_dir: logs_dir.into(),
        }))
    }

    /// Spawn a task and return immediately with its id. The child is
    /// spawned synchronously (so spawn errors come back as `Err`),
    /// then handed to a worker thread that owns the lifecycle.
    pub fn spawn(&self, spec: TaskSpec) -> Result<u64, String> {
        let running = self
            .0
            .tasks
            .lock()
            .unwrap()
            .values()
            .filter(|e| !e.state.finished())
            .count();
        if running >= MAX_RUNNING {
            return Err(format!(
                "{running} tasks already running (cap {MAX_RUNNING}) — cancel one first"
            ));
        }
        let TaskKind::Process(process) = spec.kind;
        // Merged stream: stderr becomes stdout inside the child before
        // the command runs, so the pipe carries one interleaved,
        // totally ordered stream. The group makes the whole tree
        // killable (Unix; Windows gets a child-only kill, documented).
        let mut child = bash_command(&process.command, &process.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("cannot spawn task: {e}"))?;

        let id = self.0.next_id.fetch_add(1, Ordering::Relaxed);
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("task produced no output pipe".into());
        };
        std::fs::create_dir_all(&self.0.logs_dir).ok();
        let log = self.0.logs_dir.join(format!("{id}.log"));

        let ring = Ring::new();
        let entry = TaskEntry {
            name: spec.name.clone(),
            command: process.command.clone(),
            state: TaskState::Running,
            pid: child.id(),
            started: Instant::now(),
            ring: ring.clone(),
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        self.0.tasks.lock().unwrap().insert(id, entry);

        let tasks = Arc::clone(&self.0.tasks);
        let tx = Arc::clone(&self.0.tx);
        let cancel = self.0.tasks.lock().unwrap()[&id].cancel.clone();
        std::thread::Builder::new()
            .name(format!("hi-task-{id}"))
            .spawn(move || {
                monitor(
                    id,
                    spec.name,
                    child,
                    stdout,
                    ring,
                    log,
                    cancel,
                    spec.timeout,
                    tasks,
                    tx,
                );
            })
            .map_err(|e| format!("cannot start task worker: {e}"))?;
        Ok(id)
    }

    /// One task's snapshot.
    pub fn status(&self, id: u64) -> Option<TaskSummary> {
        self.0
            .tasks
            .lock()
            .unwrap()
            .get(&id)
            .map(|e| summary(id, e))
    }

    /// Every task, oldest first.
    pub fn list(&self) -> Vec<TaskSummary> {
        let tasks = self.0.tasks.lock().unwrap();
        let mut ids: Vec<u64> = tasks.keys().copied().collect();
        ids.sort_unstable();
        ids.into_iter()
            .filter_map(|id| tasks.get(&id).map(|e| summary(id, e)))
            .collect()
    }

    /// The last `max_bytes` bytes of a task's output.
    pub fn output_tail(&self, id: u64, max_bytes: usize) -> Option<String> {
        let tasks = self.0.tasks.lock().unwrap();
        tasks.get(&id).map(|e| e.ring.tail(max_bytes))
    }

    /// How many tasks are still going.
    pub fn running_count(&self) -> usize {
        self.0
            .tasks
            .lock()
            .unwrap()
            .values()
            .filter(|e| !e.state.finished())
            .count()
    }

    /// Ask a task to stop. The worker notices within one poll interval
    /// and kills the whole process group (TERM, then KILL). Cancelling
    /// an unknown id errors; a finished task is already past help.
    pub fn cancel(&self, id: u64) -> Result<(), String> {
        let tasks = self.0.tasks.lock().unwrap();
        match tasks.get(&id) {
            None => Err(format!("no such task #{id}")),
            Some(entry) if entry.state.finished() => {
                Err(format!("task #{id} already {}", entry.state.label()))
            }
            Some(entry) => {
                entry.cancel.store(true, Ordering::Relaxed);
                Ok(())
            }
        }
    }

    /// Drain one completion notice, if any finished this frame. The
    /// front end pumps this alongside its event channel.
    pub fn try_recv_completion(&self) -> Option<TaskCompletion> {
        self.0.completions.lock().unwrap().try_recv().ok()
    }
}

impl Drop for TaskManager {
    fn drop(&mut self) {
        // Last handle out kills the lights: every running group, TERM
        // then KILL. Workers may still be polling; they observe the
        // exit and finish on their own. Their completion sends land in
        // a channel nobody reads — fine, the process is leaving.
        if Arc::strong_count(&self.0) != 1 {
            return;
        }
        let running: Vec<u32> = self
            .0
            .tasks
            .lock()
            .unwrap()
            .values()
            .filter(|e| !e.state.finished())
            .map(|e| e.pid)
            .collect();
        for pid in &running {
            kill_group(*pid, libc::SIGTERM);
        }
        std::thread::sleep(Duration::from_millis(250));
        for pid in &running {
            kill_group(*pid, libc::SIGKILL);
        }
    }
}

/// The `bash -c "exec 2>&1; …"` wrapper every process task runs under.
fn bash_command(command: &str, cwd: &std::path::Path) -> Command {
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(format!("exec 2>&1; {command}"))
        .current_dir(cwd);
    #[cfg(unix)]
    {
        // Own process group: the whole tree dies with one killpg.
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd
}

/// Kill a process group (negative pid targets the group). Best effort:
/// a vanished group is fine, permissions are not expected here.
#[cfg(unix)]
fn kill_group(pid: u32, sig: i32) {
    unsafe {
        libc::kill(-(pid as i32), sig);
    }
}

#[cfg(not(unix))]
fn kill_group(pid: u32, sig: i32) {
    let _ = (pid, sig);
    // Windows has no process groups in std; nothing to do here.
}

/// The worker: own the child, drain its output, own every state
/// transition, send exactly one completion.
#[allow(clippy::too_many_arguments)]
fn monitor(
    id: u64,
    name: String,
    mut child: Child,
    stdout: std::process::ChildStdout,
    ring: Ring,
    log: PathBuf,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    timeout: Option<Duration>,
    tasks: Arc<Mutex<HashMap<u64, TaskEntry>>>,
    tx: Arc<Mutex<Sender<TaskCompletion>>>,
) {
    let started = Instant::now();
    let deadline = timeout.map(|t| started + t);
    // Reader: pipe → ring + log file. Ends at EOF (child exit).
    let reader = std::thread::spawn(move || {
        let mut pipe = stdout;
        let mut file = std::fs::File::create(&log).ok();
        let mut buf = [0u8; 8192];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    ring.push(&buf[..n]);
                    if let Some(file) = file.as_mut() {
                        let _ = file.write_all(&buf[..n]);
                    }
                }
            }
        }
    });

    let state = loop {
        match child.try_wait() {
            Ok(Some(status)) => break exit_state(status),
            Ok(None) => {}
            Err(_) => break TaskState::Failed { exit_code: -1 },
        }
        if cancel.load(Ordering::Relaxed) {
            stop_tree(&mut child);
            break TaskState::Killed;
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            stop_tree(&mut child);
            break TaskState::TimedOut;
        }
        std::thread::sleep(POLL);
    };
    let _ = reader.join();
    let wall = started.elapsed();

    if let Some(entry) = tasks.lock().unwrap().get_mut(&id) {
        entry.state = state;
    }
    // A dead receiver just means nobody is listening (shutdown) — the
    // registry row above is the durable fact.
    let _ = tx.lock().unwrap().send(TaskCompletion {
        id,
        name,
        state,
        wall,
    });
}

/// Map an exit status to its state. Signal deaths have no code; they
/// read as failures with the conventional 128+n payload.
fn exit_state(status: std::process::ExitStatus) -> TaskState {
    match status.code() {
        Some(0) => TaskState::Succeeded { exit_code: 0 },
        Some(code) => TaskState::Failed { exit_code: code },
        None => TaskState::Failed { exit_code: -1 },
    }
}

/// Kill the whole tree: TERM the group, give it a moment, KILL.
fn stop_tree(child: &mut Child) {
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

fn summary(id: u64, entry: &TaskEntry) -> TaskSummary {
    TaskSummary {
        id,
        name: entry.name.clone(),
        command: entry.command.clone(),
        state: entry.state,
        pid: entry.pid,
        wall: entry.started.elapsed(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn manager() -> (TaskManager, PathBuf) {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir()
            .join(format!("hi-tasks-test-{}", std::process::id()))
            .join(SEQ.fetch_add(1, Ordering::Relaxed).to_string());
        std::fs::create_dir_all(&dir).unwrap();
        (TaskManager::new(dir.clone()), dir)
    }

    /// Predicate wait — no fixed sleeps (house test rule).
    fn until(what: &str, mut pred: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if pred() {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("timed out waiting for {what}");
    }

    fn state_of(m: &TaskManager, id: u64) -> TaskState {
        m.status(id).unwrap().state
    }

    /// True when nothing in the process group `pid` is alive.
    fn group_dead(pid: u32) -> bool {
        unsafe { libc::kill(-(pid as i32), 0) != 0 }
    }

    #[test]
    fn echo_completes_succeeded_with_output() {
        let (m, _dir) = manager();
        let id = m
            .spawn(TaskSpec {
                timeout: None,
                ..TaskSpec::process("echo hello-tasks", std::env::temp_dir())
            })
            .unwrap();
        until("echo to finish", || state_of(&m, id).finished());
        assert_eq!(state_of(&m, id), TaskState::Succeeded { exit_code: 0 });
        assert!(m.output_tail(id, 4096).unwrap().contains("hello-tasks"));
        // Exactly one completion, then silence.
        let c = m.try_recv_completion().unwrap();
        assert_eq!((c.id, c.state), (id, TaskState::Succeeded { exit_code: 0 }));
        assert!(m.try_recv_completion().is_none(), "exactly one notice");
    }

    #[test]
    fn failing_exit_marks_failed() {
        let (m, _dir) = manager();
        let id = m
            .spawn(TaskSpec::process("exit 3", std::env::temp_dir()))
            .unwrap();
        until("exit 3 to finish", || state_of(&m, id).finished());
        assert_eq!(state_of(&m, id), TaskState::Failed { exit_code: 3 });
    }

    #[test]
    fn cancel_kills_the_whole_group() {
        let (m, _dir) = manager();
        // The inner `sleep` is a grandchild: only a group kill gets it.
        let id = m
            .spawn(TaskSpec::process(
                "sleep 30 & echo started; wait",
                std::env::temp_dir(),
            ))
            .unwrap();
        let summary = loop {
            if let Some(s) = m.status(id)
                && s.state.finished()
            {
                panic!("task finished before cancel");
            }
            if m.output_tail(id, 4096).unwrap().contains("started") {
                break m.status(id).unwrap();
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        m.cancel(id).unwrap();
        until("cancel to land", || state_of(&m, id).finished());
        assert_eq!(state_of(&m, id), TaskState::Killed);
        until("process group to die", || group_dead(summary.pid));
        // Cancelling again is an honest refusal, not a panic.
        assert!(m.cancel(id).is_err());
    }

    #[test]
    fn timeout_kills_and_marks_timed_out() {
        let (m, _dir) = manager();
        let id = m
            .spawn(TaskSpec {
                timeout: Some(Duration::from_millis(300)),
                ..TaskSpec::process("sleep 30", std::env::temp_dir())
            })
            .unwrap();
        let pid = m.status(id).unwrap().pid;
        until("timeout to land", || state_of(&m, id).finished());
        assert_eq!(state_of(&m, id), TaskState::TimedOut);
        until("process group to die", || group_dead(pid));
    }

    #[test]
    fn ring_is_bounded_but_the_log_keeps_everything() {
        let (m, dir) = manager();
        let id = m
            .spawn(TaskSpec::process(
                "head -c 1000000 /dev/zero | tr '\\0' 'a'",
                std::env::temp_dir(),
            ))
            .unwrap();
        until("1MB task to finish", || state_of(&m, id).finished());
        let tail = m.output_tail(id, usize::MAX).unwrap();
        assert!(tail.len() <= RING_BYTES, "ring leaked: {}", tail.len());
        // The disk log holds the full million bytes.
        let log = std::fs::read(dir.join(format!("{id}.log"))).unwrap();
        assert_eq!(log.len(), 1_000_000);
    }

    #[test]
    fn spawn_failures_are_errors_without_burning_an_id() {
        let (m, _dir) = manager();
        // A missing cwd refuses the spawn of bash itself.
        let err = m
            .spawn(TaskSpec::process(
                "echo x",
                PathBuf::from("/nonexistent-cwd-xyz"),
            ))
            .unwrap_err();
        assert!(err.contains("cannot spawn"), "{err}");
        // The next successful spawn still gets #1.
        let id = m
            .spawn(TaskSpec::process("true", std::env::temp_dir()))
            .unwrap();
        assert_eq!(id, 1);
    }

    #[test]
    fn the_cap_refuses_spawns_and_drop_cleans_up() {
        let (m, _dir) = manager();
        let mut ids = Vec::new();
        for _ in 0..MAX_RUNNING {
            ids.push(
                m.spawn(TaskSpec::process("sleep 30", std::env::temp_dir()))
                    .unwrap(),
            );
        }
        let err = m
            .spawn(TaskSpec::process("sleep 30", std::env::temp_dir()))
            .unwrap_err();
        assert!(err.contains("cap"), "{err}");
        // Dropping the last handle kills every running group.
        let pids: Vec<u32> = ids.iter().map(|&id| m.status(id).unwrap().pid).collect();
        drop(m);
        for pid in pids {
            until("group to die on drop", || group_dead(pid));
        }
    }

    #[test]
    fn list_and_status_shape_the_facts() {
        let (m, _dir) = manager();
        let a = m
            .spawn(TaskSpec::process("sleep 30", std::env::temp_dir()))
            .unwrap();
        let b = m
            .spawn(TaskSpec::process("true", std::env::temp_dir()))
            .unwrap();
        assert_eq!(m.running_count(), 2);
        until("true to finish", || state_of(&m, b).finished());
        let list = m.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, a);
        assert_eq!(list[0].state, TaskState::Running);
        assert_eq!(list[1].state, TaskState::Succeeded { exit_code: 0 });
        assert_eq!(list[0].name, "sleep 30");
        assert_eq!(m.running_count(), 1);
        m.cancel(a).unwrap();
    }

    #[test]
    fn derived_names_clip_long_commands() {
        let spec = TaskSpec::process("cargo build --release --all-features", std::env::temp_dir());
        assert_eq!(spec.name, "cargo build");
        let long = TaskSpec::process(
            "aaaaaaaaaaaaaaaaaaaaaaaaaa bbbbbbbbbbbbbbbbbbbbbb",
            std::env::temp_dir(),
        );
        assert!(long.name.chars().count() <= 25, "{}", long.name);
    }

    #[test]
    fn output_tail_reads_the_stream_while_running() {
        let (m, _dir) = manager();
        let id = m
            .spawn(TaskSpec::process(
                "echo first; sleep 30; echo never",
                std::env::temp_dir(),
            ))
            .unwrap();
        until("first line to arrive", || {
            m.output_tail(id, 4096).unwrap().contains("first")
        });
        m.cancel(id).unwrap();
    }
}
