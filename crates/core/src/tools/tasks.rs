//! `task_run` / `task_status` / `task_output` / `task_cancel` — the
//! model's window onto the background task manager (#5, AD-0019).
//!
//! The tools are thin: they parse arguments, call the
//! [`TaskManager`](crate::tasks::TaskManager), and shape the facts as
//! content. `task_run` returns immediately — a spawn echo with the id —
//! and the model polls `task_status` / `task_output` in later rounds;
//! nothing here blocks on a task, that is the entire point. All four go
//! through the ordinary [`ToolGate`](crate::tool::ToolGate) like every
//! other tool — #5 adds no interception seam.
//!
//! Output reads are clamped by the shared truncation contract on top of
//! the manager's own 256 KiB ring, so no read can flood the context.

use super::truncate;
use super::{args_object, opt_str, opt_usize};
use crate::tasks::{TaskManager, TaskSpec};
use crate::tool::{Tool, ToolAnnotations};
use std::path::PathBuf;
use std::time::Duration;

/// Format one summary line — the shared shape across status tools.
fn summarize(summary: &crate::tasks::TaskSummary) -> String {
    let wall = crate::tasks::format_duration(summary.wall);
    match summary.state {
        crate::tasks::TaskState::Running => {
            format!(
                "#{id} '{name}' running ({wall} so far) — {command}",
                id = summary.id,
                name = summary.name,
                command = summary.command
            )
        }
        state => format!(
            "#{id} '{name}' {state} after {wall} — {command}",
            id = summary.id,
            name = summary.name,
            state = state.label(),
            command = summary.command,
        ),
    }
}

/// Parse the required task id, shared by three of the four tools.
fn task_id(arguments: &str) -> Result<u64, String> {
    let args = args_object(arguments)?;
    opt_usize(&args, "id")?
        .map(|v| v as u64)
        .ok_or_else(|| "task tools need an id — list them with task_status".into())
}

/// `task_run` — start long-lived work and return at once.
pub struct TaskRunTool {
    tasks: TaskManager,
    cwd: PathBuf,
}

impl TaskRunTool {
    pub fn new(tasks: TaskManager, cwd: impl Into<PathBuf>) -> Self {
        Self {
            tasks,
            cwd: cwd.into(),
        }
    }
}

impl Tool for TaskRunTool {
    fn name(&self) -> &str {
        "task_run"
    }

    fn description(&self) -> &str {
        "Start a long-lived background command (dev server, watcher, test suite, build) \
         in its own process group and return immediately with a task id. It keeps running \
         across turns. Poll with task_status, read output with task_output (tail), stop \
         with task_cancel. Optionally provide a timeout in seconds (no default — \
         background work is expected to be long-lived)."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The bash command to run in the background"
                },
                "name": {
                    "type": "string",
                    "description": "Short human label (default: the command's first two words)"
                },
                "timeout": {
                    "type": "number",
                    "description": "Wall-clock limit in seconds (optional, no default)"
                }
            },
            "required": ["command"]
        })
    }

    fn prompt_note(&self) -> Option<String> {
        Some("task_run starts work that outlives the turn — spawn a dev server or suite, keep answering, check task_status/task_output later instead of blocking; task_cancel stops it.".into())
    }

    fn execute(&self, arguments: &str) -> Result<String, String> {
        let args = args_object(arguments)?;
        let Some(command) = opt_str(&args, "command")? else {
            return Err("task_run needs a command".into());
        };
        let name = opt_str(&args, "name")?;
        let timeout = opt_usize(&args, "timeout")?.map(|s| Duration::from_secs(s as u64));
        let mut spec = TaskSpec::process(command, self.cwd.clone());
        if let Some(name) = name {
            spec.name = name;
        }
        spec.timeout = timeout;
        let id = self.tasks.spawn(spec)?;
        Ok(format!(
            "started background task #{id} — poll with task_status, read output with \
             task_output {{\"id\":{id}}}, stop with task_cancel {{\"id\":{id}}}"
        ))
    }
}

/// `task_status` — one task or all of them, point in time.
pub struct TaskStatusTool {
    tasks: TaskManager,
}

impl TaskStatusTool {
    pub fn new(tasks: TaskManager) -> Self {
        Self { tasks }
    }
}

impl Tool for TaskStatusTool {
    fn name(&self) -> &str {
        "task_status"
    }

    fn description(&self) -> &str {
        "Point-in-time status of background tasks: all of them, or one by id. \
         Reports state (running / finished / failed / cancelled / timed out), \
         wall time, and the command."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "number",
                    "description": "One task's id (omit to list every task)"
                }
            }
        })
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }

    fn execute(&self, arguments: &str) -> Result<String, String> {
        let args = args_object(arguments)?;
        match opt_usize(&args, "id")? {
            Some(id) => self
                .tasks
                .status(id as u64)
                .map(|s| summarize(&s))
                .ok_or_else(|| format!("no such task #{id}")),
            None => {
                let list = self.tasks.list();
                if list.is_empty() {
                    return Ok("no background tasks this session".into());
                }
                Ok(list.iter().map(summarize).collect::<Vec<_>>().join("\n"))
            }
        }
    }
}

/// `task_output` — the tail of a task's output so far.
pub struct TaskOutputTool {
    tasks: TaskManager,
}

impl TaskOutputTool {
    pub fn new(tasks: TaskManager) -> Self {
        Self { tasks }
    }
}

impl Tool for TaskOutputTool {
    fn name(&self) -> &str {
        "task_output"
    }

    fn description(&self) -> &str {
        "Read a background task's output so far — the tail, interleaved stdout+stderr \
         as a terminal would show it, clamped to 2000 lines / 50KB. Works while the \
         task is still running. The full stream also lands in the task's log file."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "number",
                    "description": "The task's id (from task_run or task_status)"
                },
                "max_bytes": {
                    "type": "number",
                    "description": "Read at most this many bytes of tail (default 50KB)"
                }
            },
            "required": ["id"]
        })
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }

    fn execute(&self, arguments: &str) -> Result<String, String> {
        let args = args_object(arguments)?;
        let id = task_id(arguments)?;
        let max = opt_usize(&args, "max_bytes")?.unwrap_or(truncate::MAX_BYTES);
        let Some(raw) = self.tasks.output_tail(id, max) else {
            return Err(format!("no such task #{id}"));
        };
        if raw.trim().is_empty() {
            return Ok(format!("task #{id} has produced no output yet"));
        }
        let clip = truncate::tail(&raw);
        let note = clip.note(true);
        let mut text = clip.text;
        if let Some(note) = note {
            text.push('\n');
            text.push_str(&note);
        }
        Ok(text)
    }
}

/// `task_cancel` — kill a task's whole process group.
pub struct TaskCancelTool {
    tasks: TaskManager,
}

impl TaskCancelTool {
    pub fn new(tasks: TaskManager) -> Self {
        Self { tasks }
    }
}

impl Tool for TaskCancelTool {
    fn name(&self) -> &str {
        "task_cancel"
    }

    fn description(&self) -> &str {
        "Stop a background task: kills its whole process group (grandchildren included), \
         TERM first, then KILL. The task reports as cancelled."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "number",
                    "description": "The task's id (from task_run or task_status)"
                }
            },
            "required": ["id"]
        })
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: false,
            destructive: true,
        }
    }

    fn execute(&self, arguments: &str) -> Result<String, String> {
        let id = task_id(arguments)?;
        self.tasks.cancel(id)?;
        Ok(format!(
            "task #{id} cancelled — the process group is being killed"
        ))
    }
}
