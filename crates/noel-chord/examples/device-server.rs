//! A Chord server on an owner-only Unix socket: the counter of
//! `chord-link.json`, served over WebSocket framing on the socket, to clients
//! of the same uid.
//!
//! ```text
//! cargo run -p noel-chord --example device-server [-- [--socket PATH] [--mode 600]]
//! ```
//!
//! Each connection's peer uid is checked (`SO_PEERCRED` on Linux,
//! `getpeereid` elsewhere) before a byte is read; another uid is closed at
//! once. The WebSocket upgrade must ask for `/chord`. Accepted links act as
//! `{kind: "owner", via: "device"}`. The log shows links, holds and releases,
//! so a client that is killed is seen to release its holds.

#[path = "support/counter.rs"]
mod counter;

use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use noel_chord::host::{AcceptOptions, Host, HostEvent, HostOptions};
use noel_chord::link::{websocket, WebSocketOptions};
use serde_json::json;
use tokio::net::UnixListener;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;

/// The uid of the process at the other end of a Unix socket.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn peer_uid(fd: RawFd) -> io::Result<u32> {
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `credentials` and `length` are valid for writes of the size passed.
    let result = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(credentials.uid)
}

/// The uid of the process at the other end of a Unix socket.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn peer_uid(fd: RawFd) -> io::Result<u32> {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: `uid` and `gid` are valid for writes.
    if unsafe { libc::getpeereid(fd, &mut uid, &mut gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(uid)
}

/// The WebSocket upgrade must ask for `/chord`. The error type is
/// tungstenite's.
#[allow(clippy::result_large_err)]
fn check_path(request: &Request, response: Response) -> Result<Response, ErrorResponse> {
    if request.uri().path() == "/chord" {
        return Ok(response);
    }
    let mut refusal = ErrorResponse::new(Some("not found".to_owned()));
    *refusal.status_mut() = StatusCode::NOT_FOUND;
    Err(refusal)
}

fn own_uid() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

fn default_socket() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir).join("noel-chord-device.sock"),
        None => PathBuf::from(format!("/tmp/noel-chord-device-{}.sock", own_uid())),
    }
}

fn arguments() -> Result<(PathBuf, u32), String> {
    let mut socket = default_socket();
    let mut mode = 0o600;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => socket = args.next().ok_or("--socket needs a path")?.into(),
            "--mode" => {
                let text = args.next().ok_or("--mode needs an octal mode")?;
                mode = u32::from_str_radix(&text, 8).map_err(|_| format!("bad mode {text}"))?;
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok((socket, mode))
}

fn log_event(event: &HostEvent) {
    match event {
        HostEvent::LinkOpened {
            link,
            kind,
            principal,
        } => {
            let principal = principal
                .as_ref()
                .map(|p| p.to_string())
                .unwrap_or_default();
            println!("link {link} opened: {} {principal}", kind.as_str());
        }
        HostEvent::LinkClosed { link, reason } => println!("link {link} closed: {reason}"),
        HostEvent::Held {
            link,
            id,
            type_name,
            input,
            ..
        } => {
            println!("hold link {link} id {id}: {type_name} {input}")
        }
        HostEvent::Released {
            link,
            id,
            type_name,
            input,
            ..
        } => {
            println!("release link {link} id {id}: {type_name} {input}")
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let (socket, mode) = match arguments() {
        Ok(arguments) => arguments,
        Err(error) => {
            eprintln!("device-server: {error}");
            return ExitCode::from(2);
        }
    };
    let _ = std::fs::remove_file(&socket);
    let listener = match UnixListener::bind(&socket) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("device-server: bind {}: {error}", socket.display());
            return ExitCode::FAILURE;
        }
    };
    if let Err(error) = std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(mode)) {
        eprintln!("device-server: chmod {}: {error}", socket.display());
        return ExitCode::FAILURE;
    }
    let host = Host::new(HostOptions {
        on_error: Some(Arc::new(|error| eprintln!("chord: {error}"))),
        observer: Some(Arc::new(log_event)),
        ..HostOptions::host("server")
    });
    counter::serve(&host);
    let owner = own_uid();
    println!(
        "listening {} (uid {owner}, mode {mode:o})",
        socket.display()
    );
    loop {
        let stream = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => stream,
                Err(error) => {
                    eprintln!("device-server: accept: {error}");
                    continue;
                }
            },
            _ = tokio::signal::ctrl_c() => break,
        };
        // Before reading a byte: only the owner's processes get a link.
        match peer_uid(stream.as_raw_fd()) {
            Ok(uid) if uid == owner => {}
            Ok(uid) => {
                println!("refused uid {uid}: not the owner");
                drop(stream);
                continue;
            }
            Err(error) => {
                println!("refused: no peer credentials: {error}");
                drop(stream);
                continue;
            }
        }
        let host = host.clone();
        tokio::spawn(async move {
            match tokio_tungstenite::accept_hdr_async(stream, check_path).await {
                Ok(socket) => {
                    let link = websocket(
                        socket,
                        WebSocketOptions {
                            accept_packed: false,
                        },
                    );
                    host.accept(
                        link,
                        AcceptOptions::client(json!({"kind": "owner", "via": "device"})),
                    );
                }
                Err(error) => println!("refused: websocket upgrade: {error}"),
            }
        });
    }
    host.close().await;
    let _ = std::fs::remove_file(&socket);
    println!("closed");
    ExitCode::SUCCESS
}
