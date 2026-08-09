//! Shared model catalog, credential discovery, and provider construction.
//!
//! This crate deliberately does not read Pi's settings, model cache, or auth
//! database. Callers can combine the compiled catalog with their own model
//! overrides and may opt into the standalone Codex/Claude login readers.

use std::collections::BTreeMap;
use std::env;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use pi_ai::{
    create_provider_with_client, ModelRef, Provider, ProviderApi, ProviderAuth, ProviderConfig,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageScope {
    #[default]
    General,
    CodingSubscription,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSpec {
    pub provider: String,
    pub id: String,
    pub name: String,
    pub api: ProviderApi,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    #[serde(default)]
    pub usage_scope: UsageScope,
}

impl ModelSpec {
    pub fn model_ref(&self) -> ModelRef {
        ModelRef {
            provider: self.provider.clone(),
            id: self.id.clone(),
        }
    }

    pub fn qualified_name(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }
}

/// The catalog intentionally contains a small, curated switching set. Apps can
/// append custom models without waiting for a Pi release.
pub fn builtin_models() -> Vec<ModelSpec> {
    vec![
        model(
            "faux",
            "echo",
            "Faux Echo",
            ProviderApi::Faux,
            None,
            None,
            UsageScope::General,
        ),
        model(
            "openai",
            "gpt-5.4",
            "OpenAI GPT 5.4",
            ProviderApi::OpenAiResponses,
            None,
            Some(1_050_000),
            UsageScope::General,
        ),
        model(
            "openai",
            "gpt-4.1",
            "OpenAI GPT 4.1",
            ProviderApi::OpenAi,
            None,
            Some(1_047_576),
            UsageScope::General,
        ),
        model(
            "openai-codex",
            "gpt-5.4",
            "Codex GPT 5.4",
            ProviderApi::OpenAiCodexResponses,
            Some("https://chatgpt.com/backend-api"),
            Some(400_000),
            UsageScope::CodingSubscription,
        ),
        model(
            "openai-codex",
            "gpt-5.3-codex",
            "Codex GPT 5.3",
            ProviderApi::OpenAiCodexResponses,
            Some("https://chatgpt.com/backend-api"),
            Some(400_000),
            UsageScope::CodingSubscription,
        ),
        model(
            "anthropic",
            "claude-opus-4-8",
            "Claude Opus 4.8",
            ProviderApi::Anthropic,
            None,
            Some(200_000),
            UsageScope::General,
        ),
        model(
            "anthropic",
            "claude-sonnet-4-6",
            "Claude Sonnet 4.6",
            ProviderApi::Anthropic,
            None,
            Some(200_000),
            UsageScope::General,
        ),
        model(
            "zai",
            "glm-5.1",
            "GLM 5.1",
            ProviderApi::OpenAi,
            Some("https://api.z.ai/api/paas/v4"),
            Some(202_752),
            UsageScope::General,
        ),
        model(
            "zai-coding",
            "glm-5.1",
            "GLM 5.1 Coding Plan",
            ProviderApi::OpenAi,
            Some("https://api.z.ai/api/coding/paas/v4"),
            Some(202_752),
            UsageScope::CodingSubscription,
        ),
        model(
            "zai-anthropic",
            "glm-5.1",
            "GLM 5.1 Anthropic API",
            ProviderApi::Anthropic,
            Some("https://api.z.ai/api/anthropic"),
            Some(202_752),
            UsageScope::General,
        ),
        model(
            "moonshotai",
            "k3",
            "Kimi K3",
            ProviderApi::OpenAi,
            Some("https://api.moonshot.ai/v1"),
            Some(262_144),
            UsageScope::General,
        ),
        model(
            "moonshotai",
            "k3-256k",
            "Kimi K3 256K",
            ProviderApi::OpenAi,
            Some("https://api.moonshot.ai/v1"),
            Some(262_144),
            UsageScope::General,
        ),
        model(
            "kimi-coding-openai",
            "kimi-for-coding",
            "Kimi For Coding (OpenAI API)",
            ProviderApi::OpenAi,
            Some("https://api.kimi.com/coding/v1"),
            Some(262_144),
            UsageScope::CodingSubscription,
        ),
        model(
            "kimi-coding",
            "kimi-for-coding",
            "Kimi For Coding (Anthropic API)",
            ProviderApi::Anthropic,
            Some("https://api.kimi.com/coding"),
            Some(262_144),
            UsageScope::CodingSubscription,
        ),
    ]
}

fn model(
    provider: &str,
    id: &str,
    name: &str,
    api: ProviderApi,
    base_url: Option<&str>,
    context_window: Option<u64>,
    usage_scope: UsageScope,
) -> ModelSpec {
    ModelSpec {
        provider: provider.to_string(),
        id: id.to_string(),
        name: name.to_string(),
        api,
        base_url: base_url.map(str::to_string),
        context_window,
        max_output_tokens: None,
        usage_scope,
    }
}

pub fn merge_models(base: Vec<ModelSpec>, overrides: Vec<ModelSpec>) -> Vec<ModelSpec> {
    let mut models = BTreeMap::new();
    for model in base.into_iter().chain(overrides) {
        models.insert((model.provider.clone(), model.id.clone()), model);
    }
    models.into_values().collect()
}

pub fn find_model<'a>(models: &'a [ModelSpec], reference: &str) -> Option<&'a ModelSpec> {
    let reference = reference.trim();
    if let Some((provider, id)) = reference.split_once('/') {
        return models
            .iter()
            .find(|model| model.provider == provider && model.id == id);
    }
    models.iter().find(|model| model.id == reference)
}

#[derive(Clone, PartialEq, Eq)]
pub enum Credential {
    ApiKey(String),
    ClaudeCodeOAuth {
        access_token: String,
    },
    ChatGptOAuth {
        access_token: String,
        account_id: Option<String>,
    },
}

impl fmt::Debug for Credential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ApiKey(_) => formatter.write_str("ApiKey([REDACTED])"),
            Self::ClaudeCodeOAuth { .. } => {
                formatter.write_str("ClaudeCodeOAuth { access_token: [REDACTED] }")
            }
            Self::ChatGptOAuth { account_id, .. } => formatter
                .debug_struct("ChatGptOAuth")
                .field("access_token", &"[REDACTED]")
                .field("account_id", account_id)
                .finish(),
        }
    }
}

impl From<Credential> for ProviderAuth {
    fn from(value: Credential) -> Self {
        match value {
            Credential::ApiKey(key) => Self::ApiKey(key),
            Credential::ClaudeCodeOAuth { access_token } => Self::ClaudeCodeOAuth { access_token },
            Credential::ChatGptOAuth {
                access_token,
                account_id,
            } => Self::ChatGptOAuth {
                access_token,
                account_id,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSource {
    Explicit,
    Environment,
    CodexLogin,
    ClaudeCodeLogin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCredential {
    pub credential: Credential,
    pub source: CredentialSource,
}

#[derive(Debug, Clone)]
pub struct CredentialResolver {
    home: Option<PathBuf>,
    allow_login_files: bool,
}

impl Default for CredentialResolver {
    fn default() -> Self {
        Self::standalone()
    }
}

impl CredentialResolver {
    pub fn standalone() -> Self {
        Self {
            home: home_dir(),
            allow_login_files: true,
        }
    }

    pub fn environment_only() -> Self {
        Self {
            home: None,
            allow_login_files: false,
        }
    }

    pub fn with_home(home: PathBuf) -> Self {
        Self {
            home: Some(home),
            allow_login_files: true,
        }
    }

    pub fn resolve(
        &self,
        provider: &str,
        explicit: Option<Credential>,
    ) -> Option<ResolvedCredential> {
        if let Some(credential) = explicit {
            return Some(ResolvedCredential {
                credential,
                source: CredentialSource::Explicit,
            });
        }
        if let Some(credential) = env_credential(provider) {
            return Some(ResolvedCredential {
                credential,
                source: CredentialSource::Environment,
            });
        }
        if !self.allow_login_files {
            return None;
        }
        let home = self.home.as_deref()?;
        match provider {
            "openai" | "openai-codex" => {
                read_codex_login(&home.join(".codex/auth.json")).map(|credential| {
                    ResolvedCredential {
                        credential,
                        source: CredentialSource::CodexLogin,
                    }
                })
            }
            "anthropic" => {
                read_claude_login(&home.join(".claude/.credentials.json")).map(|credential| {
                    ResolvedCredential {
                        credential,
                        source: CredentialSource::ClaudeCodeLogin,
                    }
                })
            }
            _ => None,
        }
    }
}

pub fn create_provider(
    model: &ModelSpec,
    credential: Option<Credential>,
    client: Option<reqwest::Client>,
) -> Result<Box<dyn Provider>, ProviderBuildError> {
    let auth = match (&model.api, credential) {
        (ProviderApi::Faux, None) => ProviderAuth::None,
        (_, Some(credential)) => credential.into(),
        _ => {
            return Err(ProviderBuildError::MissingCredential {
                provider: model.provider.clone(),
            })
        }
    };
    Ok(create_provider_with_client(
        ProviderConfig {
            model: model.model_ref(),
            api: model.api.clone(),
            base_url: model.base_url.clone(),
            auth,
            thinking_level: None,
            thinking_budget_tokens: None,
            session_id: None,
        },
        client.unwrap_or_default(),
    ))
}

#[derive(Debug, Error)]
pub enum ProviderBuildError {
    #[error("provider {provider} has no usable credential")]
    MissingCredential { provider: String },
}

fn env_credential(provider: &str) -> Option<Credential> {
    if provider == "anthropic" {
        if let Some(access_token) = first_env(&[
            "ANTHROPIC_OAUTH_TOKEN",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
        ]) {
            return Some(Credential::ClaudeCodeOAuth { access_token });
        }
    }
    if provider == "openai-codex" {
        if let Some(access_token) = first_env(&["CODEX_ACCESS_TOKEN"]) {
            return Some(Credential::ChatGptOAuth {
                access_token,
                account_id: first_env(&["CHATGPT_ACCOUNT_ID"]),
            });
        }
    }
    first_env(provider_env_names(provider)).map(Credential::ApiKey)
}

fn provider_env_names(provider: &str) -> &'static [&'static str] {
    match provider {
        "openai" => &["OPENAI_API_KEY", "CODEX_API_KEY"],
        "openai-codex" => &["CODEX_API_KEY"],
        "anthropic" => &["ANTHROPIC_API_KEY"],
        "zai" | "zai-coding" | "zai-anthropic" => &["ZAI_API_KEY"],
        "moonshotai" => &["MOONSHOT_API_KEY"],
        "kimi-coding" | "kimi-coding-openai" => &["KIMI_API_KEY"],
        "openrouter" => &["OPENROUTER_API_KEY"],
        "deepseek" => &["DEEPSEEK_API_KEY"],
        "groq" => &["GROQ_API_KEY"],
        "mistral" => &["MISTRAL_API_KEY"],
        _ => &[],
    }
}

fn first_env(names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| env::var(name).ok().filter(|value| !value.trim().is_empty()))
}

#[derive(Deserialize)]
struct CodexAuthFile {
    tokens: Option<CodexTokens>,
}

#[derive(Deserialize)]
struct CodexTokens {
    access_token: String,
    #[serde(default)]
    account_id: Option<String>,
}

fn read_codex_login(path: &Path) -> Option<Credential> {
    let auth: CodexAuthFile = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    let tokens = auth.tokens?;
    if tokens.access_token.trim().is_empty() {
        return None;
    }
    Some(Credential::ChatGptOAuth {
        access_token: tokens.access_token,
        account_id: tokens.account_id,
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeCredentialsFile {
    claude_ai_oauth: Option<ClaudeOAuth>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeOAuth {
    access_token: String,
}

fn read_claude_login(path: &Path) -> Option<Credential> {
    let auth: ClaudeCredentialsFile = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    let access_token = auth.claude_ai_oauth?.access_token;
    if access_token.trim().is_empty() {
        return None;
    }
    Some(Credential::ClaudeCodeOAuth { access_token })
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_switching_targets_and_distinct_scopes() {
        let models = builtin_models();
        assert!(find_model(&models, "openai-codex/gpt-5.4").is_some());
        assert!(find_model(&models, "zai-coding/glm-5.1").is_some());
        assert!(find_model(&models, "kimi-coding/kimi-for-coding").is_some());
        assert_eq!(
            find_model(&models, "zai-coding/glm-5.1")
                .unwrap()
                .usage_scope,
            UsageScope::CodingSubscription
        );
    }

    #[test]
    fn credential_debug_is_redacted() {
        let credential = Credential::ApiKey("super-secret".to_string());
        let output = format!("{credential:?}");
        assert!(!output.contains("super-secret"));
        assert!(output.contains("REDACTED"));
    }

    #[test]
    fn reads_login_files_without_pi_configuration() {
        let root = std::env::temp_dir().join(format!("pi-provider-test-{}", std::process::id()));
        let codex_dir = root.join(".codex");
        fs::create_dir_all(&codex_dir).unwrap();
        fs::write(
            codex_dir.join("auth.json"),
            r#"{"tokens":{"access_token":"token","account_id":"account"}}"#,
        )
        .unwrap();
        let resolved = CredentialResolver::with_home(root.clone())
            .resolve("openai-codex", None)
            .unwrap();
        assert_eq!(resolved.source, CredentialSource::CodexLogin);
        let _ = fs::remove_dir_all(root);
    }
}
