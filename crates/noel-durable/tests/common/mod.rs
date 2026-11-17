//! The tests' scripted provider and harness options (slice 1's tests
//! and the durability tests share them). Not every helper is used by
//! every test binary that includes this module.
#![allow(dead_code)]

use std::path::Path;
use std::sync::{Arc, Mutex};

use noel_ai::{Provider, ProviderError, ProviderRequest, StreamEvent};
use noel_durable::{
    HarnessOptions, ProviderOptions, ProviderResolver, ProviderSource, ToolRegistry,
};
use serde_json::Value;

/// A scripted provider: each request pops the next scripted reply, and
/// every request is recorded (what the model was asked). A reply may
/// `Hold` until the test releases it, as upstream's `slow` faux step
/// does, so a conversation stays busy while more input is submitted.
#[derive(Default)]
pub struct Scripted {
    pub replies: Mutex<Vec<ScriptedReply>>,
    pub requests: Mutex<Vec<Vec<String>>>,
}

pub enum ScriptedReply {
    Events(Vec<StreamEvent>),
    Hold(tokio::sync::oneshot::Receiver<Vec<StreamEvent>>),
}

#[async_trait::async_trait]
impl Provider for Scripted {
    async fn complete(&self, _request: ProviderRequest) -> Result<Vec<StreamEvent>, ProviderError> {
        unreachable!("runs stream their replies")
    }

    async fn complete_streaming(
        &self,
        request: ProviderRequest,
        on_event: &mut (dyn FnMut(StreamEvent) -> Result<(), ProviderError> + Send),
    ) -> Result<(), ProviderError> {
        self.requests.lock().unwrap().push(
            request
                .messages
                .iter()
                .map(|message| message.content.clone())
                .collect(),
        );
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop()
            .expect("a request the script did not expect");
        let events = match reply {
            ScriptedReply::Events(events) => events,
            ScriptedReply::Hold(held) => held.await.expect("the held reply was not released"),
        };
        for event in events {
            on_event(event)?;
        }
        Ok(())
    }
}

impl Scripted {
    pub fn text(reply: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::Text(reply.to_owned()),
            StreamEvent::Stop {
                reason: "stop".into(),
            },
        ]
    }

    pub fn tool_call(name: &str, arguments: Value) -> Vec<StreamEvent> {
        vec![
            StreamEvent::ToolCall {
                id: "call_1".into(),
                name: name.into(),
                arguments: arguments.to_string(),
            },
            StreamEvent::Stop {
                reason: "tool_use".into(),
            },
        ]
    }
}

/// A shim so one script serves any model (the harness asks for one
/// provider per run).
struct ScriptSource(Arc<Scripted>);

impl ProviderSource for ScriptSource {
    fn provider(&self, _model: &noel_ai::ModelRef) -> Option<Box<dyn Provider>> {
        Some(Box::new(ScriptedShim(self.0.clone())))
    }
}

struct ScriptedShim(Arc<Scripted>);

#[async_trait::async_trait]
impl Provider for ScriptedShim {
    async fn complete(&self, _request: ProviderRequest) -> Result<Vec<StreamEvent>, ProviderError> {
        unreachable!("runs stream their replies")
    }

    async fn complete_streaming(
        &self,
        request: ProviderRequest,
        on_event: &mut (dyn FnMut(StreamEvent) -> Result<(), ProviderError> + Send),
    ) -> Result<(), ProviderError> {
        self.0.complete_streaming(request, on_event).await
    }
}

pub fn options_with(script: Arc<Scripted>, tools: ToolRegistry) -> HarnessOptions {
    HarnessOptions {
        providers: ProviderResolver::new(ProviderOptions::faux())
            .with_source(Arc::new(ScriptSource(script))),
        tools,
        model: Some(noel_ai::ModelRef {
            provider: "faux".into(),
            id: "script".into(),
        }),
        ..HarnessOptions::new(Path::new("/tmp"))
    }
}
