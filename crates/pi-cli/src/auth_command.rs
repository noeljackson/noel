//! `pi auth`: print a provider credential for an external client, or check
//! whether a provider is ready. Mirrors upstream's credential commands on top
//! of pi's own auth resolution (named accounts, environment, CLI imports).

use std::io::Write as _;
use std::time::Duration;

use anyhow::Result;
use pi_config::{
    auth_for_provider, expires_within, listed_accounts_for_provider,
    load_config_with_project_trust, project_is_trusted, ConfigPaths, LoadedConfig, ResolvedAuth,
};
use serde::Serialize;

use crate::{oauth_refresh, unix_seconds};

const USAGE: &str = "Usage:
  pi auth print-api-key [--provider <provider>] [--model <model>] [--account <name>]
  pi auth print-bearer-token [--provider <provider>] [--model <model>] [--account <name>] [--min-expiry <duration>]
  pi auth check [--provider <provider>] [--model <model>] [--account <name>] [--json] [--credentials] [--no-refresh]

Auth commands require at least one of --provider or --model. Checks refresh expired OAuth credentials by default; --no-refresh prevents this. --credentials emits the credential, or includes it in JSON output. print-bearer-token refreshes tokens that expire within --min-expiry (ms, s, m, or h; default 30m). --account selects a named account (see pi accounts status). Credential-printing commands write secrets to stdout.";

/// OAuth tokens closer than this to expiry are refreshed before use.
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

    /// Checks reserve exit status 1 for "not ready", so their errors use 2.
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
        Ok((output, status)) => {
            println!("{output}");
            status
        }
        Err(message) => {
            eprintln!("Error: {message}");
            command.kind.error_status()
        }
    };
    let _ = std::io::stdout().flush();
    status
}

async fn execute(
    command: &AuthCommand,
    refresher: &Refresher<'_>,
) -> Result<(String, i32), String> {
    validate(command)?;
    let config = load_auth_config().map_err(|error| format!("{error:#}"))?;
    match command.kind {
        Kind::Check => {
            let result = check(&config, command, refresher).await?;
            Ok((result.render(command.json), result.status.exit_status()))
        }
        Kind::ApiKey | Kind::BearerToken => {
            Ok((print_credential(&config, command, refresher).await?, 0))
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
    async fn auth(
        &self,
        config: &LoadedConfig,
        provider: &str,
        account: Option<&str>,
        window_seconds: u64,
    ) -> Result<Option<ResolvedAuth>> {
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
}

impl CheckResult {
    fn not_ready(provider: &str, reason: &'static str) -> Self {
        Self {
            status: CheckStatus::NotReady,
            provider: provider.to_string(),
            reason: Some(reason),
            auth_type: None,
            credential: None,
        }
    }

    fn invalid(provider: &str) -> Self {
        Self {
            status: CheckStatus::Invalid,
            provider: provider.to_string(),
            reason: Some("invalid_state"),
            auth_type: None,
            credential: None,
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
    let provider = match resolve_providers(config, command) {
        Ok(providers) => preferred_provider(config, providers, account),
        Err(_) => {
            let label = command.provider.as_deref().or(command.model.as_deref());
            return Ok(CheckResult::invalid(label.unwrap_or_default()));
        }
    };
    if !provider_is_known(config, &provider) {
        return Ok(CheckResult::not_ready(&provider, "provider_not_found"));
    }
    validate_account(config, &provider, account)?;
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
    Ok(CheckResult {
        status: CheckStatus::Ready,
        provider,
        reason: None,
        auth_type: Some(auth_type(&auth)),
        credential,
    })
}

async fn print_credential(
    config: &LoadedConfig,
    command: &AuthCommand,
    refresher: &Refresher<'_>,
) -> Result<String, String> {
    let account = command.account.as_deref();
    let providers = resolve_providers(config, command)?;
    if command.model.is_none() && !provider_is_known(config, &providers[0]) {
        return Err(format!(
            "Unknown provider \"{}\". Use --list-models to see available providers.",
            providers[0]
        ));
    }
    let bearer = command.kind == Kind::BearerToken;
    let window = command
        .min_expiry_seconds
        .unwrap_or(DEFAULT_BEARER_MIN_EXPIRY_SECONDS)
        .max(MIN_OAUTH_VALIDITY_SECONDS);
    let mut first_is_oauth = None;
    let mut found = Vec::<(String, String)>::new();
    for provider in &providers {
        validate_account(config, provider, account)?;
        let Some(stored) = auth_for_provider(&config.auth, provider, account) else {
            continue;
        };
        let oauth = is_oauth(&stored);
        first_is_oauth.get_or_insert(oauth);
        if oauth != bearer {
            continue;
        }
        let value = if bearer {
            let Some(auth) = refresher
                .auth(config, provider, account, window)
                .await
                .map_err(|error| format!("{error:#}"))?
            else {
                continue;
            };
            if expires_within(auth.expires(), refresher.now, window) {
                return Err(if credential_value(&auth) == credential_value(&stored) {
                    format!("OAuth token for {provider} expires within the required lifetime and could not be refreshed")
                } else {
                    format!("OAuth refresh returned a token that expires too soon for {provider}")
                });
            }
            credential_value(&auth).to_string()
        } else {
            credential_value(&stored).to_string()
        };
        if !value.trim().is_empty() {
            found.push((provider.clone(), value));
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

fn validate_account(
    config: &LoadedConfig,
    provider: &str,
    account: Option<&str>,
) -> Result<(), String> {
    let Some(account) = account else {
        return Ok(());
    };
    if listed_accounts_for_provider(&config.auth, provider)
        .iter()
        .any(|(name, _)| name == account)
    {
        Ok(())
    } else {
        Err(format!(
            "No account \"{account}\" for provider \"{provider}\"; run pi accounts status to list accounts"
        ))
    }
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

    /// One-shot token endpoint that answers every request with `body`.
    fn token_stub(body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub");
        let url = format!("http://{}", listener.local_addr().expect("stub addr"));
        std::thread::spawn(move || {
            while let Ok((stream, _)) = listener.accept() {
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
        url
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
            Ok("acme-secret".to_string())
        );
        assert_eq!(
            print(&["print-api-key", "--model", "acme-large"]).await,
            Ok("acme-secret".to_string())
        );
        assert_eq!(
            print(&["print-api-key", "--provider", "zeta", "--model", "shared"]).await,
            Ok("zeta-secret".to_string())
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
            Ok("long-access".to_string())
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
        let url = token_stub(
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
            Ok("fresh-access".to_string())
        );
        let persisted = std::fs::read_to_string(root.join("auth.json")).expect("persisted auth");
        assert!(persisted.contains("fresh-access"));

        let _ = std::fs::remove_dir_all(root);
    }
}
