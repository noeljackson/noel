//! How the SDK's conversations build providers: models a program names,
//! with credentials, out of the CLI. The plan's slice 1 moves provider
//! resolution out of `noel-cli`; this is the SDK's own resolution, and
//! `noel-cli` keeps its OAuth refresh and account failover as a
//! `ProviderSource` it installs itself (the worker's path, unchanged).

use noel_ai::{MediaInput, Provider, ProviderApi};
use noel_core::controller::Image;
use noel_core::Runtime;
use noel_providers::ModelSpec;
use noel_providers::{create_provider, Credential, CredentialResolver};

/// One model a program runs on: Durable's `{ provider, modelId }`, with
/// the credential it uses. A model without a credential is allowed for
/// OpenAI-compatible endpoints (`base_url` set): a local vLLM has no key
/// (the plan's "a missing key allowed").
#[derive(Debug, Clone)]
pub struct ModelEntry {
    pub spec: ModelSpec,
    pub credential: Option<Credential>,
}

/// How a harness's providers are built: the models it knows, how their
/// credentials resolve, and whatever else supplies providers the models
/// list does not name (the CLI's refreshing one).
#[derive(Clone, Default)]
pub struct ProviderOptions {
    /// The models this harness can run, in preference order.
    pub models: Vec<ModelEntry>,
    /// How credentials resolve for a model without one of its own.
    /// The default resolves from the environment and noel's login files;
    /// `environment_only` for a program that owns its credentials.
    pub credentials: CredentialResolver,
}

impl ProviderOptions {
    /// Only faux models: offline, in-process, no credentials.
    pub fn faux() -> ProviderOptions {
        ProviderOptions {
            models: vec![ModelEntry {
                spec: ModelSpec {
                    provider: "faux".into(),
                    id: "echo".into(),
                    name: "Faux Echo".into(),
                    api: ProviderApi::Faux,
                    base_url: None,
                    context_window: None,
                    max_output_tokens: None,
                    usage_scope: noel_providers::UsageScope::default(),
                },
                credential: None,
            }],
            credentials: CredentialResolver::environment_only(),
        }
    }
}

/// A source of providers beyond the models list: `noel-cli` installs one
/// that refreshes OAuth credentials per request and fails over between
/// accounts.
pub trait ProviderSource: Send + Sync {
    fn provider(&self, model: &noel_ai::ModelRef) -> Option<Box<dyn Provider>>;
}

/// The resolver a harness's host calls: the session's active model, as
/// its models and sources build it.
#[derive(Clone, Default)]
pub struct ProviderResolver {
    options: ProviderOptions,
    /// Installed by a host with its own provider machinery.
    source: Option<std::sync::Arc<dyn ProviderSource>>,
}

impl ProviderResolver {
    pub fn new(options: ProviderOptions) -> ProviderResolver {
        ProviderResolver {
            options,
            source: None,
        }
    }

    /// Delegates models the list does not name to `source`.
    pub fn with_source(mut self, source: std::sync::Arc<dyn ProviderSource>) -> Self {
        self.source = Some(source);
        self
    }

    /// The model a reference names, as this resolver knows it.
    fn entry(&self, model: &noel_ai::ModelRef) -> Option<&ModelEntry> {
        self.options
            .models
            .iter()
            .find(|entry| entry.spec.provider == model.provider && entry.spec.id == model.id)
    }

    /// The provider for the session's active model.
    pub async fn provider_for(&self, runtime: &Runtime) -> Result<Box<dyn Provider>, String> {
        let session = runtime.session();
        let model = session
            .active_model
            .clone()
            .ok_or_else(|| "no active model; set one in HarnessOptions".to_string())?;
        if let Some(source) = &self.source
            && let Some(provider) = source.provider(&model)
        {
            return Ok(provider);
        }
        let spec = match self.entry(&model) {
            Some(entry) => entry.spec.clone(),
            // A model the list does not name and no source builds: faux
            // models never need one; anything else is a configuration
            // error a program should hear about.
            None if model.provider == "faux" => ModelSpec {
                provider: model.provider.clone(),
                id: model.id.clone(),
                name: String::new(),
                api: ProviderApi::Faux,
                base_url: None,
                context_window: None,
                max_output_tokens: None,
                usage_scope: noel_providers::UsageScope::default(),
            },
            None => {
                return Err(format!(
                    "model {}/{} is not in the harness's models",
                    model.provider, model.id
                ))
            }
        };
        let credential = self
            .entry(&model)
            .and_then(|entry| entry.credential.clone())
            .or_else(|| {
                self.options
                    .credentials
                    .resolve(&model.provider, None)
                    .map(|resolved| resolved.credential)
            });
        // A custom endpoint (base_url set) serves without a credential;
        // everything else refuses without one.
        let credential = match (credential, spec.base_url.is_some()) {
            (Some(credential), _) => Some(credential),
            (None, true) => None,
            (None, false) if spec.provider == "faux" => None,
            (None, false) => None,
        };
        create_provider(&spec, credential, None).map_err(|error| error.to_string())
    }

    /// A prompt's images as the run sends them: base64 data with its
    /// mime type, as the protocol carries them.
    pub fn media(&self, images: &[Image]) -> Result<Vec<MediaInput>, String> {
        images
            .iter()
            .map(|image| {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD
                    .decode(&image.data)
                    .map_err(|error| format!("image is not base64: {error}"))?;
                Ok(MediaInput {
                    mime_type: image.mime_type.clone(),
                    data_base64: image.data.clone(),
                    path: None,
                    width: None,
                    height: None,
                })
            })
            .collect()
    }
}

/// Paths for a harness without a user's noel installation: nothing on the
/// user's disk is read or written.
pub(crate) fn isolated_paths() -> noel_config::ConfigPaths {
    noel_config::ConfigPaths {
        cwd: std::path::PathBuf::new(),
        agent_dir: std::path::PathBuf::new(),
        session_dir: std::path::PathBuf::new(),
        settings_path: std::path::PathBuf::new(),
        project_settings_path: std::path::PathBuf::new(),
        auth_path: std::path::PathBuf::new(),
        models_path: std::path::PathBuf::new(),
        model_cache_path: std::path::PathBuf::new(),
        keybindings_path: std::path::PathBuf::new(),
    }
}
