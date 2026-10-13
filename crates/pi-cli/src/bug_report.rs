//! `/bug [description]`: write a redacted bug report for this port's tracker.
//!
//! Upstream pi uploads reports to its own service or exports them as a zip.
//! This port's bugs are filed at <https://github.com/noeljackson/pi/issues>,
//! so `/bug` writes the same kind of metadata and error diagnostics to a local
//! Markdown file and prints a new-issue link. Nothing is uploaded, and the
//! transcript is never included: `/export` shares it deliberately.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use pi_config::{auth_for_provider, LoadedConfig, ResolvedAuth};
use pi_core::Runtime;
use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::{format_diagnostics, thinking_label, TuiApp, TuiEntryKind};

/// Where this port's bugs are filed.
const ISSUES_URL: &str = "https://github.com/noeljackson/pi/issues";
const REDACTED: &str = "<redacted>";
/// Errors shown in the session that a report keeps (newest), and their size.
const MAX_REPORTED_ERRORS: usize = 20;
const MAX_ERROR_CHARS: usize = 2_000;
const MAX_TITLE_CHARS: usize = 120;

/// Handles `/bug [description]` in the TUI; the report lands in the current
/// directory, like upstream's zip export.
pub(crate) fn run_tui_command(
    app: &mut TuiApp,
    runtime: &Runtime,
    config: &LoadedConfig,
    line: &str,
) {
    let dir = std::env::current_dir().unwrap_or_else(|_| runtime.session().cwd.clone());
    report_into(app, runtime, config, line, &dir);
}

fn report_into(app: &mut TuiApp, runtime: &Runtime, config: &LoadedConfig, line: &str, dir: &Path) {
    let description = line.strip_prefix("/bug").unwrap_or_default().trim();
    let errors = app
        .entries
        .iter()
        .filter(|entry| entry.kind == TuiEntryKind::Error)
        .map(|entry| entry.text.as_str())
        .collect::<Vec<_>>();
    let report = BugReport::collect(description, runtime, config, &errors, SystemTime::now());
    match report.write(dir) {
        Ok(path) => app.push(TuiEntryKind::System, report.status_message(&path)),
        Err(error) => app.push(
            TuiEntryKind::Error,
            format!("failed to write bug report: {error}"),
        ),
    }
}

struct BugReport {
    id: String,
    created_at: String,
    description: String,
    environment: Environment,
    session_id: String,
    message_count: usize,
    model: Option<ModelSummary>,
    thinking: Option<String>,
    extensions: Vec<(String, PathBuf)>,
    settings: Value,
    config_diagnostics: String,
    errors: Vec<String>,
}

struct ModelSummary {
    reference: String,
    api: Option<String>,
    base_url: Option<String>,
    context_window: Option<u64>,
    auth: &'static str,
}

impl BugReport {
    fn collect(
        description: &str,
        runtime: &Runtime,
        config: &LoadedConfig,
        errors: &[&str],
        now: SystemTime,
    ) -> Self {
        let session = runtime.session();
        let model = session.active_model.as_ref().map(|model| {
            let definition = config
                .models
                .iter()
                .find(|candidate| candidate.provider == model.provider && candidate.id == model.id);
            ModelSummary {
                reference: format!("{}/{}", model.provider, model.id),
                // Serialized names match upstream's api ids (e.g. anthropic-messages).
                api: definition.map(|definition| {
                    serde_json::to_value(&definition.api)
                        .ok()
                        .and_then(|api| api.as_str().map(str::to_string))
                        .unwrap_or_else(|| format!("{:?}", definition.api))
                }),
                base_url: definition
                    .and_then(|definition| definition.base_url.as_deref())
                    .map(redact_url),
                context_window: definition.and_then(|definition| definition.context_window),
                auth: match auth_for_provider(
                    &config.auth,
                    &model.provider,
                    session.active_account.as_deref(),
                ) {
                    Some(ResolvedAuth::ApiKey(_)) => "api key",
                    Some(_) => "oauth",
                    None => "none",
                },
            }
        });
        let skipped = errors.len().saturating_sub(MAX_REPORTED_ERRORS);
        Self {
            id: report_id(now),
            created_at: OffsetDateTime::from(now)
                .replace_nanosecond(0)
                .ok()
                .and_then(|time| time.format(&Rfc3339).ok())
                .unwrap_or_else(|| "unknown".to_string()),
            description: description.to_string(),
            environment: Environment::collect(),
            session_id: session.session_id.clone(),
            message_count: session.messages.len(),
            model,
            thinking: thinking_label(runtime, config),
            extensions: config
                .extensions
                .iter()
                .map(|extension| (extension.name.clone(), extension.path.clone()))
                .collect(),
            settings: redacted_settings(config),
            config_diagnostics: format_diagnostics(config),
            errors: errors[skipped..]
                .iter()
                .map(|error| truncate_chars(&redact_text(error), MAX_ERROR_CHARS))
                .collect(),
        }
    }

    fn write(&self, dir: &Path) -> Result<PathBuf> {
        let path = dir.join(format!("pi-bug-report-{}.md", self.id));
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?
            .write_all(self.render().as_bytes())?;
        Ok(path)
    }

    fn render(&self) -> String {
        let environment = &self.environment;
        let yes_no = |value: bool| if value { "yes" } else { "no" };
        let or_dash = |value: Option<&str>| value.unwrap_or("-").to_string();
        let term_program = match (
            environment.term_program.as_deref(),
            environment.term_program_version.as_deref(),
        ) {
            (Some(program), Some(version)) => format!("{program} {version}"),
            (program, _) => or_dash(program),
        };
        let mut lines = vec![
            "# pi bug report".to_string(),
            String::new(),
            format!("- Report ID: {}", self.id),
            format!("- Created: {}", self.created_at),
            String::new(),
            "## Description".to_string(),
            String::new(),
            if self.description.is_empty() {
                "(none)".to_string()
            } else {
                self.description.clone()
            },
            String::new(),
            "## Environment".to_string(),
            String::new(),
            format!("- pi: {} (Rust port)", environment.version),
            format!(
                "- OS: {} {}{}",
                environment.os,
                environment.arch,
                environment
                    .os_release
                    .as_deref()
                    .map(|release| format!(", release {release}"))
                    .unwrap_or_default()
            ),
            format!("- Shell: {}", or_dash(environment.shell.as_deref())),
            format!(
                "- Terminal: TERM={}, TERM_PROGRAM={term_program}, COLORTERM={}",
                or_dash(environment.term.as_deref()),
                or_dash(environment.colorterm.as_deref())
            ),
            format!(
                "- tmux: {}, SSH: {}, CI: {}",
                yes_no(environment.tmux),
                yes_no(environment.ssh),
                yes_no(environment.ci)
            ),
            format!(
                "- PI_* variables (names only): {}",
                if environment.pi_variables.is_empty() {
                    "none".to_string()
                } else {
                    environment.pi_variables.join(", ")
                }
            ),
            String::new(),
            "## Session".to_string(),
            String::new(),
            format!("- ID: {}", self.session_id),
            format!("- Messages: {}", self.message_count),
            "- Transcript: not included; run `/export <file>.html` and attach it after reviewing it"
                .to_string(),
            String::new(),
            "## Model".to_string(),
            String::new(),
        ];
        match &self.model {
            Some(model) => {
                let mut details = Vec::new();
                if let Some(api) = &model.api {
                    details.push(format!("api: {api}"));
                }
                if let Some(window) = model.context_window {
                    details.push(format!("context window: {window}"));
                }
                lines.push(if details.is_empty() {
                    format!("- Model: {}", model.reference)
                } else {
                    format!("- Model: {} ({})", model.reference, details.join(", "))
                });
                lines.push(format!(
                    "- Base URL: {}",
                    model.base_url.as_deref().unwrap_or("provider default")
                ));
                lines.push(format!(
                    "- Auth: {} (credential values are never included)",
                    model.auth
                ));
            }
            None => lines.push("- Model: none selected".to_string()),
        }
        lines.push(format!(
            "- Thinking: {}",
            self.thinking.as_deref().unwrap_or("off")
        ));
        lines.extend([String::new(), "## Extensions".to_string(), String::new()]);
        if self.extensions.is_empty() {
            lines.push("- none".to_string());
        }
        for (name, path) in &self.extensions {
            lines.push(format!("- {name} ({})", path.display()));
        }
        lines.extend([
            String::new(),
            "## Settings".to_string(),
            String::new(),
            "Effective settings; credential-like values and prompt text are redacted.".to_string(),
            String::new(),
            fenced(
                "json",
                &serde_json::to_string_pretty(&self.settings).unwrap_or_default(),
            ),
            String::new(),
            "## Diagnostics".to_string(),
            String::new(),
            "Configuration:".to_string(),
            String::new(),
            fenced("text", &self.config_diagnostics),
            String::new(),
            "Errors shown in this session (newest last; URL secrets and credentials redacted):"
                .to_string(),
            String::new(),
        ]);
        if self.errors.is_empty() {
            lines.push("(none)".to_string());
        } else {
            lines.push(fenced("text", &self.errors.join("\n---\n")));
        }
        lines.push(String::new());
        lines.join("\n")
    }

    fn issue_url(&self) -> String {
        let base = format!("{ISSUES_URL}/new");
        let title = truncate_chars(
            self.description.lines().next().unwrap_or_default().trim(),
            MAX_TITLE_CHARS,
        );
        if title.is_empty() {
            return base;
        }
        reqwest::Url::parse_with_params(&base, [("title", title.as_str())])
            .map(|url| url.to_string())
            .unwrap_or(base)
    }

    fn status_message(&self, path: &Path) -> String {
        format!(
            "bug report written to {}\n\
             it lists pi, OS, and terminal versions, the model and provider configuration (no credentials), \
             extensions, redacted settings, and errors shown in this session; it has no transcript\n\
             review it, then attach it to a new issue: {}\n\
             to share the transcript too, run /export <file> and attach that file after reviewing it",
            path.display(),
            self.issue_url()
        )
    }
}

struct Environment {
    version: &'static str,
    os: &'static str,
    arch: &'static str,
    os_release: Option<String>,
    shell: Option<String>,
    term: Option<String>,
    term_program: Option<String>,
    term_program_version: Option<String>,
    colorterm: Option<String>,
    tmux: bool,
    ssh: bool,
    ci: bool,
    /// Names help diagnose configuration; values never leave the machine.
    pi_variables: Vec<String>,
}

impl Environment {
    fn collect() -> Self {
        let var = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
        let mut pi_variables = std::env::vars_os()
            .filter_map(|(name, _)| name.into_string().ok())
            .filter(|name| name.starts_with("PI_"))
            .collect::<Vec<_>>();
        pi_variables.sort();
        Self {
            version: env!("CARGO_PKG_VERSION"),
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            os_release: Some(pi_ai::os_release()).filter(|release| release != "unknown"),
            shell: var("SHELL").map(|shell| {
                shell
                    .rsplit(['/', '\\'])
                    .next()
                    .unwrap_or(shell.as_str())
                    .to_string()
            }),
            term: var("TERM"),
            term_program: var("TERM_PROGRAM"),
            term_program_version: var("TERM_PROGRAM_VERSION"),
            colorterm: var("COLORTERM"),
            tmux: var("TMUX").is_some(),
            ssh: ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
                .into_iter()
                .any(|name| var(name).is_some()),
            ci: var("CI").is_some(),
            pi_variables,
        }
    }
}

/// A UUIDv7 like upstream's report ids: millisecond time, then randomness.
fn report_id(now: SystemTime) -> String {
    let since_epoch = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let millis = u64::try_from(since_epoch.as_millis()).unwrap_or(u64::MAX);
    let mut bytes = [0u8; 16];
    if getrandom::getrandom(&mut bytes[6..]).is_err() {
        bytes[6..10].copy_from_slice(&since_epoch.subsec_nanos().to_be_bytes());
        bytes[10..14].copy_from_slice(&std::process::id().to_be_bytes());
    }
    bytes[..6].copy_from_slice(&millis.to_be_bytes()[2..]);
    bytes[6] = 0x70 | (bytes[6] & 0x0f);
    bytes[8] = 0x80 | (bytes[8] & 0x3f);
    let hex = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

fn redacted_settings(config: &LoadedConfig) -> Value {
    let mut settings = serde_json::to_value(&config.settings).unwrap_or(Value::Null);
    if let Some(object) = settings.as_object_mut() {
        // Prompt text is conversation content, not configuration.
        for key in ["systemPrompt", "appendSystemPrompt"] {
            if let Some(value) = object.get_mut(key) {
                if !value.is_null() && value.as_array().is_none_or(|items| !items.is_empty()) {
                    *value = Value::String(REDACTED.to_string());
                }
            }
        }
    }
    redact_json(settings)
}

/// Upstream's redaction: drop credential-like keys' values and URL secrets.
/// Null fields are dropped so the report only shows configured settings.
fn redact_json(value: Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .into_iter()
                .filter(|(_, child)| !child.is_null())
                .map(|(key, child)| {
                    let child = if is_sensitive_key(&key) {
                        Value::String(REDACTED.to_string())
                    } else {
                        redact_json(child)
                    };
                    (key, child)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(redact_json).collect()),
        Value::String(text) => Value::String(redact_url(&text)),
        other => other,
    }
}

/// Upstream's rule: a key whose words (split on `-`, `_`, and camelCase)
/// include api key, secret, token, password, passwd, credential,
/// authorization, or cookie.
fn is_sensitive_key(key: &str) -> bool {
    let mut words = vec![String::new()];
    let mut previous = None::<char>;
    for ch in key.chars() {
        if ch == '-' || ch == '_' {
            words.push(String::new());
        } else {
            if ch.is_ascii_uppercase()
                && previous.is_some_and(|previous| {
                    previous.is_ascii_lowercase() || previous.is_ascii_digit()
                })
            {
                words.push(String::new());
            }
            if let Some(word) = words.last_mut() {
                word.push(ch.to_ascii_lowercase());
            }
        }
        previous = Some(ch);
    }
    words.iter().enumerate().any(|(index, word)| {
        matches!(
            word.as_str(),
            "apikey"
                | "secret"
                | "token"
                | "password"
                | "passwd"
                | "credential"
                | "authorization"
                | "cookie"
        ) || (word == "api" && words.get(index + 1).is_some_and(|next| next == "key"))
    })
}

/// Query parameters whose values a report hides: upstream's settings key rule
/// plus `auth`, `sig`, `signature`, and any name containing key, token,
/// secret, or password (Google-style `key=`, `X-Amz-Security-Token`, ...).
fn is_sensitive_param(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    is_sensitive_key(name)
        || ["key", "token", "secret", "password"]
            .iter()
            .any(|part| lower.contains(part))
        || lower
            .split(|ch: char| !ch.is_ascii_alphanumeric())
            .any(|word| matches!(word, "auth" | "sig" | "signature"))
}

/// Strip URL user info and credential-like query values; other text passes
/// through unchanged. `git:https://…`-style prefixes are preserved.
fn redact_url(value: &str) -> String {
    if let Some((prefix, rest)) = value.split_once(':') {
        if is_scheme(prefix)
            && rest
                .split_once("://")
                .is_some_and(|(scheme, _)| is_scheme(scheme))
        {
            return format!("{prefix}:{}", redact_url(rest));
        }
    }
    let Ok(mut url) = reqwest::Url::parse(value) else {
        return value.to_string();
    };
    let mut changed = false;
    if !url.username().is_empty() || url.password().is_some() {
        let _ = url.set_username("");
        let _ = url.set_password(None);
        changed = true;
    }
    let pairs = url
        .query_pairs()
        .map(|(key, value)| {
            // A nested URL (an unencoded redirect target) gets the same treatment.
            let redacted = if is_sensitive_param(&key) {
                REDACTED.to_string()
            } else {
                redact_url(&value)
            };
            (redacted != value, key.into_owned(), redacted)
        })
        .collect::<Vec<_>>();
    if pairs.iter().any(|(redacted, ..)| *redacted) {
        url.query_pairs_mut()
            .clear()
            .extend_pairs(pairs.iter().map(|(_, key, value)| (key, value)));
        changed = true;
    }
    if changed {
        url.to_string()
    } else {
        value.to_string()
    }
}

/// Error text with credentials replaced: secrets in any URL it mentions
/// (see [`redact_url`]), `Bearer`/`Basic` credentials, and well-known API key
/// and token formats. Other text passes through unchanged.
fn redact_text(text: &str) -> String {
    redact_credentials(&redact_urls(text))
}

/// Applies [`redact_url`] to each `scheme://…` URL inside `text`.
fn redact_urls(text: &str) -> String {
    let mut redacted = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(separator) = rest.find("://") {
        let scheme_start = rest[..separator]
            .char_indices()
            .rev()
            .take_while(|(_, ch)| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.'))
            .last()
            .map_or(separator, |(index, _)| index);
        let end = rest[separator..]
            .find(|ch: char| ch.is_whitespace() || matches!(ch, '"' | '\'' | '<' | '>' | '`'))
            .map_or(rest.len(), |offset| separator + offset);
        // Sentence punctuation and closing brackets after a URL are not part of it.
        let url =
            rest[scheme_start..end].trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}']);
        if is_scheme(&rest[scheme_start..separator]) && url.len() > separator - scheme_start + 3 {
            redacted.push_str(&rest[..scheme_start]);
            redacted.push_str(&redact_url(url));
            rest = &rest[scheme_start + url.len()..];
        } else {
            redacted.push_str(&rest[..separator + 3]);
            rest = &rest[separator + 3..];
        }
    }
    redacted.push_str(rest);
    redacted
}

/// Well-known credential prefixes and the fewest characters that must follow.
const CREDENTIAL_PREFIXES: &[(&str, usize)] = &[
    // OpenAI and Anthropic (`sk-ant-…`) API keys.
    ("sk-", 16),
    // Google API keys.
    ("AIza", 30),
    // GitHub tokens.
    ("github_pat_", 20),
    ("ghp_", 20),
    ("gho_", 20),
    ("ghs_", 20),
    ("ghu_", 20),
    ("ghr_", 20),
    // Slack tokens.
    ("xoxa-", 10),
    ("xoxb-", 10),
    ("xoxe-", 10),
    ("xoxp-", 10),
    ("xoxr-", 10),
    ("xoxs-", 10),
    // AWS access key ids (long-term and temporary).
    ("AKIA", 16),
    ("ASIA", 16),
];

/// Replaces `Bearer`/`Basic` credentials and words that start with a
/// [`CREDENTIAL_PREFIXES`] entry.
fn redact_credentials(text: &str) -> String {
    let mut redacted = String::with_capacity(text.len());
    let mut index = 0;
    while let Some(ch) = text[index..].chars().next() {
        if !text[..index].ends_with(is_word_char) {
            if let Some((kept, secret)) = credential_at(&text[index..]) {
                redacted.push_str(&text[index..index + kept]);
                redacted.push_str(REDACTED);
                index += kept + secret;
                continue;
            }
        }
        redacted.push(ch);
        index += ch.len_utf8();
    }
    redacted
}

/// A credential at the start of `text`, as (bytes kept, secret bytes): the
/// value after a `Bearer` or `Basic` scheme, or a prefixed key or token.
fn credential_at(text: &str) -> Option<(usize, usize)> {
    for scheme in ["bearer", "basic"] {
        let Some(after) = text
            .get(..scheme.len())
            .filter(|word| word.eq_ignore_ascii_case(scheme))
            .map(|_| &text[scheme.len()..])
        else {
            continue;
        };
        let value = after.trim_start_matches([' ', '\t']);
        let gap = after.len() - value.len();
        let length = value
            .find(|ch: char| !(ch.is_ascii_alphanumeric() || "-._~+/=".contains(ch)))
            .unwrap_or(value.len());
        if gap > 0 && looks_like_credential(&value[..length]) {
            return Some((scheme.len() + gap, length));
        }
    }
    CREDENTIAL_PREFIXES.iter().find_map(|(prefix, min_length)| {
        let body = text.strip_prefix(prefix)?;
        let length = body.find(|ch| !is_word_char(ch)).unwrap_or(body.len());
        (length >= *min_length).then_some((0, prefix.len() + length))
    })
}

/// Whether the word after `Bearer`/`Basic` is a credential rather than prose
/// such as "bearer token" or "Basic authentication": credentials carry digits
/// or symbols, or are long.
fn looks_like_credential(value: &str) -> bool {
    value.len() >= 8 && (value.len() >= 24 || value.chars().any(|ch| !ch.is_ascii_alphabetic()))
}

fn is_word_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'
}

fn is_scheme(value: &str) -> bool {
    let mut chars = value.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '.' | '-'))
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((index, _)) => format!("{}…", &text[..index]),
        None => text.to_string(),
    }
}

/// A code block whose fence outlasts any backtick run in `content`.
fn fenced(language: &str, content: &str) -> String {
    let longest_run = content
        .split(|ch| ch != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    let fence = "`".repeat(longest_run.max(2) + 1);
    format!("{fence}{language}\n{content}\n{fence}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::minimal_test_config;
    use pi_ai::ModelRef;
    use pi_config::{ModelDefinition, ProviderApi, ResourceFile};
    use pi_core::{ReloadableSystems, SessionState};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pi-cli-bug-report-{name}-{}-{}",
            std::process::id(),
            crate::unique_temp_suffix()
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn sensitive_keys_follow_upstream_word_rule() {
        for key in [
            "apiKey",
            "api_key",
            "x-api-key",
            "APIKey",
            "accessToken",
            "refresh_token",
            "clientSecret",
            "password",
            "Authorization",
            "session-cookie",
            "credential",
        ] {
            assert!(is_sensitive_key(key), "{key} should be sensitive");
        }
        for key in [
            "maxTokens",
            "tokens",
            "credentials",
            "keybindings",
            "theme",
            "myapi_key",
            "secretary",
        ] {
            assert!(!is_sensitive_key(key), "{key} should not be sensitive");
        }
    }

    #[test]
    fn urls_lose_credentials_and_secret_query_values() {
        assert_eq!(
            redact_url("https://user:hunter2@example.com/v1?api_key=abc&region=eu"),
            "https://example.com/v1?api_key=%3Credacted%3E&region=eu"
        );
        assert_eq!(
            redact_url("git:https://token@github.com/org/repo"),
            "git:https://github.com/org/repo"
        );
        assert_eq!(
            redact_url("https://generativelanguage.test/v1?key=AIza123"),
            "https://generativelanguage.test/v1?key=%3Credacted%3E"
        );
        assert_eq!(
            redact_url(
                "https://s3.test/o?X-Amz-Signature=f00&sig=1&auth=2&client_secret=3&apikey=4&passwordHash=5&alt=sse"
            ),
            "https://s3.test/o?X-Amz-Signature=%3Credacted%3E&sig=%3Credacted%3E&auth=%3Credacted%3E\
             &client_secret=%3Credacted%3E&apikey=%3Credacted%3E&passwordHash=%3Credacted%3E&alt=sse"
        );
        // An unencoded redirect target is a URL of its own.
        assert_eq!(
            redact_url("https://a.test/cb?redirect=https://b.test/?token=t1"),
            "https://a.test/cb?redirect=https%3A%2F%2Fb.test%2F%3Ftoken%3D%253Credacted%253E"
        );
        for unchanged in [
            "https://example.com/v1?region=eu&authuser=0",
            "/home/user/project",
            "not a url",
            "dark",
        ] {
            assert_eq!(redact_url(unchanged), unchanged);
        }
    }

    #[test]
    fn error_text_redaction_covers_urls_auth_schemes_and_key_formats() {
        let secret = |prefix: &str| format!("{prefix}AbCdEf0123456789xYz_AbCdEf0123456789");
        for (input, expected) in [
            (
                "request to https://api.test/v1/models?key=abc&region=eu failed: 403",
                "request to https://api.test/v1/models?key=%3Credacted%3E&region=eu failed: 403",
            ),
            (
                "error sending request for url (https://u:pw@api.test/v1?access_token=t1&page=2).",
                "error sending request for url (https://api.test/v1?access_token=%3Credacted%3E&page=2).",
            ),
            (
                "Authorization: Bearer abc.def-123_456 rejected",
                "Authorization: Bearer <redacted> rejected",
            ),
            (
                "header \"authorization\": \"bearer eyJhbGciOiJIUzI1NiJ9.e30.sig\"",
                "header \"authorization\": \"bearer <redacted>\"",
            ),
            (
                "proxy wants Basic dXNlcjpwYXNzd29yZA==",
                "proxy wants Basic <redacted>",
            ),
        ] {
            assert_eq!(redact_text(input), expected);
        }
        for prefix in [
            "sk-",
            "sk-ant-api03-",
            "sk-proj-",
            "AIza",
            "ghp_",
            "gho_",
            "ghs_",
            "ghu_",
            "github_pat_",
            "xoxb-",
            "xoxp-",
        ] {
            assert_eq!(
                redact_text(&format!("invalid x-api-key: {} (401)", secret(prefix))),
                "invalid x-api-key: <redacted> (401)",
                "{prefix}"
            );
        }
        assert_eq!(
            redact_text("aws key AKIAIOSFODNN7EXAMPLE was denied"),
            "aws key <redacted> was denied"
        );

        for ordinary in [
            "provider returned 529: overloaded",
            "invalid bearer token; Basic authentication is not supported",
            "max_tokens=4096 exceeds the limit for task-1234567890abcdefghij",
            "sk-learn and risk-assessment are not keys",
            "see https://docs.test/errors#rate-limits?retry=later or https://example.com/v1?region=eu",
            "unicode é :// stays put",
        ] {
            assert_eq!(redact_text(ordinary), ordinary);
        }
    }

    #[test]
    fn settings_redaction_drops_nulls_secrets_and_prompts() {
        let mut config = minimal_test_config();
        config.settings.theme = Some("dark".to_string());
        config.settings.system_prompt = Some("private instructions".to_string());
        config.settings.append_system_prompt = vec!["more private text".to_string()];
        let settings = redacted_settings(&config);
        assert_eq!(settings["theme"], "dark");
        assert_eq!(settings["systemPrompt"], REDACTED);
        assert_eq!(settings["appendSystemPrompt"], REDACTED);
        assert!(settings.get("defaultModel").is_none());

        let nested = redact_json(serde_json::json!({
            "providers": { "acme": { "apiKey": "sk-123", "baseUrl": "https://u:p@acme.test" } },
            "list": [{ "token": "t" }, "https://ok.test"],
            "empty": null,
        }));
        assert_eq!(
            nested,
            serde_json::json!({
                "providers": { "acme": { "apiKey": REDACTED, "baseUrl": "https://acme.test/" } },
                "list": [{ "token": REDACTED }, "https://ok.test"],
            })
        );
    }

    #[test]
    fn report_ids_are_uuid_v7() {
        let id = report_id(UNIX_EPOCH + std::time::Duration::from_millis(0x0192_3456_789a));
        assert_eq!(id.len(), 36);
        assert!(id.starts_with("01923456-789a-7"), "{id}");
        assert!(
            matches!(id.as_bytes()[19], b'8' | b'9' | b'a' | b'b'),
            "{id}"
        );
        assert_ne!(report_id(SystemTime::now()), report_id(SystemTime::now()));
    }

    #[test]
    fn fences_outlast_backticks_in_content() {
        assert_eq!(fenced("text", "plain"), "```text\nplain\n```");
        assert_eq!(
            fenced("text", "has ``` fence"),
            "````text\nhas ``` fence\n````"
        );
    }

    #[test]
    fn bug_command_writes_redacted_report_and_links_this_ports_tracker() {
        let dir = temp_dir("report");
        let mut config = minimal_test_config();
        config.models = vec![ModelDefinition {
            provider: "faux".to_string(),
            id: "echo".to_string(),
            name: None,
            api: ProviderApi::default(),
            base_url: Some("https://user:hunter2@faux.test/v1?token=abc".to_string()),
            context_window: Some(4096),
            ..ModelDefinition::default()
        }];
        config.extensions = vec![ResourceFile {
            name: "assist".to_string(),
            path: PathBuf::from("/agent/extensions/assist.md"),
            content: "never in reports".to_string(),
        }];
        config.settings.theme = Some("dark".to_string());
        let mut session = SessionState::new("session-1", dir.clone());
        session.active_model = Some(ModelRef {
            provider: "faux".to_string(),
            id: "echo".to_string(),
        });
        let runtime = Runtime::new(session, ReloadableSystems::default());
        let mut app = TuiApp::default();
        app.push(TuiEntryKind::User, "private prompt text");
        app.push(TuiEntryKind::Error, "provider returned 529: overloaded");
        app.push(
            TuiEntryKind::Error,
            "GET https://faux.test/v1?token=url-secret-1 failed; sent Bearer sk-ant-oat01-headersecret2",
        );

        report_into(
            &mut app,
            &runtime,
            &config,
            "/bug model switch lost my draft",
            &dir,
        );

        let status = app.entries.last().expect("status entry");
        assert_eq!(status.kind, TuiEntryKind::System, "{}", status.text);
        assert!(status.text.contains(
            "https://github.com/noeljackson/pi/issues/new?title=model+switch+lost+my+draft"
        ));
        let reports = fs::read_dir(&dir)
            .expect("read report dir")
            .map(|entry| entry.expect("dir entry").path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("pi-bug-report-") && name.ends_with(".md"))
            })
            .collect::<Vec<_>>();
        assert_eq!(reports.len(), 1, "{reports:?}");
        assert!(status.text.contains(&reports[0].display().to_string()));
        let report = fs::read_to_string(&reports[0]).expect("read report");
        for expected in [
            "# pi bug report",
            "model switch lost my draft",
            "- ID: session-1",
            "- Model: faux/echo (api: openai-completions, context window: 4096)",
            "- Base URL: https://faux.test/v1?token=%3Credacted%3E",
            "- Auth: none",
            "- assist (/agent/extensions/assist.md)",
            "\"theme\": \"dark\"",
            "provider returned 529: overloaded",
            "GET https://faux.test/v1?token=%3Credacted%3E failed; sent Bearer <redacted>",
            "Transcript: not included",
        ] {
            assert!(
                report.contains(expected),
                "missing {expected:?} in\n{report}"
            );
        }
        for leaked in [
            "hunter2",
            "private prompt text",
            "never in reports",
            "url-secret-1",
            "headersecret2",
        ] {
            assert!(!report.contains(leaked), "report leaked {leaked:?}");
        }

        report_into(&mut app, &runtime, &config, "/bug", &dir);
        let status = app.entries.last().expect("status entry");
        assert!(status
            .text
            .contains("attach it to a new issue: https://github.com/noeljackson/pi/issues/new\n"));

        let _ = fs::remove_dir_all(dir);
    }
}
