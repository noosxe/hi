//! The one exit path (#175): everything `hi` started and may leave
//! behind dies here. Every front end routes every way out through
//! [`shutdown`] (or an [`ExitGuard`], which calls it on drop, so early
//! returns, error exits and unwinding are covered too):
//!
//! - the background task manager's running groups
//!   (`TaskManager::shutdown`), regardless of handle count;
//! - the live foreground `bash` groups (`BashGroups::kill_all` on the
//!   process-wide registry) — a call abandoned by Ctrl-C, or by Esc
//!   followed by a quit.
//!
//! The sweep also **closes** both registries: a `task_run` or `bash`
//! call the still-running agent thread tries to start afterwards is
//! refused ("hi is exiting") and spawns nothing, so nothing slips in
//! behind the sweep.
//!
//! Out of scope: Esc alone does not kill an abandoned `bash` call (the
//! guarantee covers exits), members that left their group (`setsid`),
//! and Windows (#31). Call it after the terminal is restored — the
//! sweep may wait a fraction of a second for TERM to take.

use crate::process::BashGroups;
use crate::tasks::TaskManager;

/// Kill every task group and every live foreground `bash` group.
/// Idempotent; with nothing running it neither signals nor waits.
pub fn shutdown(tasks: Option<&TaskManager>) {
    if let Some(tasks) = tasks {
        tasks.shutdown();
    }
    BashGroups::global().kill_all();
}

/// Runs [`shutdown`] when dropped.
pub struct ExitGuard(Option<TaskManager>);

impl ExitGuard {
    pub fn new(tasks: Option<TaskManager>) -> Self {
        Self(tasks)
    }
}

impl Drop for ExitGuard {
    fn drop(&mut self) {
        shutdown(self.0.as_ref());
    }
}
