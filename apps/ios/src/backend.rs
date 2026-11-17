//! The app's link to `noel server`: noel-client over an SSH exec channel.
//!
//! The channel runs `noel bridge` without a PTY. Its stdin and stdout carry
//! the WebSocket link, and the SSH session is owned by that byte stream.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use futures::channel::mpsc::UnboundedSender;
use noel_chord::v2::object::BoxFuture;
use noel_chord::v2::Link;
use noel_client::app::EventSink;
pub use noel_client::app::{Command, Event};
use noel_client::{websocket_link, Connector, Report};
use russh::client;
use russh::keys::{HashAlg, PublicKeyOrCertificate};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc, Mutex};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);
const KNOWN_HOSTS: &str = "known_hosts.json";

/// Where and how to reach the server.
#[derive(Clone)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    /// The remote `noel` executable, run as `<remote_program> bridge`.
    pub remote_program: String,
}

impl Endpoint {
    pub fn new(
        host: impl Into<String>,
        port: u16,
        username: impl Into<String>,
        password: impl Into<String>,
        remote_program: impl Into<String>,
    ) -> Self {
        Self {
            host: host.into(),
            port,
            username: username.into(),
            password: password.into(),
            remote_program: remote_program.into(),
        }
    }

    fn address(&self) -> String {
        if self.host.contains(':') && !self.host.starts_with('[') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

impl fmt::Debug for Endpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Endpoint")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("remote_program", &self.remote_program)
            .finish()
    }
}

/// Backend configuration. `app_home` is the iOS app's home directory.
#[derive(Clone, Debug)]
pub struct Config {
    pub endpoint: Endpoint,
    pub app_home: PathBuf,
    /// The limit for SSH connection, authentication, exec, and WebSocket upgrade.
    pub timeout: Duration,
}

impl Config {
    pub fn new(endpoint: Endpoint, app_home: impl Into<PathBuf>) -> Self {
        Self {
            endpoint,
            app_home: app_home.into(),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    pub fn known_hosts_path(&self) -> PathBuf {
        self.app_home.join(KNOWN_HOSTS)
    }
}

/// A reusable SSH connector. Clones share access to the known-hosts file.
#[derive(Clone, Debug)]
pub struct Backend {
    config: Config,
    known_hosts_lock: Arc<Mutex<()>>,
}

impl Backend {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            known_hosts_lock: Arc::new(Mutex::new(())),
        }
    }
}

impl Connector for Backend {
    fn open(&self, _interactive: bool, report: Report) -> BoxFuture<Result<Link>> {
        let backend = self.clone();
        Box::pin(async move {
            let endpoint = backend.config.endpoint.clone();
            let timeout = backend.config.timeout;
            match tokio::time::timeout(timeout, backend.open(report)).await {
                Ok(result) => result,
                Err(_) => Err(anyhow!(
                    "connect to {} timed out after {:.1} s",
                    endpoint.address(),
                    timeout.as_secs_f64()
                )),
            }
        })
    }
}

impl Backend {
    async fn open(self, report: Report) -> Result<Link> {
        let endpoint = &self.config.endpoint;
        let address = endpoint.address();
        let handler = HostKeyHandler {
            endpoint: endpoint.clone(),
            known_hosts_path: self.config.known_hosts_path(),
            known_hosts_lock: self.known_hosts_lock,
            report,
        };
        let ssh_config = Arc::new(client::Config {
            nodelay: true,
            ..client::Config::default()
        });
        let mut session =
            client::connect(ssh_config, (endpoint.host.as_str(), endpoint.port), handler)
                .await
                .map_err(|error| anyhow!("SSH connection to {address}: {error:#}"))?;
        let authentication = session
            .authenticate_password(&endpoint.username, &endpoint.password)
            .await
            .map_err(|error| anyhow!("SSH authentication to {address}: {error}"))?;
        if !authentication.success() {
            bail!(
                "SSH authentication to {address} failed for {}",
                endpoint.username
            );
        }

        let channel = session
            .channel_open_session()
            .await
            .map_err(|error| anyhow!("open SSH exec channel on {address}: {error}"))?;
        channel
            .exec(
                true,
                format!("{} bridge", shell_quote(&endpoint.remote_program)),
            )
            .await
            .map_err(|error| anyhow!("start the bridge on {address}: {error}"))?;
        websocket_link(BridgeStream {
            stream: channel.into_stream(),
            _session: session,
        })
        .await
    }
}

struct BridgeStream {
    stream: russh::ChannelStream<client::Msg>,
    _session: client::Handle<HostKeyHandler>,
}

impl AsyncRead for BridgeStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(context, buffer)
    }
}

impl AsyncWrite for BridgeStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(context, buffer)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(context)
    }
}

struct HostKeyHandler {
    endpoint: Endpoint,
    known_hosts_path: PathBuf,
    known_hosts_lock: Arc<Mutex<()>>,
    report: Report,
}

impl client::Handler for HostKeyHandler {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let fingerprint = server_public_key
            .public_key()
            .fingerprint(HashAlg::Sha256)
            .to_string();
        let address = self.endpoint.address();
        let _guard = self.known_hosts_lock.lock().await;
        let mut known_hosts = read_known_hosts(&self.known_hosts_path)?;
        match known_hosts.check(&address, &fingerprint) {
            HostKeyMatch::New => {
                write_known_hosts(&self.known_hosts_path, &known_hosts)?;
                (self.report)(format!(
                    "trusted new SSH host key for {address}: {fingerprint}"
                ));
                Ok(true)
            }
            HostKeyMatch::Same => Ok(true),
            HostKeyMatch::Changed { expected } => bail!(
                "SSH host key for {address} changed: stored {expected}, received {fingerprint}; refusing to connect"
            ),
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct KnownHosts {
    #[serde(default)]
    hosts: BTreeMap<String, String>,
}

enum HostKeyMatch {
    New,
    Same,
    Changed { expected: String },
}

impl KnownHosts {
    fn check(&mut self, address: &str, fingerprint: &str) -> HostKeyMatch {
        match self.hosts.get(address) {
            None => {
                self.hosts
                    .insert(address.to_owned(), fingerprint.to_owned());
                HostKeyMatch::New
            }
            Some(expected) if expected == fingerprint => HostKeyMatch::Same,
            Some(expected) => HostKeyMatch::Changed {
                expected: expected.clone(),
            },
        }
    }
}

fn read_known_hosts(path: &Path) -> Result<KnownHosts> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("read SSH known hosts at {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(KnownHosts::default()),
        Err(error) => {
            Err(error).with_context(|| format!("read SSH known hosts at {}", path.display()))
        }
    }
}

fn write_known_hosts(path: &Path, known_hosts: &KnownHosts) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("known-hosts path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create app home directory {}", parent.display()))?;
    let temporary = path.with_extension("json.tmp");
    let result = (|| -> Result<()> {
        let mut file = fs::File::create(&temporary)
            .with_context(|| format!("create {}", temporary.display()))?;
        serde_json::to_writer_pretty(&mut file, known_hosts)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, path).with_context(|| {
            format!(
                "replace SSH known hosts {} with {}",
                path.display(),
                temporary.display()
            )
        })?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Events into the window's channel.
struct Window(UnboundedSender<Event>);

impl EventSink for Window {
    fn send(&self, event: Event) -> bool {
        self.0.unbounded_send(event).is_ok()
    }
}

/// Connects with `config` and serves the window until it is gone.
pub async fn run(
    config: Config,
    events: UnboundedSender<Event>,
    commands: mpsc::UnboundedReceiver<Command>,
) -> Result<()> {
    noel_client::app::run(
        Arc::new(Backend::new(config)),
        Arc::new(Window(events)),
        commands,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_hosts_accept_only_the_first_key_seen_for_an_address() {
        let mut hosts = KnownHosts::default();
        assert!(matches!(
            hosts.check("server:22", "SHA256:first"),
            HostKeyMatch::New
        ));
        assert!(matches!(
            hosts.check("server:22", "SHA256:first"),
            HostKeyMatch::Same
        ));
        match hosts.check("server:22", "SHA256:changed") {
            HostKeyMatch::Changed { expected } => assert_eq!(expected, "SHA256:first"),
            _ => panic!("a changed key was accepted"),
        }
        assert_eq!(hosts.hosts["server:22"], "SHA256:first");
        assert!(matches!(
            hosts.check("server:2222", "SHA256:changed"),
            HostKeyMatch::New
        ));
    }

    #[test]
    fn endpoint_addresses_distinguish_ports_and_bracket_ipv6() {
        let mut endpoint = Endpoint::new("::1", 22, "user", "secret", "noel");
        assert_eq!(endpoint.address(), "[::1]:22");
        endpoint.host = "server".into();
        endpoint.port = 2222;
        assert_eq!(endpoint.address(), "server:2222");
        assert!(!format!("{endpoint:?}").contains("secret"));
    }

    #[test]
    fn remote_program_is_one_shell_word() {
        assert_eq!(shell_quote("noel"), "'noel'");
        assert_eq!(shell_quote("/tmp/it's noel"), "'/tmp/it'\\''s noel'");
    }
}
