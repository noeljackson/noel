//! Isolation for tool subprocesses (bash and extension tools).
//!
//! A tool subprocess starts a new session: it leads a fresh process group and
//! has no controlling terminal. A command that reads or opens the terminal
//! therefore fails at once instead of being stopped by SIGTTIN/SIGTTOU, and
//! signals the terminal sends to pi never reach it. While the tool runs its
//! group is registered: dropping the tool future (an interrupt or a timeout)
//! kills the whole tree, and [`kill_running_tool_groups`] lets a fatal-signal
//! handler do the same before pi exits.

use std::collections::BTreeSet;
use std::io;
use std::sync::{Mutex, MutexGuard, PoisonError};

use nix::sys::signal::{killpg, Signal};
use nix::unistd::{setsid, Pid};
use tokio::process::{Child, Command};

/// Process groups of running tool subprocesses, each led by the direct child.
static RUNNING_GROUPS: Mutex<BTreeSet<i32>> = Mutex::new(BTreeSet::new());

fn running_groups() -> MutexGuard<'static, BTreeSet<i32>> {
    RUNNING_GROUPS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// The process group of a running tool subprocess. Dropping it SIGKILLs the
/// whole group; call [`ToolProcessGroup::finish`] once the tool exited
/// normally, so background jobs it deliberately started keep running.
#[derive(Debug)]
pub struct ToolProcessGroup {
    pgid: Option<i32>,
}

impl ToolProcessGroup {
    /// Releases the group without killing it.
    pub fn finish(mut self) {
        if let Some(pgid) = self.pgid.take() {
            running_groups().remove(&pgid);
        }
    }
}

impl Drop for ToolProcessGroup {
    fn drop(&mut self) {
        // A group that kill_running_tool_groups already took is not
        // signalled again.
        if let Some(pgid) = self.pgid.take() {
            if running_groups().remove(&pgid) {
                kill_group(pgid);
            }
        }
    }
}

/// Spawns `command` in a new session (see the module docs) and registers its
/// process group. The direct child is also killed when its handle drops.
pub fn spawn_tool_process(command: &mut Command) -> io::Result<(Child, ToolProcessGroup)> {
    command.kill_on_drop(true);
    // SAFETY: the hook runs in the child between fork and exec and only calls
    // setsid(2), which is async-signal-safe. setsid fails for a process group
    // leader, so the command must not also use `process_group`.
    unsafe {
        command.pre_exec(|| setsid().map(drop).map_err(io::Error::from));
    }
    // Registering under the same lock as the spawn means
    // kill_running_tool_groups cannot run between the two and miss the group.
    let mut groups = running_groups();
    let child = command.spawn()?;
    // A session leader's pid is its process group id. Never record 0 or 1:
    // killpg(0) would signal pi's own group.
    let pgid = child
        .id()
        .and_then(|pid| i32::try_from(pid).ok())
        .filter(|&pgid| pgid > 1);
    if let Some(pgid) = pgid {
        groups.insert(pgid);
    }
    Ok((child, ToolProcessGroup { pgid }))
}

/// SIGKILLs the process group of every running tool subprocess. Meant for a
/// fatal-signal handler that is about to exit; the guards of the killed
/// groups no longer signal them.
pub fn kill_running_tool_groups() {
    let groups = std::mem::take(&mut *running_groups());
    for pgid in groups {
        kill_group(pgid);
    }
}

fn kill_group(pgid: i32) {
    if pgid > 1 {
        let _ = killpg(Pid::from_raw(pgid), Signal::SIGKILL);
    }
}
