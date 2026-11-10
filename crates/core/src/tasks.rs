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
//!   server's watchers) die with it. The `bash` tool shares that
//!   plumbing (`process::own_group` / `stop_tree`, #30) for its timeout
//!   kill; the difference is that a task outlives its leader until the
//!   group is empty, while `bash` returns when the leader exits.
//!   A task lives until its group is empty (#174), reports the
//!   leader's state, and waits a bounded time for the reader's EOF.
//! - **Merged output, total order.** The command runs under
//!   `bash -c "exec 2>&1; …"`, so stdout and stderr interleave in one
//!   pipe exactly as a terminal would show them — one reader thread,
//!   no interleaving races.
//! - **Two-tier buffering.** A bounded ring (256 KiB) backs instant
//!   `output_tail` reads; the full stream lands in a log file under
//!   the tasks dir for post-mortems, one file per run and task
//!   (`<run>-<id>.log`, #188).
//! - **Logs are bounded on disk too (#74).** Manager construction
//!   prunes the tasks dir best-effort: logs older than
//!   [`TASK_LOG_MAX_AGE`] go, aged survivors beyond
//!   [`TASK_LOG_CEIL_BYTES`] (newest kept) go, and nothing younger
//!   than [`TASK_LOG_CEIL_GRACE`] is ever evicted — a concurrent
//!   run's live log is untouchable. Never blocks startup; one stderr
//!   line only when something was removed.
//! - **Completion is a fact, not a trigger.** Finishing sends one
//!   [`TaskCompletion`] into a channel the front end drains per frame;
//!   auto-starting a turn is out of scope (that is #4's orchestrator).
//! - **Tasks are runtime objects.** Spawn echoes persist as ordinary
//!   transcript messages; running state is never resurrected; dropping
//!   the last [`TaskManager`] handle kills every running group, so a
//!   clean `/exit` orphans nothing (a crash can — the OS is the
//!   boundary, the same trust stance as every tool).
//! - **Exit is explicit (#175).** [`TaskManager::shutdown`] kills every
//!   running group (TERM, grace, KILL) whatever the handle count is —
//!   the in-flight turn's worker thread holds a registry clone at a
//!   mid-turn quit, so a last-handle `Drop` alone never fires there.
//!   Idempotent; `Drop` reuses it under the last-handle condition.
//!   It also closes the manager: `spawn` afterwards returns "hi is
//!   exiting" and starts nothing (the flag is set and re-checked under
//!   the tasks lock, so a racing spawn is either swept or kills its own
//!   child).
//!   `exit::shutdown` calls it on every way out of `hi`.
//!
//! The manager tracks *life and output*; what the worker is, is a
//! [`TaskKind`]. `Process` is the v1 kind; `Agent` arrives with #4 and
//! slots into the same lifecycle.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use crate::process::{kill_group, stop_tree};
use std::time::{Duration, Instant};

/// In-memory output kept for instant tail reads. The full stream goes
/// to the log file regardless of this bound.
pub const RING_BYTES: usize = 256 * 1024;
/// How many tasks may run at once. Beyond it, `spawn` refuses — an
/// error the caller (tool) turns into content the model can read.
pub const MAX_RUNNING: usize = 16;
/// The refusal `spawn` returns once `shutdown` closed the manager.
const EXITING: &str = "hi is exiting; not starting new tasks";

/// Child poll cadence — the bash tool's interval.
const POLL: Duration = Duration::from_millis(50);
/// How long the monitor waits for the reader to hit EOF once the task
/// is over (the `DRAIN_GRACE` of the bash tool).
const READER_GRACE: Duration = Duration::from_millis(500);

/// Retention policy for the tasks dir (#74), applied once at manager
/// construction: task logs are post-mortem artifacts with a bounded
/// lifetime, not an archive.
///
/// - [`TASK_LOG_MAX_AGE`]: logs older than this are deleted.
/// - [`TASK_LOG_CEIL_BYTES`]: among the aged survivors, keep only the
///   newest this many bytes in total.
/// - [`TASK_LOG_CEIL_GRACE`]: the byte ceiling never touches logs
///   younger than this, so a concurrent run's live log cannot be
///   evicted by another instance's startup.
///
/// Deliberately constants, not config: the settings surface (and the
/// pi-parity question behind it) belongs to #23, which covers session
/// files the same way. Errors never block startup; the prune reports
/// one stderr line only when it removed something.
pub const TASK_LOG_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);
pub const TASK_LOG_CEIL_BYTES: usize = 64 * 1024 * 1024;
pub const TASK_LOG_CEIL_GRACE: Duration = Duration::from_secs(24 * 3600);

/// Whether `name` is one of our task logs: the legacy `<id>.log`, or
/// `<pid>-<millis>-<seq>-<id>.log` (a per-run prefix, #188) — every
/// part numeric. A user's stray `notes.log` never matches.
fn is_task_log_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".log") else {
        return false;
    };
    let parts: Vec<&str> = stem.split('-').collect();
    matches!(parts.len(), 1 | 4) && parts.iter().all(|p| p.parse::<u64>().is_ok())
}

/// Delete task logs per the retention policy: task-log files older
/// than `max_age`, then (among files older than `grace`) the oldest
/// beyond a total of `ceil_bytes`, newest kept. Returns how many
/// files were removed; every error is swallowed — pruning is
/// best-effort by ruling and must never block startup.
fn prune_logs(dir: &Path, max_age: Duration, ceil_bytes: usize, grace: Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let logs: Vec<(PathBuf, std::time::SystemTime, u64)> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            // Only our own logs (legacy `<id>.log` or the per-run
            // `<run>-<id>.log`). A user's stray notes.log is never
            // touched.
            if !is_task_log_name(&name) {
                return None;
            }
            let Ok(meta) = e.metadata() else {
                return None;
            };
            if !meta.is_file() {
                return None;
            }
            let mtime = meta.modified().ok()?;
            Some((dir.join(name.as_ref()), mtime, meta.len()))
        })
        .collect();
    let mut removed = 0;
    let mut survivors = Vec::new();
    for (path, mtime, size) in logs {
        // Fail-safe: a future mtime (clock skew) counts as fresh and
        // grace-exempt.
        let aged = mtime.elapsed().is_ok_and(|d| d > max_age);
        if aged {
            if std::fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        } else {
            let exempt = mtime.elapsed().map(|d| d < grace).unwrap_or(true);
            if !exempt {
                survivors.push((path, mtime, size));
            }
        }
    }
    // Byte ceiling over the grace-aged survivors, newest kept first.
    survivors.sort_by(|a, b| b.1.cmp(&a.1).then(b.0.cmp(&a.0)));
    let mut total = 0usize;
    for (path, _, size) in survivors {
        if total + size as usize <= ceil_bytes {
            total += size as usize;
        } else if std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

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
    /// Extra environment for this one command, merged over the
    /// inherited environment (listed keys set or overridden,
    /// everything else passes through). `None` inherits untouched.
    pub env: Option<BTreeMap<String, String>>,
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
                env: None,
            }),
            timeout: None,
        }
    }

    /// Extra environment for this task, merged over the inherited
    /// environment. Additive: an override on top of `process()`.
    pub fn with_env(mut self, env: BTreeMap<String, String>) -> Self {
        let TaskKind::Process(spec) = &mut self.kind;
        spec.env = Some(env);
        self
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
        // Poison-tolerant: `guarded` pushes from a panic handler.
        let mut ring = self.0.lock().unwrap_or_else(|e| e.into_inner());
        ring.extend(bytes.iter().copied());
        let overflow = ring.len().saturating_sub(RING_BYTES);
        drop(ring.drain(..overflow));
    }

    /// The last `max_bytes` bytes, lossily decoded (a split multibyte
    /// char at the seam becomes a replacement char — display-only).
    fn tail(&self, max_bytes: usize) -> String {
        let ring = self.0.lock().unwrap_or_else(|e| e.into_inner());
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
    /// Total runtime, fixed when the task leaves `Running` — a
    /// finished task's wall must stop growing.
    finished_wall: Option<Duration>,
    ring: Ring,
    /// Set by `cancel`; the worker's poll loop observes it.
    cancel: Arc<std::sync::atomic::AtomicBool>,
}

struct Inner {
    next_id: AtomicU64,
    /// This manager's log-file prefix, `<pid>-<millis>-<seq>`, fixed at
    /// construction: every run (and every manager in one process) owns
    /// its own files, so a second run never truncates this one's logs
    /// (#188). Task ids stay 1, 2, 3 per manager.
    run: String,
    tasks: Arc<Mutex<HashMap<u64, TaskEntry>>>,
    tx: Arc<Mutex<Sender<TaskCompletion>>>,
    completions: Mutex<Receiver<TaskCompletion>>,
    logs_dir: PathBuf,
    /// Ids already swept by `shutdown`: a second sweep skips them, so
    /// it does no extra signalling or waiting while workers are still
    /// reaping the groups the first one killed.
    swept: Mutex<std::collections::HashSet<u64>>,
    /// Set by `shutdown` under the `tasks` lock; `spawn` refuses once
    /// set, and re-checks it under the same lock as the insert.
    closed: std::sync::atomic::AtomicBool,
}

/// A cloneable handle to the background task manager. `shutdown` (or
/// dropping the last handle) kills every running task's process group.
#[derive(Clone)]
pub struct TaskManager(Arc<Inner>);

impl TaskManager {
    /// Logs land in `logs_dir/<run>-<id>.log` (created on demand, with
    /// `create_new`: a collision fails the spawn, never truncates). A
    /// best-effort retention prune (#74) runs first — before anything
    /// can spawn — and reports one stderr line only if it removed
    /// files; a failed unlink never blocks startup.
    pub fn new(logs_dir: impl Into<PathBuf>) -> Self {
        let logs_dir = logs_dir.into();
        let removed = prune_logs(
            &logs_dir,
            TASK_LOG_MAX_AGE,
            TASK_LOG_CEIL_BYTES,
            TASK_LOG_CEIL_GRACE,
        );
        if removed > 0 {
            eprintln!(
                "task log prune: removed {removed} file(s) from {}",
                logs_dir.display()
            );
        }
        let (tx, rx) = channel();
        static NEXT_RUN: AtomicU64 = AtomicU64::new(0);
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let run = format!(
            "{}-{millis}-{}",
            std::process::id(),
            NEXT_RUN.fetch_add(1, Ordering::Relaxed)
        );
        Self(Arc::new(Inner {
            next_id: AtomicU64::new(1),
            run,
            tasks: Arc::new(Mutex::new(HashMap::new())),
            tx: Arc::new(Mutex::new(tx)),
            completions: Mutex::new(rx),
            logs_dir,
            swept: Mutex::new(Default::default()),
            closed: std::sync::atomic::AtomicBool::new(false),
        }))
    }

    /// Spawn a task and return immediately with its id. The child is
    /// spawned synchronously (so spawn errors come back as `Err`),
    /// then handed to a worker thread that owns the lifecycle.
    pub fn spawn(&self, spec: TaskSpec) -> Result<u64, String> {
        if self.0.closed.load(Ordering::SeqCst) {
            return Err(EXITING.into());
        }
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
        let mut child = bash_command(&process)
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
        let log_path = self.0.logs_dir.join(format!("{}-{id}.log", self.0.run));
        // `create_new`: never truncate another run's log. Any other
        // failure (read-only dir, …) keeps the old tolerance — the
        // task still runs, ring-only.
        let log = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&log_path)
        {
            Ok(file) => Some(file),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                kill_group(child.id(), libc::SIGKILL);
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "task log {} already exists — refusing to overwrite it",
                    log_path.display()
                ));
            }
            Err(_) => None,
        };

        let ring = Ring::new();
        let entry = TaskEntry {
            name: spec.name.clone(),
            command: process.command.clone(),
            state: TaskState::Running,
            pid: child.id(),
            started: Instant::now(),
            finished_wall: None,
            ring: ring.clone(),
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let cancel = entry.cancel.clone();
        {
            // `shutdown` closes under this lock: the insert either
            // lands before its snapshot (and is swept) or sees the flag
            // here and kills its own child — no window in between.
            let mut tasks = self.0.tasks.lock().unwrap();
            if self.0.closed.load(Ordering::SeqCst) {
                drop(tasks);
                kill_group(child.id(), libc::SIGKILL);
                let _ = child.wait();
                return Err(EXITING.into());
            }
            tasks.insert(id, entry);
        }

        let tasks = Arc::clone(&self.0.tasks);
        let tx = Arc::clone(&self.0.tx);
        let pid = child.id();
        let sent = Arc::new(std::sync::atomic::AtomicBool::new(false));
        std::thread::Builder::new()
            .name(format!("hi-task-{id}"))
            .spawn(move || {
                let name = spec.name.clone();
                let (tasks_for_guard, tx_for_guard) = (Arc::clone(&tasks), Arc::clone(&tx));
                let sent_for_guard = Arc::clone(&sent);
                guarded(
                    id,
                    name,
                    pid,
                    &tasks_for_guard,
                    &tx_for_guard,
                    &sent_for_guard,
                    move || {
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
                            sent,
                        );
                    },
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

impl TaskManager {
    /// Kill every running task's process group — TERM, a grace beat,
    /// KILL — regardless of how many handles exist (#175). Idempotent:
    /// groups an earlier call already swept are skipped, and with
    /// nothing to sweep it returns at once without sleeping. Workers
    /// may still be polling; they observe the exit and finish on their
    /// own, and their completion sends land in a channel nobody reads.
    pub fn shutdown(&self) {
        let running: Vec<(u64, u32)> = {
            let tasks = self.0.tasks.lock().unwrap();
            self.0.closed.store(true, Ordering::SeqCst);
            tasks
                .iter()
                .filter(|(_, e)| !e.state.finished())
                .map(|(id, e)| (*id, e.pid))
                .collect()
        };
        let mut swept = self.0.swept.lock().unwrap();
        let pids: Vec<u32> = running
            .into_iter()
            .filter(|(id, _)| swept.insert(*id))
            .map(|(_, pid)| pid)
            .collect();
        drop(swept);
        crate::process::terminate_groups(&pids, crate::process::EXIT_GRACE);
    }
}

impl Drop for TaskManager {
    fn drop(&mut self) {
        // Last handle out kills the lights.
        if Arc::strong_count(&self.0) == 1 {
            self.shutdown();
        }
    }
}

/// The `bash -c "exec 2>&1; …"` wrapper every process task runs under.
fn bash_command(spec: &ProcessSpec) -> Command {
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(format!("exec 2>&1; {}", spec.command))
        .current_dir(&spec.cwd);
    if let Some(env) = &spec.env {
        // Merge, not replace: `Command::envs` sets or overrides the
        // listed keys and leaves the rest of the inherited
        // environment alone.
        cmd.envs(env);
    }
    crate::process::own_group(&mut cmd);
    cmd
}

/// Run a task's `monitor` so that a panic in it cannot leave the task
/// `Running` forever with no completion (#219). Only the monitor moves
/// a task out of `Running`, and under the TUI the panic message is
/// silenced off the UI thread, so nothing else would ever say it
/// died. On a panic:
///
/// - a task still `Running` has its group killed (never before the
///   check: a finished task's leader is reaped and its pgid may be
///   recycled), becomes `Failed` (exit code -1, as for a signal
///   death) and gets a line in its output saying why;
/// - the one completion notice goes out unless the monitor already
///   sent it (`sent`, set right after its send) — including when the
///   panic came after the state flipped but before the send.
///
/// Everything here tolerates poisoned locks: it runs in a panic's wake.
fn guarded(
    id: u64,
    name: String,
    pid: u32,
    tasks: &Arc<Mutex<HashMap<u64, TaskEntry>>>,
    tx: &Arc<Mutex<Sender<TaskCompletion>>>,
    sent: &std::sync::atomic::AtomicBool,
    body: impl FnOnce(),
) {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)).is_ok() {
        return;
    }
    let (state, wall) = {
        let mut tasks = tasks.lock().unwrap_or_else(|e| e.into_inner());
        let Some(entry) = tasks.get_mut(&id) else {
            return;
        };
        if !entry.state.finished() {
            kill_group(pid, libc::SIGKILL);
            let wall = entry.started.elapsed();
            entry.state = TaskState::Failed { exit_code: -1 };
            entry.finished_wall = Some(wall);
            entry.ring.push(
                b"\n[hi: the task monitor panicked; the task was killed and marked failed]\n",
            );
        }
        let wall = entry
            .finished_wall
            .unwrap_or_else(|| entry.started.elapsed());
        (entry.state, wall)
    };
    if !sent.swap(true, Ordering::SeqCst) {
        let _ = tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send(TaskCompletion {
                id,
                name,
                state,
                wall,
            });
    }
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
    log: Option<std::fs::File>,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    timeout: Option<Duration>,
    tasks: Arc<Mutex<HashMap<u64, TaskEntry>>>,
    tx: Arc<Mutex<Sender<TaskCompletion>>>,
    sent: Arc<std::sync::atomic::AtomicBool>,
) {
    let started = Instant::now();
    // Unrepresentable timeout = no deadline (#172); `+` would panic the
    // monitor and leave the task `Running` forever.
    let deadline = timeout.and_then(|t| started.checked_add(t));
    // Reader: pipe → ring + log file. Ends at EOF (every holder gone).
    let reader = std::thread::spawn(move || {
        let mut pipe = stdout;
        let mut file = log;
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

    // A task lives until its whole group is gone (#174, AD-0019
    // addendum): the leader's exit is remembered, not final. Cancel and
    // the deadline stay honoured while any member survives.
    let pid = child.id();
    let mut leader: Option<TaskState> = None;
    let state = loop {
        if leader.is_none() {
            match child.try_wait() {
                Ok(Some(status)) => leader = Some(exit_state(status)),
                Ok(None) => {}
                Err(_) => break TaskState::Failed { exit_code: -1 },
            }
        }
        // The leader is reaped here, so a probe sees only live members.
        if let Some(state) = leader
            && !crate::process::group_alive(pid)
        {
            break state;
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
    // Bounded wait for the reader's EOF: something that left the group
    // (`setsid`, `set -m`) can hold the pipe and must not wedge the
    // task. A late reader is detached and keeps draining to EOF.
    let grace_end = Instant::now() + READER_GRACE;
    while !reader.is_finished() && Instant::now() < grace_end {
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(reader);
    let wall = started.elapsed();

    if let Some(entry) = tasks.lock().unwrap().get_mut(&id) {
        entry.state = state;
        entry.finished_wall = Some(wall);
    }
    // A dead receiver just means nobody is listening (shutdown) — the
    // registry row above is the durable fact.
    let _ = tx.lock().unwrap().send(TaskCompletion {
        id,
        name,
        state,
        wall,
    });
    // Right after the send: `guarded` sends the notice itself when a
    // panic got in between (#219).
    sent.store(true, Ordering::SeqCst);
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

fn summary(id: u64, entry: &TaskEntry) -> TaskSummary {
    TaskSummary {
        id,
        name: entry.name.clone(),
        command: entry.command.clone(),
        state: entry.state,
        pid: entry.pid,
        wall: entry
            .finished_wall
            .unwrap_or_else(|| entry.started.elapsed()),
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

    /// Backdate a file's mtime for retention tests (std `FileTimes`).
    fn backdate(path: &Path, age: Duration) {
        let f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::now() - age))
            .unwrap();
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
    fn a_monitor_panic_after_the_state_flip_still_sends_one_notice_and_kills_nothing() {
        // #219 review: the monitor set the state, then panicked before
        // its send. The notice must still go out exactly once, and the
        // (reaped, possibly recycled) group must not be signalled.
        let (m, _dir) = manager();
        let mut child = bash_command(&ProcessSpec {
            command: "sleep 30".into(),
            cwd: std::env::temp_dir(),
            env: Default::default(),
        })
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
        let pid = child.id();
        let ring = Ring::new();
        m.0.tasks.lock().unwrap().insert(
            7,
            TaskEntry {
                name: "done".into(),
                command: "sleep 30".into(),
                state: TaskState::Succeeded { exit_code: 0 },
                pid,
                started: Instant::now(),
                finished_wall: Some(Duration::from_millis(5)),
                ring: ring.clone(),
                cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
        );
        // A poisoned ring must not turn the handler into a second panic.
        let poison = ring.clone();
        let _ = std::thread::spawn(move || {
            let _guard = poison.0.lock().unwrap();
            panic!("poison the ring");
        })
        .join();
        let sent = std::sync::atomic::AtomicBool::new(false);
        guarded(7, "done".into(), pid, &m.0.tasks, &m.0.tx, &sent, || {
            panic!("after the flip")
        });
        let c = m.try_recv_completion().expect("the notice still goes out");
        assert_eq!(
            (c.id, c.state, c.wall),
            (
                7,
                TaskState::Succeeded { exit_code: 0 },
                Duration::from_millis(5)
            )
        );
        assert!(m.try_recv_completion().is_none(), "exactly one");
        assert!(
            child.try_wait().unwrap().is_none(),
            "the group was not killed"
        );
        // And once the monitor did send, a later panic adds nothing.
        let sent = std::sync::atomic::AtomicBool::new(true);
        guarded(7, "done".into(), pid, &m.0.tasks, &m.0.tx, &sent, || {
            panic!("after the send")
        });
        assert!(m.try_recv_completion().is_none());
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn a_panicking_monitor_fails_the_task_and_still_notifies() {
        // #219: the monitor is the only thing that moves a task out of
        // `Running`; a panic in it left the task running forever.
        let (m, _dir) = manager();
        // A guard-only row: its "monitor" panics straight away.
        let mut child = bash_command(&ProcessSpec {
            command: "sleep 30".into(),
            cwd: std::env::temp_dir(),
            env: Default::default(),
        })
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
        let fake_pid = child.id();
        let ring = Ring::new();
        m.0.tasks.lock().unwrap().insert(
            99,
            TaskEntry {
                name: "doomed".into(),
                command: "sleep 30".into(),
                state: TaskState::Running,
                pid: fake_pid,
                started: Instant::now(),
                finished_wall: None,
                ring: ring.clone(),
                cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
        );
        guarded(
            99,
            "doomed".into(),
            fake_pid,
            &m.0.tasks,
            &m.0.tx,
            &std::sync::atomic::AtomicBool::new(false),
            || panic!("monitor boom"),
        );
        assert_eq!(state_of(&m, 99), TaskState::Failed { exit_code: -1 });
        let c = m.try_recv_completion().expect("one completion notice");
        assert_eq!((c.id, c.state), (99, TaskState::Failed { exit_code: -1 }));
        assert!(m.try_recv_completion().is_none());
        assert!(ring.tail(4096).contains("monitor panicked"));
        // KILLed: the leader reaps, and the group is gone.
        let status = child.wait().unwrap();
        assert!(!status.success(), "the child was killed");
        assert!(!crate::process::group_alive(fake_pid));
    }

    #[test]
    fn a_finished_tasks_wall_stops_growing() {
        let (m, _dir) = manager();
        let id = spawn_script(&m, "true", None);
        until("task to finish", || state_of(&m, id).finished());
        let first = m.status(id).unwrap().wall;
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(m.status(id).unwrap().wall, first, "wall of a finished task");
        assert_eq!(m.list()[0].wall, first);
    }

    #[test]
    fn startup_prune_deletes_old_task_logs() {
        let (manager, dir) = manager();
        std::fs::write(dir.join("1.log"), "old").unwrap();
        backdate(&dir.join("1.log"), Duration::from_secs(8 * 24 * 3600));
        std::fs::write(dir.join("2.log"), "fresh").unwrap();
        drop(manager);
        std::fs::write(dir.join("3.log"), "new").unwrap();
        let pruned = prune_logs(
            &dir,
            TASK_LOG_MAX_AGE,
            TASK_LOG_CEIL_BYTES,
            TASK_LOG_CEIL_GRACE,
        );
        assert_eq!(pruned, 1);
        assert!(!dir.join("1.log").exists());
        assert!(dir.join("2.log").exists());
        assert!(dir.join("3.log").exists());
    }

    /// The logs in `dir` whose content equals `want` — poll-friendly.
    fn logs_with(dir: &Path, want: &str) -> usize {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| is_task_log_name(&e.file_name().to_string_lossy()))
            .filter(|e| std::fs::read_to_string(e.path()).is_ok_and(|c| c.trim() == want))
            .count()
    }

    #[test]
    fn two_managers_on_one_dir_keep_their_own_logs() {
        // #188 (r16): a second run used to truncate the first run's
        // `1.log`. Both tasks get id 1; the files must not collide.
        let (a, dir) = manager();
        let b = TaskManager::new(dir.clone());
        let id_a = a
            .spawn(TaskSpec::process("echo from-run-a", std::env::temp_dir()))
            .unwrap();
        let id_b = b
            .spawn(TaskSpec::process("echo from-run-b", std::env::temp_dir()))
            .unwrap();
        assert_eq!((id_a, id_b), (1, 1), "task ids stay per-run");
        until("both logs", || {
            logs_with(&dir, "from-run-a") == 1 && logs_with(&dir, "from-run-b") == 1
        });
        until("both finished", || {
            state_of(&a, id_a).finished() && state_of(&b, id_b).finished()
        });
        assert_eq!(logs_with(&dir, "from-run-a"), 1);
        assert_eq!(logs_with(&dir, "from-run-b"), 1);
    }

    #[test]
    fn a_colliding_log_name_fails_the_spawn_and_is_not_truncated() {
        let (m, dir) = manager();
        let taken = dir.join(format!("{}-1.log", m.0.run));
        std::fs::write(&taken, "precious").unwrap();
        let err = m
            .spawn(TaskSpec::process("echo x", std::env::temp_dir()))
            .unwrap_err();
        assert!(err.contains("already exists"), "{err}");
        assert_eq!(std::fs::read_to_string(&taken).unwrap(), "precious");
    }

    #[test]
    fn task_log_names_are_ours_only() {
        assert!(is_task_log_name("7.log"));
        assert!(is_task_log_name("123-1731200000000-0-4.log"));
        assert!(!is_task_log_name("notes.log"));
        assert!(!is_task_log_name("1-2.log"));
        assert!(!is_task_log_name("123-1731200000000-0-x.log"));
        assert!(!is_task_log_name("7.txt"));
    }

    #[test]
    fn prune_removes_old_new_style_and_legacy_logs_only() {
        let (manager, dir) = manager();
        drop(manager);
        let old = Duration::from_secs(8 * 24 * 3600);
        for name in ["5.log", "10-20-0-1.log", "notes.log", "10-20-0-x.log"] {
            std::fs::write(dir.join(name), "data").unwrap();
            backdate(&dir.join(name), old);
        }
        std::fs::write(dir.join("10-20-0-2.log"), "fresh").unwrap();
        let pruned = prune_logs(
            &dir,
            TASK_LOG_MAX_AGE,
            TASK_LOG_CEIL_BYTES,
            TASK_LOG_CEIL_GRACE,
        );
        assert_eq!(pruned, 2);
        assert!(!dir.join("5.log").exists());
        assert!(!dir.join("10-20-0-1.log").exists());
        assert!(dir.join("10-20-0-2.log").exists());
        assert!(dir.join("notes.log").exists());
        assert!(dir.join("10-20-0-x.log").exists());
    }

    #[test]
    fn startup_prune_ceiling_keeps_the_newest_aged_logs() {
        let (manager, dir) = manager();
        drop(manager);
        std::fs::write(dir.join("1.log"), vec![b'a'; 4096]).unwrap();
        std::fs::write(dir.join("2.log"), vec![b'b'; 4096]).unwrap();
        backdate(&dir.join("1.log"), Duration::from_secs(2 * 24 * 3600));
        backdate(&dir.join("2.log"), Duration::from_secs(2 * 24 * 3600 + 60));
        // Ceiling 4 KiB: the newest aged log (1.log) survives, the
        // older one goes.
        let pruned = prune_logs(&dir, TASK_LOG_MAX_AGE, 4096, TASK_LOG_CEIL_GRACE);
        assert_eq!(pruned, 1);
        assert!(dir.join("1.log").exists());
        assert!(!dir.join("2.log").exists());
    }

    #[test]
    fn startup_prune_ceiling_never_touches_logs_within_the_grace_window() {
        let (manager, dir) = manager();
        drop(manager);
        std::fs::write(dir.join("1.log"), vec![b'a'; 4096]).unwrap();
        std::fs::write(dir.join("2.log"), vec![b'b'; 4096]).unwrap();
        backdate(&dir.join("1.log"), Duration::from_secs(3600));
        backdate(&dir.join("2.log"), Duration::from_secs(3600 + 60));
        // Both fit the grace window, so the ceiling must not fire even
        // though 8 KiB > 4 KiB.
        let pruned = prune_logs(&dir, TASK_LOG_MAX_AGE, 4096, Duration::from_secs(24 * 3600));
        assert_eq!(pruned, 0);
        assert!(dir.join("1.log").exists());
        assert!(dir.join("2.log").exists());
    }

    #[test]
    fn startup_prune_only_touches_task_log_names() {
        let (manager, dir) = manager();
        drop(manager);
        std::fs::write(dir.join("notes.log"), "user data").unwrap();
        std::fs::write(dir.join("junk.txt"), "user data").unwrap();
        let pruned = prune_logs(
            &dir,
            Duration::ZERO,
            TASK_LOG_CEIL_BYTES,
            TASK_LOG_CEIL_GRACE,
        );
        assert_eq!(pruned, 0);
        assert!(dir.join("notes.log").exists());
        assert!(dir.join("junk.txt").exists());
    }

    #[test]
    fn startup_prune_survives_unlink_errors_and_a_missing_dir() {
        // A directory named like a log cannot be unlinked as a file;
        // the prune must skip it and carry on.
        let (manager, dir) = manager();
        drop(manager);
        std::fs::create_dir_all(dir.join("9.log")).unwrap();
        std::fs::write(dir.join("10.log"), "old").unwrap();
        backdate(&dir.join("10.log"), Duration::from_secs(8 * 24 * 3600));
        let pruned = prune_logs(
            &dir,
            TASK_LOG_MAX_AGE,
            TASK_LOG_CEIL_BYTES,
            TASK_LOG_CEIL_GRACE,
        );
        assert_eq!(pruned, 1);
        assert!(dir.join("9.log").exists());
        // A missing dir is a silent no-op.
        assert_eq!(
            prune_logs(
                &dir.join("nope"),
                TASK_LOG_MAX_AGE,
                TASK_LOG_CEIL_BYTES,
                TASK_LOG_CEIL_GRACE
            ),
            0
        );
    }

    #[test]
    fn process_env_merges_over_the_inherited_environment() {
        let (m, _dir) = manager();
        let spec = TaskSpec::process(
            "echo \"v=$HI_WIRE_TEST p=${PATH:+set}\"",
            std::env::temp_dir(),
        )
        .with_env(BTreeMap::from([(
            "HI_WIRE_TEST".to_string(),
            "wire".to_string(),
        )]));
        let id = m.spawn(spec).unwrap();
        until("the env echo", || state_of(&m, id).finished());
        let out = m.output_tail(id, 4096).unwrap();
        assert!(out.contains("v=wire"), "{out}");
        // The merge is over the inherited environment, not a
        // replacement of it — the child still has PATH.
        assert!(out.contains("p=set"), "inherited env must survive: {out}");
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

    /// Kills a process group on drop, so a failing assertion cannot
    /// leave a survivor behind.
    struct KillGroup(i32);
    impl Drop for KillGroup {
        fn drop(&mut self) {
            unsafe { libc::kill(-self.0, libc::SIGKILL) };
        }
    }

    /// Creates the release file on drop so a failing test frees its
    /// polling survivor too.
    struct ReleaseOnDrop(PathBuf);
    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.0, "");
        }
    }

    fn read_pid(file: &Path) -> i32 {
        let mut pid = 0;
        until("pid file", || {
            pid = std::fs::read_to_string(file)
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0);
            pid != 0
        });
        pid
    }

    fn pid_dead(pid: i32) -> bool {
        // The survivor's parent is gone (reparented to init) or reaped
        // by it; a zombie would still answer kill(0), so also accept it.
        let gone = unsafe { libc::kill(pid, 0) != 0 };
        gone || {
            std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| {
                s.rsplit(')')
                    .next()
                    .is_some_and(|r| r.trim_start().starts_with('Z'))
            })
        }
    }

    fn spawn_script(m: &TaskManager, script: &str, timeout: Option<Duration>) -> u64 {
        m.spawn(TaskSpec {
            timeout,
            ..TaskSpec::process(script, std::env::temp_dir())
        })
        .unwrap()
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hi-174-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Script prefix: announce the background sleep, mark the exit.
    fn survivor_script(dir: &Path, tail: &str) -> String {
        format!(
            "sleep 30 & echo $! > {p}; touch {m}; {tail}",
            p = dir.join("pid").display(),
            m = dir.join("marker").display(),
        )
    }

    #[test]
    fn a_background_child_keeps_the_task_running_until_cancelled() {
        let (m, _d) = manager();
        let dir = scratch("cancel");
        let id = spawn_script(&m, &survivor_script(&dir, "exit 0"), None);
        let pid = read_pid(&dir.join("pid"));
        let _guard = KillGroup(m.status(id).unwrap().pid as i32);
        until("leader to exit", || dir.join("marker").exists());
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(state_of(&m, id), TaskState::Running);
        assert!(m.try_recv_completion().is_none());
        let t = Instant::now();
        m.cancel(id).unwrap();
        until("cancel to land", || state_of(&m, id).finished());
        assert_eq!(state_of(&m, id), TaskState::Killed);
        assert!(t.elapsed() < Duration::from_secs(10));
        until("survivor to die", || pid_dead(pid));
    }

    #[test]
    fn a_background_child_is_killed_by_the_timeout_after_the_leader_exited() {
        let (m, _d) = manager();
        let dir = scratch("timeout");
        let id = spawn_script(
            &m,
            &survivor_script(&dir, "exit 0"),
            Some(Duration::from_millis(1500)),
        );
        let pid = read_pid(&dir.join("pid"));
        let _guard = KillGroup(m.status(id).unwrap().pid as i32);
        until("timeout to land", || state_of(&m, id).finished());
        assert_eq!(state_of(&m, id), TaskState::TimedOut);
        until("survivor to die", || pid_dead(pid));
    }

    #[test]
    fn a_short_lived_survivor_finishes_the_task_with_the_leaders_state() {
        let (m, _d) = manager();
        let dir = scratch("short");
        let release = dir.join("release");
        let script = format!(
            "(while [ ! -e {r} ]; do sleep 0.02; done) & touch {m}; exit 3",
            r = release.display(),
            m = dir.join("marker").display()
        );
        let spawned = Instant::now();
        let id = spawn_script(&m, &script, None);
        let _guard = KillGroup(m.status(id).unwrap().pid as i32);
        let _release = ReleaseOnDrop(release.clone());
        until("leader to exit", || dir.join("marker").exists());
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(state_of(&m, id), TaskState::Running, "survivor still alive");
        assert!(m.try_recv_completion().is_none());
        let held = spawned.elapsed();
        std::fs::write(&release, "").unwrap();
        until("task to finish", || state_of(&m, id).finished());
        assert_eq!(state_of(&m, id), TaskState::Failed { exit_code: 3 });
        let c = m.try_recv_completion().unwrap();
        assert_eq!((c.id, c.state), (id, TaskState::Failed { exit_code: 3 }));
        assert!(c.wall >= held, "wall {:?} < held {held:?}", c.wall);
        std::thread::sleep(Duration::from_millis(200));
        assert!(m.try_recv_completion().is_none(), "exactly one notice");
    }

    #[test]
    fn a_survivors_output_after_the_leader_exited_reaches_ring_and_log() {
        let (m, dir) = manager();
        let sdir = scratch("late");
        let release = sdir.join("release");
        let script = format!(
            "(while [ ! -e {r} ]; do sleep 0.02; done; echo late-words) & exit 0",
            r = release.display()
        );
        let id = spawn_script(&m, &script, None);
        let leader = m.status(id).unwrap().pid as i32;
        let _guard = KillGroup(leader);
        let _release = ReleaseOnDrop(release.clone());
        // Gone (reaped by the monitor) while the survivor still waits:
        // the line below can only come from a task past its leader.
        until("leader to be reaped", || pid_dead(leader));
        assert_eq!(state_of(&m, id), TaskState::Running);
        assert!(!m.output_tail(id, 4096).unwrap().contains("late-words"));
        std::fs::write(&release, "").unwrap();
        until("task to finish", || state_of(&m, id).finished());
        assert_eq!(state_of(&m, id), TaskState::Succeeded { exit_code: 0 });
        let log = std::fs::read_to_string(dir.join(format!("{}-{id}.log", m.0.run))).unwrap();
        assert!(log.contains("late-words"), "log: {log}");
        assert!(m.output_tail(id, 4096).unwrap().contains("late-words"));
    }

    #[test]
    fn a_process_that_left_the_group_cannot_wedge_the_task() {
        let (m, _d) = manager();
        let dir = scratch("escape");
        // `set -m` gives background jobs their own process group.
        let script = format!("set -m; {}", survivor_script(&dir, "exit 0"));
        let id = spawn_script(&m, &script, None);
        let pid = read_pid(&dir.join("pid"));
        let _leader = KillGroup(m.status(id).unwrap().pid as i32);
        let _escapee = KillGroup(pid);
        let leader_pid = m.status(id).unwrap().pid as i32;
        assert_eq!(unsafe { libc::getpgid(pid) }, pid, "own group");
        assert_ne!(pid, leader_pid);
        until("task to finish despite the held pipe", || {
            state_of(&m, id).finished()
        });
        assert_eq!(state_of(&m, id), TaskState::Succeeded { exit_code: 0 });
        // Not tracked, not killed.
        assert!(!pid_dead(pid));
    }

    #[test]
    fn a_task_holding_an_escaped_foreground_job_is_still_cancellable() {
        let (m, _d) = manager();
        let dir = scratch("escape-fg");
        // The foreground job gets its own group too; the leader waits on it.
        let script = format!(
            "set -m; sh -c 'echo $$ > {p}; exec sleep 30'; echo after",
            p = dir.join("pid").display()
        );
        let id = spawn_script(&m, &script, None);
        let pid = read_pid(&dir.join("pid"));
        let leader_pid = m.status(id).unwrap().pid as i32;
        let _leader = KillGroup(leader_pid);
        let _escapee = KillGroup(pid);
        assert_eq!(unsafe { libc::getpgid(pid) }, pid, "own group");
        assert_ne!(pid, leader_pid);
        assert_eq!(state_of(&m, id), TaskState::Running);
        m.cancel(id).unwrap();
        until("cancel to land", || state_of(&m, id).finished());
        assert_eq!(state_of(&m, id), TaskState::Killed);
    }

    #[test]
    fn dropping_the_manager_sweeps_a_group_whose_leader_already_exited() {
        let (m, _d) = manager();
        let dir = scratch("drop");
        let id = spawn_script(&m, &survivor_script(&dir, "exit 0"), None);
        let pid = read_pid(&dir.join("pid"));
        let _guard = KillGroup(m.status(id).unwrap().pid as i32);
        until("leader to exit", || dir.join("marker").exists());
        drop(m);
        until("survivor to die", || pid_dead(pid));
    }

    #[test]
    fn an_unrepresentable_timeout_means_no_deadline() {
        // #172: `started + Duration::MAX` panicked the monitor thread
        // and left the task `Running` forever. It must reach a final state.
        let (m, _dir) = manager();
        let id = m
            .spawn(TaskSpec {
                timeout: Some(Duration::from_secs(u64::MAX)),
                ..TaskSpec::process("echo done", std::env::temp_dir())
            })
            .unwrap();
        until("the task to finish", || state_of(&m, id).finished());
        assert!(matches!(state_of(&m, id), TaskState::Succeeded { .. }));
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
        let log = std::fs::read(dir.join(format!("{}-{id}.log", m.0.run))).unwrap();
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
    fn shutdown_kills_running_tasks_while_another_handle_lives() {
        let (m, _dir) = manager();
        let other = m.clone(); // a worker thread's clone at a mid-turn quit
        let id = m
            .spawn(TaskSpec::process("sleep 30", std::env::temp_dir()))
            .unwrap();
        let pid = m.status(id).unwrap().pid;
        // Two handles stay alive across the call: no last-handle help.
        m.shutdown();
        until("group to die on shutdown", || group_dead(pid));
        drop(other);
        drop(m);
    }

    #[test]
    fn a_spawn_after_shutdown_is_refused_and_starts_nothing() {
        let (m, dir) = manager();
        m.shutdown();
        let pidf = dir.join("late.pid");
        let err = m
            .spawn(TaskSpec::process(
                format!("echo $$ > {}; sleep 30", pidf.display()),
                std::env::temp_dir(),
            ))
            .unwrap_err();
        assert!(err.contains("exiting"), "{err}");
        std::thread::sleep(Duration::from_millis(300));
        assert!(!pidf.exists(), "a process started after shutdown");
        assert_eq!(m.running_count(), 0);
    }

    #[test]
    fn shutdown_is_idempotent_and_free_when_nothing_runs() {
        let (empty, _d) = manager();
        let started = Instant::now();
        empty.shutdown(); // nothing running: no sleep
        assert!(started.elapsed() < Duration::from_millis(100));
        let (m, _dir) = manager();
        let id = m
            .spawn(TaskSpec::process("sleep 30", std::env::temp_dir()))
            .unwrap();
        let pid = m.status(id).unwrap().pid;
        m.shutdown();
        until("group to die", || group_dead(pid));
        let started = Instant::now();
        m.shutdown();
        drop(m); // Drop after shutdown: still nothing to do
        assert!(started.elapsed() < Duration::from_millis(100));
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
