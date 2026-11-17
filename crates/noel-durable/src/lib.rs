//! The Durable SDK (plan: `docs/durable-sdk-2026-11.md`, issue #210).
//!
//! A program embeds noel's session engine in-process, the way upstream
//! Durable's hosts embed its Harness: open a [`Harness`] on storage, submit
//! input to its [`Conversation`], and await the [`Submission`]. Slice 1 is
//! the embedding host: one conversation over one journal, an owned event
//! stream, in-process Rust tools, and provider resolution out of the CLI.
//!
//! What this wraps is `noel-core`'s `SessionController`, the one driver of
//! runs (`docs/spec.md` §9) that RPC mode and the server's workers already
//! host. The SDK is a third host, for programs: it owns the driver task
//! (the worker's command loop, cut to the operations a program sends), and
//! its handle methods are request/response over it.

mod driver;
mod events;
mod host;
mod provider;
mod submission;
mod tools;

pub use events::{Outcome, OwnedEvent};
pub use noel_providers::ModelSpec;
pub use provider::{ProviderOptions, ProviderResolver, ProviderSource};
pub use submission::{Settled, Submission, SubmissionState};
pub use tools::{Tool, ToolContext, ToolRegistry, ToolResult};

use std::path::PathBuf;
use std::sync::Arc;

use noel_config::LoadedConfig;
use noel_core::storage::Storage;

pub use noel_core::storage::MemoryJournal;

/// A program's error.
pub type Error = anyhow::Error;

/// Opening or using a harness failed.
#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    #[error("storage: {0}")]
    Storage(#[from] noel_core::SessionError),
    #[error("provider: {0}")]
    Provider(String),
}

/// An input the conversation is asked to admit (Durable's
/// `SubmissionDraft`, the kinds a plain chat host sends).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// A user message. A busy conversation queues it as a follow-up.
    Prompt {
        content: String,
        /// Base64 images attached to the prompt.
        images: Vec<InputImage>,
    },
    /// A steer: queued for the active run's next provider round. A busy
    /// conversation only; an idle one answers it as a prompt.
    Steer { content: String },
    /// A follow-up: queued for after the active run answers.
    FollowUp { content: String },
    /// A continue: start a run on the context as it is (idle only).
    Continue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputImage {
    pub data: String,
    pub mime_type: String,
}

impl Input {
    /// A prompt with no images.
    pub fn prompt(content: impl Into<String>) -> Input {
        Input::Prompt {
            content: content.into(),
            images: Vec::new(),
        }
    }

    pub fn steer(content: impl Into<String>) -> Input {
        Input::Steer {
            content: content.into(),
        }
    }

    pub fn follow_up(content: impl Into<String>) -> Input {
        Input::FollowUp {
            content: content.into(),
        }
    }
}

/// A submission's id: the caller's `requestId` (upstream's dedup key: a
/// repeat returns the first submission and writes nothing), or a fresh one
/// the admission names.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum InputId {
    #[default]
    Fresh,
    RequestId(String),
}

/// Why a submission was not admitted. Nothing was written.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SubmitError {
    /// A run is active and the input said to reject instead of queue
    /// (Durable's `ConversationBusy`); a continue while busy, too.
    #[error("conversation is busy")]
    Busy,
    /// Its `requestId` names a submission of another kind.
    #[error("request id {0} names a {1} submission")]
    Reused(String, String),
    /// The host refused it (no provider for the model, bad image).
    #[error("{0}")]
    Refused(String),
}

impl From<noel_core::controller::SubmitError> for SubmitError {
    fn from(error: noel_core::controller::SubmitError) -> Self {
        match error {
            noel_core::controller::SubmitError::Busy => SubmitError::Busy,
            noel_core::controller::SubmitError::Reused { request_id, kind } => {
                SubmitError::Reused(request_id, kind.as_str().to_owned())
            }
            noel_core::controller::SubmitError::QueuedPrompt => {
                SubmitError::Refused("a prompt with a request id cannot be queued".into())
            }
            noel_core::controller::SubmitError::Refused(reason) => SubmitError::Refused(reason),
        }
    }
}

/// Builds the environment one tool call runs in (upstream Durable's
/// `env(target)` hook); an `Err` is that call's error result.
pub type EnvHook = noel_core::EnvHook;

/// The target a tool call's environment is built for.
pub use noel_core::EnvTarget;

/// What a harness is opened with.
pub struct HarnessOptions {
    /// The config its models and settings come from. A program without a
    /// user's noel installation builds one with
    /// [`ProviderOptions::minimal`].
    pub config: LoadedConfig,
    /// How the conversation's providers are built.
    pub providers: ProviderResolver,
    /// In-process Rust tools, offered to the model beside noel's built-ins.
    pub tools: ToolRegistry,
    /// The model the session runs on; `None` keeps the session's own.
    pub model: Option<noel_ai::ModelRef>,
    /// The working directory the session reports and its file tools use.
    pub cwd: PathBuf,
    /// Builds each tool call's environment; the default is a local
    /// environment at `cwd` (upstream's `NodeExecutionEnv` equivalent).
    pub env: Option<EnvHook>,
}

impl HarnessOptions {
    /// The empty options: faux providers, no extra tools, no model chosen.
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        let cwd = cwd.into();
        Self {
            config: empty_config(cwd.clone()),
            providers: ProviderResolver::default(),
            tools: ToolRegistry::new(),
            model: None,
            cwd,
            env: None,
        }
    }
}

/// An empty config: no models, no settings, nothing read from a user's
/// noel installation. A program names its own models in
/// [`ProviderOptions`].
pub fn empty_config(cwd: std::path::PathBuf) -> LoadedConfig {
    let paths = noel_config::ConfigPaths {
        cwd,
        ..provider::isolated_paths()
    };
    noel_config::load_config(paths).unwrap_or(LoadedConfig {
        paths: noel_config::ConfigPaths {
            cwd: std::path::PathBuf::new(),
            agent_dir: std::path::PathBuf::new(),
            session_dir: std::path::PathBuf::new(),
            settings_path: std::path::PathBuf::new(),
            project_settings_path: std::path::PathBuf::new(),
            auth_path: std::path::PathBuf::new(),
            models_path: std::path::PathBuf::new(),
            model_cache_path: std::path::PathBuf::new(),
            keybindings_path: std::path::PathBuf::new(),
        },
        settings: Default::default(),
        auth: Default::default(),
        models: Vec::new(),
        model_scope: Vec::new(),
        image_models: Vec::new(),
        keybindings: Vec::new(),
        context_files: Vec::new(),
        extensions: Vec::new(),
        system_prompt: None,
        append_system_prompt: Vec::new(),
        agents: Vec::new(),
        skills: Vec::new(),
        mcp_servers: Vec::new(),
        prompt_templates: Vec::new(),
        themes: Vec::new(),
        diagnostics: Vec::new(),
    })
}

/// What a host opens and drives: storage, models, tools, and the event
/// stream everything reports to. `close` stops the driver; dropping the
/// harness stops it too, and the journal holds what was saved.
pub struct Harness {
    driver: driver::Handle,
    session_id: String,
}

impl Harness {
    /// Opens a session on `storage` (a fresh one when it is empty, the
    /// session it holds otherwise), with `options`.
    pub async fn open(storage: Storage, options: HarnessOptions) -> Result<Harness, HarnessError> {
        use noel_core::{Runtime, SessionStore};
        // An empty journal starts a session in it (upstream opens fresh
        // storage); one that holds a session opens it, and a run it says
        // was interrupted resumes at open.
        let empty = storage.read().is_ok_and(|bytes| bytes.is_empty())
            || storage
                .read()
                .err()
                .is_some_and(|error| matches!(error, noel_core::SessionError::Read { source, .. } if source.kind() == std::io::ErrorKind::NotFound));
        let (store, state) = if empty {
            let cwd = options.cwd.clone();
            SessionStore::create_in_storage(storage, cwd, "s1")?
        } else {
            SessionStore::open_storage(storage)?
        };
        let systems = noel_core::ReloadableSystems::from_config(&options.config, 1);
        let mut runtime = Runtime::with_store(state, systems, store);
        let model = options.model.clone();
        if let Some(model) = model {
            runtime
                .set_active_model(Some(model))
                .map_err(|error| HarnessError::Provider(error.to_string()))?;
        }
        let session_id = runtime.session().session_id.clone();
        let driver = driver::spawn(runtime, options).await?;
        Ok(Harness { driver, session_id })
    }

    /// Starts a new session named `session_id` in an empty in-memory
    /// journal (`MemoryStorage`, upstream's tests' default).
    pub async fn in_memory(options: HarnessOptions) -> Result<Harness, HarnessError> {
        let storage: Storage = Arc::new(MemoryJournal::new(PathBuf::from("memory://session")));
        let (store, state) = noel_core::SessionStore::create_in_storage(
            storage,
            options.cwd.clone(),
            "s1".to_string(),
        )?;
        Self::open_on(store, state, options).await
    }

    async fn open_on(
        store: noel_core::SessionStore,
        state: noel_core::SessionState,
        options: HarnessOptions,
    ) -> Result<Harness, HarnessError> {
        let systems = noel_core::ReloadableSystems::from_config(&options.config, 1);
        let mut runtime = noel_core::Runtime::with_store(state, systems, store);
        let model = options.model.clone();
        if let Some(model) = model {
            runtime
                .set_active_model(Some(model))
                .map_err(|error| HarnessError::Provider(error.to_string()))?;
        }
        let session_id = runtime.session().session_id.clone();
        let driver = driver::spawn(runtime, options).await?;
        Ok(Harness { driver, session_id })
    }

    /// The session's id.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The conversation this harness drives: its one session.
    pub fn conversation(&self) -> Conversation<'_> {
        Conversation { harness: self }
    }

    /// The events that happened since the last take, in order.
    pub async fn take_events(&mut self) -> Vec<OwnedEvent> {
        self.driver.take_events().await
    }

    /// Subscribes to events as they happen.
    pub fn subscribe(&self) -> events::Subscription {
        self.driver.subscribe()
    }

    /// Stops the driver and closes the session. Any active run is closed
    /// (its resume counts no restart); nothing else is written.
    pub async fn close(self) -> Result<(), Error> {
        self.driver.close().await
    }
}

/// The conversation a harness drives: submissions in, events and answers
/// out. Durable's `Conversation` for one session.
pub struct Conversation<'h> {
    harness: &'h Harness,
}

impl Conversation<'_> {
    /// Submits `input` named by `id`. An idle conversation starts a run and
    /// returns its submission as running; a busy one queues it (a steer or
    /// a follow-up, or a prompt as a follow-up by default) or refuses it
    /// ([`SubmitError::Busy`] is never queued).
    pub async fn submit(&mut self, input: Input, id: InputId) -> Result<Submission, SubmitError> {
        self.harness.driver.submit(input, id).await
    }

    /// Withdraws a queued submission, unanswered. Refused once its run has
    /// placed it or settled.
    pub async fn withdraw(&mut self, submission_id: &str) -> Result<Withdrawn, String> {
        self.harness.driver.withdraw(submission_id).await
    }

    /// Aborts the active run, withdrawing everything queued with it
    /// (Durable's rule, the SDK's policy).
    pub async fn abort(&mut self) -> Result<(), String> {
        self.harness.driver.abort().await
    }

    /// The queue as it is now: steering, then follow-ups.
    pub async fn queue(&mut self) -> Vec<QueuedItem> {
        self.harness.driver.queue().await
    }
}

/// What withdrawing a submission found.
pub use noel_core::controller::Withdrawal as Withdrawn;

/// One queued submission, as the queue reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedItem {
    /// The journal's identity of the submission.
    pub submission_id: String,
    pub request_id: Option<String>,
    /// `steer` or `followUp`.
    pub mode: &'static str,
    pub content: String,
}

impl std::fmt::Debug for HarnessOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HarnessOptions")
            .field("model", &self.model)
            .field("cwd", &self.cwd)
            .finish_non_exhaustive()
    }
}
