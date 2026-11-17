//! The plugin SDK against a server's side of its link (#181): what it
//! contributes, its tools, commands and panels, as `noel server` uses them.

use std::time::Duration;

use noel_chord::v2::host::{Host, HostOptions, Resolved, Route, StateUpdate};
use noel_chord::v2::message::Address;
use noel_chord::v2::object::BoxFuture;
use noel_chord::v2::{ChordError, Code, Link, LinkOptions, WireError};
use noel_plugin::Plugin;
use serde_json::{json, Value};

/// A `plugin` route that only has the links it is given, as `noel server`
/// gives it each plugin's.
struct Given;

impl Route for Given {
    fn resolve(&self, address: Address) -> BoxFuture<Result<Resolved, ChordError>> {
        Box::pin(async move {
            Err(ChordError::new(
                Code::Unavailable,
                format!("no plugin {}", address.params),
            ))
        })
    }
}

/// The server's view of a plugin served over a memory link, as `noel
/// server` links it: the `plugin` route's link for its address.
fn served(plugin: Plugin) -> (Host, tokio::task::JoinHandle<()>) {
    let (server_side, plugin_side) = Link::memory_pair();
    let task = tokio::spawn(plugin.serve(plugin_side));
    let server = Host::new(HostOptions::default());
    server.route("plugin", Given);
    server.route_link(
        noel_client::plugin_address("sample"),
        server_side,
        LinkOptions::new(json!({"kind": "plugin", "name": "sample"})),
    );
    (server, task)
}

async fn next(updates: &mut noel_chord::v2::StateStream) -> Value {
    let update = tokio::time::timeout(Duration::from_secs(5), updates.next())
        .await
        .expect("an update in time")
        .expect("an update");
    match update {
        StateUpdate::Snapshot(value) | StateUpdate::Ops { value, .. } => value,
        StateUpdate::Failed(error) => panic!("contributions failed: {error:?}"),
    }
}

#[tokio::test]
async fn a_plugin_contributes_and_answers_its_tools_commands_and_panels() {
    let plugin = Plugin::new("sample")
        .tool(
            "shout",
            "Shouts its text.",
            json!({"type": "object"}),
            |args, _| async move {
                match args["text"].as_str() {
                    Some(text) => Ok(text.to_uppercase()),
                    None => Err("no text".to_owned()),
                }
            },
        )
        .command("hello", "Says hello.", |args, _| async move {
            Ok(format!("hello {args}"))
        })
        .panel(
            "counter",
            "Counter",
            json!({"type": "text", "text": "count 0"}),
            |action, value, context| async move {
                let text = format!("{action} {}", value.unwrap_or_default());
                context.set_panel("counter", json!({"type": "text", "text": text}));
                Ok(())
            },
        );
    let (server, task) = served(plugin);
    let reference = server.get(
        noel_client::contracts::plugin(),
        json!({"plugin": "sample"}),
    );
    tokio::time::timeout(Duration::from_secs(5), reference.ready())
        .await
        .expect("the plugin binds in time")
        .expect("the plugin binds");
    let mut contributions = reference.subscribe("contributions");
    let first = next(&mut contributions).await;
    assert_eq!(first["tools"][0]["name"], "shout");
    assert_eq!(first["tools"][0]["replaySafe"], false);
    assert_eq!(first["commands"][0]["name"], "hello");
    assert_eq!(first["panels"][0]["view"]["text"], "count 0");

    let shouted = reference
        .call(
            "callTool",
            json!({"name": "shout", "args": r#"{"text":"hi"}"#}),
        )
        .await
        .unwrap();
    assert_eq!(shouted, json!({"output": "HI", "isError": false}));
    let failed = reference
        .call("callTool", json!({"name": "shout", "args": "{}"}))
        .await
        .unwrap();
    assert_eq!(failed, json!({"output": "no text", "isError": true}));
    let unknown = reference
        .call("callTool", json!({"name": "nope", "args": "{}"}))
        .await
        .unwrap();
    assert_eq!(unknown["isError"], true);

    let hello = reference
        .call("runCommand", json!({"name": "hello", "args": "there"}))
        .await
        .unwrap();
    assert_eq!(hello, json!({"output": "hello there"}));
    let missing = reference
        .call("runCommand", json!({"name": "nope"}))
        .await
        .unwrap_err();
    assert!(matches!(missing, WireError::Plugin(error) if error.code == "command"));

    // A panel action changes the view, which the contributions carry.
    reference
        .call(
            "panelAction",
            json!({"panel": "counter", "action": "add", "value": "1"}),
        )
        .await
        .unwrap();
    let changed = next(&mut contributions).await;
    assert_eq!(changed["panels"][0]["view"]["text"], "add 1");

    server.close().await;
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("the plugin ends with its link")
        .unwrap();
}
