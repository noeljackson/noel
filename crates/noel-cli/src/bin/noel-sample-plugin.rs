//! A sample noel plugin (roadmap phase 7), for the plugin tests and as an
//! example of the Rust SDK: a tool, commands and a panel.
//!
//! - tool `sample_reverse`: the text reversed;
//! - command `count`: counts one more, shown in the `counter` panel, whose
//!   `reset` button starts over;
//! - command `sessions`: how many sessions `noel.sessions` lists, which
//!   needs the permission `sessions`;
//! - command `crash`: exits, so the server starts it again.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use noel_chord::v2::host::StateUpdate;
use noel_plugin::{Context, Plugin};
use serde_json::{json, Value};

fn counter_view(count: u64) -> Value {
    json!({"type": "stack", "children": [
        {"type": "text", "text": format!("count {count}")},
        {"type": "button", "label": "Reset", "action": "reset"},
    ]})
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let count = Arc::new(AtomicU64::new(0));
    let (counted, reset) = (count.clone(), count.clone());
    Plugin::new("sample")
        .tool(
            "sample_reverse",
            "Reverses the text it is given.",
            json!({
                "type": "object",
                "required": ["text"],
                "properties": {"text": {"type": "string"}},
            }),
            |args, _| async move {
                match args["text"].as_str() {
                    Some(text) => Ok(text.chars().rev().collect()),
                    None => Err("give `text`".to_owned()),
                }
            },
        )
        .command("count", "Counts one more.", move |_, context: Context| {
            let count = counted.clone();
            async move {
                let now = count.fetch_add(1, Ordering::SeqCst) + 1;
                context.set_panel("counter", counter_view(now));
                Ok(format!("count {now}"))
            }
        })
        .command(
            "sessions",
            "How many sessions there are (needs `sessions`).",
            |_, context: Context| async move {
                let sessions = context.server(noel_client::contracts::sessions(), json!({}));
                let mut directory = sessions.subscribe("directory");
                sessions.ready().await.map_err(|error| error.to_string())?;
                match directory.next().await {
                    Some(StateUpdate::Snapshot(value)) => Ok(format!(
                        "{} sessions",
                        value["sessions"].as_array().map_or(0, Vec::len)
                    )),
                    other => Err(format!("no directory: {other:?}")),
                }
            },
        )
        .command("crash", "Exits at once.", |_, _| async {
            // On purpose: the server's restart of a plugin that dies is
            // what this command exercises.
            std::process::exit(3);
        })
        .panel(
            "counter",
            "Counter",
            counter_view(0),
            move |action, _, context| {
                let count = reset.clone();
                async move {
                    if action != "reset" {
                        return Err(format!("no action {action}"));
                    }
                    count.store(0, Ordering::SeqCst);
                    context.set_panel("counter", counter_view(0));
                    Ok(())
                }
            },
        )
        .run_stdio()
        .await;
}
