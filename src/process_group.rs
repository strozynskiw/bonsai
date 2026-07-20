//! Run shell children in their own process group and signal the whole group, so
//! a timed-out or stopped command takes its descendants down with it instead of
//! leaking them. Foreground bash and background tasks share this so neither can
//! orphan a process the shell forked. On non-unix there is no process group, so
//! signalling falls back to the direct child.

use tokio::process::{Child, Command};

/// Synchronous cancellation backstop for an asynchronously supervised child.
///
/// Owners should keep this armed from spawn until every child/pipe task has
/// completed. Dropping the supervising future then kills the Unix process
/// group even though async cleanup can no longer run. `kill_on_drop` remains
/// the direct-child fallback on platforms without process groups.
#[derive(Debug)]
pub(crate) struct ProcessGroupGuard {
    pid: Option<u32>,
    armed: bool,
}

impl ProcessGroupGuard {
    /// Create an armed guard for the child process group identified by `pid`.
    #[must_use]
    pub(crate) fn new(pid: Option<u32>) -> Self {
        Self { pid, armed: true }
    }

    /// Keep the successfully supervised process group alive when this guard drops.
    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            force_kill_group(pid);
        }
    }
}

/// Put the spawned child in a new process group (its pid becomes the group id),
/// so a later `killpg` reaches every descendant it forks. Must be called before
/// `spawn`. No-op on non-unix.
#[cfg(unix)]
pub(crate) fn configure(command: &mut Command) {
    command.process_group(0);
}

#[cfg(not(unix))]
pub(crate) fn configure(_command: &mut Command) {}

/// Send `SIGTERM` to the child's whole process group, giving it a chance to exit
/// cleanly. No-op on non-unix (there is no group; callers escalate to
/// [`force_kill`], which kills the child directly).
#[cfg(unix)]
pub(crate) fn terminate_group(pid: u32) {
    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;

    let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGTERM);
}

#[cfg(not(unix))]
pub(crate) fn terminate_group(_pid: u32) {}

/// Send `SIGKILL` to a process group by id. Used as a synchronous drop backstop
/// when an async owner is aborted before it can run the normal graceful stop
/// path.
#[cfg(unix)]
pub(crate) fn force_kill_group(pid: u32) {
    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;

    let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGKILL);
}

/// `SIGKILL` the child's whole process group (unix), or kill just the child as a
/// fallback when the pid is gone or the platform has no groups.
pub(crate) async fn force_kill(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        force_kill_group(pid);
        return;
    }

    let _ = child.kill().await;
}
