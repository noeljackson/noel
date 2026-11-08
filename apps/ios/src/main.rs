mod backend;
mod ui;

use backend::{Command, Config, Endpoint, Event};

fn main() {
    let (events_tx, events) = futures::channel::mpsc::unbounded::<Event>();
    let (commands, commands_rx) = tokio::sync::mpsc::unbounded_channel::<Command>();
    let (endpoint_tx, mut endpoint_rx) = tokio::sync::mpsc::unbounded_channel::<Endpoint>();
    let failures = events_tx.clone();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().expect("a tokio runtime");
        let Some(endpoint) = runtime.block_on(endpoint_rx.recv()) else {
            return;
        };
        let app_home =
            std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".noel");
        if let Err(error) = runtime.block_on(backend::run(
            Config::new(endpoint, app_home),
            events_tx,
            commands_rx,
        )) {
            let _ = failures.unbounded_send(Event::Status(format!("connect: {error:#}")));
        }
        runtime.block_on(std::future::pending::<()>());
    });
    ui::run(events, commands, endpoint_tx);
}
