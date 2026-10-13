//! Fatal-signal handling for every mode, installed once from `main`.
//!
//! SIGTERM and SIGHUP end noel. SIGINT does too unless the raw-mode TUI owns
//! the terminal: there ctrl+c is a key handled by the TUI's own bindings, and
//! a SIGINT sent from outside is ignored. Before exiting, noel kills the process
//! groups of running tools (they run in their own sessions, so the terminal's
//! signals never reach them) and restores the terminal if the TUI owns it,
//! then exits like upstream pi: status 128 + the signal number for SIGTERM and
//! SIGHUP, and death by the signal itself for SIGINT.

use std::io;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// `Some(owned_screen)` while the TUI holds the terminal (in raw mode).
static TUI_TERMINAL: Mutex<Option<bool>> = Mutex::new(None);

fn tui_terminal() -> MutexGuard<'static, Option<bool>> {
    TUI_TERMINAL.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Records that the TUI is taking the terminal; called before it enables raw
/// mode, so a signal never finds raw mode on without a claim.
pub(crate) fn claim_tui_terminal(owned_screen: bool) {
    *tui_terminal() = Some(owned_screen);
}

/// Restores the terminal if the TUI still owns it. The TUI's normal exit and
/// the signal path both restore through the same lock, so it happens once.
pub(crate) fn release_tui_terminal() {
    let mut terminal = tui_terminal();
    if let Some(owned_screen) = terminal.take() {
        crate::restore_terminal_modes(&mut io::stdout().lock(), owned_screen);
    }
}

/// Starts listening for SIGTERM, SIGHUP and SIGINT. Must run inside the
/// Tokio runtime; the handlers stay installed for the life of the process.
pub(crate) fn install() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        for kind in [
            SignalKind::terminate(),
            SignalKind::hangup(),
            SignalKind::interrupt(),
        ] {
            let Ok(mut signals) = signal(kind) else {
                continue;
            };
            tokio::spawn(async move {
                while signals.recv().await.is_some() {
                    if kind == SignalKind::interrupt() && tui_terminal().is_some() {
                        continue;
                    }
                    exit_for_signal(kind.as_raw_value());
                }
            });
        }
    }
}

#[cfg(unix)]
fn exit_for_signal(signal: i32) -> ! {
    noel_core::kill_running_tool_groups();
    let mut terminal = tui_terminal();
    // Both locks stay held until exit: a concurrent normal restore waits, and
    // the TUI cannot paint over the restored terminal. Outside the TUI stdout
    // is left alone, so a stalled stdout pipe cannot block exit.
    let _stdout = terminal.take().map(|owned_screen| {
        let mut stdout = io::stdout().lock();
        crate::restore_terminal_modes(&mut stdout, owned_screen);
        stdout
    });
    exit_like_upstream(signal)
}

/// SIGTERM and SIGHUP exit with status 128 + the signal number, as upstream
/// pi does. SIGINT ends the process by the signal itself, as upstream's
/// default disposition does: bash, for one, stops a running loop on ctrl+c
/// only when the child died of SIGINT.
#[cfg(unix)]
fn exit_like_upstream(signal: i32) -> ! {
    use nix::sys::signal::{raise, SigHandler, Signal};

    if signal == Signal::SIGINT as i32 {
        // SAFETY: restoring the default disposition installs no handler.
        if unsafe { nix::sys::signal::signal(Signal::SIGINT, SigHandler::SigDfl) }.is_ok() {
            let _ = raise(Signal::SIGINT);
        }
    }
    std::process::exit(128 + signal)
}
