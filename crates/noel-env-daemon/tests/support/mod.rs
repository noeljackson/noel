//! What the client tests share: the daemon they start and scratch directories.

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use noel_env::{Connection, RemoteEnv, RemoteEnvOptions};

/// The daemon the client tests start: `NOEL_ENV_TEST_DAEMON` (`make env-interop` points it at upstream's `pi-env`),
/// else this crate's `noel-env`.
pub fn daemon() -> String {
    std::env::var("NOEL_ENV_TEST_DAEMON")
        .ok()
        .filter(|daemon| !daemon.is_empty())
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_noel-env").to_string())
}

/// A connection that starts the daemon directly, without ssh.
pub fn connection() -> Connection {
    Connection::command(vec![daemon()])
}

pub fn remote_env(connection: &Connection, cwd: &str) -> RemoteEnv {
    RemoteEnv::new(RemoteEnvOptions::new(
        connection.clone(),
        "pi-env:test",
        cwd,
    ))
}

/// A fresh directory, canonical so resolved paths spell it as the daemon does, removed on drop.
pub struct Scratch(pub PathBuf);

impl Scratch {
    pub fn new(name: &str) -> Scratch {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "noel-env-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Scratch(path.canonicalize().unwrap())
    }

    pub fn text(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }

    pub fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
