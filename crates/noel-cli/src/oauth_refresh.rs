use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use noel_config::{
    auth_for_provider, expires_within, import_account_name, is_expired, read_auth_file,
    write_auth_file, AuthCredential, AuthData, AuthFileLock, ResolvedAuth, AUTH_LOCK_TIMEOUT,
};
use serde::Deserialize;

pub const CODEX_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const CLAUDE_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub const CLAUDE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
/// Fallback for the Claude token endpoint: the same OAuth server is also
/// served from the API host, which some egress filters allow when they block
/// `platform.claude.com`.
pub const CLAUDE_TOKEN_FALLBACK_URL: &str = "https://api.anthropic.com/v1/oauth/token";

/// The request path refreshes OAuth tokens this close to expiry.
pub const REFRESH_WINDOW_SECONDS: u64 = 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshedTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires: u64,
}

#[derive(Debug, Deserialize)]
struct RefreshResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    expires_at: Option<u64>,
}

fn token_endpoint(provider: &str) -> Option<(&'static str, &'static str)> {
    match provider {
        "anthropic" => Some((CLAUDE_TOKEN_URL, CLAUDE_CLIENT_ID)),
        "openai-codex" => Some((CODEX_TOKEN_URL, CODEX_CLIENT_ID)),
        _ => None,
    }
}

fn persisted_account_name(auth: &AuthData, provider: &str, bound: Option<&str>) -> String {
    bound
        .map(str::to_string)
        .or_else(|| auth.account_for_resolution(provider).map(str::to_string))
        .unwrap_or_else(|| import_account_name(provider).to_string())
}

fn relogin_hint(provider: &str) -> &'static str {
    match provider {
        "anthropic" => "claude login",
        _ => "codex login",
    }
}

fn refresh_failure(account: &str, provider: &str, reason: &str) -> anyhow::Error {
    anyhow!(
        "account {account}: token expired and refresh failed ({reason}); re-run {} or noel login {provider} --api-key",
        relogin_hint(provider)
    )
}

pub fn needs_refresh(auth: &ResolvedAuth, now: u64, window_seconds: u64) -> bool {
    auth.refresh_token()
        .map(|token| !token.trim().is_empty())
        .unwrap_or(false)
        && expires_within(auth.expires(), now, window_seconds)
}

fn refresh_request_form<'a>(
    client_id: &'a str,
    refresh_token: &'a str,
) -> [(&'static str, &'a str); 3] {
    [
        ("grant_type", "refresh_token"),
        ("client_id", client_id),
        ("refresh_token", refresh_token),
    ]
}

pub fn parse_refresh_response(
    body: &str,
    previous_refresh_token: &str,
    now: u64,
) -> Result<RefreshedTokens> {
    let response = serde_json::from_str::<RefreshResponse>(body)?;
    if response.access_token.trim().is_empty() {
        return Err(anyhow!("refresh response did not include an access token"));
    }
    let expires = match (response.expires_in, response.expires_at) {
        (Some(seconds), _) => now.saturating_add(seconds),
        (None, Some(at)) if at > 1_000_000_000_000 => at / 1000,
        (None, Some(at)) => at,
        (None, None) => 0,
    };
    Ok(RefreshedTokens {
        access_token: response.access_token,
        refresh_token: response
            .refresh_token
            .filter(|token| !token.trim().is_empty())
            .unwrap_or_else(|| previous_refresh_token.to_string()),
        expires,
    })
}

pub async fn execute_refresh(
    client: &reqwest::Client,
    token_url: &str,
    client_id: &str,
    refresh_token: &str,
    now: u64,
) -> Result<RefreshedTokens> {
    let response = client
        .post(token_url)
        .form(&refresh_request_form(client_id, refresh_token))
        .send()
        .await
        .context("oauth token refresh request failed")?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!("token endpoint returned {status}: {body}"));
    }
    parse_refresh_response(&body, refresh_token, now)
}

/// A transport-level failure (connect error, timeout, TLS) rather than an
/// OAuth decision: worth retrying against a fallback endpoint.
fn is_transport_failure(error: &anyhow::Error) -> bool {
    error.downcast_ref::<reqwest::Error>().is_some_and(|error| {
        !matches!(
            error.status(),
            Some(status) if status.is_client_error() || status.is_server_error()
        )
    })
}

/// Refresh with endpoint fallback: when `token_url` is the canonical Claude
/// primary, retry once against `api.anthropic.com` if the first attempt never
/// reached a server (connect error, timeout, TLS). OAuth denials are
/// returned as-is so callers keep reporting `invalid_grant` instead of
/// masking it by trying the mirror.
async fn execute_refresh_with_fallback(
    client: &reqwest::Client,
    token_url: &str,
    fallback_url: Option<&str>,
    client_id: &str,
    refresh_token: &str,
    now: u64,
) -> Result<RefreshedTokens> {
    let primary = execute_refresh(client, token_url, client_id, refresh_token, now).await;
    if let (Some(fallback_url), Err(error)) = (fallback_url, &primary) {
        if is_transport_failure(error) {
            return execute_refresh(client, fallback_url, client_id, refresh_token, now).await;
        }
    }
    primary
}

type RefreshLockMap = BTreeMap<(String, String), Arc<tokio::sync::Mutex<()>>>;

fn refresh_lock(provider: &str, account: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<Mutex<RefreshLockMap>> = OnceLock::new();
    LOCKS
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .entry((provider.to_string(), account.to_string()))
        .or_default()
        .clone()
}

/// The auth.json lock that other noel processes take for logins, logouts, and
/// refreshes, awaited without blocking the runtime.
async fn lock_auth_file(auth_path: &Path) -> Result<AuthFileLock> {
    let deadline = tokio::time::Instant::now() + AUTH_LOCK_TIMEOUT;
    loop {
        if let Some(lock) = AuthFileLock::try_acquire(auth_path)? {
            return Ok(lock);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(anyhow!(
                "timed out waiting for another noel process to finish updating {}",
                auth_path.display()
            ));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Stores refreshed tokens for one account in auth.json as it is now; the
/// caller holds the auth.json lock.
fn persist_tokens(
    auth_path: &Path,
    provider: &str,
    account: &str,
    tokens: &RefreshedTokens,
    account_id: Option<String>,
) -> Result<()> {
    let mut stored = read_auth_file(auth_path)?;
    stored.insert(
        provider,
        account,
        AuthCredential::OAuth {
            access_token: tokens.access_token.clone(),
            refresh_token: Some(tokens.refresh_token.clone()),
            expires: tokens.expires,
            account_id,
        },
    );
    write_auth_file(auth_path, &stored)?;
    Ok(())
}

fn tokens_to_auth(
    provider: &str,
    previous: &ResolvedAuth,
    tokens: &RefreshedTokens,
) -> ResolvedAuth {
    let account_id = account_id_of(previous);
    match provider {
        "anthropic" => ResolvedAuth::ClaudeCodeOAuth {
            access_token: tokens.access_token.clone(),
            refresh_token: Some(tokens.refresh_token.clone()),
            expires: tokens.expires,
        },
        _ => ResolvedAuth::ChatGptOAuth {
            access_token: tokens.access_token.clone(),
            refresh_token: Some(tokens.refresh_token.clone()),
            expires: tokens.expires,
            account_id,
        },
    }
}

fn account_id_of(auth: &ResolvedAuth) -> Option<String> {
    match auth {
        ResolvedAuth::ChatGptOAuth { account_id, .. } => account_id.clone(),
        _ => None,
    }
}

pub async fn refresh_expiring_auth(
    client: &reqwest::Client,
    auth_path: &Path,
    auth: &AuthData,
    provider: &str,
    account: Option<&str>,
    endpoint_override: Option<(&str, &str)>,
    now: u64,
) -> Result<Option<ResolvedAuth>> {
    refresh_auth_expiring_within(
        client,
        auth_path,
        auth,
        provider,
        account,
        endpoint_override,
        now,
        REFRESH_WINDOW_SECONDS,
    )
    .await
}

/// [`refresh_expiring_auth`] for callers that need more remaining lifetime
/// than the request path: tokens expiring within `window_seconds` refresh.
#[allow(clippy::too_many_arguments)]
pub async fn refresh_auth_expiring_within(
    client: &reqwest::Client,
    auth_path: &Path,
    auth: &AuthData,
    provider: &str,
    account: Option<&str>,
    endpoint_override: Option<(&str, &str)>,
    now: u64,
    window_seconds: u64,
) -> Result<Option<ResolvedAuth>> {
    let Some(resolved) = auth_for_provider(auth, provider, account) else {
        return Ok(None);
    };
    if !expires_within(resolved.expires(), now, window_seconds) {
        return Ok(Some(resolved));
    }
    let account = persisted_account_name(auth, provider, account);
    let lock = refresh_lock(provider, &account);
    let _guard = lock.lock().await;
    let _file_lock = lock_auth_file(auth_path).await?;

    // `auth` may be a snapshot from before an earlier refresh, here or in
    // another process. Refresh tokens rotate, so continue from the auth
    // file's credential: it may be fresh already, and only it holds the
    // refresh token that is still valid.
    let stored = read_auth_file(auth_path)?;
    let resolved = stored
        .credential(provider, &account)
        .and_then(|_| auth_for_provider(&stored, provider, Some(&account)))
        .unwrap_or(resolved);
    if !expires_within(resolved.expires(), now, window_seconds) {
        return Ok(Some(resolved));
    }
    if !needs_refresh(&resolved, now, window_seconds) {
        if is_expired(resolved.expires(), now) {
            return Err(refresh_failure(
                &account,
                provider,
                "no refresh token is stored",
            ));
        }
        return Ok(Some(resolved));
    }
    let Some((token_url, client_id)) = endpoint_override.or_else(|| token_endpoint(provider))
    else {
        return Ok(Some(resolved));
    };

    let fallback_url = (token_url == CLAUDE_TOKEN_URL).then_some(CLAUDE_TOKEN_FALLBACK_URL);
    match execute_refresh_with_fallback(
        client,
        token_url,
        fallback_url,
        client_id,
        resolved.refresh_token().unwrap_or(""),
        now,
    )
    .await
    {
        Ok(tokens) => {
            persist_tokens(
                auth_path,
                provider,
                &account,
                &tokens,
                account_id_of(&resolved),
            )?;
            Ok(Some(tokens_to_auth(provider, &resolved, &tokens)))
        }
        Err(error) if is_expired(resolved.expires(), now) => {
            Err(refresh_failure(&account, provider, &error.to_string()))
        }
        Err(_) => Ok(Some(resolved)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    struct StubServer {
        url: String,
        hits: Arc<AtomicUsize>,
        requests: Arc<Mutex<Vec<String>>>,
    }

    fn spawn_stub(status: u16, body: &'static str) -> StubServer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub");
        let url = format!("http://{}", listener.local_addr().expect("stub addr"));
        let hits = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let thread_hits = hits.clone();
        let thread_requests = requests.clone();
        thread::spawn(move || {
            while let Ok((stream, _)) = listener.accept() {
                thread_hits.fetch_add(1, Ordering::SeqCst);
                let mut reader = BufReader::new(stream);
                let mut request = String::new();
                let mut content_length = 0;
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok() {
                    let trimmed = line.trim_end();
                    if trimmed.is_empty() {
                        break;
                    }
                    if let Some(value) = trimmed
                        .to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                    {
                        content_length = value.trim().parse().unwrap_or(0);
                    }
                    request.push_str(trimmed);
                    request.push('\n');
                    line.clear();
                }
                let mut body_bytes = vec![0; content_length];
                if reader.read_exact(&mut body_bytes).is_ok() {
                    request.push_str(&String::from_utf8_lossy(&body_bytes));
                }
                thread_requests.lock().expect("requests lock").push(request);
                let mut stream = reader.into_inner();
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        StubServer {
            url,
            hits,
            requests,
        }
    }

    fn test_dir(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or_default()
        ))
    }

    fn expired_stored_auth(provider: &str, account: &str) -> AuthData {
        let mut auth = AuthData::default();
        auth.insert(
            provider,
            account,
            AuthCredential::OAuth {
                access_token: "stale-access".to_string(),
                refresh_token: Some("stale-refresh".to_string()),
                expires: 1_000,
                account_id: Some("account-id".to_string()),
            },
        );
        auth
    }

    #[test]
    fn token_endpoints_cover_codex_and_claude() {
        assert_eq!(
            token_endpoint("openai-codex"),
            Some((CODEX_TOKEN_URL, CODEX_CLIENT_ID))
        );
        // `openai` has an API key, which nothing refreshes.
        assert_eq!(token_endpoint("openai"), None);
        assert_eq!(
            token_endpoint("anthropic"),
            Some((CLAUDE_TOKEN_URL, CLAUDE_CLIENT_ID))
        );
        assert_eq!(token_endpoint("google"), None);
    }

    #[test]
    fn refresh_request_form_contains_required_fields() {
        let form = refresh_request_form(CODEX_CLIENT_ID, "refresh-123");
        assert!(form.contains(&("grant_type", "refresh_token")));
        assert!(form.contains(&("client_id", CODEX_CLIENT_ID)));
        assert!(form.contains(&("refresh_token", "refresh-123")));
    }

    #[test]
    fn parse_refresh_response_with_expires_in() {
        let tokens = parse_refresh_response(
            r#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}"#,
            "old-refresh",
            1_000_000,
        )
        .expect("parse");
        assert_eq!(tokens.access_token, "new-access");
        assert_eq!(tokens.refresh_token, "new-refresh");
        assert_eq!(tokens.expires, 1_000_000 + 3600);
    }

    #[test]
    fn parse_refresh_response_with_expires_at_seconds_and_millis() {
        let seconds = parse_refresh_response(
            r#"{"access_token":"a","expires_at":4102444800}"#,
            "old",
            1_000,
        )
        .expect("parse seconds");
        assert_eq!(seconds.expires, 4102444800);

        let millis = parse_refresh_response(
            r#"{"access_token":"a","expires_at":4102444800000}"#,
            "old",
            1_000,
        )
        .expect("parse millis");
        assert_eq!(millis.expires, 4102444800);
    }

    #[test]
    fn parse_refresh_response_keeps_previous_refresh_token_when_missing() {
        let tokens = parse_refresh_response(r#"{"access_token":"new-access"}"#, "old-refresh", 0)
            .expect("parse");
        assert_eq!(tokens.refresh_token, "old-refresh");
        assert_eq!(tokens.expires, 0);
    }

    #[test]
    fn parse_refresh_response_rejects_missing_access_token() {
        assert!(parse_refresh_response(r#"{"refresh_token":"x"}"#, "old", 0).is_err());
        assert!(parse_refresh_response("not json", "old", 0).is_err());
    }

    #[test]
    fn needs_refresh_only_when_expiring_with_refresh_token() {
        let base = ResolvedAuth::ChatGptOAuth {
            access_token: "access".to_string(),
            refresh_token: Some("refresh".to_string()),
            expires: 1_000,
            account_id: None,
        };
        assert!(needs_refresh(&base, 941, REFRESH_WINDOW_SECONDS));
        assert!(!needs_refresh(&base, 100, REFRESH_WINDOW_SECONDS));

        let without_refresh = ResolvedAuth::ChatGptOAuth {
            access_token: "access".to_string(),
            refresh_token: None,
            expires: 1_000,
            account_id: None,
        };
        assert!(!needs_refresh(
            &without_refresh,
            1_000,
            REFRESH_WINDOW_SECONDS
        ));

        let unknown_expiry = ResolvedAuth::ChatGptOAuth {
            access_token: "access".to_string(),
            refresh_token: Some("refresh".to_string()),
            expires: 0,
            account_id: None,
        };
        assert!(!needs_refresh(
            &unknown_expiry,
            u64::MAX,
            REFRESH_WINDOW_SECONDS
        ));
    }

    #[tokio::test]
    async fn execute_refresh_posts_form_to_token_endpoint() {
        let stub = spawn_stub(
            200,
            r#"{"access_token":"fresh-access","refresh_token":"fresh-refresh","expires_in":3600}"#,
        );
        let client = reqwest::Client::new();

        let tokens = execute_refresh(&client, &stub.url, CODEX_CLIENT_ID, "stale-refresh", 1000)
            .await
            .expect("refresh");

        assert_eq!(tokens.access_token, "fresh-access");
        assert_eq!(tokens.refresh_token, "fresh-refresh");
        assert_eq!(tokens.expires, 4600);
        let requests = stub.requests.lock().expect("requests lock");
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert!(request.starts_with("POST / HTTP/1.1"), "{request}");
        assert!(request.contains("grant_type=refresh_token"), "{request}");
        assert!(
            request.contains(&format!("client_id={CODEX_CLIENT_ID}")),
            "{request}"
        );
        assert!(request.contains("refresh_token=stale-refresh"), "{request}");
    }

    #[tokio::test]
    async fn execute_refresh_surfaces_endpoint_errors() {
        let stub = spawn_stub(400, r#"{"error":"invalid_grant"}"#);
        let client = reqwest::Client::new();

        let error = execute_refresh(&client, &stub.url, CLAUDE_CLIENT_ID, "stale", 0)
            .await
            .expect_err("refresh should fail");
        assert!(error.to_string().contains("400"), "{error}");
    }

    #[tokio::test]
    async fn transport_failure_retries_fallback_endpoint() {
        let fallback = spawn_stub(
            200,
            r#"{"access_token":"fresh","refresh_token":"rotated","expires_in":3600}"#,
        );
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_millis(300))
            .build()
            .unwrap();
        // Unroutable primary: the request fails before reaching any server.
        let dead_primary = "https://127.0.0.1:9/v1/oauth/token";

        let tokens = execute_refresh_with_fallback(
            &client,
            dead_primary,
            Some(&fallback.url),
            CLAUDE_CLIENT_ID,
            "stale",
            1_000,
        )
        .await
        .expect("fallback refresh succeeds");
        assert_eq!(tokens.access_token, "fresh");
        assert_eq!(tokens.refresh_token, "rotated");
        assert_eq!(tokens.expires, 4_600);
        let requests = fallback.requests.lock().expect("requests lock");
        assert!(requests[0].contains("grant_type=refresh_token"));
    }

    #[tokio::test]
    async fn oauth_denial_does_not_retry_fallback_endpoint() {
        let primary = spawn_stub(400, r#"{"error":"invalid_grant"}"#);
        let fallback = spawn_stub(200, r#"{"access_token":"should-not-happen"}"#);
        let client = reqwest::Client::new();

        let error = execute_refresh_with_fallback(
            &client,
            &primary.url,
            Some(&fallback.url),
            CLAUDE_CLIENT_ID,
            "revoked",
            1_000,
        )
        .await
        .expect_err("oauth denial surfaces");
        assert!(error.to_string().contains("invalid_grant"), "{error}");
        assert_eq!(fallback.hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn refresh_expiring_auth_refreshes_and_persists() {
        let root = test_dir("noel-cli-oauth-refresh");
        let auth_path = root.join("agent").join("auth.json");
        let stub = spawn_stub(
            200,
            r#"{"access_token":"fresh-access","refresh_token":"fresh-refresh","expires_in":3600}"#,
        );
        let auth = expired_stored_auth("openai-codex", "default");
        let client = reqwest::Client::new();

        let resolved = refresh_expiring_auth(
            &client,
            &auth_path,
            &auth,
            "openai-codex",
            None,
            Some((&stub.url, "test-client")),
            1_000,
        )
        .await
        .expect("refresh")
        .expect("auth");

        assert_eq!(
            resolved,
            ResolvedAuth::ChatGptOAuth {
                access_token: "fresh-access".to_string(),
                refresh_token: Some("fresh-refresh".to_string()),
                expires: 1_000 + 3600,
                account_id: Some("account-id".to_string())
            }
        );

        let persisted = read_auth_file(&auth_path).expect("read auth file");
        let credential = persisted
            .credential("openai-codex", "default")
            .expect("persisted credential");
        assert_eq!(
            credential,
            &AuthCredential::OAuth {
                access_token: "fresh-access".to_string(),
                refresh_token: Some("fresh-refresh".to_string()),
                expires: 4600,
                account_id: Some("account-id".to_string()),
            }
        );
        assert_eq!(stub.hits.load(Ordering::SeqCst), 1);

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn refresh_expiring_auth_persists_under_bound_account() {
        let root = test_dir("noel-cli-oauth-bound-account");
        let auth_path = root.join("agent").join("auth.json");
        let stub = spawn_stub(
            200,
            r#"{"access_token":"work-access","refresh_token":"work-refresh","expires_in":3600}"#,
        );
        let auth = expired_stored_auth("openai-codex", "work");
        let client = reqwest::Client::new();

        let resolved = refresh_expiring_auth(
            &client,
            &auth_path,
            &auth,
            "openai-codex",
            Some("work"),
            Some((&stub.url, "test-client")),
            1_000,
        )
        .await
        .expect("refresh")
        .expect("auth");
        assert_eq!(resolved.refresh_token(), Some("work-refresh"));

        let persisted = read_auth_file(&auth_path).expect("read auth file");
        assert!(persisted
            .credential("openai-codex", "work")
            .is_some_and(|credential| {
                matches!(
                    credential,
                    AuthCredential::OAuth { access_token, .. } if access_token == "work-access"
                )
            }));

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn refresh_expiring_auth_collapses_concurrent_refreshes() {
        let root = test_dir("noel-cli-oauth-single-flight");
        let auth_path = root.join("agent").join("auth.json");
        let stub = spawn_stub(
            200,
            r#"{"access_token":"fresh-access","refresh_token":"fresh-refresh","expires_in":3600}"#,
        );
        let auth = expired_stored_auth("openai-codex", "default");
        let client = reqwest::Client::new();

        let (first, second) = tokio::join!(
            refresh_expiring_auth(
                &client,
                &auth_path,
                &auth,
                "openai-codex",
                None,
                Some((&stub.url, "test-client")),
                1_000,
            ),
            refresh_expiring_auth(
                &client,
                &auth_path,
                &auth,
                "openai-codex",
                None,
                Some((&stub.url, "test-client")),
                1_000,
            ),
        );
        first.expect("first refresh").expect("first auth");
        second.expect("second refresh").expect("second auth");

        assert_eq!(stub.hits.load(Ordering::SeqCst), 1);

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn refresh_spends_the_auth_files_rotated_token_not_the_snapshots() {
        // A long-running noel refreshed once: its startup snapshot still holds
        // the spent tokens, the auth file the rotated ones, expiring now.
        let root = test_dir("noel-cli-oauth-rotated");
        let auth_path = root.join("agent").join("auth.json");
        let stub = spawn_stub(
            200,
            r#"{"access_token":"third-access","refresh_token":"third-refresh","expires_in":3600}"#,
        );
        let snapshot = expired_stored_auth("openai-codex", "default");
        let mut stored = AuthData::default();
        stored.insert(
            "openai-codex",
            "default",
            AuthCredential::OAuth {
                access_token: "second-access".to_string(),
                refresh_token: Some("second-refresh".to_string()),
                expires: 5_030,
                account_id: Some("account-id".to_string()),
            },
        );
        std::fs::create_dir_all(auth_path.parent().expect("auth dir")).expect("create auth dir");
        std::fs::write(
            &auth_path,
            serde_json::to_string(&stored).expect("auth json"),
        )
        .expect("write auth");
        let client = reqwest::Client::new();
        let refresh = |now| {
            refresh_expiring_auth(
                &client,
                &auth_path,
                &snapshot,
                "openai-codex",
                None,
                Some((&stub.url, "test-client")),
                now,
            )
        };

        let resolved = refresh(5_000).await.expect("refresh").expect("auth");
        assert_eq!(resolved.refresh_token(), Some("third-refresh"));
        let request = stub.requests.lock().expect("requests lock")[0].clone();
        assert!(
            request.contains("refresh_token=second-refresh"),
            "{request}"
        );

        // The next request from the same snapshot reuses the saved tokens.
        let resolved = refresh(5_100).await.expect("resolve").expect("auth");
        assert_eq!(resolved.expires(), 5_000 + 3600);
        assert_eq!(resolved.refresh_token(), Some("third-refresh"));
        assert_eq!(stub.hits.load(Ordering::SeqCst), 1);

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn refresh_expiring_auth_keeps_valid_tokens() {
        let root = test_dir("noel-cli-oauth-valid");
        let auth_path = root.join("agent").join("auth.json");
        let stub = spawn_stub(200, r#"{"access_token":"unused"}"#);
        let mut auth = expired_stored_auth("openai-codex", "default");
        auth.insert(
            "openai-codex",
            "default",
            AuthCredential::OAuth {
                access_token: "valid-access".to_string(),
                refresh_token: Some("valid-refresh".to_string()),
                expires: 4_000_000_000,
                account_id: None,
            },
        );
        let client = reqwest::Client::new();

        let resolved = refresh_expiring_auth(
            &client,
            &auth_path,
            &auth,
            "openai-codex",
            None,
            Some((&stub.url, "test-client")),
            1_000,
        )
        .await
        .expect("resolve")
        .expect("auth");
        assert_eq!(resolved.expires(), 4_000_000_000);
        assert_eq!(stub.hits.load(Ordering::SeqCst), 0);

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn refresh_window_extends_to_requested_lifetime() {
        let root = test_dir("noel-cli-oauth-window");
        let auth_path = root.join("agent").join("auth.json");
        let stub = spawn_stub(
            200,
            r#"{"access_token":"long-access","refresh_token":"long-refresh","expires_in":7200}"#,
        );
        let mut auth = AuthData::default();
        auth.insert(
            "anthropic",
            "default",
            AuthCredential::OAuth {
                access_token: "ten-minutes-left".to_string(),
                refresh_token: Some("refresh".to_string()),
                expires: 1_600,
                account_id: None,
            },
        );
        let client = reqwest::Client::new();
        let endpoint = Some((stub.url.as_str(), "test-client"));

        let request_path = refresh_expiring_auth(
            &client,
            &auth_path,
            &auth,
            "anthropic",
            None,
            endpoint,
            1_000,
        )
        .await
        .expect("request path")
        .expect("auth");
        assert_eq!(request_path.expires(), 1_600);
        assert_eq!(stub.hits.load(Ordering::SeqCst), 0);

        let extended = refresh_auth_expiring_within(
            &client,
            &auth_path,
            &auth,
            "anthropic",
            None,
            endpoint,
            1_000,
            30 * 60,
        )
        .await
        .expect("windowed refresh")
        .expect("auth");
        assert_eq!(extended.expires(), 1_000 + 7200);
        assert_eq!(stub.hits.load(Ordering::SeqCst), 1);

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn expired_token_without_refresh_token_reports_guidance() {
        let root = test_dir("noel-cli-oauth-no-refresh");
        let auth_path = root.join("agent").join("auth.json");
        let mut auth = AuthData::default();
        auth.insert(
            "anthropic",
            "default",
            AuthCredential::OAuth {
                access_token: "stale-access".to_string(),
                refresh_token: None,
                expires: 1_000,
                account_id: None,
            },
        );
        let client = reqwest::Client::new();

        let error =
            refresh_expiring_auth(&client, &auth_path, &auth, "anthropic", None, None, 5_000)
                .await
                .expect_err("expired token without refresh token should fail");
        let message = error.to_string();
        assert!(message.contains("account default"), "{message}");
        assert!(message.contains("claude login"), "{message}");
        assert!(
            message.contains("noel login anthropic --api-key"),
            "{message}"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn failed_refresh_keeps_unexpired_token() {
        let root = test_dir("noel-cli-oauth-soft-fail");
        let auth_path = root.join("agent").join("auth.json");
        let stub = spawn_stub(500, "server error");
        let mut auth = expired_stored_auth("anthropic", "default");
        auth.insert(
            "anthropic",
            "default",
            AuthCredential::OAuth {
                access_token: "almost-stale".to_string(),
                refresh_token: Some("refresh".to_string()),
                expires: 1_030,
                account_id: None,
            },
        );
        let client = reqwest::Client::new();

        let resolved = refresh_expiring_auth(
            &client,
            &auth_path,
            &auth,
            "anthropic",
            None,
            Some((&stub.url, "test-client")),
            1_000,
        )
        .await
        .expect("soft failure keeps current auth")
        .expect("auth");
        assert_eq!(
            resolved,
            ResolvedAuth::ClaudeCodeOAuth {
                access_token: "almost-stale".to_string(),
                refresh_token: Some("refresh".to_string()),
                expires: 1_030,
            }
        );
        assert_eq!(stub.hits.load(Ordering::SeqCst), 1);

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn refresh_continues_from_the_auth_file_not_the_startup_snapshot() {
        let root = test_dir("noel-cli-oauth-live-refresh-token");
        let auth_path = root.join("agent").join("auth.json");
        let stub = spawn_stub(
            200,
            r#"{"access_token":"fresh-access","refresh_token":"fresh-refresh","expires_in":3600}"#,
        );
        // The startup snapshot still holds a refresh token that another noel
        // process has since rotated; auth.json holds the rotated one (about
        // to expire as well) and an account this process never saw.
        let snapshot = expired_stored_auth("anthropic", "default");
        let mut current = AuthData::default();
        current.insert(
            "anthropic",
            "default",
            AuthCredential::OAuth {
                access_token: "rotated-access".to_string(),
                refresh_token: Some("rotated-refresh".to_string()),
                expires: 1_010,
                account_id: None,
            },
        );
        current.insert(
            "openai",
            "default",
            AuthCredential::ApiKey {
                key: "other-process".to_string(),
            },
        );
        write_auth_file(&auth_path, &current).expect("seed auth file");

        let resolved = refresh_expiring_auth(
            &reqwest::Client::new(),
            &auth_path,
            &snapshot,
            "anthropic",
            None,
            Some((&stub.url, "test-client")),
            1_000,
        )
        .await
        .expect("refresh")
        .expect("auth");

        assert_eq!(resolved.refresh_token(), Some("fresh-refresh"));
        let requests = stub.requests.lock().expect("requests lock").clone();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0].contains("refresh_token=rotated-refresh"),
            "{}",
            requests[0]
        );
        let persisted = read_auth_file(&auth_path).expect("read auth file");
        assert!(matches!(
            persisted.credential("anthropic", "default"),
            Some(AuthCredential::OAuth { access_token, .. }) if access_token == "fresh-access"
        ));
        assert_eq!(
            persisted.credential("openai", "default"),
            Some(&AuthCredential::ApiKey {
                key: "other-process".to_string()
            })
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn refresh_waits_while_another_holder_locks_auth_file() {
        let root = test_dir("noel-cli-oauth-locked");
        let auth_path = root.join("agent").join("auth.json");
        let stub = spawn_stub(200, r#"{"access_token":"fresh-access","expires_in":3600}"#);
        let auth = expired_stored_auth("openai-codex", "default");
        let client = reqwest::Client::new();
        let refresh = || {
            refresh_expiring_auth(
                &client,
                &auth_path,
                &auth,
                "openai-codex",
                None,
                Some((&stub.url, "test-client")),
                1_000,
            )
        };

        let held = AuthFileLock::try_acquire(&auth_path)
            .expect("lock")
            .expect("free lock");
        let blocked = tokio::time::timeout(Duration::from_millis(200), refresh()).await;
        assert!(blocked.is_err(), "refresh ran while auth.json was locked");
        assert_eq!(stub.hits.load(Ordering::SeqCst), 0);

        drop(held);
        refresh().await.expect("refresh").expect("auth");
        assert_eq!(stub.hits.load(Ordering::SeqCst), 1);

        let _ = std::fs::remove_dir_all(root);
    }
}
