//! `noel-app`: noel's desktop app (roadmap phase 9, spec §14). A GPUI
//! window on `noel server`: the sessions, the selected session's
//! transcript, a composer, a command palette and the plugins' panels.
//!
//! ```text
//! noel-app [--remote <ssh-destination>] [--noel <program>] [--session <id>] [--smoke]
//! ```
//!
//! `noel app` runs it. `--smoke` connects, waits for the directory (and the
//! session's view with `--session`), prints what it saw as JSON and exits:
//! the app's link without a window, for tests.

mod backend;
mod ui;

use std::time::Duration;

use backend::{Command, Endpoint, Event};
use futures::StreamExt;
use serde_json::json;

struct Args {
    endpoint: Endpoint,
    session: Option<String>,
    smoke: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args().skip(1);
    let mut remote = None;
    let mut program = None;
    let mut session = None;
    let mut smoke = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--remote" => remote = Some(args.next().ok_or("--remote needs a destination")?),
            "--noel" => program = Some(args.next().ok_or("--noel needs a program")?),
            "--session" => session = Some(args.next().ok_or("--session needs an id")?),
            "--smoke" => smoke = true,
            "-h" | "--help" => {
                println!("noel-app [--remote <ssh-destination>] [--noel <program>] [--session <id>] [--smoke]");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    // Locally, the `noel` beside this program; remotely, `noel` on its PATH.
    let program = program.unwrap_or_else(|| {
        let beside = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join("noel")))
            .filter(|path| path.is_file() && remote.is_none());
        beside.map_or("noel".to_owned(), |path| path.display().to_string())
    });
    Ok(Args {
        endpoint: Endpoint { program, remote },
        session,
        smoke,
    })
}

fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(error) => {
            eprintln!("noel-app: {error}");
            std::process::exit(2);
        }
    };
    let (events_tx, events) = futures::channel::mpsc::unbounded::<Event>();
    let (commands, commands_rx) = tokio::sync::mpsc::unbounded_channel::<Command>();
    let endpoint = args.endpoint.clone();
    let failures = events_tx.clone();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().expect("a tokio runtime");
        if let Err(error) = runtime.block_on(backend::run(endpoint, events_tx, commands_rx)) {
            let _ = failures.unbounded_send(Event::Status(format!("connect: {error:#}")));
        }
        // Keeps the tasks the link runs alive while the window is open.
        runtime.block_on(std::future::pending::<()>());
    });
    if let Some(session) = &args.session {
        let _ = commands.send(Command::Select(session.clone()));
    }
    if args.smoke {
        std::process::exit(smoke(events, args.session.is_some()));
    }
    ui::run(events, commands, args.endpoint);
}

/// The `--smoke` run: what the link saw, as JSON, within 30 s.
fn smoke(mut events: futures::channel::mpsc::UnboundedReceiver<Event>, want_view: bool) -> i32 {
    let outcome = futures::executor::block_on(async {
        let mut sessions = None;
        let mut plugins = None;
        let mut view = None;
        let mut statuses = Vec::new();
        let deadline = futures::FutureExt::fuse(futures_timer(Duration::from_secs(30)));
        futures::pin_mut!(deadline);
        loop {
            futures::select! {
                event = events.next() => match event {
                    Some(Event::Sessions(list)) => sessions = Some(list.len()),
                    Some(Event::Plugins(list)) => plugins = Some(list.len()),
                    Some(Event::View { session, entries, .. }) => view = Some(json!({"session": session, "entries": entries.len()})),
                    Some(Event::Status(status)) => statuses.push(status),
                    Some(Event::Connected) => {}
                    None => break,
                },
                () = deadline => break,
            }
            if sessions.is_some() && plugins.is_some() && (!want_view || view.is_some()) {
                break;
            }
        }
        json!({"sessions": sessions, "plugins": plugins, "view": view, "statuses": statuses})
    });
    println!("{outcome}");
    if outcome["sessions"].is_null() {
        1
    } else {
        0
    }
}

/// A timer that needs no runtime: a thread that sleeps.
async fn futures_timer(duration: Duration) {
    let (tx, rx) = futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        std::thread::sleep(duration);
        let _ = tx.send(());
    });
    let _ = rx.await;
}
