use std::path::Path;

use pi_config::{
    read_usage_cache, write_usage_cache, AccountUsage, Balance, ResolvedAuth, UsageWindow,
};
use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

pub const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
pub const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
pub const MOONSHOT_BALANCE_URL: &str = "https://api.moonshot.ai/v1/users/me/balance";
pub const OPENROUTER_CREDITS_URL: &str = "https://openrouter.ai/api/v1/credits";
pub const DEEPSEEK_BALANCE_URL: &str = "https://api.deepseek.com/user/balance";
pub const COPILOT_USAGE_URL: &str = "https://api.github.com/copilot_internal/user";

const ZAI_QUOTA_PATH: &str = "/api/monitor/usage/quota/limit";
const ZAI_ALLOWED_ORIGINS: [&str; 2] = ["https://api.z.ai", "https://open.bigmodel.cn"];

pub const USAGE_CACHE_TTL_SECONDS: i64 = 15 * 60;

#[derive(Debug, Clone, PartialEq)]
pub enum UsageProbeResult {
    Usage(AccountUsage),
    Unsupported,
    Failed(String),
}

pub fn usage_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

#[derive(Debug, Clone)]
pub struct UsageRequest<'a> {
    pub provider: &'a str,
    pub account: &'a str,
    pub auth: &'a ResolvedAuth,
    pub base_url: Option<&'a str>,
    pub refresh: bool,
    pub endpoint_override: Option<&'a str>,
}

pub async fn get_usage(
    client: &reqwest::Client,
    cache_path: &Path,
    request: UsageRequest<'_>,
    now: i64,
) -> UsageProbeResult {
    let key = format!("{}:{}", request.provider, request.account);
    let cached = read_usage_cache(cache_path)
        .ok()
        .flatten()
        .and_then(|cache| cache.accounts.get(&key).cloned());
    if !request.refresh {
        if let Some(usage) = cached
            .as_ref()
            .filter(|usage| now - usage.fetched_at < USAGE_CACHE_TTL_SECONDS)
        {
            return UsageProbeResult::Usage(usage.clone());
        }
    }

    match probe(
        client,
        request.provider,
        request.auth,
        request.base_url,
        request.endpoint_override,
        now,
    )
    .await
    {
        UsageProbeResult::Usage(usage) => {
            let _ = store_usage(cache_path, &key, &usage, now);
            UsageProbeResult::Usage(usage)
        }
        UsageProbeResult::Failed(reason) => match cached {
            Some(stale) => UsageProbeResult::Usage(stale),
            None => UsageProbeResult::Failed(reason),
        },
        UsageProbeResult::Unsupported => UsageProbeResult::Unsupported,
    }
}

fn store_usage(cache_path: &Path, key: &str, usage: &AccountUsage, now: i64) -> Result<(), String> {
    let mut cache = read_usage_cache(cache_path)
        .ok()
        .flatten()
        .unwrap_or_default();
    cache.refreshed_at = now.max(0) as u64;
    cache.accounts.insert(key.to_string(), usage.clone());
    write_usage_cache(cache_path, &cache).map_err(|error| error.to_string())
}

async fn probe(
    client: &reqwest::Client,
    provider: &str,
    auth: &ResolvedAuth,
    base_url: Option<&str>,
    endpoint_override: Option<&str>,
    now: i64,
) -> UsageProbeResult {
    match provider {
        "openai" | "openai-codex" => match auth {
            ResolvedAuth::ChatGptOAuth {
                access_token,
                account_id,
                ..
            } => {
                let url = endpoint_override.unwrap_or(CODEX_USAGE_URL);
                match crate::chatgpt_model_headers(access_token, account_id.as_deref()) {
                    Ok(headers) => fetch_usage(client, url, headers, now, parse_codex_usage).await,
                    Err(error) => UsageProbeResult::Failed(error.to_string()),
                }
            }
            _ => UsageProbeResult::Unsupported,
        },
        "anthropic" => match auth {
            ResolvedAuth::ClaudeCodeOAuth { .. } => {
                let url = endpoint_override.unwrap_or(CLAUDE_USAGE_URL);
                match crate::anthropic_model_headers(auth) {
                    Ok(headers) => fetch_usage(client, url, headers, now, parse_claude_usage).await,
                    Err(error) => UsageProbeResult::Failed(error.to_string()),
                }
            }
            _ => UsageProbeResult::Unsupported,
        },
        "zai" | "zai-coding" | "zai-anthropic" => match auth {
            ResolvedAuth::ApiKey(api_key) => {
                let origin = base_url
                    .and_then(origin_of)
                    .unwrap_or("https://api.z.ai")
                    .to_string();
                if !ZAI_ALLOWED_ORIGINS.contains(&origin.as_str()) {
                    return UsageProbeResult::Unsupported;
                }
                let url = endpoint_override
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("{origin}{ZAI_QUOTA_PATH}"));
                fetch_usage(client, &url, bearer_headers(api_key), now, parse_zai_usage).await
            }
            _ => UsageProbeResult::Unsupported,
        },
        "moonshotai" => match auth {
            ResolvedAuth::ApiKey(api_key) => {
                let url = endpoint_override.unwrap_or(MOONSHOT_BALANCE_URL);
                fetch_usage(
                    client,
                    url,
                    bearer_headers(api_key),
                    now,
                    parse_moonshot_balance,
                )
                .await
            }
            _ => UsageProbeResult::Unsupported,
        },
        "openrouter" => match auth {
            ResolvedAuth::ApiKey(api_key) => {
                let url = endpoint_override.unwrap_or(OPENROUTER_CREDITS_URL);
                fetch_usage(
                    client,
                    url,
                    bearer_headers(api_key),
                    now,
                    parse_openrouter_credits,
                )
                .await
            }
            _ => UsageProbeResult::Unsupported,
        },
        "deepseek" => match auth {
            ResolvedAuth::ApiKey(api_key) => {
                let url = endpoint_override.unwrap_or(DEEPSEEK_BALANCE_URL);
                fetch_usage(
                    client,
                    url,
                    bearer_headers(api_key),
                    now,
                    parse_deepseek_balance,
                )
                .await
            }
            _ => UsageProbeResult::Unsupported,
        },
        "github-copilot" => match auth {
            ResolvedAuth::ApiKey(token) => {
                let url = endpoint_override.unwrap_or(COPILOT_USAGE_URL);
                fetch_usage(client, url, github_headers(token), now, parse_copilot_usage).await
            }
            _ => UsageProbeResult::Unsupported,
        },
        _ => UsageProbeResult::Unsupported,
    }
}

fn bearer_headers(value: &str) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    if let Ok(value) = reqwest::header::HeaderValue::from_str(&format!("Bearer {value}")) {
        headers.insert(reqwest::header::AUTHORIZATION, value);
    }
    headers.insert(
        reqwest::header::ACCEPT,
        reqwest::header::HeaderValue::from_static("application/json"),
    );
    headers
}

fn github_headers(token: &str) -> reqwest::header::HeaderMap {
    let mut headers = bearer_headers(token);
    headers.insert(
        "x-github-api-version",
        reqwest::header::HeaderValue::from_static("2022-11-28"),
    );
    headers.insert(
        "user-agent",
        reqwest::header::HeaderValue::from_static("pi"),
    );
    headers
}

async fn fetch_usage(
    client: &reqwest::Client,
    url: &str,
    headers: reqwest::header::HeaderMap,
    now: i64,
    parse: fn(&str, i64) -> Result<AccountUsage, String>,
) -> UsageProbeResult {
    let result = async {
        let response = client
            .get(url)
            .headers(headers)
            .send()
            .await
            .map_err(|error| error.to_string())?;
        let status = response.status();
        let body = response.text().await.map_err(|error| error.to_string())?;
        if !status.is_success() {
            return Err(format!("status {status}: {body}"));
        }
        parse(&body, now)
    }
    .await;
    match result {
        Ok(usage) => UsageProbeResult::Usage(usage),
        Err(reason) => UsageProbeResult::Failed(reason),
    }
}

fn origin_of(base_url: &str) -> Option<&str> {
    let rest = base_url.split_once("//")?.1;
    let end = rest.find('/').unwrap_or(rest.len());
    Some(&base_url[..base_url.len() - rest.len() + end])
}

fn flexible_f64(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
}

fn flexible_epoch(value: &Value) -> Option<i64> {
    if let Some(number) = value.as_i64() {
        return Some(if number > 1_000_000_000_000 {
            number / 1000
        } else {
            number
        });
    }
    value.as_str().and_then(|text| {
        OffsetDateTime::parse(text, &Rfc3339)
            .ok()
            .map(|parsed| parsed.unix_timestamp())
    })
}

fn window(label: &str, used_pct: Option<f64>, resets_at: Option<i64>) -> Option<UsageWindow> {
    Some(UsageWindow {
        label: label.to_string(),
        used_pct: used_pct?,
        resets_at,
    })
}

fn parse_codex_usage(body: &str, now: i64) -> Result<AccountUsage, String> {
    let value = serde_json::from_str::<Value>(body).map_err(|error| error.to_string())?;
    let rate_limit = value.get("rate_limit");
    let mut windows = Vec::new();
    for (key, label) in [("primary_window", "5h"), ("secondary_window", "weekly")] {
        let window_value = rate_limit.and_then(|rate_limit| rate_limit.get(key));
        if let Some(window) = window(
            label,
            window_value
                .and_then(|window| window.get("used_percent"))
                .and_then(flexible_f64),
            window_value
                .and_then(|window| window.get("reset_at"))
                .and_then(flexible_epoch),
        ) {
            windows.push(window);
        }
    }
    let balance = value
        .get("credits")
        .and_then(|credits| credits.get("balance"))
        .and_then(flexible_f64)
        .map(|amount| Balance {
            amount,
            currency: "USD".to_string(),
        });
    Ok(AccountUsage {
        plan: value
            .get("plan_type")
            .and_then(Value::as_str)
            .map(str::to_string),
        windows,
        balance,
        fetched_at: now,
    })
}

fn parse_claude_usage(body: &str, now: i64) -> Result<AccountUsage, String> {
    let value = serde_json::from_str::<Value>(body).map_err(|error| error.to_string())?;
    let mut windows = Vec::new();
    for (key, label) in [("five_hour", "5h"), ("seven_day", "weekly")] {
        let window_value = value.get(key);
        if let Some(window) = window(
            label,
            window_value
                .and_then(|window| window.get("utilization"))
                .and_then(flexible_f64),
            window_value
                .and_then(|window| window.get("resets_at"))
                .and_then(flexible_epoch),
        ) {
            windows.push(window);
        }
    }
    Ok(AccountUsage {
        plan: None,
        windows,
        balance: None,
        fetched_at: now,
    })
}

fn parse_zai_usage(body: &str, now: i64) -> Result<AccountUsage, String> {
    let value = serde_json::from_str::<Value>(body).map_err(|error| error.to_string())?;
    let limits = value
        .get("data")
        .and_then(|data| data.get("limits"))
        .and_then(Value::as_array);
    let mut windows = Vec::new();
    for limit in limits.into_iter().flatten() {
        let kind = limit.get("type").and_then(Value::as_str).unwrap_or("");
        let label = match kind {
            "TIME_LIMIT" => "5h",
            "TOKENS_LIMIT" => "weekly",
            other => other,
        };
        if let Some(window) = window(
            label,
            limit.get("percentage").and_then(flexible_f64),
            limit.get("next_reset_time").and_then(flexible_epoch),
        ) {
            windows.push(window);
        }
    }
    Ok(AccountUsage {
        plan: None,
        windows,
        balance: None,
        fetched_at: now,
    })
}

fn parse_moonshot_balance(body: &str, now: i64) -> Result<AccountUsage, String> {
    let value = serde_json::from_str::<Value>(body).map_err(|error| error.to_string())?;
    let data = value.get("data").cloned().unwrap_or(Value::Null);
    let amount = data
        .get("cash_balance")
        .or_else(|| data.get("balance"))
        .and_then(flexible_f64);
    Ok(AccountUsage {
        plan: None,
        windows: Vec::new(),
        balance: amount.map(|amount| Balance {
            amount,
            currency: "CNY".to_string(),
        }),
        fetched_at: now,
    })
}

fn parse_openrouter_credits(body: &str, now: i64) -> Result<AccountUsage, String> {
    let value = serde_json::from_str::<Value>(body).map_err(|error| error.to_string())?;
    let data = value.get("data").cloned().unwrap_or(Value::Null);
    let remaining = match (
        data.get("total_credits").and_then(flexible_f64),
        data.get("total_usage").and_then(flexible_f64),
    ) {
        (Some(credits), Some(used)) => Some(credits - used),
        _ => None,
    };
    Ok(AccountUsage {
        plan: None,
        windows: Vec::new(),
        balance: remaining.map(|amount| Balance {
            amount,
            currency: "USD".to_string(),
        }),
        fetched_at: now,
    })
}

fn parse_deepseek_balance(body: &str, now: i64) -> Result<AccountUsage, String> {
    let value = serde_json::from_str::<Value>(body).map_err(|error| error.to_string())?;
    let infos = value
        .get("balance_infos")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let entry = infos
        .iter()
        .find(|info| info.get("currency").and_then(Value::as_str) == Some("USD"))
        .or_else(|| infos.first());
    let balance = entry.and_then(|info| {
        let amount = info.get("total_balance").and_then(flexible_f64)?;
        let currency = info
            .get("currency")
            .and_then(Value::as_str)
            .unwrap_or("USD")
            .to_string();
        Some(Balance { amount, currency })
    });
    Ok(AccountUsage {
        plan: None,
        windows: Vec::new(),
        balance,
        fetched_at: now,
    })
}

fn parse_copilot_usage(body: &str, now: i64) -> Result<AccountUsage, String> {
    let value = serde_json::from_str::<Value>(body).map_err(|error| error.to_string())?;
    Ok(AccountUsage {
        plan: value
            .get("plan")
            .or_else(|| value.get("copilot_plan"))
            .and_then(Value::as_str)
            .map(str::to_string),
        windows: Vec::new(),
        balance: None,
        fetched_at: now,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn oauth_codex_auth() -> ResolvedAuth {
        ResolvedAuth::ChatGptOAuth {
            access_token: "codex-access".to_string(),
            refresh_token: None,
            expires: 0,
            account_id: Some("account-id".to_string()),
        }
    }

    fn claude_auth() -> ResolvedAuth {
        ResolvedAuth::ClaudeCodeOAuth {
            access_token: "claude-access".to_string(),
            refresh_token: None,
            expires: 0,
        }
    }

    #[test]
    fn origin_of_extracts_scheme_and_host() {
        assert_eq!(
            origin_of("https://api.z.ai/api/coding/paas/v4"),
            Some("https://api.z.ai")
        );
        assert_eq!(
            origin_of("https://open.bigmodel.cn"),
            Some("https://open.bigmodel.cn")
        );
        assert_eq!(origin_of("not-a-url"), None);
    }

    #[test]
    fn parse_codex_usage_normalizes_windows_and_credits() {
        let usage = parse_codex_usage(
            r#"{"plan_type":"plus","rate_limit":{"primary_window":{"used_percent":12,"reset_at":1800000000},"secondary_window":{"used_percent":34.5,"reset_at":1800500000}},"credits":{"has_credits":true,"unlimited":false,"balance":"10.50"}}"#,
            1000,
        )
        .expect("parse codex usage");
        assert_eq!(usage.plan.as_deref(), Some("plus"));
        assert_eq!(usage.windows.len(), 2);
        assert_eq!(usage.windows[0].label, "5h");
        assert_eq!(usage.windows[0].used_pct, 12.0);
        assert_eq!(usage.windows[0].resets_at, Some(1800000000));
        assert_eq!(usage.windows[1].label, "weekly");
        assert_eq!(
            usage.balance,
            Some(Balance {
                amount: 10.5,
                currency: "USD".to_string()
            })
        );
        assert_eq!(usage.fetched_at, 1000);
    }

    #[test]
    fn parse_codex_usage_tolerates_missing_sections() {
        let usage = parse_codex_usage(r#"{"plan_type":"free"}"#, 1000).expect("parse sparse");
        assert_eq!(usage.plan.as_deref(), Some("free"));
        assert!(usage.windows.is_empty());
        assert_eq!(usage.balance, None);
    }

    #[test]
    fn parse_claude_usage_normalizes_utilization_windows() {
        let usage = parse_claude_usage(
            r#"{"five_hour":{"utilization":42.0,"resets_at":"2027-11-24T15:00:00Z"},"seven_day":{"utilization":8.0,"resets_at":"2027-11-30T00:00:00.000Z"}}"#,
            1000,
        )
        .expect("parse claude usage");
        assert_eq!(usage.windows.len(), 2);
        assert_eq!(usage.windows[0].label, "5h");
        assert_eq!(usage.windows[0].used_pct, 42.0);
        assert!(usage.windows[0].resets_at.is_some());
        assert_eq!(usage.windows[1].label, "weekly");
        assert!(usage.windows[1].resets_at.is_some());
    }

    #[test]
    fn parse_zai_usage_maps_limit_types() {
        let usage = parse_zai_usage(
            r#"{"code":200,"msg":"ok","data":{"limits":[{"type":"TIME_LIMIT","percentage":12,"next_reset_time":1800000000000},{"type":"TOKENS_LIMIT","percentage":34,"next_reset_time":1800500000000,"usage":20000000}]}}"#,
            1000,
        )
        .expect("parse zai usage");
        assert_eq!(usage.windows.len(), 2);
        assert_eq!(usage.windows[0].label, "5h");
        assert_eq!(usage.windows[0].used_pct, 12.0);
        assert_eq!(usage.windows[0].resets_at, Some(1800000000));
        assert_eq!(usage.windows[1].label, "weekly");
        assert_eq!(usage.windows[1].resets_at, Some(1800500000));
    }

    #[test]
    fn parse_balance_variants() {
        let moonshot = parse_moonshot_balance(
            r#"{"code":0,"data":{"cash_balance":"12.34","voucher_balance":"0.50"}}"#,
            1000,
        )
        .expect("moonshot");
        assert_eq!(
            moonshot.balance,
            Some(Balance {
                amount: 12.34,
                currency: "CNY".to_string()
            })
        );

        let openrouter =
            parse_openrouter_credits(r#"{"data":{"total_credits":10.0,"total_usage":2.5}}"#, 1000)
                .expect("openrouter");
        assert_eq!(
            openrouter.balance,
            Some(Balance {
                amount: 7.5,
                currency: "USD".to_string()
            })
        );

        let deepseek = parse_deepseek_balance(
            r#"{"is_available":true,"balance_infos":[{"currency":"CNY","total_balance":"20.00"},{"currency":"USD","total_balance":"5.00"}]}"#,
            1000,
        )
        .expect("deepseek");
        assert_eq!(
            deepseek.balance,
            Some(Balance {
                amount: 5.0,
                currency: "USD".to_string()
            })
        );
    }

    #[test]
    fn parse_copilot_usage_reads_plan_leniently() {
        let usage = parse_copilot_usage(r#"{"plan":"individual_pro","quota_snapshots":{}}"#, 1000)
            .expect("copilot");
        assert_eq!(usage.plan.as_deref(), Some("individual_pro"));
        let sparse = parse_copilot_usage(r#"{"login":"octocat"}"#, 1000).expect("sparse");
        assert_eq!(sparse.plan, None);
    }

    #[tokio::test]
    async fn probe_codex_sends_oauth_headers() {
        let stub = spawn_stub(
            200,
            r#"{"plan_type":"plus","rate_limit":{"primary_window":{"used_percent":5,"reset_at":1800000000}}}"#,
        );
        let client = usage_http_client();
        let result = probe(
            &client,
            "openai-codex",
            &oauth_codex_auth(),
            None,
            Some(&stub.url),
            1000,
        )
        .await;

        assert!(matches!(result, UsageProbeResult::Usage(_)));
        let requests = stub.requests.lock().expect("requests lock");
        let request = &requests[0];
        assert!(request.starts_with("GET / HTTP/1.1"), "{request}");
        assert!(
            request.contains("authorization: Bearer codex-access"),
            "{request}"
        );
        assert!(
            request.contains("chatgpt-account-id: account-id"),
            "{request}"
        );
    }

    #[tokio::test]
    async fn probe_claude_sends_oauth_beta_header() {
        let stub = spawn_stub(
            200,
            r#"{"five_hour":{"utilization":10.0,"resets_at":"2027-11-24T15:00:00Z"}}"#,
        );
        let client = usage_http_client();
        let result = probe(
            &client,
            "anthropic",
            &claude_auth(),
            None,
            Some(&stub.url),
            1000,
        )
        .await;

        assert!(matches!(result, UsageProbeResult::Usage(_)));
        let requests = stub.requests.lock().expect("requests lock");
        let request = &requests[0];
        assert!(
            request.contains("authorization: Bearer claude-access"),
            "{request}"
        );
        assert!(
            request.contains("anthropic-beta: oauth-2025-04-20"),
            "{request}"
        );
    }

    #[tokio::test]
    async fn probe_zai_enforces_origin_allowlist() {
        let stub = spawn_stub(200, r#"{"code":200,"data":{"limits":[]}}"#);
        let client = usage_http_client();
        let auth = ResolvedAuth::ApiKey("zai-key".to_string());

        let blocked = probe(
            &client,
            "zai-coding",
            &auth,
            Some("https://evil.example.com/v4"),
            None,
            1000,
        )
        .await;
        assert_eq!(blocked, UsageProbeResult::Unsupported);

        let allowed = probe(
            &client,
            "zai-coding",
            &auth,
            Some("https://api.z.ai/api/coding/paas/v4"),
            Some(&stub.url),
            1000,
        )
        .await;
        assert!(matches!(allowed, UsageProbeResult::Usage(_)));
        let requests = stub.requests.lock().expect("requests lock");
        assert!(
            requests[0].contains("authorization: Bearer zai-key"),
            "{}",
            requests[0]
        );
    }

    #[tokio::test]
    async fn probe_api_key_balance_providers_and_unsupported() {
        let stub = spawn_stub(200, r#"{"data":{"total_credits":10.0,"total_usage":2.5}}"#);
        let client = usage_http_client();
        let auth = ResolvedAuth::ApiKey("or-key".to_string());

        let result = probe(&client, "openrouter", &auth, None, Some(&stub.url), 1000).await;
        match result {
            UsageProbeResult::Usage(usage) => assert_eq!(
                usage.balance,
                Some(Balance {
                    amount: 7.5,
                    currency: "USD".to_string()
                })
            ),
            other => panic!("expected usage, got {other:?}"),
        }
        let captured = stub.requests.lock().expect("requests lock").clone();
        assert!(
            captured[0].contains("authorization: Bearer or-key"),
            "{}",
            captured[0]
        );

        assert_eq!(
            probe(&client, "xai", &auth, None, None, 1000).await,
            UsageProbeResult::Unsupported
        );
        assert_eq!(
            probe(&client, "google", &auth, None, None, 1000).await,
            UsageProbeResult::Unsupported
        );
        assert_eq!(
            probe(&client, "openrouter", &claude_auth(), None, None, 1000).await,
            UsageProbeResult::Unsupported
        );
    }

    #[tokio::test]
    async fn get_usage_caches_within_ttl() {
        let root = test_dir("pi-cli-usage-cache-ttl");
        let cache_path = root.join("usage-cache.json");
        let stub = spawn_stub(
            200,
            r#"{"plan_type":"plus","rate_limit":{"primary_window":{"used_percent":5,"reset_at":1800000000}}}"#,
        );
        let client = usage_http_client();
        let auth = oauth_codex_auth();

        let first = get_usage(
            &client,
            &cache_path,
            UsageRequest {
                provider: "openai-codex",
                account: "default",
                auth: &auth,
                base_url: None,
                refresh: false,
                endpoint_override: Some(&stub.url),
            },
            1_000,
        )
        .await;
        assert!(matches!(first, UsageProbeResult::Usage(_)));
        assert_eq!(stub.hits.load(Ordering::SeqCst), 1);

        let second = get_usage(
            &client,
            &cache_path,
            UsageRequest {
                provider: "openai-codex",
                account: "default",
                auth: &auth,
                base_url: None,
                refresh: false,
                endpoint_override: Some(&stub.url),
            },
            1_000 + USAGE_CACHE_TTL_SECONDS - 1,
        )
        .await;
        assert!(matches!(second, UsageProbeResult::Usage(_)));
        assert_eq!(stub.hits.load(Ordering::SeqCst), 1);

        let third = get_usage(
            &client,
            &cache_path,
            UsageRequest {
                provider: "openai-codex",
                account: "default",
                auth: &auth,
                base_url: None,
                refresh: false,
                endpoint_override: Some(&stub.url),
            },
            1_000 + USAGE_CACHE_TTL_SECONDS,
        )
        .await;
        assert!(matches!(third, UsageProbeResult::Usage(_)));
        assert_eq!(stub.hits.load(Ordering::SeqCst), 2);

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn get_usage_falls_back_to_stale_cache_on_failure() {
        let root = test_dir("pi-cli-usage-stale");
        let cache_path = root.join("usage-cache.json");
        let stub = spawn_stub(
            200,
            r#"{"five_hour":{"utilization":33.0,"resets_at":"2027-11-24T15:00:00Z"}}"#,
        );
        let client = usage_http_client();
        let auth = claude_auth();

        let first = get_usage(
            &client,
            &cache_path,
            UsageRequest {
                provider: "anthropic",
                account: "default",
                auth: &auth,
                base_url: None,
                refresh: false,
                endpoint_override: Some(&stub.url),
            },
            1_000,
        )
        .await;
        let UsageProbeResult::Usage(fresh) = first else {
            panic!("expected usage");
        };
        assert_eq!(fresh.windows[0].used_pct, 33.0);

        let rate_limited = spawn_stub(429, r#"{"error":"rate limited"}"#);
        let second = get_usage(
            &client,
            &cache_path,
            UsageRequest {
                provider: "anthropic",
                account: "default",
                auth: &auth,
                base_url: None,
                refresh: true,
                endpoint_override: Some(&rate_limited.url),
            },
            5_000,
        )
        .await;
        let UsageProbeResult::Usage(stale) = second else {
            panic!("expected stale cache fallback");
        };
        assert_eq!(stale.windows[0].used_pct, 33.0);
        assert_eq!(stale.fetched_at, 1_000);
        assert_eq!(rate_limited.hits.load(Ordering::SeqCst), 1);

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn get_usage_reports_failure_without_cache() {
        let root = test_dir("pi-cli-usage-failed");
        let cache_path = root.join("usage-cache.json");
        let stub = spawn_stub(429, r#"{"error":"rate limited"}"#);
        let client = usage_http_client();
        let auth = claude_auth();

        let result = get_usage(
            &client,
            &cache_path,
            UsageRequest {
                provider: "anthropic",
                account: "default",
                auth: &auth,
                base_url: None,
                refresh: false,
                endpoint_override: Some(&stub.url),
            },
            1_000,
        )
        .await;
        match result {
            UsageProbeResult::Failed(reason) => assert!(reason.contains("429"), "{reason}"),
            other => panic!("expected failure, got {other:?}"),
        }
        assert!(!cache_path.exists());

        let _ = std::fs::remove_dir_all(root);
    }
}
