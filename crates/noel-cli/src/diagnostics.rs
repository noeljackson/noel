//! Diagnostics (#183): one tracing subscriber for the process. `NOEL_LOG`
//! (or `RUST_LOG`) filters it, `info` by default; events go to stderr,
//! which is the log file for `noel server`, its workers and the bridge,
//! and nowhere while the TUI holds the terminal. User-facing output of the
//! print and JSON modes stays on stdout and stderr as it is.

use std::io::{self, Write};

use tracing_subscriber::EnvFilter;

/// Installs the subscriber; a second call does nothing.
pub(crate) fn init() {
    let filter = EnvFilter::try_from_env("NOEL_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_ansi(false)
        .with_writer(|| Diagnostics)
        .try_init();
}

/// Stderr, unless the TUI holds the terminal.
struct Diagnostics;

impl Write for Diagnostics {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if crate::signals::tui_owns_terminal() {
            return Ok(bytes.len());
        }
        io::stderr().write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stderr().flush()
    }
}
