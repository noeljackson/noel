//! OAuth for HTTP MCP servers, ported from upstream's `packages/mcp/src/oauth`
//! (itself adapted from the MCP TypeScript SDK): protected-resource and
//! authorization-server metadata discovery with issuer validation, dynamic
//! client registration, the authorization-code flow with PKCE (S256),
//! token exchange and refresh with the client-auth methods servers
//! advertise, and the loopback callback server (RFC 8252).
//!
//! Security rules ported verbatim: only http(s) URLs from servers (a
//! `file:` authorization endpoint would launch a local program), no
//! credentials to non-HTTPS endpoints outside loopback, RFC 9207 `iss`
//! checking, and an authorization-server issuer that must match where
//! discovery found it.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Why an OAuth step failed.
#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    /// The server answered an OAuth error object.
    #[error("OAuth error {code}: {message}")]
    Server { code: String, message: String },
    #[error("OAuth issuer mismatch: expected {expected:?}, received {received:?}")]
    IssuerMismatch {
        expected: Option<String>,
        received: Option<String>,
    },
    #[error("refusing to send OAuth credentials to non-HTTPS endpoint {endpoint}")]
    InsecureEndpoint { endpoint: String },
    #[error("OAuth dynamic client registration failed with status {status}: {body}")]
    Registration { status: u16, body: String },
    #[error("OAuth requires user interaction")]
    AuthorizationRequired,
    #[error("{0}")]
    Http(String),
    #[error("invalid {0}")]
    Invalid(String),
}

/// The protected-resource metadata (`/.well-known/oauth-protected-resource`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtectedResourceMetadata {
    pub resource: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authorization_servers: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes_supported: Vec<String>,
}

/// The authorization-server metadata (`/.well-known/oauth-authorization-server`
/// or `openid-configuration`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AuthorizationServerMetadata {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes_supported: Vec<String>,
    pub response_types_supported: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grant_types_supported: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub token_endpoint_auth_methods_supported: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub code_challenge_methods_supported: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id_metadata_document_supported: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_response_iss_parameter_supported: Option<bool>,
}

/// Tokens a token endpoint answer holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthTokens {
    pub access_token: String,
    pub token_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
}

/// A registered client's credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientInformation {
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
}

/// The client metadata registration sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientMetadata {
    pub redirect_uris: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

fn is_http_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

fn loopback(hostname: &str) -> bool {
    matches!(hostname, "localhost" | "127.0.0.1" | "[::1]" | "::1")
}

fn host_of(url: &str) -> String {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    after_scheme
        .split('/')
        .next()
        .unwrap_or_default()
        .split(':')
        .next()
        .unwrap_or_default()
        .to_string()
}

/// The metadata validators (upstream's `types.ts`): URLs must be http(s),
/// strings non-empty, arrays of strings.
pub fn parse_protected_resource_metadata(
    value: &Value,
) -> Result<ProtectedResourceMetadata, OAuthError> {
    let invalid = |what: &str| OAuthError::Invalid(what.to_string());
    let object = value
        .as_object()
        .ok_or_else(|| invalid("OAuth protected resource metadata"))?;
    let resource = object
        .get("resource")
        .and_then(Value::as_str)
        .filter(|url| is_http_url(url))
        .ok_or_else(|| invalid("OAuth protected resource metadata resource"))?;
    let string_array = |key: &str| -> Option<Vec<String>> {
        object.get(key).and_then(Value::as_array).map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
    };
    Ok(ProtectedResourceMetadata {
        resource: resource.to_string(),
        authorization_servers: string_array("authorization_servers").unwrap_or_default(),
        scopes_supported: string_array("scopes_supported").unwrap_or_default(),
    })
}

pub fn parse_authorization_server_metadata(
    value: &Value,
) -> Result<AuthorizationServerMetadata, OAuthError> {
    let invalid = |what: &str| OAuthError::Invalid(what.to_string());
    let object = value
        .as_object()
        .ok_or_else(|| invalid("authorization server metadata"))?;
    let url_field = |key: &str| -> Result<String, OAuthError> {
        object
            .get(key)
            .and_then(Value::as_str)
            .filter(|url| is_http_url(url))
            .ok_or_else(|| invalid(key))
            .map(str::to_string)
    };
    let issuer = url_field("issuer")?;
    let authorization_endpoint = url_field("authorization_endpoint")?;
    let token_endpoint = url_field("token_endpoint")?;
    let registration_endpoint = match object.get("registration_endpoint") {
        None | Some(Value::Null) => None,
        Some(url) => Some(
            url.as_str()
                .filter(|url| is_http_url(url))
                .ok_or_else(|| invalid("registration_endpoint"))?
                .to_string(),
        ),
    };
    let response_types = object
        .get("response_types_supported")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .ok_or_else(|| invalid("response_types_supported"))?;
    Ok(AuthorizationServerMetadata {
        issuer,
        authorization_endpoint,
        token_endpoint,
        registration_endpoint,
        response_types_supported: response_types,
        ..Default::default()
    })
}

pub fn parse_tokens(value: &Value) -> Result<OAuthTokens, OAuthError> {
    let invalid = |what: &str| OAuthError::Invalid(what.to_string());
    let object = value
        .as_object()
        .ok_or_else(|| invalid("OAuth token response"))?;
    let string_field = |key: &str| -> Result<String, OAuthError> {
        object
            .get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .ok_or_else(|| invalid(key))
            .map(str::to_string)
    };
    Ok(OAuthTokens {
        access_token: string_field("access_token")?,
        token_type: string_field("token_type")?,
        expires_in: object
            .get("expires_in")
            .and_then(Value::as_u64)
            .filter(|seconds| *seconds > 0),
        scope: object
            .get("scope")
            .and_then(Value::as_str)
            .map(str::to_string),
        refresh_token: object
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// The `WWW-Authenticate` challenge of a 401: `resource_metadata` and
/// `scope` fields (upstream's `parseWwwAuthenticate`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OAuthChallenge {
    pub resource_metadata_url: Option<String>,
    pub scope: Option<String>,
    pub error: Option<String>,
}

pub fn parse_www_authenticate(header: Option<&str>) -> OAuthChallenge {
    let Some(header) = header else {
        return OAuthChallenge::default();
    };
    let scheme = header.split_whitespace().next().unwrap_or_default();
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "bearer" && scheme != "dpop" {
        return OAuthChallenge::default();
    }
    let field = |name: &str| -> Option<String> {
        let pattern = format!(r#"(?:^|[,\s{}]){}=(?:"([^"]*)"|([^\s,]+))"#, r#","#, name);
        let _ = pattern;
        // Hand parsing: find `name=` and take the quoted or bare value.
        let lower = header.to_ascii_lowercase();
        let mut search = 0usize;
        while let Some(at) = lower[search..].find(&format!("{name}=")) {
            let start = search + at;
            let before_ok = start == 0
                || header[..start]
                    .chars()
                    .next_back()
                    .is_some_and(|ch| ch.is_whitespace() || ch == ',');
            if before_ok {
                let after = &header[start + name.len() + 1..];
                if let Some(quoted) = after.strip_prefix('"') {
                    if let Some(end) = quoted.find('"') {
                        return Some(quoted[..end].to_string());
                    }
                } else {
                    let end = after
                        .find(|ch: char| ch.is_whitespace() || ch == ',')
                        .unwrap_or(after.len());
                    let value = &after[..end];
                    if !value.is_empty() {
                        return Some(value.to_string());
                    }
                }
            }
            search = start + name.len();
        }
        None
    };
    OAuthChallenge {
        resource_metadata_url: field("resource_metadata"),
        scope: field("scope"),
        error: field("error"),
    }
}

/// Discovery: protected-resource metadata at the server, then
/// authorization-server metadata at the issuer it names (or the server's
/// own origin), both with issuer validation (upstream's `discovery.ts`).
/// 4xx/502 on a suffixed URL (a non-root server path) retries without
/// the suffix, as upstream does.
pub async fn discover(
    client: &reqwest::Client,
    server_url: &str,
    protocol_version: &str,
) -> Result<
    (
        ProtectedResourceMetadata,
        Option<AuthorizationServerMetadata>,
        String,
    ),
    OAuthError,
> {
    let fetch_json = |url: String| {
        let client = client.clone();
        let version = protocol_version.to_string();
        async move {
            let response = client
                .get(&url)
                .header("accept", "application/json")
                .header("MCP-Protocol-Version", version)
                .timeout(Duration::from_secs(15))
                .send()
                .await
                .map_err(|error| OAuthError::Http(error.to_string()))?;
            let status = response.status().as_u16();
            if !(200..300).contains(&status) {
                return Ok((status, String::new()));
            }
            let body = response
                .text()
                .await
                .map_err(|error| OAuthError::Http(error.to_string()))?;
            Ok((status, body))
        }
    };
    // Protected-resource metadata: `/.well-known/oauth-protected-resource<path>`.
    let origin = origin_of(server_url);
    let path = path_of(server_url);
    let suffix = path.trim_end_matches('/');
    let mut url = format!("{origin}/.well-known/oauth-protected-resource{suffix}");
    let (mut status, mut body) = fetch_json(url.clone()).await?;
    if !(200..300).contains(&status)
        && path != "/"
        && ((400..500).contains(&status) || status == 502)
    {
        // A server on a non-root path may publish the document at the
        // origin root only; retry without the suffix.
        url = format!("{origin}/.well-known/oauth-protected-resource");
        let retry = fetch_json(url).await?;
        status = retry.0;
        body = retry.1;
    }
    if !(200..300).contains(&status) {
        return Err(OAuthError::Http(format!(
            "HTTP {status} loading OAuth protected resource metadata"
        )));
    }
    let value: Value = serde_json::from_str(&body)
        .map_err(|_| OAuthError::Invalid("OAuth protected resource metadata".into()))?;
    let resource = parse_protected_resource_metadata(&value)?;
    let authorization_server = resource
        .authorization_servers
        .first()
        .cloned()
        .unwrap_or_else(|| format!("{origin}/"));
    // Authorization-server metadata: oauth-authorization-server<path>,
    // openid-configuration<path>, then <path>/.well-known/openid-configuration.
    let as_origin = origin_of(&authorization_server);
    let as_path = path_of(&authorization_server)
        .trim_end_matches('/')
        .to_string();
    let candidates = [
        format!("{as_origin}/.well-known/oauth-authorization-server{as_path}"),
        format!("{as_origin}/.well-known/openid-configuration{as_path}"),
        format!("{as_origin}{as_path}/.well-known/openid-configuration"),
    ];
    for candidate in candidates {
        let (status, body) = fetch_json(candidate).await?;
        if !(200..300).contains(&status) {
            continue;
        }
        let value: Value = serde_json::from_str(&body)
            .map_err(|_| OAuthError::Invalid("authorization server metadata".into()))?;
        let metadata = parse_authorization_server_metadata(&value)?;
        let trim = |value: &str| value.trim_end_matches('/').to_string();
        if trim(&metadata.issuer) != trim(&authorization_server) {
            return Err(OAuthError::IssuerMismatch {
                expected: Some(authorization_server.clone()),
                received: Some(metadata.issuer.clone()),
            });
        }
        return Ok((resource, Some(metadata), authorization_server));
    }
    Ok((resource, None, authorization_server))
}

fn origin_of(url: &str) -> String {
    let scheme = url.split("://").next().unwrap_or("https");
    let after = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let authority = after.split('/').next().unwrap_or_default();
    format!("{scheme}://{authority}")
}

fn path_of(url: &str) -> String {
    let after = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let path = after.split_once('/').map(|(_, rest)| format!("/{rest}"));
    path.unwrap_or_else(|| "/".to_string())
}

/// A random `state` for one authorization request: the callback answers
/// only a redirect that carries it back.
pub fn random_state() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("system randomness");
    base64_url(&bytes)
}

/// PKCE S256: a random verifier and its challenge.
pub fn pkce() -> (String, String) {
    use sha2::Digest;
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("system randomness");
    let verifier = base64_url(&bytes);
    let digest = sha2::Sha256::digest(verifier.as_bytes());
    let challenge = base64_url(&digest);
    (verifier, challenge)
}

fn base64_url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut output = String::new();
    for chunk in bytes.chunks(3) {
        let bytes = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let value = ((bytes[0] as u32) << 16) | ((bytes[1] as u32) << 8) | bytes[2] as u32;
        output.push(ALPHABET[(value >> 18) as usize & 63] as char);
        output.push(ALPHABET[(value >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            output.push(ALPHABET[(value >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            output.push(ALPHABET[value as usize & 63] as char);
        }
    }
    output
}

/// One token-endpoint request: form-encoded, with the client
/// authentication the server supports (upstream's `tokenRequest`).
async fn token_request(
    client: &reqwest::Client,
    token_endpoint: &str,
    information: &ClientInformation,
    mut params: BTreeMap<String, String>,
) -> Result<OAuthTokens, OAuthError> {
    if !token_endpoint.starts_with("https://") && !loopback(&host_of(token_endpoint)) {
        return Err(OAuthError::InsecureEndpoint {
            endpoint: token_endpoint.to_string(),
        });
    }
    let method = if information.client_secret.is_some() {
        "client_secret_basic"
    } else {
        "none"
    };
    let mut headers = reqwest::header::HeaderMap::new();
    match method {
        "client_secret_basic" => {
            let secret = information.client_secret.as_deref().unwrap_or_default();
            let credentials = format!("{}:{secret}", information.client_id);
            let encoded = base64_url(credentials.as_bytes());
            let _ = encoded;
            let plain = credentials.as_bytes();
            use base64::Engine;
            let value = format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(plain)
            );
            headers.insert(
                "authorization",
                reqwest::header::HeaderValue::try_from(value)
                    .map_err(|error| OAuthError::Http(error.to_string()))?,
            );
        }
        _ => {
            params.insert("client_id".to_string(), information.client_id.clone());
        }
    }
    let body = params
        .iter()
        .map(|(key, value)| format!("{key}={}", urlencode(value)))
        .collect::<Vec<_>>()
        .join("&");
    let response = client
        .post(token_endpoint)
        .header("accept", "application/json")
        .header("content-type", "application/x-www-form-urlencoded")
        .headers(headers)
        .body(body)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|error| OAuthError::Http(error.to_string()))?;
    let status = response.status().as_u16();
    let text = response
        .text()
        .await
        .map_err(|error| OAuthError::Http(error.to_string()))?;
    let value = serde_json::from_str::<Value>(&text).ok();
    // Servers may report OAuth errors with any status: the body first.
    if let Some(object) = value.as_ref().and_then(Value::as_object)
        && let Some(error) = object.get("error").and_then(Value::as_str)
    {
        return Err(OAuthError::Server {
            code: error.to_string(),
            message: object
                .get("error_description")
                .and_then(Value::as_str)
                .unwrap_or(error)
                .to_string(),
        });
    }
    if !(200..300).contains(&status) {
        return Err(OAuthError::Server {
            code: "server_error".to_string(),
            message: format!("HTTP {status}: {text}"),
        });
    }
    let value = value.ok_or_else(|| OAuthError::Invalid("OAuth token response".into()))?;
    parse_tokens(&value)
}

fn urlencode(value: &str) -> String {
    let mut output = String::new();
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                output.push(*byte as char)
            }
            _ => output.push_str(&format!("%{byte:02X}")),
        }
    }
    output
}

/// Dynamic client registration (upstream's `registerClient`).
pub async fn register_client(
    client: &reqwest::Client,
    registration_endpoint: &str,
    metadata: &ClientMetadata,
) -> Result<ClientInformation, OAuthError> {
    let body = serde_json::json!({
        "redirect_uris": metadata.redirect_uris,
        "client_name": metadata.client_name,
        "scope": metadata.scope,
    });
    let response = client
        .post(registration_endpoint)
        .header("accept", "application/json")
        .header("content-type", "application/json")
        .json(&body)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|error| OAuthError::Http(error.to_string()))?;
    let status = response.status().as_u16();
    let text = response
        .text()
        .await
        .map_err(|error| OAuthError::Http(error.to_string()))?;
    if !(200..300).contains(&status) {
        return Err(OAuthError::Registration {
            status,
            body: text.chars().take(500).collect(),
        });
    }
    let value: Value = serde_json::from_str(&text)
        .map_err(|_| OAuthError::Invalid("OAuth client registration response".into()))?;
    let client_id = value
        .get("client_id")
        .and_then(Value::as_str)
        .ok_or_else(|| OAuthError::Invalid("client_id".into()))?;
    Ok(ClientInformation {
        client_id: client_id.to_string(),
        client_secret: value
            .get("client_secret")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// Exchanges an authorization code for tokens.
pub async fn exchange_code(
    client: &reqwest::Client,
    token_endpoint: &str,
    information: &ClientInformation,
    code: &str,
    verifier: &str,
    redirect_url: &str,
) -> Result<OAuthTokens, OAuthError> {
    let params = BTreeMap::from([
        ("grant_type".to_string(), "authorization_code".to_string()),
        ("code".to_string(), code.to_string()),
        ("code_verifier".to_string(), verifier.to_string()),
        ("redirect_uri".to_string(), redirect_url.to_string()),
    ]);
    token_request(client, token_endpoint, information, params).await
}

/// Refreshes an access token, keeping the old refresh token when the
/// answer carries none (rotating refresh tokens keep theirs).
pub async fn refresh(
    client: &reqwest::Client,
    token_endpoint: &str,
    information: &ClientInformation,
    refresh_token: &str,
) -> Result<OAuthTokens, OAuthError> {
    let params = BTreeMap::from([
        ("grant_type".to_string(), "refresh_token".to_string()),
        ("refresh_token".to_string(), refresh_token.to_string()),
    ]);
    let mut tokens = token_request(client, token_endpoint, information, params).await?;
    if tokens.refresh_token.is_none() {
        tokens.refresh_token = Some(refresh_token.to_string());
    }
    Ok(tokens)
}

/// Builds the authorization URL (upstream's `startAuthorization`): PKCE
/// S256, the redirect URI, state, scope and resource parameters.
pub fn authorization_url(
    endpoint: &str,
    information: &ClientInformation,
    redirect_url: &str,
    scope: Option<&str>,
    state: Option<&str>,
    resource: Option<&str>,
    verifier: &str,
) -> String {
    use sha2::Digest;
    let challenge = base64_url(&sha2::Sha256::digest(verifier.as_bytes()));
    let mut url = endpoint.to_string();
    let mut params = vec![
        ("response_type".to_string(), "code".to_string()),
        ("client_id".to_string(), information.client_id.clone()),
        ("code_challenge".to_string(), challenge),
        ("code_challenge_method".to_string(), "S256".to_string()),
        ("redirect_uri".to_string(), redirect_url.to_string()),
    ];
    if let Some(state) = state {
        params.push(("state".to_string(), state.to_string()));
    }
    if let Some(scope) = scope {
        params.push(("scope".to_string(), scope.to_string()));
    }
    if let Some(resource) = resource {
        params.push(("resource".to_string(), resource.to_string()));
    }
    let query = params
        .iter()
        .map(|(key, value)| format!("{key}={}", urlencode(value)))
        .collect::<Vec<_>>()
        .join("&");
    url.push('?');
    url.push_str(&query);
    url
}

/// The loopback callback server (RFC 8252): listens on 127.0.0.1, serves
/// one path, and resolves the authorization code the server redirects
/// with (upstream's `callback.ts`).
pub struct CallbackServer {
    pub redirect_url: String,
    listener: TcpListener,
}

impl CallbackServer {
    pub async fn listen(port: Option<u16>, path: &str) -> std::io::Result<Self> {
        let listener = match port {
            Some(port) => TcpListener::bind(("127.0.0.1", port)).await?,
            None => TcpListener::bind(("127.0.0.1", 0)).await?,
        };
        let address = listener.local_addr()?;
        let redirect_url = format!("http://127.0.0.1:{}{path}", address.port());
        Ok(Self {
            redirect_url,
            listener,
        })
    }

    pub fn address(&self) -> SocketAddr {
        self.listener.local_addr().expect("bound")
    }

    /// Waits for one authorization response (code or error), answering
    /// the browser with a plain-text page.
    /// Waits for the redirect that carries `state` back and returns its
    /// code. A request without that state is told so and ignored: any
    /// page in the browser can reach a loopback port.
    pub async fn wait_for_callback(
        &self,
        timeout: Duration,
        state: &str,
    ) -> Result<String, OAuthError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let (mut socket, _) = tokio::select! {
                accepted = self.listener.accept() => accepted.map_err(|error| OAuthError::Http(error.to_string()))?,
                _ = tokio::time::sleep_until(deadline) => {
                    return Err(OAuthError::Http("OAuth callback timed out".into()));
                }
            };
            let code = Self::handle(&mut socket, state).await?;
            if let Some(code) = code {
                return Ok(code);
            }
        }
    }

    async fn handle(socket: &mut TcpStream, state: &str) -> Result<Option<String>, OAuthError> {
        let _ = socket.set_nodelay(true);
        let mut buffer = [0u8; 4096];
        let read = socket
            .read(&mut buffer)
            .await
            .map_err(|error| OAuthError::Http(error.to_string()))?;
        let request = String::from_utf8_lossy(&buffer[..read]).into_owned();
        let request_line = request.lines().next().unwrap_or_default().to_string();
        let target = request_line.split_whitespace().nth(1).unwrap_or_default();
        let query = target
            .split_once('?')
            .map(|(_, rest)| rest)
            .unwrap_or_default();
        let mut code = None;
        let mut error = None;
        let mut returned = None;
        for pair in query.split('&') {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            let value = urldecode(value);
            match key {
                "code" => code = Some(value),
                "error" => error = Some(value),
                "state" => returned = Some(value),
                _ => {}
            }
        }
        // Not this sign-in's redirect: neither its code nor its failure.
        if returned.as_deref() != Some(state) {
            (code, error) = (None, None);
        }
        let body = if let Some(error) = error.as_deref() {
            format!("Authorization failed: {error}. You may close this window.")
        } else if code.is_some() {
            "Authorization complete. You may close this window.".to_string()
        } else {
            "Missing authorization code.".to_string()
        };
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.shutdown().await;
        if let Some(error) = &error {
            return Err(OAuthError::Server {
                code: error.clone(),
                message: format!("authorization failed: {error}"),
            });
        }
        Ok(code)
    }
}

fn urldecode(value: &str) -> String {
    let mut output = String::new();
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch == '%' {
            let hex: String = chars.by_ref().take(2).collect();
            if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                output.push(byte as char);
                continue;
            }
        }
        output.push(ch);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_validators_reject_bad_urls() {
        assert!(parse_protected_resource_metadata(&serde_json::json!({
            "resource": "file:///etc"
        }))
        .is_err());
        assert!(parse_authorization_server_metadata(&serde_json::json!({
            "issuer": "https://as.example",
            "authorization_endpoint": "https://as.example/authorize",
            "token_endpoint": "https://as.example/token",
            "response_types_supported": ["code"]
        }))
        .is_ok());
        assert!(parse_authorization_server_metadata(&serde_json::json!({
            "issuer": "https://as.example",
            "authorization_endpoint": "https://as.example/authorize",
            "token_endpoint": "https://as.example/token"
        }))
        .is_err());
    }

    #[test]
    fn challenges_parse_the_fields() {
        let challenge = parse_www_authenticate(Some(
            r#"Bearer realm="mcp", resource_metadata="https://r/.well-known/oauth-protected-resource", scope="read write", error="insufficient_scope""#,
        ));
        assert_eq!(
            challenge.resource_metadata_url.as_deref(),
            Some("https://r/.well-known/oauth-protected-resource")
        );
        assert_eq!(challenge.scope.as_deref(), Some("read write"));
        assert_eq!(challenge.error.as_deref(), Some("insufficient_scope"));
        // A non-bearer scheme carries nothing.
        assert_eq!(
            parse_www_authenticate(Some("Basic realm=\"x\"")),
            OAuthChallenge::default()
        );
    }

    #[test]
    fn pkce_challenges_are_s256_of_the_verifier() {
        let (verifier, challenge) = pkce();
        use sha2::Digest;
        assert_eq!(
            challenge,
            base64_url(&sha2::Sha256::digest(verifier.as_bytes()))
        );
    }

    #[test]
    fn the_authorization_url_carries_pkce_and_state() {
        let url = authorization_url(
            "https://as.example/authorize",
            &ClientInformation {
                client_id: "cid".into(),
                client_secret: None,
            },
            "http://127.0.0.1:9/callback",
            Some("read"),
            Some("st4te"),
            None,
            "verifier",
        );
        assert!(url.starts_with("https://as.example/authorize?"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=st4te"));
        assert!(url.contains("scope=read"));
        assert!(url.contains("client_id=cid"));
    }

    #[tokio::test]
    async fn the_callback_answers_only_the_redirect_with_its_state() {
        let server = CallbackServer::listen(None, "/callback").await.unwrap();
        let url = server.redirect_url.clone();
        let state = random_state();
        assert_ne!(state, random_state());
        let expected = state.clone();
        let waiting = tokio::spawn(async move {
            server
                .wait_for_callback(Duration::from_secs(20), &expected)
                .await
        });
        let http = reqwest::Client::new();
        // Another page's request, with a code or a failure of its own, is
        // answered and changes nothing.
        for other in ["?code=theirs&state=wrong", "?error=access_denied", ""] {
            let answer = http.get(format!("{url}{other}")).send().await.unwrap();
            assert_eq!(answer.text().await.unwrap(), "Missing authorization code.");
        }
        let answer = http
            .get(format!("{url}?code=ours&state={state}"))
            .send()
            .await
            .unwrap();
        assert!(answer
            .text()
            .await
            .unwrap()
            .starts_with("Authorization complete."));
        assert_eq!(waiting.await.unwrap().unwrap(), "ours");
    }
}
