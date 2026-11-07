//! Remote access for paired devices (roadmap phase 11, spec §16), after
//! upstream Chord's app kit (`node/app/pairing.ts`, `owner.ts`), without a
//! web page: the owner approves a pairing from the device socket.
//!
//! - **Listening:** with `server.listen` set (`host:port`), the server also
//!   listens over TLS with a self-signed certificate it keeps in
//!   `<agent dir>/server-tls/` (0600); devices pin its SHA-256 fingerprint.
//!   Off unless set.
//! - **Pairing:** `POST /_chord/pair` (`{label?}` → `{code, poll}`; 429 past
//!   100 pending pairings or 10 per address), `POST /_chord/pair/poll`
//!   (`{poll}` → `{status: "waiting"}`, then `{status: "approved", token}`
//!   once, else 404). Codes are `XXXX-XXXX` without 0/O/1/I/L, single use,
//!   ten minutes; poll secrets and device tokens are kept only hashed.
//! - **Approval and revocation:** `noel.devices` (owner): the paired
//!   devices, the pending pairings and the certificate's fingerprint;
//!   `approve({code})` issues the device's token, `revoke({id})` drops it
//!   and revokes `{device: id}`, which ends its objects and closes its links.
//! - **The link:** `GET /chord`, a WebSocket upgrade with `Authorization:
//!   Bearer <token>`, is stamped `{kind: "owner", via: "remote", device}`;
//!   without a valid token it is refused (401).
//!
//! The client side: `noel client pair <https://host:port>` and
//! `noel client --server <https://host:port>` ([`pair`], [`connect`]).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context as _, Result};
use noel_chord::v2::host::{Host, LinkOptions};
use noel_chord::v2::object::{Cell, Implementation};
use noel_chord::v2::{Link, WireError};
use noel_client::contracts;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream};

use super::errors::{forbidden, refused};

const PAIRING_TTL: Duration = Duration::from_secs(10 * 60);
const MAX_PENDING: usize = 100;
const MAX_PER_ADDRESS: usize = 10;
const ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
const MAX_HEAD: usize = 8 * 1024;
const MAX_BODY: usize = 4 * 1024;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    getrandom::fill(&mut buffer).expect("system randomness");
    buffer.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hash(secret: &str) -> String {
    Sha256::digest(secret.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn now_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

/// A certificate's SHA-256 fingerprint, lowercase hex.
pub(crate) fn fingerprint(der: &[u8]) -> String {
    Sha256::digest(der)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    noel_config::write_file_atomic(path, contents)?;
    super::set_mode(path, 0o600)?;
    Ok(())
}

// ─── the server side ────────────────────────────────────────────────────────

#[derive(Clone)]
struct Device {
    id: String,
    label: String,
    created: u64,
    hash: String,
}

struct Pairing {
    code: String,
    label: String,
    address: String,
    expires: std::time::Instant,
    poll_hash: String,
    /// Approved: the device made for it and the token its poll gets.
    approved: Option<(String, String)>,
}

struct State {
    devices: Vec<Device>,
    pairings: Vec<Pairing>,
}

impl State {
    /// Drops the pairings that expired (#175). One approved whose token no
    /// poll collected takes its device with it: nobody holds that token.
    /// Whether devices changed, to save them.
    fn prune(&mut self, now: std::time::Instant) -> bool {
        let mut orphans = Vec::new();
        self.pairings.retain(|pairing| {
            let keep = pairing.expires > now;
            if let (false, Some((device, _))) = (keep, &pairing.approved) {
                orphans.push(device.clone());
            }
            keep
        });
        let before = self.devices.len();
        self.devices.retain(|device| !orphans.contains(&device.id));
        self.devices.len() != before
    }
}

pub(crate) struct Remote {
    host: Host,
    store: PathBuf,
    state: Mutex<State>,
    directory: Cell,
    fingerprint: String,
    listening: String,
}

impl Remote {
    /// Starts listening on `address` when it is set, and serves
    /// `noel.devices` either way.
    pub(crate) async fn start(
        host: &Host,
        agent_dir: &Path,
        address: Option<&str>,
    ) -> Result<Arc<Remote>> {
        let (config, fingerprint) = server_tls(agent_dir)?;
        let store = agent_dir.join("devices.json");
        let devices = read_devices(&store);
        let schema = contracts::devices().declaration.variants["owner"].state["devices"].clone();
        let listening = address.unwrap_or_default().to_owned();
        let directory = Cell::new(
            schema,
            json!({"devices": [], "pending": [], "fingerprint": fingerprint}),
        )?;
        let remote = Arc::new(Remote {
            host: host.clone(),
            store,
            state: Mutex::new(State {
                devices,
                pairings: Vec::new(),
            }),
            directory,
            fingerprint,
            listening,
        });
        host.revoked_by(
            |principal| match principal.get("via").and_then(Value::as_str) {
                Some("remote") => vec![json!({"device": principal["device"]})],
                _ => Vec::new(),
            },
        );
        serve_devices(host, remote.clone());
        remote.publish();
        if let Some(address) = address {
            let listener = TcpListener::bind(address)
                .await
                .with_context(|| format!("listen on {address}"))?;
            eprintln!(
                "noel server: listening for paired devices on {address} (TLS, fingerprint {})",
                remote.fingerprint
            );
            let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
            tokio::spawn(remote.clone().accept(listener, acceptor));
        }
        Ok(remote)
    }

    fn publish(&self) {
        let mut state = lock(&self.state);
        let now = std::time::Instant::now();
        if state.prune(now) {
            let devices = state.devices.clone();
            self.save(&devices);
        }
        let mut value = json!({
            "devices": state.devices.iter().map(|device| json!({
                "id": device.id, "label": device.label, "created": device.created,
            })).collect::<Vec<_>>(),
            "pending": state.pairings.iter().filter(|pairing| pairing.approved.is_none()).map(|pairing| json!({
                "code": pairing.code, "label": pairing.label,
            })).collect::<Vec<_>>(),
            "fingerprint": self.fingerprint,
        });
        if !self.listening.is_empty() {
            value["listening"] = json!(self.listening);
        }
        drop(state);
        if let Err(error) = self.directory.update(value) {
            eprintln!("noel server: devices: {error}");
        }
    }

    fn save(&self, devices: &[Device]) {
        let records: Vec<Value> = devices
            .iter()
            .map(|device| {
                json!({"id": device.id, "label": device.label, "created": device.created, "hash": device.hash})
            })
            .collect();
        if let Err(error) = write_private(&self.store, json!(records).to_string().as_bytes()) {
            eprintln!("noel server: {}: {error:#}", self.store.display());
        }
    }

    /// Begins a pairing for an anonymous client at `address`.
    fn begin(&self, label: String, address: String) -> Option<(String, String)> {
        let mut state = lock(&self.state);
        let now = std::time::Instant::now();
        if state.prune(now) {
            let devices = state.devices.clone();
            self.save(&devices);
        }
        // Only pairings waiting for approval count (#175).
        let waiting = || {
            state
                .pairings
                .iter()
                .filter(|pairing| pairing.approved.is_none())
        };
        let from_address = waiting()
            .filter(|pairing| pairing.address == address)
            .count();
        if waiting().count() >= MAX_PENDING || from_address >= MAX_PER_ADDRESS {
            return None;
        }
        let code = loop {
            let mut bytes = [0u8; 8];
            getrandom::fill(&mut bytes).expect("system randomness");
            let letters: String = bytes
                .iter()
                .map(|byte| ALPHABET[usize::from(*byte) % ALPHABET.len()] as char)
                .collect();
            let code = format!("{}-{}", &letters[..4], &letters[4..]);
            if !state.pairings.iter().any(|pairing| pairing.code == code) {
                break code;
            }
        };
        let poll = random_hex(32);
        state.pairings.push(Pairing {
            code: code.clone(),
            label,
            address,
            expires: now + PAIRING_TTL,
            poll_hash: hash(&poll),
            approved: None,
        });
        Some((code, poll))
    }

    /// What a poll learns: `Some(None)` waiting, `Some(Some(token))` once
    /// approved (the pairing ends), `None` for no such pairing.
    fn poll(&self, poll: &str) -> Option<Option<String>> {
        let mut state = lock(&self.state);
        let now = std::time::Instant::now();
        if state.prune(now) {
            let devices = state.devices.clone();
            self.save(&devices);
        }
        let wanted = hash(poll);
        let index = state
            .pairings
            .iter()
            .position(|pairing| pairing.poll_hash == wanted)?;
        match state.pairings[index].approved.clone() {
            None => Some(None),
            Some((_, token)) => {
                state.pairings.remove(index);
                Some(Some(token))
            }
        }
    }

    /// The owner approves pairing `code`: a device with a new token.
    fn approve(&self, code: &str) -> Result<String, WireError> {
        let mut state = lock(&self.state);
        let now = std::time::Instant::now();
        if state.prune(now) {
            let devices = state.devices.clone();
            self.save(&devices);
        }
        let code = code.trim().to_uppercase();
        let Some(pairing) = state
            .pairings
            .iter_mut()
            .find(|pairing| pairing.code == code && pairing.approved.is_none())
        else {
            return Err(refused(
                "pairing",
                format!("no pairing {code} (or it expired)"),
            ));
        };
        let token = random_hex(32);
        let device = Device {
            id: format!("d-{}", random_hex(6)),
            label: pairing.label.clone(),
            created: now_s(),
            hash: hash(&token),
        };
        pairing.approved = Some((device.id.clone(), token));
        state.devices.push(device.clone());
        let devices = state.devices.clone();
        drop(state);
        self.save(&devices);
        self.publish();
        Ok(device.id)
    }

    /// Drops device `id` and revokes it: its objects end, its links close.
    fn revoke(&self, id: &str) -> Result<(), WireError> {
        let mut state = lock(&self.state);
        let before = state.devices.len();
        state.devices.retain(|device| device.id != id);
        if state.devices.len() == before {
            return Err(refused("device", format!("no device {id}")));
        }
        let devices = state.devices.clone();
        drop(state);
        self.save(&devices);
        self.host.revoke(json!({ "device": id }));
        self.publish();
        Ok(())
    }

    fn device(&self, token: &str) -> Option<Device> {
        let wanted = hash(token);
        lock(&self.state)
            .devices
            .iter()
            .find(|device| device.hash == wanted)
            .cloned()
    }

    async fn accept(self: Arc<Self>, listener: TcpListener, acceptor: tokio_rustls::TlsAcceptor) {
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(accepted) => accepted,
                Err(error) => {
                    eprintln!("noel server: remote accept: {error}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let (remote, acceptor) = (self.clone(), acceptor.clone());
            tokio::spawn(async move {
                let handled = tokio::time::timeout(Duration::from_secs(15), async {
                    let tls = acceptor.accept(stream).await?;
                    remote.handle(tls, peer.ip().to_string()).await
                })
                .await;
                match handled {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => eprintln!("noel server: remote {peer}: {error:#}"),
                    Err(_) => eprintln!("noel server: remote {peer}: too slow"),
                }
            });
        }
    }

    /// One request: a pairing call, or the WebSocket link of a paired device.
    async fn handle<S>(&self, mut stream: S, address: String) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (head, rest) = read_head(&mut stream).await?;
        let request = parse_head(&head)?;
        match (request.method.as_str(), request.path.as_str()) {
            ("POST", "/_chord/pair") => {
                let body = read_body(&mut stream, &request, rest).await?;
                let label = serde_json::from_slice::<Value>(&body)
                    .ok()
                    .and_then(|body| body["label"].as_str().map(str::trim).map(str::to_owned))
                    .filter(|label| !label.is_empty())
                    .map(|label| label.chars().take(100).collect())
                    .unwrap_or_else(|| "device".to_owned());
                match self.begin(label, address) {
                    Some((code, poll)) => {
                        self.publish();
                        respond(&mut stream, 200, &json!({"code": code, "poll": poll})).await
                    }
                    None => respond(&mut stream, 429, &json!({"error": "too many pairings"})).await,
                }
            }
            ("POST", "/_chord/pair/poll") => {
                let body = read_body(&mut stream, &request, rest).await?;
                let poll = serde_json::from_slice::<Value>(&body)
                    .ok()
                    .and_then(|body| body["poll"].as_str().map(str::to_owned));
                match poll.and_then(|poll| self.poll(&poll)) {
                    Some(None) => respond(&mut stream, 200, &json!({"status": "waiting"})).await,
                    Some(Some(token)) => {
                        self.publish();
                        respond(
                            &mut stream,
                            200,
                            &json!({"status": "approved", "token": token}),
                        )
                        .await
                    }
                    None => respond(&mut stream, 404, &json!({"error": "no such pairing"})).await,
                }
            }
            ("GET", "/chord") => {
                let token = request
                    .header("authorization")
                    .and_then(|value| value.strip_prefix("Bearer "))
                    .map(str::trim);
                let Some(device) = token.and_then(|token| self.device(token)) else {
                    return respond(
                        &mut stream,
                        401,
                        &json!({"error": "pair this device first"}),
                    )
                    .await;
                };
                let replayed = Replay {
                    prefix: [head, rest].concat(),
                    position: 0,
                    inner: stream,
                };
                let socket = tokio_tungstenite::accept_async(replayed).await?;
                let principal = json!({"kind": "owner", "via": "remote", "device": device.id});
                eprintln!(
                    "noel server: device {} ({}) connected",
                    device.id, device.label
                );
                self.host.accept(
                    noel_chord::v2::transport::websocket(socket),
                    LinkOptions::new(principal),
                );
                Ok(())
            }
            _ => respond(&mut stream, 404, &json!({"error": "not found"})).await,
        }
    }
}

/// Serves `noel.devices` to the owner on this machine (a paired device
/// cannot pair or revoke devices).
fn serve_devices(host: &Host, remote: Arc<Remote>) {
    host.serve(contracts::devices(), move |_params, principal| {
        let local_owner = principal.get("kind").and_then(Value::as_str) == Some("owner")
            && principal.get("via").and_then(Value::as_str) != Some("remote");
        if !local_owner {
            return Err(forbidden("noel.devices is the owner's, on this machine"));
        }
        let (approving, revoking) = (remote.clone(), remote.clone());
        Ok(Implementation::new("owner")
            .state("devices", &remote.directory)
            .method("approve", move |args: Value, _| {
                let remote = approving.clone();
                async move {
                    let id = remote.approve(args["code"].as_str().unwrap_or_default())?;
                    Ok(json!({ "id": id }))
                }
            })
            .method("revoke", move |args: Value, _| {
                let remote = revoking.clone();
                async move {
                    remote.revoke(args["id"].as_str().unwrap_or_default())?;
                    Ok(Value::Null)
                }
            }))
    });
}

fn read_devices(path: &Path) -> Vec<Device> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<Value>>(&text)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|record| {
            Some(Device {
                id: record["id"].as_str()?.to_owned(),
                label: record["label"].as_str().unwrap_or("device").to_owned(),
                created: record["created"].as_u64().unwrap_or(0),
                hash: record["hash"].as_str()?.to_owned(),
            })
        })
        .collect()
}

/// The server's TLS configuration and its certificate's fingerprint: the
/// self-signed certificate in `<agent dir>/server-tls/`, made the first
/// time.
fn server_tls(agent_dir: &Path) -> Result<(rustls::ServerConfig, String)> {
    let dir = agent_dir.join("server-tls");
    let (cert_path, key_path) = (dir.join("cert.pem"), dir.join("key.pem"));
    if !cert_path.is_file() || !key_path.is_file() {
        let generated =
            rcgen::generate_simple_self_signed(vec!["noel".to_owned(), "localhost".to_owned()])?;
        write_private(&key_path, generated.key_pair.serialize_pem().as_bytes())?;
        write_private(&cert_path, generated.cert.pem().as_bytes())?;
    }
    let cert_der = pem_block(&fs::read_to_string(&cert_path)?, "CERTIFICATE")?;
    let key_der = pem_block(&fs::read_to_string(&key_path)?, "PRIVATE KEY")?;
    let fingerprint = fingerprint(&cert_der);
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(cert_der)],
            PrivateKeyDer::Pkcs8(key_der.into()),
        )?;
    Ok((config, fingerprint))
}

/// The DER bytes of the first `label` block of a PEM file.
fn pem_block(pem: &str, label: &str) -> Result<Vec<u8>> {
    use base64::Engine;
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let start = pem
        .find(&begin)
        .ok_or_else(|| anyhow!("no {label} block"))?
        + begin.len();
    let stop = pem[start..]
        .find(&end)
        .ok_or_else(|| anyhow!("no end of {label}"))?
        + start;
    let text: String = pem[start..stop]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    Ok(base64::engine::general_purpose::STANDARD.decode(text)?)
}

struct Head {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
}

impl Head {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// The request head (up to its blank line) and what was read past it.
async fn read_head<S: AsyncRead + Unpin>(stream: &mut S) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            let rest = buffer.split_off(end + 4);
            return Ok((buffer, rest));
        }
        if buffer.len() > MAX_HEAD {
            bail!("a request head over {MAX_HEAD} bytes");
        }
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            bail!("the connection ended in a request head");
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

fn parse_head(head: &[u8]) -> Result<Head> {
    let text = std::str::from_utf8(head)?;
    let mut lines = text.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split(' ');
    let method = first.next().unwrap_or_default().to_owned();
    let path = first.next().unwrap_or_default().to_owned();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_owned(), value.trim().to_owned()))
        .collect();
    Ok(Head {
        method,
        path,
        headers,
    })
}

async fn read_body<S: AsyncRead + Unpin>(
    stream: &mut S,
    head: &Head,
    mut body: Vec<u8>,
) -> Result<Vec<u8>> {
    let length: usize = head
        .header("content-length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    if length > MAX_BODY {
        bail!("a body over {MAX_BODY} bytes");
    }
    while body.len() < length {
        let mut chunk = vec![0u8; length - body.len()];
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(length);
    Ok(body)
}

async fn respond<S: AsyncWrite + Unpin>(stream: &mut S, status: u16, body: &Value) -> Result<()> {
    let reason = match status {
        200 => "OK",
        401 => "Unauthorized",
        404 => "Not Found",
        429 => "Too Many Requests",
        _ => "Error",
    };
    let body = body.to_string();
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

/// A stream that first gives back bytes already read from it.
struct Replay<S> {
    prefix: Vec<u8>,
    position: usize,
    inner: S,
}

impl<S: AsyncRead + Unpin> AsyncRead for Replay<S> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.position < self.prefix.len() {
            let left = &self.prefix[self.position..];
            let taken = left.len().min(buf.remaining());
            buf.put_slice(&left[..taken]);
            self.position += taken;
            return std::task::Poll::Ready(Ok(()));
        }
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Replay<S> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

// ─── the client side ────────────────────────────────────────────────────────

/// A server a device paired with, as `<agent dir>/remotes.json` keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Paired {
    pub host: String,
    pub port: u16,
    pub pin: String,
    pub token: String,
}

/// `https://host:port` (or `host:port`) as host and port (default 8443).
pub(crate) fn parse_url(url: &str) -> Result<(String, u16)> {
    let rest = url
        .strip_prefix("https://")
        .unwrap_or(url)
        .trim_end_matches('/');
    match rest.rsplit_once(':') {
        Some((host, port)) => Ok((host.to_owned(), port.parse().context("a port number")?)),
        None => Ok((rest.to_owned(), 8443)),
    }
}

fn remotes_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("remotes.json")
}

/// The pairing kept for `url`.
pub(crate) fn paired(agent_dir: &Path, url: &str) -> Result<Paired> {
    let (host, port) = parse_url(url)?;
    let key = format!("{host}:{port}");
    let remotes: Value = fs::read_to_string(remotes_path(agent_dir))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    let entry = &remotes[&key];
    match (entry["pin"].as_str(), entry["token"].as_str()) {
        (Some(pin), Some(token)) => Ok(Paired {
            host,
            port,
            pin: pin.to_owned(),
            token: token.to_owned(),
        }),
        _ => bail!("this device is not paired with {key}: run `noel client pair https://{key}`"),
    }
}

fn keep_pairing(agent_dir: &Path, paired: &Paired) -> Result<()> {
    let path = remotes_path(agent_dir);
    let mut remotes: BTreeMap<String, Value> = fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    remotes.insert(
        format!("{}:{}", paired.host, paired.port),
        json!({"pin": paired.pin, "token": paired.token}),
    );
    write_private(&path, json!(remotes).to_string().as_bytes())
}

/// Accepts the server's certificate when its fingerprint is the pin, or,
/// with no pin yet (pairing), records it.
#[derive(Debug)]
struct Pinned {
    pin: Option<String>,
    seen: Mutex<Option<String>>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl rustls::client::danger::ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let seen = fingerprint(end_entity);
        *lock(&self.seen) = Some(seen.clone());
        match &self.pin {
            Some(pin) if *pin != seen => Err(rustls::Error::General(format!(
                "the server's certificate {seen} is not the paired one {pin}"
            ))),
            _ => Ok(rustls::client::danger::ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// A TLS stream to `host:port`, checking the certificate against `pin`;
/// with the fingerprint it saw.
async fn tls(
    host: &str,
    port: u16,
    pin: Option<String>,
) -> Result<(tokio_rustls::client::TlsStream<TcpStream>, String)> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(Pinned {
        pin,
        seen: Mutex::new(None),
        provider: provider.clone(),
    });
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(verifier.clone())
        .with_no_client_auth();
    let tcp = TcpStream::connect((host, port))
        .await
        .with_context(|| format!("connect to {host}:{port}"))?;
    let name = ServerName::try_from("noel")
        .expect("a server name")
        .to_owned();
    let stream = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(name, tcp)
        .await?;
    let seen = lock(&verifier.seen).clone().unwrap_or_default();
    Ok((stream, seen))
}

/// One JSON POST over a fresh TLS connection.
async fn post(
    host: &str,
    port: u16,
    pin: Option<String>,
    path: &str,
    body: &Value,
) -> Result<(u16, Value, String)> {
    let (mut stream, seen) = tls(host, port, pin).await?;
    let body = body.to_string();
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await?;
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response).await;
    let text = String::from_utf8_lossy(&response);
    let status = text
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let json = text
        .split_once("\r\n\r\n")
        .and_then(|(_, body)| serde_json::from_str(body).ok())
        .unwrap_or(Value::Null);
    Ok((status, json, seen))
}

/// `noel client pair <url>`: asks the server for a code, shows it with the
/// certificate's fingerprint, waits for the owner's approval, and keeps the
/// device's token pinned to that certificate.
pub(crate) async fn pair(agent_dir: &Path, url: &str, label: &str) -> Result<()> {
    let (host, port) = parse_url(url)?;
    let (status, begun, seen) = post(
        &host,
        port,
        None,
        "/_chord/pair",
        &json!({ "label": label }),
    )
    .await?;
    if status != 200 {
        bail!("the server refused to pair ({status}): {begun}");
    }
    let code = begun["code"].as_str().unwrap_or_default().to_owned();
    let poll = begun["poll"].as_str().unwrap_or_default().to_owned();
    println!("pairing code: {code}");
    println!("server certificate: {seen}");
    println!("On the server's machine, check the certificate (`noel client devices`) and run:");
    println!("  noel client devices approve {code}");
    let deadline = tokio::time::Instant::now() + PAIRING_TTL;
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let (status, polled, _) = post(
            &host,
            port,
            Some(seen.clone()),
            "/_chord/pair/poll",
            &json!({ "poll": poll }),
        )
        .await?;
        match (status, polled["status"].as_str()) {
            (200, Some("approved")) => {
                let token = polled["token"].as_str().unwrap_or_default().to_owned();
                keep_pairing(
                    agent_dir,
                    &Paired {
                        host: host.clone(),
                        port,
                        pin: seen,
                        token,
                    },
                )?;
                println!("paired with {host}:{port}: use `noel client --server https://{host}:{port} ...`");
                return Ok(());
            }
            (200, _) => {}
            _ => bail!("the pairing ended unapproved ({status})"),
        }
        if tokio::time::Instant::now() > deadline {
            bail!("the pairing expired");
        }
    }
}

/// The WebSocket link to a paired server.
pub(crate) async fn connect(paired: &Paired) -> Result<Link> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let (stream, _) = tls(&paired.host, paired.port, Some(paired.pin.clone())).await?;
    let mut request =
        format!("wss://{}:{}/chord", paired.host, paired.port).into_client_request()?;
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {}", paired.token).parse()?);
    let (socket, _) = tokio_tungstenite::client_async(request, stream)
        .await
        .map_err(|error| anyhow!("the server refused the link: {error}"))?;
    Ok(noel_chord::v2::transport::websocket(socket))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairing(code: &str, expires: std::time::Instant, approved: Option<&str>) -> Pairing {
        Pairing {
            code: code.into(),
            label: code.into(),
            address: "a".into(),
            expires,
            poll_hash: String::new(),
            approved: approved.map(|device| (device.to_owned(), "token".to_owned())),
        }
    }

    fn device(id: &str) -> Device {
        Device {
            id: id.into(),
            label: id.into(),
            created: 0,
            hash: String::new(),
        }
    }

    /// #175: an approved pairing whose token no poll collected takes its
    /// device when it expires; a delivered device stays.
    #[test]
    fn an_uncollected_approval_takes_its_device_when_it_expires() {
        let now = std::time::Instant::now();
        let later = now + Duration::from_secs(60);
        let mut state = State {
            devices: vec![device("kept"), device("orphan"), device("fresh")],
            pairings: vec![
                pairing("OLD1", now, Some("orphan")),
                pairing("OLD2", now, None),
                pairing("NEW1", later, Some("fresh")),
            ],
        };
        assert!(state.prune(now));
        let ids: Vec<&str> = state.devices.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, ["kept", "fresh"]);
        let codes: Vec<&str> = state.pairings.iter().map(|p| p.code.as_str()).collect();
        assert_eq!(codes, ["NEW1"]);
        assert!(!state.prune(now));
    }
}
