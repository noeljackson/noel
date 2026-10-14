//! Mode 2 (spec §5–§8): a long-running `noel server` that holds sessions, one
//! `noel worker` per session that runs it, and clients that reach them, all
//! over Chord link protocol 2 (`noel-chord`).
//!
//! - **The server** ([`serve`]) is the user's one host, `server`, on an
//!   owner-only Unix socket in the runtime directory. It serves
//!   `noel.sessions` and `noel.models`, routes `session:<id>` to its workers,
//!   spawns, watches and adopts them, and resumes a run a worker crash
//!   interrupted.
//! - **A worker** ([`worker`]) is the host `session:<id>`: the session's only
//!   writer, holding its journal lock. It serves `noel.chat` with the view
//!   of the conversation ([`view`]) and drives runs with the session
//!   controller. It outlives its server and waits to be adopted.
//! - **The client** ([`client`]) is line-mode `noel client`.
//!
//! Only device access is built: the owner's processes on this machine, by
//! the socket's peer uid. There is no relay, tunnel, pairing or guest yet.

pub(crate) mod client;
pub(crate) mod serve;
mod view;
pub(crate) mod worker;

use std::fs;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use noel_chord::Description;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::net::UnixStream;

/// `EX_TEMPFAIL`: another server is running, or the session's journal is
/// locked by another process.
pub(crate) const EXIT_LOCKED: i32 = 75;

/// Overrides where the server's socket, lock and worker sockets live.
pub(crate) const ENV_RUNTIME_DIR: &str = "NOEL_RUNTIME_DIR";

/// The worker's `ready` and `fatal` lines, and the control lines on a
/// server–worker link, are `{"chord2control": message}`.
pub(crate) const CONTROL_KEY: &str = "chord2control";

/// Default for `server.idleExitMs`.
pub(crate) const DEFAULT_SERVER_IDLE_MS: u64 = 10 * 60 * 1000;
/// Default for `server.workerIdleExitMs`.
pub(crate) const DEFAULT_WORKER_IDLE_MS: u64 = 60 * 1000;

const CHAT_CONTRACT: &str = include_str!("contracts/noel.chat.json");
const SESSIONS_CONTRACT: &str = include_str!("contracts/noel.sessions.json");
const MODELS_CONTRACT: &str = include_str!("contracts/noel.models.json");

pub(crate) fn chat_contract() -> Description {
    serde_json::from_str(CHAT_CONTRACT).expect("noel.chat's contract parses")
}

pub(crate) fn sessions_contract() -> Description {
    serde_json::from_str(SESSIONS_CONTRACT).expect("noel.sessions' contract parses")
}

pub(crate) fn models_contract() -> Description {
    serde_json::from_str(MODELS_CONTRACT).expect("noel.models' contract parses")
}

/// Runs `noel server`, `noel worker` or `noel client` when the command line
/// names one, and returns the exit status; `None` for any other command.
pub(crate) async fn try_run(args: &[String]) -> Option<i32> {
    use clap::Parser;
    let command = args.get(1)?.as_str();
    let rest = std::iter::once(format!("noel {command}")).chain(args[2..].iter().cloned());
    Some(match command {
        "server" => match serve::ServerArgs::try_parse_from(rest) {
            Ok(parsed) => serve::run(parsed).await,
            Err(error) => usage(error),
        },
        "worker" => match worker::WorkerArgs::try_parse_from(rest) {
            Ok(parsed) => worker::run(parsed).await,
            Err(error) => usage(error),
        },
        "client" => match client::ClientArgs::try_parse_from(rest) {
            Ok(parsed) => client::run(parsed).await,
            Err(error) => usage(error),
        },
        _ => return None,
    })
}

fn usage(error: clap::Error) -> i32 {
    let code = error.exit_code();
    let _ = error.print();
    code
}

// ─── the runtime directory ──────────────────────────────────────────────────

/// Where the server's socket and lock and the workers' sockets live:
/// `NOEL_RUNTIME_DIR`, else `$XDG_RUNTIME_DIR/noel`, else `/tmp/noel-<uid>`.
#[derive(Debug, Clone)]
pub(crate) struct RuntimeDir {
    root: PathBuf,
}

impl RuntimeDir {
    pub fn locate() -> PathBuf {
        if let Some(dir) = std::env::var_os(ENV_RUNTIME_DIR).filter(|dir| !dir.is_empty()) {
            return PathBuf::from(dir);
        }
        match std::env::var_os("XDG_RUNTIME_DIR").filter(|dir| !dir.is_empty()) {
            Some(dir) => PathBuf::from(dir).join("noel"),
            None => PathBuf::from(format!("/tmp/noel-{}", own_uid())),
        }
    }

    /// The directory, created with mode 0700 when missing (its parent must
    /// exist). An existing one must be a directory, not a link, owned by
    /// this user with permissions 0700, or nothing runs in it.
    pub fn ensure(root: PathBuf) -> Result<Self> {
        match fs::symlink_metadata(&root) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let mut builder = fs::DirBuilder::new();
                #[cfg(unix)]
                std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
                builder
                    .create(&root)
                    .map_err(|error| anyhow!("create {}: {error}", root.display()))?;
                // A setgid parent passes its bit on; the directory is 0700.
                set_mode(&root, 0o700)?;
            }
            Err(error) => return Err(anyhow!("{}: {error}", root.display())),
            Ok(_) => {}
        }
        Self::check(root)
    }

    /// An existing directory that is safe to use (see [`RuntimeDir::ensure`]).
    pub fn check(root: PathBuf) -> Result<Self> {
        let metadata =
            fs::symlink_metadata(&root).map_err(|error| anyhow!("{}: {error}", root.display()))?;
        if !metadata.is_dir() {
            return Err(anyhow!(
                "{} is not a directory; refusing to use it",
                root.display()
            ));
        }
        #[cfg(unix)]
        {
            if metadata.uid() != own_uid() {
                return Err(anyhow!(
                    "{} is owned by uid {}, not {}; refusing to use it",
                    root.display(),
                    metadata.uid(),
                    own_uid()
                ));
            }
            // Only the permissions decide who may enter: a setgid or sticky
            // bit on a 0700 directory of this user lets no one else in.
            let mode = metadata.permissions().mode() & 0o7777;
            if mode & 0o777 != 0o700 {
                return Err(anyhow!(
                    "{} has mode {mode:o}, not 700; refusing to use it",
                    root.display()
                ));
            }
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn server_socket(&self) -> PathBuf {
        self.root.join("server.sock")
    }

    pub fn server_lock(&self) -> PathBuf {
        self.root.join("server.lock")
    }

    pub fn server_record(&self) -> PathBuf {
        self.root.join("server.json")
    }

    pub fn server_log(&self) -> PathBuf {
        self.root.join("server.log")
    }

    pub fn workers(&self) -> PathBuf {
        self.root.join("workers")
    }

    pub fn worker_socket(&self, session: &str) -> PathBuf {
        self.workers().join(format!("{session}.sock"))
    }

    pub fn worker_record(&self, session: &str) -> PathBuf {
        self.workers().join(format!("{session}.json"))
    }

    pub fn worker_log(&self, session: &str) -> PathBuf {
        self.workers().join(format!("{session}.log"))
    }

    /// `workers/`, created with mode 0700 when missing.
    pub fn ensure_workers(&self) -> Result<PathBuf> {
        let dir = self.workers();
        if !dir.exists() {
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
            builder
                .create(&dir)
                .map_err(|error| anyhow!("create {}: {error}", dir.display()))?;
        }
        Ok(dir)
    }
}

pub(crate) fn own_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// Removes a socket left at `path` by a process that is gone, only when it
/// is a socket this user owns. Callers hold the lock that makes them its
/// only user.
pub(crate) fn remove_stale_socket(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(anyhow!("{}: {error}", path.display())),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        if !metadata.file_type().is_socket() || metadata.uid() != own_uid() {
            return Err(anyhow!(
                "{} exists and is not a socket of this user; refusing to replace it",
                path.display()
            ));
        }
    }
    fs::remove_file(path).map_err(|error| anyhow!("remove {}: {error}", path.display()))
}

pub(crate) fn set_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|error| anyhow!("chmod {}: {error}", path.display()))?;
    Ok(())
}

/// Writes `value` to `path` (mode 0600) by replacing it.
pub(crate) fn write_record(path: &Path, value: &Value) -> Result<()> {
    let temporary = path.with_extension(format!("json.{}", std::process::id()));
    fs::write(&temporary, format!("{value}\n"))
        .map_err(|error| anyhow!("write {}: {error}", temporary.display()))?;
    set_mode(&temporary, 0o600)?;
    fs::rename(&temporary, path).map_err(|error| anyhow!("write {}: {error}", path.display()))
}

/// The other end of a Unix socket, checked before a byte is read: its uid
/// must be this user's. Returns its pid when the platform reports one.
pub(crate) fn peer_check(stream: &UnixStream) -> std::result::Result<Option<u32>, String> {
    let credentials = stream
        .peer_cred()
        .map_err(|error| format!("no peer credentials: {error}"))?;
    if credentials.uid() != own_uid() {
        return Err(format!("uid {} is not the owner", credentials.uid()));
    }
    Ok(credentials.pid().and_then(|pid| u32::try_from(pid).ok()))
}

/// Names this binary: its version, and the identity of the executable file
/// the process runs (device, inode, size and modification time). A server
/// adopts only workers of its own build.
pub(crate) fn build_id() -> String {
    let metadata = fs::metadata("/proc/self/exe")
        .or_else(|_| std::env::current_exe().and_then(fs::metadata))
        .ok();
    let identity = metadata
        .map(|metadata| {
            #[cfg(unix)]
            {
                format!(
                    "{:x}.{:x}.{:x}.{:x}",
                    metadata.dev(),
                    metadata.ino(),
                    metadata.len(),
                    metadata.mtime_nsec() as u64 ^ ((metadata.mtime() as u64) << 20)
                )
            }
            #[cfg(not(unix))]
            {
                format!("{:x}", metadata.len())
            }
        })
        .unwrap_or_else(|| "unknown".to_owned());
    format!("{}+{identity}", env!("CARGO_PKG_VERSION"))
}

/// The program a server runs for its workers and a client for its server:
/// this process's own executable, even when it was replaced on disk.
pub(crate) fn own_exe() -> PathBuf {
    let proc_exe = PathBuf::from("/proc/self/exe");
    if proc_exe.exists() {
        return proc_exe;
    }
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("noel"))
}

/// A session id the server routes: what `--session-id` accepts, so it is a
/// file name and never a path.
pub(crate) fn valid_session_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        && !id.contains("..")
}

/// The id of the session a `create` or `fork` with `request_id` makes, so a
/// repeat finds it instead of making another (spec §3.1's `requestId`).
pub(crate) fn derived_session_id(method: &str, request_id: &str) -> String {
    let digest = Sha256::digest(format!("noel.sessions.{method}\0{request_id}").as_bytes());
    let hex = digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("s-{hex}")
}

/// The principal of the owner's processes on this machine.
pub(crate) fn owner_device() -> Value {
    json!({"kind": "owner", "via": "device"})
}

/// The principal of a worker's own calls.
pub(crate) fn worker_principal(session: &str, generation: u64) -> Value {
    json!({"kind": "worker", "session": session, "generation": generation})
}

/// The `noel.chat` variant `principal` gets on `session`: the owner and a
/// full guest of the session participate, a viewing guest views, anyone
/// else gets nothing (spec §6, §7.2).
pub(crate) fn chat_variant(principal: &Value, session: &str) -> Option<&'static str> {
    match principal.get("kind").and_then(Value::as_str) {
        Some("owner") => Some("participant"),
        Some("guest") if principal.get("session").and_then(Value::as_str) == Some(session) => {
            match principal.get("mode").and_then(Value::as_str) {
                Some("full") => Some("participant"),
                Some("view") => Some("viewer"),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Keys objects by principal (spec §6): `owner@<via>`,
/// `guest:<id>:r<revision>`, `worker:<session>:<generation>`.
pub(crate) fn principal_key(principal: &Value) -> String {
    let text = |key: &str| {
        principal
            .get(key)
            .map(|value| match value {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            })
            .unwrap_or_default()
    };
    match principal.get("kind").and_then(Value::as_str) {
        Some("owner") => format!("owner@{}", text("via")),
        Some("guest") => format!("guest:{}:r{}", text("id"), text("revision")),
        Some("worker") => format!("worker:{}:{}", text("session"), text("generation")),
        _ => noel_chord::canonical_json(principal),
    }
}

/// A control line: `{"chord2control": message}`.
pub(crate) fn control_line(message: &Value) -> String {
    let mut line = json!({ CONTROL_KEY: message }).to_string();
    line.push('\n');
    line
}

/// The message of a control line, if `line` is one.
pub(crate) fn parse_control(line: &str) -> Option<Value> {
    let mut value: Value = serde_json::from_str(line.trim()).ok()?;
    value.as_object_mut()?.remove(CONTROL_KEY)
}

/// A random id for a `requestId`.
pub(crate) fn new_request_id() -> String {
    noel_core::controller::new_id()
}

/// Milliseconds since the epoch.
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nrt-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn the_runtime_directory_is_made_private_and_a_shared_one_is_refused() {
        let dir = temp("rd");
        let made = RuntimeDir::ensure(dir.clone()).unwrap();
        let mode = fs::metadata(made.root()).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o700);
        // A directory others may enter is refused, and left as it is.
        set_mode(&dir, 0o755).unwrap();
        let error = RuntimeDir::ensure(dir.clone()).unwrap_err().to_string();
        assert!(error.contains("mode 755"), "{error}");
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o7777,
            0o755
        );
        // A link to a private directory is refused too.
        set_mode(&dir, 0o700).unwrap();
        let link = temp("rl");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert!(RuntimeDir::check(link.clone()).is_err());
        fs::remove_file(link).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_setgid_parent_leaves_the_runtime_directory_usable() {
        let parent = temp("sg");
        fs::create_dir(&parent).unwrap();
        set_mode(&parent, 0o2755).unwrap();
        // Only when this system lets a user set the bit on their own directory.
        if fs::metadata(&parent).unwrap().permissions().mode() & 0o2000 != 0 {
            let dir = parent.join("noel");
            let made = RuntimeDir::ensure(dir.clone()).unwrap();
            let mode = fs::metadata(made.root()).unwrap().permissions().mode() & 0o7777;
            assert_eq!(mode, 0o700);
            // One made earlier with the inherited bit is used as it is.
            set_mode(&dir, 0o2700).unwrap();
            RuntimeDir::ensure(dir.clone()).unwrap();
            assert_eq!(
                fs::metadata(&dir).unwrap().permissions().mode() & 0o7777,
                0o2700
            );
            // Group permissions are still refused.
            set_mode(&dir, 0o2750).unwrap();
            let error = RuntimeDir::ensure(dir).unwrap_err().to_string();
            assert!(error.contains("mode 2750"), "{error}");
        }
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn only_a_stale_socket_of_this_user_is_removed() {
        let dir = temp("st");
        RuntimeDir::ensure(dir.clone()).unwrap();
        let file = dir.join("server.sock");
        fs::write(&file, "not a socket").unwrap();
        assert!(remove_stale_socket(&file).is_err());
        assert!(file.exists());
        fs::remove_file(&file).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&file).unwrap();
        drop(listener);
        remove_stale_socket(&file).unwrap();
        assert!(!file.exists());
        remove_stale_socket(&file).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn session_ids_are_file_names_and_derived_ids_are_stable() {
        for id in ["abc", "1760-123-0", "s-0a1b", "a.b_c-d"] {
            assert!(valid_session_id(id), "{id}");
        }
        for id in ["", "../x", "a/b", ".hidden", "x..y", "-a", "a-", "a b"] {
            assert!(!valid_session_id(id), "{id}");
        }
        let id = derived_session_id("create", "r1");
        assert_eq!(id, derived_session_id("create", "r1"));
        assert_ne!(id, derived_session_id("create", "r2"));
        assert_ne!(id, derived_session_id("fork", "r1"));
        assert!(valid_session_id(&id) && id.len() == 26, "{id}");
    }

    #[test]
    fn principals_get_their_variant_and_key() {
        assert_eq!(chat_variant(&owner_device(), "s1"), Some("participant"));
        let guest = |session: &str, mode: &str| json!({"kind": "guest", "id": "g", "name": "G", "session": session, "mode": mode, "revision": 2});
        assert_eq!(chat_variant(&guest("s1", "view"), "s1"), Some("viewer"));
        assert_eq!(
            chat_variant(&guest("s1", "full"), "s1"),
            Some("participant")
        );
        assert_eq!(chat_variant(&guest("s2", "full"), "s1"), None);
        assert_eq!(chat_variant(&worker_principal("s1", 1), "s1"), None);
        assert_eq!(chat_variant(&json!({"host": "server"}), "s1"), None);
        assert_eq!(principal_key(&owner_device()), "owner@device");
        assert_eq!(principal_key(&guest("s1", "view")), "guest:g:r2");
        assert_eq!(principal_key(&worker_principal("s1", 3)), "worker:s1:3");
    }

    #[test]
    fn the_contracts_parse_and_have_their_variants() {
        let chat = chat_contract();
        assert_eq!((chat.name.as_str(), chat.version), ("noel.chat", 1));
        assert!(chat.variants.get("viewer").is_some());
        assert!(chat.variants.get("participant").is_some());
        assert_eq!(sessions_contract().name, "noel.sessions");
        assert_eq!(models_contract().name, "noel.models");
    }

    #[test]
    fn control_lines_round_trip() {
        let line = control_line(&json!({"type": "ready", "pid": 7}));
        assert_eq!(line, "{\"chord2control\":{\"pid\":7,\"type\":\"ready\"}}\n");
        assert_eq!(
            parse_control(&line),
            Some(json!({"type": "ready", "pid": 7}))
        );
        assert_eq!(parse_control("{\"chord2\":{}}"), None);
        assert_eq!(parse_control("ready"), None);
    }
}
