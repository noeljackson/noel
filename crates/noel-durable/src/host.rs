//! The SDK's host of the controller: providers, tools, and the event
//! recorder. The server's `WorkerHost`, for programs.

use std::sync::{Arc, Mutex};

use noel_ai::{MediaInput, Provider};
use noel_core::controller::{Event as ControllerEvent, Host as ControllerHost, Image};
use noel_core::Runtime;

use crate::events::{self, Events};
use crate::provider::ProviderResolver;
use crate::submission::Settled;
use crate::tools::ToolRegistry;

pub(super) struct SdkHost {
    pub(super) providers: Arc<ProviderResolver>,
    pub(super) tools: Mutex<ToolRegistry>,
    pub(super) events: Arc<Events>,
    /// Waiters for the active run's settle: what a busy submit's reply
    /// resolves on when the run ends.
    pub(super) waiters: Mutex<Vec<tokio::sync::oneshot::Sender<Settled>>>,
    /// The durable task definitions (slice 3).
    pub(super) tasks: crate::tasks::TaskRegistry,
}

impl ControllerHost for SdkHost {
    async fn provider(&mut self, runtime: &Runtime) -> Result<Box<dyn Provider>, String> {
        self.providers.provider_for(runtime).await
    }

    fn media(&self, images: &[Image]) -> Result<Vec<MediaInput>, String> {
        self.providers.media(images)
    }

    fn expand(&self, text: &str) -> String {
        text.to_owned()
    }

    fn event(&mut self, event: ControllerEvent<'_>) {
        events::record(&self.events, event);
    }
}
