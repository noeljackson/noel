//! `pi auth`: print a provider credential for an external client, or check
//! whether a provider is ready. Mirrors upstream's credential commands on top
//! of pi's own auth resolution (named accounts, environment, CLI imports).

use std::io::Write as _;
use std::time::Duration;

use anyhow::Result;
use pi_config::{
    auth_for_provider, expires_within, is_expired, listed_accounts_for_provider,
    load_config_with_project_trust, project_is_trusted, AuthData, ConfigPaths, LoadedConfig,
    ResolvedAuth,
};
use serde::Serialize;

use crate::{oauth_refresh, unix_seconds};

const USAGE: &str = "Usage:
  pi auth print-api-key [--provider <provider>] [--model <model>] [--account <name>]
  pi auth print-bearer-token [--provider <provider>] [--model <model>] [--account <name>] [--min-expiry <duration>]
  pi auth check [--provider <provider>] [--model <model>] [--account <name>] [--json] [--credentials] [--no-refresh]

Auth commands require at least one of --provider or --model. Checks refresh expired OAuth credentials by default; --no-refresh prevents this. --credentials emits the credential, or includes it in JSON output. print-bearer-token refreshes tokens that expire within --min-expiry (ms, s, m, or h; default 30m). Logins read from Claude Code or Codex files refresh only once they expire, which keeps that CLI signed in; one that expires sooner prints with a warning. --account selects a named account (see pi accounts status). Credential-printing commands write secrets to stdout.";

/// pi's own OAuth tokens closer than this to expiry are refreshed before use
/// (imported logins wait until they expire); a check reports a ready token
/// this close to expiry as expiring soon.
const MIN_OAUTH_VALIDITY_SECONDS: u64 = 5 * 60;
/// Remaining lifetime `print-bearer-token` requires without `--min-expiry`.
const DEFAULT_BEARER_MIN_EXPIRY_SECONDS: u64 = 30 * 60;
const REFRESH_TIMEOUT: Duration = Duration::from_secs(15);
const THINKING_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Check,
    ApiKey,
    BearerToken,
}

impl Kind {
    fn command_name(self) -> &'static str {
        match self {
            Kind::Check => "auth check",
            Kind::ApiKey => "auth print-api-key",
            Kind::BearerToken => "auth print-bearer-token",
        }
    }

    /// Exit status for an error once the arguments parse: checks reserve 1
    /// for "not ready", so theirs use 2. Like upstream (whose tests pin
    /// `auth check --credentails` to 1), an unknown command or option exits 1
    /// for every kind; see `run`.
    fn error_status(self) -> i32 {
        if self == Kind::Check {
            2
        } else {
            1
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AuthCommand {
    kind: Kind,
    provider: Option<String>,
    model: Option<String>,
    account: Option<String>,
    json: bool,
    credentials: bool,
    no_refresh: bool,
    min_expiry_seconds: Option<u64>,
    positional: bool,
}

/// Runs `pi auth <args>`, prints the outcome, and returns the exit status.
pub(crate) async fn run(args: &[String]) -> i32 {
    if args.is_empty() || args[0] == "help" || args.iter().any(|arg| arg == "--help" || arg == "-h")
    {
        println!("{USAGE}");
        return 0;
    }
    let command = match parse(args) {
        Ok(command) => command,
        Err(message) => {
            // Upstream exits 1 here for every kind, `auth check` included.
            eprintln!("Error: {message}");
            eprintln!("Use \"pi auth --help\" for usage.");
            return 1;
        }
    };
    let client = reqwest::Client::builder()
        .timeout(REFRESH_TIMEOUT)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let refresher = Refresher {
        client: &client,
        endpoint: None,
        now: unix_seconds().unwrap_or(0),
    };
    let status = match execute(&command, &refresher).await {
        Ok(outcome) => {
            if let Some(warning) = &outcome.warning {
                eprintln!("Warning: {warning}");
            }
            println!("{}", outcome.stdout);
            outcome.status
        }
        Err(message) => {
            eprintln!("Error: {message}");
            command.kind.error_status()
        }
    };
    let _ = std::io::stdout().flush();
    status
}

/// What a `pi auth` command prints and its exit status.
struct Outcome {
    stdout: String,
    /// For stderr; JSON checks carry it as `expiresSoon` instead.
    warning: Option<String>,
    status: i32,
}

async fn execute(command: &AuthCommand, refresher: &Refresher<'_>) -> Result<Outcome, String> {
    validate(command)?;
    let config = load_auth_config().map_err(|error| format!("{error:#}"))?;
    match command.kind {
        Kind::Check => {
            let result = check(&config, command, refresher).await?;
            Ok(Outcome {
                stdout: result.render(command.json),
                warning: result.warning.filter(|_| !command.json),
                status: result.status.exit_status(),
            })
        }
        Kind::ApiKey | Kind::BearerToken => {
            let printed = print_credential(&config, command, refresher).await?;
            Ok(Outcome {
                stdout: printed.value,
                warning: printed.warning,
                status: 0,
            })
        }
    }
}

fn load_auth_config() -> Result<LoadedConfig> {
    let cwd = std::env::current_dir()?;
    let paths = ConfigPaths::discover(cwd.clone(), None)?;
    let trusted = project_is_trusted(&paths.agent_dir, &cwd)?;
    Ok(load_config_with_project_trust(paths, trusted)?)
}

fn parse(args: &[String]) -> Result<AuthCommand, String> {
    let kind = match args.first().map(String::as_str) {
        Some("check") => Kind::Check,
        Some("print-api-key") => Kind::ApiKey,
        Some("print-bearer-token") => Kind::BearerToken,
        other => {
            return Err(format!(
                "Unknown auth command \"{}\". Use \"pi auth print-api-key\", \"pi auth print-bearer-token\", or \"pi auth check\".",
                other.unwrap_or_default()
            ))
        }
    };
    let mut command = AuthCommand {
        kind,
        provider: None,
        model: None,
        account: None,
        json: false,
        credentials: false,
        no_refresh: false,
        min_expiry_seconds: None,
        positional: false,
    };
    let mut rest = args[1..].iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--provider" | "--model" | "--account" => {
                let value = rest
                    .next()
                    .map(|value| value.trim())
                    .filter(|value| !value.is_empty() && !value.starts_with('-'))
                    .ok_or_else(|| format!("{arg} requires a value"))?
                    .to_string();
                match arg.as_str() {
                    "--provider" => command.provider = Some(value),
                    "--model" => command.model = Some(value),
                    _ => command.account = Some(value),
                }
            }
            "--min-expiry" => {
                if kind != Kind::BearerToken {
                    return Err("--min-expiry is only supported by print-bearer-token".to_string());
                }
                command.min_expiry_seconds = Some(
                    rest.next()
                        .and_then(|value| parse_duration_seconds(value))
                        .ok_or("--min-expiry must use a duration such as 30m or 1h")?,
                );
            }
            "--json" | "--credentials" | "--no-refresh" => {
                if kind != Kind::Check {
                    return Err(format!("{arg} is only supported by auth check"));
                }
                match arg.as_str() {
                    "--json" => command.json = true,
                    "--credentials" => command.credentials = true,
                    _ => command.no_refresh = true,
                }
            }
            option if option.starts_with('-') => {
                return Err(format!(
                    "Unknown option {option} for \"{}\".",
                    kind.command_name()
                ))
            }
            _ => command.positional = true,
        }
    }
    Ok(command)
}

fn validate(command: &AuthCommand) -> Result<(), String> {
    if command.positional {
        return Err("Auth commands only accept --provider, --model, and --account".to_string());
    }
    if command.provider.is_none() && command.model.is_none() {
        return Err(if command.kind == Kind::Check {
            "Auth checks require --provider <provider> or --model <model>".to_string()
        } else {
            "Credential printing requires --provider <provider> or --model <model>".to_string()
        });
    }
    Ok(())
}

/// `30m`, `1h`, `45s`, or `500ms` (any case) in whole seconds, rounded up.
fn parse_duration_seconds(value: &str) -> Option<u64> {
    let digits = value.bytes().take_while(u8::is_ascii_digit).count();
    let amount = value[..digits].parse::<u64>().ok()?;
    match value[digits..].to_ascii_lowercase().as_str() {
        "ms" => Some(amount.div_ceil(1000)),
        "s" => Some(amount),
        "m" => amount.checked_mul(60),
        "h" => amount.checked_mul(3600),
        _ => None,
    }
}

/// OAuth refresh inputs; tests point `endpoint` at a stub token server.
struct Refresher<'a> {
    client: &'a reqwest::Client,
    endpoint: Option<(&'a str, &'a str)>,
    now: u64,
}

impl Refresher<'_> {
    /// The credential, first refreshed when it expires within
    /// `window_seconds`. An imported login (see [`imported_login`]) refreshes
    /// only on the request path's schedule, once it expires.
    async fn auth(
        &self,
        config: &LoadedConfig,
        provider: &str,
        account: Option<&str>,
        window_seconds: u64,
    ) -> Result<Option<ResolvedAuth>> {
        let window_seconds = if imported_login(&config.auth, provider, account).is_some() {
            oauth_refresh::REFRESH_WINDOW_SECONDS
        } else {
            window_seconds
        };
        oauth_refresh::refresh_auth_expiring_within(
            self.client,
            &config.paths.auth_path,
            &config.auth,
            provider,
            account,
            self.endpoint,
            self.now,
            window_seconds,
        )
        .await
    }
}

/// The CLI whose own login file supplies this credential (Claude Code for
/// anthropic, Codex for openai and openai-codex): pi reads it when no stored
/// account answers the request. Refreshing it spends that CLI's single-use
/// refresh token and signs the CLI out. Stored accounts, including the
/// `claude-import`/`codex-import` copy pi saves once it has refreshed such a
/// login, hold pi's own refresh token. (An environment token can resolve the
/// same way, but it never expires or refreshes.)
fn imported_login(auth: &AuthData, provider: &str, account: Option<&str>) -> Option<&'static str> {
    let cli = match provider {
        "anthropic" => "Claude Code",
        "openai" | "openai-codex" => "Codex",
        _ => return None,
    };
    let stored = match account {
        Some(account) => auth.credential(provider, account).is_some(),
        None => auth.account_for_resolution(provider).is_some(),
    };
    (!stored).then_some(cli)
}

/// Seconds left on an unexpired OAuth token that expires within
/// `window_seconds`.
fn expires_soon(auth: &ResolvedAuth, now: u64, window_seconds: u64) -> Option<u64> {
    let expires = auth.expires();
    (is_oauth(auth) && expires_within(expires, now, window_seconds) && !is_expired(expires, now))
        .then(|| expires - now)
}

/// Stderr text for a token with `remaining` seconds left, short of the
/// `requested` lifetime when one was asked for.
fn expiry_warning(
    provider: &str,
    remaining: u64,
    requested: Option<u64>,
    imported: Option<&str>,
) -> String {
    let source = imported
        .map(|cli| format!(" from {cli}"))
        .unwrap_or_default();
    let shortfall = requested
        .map(|seconds| format!(", sooner than the requested {}", format_remaining(seconds)))
        .unwrap_or_default();
    let reason = imported
        .map(|cli| {
            format!("; pi refreshes {cli}'s login only once it expires, so {cli} stays signed in")
        })
        .unwrap_or_default();
    format!(
        "the {provider} OAuth token{source} expires in {}{shortfall}{reason}",
        format_remaining(remaining)
    )
}

/// `45s`, `12m`, or `2h 5m`.
fn format_remaining(seconds: u64) -> String {
    match (seconds / 3600, seconds % 3600 / 60) {
        (0, 0) => format!("{seconds}s"),
        (0, minutes) => format!("{minutes}m"),
        (hours, minutes) => format!("{hours}h {minutes}m"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckStatus {
    Ready,
    NotReady,
    Invalid,
}

impl CheckStatus {
    fn label(self) -> &'static str {
        match self {
            CheckStatus::Ready => "ready",
            CheckStatus::NotReady => "not_ready",
            CheckStatus::Invalid => "invalid",
        }
    }

    fn exit_status(self) -> i32 {
        match self {
            CheckStatus::Ready => 0,
            CheckStatus::NotReady => 1,
            CheckStatus::Invalid => 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CheckResult {
    status: CheckStatus,
    provider: String,
    reason: Option<&'static str>,
    auth_type: Option<&'static str>,
    credential: Option<String>,
    /// Set when a ready OAuth token expires within the check's refresh
    /// window: JSON reports `expiresSoon`, text output warns on stderr.
    warning: Option<String>,
}

impl CheckResult {
    fn not_ready(provider: &str, reason: &'static str) -> Self {
        Self {
            status: CheckStatus::NotReady,
            provider: provider.to_string(),
            reason: Some(reason),
            auth_type: None,
            credential: None,
            warning: None,
        }
    }

    fn invalid(provider: &str) -> Self {
        Self {
            status: CheckStatus::Invalid,
            provider: provider.to_string(),
            reason: Some("invalid_state"),
            auth_type: None,
            credential: None,
            warning: None,
        }
    }

    fn render(&self, json: bool) -> String {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Output<'a> {
            status: &'a str,
            provider: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            reason: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            auth_type: Option<&'a str>,
            #[serde(skip_serializing_if = "std::ops::Not::not")]
            expires_soon: bool,
            #[serde(skip_serializing_if = "Option::is_none")]
            credentials: Option<&'a str>,
        }
        if !json {
            return self
                .credential
                .clone()
                .unwrap_or_else(|| self.status.label().to_string());
        }
        serde_json::to_string(&Output {
            status: self.status.label(),
            provider: &self.provider,
            reason: self.reason,
            auth_type: self.auth_type,
            expires_soon: self.warning.is_some(),
            credentials: self.credential.as_deref(),
        })
        .unwrap_or_default()
    }
}

async fn check(
    config: &LoadedConfig,
    command: &AuthCommand,
    refresher: &Refresher<'_>,
) -> Result<CheckResult, String> {
    let account = command.account.as_deref();
    let providers = match resolve_providers(config, command) {
        Ok(providers) => providers,
        Err(_) => {
            let label = command.provider.as_deref().or(command.model.as_deref());
            return Ok(CheckResult::invalid(label.unwrap_or_default()));
        }
    };
    // Only a lone --provider can be unknown: --model resolves known ones.
    if let [provider] = providers.as_slice() {
        if !provider_is_known(config, provider) {
            return Ok(CheckResult::not_ready(provider, "provider_not_found"));
        }
    }
    let provider = preferred_provider(
        config,
        providers_with_account(config, providers, account)?,
        account,
    );
    let Some(stored) = auth_for_provider(&config.auth, &provider, account)
        .filter(|auth| !credential_value(auth).trim().is_empty())
    else {
        return Ok(CheckResult::not_ready(
            &provider,
            "credentials_not_configured",
        ));
    };
    let auth = if command.no_refresh {
        stored
    } else {
        match refresher
            .auth(config, &provider, account, MIN_OAUTH_VALIDITY_SECONDS)
            .await
        {
            Ok(Some(auth)) => auth,
            Ok(None) => {
                return Ok(CheckResult::not_ready(
                    &provider,
                    "credentials_not_configured",
                ))
            }
            Err(_) => return Ok(CheckResult::invalid(&provider)),
        }
    };
    let credential = if command.credentials {
        let value = credential_value(&auth);
        if value.trim().is_empty() {
            return Ok(CheckResult::not_ready(
                &provider,
                "credential_not_available",
            ));
        }
        Some(value.to_string())
    } else {
        None
    };
    let warning = expires_soon(&auth, refresher.now, MIN_OAUTH_VALIDITY_SECONDS).map(|remaining| {
        let imported = imported_login(&config.auth, &provider, account);
        expiry_warning(&provider, remaining, None, imported)
    });
    Ok(CheckResult {
        status: CheckStatus::Ready,
        provider,
        reason: None,
        auth_type: Some(auth_type(&auth)),
        credential,
        warning,
    })
}

/// A credential to print, with a stderr warning when an imported login
/// expires sooner than the requested lifetime.
#[derive(Debug, PartialEq, Eq)]
struct Printed {
    value: String,
    warning: Option<String>,
}

async fn print_credential(
    config: &LoadedConfig,
    command: &AuthCommand,
    refresher: &Refresher<'_>,
) -> Result<Printed, String> {
    let account = command.account.as_deref();
    let providers = resolve_providers(config, command)?;
    if command.model.is_none() && !provider_is_known(config, &providers[0]) {
        return Err(format!(
            "Unknown provider \"{}\". Use --list-models to see available providers.",
            providers[0]
        ));
    }
    let providers = providers_with_account(config, providers, account)?;
    let bearer = command.kind == Kind::BearerToken;
    let requested = command
        .min_expiry_seconds
        .unwrap_or(DEFAULT_BEARER_MIN_EXPIRY_SECONDS);
    let window = requested.max(MIN_OAUTH_VALIDITY_SECONDS);
    let mut first_is_oauth = None;
    let mut found = Vec::<(String, Printed)>::new();
    for provider in &providers {
        let Some(stored) = auth_for_provider(&config.auth, provider, account) else {
            continue;
        };
        let oauth = is_oauth(&stored);
        first_is_oauth.get_or_insert(oauth);
        if oauth != bearer {
            continue;
        }
        let mut warning = None;
        let value = if bearer {
            let Some(auth) = refresher
                .auth(config, provider, account, window)
                .await
                .map_err(|error| format!("{error:#}"))?
            else {
                continue;
            };
            if expires_within(auth.expires(), refresher.now, window) {
                // An imported login is left unrefreshed on purpose: print it,
                // warning when it falls short of the requested lifetime.
                let imported = imported_login(&config.auth, provider, account)
                    .filter(|_| !is_expired(auth.expires(), refresher.now));
                if imported.is_none() {
                    return Err(if credential_value(&auth) == credential_value(&stored) {
                        format!("OAuth token for {provider} expires within the required lifetime and could not be refreshed")
                    } else {
                        format!(
                            "OAuth refresh returned a token that expires too soon for {provider}"
                        )
                    });
                }
                warning = expires_soon(&auth, refresher.now, requested).map(|remaining| {
                    expiry_warning(provider, remaining, Some(requested), imported)
                });
            }
            credential_value(&auth).to_string()
        } else {
            credential_value(&stored).to_string()
        };
        if !value.trim().is_empty() {
            found.push((provider.clone(), Printed { value, warning }));
        }
    }
    match found.len() {
        1 => Ok(found.remove(0).1),
        0 => Err(
            match (command.provider.as_deref(), bearer, first_is_oauth) {
                (Some(provider), false, Some(true)) => {
                    format!("Provider \"{provider}\" is configured with OAuth, not an API key")
                }
                (Some(provider), true, Some(false) | None) => {
                    format!("Provider \"{provider}\" is not configured with an OAuth bearer token")
                }
                _ if bearer => "No usable OAuth bearer token is configured".to_string(),
                _ => "No usable API key is configured".to_string(),
            },
        ),
        _ => Err(format!(
            "Multiple configured providers matched ({}). Specify --provider.",
            found
                .iter()
                .map(|(provider, _)| provider.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Providers the request names: `--provider` itself, or each provider whose
/// catalog lists the `--model` reference (`id`, `provider/id`, or display
/// name, with an optional `:<thinking>` suffix).
fn resolve_providers(config: &LoadedConfig, command: &AuthCommand) -> Result<Vec<String>, String> {
    let provider = command.provider.as_deref();
    let Some(model) = command.model.as_deref() else {
        return Ok(provider.map(str::to_string).into_iter().collect());
    };
    let model = match model.rsplit_once(':') {
        Some((base, level)) if THINKING_LEVELS.contains(&level) => base,
        _ => model,
    };
    let mut providers = Vec::<String>::new();
    for definition in &config.models {
        if provider.is_some_and(|provider| provider != definition.provider) {
            continue;
        }
        let matches = model == definition.id
            || model == format!("{}/{}", definition.provider, definition.id)
            || definition.name.as_deref() == Some(model);
        if matches && !providers.contains(&definition.provider) {
            providers.push(definition.provider.clone());
        }
    }
    if providers.is_empty() {
        // `provider/id` names a provider even when its catalog lacks the id.
        if let Some((prefix, id)) = model.split_once('/') {
            if !id.is_empty()
                && provider.is_none_or(|provider| provider == prefix)
                && provider_is_known(config, prefix)
            {
                providers.push(prefix.to_string());
            }
        }
    }
    if providers.is_empty() {
        return Err(format!(
            "Model \"{model}\" not found. Use --list-models to see available models."
        ));
    }
    Ok(providers)
}

/// A check reports on one provider: the first matching one with credentials.
fn preferred_provider(
    config: &LoadedConfig,
    providers: Vec<String>,
    account: Option<&str>,
) -> String {
    providers
        .iter()
        .find(|provider| auth_for_provider(&config.auth, provider, account).is_some())
        .unwrap_or(&providers[0])
        .clone()
}

fn provider_is_known(config: &LoadedConfig, provider: &str) -> bool {
    config.models.iter().any(|model| model.provider == provider)
        || !config.auth.accounts_for_provider(provider).is_empty()
}

/// With `--account`, the providers that have that account (as `pi accounts
/// status` lists them), so a `--model` several providers serve resolves to
/// the one holding it. It is an error only when none does.
fn providers_with_account(
    config: &LoadedConfig,
    providers: Vec<String>,
    account: Option<&str>,
) -> Result<Vec<String>, String> {
    let Some(account) = account else {
        return Ok(providers);
    };
    let (with_account, without): (Vec<_>, Vec<_>) = providers.into_iter().partition(|provider| {
        listed_accounts_for_provider(&config.auth, provider)
            .iter()
            .any(|(name, _)| name == account)
    });
    if !with_account.is_empty() {
        return Ok(with_account);
    }
    let providers = without
        .iter()
        .map(|provider| format!("\"{provider}\""))
        .collect::<Vec<_>>();
    Err(format!(
        "No account \"{account}\" for provider{} {}; run pi accounts status to list accounts",
        if providers.len() == 1 { "" } else { "s" },
        providers.join(", ")
    ))
}

fn is_oauth(auth: &ResolvedAuth) -> bool {
    !matches!(auth, ResolvedAuth::ApiKey(_))
}

fn auth_type(auth: &ResolvedAuth) -> &'static str {
    if is_oauth(auth) {
        "oauth"
    } else {
        "api_key"
    }
}

fn credential_value(auth: &ResolvedAuth) -> &str {
    match auth {
        ResolvedAuth::ApiKey(key) => key,
        ResolvedAuth::ClaudeCodeOAuth { access_token, .. }
        | ResolvedAuth::ChatGptOAuth { access_token, .. } => access_token,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_config::{AuthCredential, ModelDefinition, ProviderApi};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    fn model(provider: &str, id: &str) -> ModelDefinition {
        ModelDefinition {
            provider: provider.to_string(),
            id: id.to_string(),
            name: None,
            api: ProviderApi::default(),
            base_url: None,
            context_window: None,
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "pi-cli-auth-command-{name}-{}-{}",
            std::process::id(),
            crate::unique_temp_suffix()
        ))
    }

    /// Synthetic providers without environment or CLI-import fallbacks, so
    /// tests never observe credentials from the machine running them.
    fn config_with(auth: &[(&str, &str, AuthCredential)], root: &std::path::Path) -> LoadedConfig {
        let mut config = crate::tests::minimal_test_config();
        config.paths.auth_path = root.join("auth.json");
        config.models = vec![
            model("acme", "acme-large"),
            model("acme", "shared"),
            model("zeta", "shared"),
            model("anthropic", "claude-test"),
        ];
        for (provider, account, credential) in auth {
            config.auth.insert(provider, account, credential.clone());
        }
        config
    }

    fn api_key(key: &str) -> AuthCredential {
        AuthCredential::ApiKey {
            key: key.to_string(),
        }
    }

    fn oauth(access: &str, refresh: Option<&str>, expires: u64) -> AuthCredential {
        AuthCredential::OAuth {
            access_token: access.to_string(),
            refresh_token: refresh.map(str::to_string),
            expires,
            account_id: None,
        }
    }

    fn command(values: &[&str]) -> AuthCommand {
        let command = parse(&args(values)).expect("parse auth command");
        validate(&command).expect("valid auth command");
        command
    }

    fn printed(value: &str) -> Printed {
        Printed {
            value: value.to_string(),
            warning: None,
        }
    }

    /// Token endpoint that answers every request with `body`; returns its URL
    /// and a request count.
    fn token_stub(body: &'static str) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub");
        let url = format!("http://{}", listener.local_addr().expect("stub addr"));
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        std::thread::spawn(move || {
            while let Ok((stream, _)) = listener.accept() {
                counter.fetch_add(1, Ordering::SeqCst);
                let mut reader = BufReader::new(stream);
                let mut content_length = 0;
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok() && !line.trim_end().is_empty() {
                    if let Some(value) = line
                        .trim_end()
                        .to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                    {
                        content_length = value.trim().parse().unwrap_or(0);
                    }
                    line.clear();
                }
                let mut request_body = vec![0; content_length];
                let _ = reader.read_exact(&mut request_body);
                let mut stream = reader.into_inner();
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (url, hits)
    }

    #[test]
    fn parse_accepts_upstream_subcommands_and_rejects_misplaced_options() {
        let parsed = command(&[
            "print-bearer-token",
            "--provider",
            "anthropic",
            "--min-expiry",
            "45m",
        ]);
        assert_eq!(parsed.kind, Kind::BearerToken);
        assert_eq!(parsed.provider.as_deref(), Some("anthropic"));
        assert_eq!(parsed.min_expiry_seconds, Some(45 * 60));

        let parsed = command(&["check", "--model", "acme-large", "--json", "--credentials"]);
        assert!(parsed.json && parsed.credentials && !parsed.no_refresh);

        for (input, expected) in [
            (vec!["status"], "Unknown auth command \"status\""),
            (
                vec!["check", "--provider", "acme", "--min-expiry", "1h"],
                "--min-expiry is only supported by print-bearer-token",
            ),
            (
                vec!["print-api-key", "--provider", "acme", "--json"],
                "--json is only supported by auth check",
            ),
            (
                vec!["print-bearer-token", "--min-expiry", "soon"],
                "--min-expiry must use a duration",
            ),
            (
                vec!["check", "--provider", "acme", "--verbose"],
                "Unknown option --verbose for \"auth check\"",
            ),
            (vec!["check", "--provider"], "--provider requires a value"),
        ] {
            let error = parse(&args(&input)).expect_err("invalid auth command");
            assert!(error.contains(expected), "{input:?}: {error}");
        }

        let missing = parse(&args(&["check"])).expect("parse");
        assert!(validate(&missing)
            .expect_err("missing provider")
            .contains("Auth checks require --provider"));
        let missing = parse(&args(&["print-api-key"])).expect("parse");
        assert!(validate(&missing)
            .expect_err("missing provider")
            .contains("Credential printing requires --provider"));
        let positional = parse(&args(&["check", "--provider", "acme", "extra"])).expect("parse");
        assert!(validate(&positional).is_err());
    }

    #[test]
    fn durations_use_upstream_units_and_round_up_to_seconds() {
        assert_eq!(parse_duration_seconds("30m"), Some(1800));
        assert_eq!(parse_duration_seconds("1H"), Some(3600));
        assert_eq!(parse_duration_seconds("45s"), Some(45));
        assert_eq!(parse_duration_seconds("1500ms"), Some(2));
        for invalid in ["", "m", "10", "10d", "-5m", "1.5h"] {
            assert_eq!(parse_duration_seconds(invalid), None, "{invalid}");
        }
    }

    #[tokio::test]
    async fn check_reports_ready_not_ready_and_invalid() {
        let root = temp_dir("check");
        let config = config_with(&[("acme", "default", api_key("acme-secret"))], &root);
        let client = reqwest::Client::new();
        let refresher = Refresher {
            client: &client,
            endpoint: None,
            now: 1_000,
        };

        let ready = check(
            &config,
            &command(&["check", "--provider", "acme"]),
            &refresher,
        )
        .await
        .expect("check");
        assert_eq!(ready.status, CheckStatus::Ready);
        assert_eq!(ready.render(false), "ready");
        assert_eq!(
            ready.render(true),
            r#"{"status":"ready","provider":"acme","authType":"api_key"}"#
        );

        let with_credentials = check(
            &config,
            &command(&["check", "--model", "acme/acme-large", "--credentials"]),
            &refresher,
        )
        .await
        .expect("check");
        assert_eq!(with_credentials.render(false), "acme-secret");
        assert_eq!(
            with_credentials.render(true),
            r#"{"status":"ready","provider":"acme","authType":"api_key","credentials":"acme-secret"}"#
        );

        let unconfigured = check(
            &config,
            &command(&["check", "--provider", "zeta"]),
            &refresher,
        )
        .await
        .expect("check");
        assert_eq!(unconfigured.status.exit_status(), 1);
        assert_eq!(
            unconfigured.render(true),
            r#"{"status":"not_ready","provider":"zeta","reason":"credentials_not_configured"}"#
        );

        let unknown = check(
            &config,
            &command(&["check", "--provider", "nope"]),
            &refresher,
        )
        .await
        .expect("check");
        assert_eq!(unknown.reason, Some("provider_not_found"));

        let missing_model = check(
            &config,
            &command(&["check", "--model", "missing"]),
            &refresher,
        )
        .await
        .expect("check");
        assert_eq!(missing_model.status.exit_status(), 2);
        assert_eq!(missing_model.render(false), "invalid");

        // A shared model id resolves to the provider that has credentials.
        let shared = check(
            &config,
            &command(&["check", "--model", "shared"]),
            &refresher,
        )
        .await
        .expect("check");
        assert_eq!(shared.provider, "acme");

        let unknown_account = check(
            &config,
            &command(&["check", "--provider", "acme", "--account", "work"]),
            &refresher,
        )
        .await
        .expect_err("unknown account");
        assert!(
            unknown_account.contains("No account \"work\""),
            "{unknown_account}"
        );
    }

    #[tokio::test]
    async fn check_refresh_failures_are_invalid_unless_refresh_is_disabled() {
        let root = temp_dir("check-refresh");
        let config = config_with(
            &[("anthropic", "default", oauth("expired-access", None, 500))],
            &root,
        );
        let client = reqwest::Client::new();
        let refresher = Refresher {
            client: &client,
            endpoint: None,
            now: 1_000,
        };

        let refreshed = check(
            &config,
            &command(&["check", "--provider", "anthropic"]),
            &refresher,
        )
        .await
        .expect("check");
        assert_eq!(refreshed.status, CheckStatus::Invalid);

        let stored = check(
            &config,
            &command(&["check", "--provider", "anthropic", "--no-refresh", "--json"]),
            &refresher,
        )
        .await
        .expect("check");
        assert_eq!(
            stored.render(true),
            r#"{"status":"ready","provider":"anthropic","authType":"oauth"}"#
        );
    }

    #[tokio::test]
    async fn print_api_key_requires_one_api_key_credential() {
        let root = temp_dir("api-key");
        let config = config_with(
            &[
                ("acme", "default", api_key("acme-secret")),
                ("zeta", "default", api_key("zeta-secret")),
                ("anthropic", "default", oauth("oauth-access", None, 0)),
            ],
            &root,
        );
        let client = reqwest::Client::new();
        let refresher = Refresher {
            client: &client,
            endpoint: None,
            now: 1_000,
        };
        let print = |values: &'static [&'static str]| {
            let command = command(values);
            let config = &config;
            let refresher = &refresher;
            async move { print_credential(config, &command, refresher).await }
        };

        assert_eq!(
            print(&["print-api-key", "--provider", "acme"]).await,
            Ok(printed("acme-secret"))
        );
        assert_eq!(
            print(&["print-api-key", "--model", "acme-large"]).await,
            Ok(printed("acme-secret"))
        );
        assert_eq!(
            print(&["print-api-key", "--provider", "zeta", "--model", "shared"]).await,
            Ok(printed("zeta-secret"))
        );
        let ambiguous = print(&["print-api-key", "--model", "shared"])
            .await
            .expect_err("ambiguous model");
        assert!(ambiguous.contains("Multiple configured providers matched (acme, zeta)"));
        assert_eq!(
            print(&["print-api-key", "--provider", "anthropic"]).await,
            Err("Provider \"anthropic\" is configured with OAuth, not an API key".to_string())
        );
        assert_eq!(
            print(&["print-bearer-token", "--provider", "acme"]).await,
            Err("Provider \"acme\" is not configured with an OAuth bearer token".to_string())
        );
        let unknown = print(&["print-api-key", "--provider", "nope"])
            .await
            .expect_err("unknown provider");
        assert!(unknown.contains("Unknown provider \"nope\""));
    }

    #[tokio::test]
    async fn print_bearer_token_enforces_min_expiry_and_refreshes() {
        let root = temp_dir("bearer");
        let client = reqwest::Client::new();
        // No refresh token: nothing may reach the real Claude token endpoint.
        let long_lived = config_with(
            &[(
                "anthropic",
                "default",
                oauth("long-access", None, 1_000 + 3 * 3600),
            )],
            &root,
        );
        let refresher = Refresher {
            client: &client,
            endpoint: None,
            now: 1_000,
        };
        assert_eq!(
            print_credential(
                &long_lived,
                &command(&["print-bearer-token", "--provider", "anthropic"]),
                &refresher,
            )
            .await,
            Ok(printed("long-access"))
        );
        let too_short = print_credential(
            &long_lived,
            &command(&[
                "print-bearer-token",
                "--provider",
                "anthropic",
                "--min-expiry",
                "4h",
            ]),
            &refresher,
        )
        .await
        .expect_err("token shorter than --min-expiry cannot be refreshed");
        assert!(too_short.contains("could not be refreshed"), "{too_short}");

        let expiring = config_with(
            &[(
                "anthropic",
                "default",
                oauth("ten-minutes-left", Some("refresh"), 1_600),
            )],
            &root,
        );
        let (url, hits) = token_stub(
            r#"{"access_token":"fresh-access","refresh_token":"fresh-refresh","expires_in":7200}"#,
        );
        let refresher = Refresher {
            client: &client,
            endpoint: Some((url.as_str(), "test-client")),
            now: 1_000,
        };
        assert_eq!(
            print_credential(
                &expiring,
                &command(&["print-bearer-token", "--provider", "anthropic"]),
                &refresher,
            )
            .await,
            Ok(printed("fresh-access"))
        );
        let persisted = std::fs::read_to_string(root.join("auth.json")).expect("persisted auth");
        assert!(persisted.contains("fresh-access"));
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // The claude-import copy pi saved after refreshing a Claude Code login
        // holds pi's own refresh token, so it refreshes early like any account.
        let copy_root = temp_dir("bearer-import-copy");
        let saved_copy = config_with(
            &[(
                "anthropic",
                "claude-import",
                oauth("ten-minutes-left", Some("pi-refresh"), 1_600),
            )],
            &copy_root,
        );
        assert_eq!(
            print_credential(
                &saved_copy,
                &command(&["print-bearer-token", "--provider", "anthropic"]),
                &refresher,
            )
            .await,
            Ok(printed("fresh-access"))
        );
        assert_eq!(hits.load(Ordering::SeqCst), 2);

        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(copy_root);
    }

    #[tokio::test]
    async fn account_narrows_model_matches_to_providers_with_that_account() {
        let root = temp_dir("account");
        let mut auth = vec![
            ("acme", "default", api_key("acme-secret")),
            ("zeta", "default", api_key("zeta-secret")),
            ("zeta", "work", api_key("zeta-work")),
        ];
        let config = config_with(&auth, &root);
        let client = reqwest::Client::new();
        let refresher = Refresher {
            client: &client,
            endpoint: None,
            now: 1_000,
        };

        // Only zeta has "work", so the model both providers serve resolves to it.
        assert_eq!(
            print_credential(
                &config,
                &command(&["print-api-key", "--model", "shared", "--account", "work"]),
                &refresher,
            )
            .await,
            Ok(printed("zeta-work"))
        );
        let checked = check(
            &config,
            &command(&[
                "check",
                "--model",
                "shared",
                "--account",
                "work",
                "--credentials",
            ]),
            &refresher,
        )
        .await
        .expect("check");
        assert_eq!(checked.provider, "zeta");
        assert_eq!(checked.render(false), "zeta-work");

        // An account no matching provider has is an error for both commands.
        let missing = print_credential(
            &config,
            &command(&["print-api-key", "--model", "shared", "--account", "home"]),
            &refresher,
        )
        .await
        .expect_err("no provider has the account");
        assert_eq!(
            missing,
            "No account \"home\" for providers \"acme\", \"zeta\"; run pi accounts status to list accounts"
        );
        let missing = check(
            &config,
            &command(&["check", "--model", "shared", "--account", "home"]),
            &refresher,
        )
        .await
        .expect_err("no provider has the account");
        assert!(missing.starts_with("No account \"home\""), "{missing}");

        // When several have it, printing stays ambiguous.
        auth.push(("acme", "work", api_key("acme-work")));
        let both = config_with(&auth, &root);
        let ambiguous = print_credential(
            &both,
            &command(&["print-api-key", "--model", "shared", "--account", "work"]),
            &refresher,
        )
        .await
        .expect_err("ambiguous account");
        assert!(
            ambiguous.contains("Multiple configured providers matched (acme, zeta)"),
            "{ambiguous}"
        );
    }

    #[test]
    fn imported_logins_are_the_ones_pi_has_no_stored_account_for() {
        let mut auth = AuthData::default();
        // With nothing stored, pi reads Claude Code's or Codex's own login.
        assert_eq!(
            imported_login(&auth, "anthropic", None),
            Some("Claude Code")
        );
        assert_eq!(imported_login(&auth, "openai-codex", None), Some("Codex"));
        assert_eq!(
            imported_login(&auth, "openai", Some("codex-import")),
            Some("Codex")
        );
        // Other providers have no login file to import.
        assert_eq!(imported_login(&auth, "acme", None), None);

        // pi's own accounts, including the copy it saves after a refresh.
        auth.insert(
            "anthropic",
            "claude-import",
            oauth("copy", Some("pi-refresh"), 0),
        );
        assert_eq!(imported_login(&auth, "anthropic", None), None);
        assert_eq!(
            imported_login(&auth, "anthropic", Some("claude-import")),
            None
        );
        assert_eq!(
            imported_login(&auth, "anthropic", Some("work")),
            Some("Claude Code")
        );
        // Several accounts without a default leave the login file in use.
        auth.insert("anthropic", "work", api_key("work-key"));
        assert_eq!(
            imported_login(&auth, "anthropic", None),
            Some("Claude Code")
        );
        assert_eq!(imported_login(&auth, "anthropic", Some("work")), None);
        auth.insert("anthropic", "default", api_key("default-key"));
        assert_eq!(imported_login(&auth, "anthropic", None), None);
    }

    #[tokio::test]
    async fn check_refreshes_pi_tokens_early_and_reports_ones_expiring_soon() {
        let root = temp_dir("check-expiry");
        let client = reqwest::Client::new();
        let (url, hits) = token_stub(
            r#"{"access_token":"fresh-access","refresh_token":"fresh-refresh","expires_in":7200}"#,
        );
        let refresher = Refresher {
            client: &client,
            endpoint: Some((url.as_str(), "test-client")),
            now: 1_000,
        };
        // pi's own token, three minutes from expiry: inside the check window.
        let expiring = config_with(
            &[(
                "anthropic",
                "default",
                oauth("three-minutes-left", Some("refresh"), 1_180),
            )],
            &root,
        );

        let refreshed = check(
            &expiring,
            &command(&["check", "--provider", "anthropic", "--credentials"]),
            &refresher,
        )
        .await
        .expect("check");
        assert_eq!(refreshed.render(false), "fresh-access");
        assert_eq!(refreshed.warning, None);
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // Unrefreshed, it stays ready but expires soon.
        let kept = check(
            &expiring,
            &command(&["check", "--provider", "anthropic", "--no-refresh", "--json"]),
            &refresher,
        )
        .await
        .expect("check");
        assert_eq!(kept.status.exit_status(), 0);
        assert_eq!(
            kept.render(true),
            r#"{"status":"ready","provider":"anthropic","authType":"oauth","expiresSoon":true}"#
        );
        assert_eq!(
            kept.warning.as_deref(),
            Some("the anthropic OAuth token expires in 3m")
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // So does a token pi has no refresh token for. It gets its own auth
        // file: the one above now holds the refreshed token, which wins.
        let unrefreshable = config_with(
            &[(
                "anthropic",
                "default",
                oauth("no-refresh-token", None, 1_180),
            )],
            &root.join("unrefreshable"),
        );
        let result = check(
            &unrefreshable,
            &command(&["check", "--provider", "anthropic"]),
            &refresher,
        )
        .await
        .expect("check");
        assert_eq!(result.render(false), "ready");
        assert!(result.warning.is_some());
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn expiry_warnings_name_the_imported_login_and_requested_lifetime() {
        assert_eq!(
            expiry_warning("anthropic", 12 * 60 + 30, Some(30 * 60), Some("Claude Code")),
            "the anthropic OAuth token from Claude Code expires in 12m, sooner than the requested 30m; pi refreshes Claude Code's login only once it expires, so Claude Code stays signed in"
        );
        assert_eq!(
            expiry_warning("openai-codex", 45, None, None),
            "the openai-codex OAuth token expires in 45s"
        );
        assert_eq!(format_remaining(2 * 3600 + 5 * 60 + 9), "2h 5m");
        assert_eq!(
            expires_soon(
                &ResolvedAuth::ApiKey("key".to_string()),
                1_000,
                MIN_OAUTH_VALIDITY_SECONDS
            ),
            None
        );
    }
}
