use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use pi_config::jwt_exp_claim;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::oauth_refresh::{CLAUDE_CLIENT_ID, CODEX_CLIENT_ID};

pub const CODEX_ISSUER: &str = "https://auth.openai.com";
pub const CLAUDE_AUTHORIZE_URL: &str = "https://platform.claude.com/oauth/authorize";
pub const CLAUDE_MANUAL_REDIRECT_URI: &str = "https://platform.claude.com/oauth/code/callback";

const CLAUDE_SCOPES: [&str; 5] = [
    "org:create_api_key",
    "user:profile",
    "user:inference",
    "user:sessions:claude_code",
    "user:mcp_servers",
];

pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCode {
    pub verification_url: String,
    pub user_code: String,
    pub device_auth_id: String,
    pub interval: u64,
}

#[derive(Debug, Deserialize)]
struct UserCodeResp {
    device_auth_id: String,
    #[serde(alias = "usercode")]
    user_code: String,
    #[serde(default)]
    interval: Option<Value>,
}

impl UserCodeResp {
    fn interval_seconds(&self) -> u64 {
        match &self.interval {
            Some(Value::String(text)) => text.trim().parse().unwrap_or(5),
            Some(Value::Number(number)) => number.as_u64().unwrap_or(5),
            _ => 5,
        }
        .max(1)
    }
}

#[derive(Debug, Deserialize)]
pub struct CodeSuccessResp {
    authorization_code: String,
    code_verifier: String,
}

#[derive(Debug, Deserialize)]
pub struct ExchangedTokens {
    #[serde(default)]
    id_token: Option<String>,
    access_token: String,
    refresh_token: String,
}

pub async fn request_device_code(
    client: &reqwest::Client,
    base_url: &str,
    client_id: &str,
) -> Result<DeviceCode> {
    let response = client
        .post(format!("{base_url}/api/accounts/deviceauth/usercode"))
        .json(&serde_json::json!({ "client_id": client_id }))
        .send()
        .await
        .context("device code request failed")?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!(
            "device code request failed with status {status}: {body}"
        ));
    }
    let parsed = serde_json::from_str::<UserCodeResp>(&body)?;
    let interval = parsed.interval_seconds();
    Ok(DeviceCode {
        verification_url: format!("{base_url}/codex/device"),
        user_code: parsed.user_code,
        device_auth_id: parsed.device_auth_id,
        interval,
    })
}

pub async fn poll_device_token(
    client: &reqwest::Client,
    base_url: &str,
    device: &DeviceCode,
    timeout: Duration,
) -> Result<CodeSuccessResp> {
    let start = Instant::now();
    loop {
        let response = client
            .post(format!("{base_url}/api/accounts/deviceauth/token"))
            .json(&serde_json::json!({
                "device_auth_id": device.device_auth_id,
                "user_code": device.user_code,
            }))
            .send()
            .await
            .context("device token poll failed")?;
        let status = response.status();
        if status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return serde_json::from_str::<CodeSuccessResp>(&body)
                .map_err(|error| anyhow!("invalid device token response: {error}"));
        }
        if status != reqwest::StatusCode::FORBIDDEN && status != reqwest::StatusCode::NOT_FOUND {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("device auth failed with status {status}: {body}"));
        }
        if start.elapsed() >= timeout {
            return Err(anyhow!("device auth timed out after 15 minutes"));
        }
        tokio::time::sleep(Duration::from_secs(device.interval)).await;
    }
}

pub async fn exchange_authorization_code(
    client: &reqwest::Client,
    token_url: &str,
    client_id: &str,
    redirect_uri: &str,
    code: &str,
    code_verifier: &str,
) -> Result<ExchangedTokens> {
    let response = client
        .post(token_url)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", client_id),
            ("code_verifier", code_verifier),
        ])
        .send()
        .await
        .context("authorization code exchange failed")?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!(
            "device code exchange failed with status {status}: {body}"
        ));
    }
    Ok(serde_json::from_str::<ExchangedTokens>(&body)?)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

fn random_urlsafe(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    getrandom::getrandom(&mut buffer).expect("system randomness unavailable");
    URL_SAFE_NO_PAD.encode(buffer)
}

pub fn s256_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

pub fn generate_pkce() -> Pkce {
    let verifier = random_urlsafe(32);
    let challenge = s256_challenge(&verifier);
    Pkce {
        verifier,
        challenge,
    }
}

pub fn generate_state() -> String {
    random_urlsafe(32)
}

fn percent_encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

pub fn claude_authorize_url(
    authorize_url: &str,
    client_id: &str,
    redirect_uri: &str,
    code_challenge: &str,
    state: &str,
) -> String {
    let scope = CLAUDE_SCOPES.join(" ");
    format!(
        "{authorize_url}?code=true&client_id={}&response_type=code&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}",
        percent_encode(client_id),
        percent_encode(redirect_uri),
        percent_encode(&scope),
        percent_encode(code_challenge),
        percent_encode(state),
    )
}

pub fn parse_pasted_code(input: &str, expected_state: &str) -> Result<String> {
    let (code, state) = input.trim().split_once('#').ok_or_else(|| {
        anyhow!("invalid code: paste the full value shown after signing in (code#state)")
    })?;
    if code.trim().is_empty() || state.trim().is_empty() {
        return Err(anyhow!(
            "invalid code: paste the full value shown after signing in (code#state)"
        ));
    }
    if state.trim() != expected_state {
        return Err(anyhow!(
            "pasted code does not match this login session (state mismatch); restart login and copy the full code"
        ));
    }
    Ok(code.trim().to_string())
}

#[derive(Debug, Deserialize)]
pub struct ClaudeTokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

pub async fn claude_exchange_code(
    client: &reqwest::Client,
    token_url: &str,
    client_id: &str,
    redirect_uri: &str,
    code: &str,
    code_verifier: &str,
    state: &str,
) -> Result<ClaudeTokenResponse> {
    let response = client
        .post(token_url)
        .json(&serde_json::json!({
            "grant_type": "authorization_code",
            "code": code,
            "redirect_uri": redirect_uri,
            "client_id": client_id,
            "code_verifier": code_verifier,
            "state": state,
        }))
        .send()
        .await
        .context("claude token exchange failed")?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!(
            "token exchange failed with status {status}: {body}"
        ));
    }
    Ok(serde_json::from_str::<ClaudeTokenResponse>(&body)?)
}

pub fn chatgpt_account_id(token: &str) -> Option<String> {
    let parts = token.split('.').collect::<Vec<_>>();
    if parts.len() != 3 {
        return None;
    }
    let payload = URL_SAFE_NO_PAD.decode(parts[1]).ok()?;
    let payload = serde_json::from_slice::<Value>(&payload).ok()?;
    payload
        .pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
}

pub struct LoginTokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires: u64,
    pub account_id: Option<String>,
}

pub async fn run_codex_device_login<F>(
    client: &reqwest::Client,
    base_url: &str,
    token_url: &str,
    on_code: F,
) -> Result<LoginTokens>
where
    F: FnOnce(&DeviceCode),
{
    let device = request_device_code(client, base_url, CODEX_CLIENT_ID).await?;
    on_code(&device);
    let success = tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            return Err(anyhow!("login cancelled"));
        }
        result = poll_device_token(client, base_url, &device, LOGIN_TIMEOUT) => result?,
    };
    let redirect_uri = format!("{base_url}/deviceauth/callback");
    let tokens = exchange_authorization_code(
        client,
        token_url,
        CODEX_CLIENT_ID,
        &redirect_uri,
        &success.authorization_code,
        &success.code_verifier,
    )
    .await?;
    let expires = jwt_exp_claim(&tokens.access_token).unwrap_or(0);
    let account_id = chatgpt_account_id(&tokens.access_token)
        .or_else(|| tokens.id_token.as_deref().and_then(chatgpt_account_id));
    Ok(LoginTokens {
        access_token: tokens.access_token,
        refresh_token: Some(tokens.refresh_token),
        expires,
        account_id,
    })
}

pub async fn run_claude_pkce_login<F>(
    client: &reqwest::Client,
    authorize_url: &str,
    token_url: &str,
    read_code: F,
) -> Result<LoginTokens>
where
    F: FnOnce(&str) -> Result<String>,
{
    let pkce = generate_pkce();
    let state = generate_state();
    let url = claude_authorize_url(
        authorize_url,
        CLAUDE_CLIENT_ID,
        CLAUDE_MANUAL_REDIRECT_URI,
        &pkce.challenge,
        &state,
    );
    let pasted = read_code(&url)?;
    let code = parse_pasted_code(&pasted, &state)?;
    let tokens = claude_exchange_code(
        client,
        token_url,
        CLAUDE_CLIENT_ID,
        CLAUDE_MANUAL_REDIRECT_URI,
        &code,
        &pkce.verifier,
        &state,
    )
    .await?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    Ok(LoginTokens {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires: tokens
            .expires_in
            .map(|seconds| now.saturating_add(seconds))
            .unwrap_or(0),
        account_id: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_config::{AuthCredential, AuthData, DEFAULT_ACCOUNT_NAME};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;

    struct StubServer {
        url: String,
        hits: Arc<AtomicUsize>,
        requests: Arc<Mutex<Vec<String>>>,
    }

    fn spawn_stub(responses: Vec<(u16, &'static str)>) -> StubServer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub");
        let url = format!("http://{}", listener.local_addr().expect("stub addr"));
        let hits = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let responses = Arc::new(Mutex::new(
            responses
                .into_iter()
                .map(|(status, body)| (status, body.to_string()))
                .collect::<Vec<_>>(),
        ));
        let server = StubServer {
            url,
            hits: hits.clone(),
            requests: requests.clone(),
        };
        thread::spawn(move || {
            while let Ok((stream, _)) = listener.accept() {
                hits.fetch_add(1, Ordering::SeqCst);
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
                requests.lock().expect("requests lock").push(request);
                let (status, body) = {
                    let mut pending = responses.lock().expect("responses lock");
                    if pending.len() > 1 {
                        pending.remove(0)
                    } else {
                        pending[0].clone()
                    }
                };
                let mut stream = reader.into_inner();
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        server
    }

    fn fake_jwt(payload: &str) -> String {
        let encode = |json: &str| URL_SAFE_NO_PAD.encode(json.as_bytes());
        format!(
            "{}.{}.signature",
            encode(r#"{"alg":"none"}"#),
            encode(payload)
        )
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

    #[tokio::test]
    async fn device_code_response_parses_usercode_alias_and_interval() {
        let stub = spawn_stub(vec![(
            200,
            r#"{"device_auth_id":"dev-1","usercode":"ABCD-EFGH","interval":"2"}"#,
        )]);
        let client = reqwest::Client::new();
        let device = request_device_code(&client, &stub.url, CODEX_CLIENT_ID)
            .await
            .expect("device code");
        assert_eq!(device.device_auth_id, "dev-1");
        assert_eq!(device.user_code, "ABCD-EFGH");
        assert_eq!(device.interval, 2);
        assert_eq!(
            device.verification_url,
            format!("{}/codex/device", stub.url)
        );
        let requests = stub.requests.lock().expect("requests lock");
        assert!(
            requests[0].contains("/api/accounts/deviceauth/usercode"),
            "{}",
            requests[0]
        );
        assert!(
            requests[0].contains(&format!("\"client_id\":\"{CODEX_CLIENT_ID}\"")),
            "{}",
            requests[0]
        );
    }

    #[tokio::test]
    async fn device_flow_pending_then_success_then_exchange() {
        let access_token = fake_jwt(
            r#"{"exp":4102444800,"https://api.openai.com/auth":{"chatgpt_account_id":"acct-123"}}"#,
        );
        let success_body = format!(
            r#"{{"id_token":"id","access_token":"{access_token}","refresh_token":"refresh-1"}}"#
        );
        let stub = spawn_stub(vec![
            (403, r#"{"error":"authorization_pending"}"#),
            (403, r#"{"error":"slow_down"}"#),
            (
                200,
                r#"{"authorization_code":"code-1","code_challenge":"ch","code_verifier":"ver-1"}"#,
            ),
            (200, Box::leak(success_body.into_boxed_str())),
        ]);
        let client = reqwest::Client::new();
        let device = DeviceCode {
            verification_url: format!("{}/codex/device", stub.url),
            user_code: "ABCD".to_string(),
            device_auth_id: "dev-1".to_string(),
            interval: 1,
        };
        let success = poll_device_token(&client, &stub.url, &device, Duration::from_secs(10))
            .await
            .expect("poll success");
        assert_eq!(success.authorization_code, "code-1");
        assert_eq!(success.code_verifier, "ver-1");
        assert_eq!(stub.hits.load(Ordering::SeqCst), 3);

        let redirect_uri = format!("{}/deviceauth/callback", stub.url);
        let tokens = exchange_authorization_code(
            &client,
            &stub.url,
            CODEX_CLIENT_ID,
            &redirect_uri,
            &success.authorization_code,
            &success.code_verifier,
        )
        .await
        .expect("exchange");
        assert_eq!(tokens.access_token, access_token);
        assert_eq!(jwt_exp_claim(&tokens.access_token), Some(4102444800));
        assert_eq!(
            chatgpt_account_id(&tokens.access_token).as_deref(),
            Some("acct-123")
        );

        let requests = stub.requests.lock().expect("requests lock");
        let exchange_request = &requests[3];
        assert!(
            exchange_request.contains("grant_type=authorization_code"),
            "{exchange_request}"
        );
        assert!(
            exchange_request.contains("code_verifier=ver-1"),
            "{exchange_request}"
        );
        assert!(
            exchange_request.contains("redirect_uri="),
            "{exchange_request}"
        );
    }

    #[tokio::test]
    async fn device_flow_times_out_when_only_pending() {
        let stub = spawn_stub(vec![(403, r#"{"error":"authorization_pending"}"#)]);
        let client = reqwest::Client::new();
        let device = DeviceCode {
            verification_url: String::new(),
            user_code: "ABCD".to_string(),
            device_auth_id: "dev-1".to_string(),
            interval: 1,
        };
        let error = poll_device_token(&client, &stub.url, &device, Duration::from_millis(1500))
            .await
            .expect_err("should time out");
        assert!(error.to_string().contains("timed out"), "{error}");
    }

    #[tokio::test]
    async fn device_flow_surfaces_endpoint_errors() {
        let stub = spawn_stub(vec![(500, "server error")]);
        let client = reqwest::Client::new();
        let device = DeviceCode {
            verification_url: String::new(),
            user_code: "ABCD".to_string(),
            device_auth_id: "dev-1".to_string(),
            interval: 1,
        };
        let error = poll_device_token(&client, &stub.url, &device, Duration::from_secs(10))
            .await
            .expect_err("should fail");
        assert!(error.to_string().contains("500"), "{error}");
    }

    #[test]
    fn pkce_s256_matches_rfc7636_test_vector() {
        assert_eq!(
            s256_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        let pkce = generate_pkce();
        assert_eq!(pkce.verifier.len(), 43);
        assert_eq!(pkce.challenge, s256_challenge(&pkce.verifier));
        assert_ne!(generate_state(), generate_state());
    }

    #[test]
    fn claude_authorize_url_contains_official_parameters() {
        let url = claude_authorize_url(
            CLAUDE_AUTHORIZE_URL,
            CLAUDE_CLIENT_ID,
            CLAUDE_MANUAL_REDIRECT_URI,
            "challenge",
            "state-1",
        );
        assert!(
            url.starts_with("https://platform.claude.com/oauth/authorize?"),
            "{url}"
        );
        assert!(url.contains("code=true"), "{url}");
        assert!(
            url.contains(&format!("client_id={CLAUDE_CLIENT_ID}")),
            "{url}"
        );
        assert!(url.contains("response_type=code"), "{url}");
        assert!(
            url.contains(
                "redirect_uri=https%3A%2F%2Fplatform.claude.com%2Foauth%2Fcode%2Fcallback"
            ),
            "{url}"
        );
        assert!(url.contains("scope=org%3Acreate_api_key%20user%3Aprofile%20user%3Ainference%20user%3Asessions%3Aclaude_code%20user%3Amcp_servers"), "{url}");
        assert!(url.contains("code_challenge=challenge"), "{url}");
        assert!(url.contains("code_challenge_method=S256"), "{url}");
        assert!(url.contains("state=state-1"), "{url}");
    }

    #[test]
    fn parse_pasted_code_splits_and_verifies_state() {
        assert_eq!(
            parse_pasted_code("code123#state456", "state456").expect("parse"),
            "code123"
        );
        assert!(parse_pasted_code("code123", "state456").is_err());
        assert!(parse_pasted_code("#state456", "state456").is_err());
        let mismatch = parse_pasted_code("code123#other", "state456").expect_err("mismatch");
        assert!(
            mismatch.to_string().contains("state mismatch"),
            "{mismatch}"
        );
    }

    #[tokio::test]
    async fn claude_exchange_posts_json_with_verifier_and_state() {
        let stub = spawn_stub(vec![(
            200,
            r#"{"access_token":"claude-access","refresh_token":"claude-refresh","expires_in":3600}"#,
        )]);
        let client = reqwest::Client::new();
        let tokens = claude_exchange_code(
            &client,
            &stub.url,
            CLAUDE_CLIENT_ID,
            CLAUDE_MANUAL_REDIRECT_URI,
            "code-1",
            "verifier-1",
            "state-1",
        )
        .await
        .expect("exchange");
        assert_eq!(tokens.access_token, "claude-access");
        assert_eq!(tokens.refresh_token.as_deref(), Some("claude-refresh"));
        assert_eq!(tokens.expires_in, Some(3600));
        let requests = stub.requests.lock().expect("requests lock");
        let request = &requests[0];
        assert!(
            request.contains("\"code_verifier\":\"verifier-1\""),
            "{request}"
        );
        assert!(request.contains("\"state\":\"state-1\""), "{request}");
        assert!(
            request
                .contains("\"redirect_uri\":\"https://platform.claude.com/oauth/code/callback\""),
            "{request}"
        );
        assert!(
            request.contains("\"grant_type\":\"authorization_code\""),
            "{request}"
        );
    }

    #[tokio::test]
    async fn codex_login_stores_oauth_credential_with_jwt_metadata() {
        let root = test_dir("pi-cli-oauth-login-store");
        let auth_path = root.join("auth.json");
        let access_token = fake_jwt(
            r#"{"exp":4102444800,"https://api.openai.com/auth":{"chatgpt_account_id":"acct-xyz"}}"#,
        );
        let success_body = format!(
            r#"{{"id_token":"id","access_token":"{access_token}","refresh_token":"refresh-1"}}"#
        );
        let stub = spawn_stub(vec![
            (
                200,
                r#"{"device_auth_id":"dev-1","user_code":"ABCD","interval":1}"#,
            ),
            (
                200,
                r#"{"authorization_code":"code-1","code_challenge":"ch","code_verifier":"ver-1"}"#,
            ),
            (200, Box::leak(success_body.into_boxed_str())),
        ]);
        let client = reqwest::Client::new();

        let tokens = run_codex_device_login(&client, &stub.url, &stub.url, |_| {})
            .await
            .expect("login");
        assert_eq!(tokens.expires, 4102444800);
        assert_eq!(tokens.account_id.as_deref(), Some("acct-xyz"));

        let mut auth = AuthData::default();
        auth.insert(
            "openai-codex",
            DEFAULT_ACCOUNT_NAME,
            AuthCredential::OAuth {
                access_token: tokens.access_token.clone(),
                refresh_token: tokens.refresh_token.clone(),
                expires: tokens.expires,
                account_id: tokens.account_id.clone(),
            },
        );
        let serialized = serde_json::to_string(&auth).expect("serialize");
        let reloaded = serde_json::from_str::<AuthData>(&serialized).expect("reload");
        let credential = reloaded
            .credential("openai-codex", DEFAULT_ACCOUNT_NAME)
            .expect("credential");
        assert!(matches!(
            credential,
            AuthCredential::OAuth { expires: 4102444800, account_id: Some(id), .. } if id == "acct-xyz"
        ));

        let _ = std::fs::remove_dir_all(root);
        let _ = auth_path;
    }
}
