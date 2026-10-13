//! Failover-only multi-account provider wrapper.
//!
//! Intent: availability, not throughput. One "sticky" account carries the
//! session; other stored accounts are used only when the sticky one returns
//! an exhaustion signal (HTTP 429 or 529) with no streamed output. Selection
//! is sticky across turns, cooldowns come from the provider's own reset
//! timestamps, and switching is rate-limited so the wrapper never flaps
//! between accounts. This deliberately does not load-balance, rebalance to
//! the lowest-usage account, or spread requests across the pool.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use async_trait::async_trait;
use pi_ai::{Provider, ProviderAuth, ProviderError, ProviderRequest, StreamEvent};
use pi_config::{auth_for_provider, AuthData, ResolvedAuth, UsageCache, UsageWindow};

/// Minimum time between voluntary account switches, in milliseconds.
/// Prevents rapid flapping when multiple accounts are near their limits.
/// Involuntary switches (the current account just returned 429/529) are not
/// subject to this guard.
pub const MIN_SWITCH_INTERVAL_MS: u64 = 10_000;

/// Usage windows at or above this percentage are treated as exhausted for
/// candidate filtering when no explicit cooldown exists.
pub const EXHAUSTION_THRESHOLD_PCT: f64 = 90.0;

/// Fallback cooldown in seconds when no usage-window reset time is known:
/// long enough that a saturated account is skipped for the current work,
/// short enough that a transient 429 does not bench an account for an hour.
pub const DEFAULT_COOLDOWN_SECONDS: i64 = 300;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailoverConfig {
    pub enabled: bool,
    /// Explicit per-account cooldown overrides: account -> epoch seconds.
    pub cooldowns: BTreeMap<String, i64>,
    pub min_switch_interval_ms: u64,
}

impl Default for FailoverConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            cooldowns: BTreeMap::new(),
            min_switch_interval_ms: MIN_SWITCH_INTERVAL_MS,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FailoverCandidate {
    pub account: String,
    pub auth: ResolvedAuth,
    /// Cooldown deadline (epoch seconds). Zero or past means available.
    pub cooldown_until: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailoverDecision {
    /// The account that should serve the current request.
    pub account: String,
    /// True when this is a switch away from the previously used account.
    pub switched: bool,
}

/// Order candidates for sticky selection: the previous account first, then
/// the remaining accounts in storage order. Deterministic: storage order is
/// the sorted key order of `auth.json`.
fn ordered_candidates(
    candidates: &[FailoverCandidate],
    previous: Option<&str>,
) -> Vec<FailoverCandidate> {
    let mut ordered: Vec<FailoverCandidate> = Vec::with_capacity(candidates.len());
    if let Some(previous) = previous {
        if let Some(index) = candidates
            .iter()
            .position(|candidate| candidate.account == previous)
        {
            ordered.push(candidates[index].clone());
        }
    }
    for candidate in candidates {
        if previous != Some(candidate.account.as_str()) {
            ordered.push(candidate.clone());
        }
    }
    ordered
}

/// Pick the account that should serve the next request.
///
/// Sticky by design: `previous` wins unless it is cooling down, in which case
/// the next available account in storage order serves the request and becomes
/// the new sticky choice for subsequent turns.
pub fn select_account(
    candidates: &[FailoverCandidate],
    previous: Option<&str>,
    now: i64,
) -> Option<FailoverDecision> {
    let ordered = ordered_candidates(candidates, previous);
    let pick = ordered
        .iter()
        .find(|candidate| candidate.cooldown_until <= now)?;
    Some(FailoverDecision {
        account: pick.account.clone(),
        switched: previous.is_some_and(|previous| previous != pick.account),
    })
}

/// Build the failover pool for a provider from stored auth and cached usage.
///
/// Env and imported-file accounts are excluded: they are not stored under an
/// account name, so they cannot participate in named rotation, and mixing
/// them in would produce ambiguous resolution.
pub fn failover_candidates(
    auth: &AuthData,
    usage_cache: Option<&UsageCache>,
    provider: &str,
    config: &FailoverConfig,
    now: i64,
) -> Vec<FailoverCandidate> {
    auth.accounts_for_provider(provider)
        .into_iter()
        .filter_map(|account| {
            let resolved = auth_for_provider(auth, provider, Some(&account))?;
            let cooldown_until = account_cooldown(
                &account,
                usage_cache
                    .and_then(|cache| cache.accounts.get(&format!("{provider}:{account}")))
                    .map(|usage| usage.windows.as_slice()),
                &config.cooldowns,
                now,
            );
            Some(FailoverCandidate {
                account,
                auth: resolved,
                cooldown_until,
            })
        })
        .collect()
}

/// Cooldown deadline for one account. Explicit settings win; otherwise the
/// 5h usage window's reset time applies once usage crosses the exhaustion
/// threshold. The weekly window is deliberately ignored: a weekly-capped
/// account has no useful failover target, and benching it for days would
/// defeat the availability goal.
fn account_cooldown(
    account: &str,
    windows: Option<&[UsageWindow]>,
    overrides: &BTreeMap<String, i64>,
    now: i64,
) -> i64 {
    if let Some(explicit) = overrides.get(account) {
        return *explicit;
    }
    let Some(short_window) =
        windows.and_then(|windows| windows.iter().find(|window| window.label == "5h"))
    else {
        return 0;
    };
    if short_window.used_pct < EXHAUSTION_THRESHOLD_PCT {
        return 0;
    }
    short_window
        .resets_at
        .filter(|resets_at| *resets_at > now)
        .unwrap_or(0)
}

fn is_rate_limited(error: &ProviderError) -> bool {
    match error {
        ProviderError::Status { status, .. } => status.as_u16() == 429 || status.as_u16() == 529,
        _ => false,
    }
}

fn status_of(error: &ProviderError) -> u16 {
    match error {
        ProviderError::Status { status, .. } => status.as_u16(),
        _ => 0,
    }
}

/// Signals emitted by the wrapper so the CLI can surface account switches in
/// the transcript without the wrapper owning UI state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailoverEvent {
    /// Serving with this account; `switched` is true when it differs from
    /// the previously used account.
    Serving { account: String, switched: bool },
    /// An account returned an exhaustion signal and is now cooling down
    /// until the given epoch second.
    Cooldown {
        account: String,
        until: i64,
        status: u16,
    },
}

struct PoolState {
    /// Account that currently carries the session. Sticky across turns
    /// within the process.
    current: Option<String>,
    /// Monotonic milliseconds of the last account switch.
    last_switch_ms: AtomicU64,
    /// Account -> cooldown deadline in epoch seconds.
    cooldowns: BTreeMap<String, i64>,
}

/// Monotonic clock base for switch rate limiting. `Instant` cannot be
/// constructed in a const context, so it is lazily initialized once.
fn monotonic_base() -> Instant {
    static BASE: OnceLock<Instant> = OnceLock::new();
    *BASE.get_or_init(Instant::now)
}

fn monotonic_ms() -> u64 {
    monotonic_base().elapsed().as_millis() as u64
}

/// Process-wide failover memory. Providers are reconstructed every turn, so
/// stickiness (which account carried the session) and learned cooldowns must
/// live outside any single wrapper instance. Scoped to this process, which
/// is the session lifetime for `pi`.
#[derive(Default)]
struct FailoverMemory {
    /// provider -> account that last served a request.
    current: BTreeMap<String, String>,
    /// "provider:account" -> cooldown deadline (epoch seconds).
    cooldowns: BTreeMap<String, i64>,
    /// Human-readable notices (switches, cooldowns) for UI surfacing.
    notices: Vec<String>,
}

static MEMORY: Mutex<Option<FailoverMemory>> = Mutex::new(None);

fn with_memory<R>(f: impl FnOnce(&mut FailoverMemory) -> R) -> R {
    let mut guard = MEMORY.lock().expect("failover memory poisoned");
    let memory = guard.get_or_insert_with(FailoverMemory::default);
    f(memory)
}

/// Seed the wrapper with the process-level sticky account and cooldowns for
/// this provider. Called at build time.
pub fn remembered_state(provider: &str) -> (Option<String>, BTreeMap<String, i64>) {
    with_memory(|memory| {
        let cooldowns = memory
            .cooldowns
            .iter()
            .filter_map(|(key, until)| {
                key.strip_prefix(&format!("{provider}:"))
                    .map(|account| (account.to_string(), *until))
            })
            .collect();
        (memory.current.get(provider).cloned(), cooldowns)
    })
}

/// Notices generated since the last drain, oldest first. The UI drains these
/// after a turn to surface account switches without injecting text into the
/// assistant stream.
pub fn drain_notices() -> Vec<String> {
    with_memory(|memory| std::mem::take(&mut memory.notices))
}

pub struct FailoverProvider {
    /// Per-account constructed providers, in candidate order.
    providers: Vec<(String, Box<dyn Provider>)>,
    state: Mutex<PoolState>,
    min_switch_interval_ms: u64,
    on_event: Box<dyn Fn(FailoverEvent) + Send + Sync>,
}

impl std::fmt::Debug for FailoverProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FailoverProvider")
            .field(
                "accounts",
                &self.providers.iter().map(|(a, _)| a).collect::<Vec<_>>(),
            )
            .field("min_switch_interval_ms", &self.min_switch_interval_ms)
            .finish()
    }
}

pub struct FailoverProviderBuilder {
    accounts: Vec<(String, ProviderAuth)>,
    provider_name: String,
    initial_current: Option<String>,
    min_switch_interval_ms: u64,
    on_event: Option<Box<dyn Fn(FailoverEvent) + Send + Sync>>,
    cooldowns: BTreeMap<String, i64>,
}

impl FailoverProviderBuilder {
    pub fn new(provider_name: impl Into<String>) -> Self {
        Self {
            accounts: Vec::new(),
            provider_name: provider_name.into(),
            initial_current: None,
            min_switch_interval_ms: MIN_SWITCH_INTERVAL_MS,
            on_event: None,
            cooldowns: BTreeMap::new(),
        }
    }

    /// Seed the sticky account from process memory (previous turn's
    /// choice). Without this, per-turn provider reconstruction would reset
    /// stickiness to storage order.
    pub fn remember(mut self, account: Option<String>) -> Self {
        self.initial_current = account;
        self
    }

    pub fn account(mut self, account: impl Into<String>, auth: ProviderAuth) -> Self {
        self.accounts.push((account.into(), auth));
        self
    }

    pub fn min_switch_interval_ms(mut self, ms: u64) -> Self {
        self.min_switch_interval_ms = ms;
        self
    }

    #[cfg(test)]
    pub fn on_event(mut self, callback: impl Fn(FailoverEvent) + Send + Sync + 'static) -> Self {
        self.on_event = Some(Box::new(callback));
        self
    }

    pub fn cooldown(mut self, account: impl Into<String>, until: i64) -> Self {
        self.cooldowns.insert(account.into(), until);
        self
    }

    /// Build the wrapper. `factory` constructs the concrete provider for one
    /// account; the wrapper stays agnostic to provider APIs.
    pub fn build(
        self,
        mut factory: impl FnMut(ProviderAuth) -> Box<dyn Provider>,
    ) -> FailoverProvider {
        let providers = self
            .accounts
            .into_iter()
            .map(|(account, auth)| (account, factory(auth)))
            .collect();
        let provider_name = self.provider_name.clone();
        let on_event = self.on_event.unwrap_or_else(|| Box::new(|_| {}));
        FailoverProvider {
            providers,
            state: Mutex::new(PoolState {
                current: self.initial_current.clone(),
                last_switch_ms: AtomicU64::new(0),
                cooldowns: self.cooldowns,
            }),
            min_switch_interval_ms: self.min_switch_interval_ms,
            on_event: Box::new(move |event| {
                record_failover_event(&provider_name, &event);
                on_event(event);
            }),
        }
    }
}

/// Persist a wrapper event into process memory: sticky account, learned
/// cooldowns, and a UI notice.
fn record_failover_event(provider: &str, event: &FailoverEvent) {
    match event {
        FailoverEvent::Serving { account, .. } => {
            with_memory(|memory| {
                memory.current.insert(provider.to_string(), account.clone());
            });
        }
        FailoverEvent::Cooldown { account, until, .. } => {
            with_memory(|memory| {
                memory
                    .cooldowns
                    .insert(format!("{provider}:{account}"), *until);
                memory.notices.push(format!(
                    "account {account} hit its limit; cooling down until {}",
                    format_cooldown(*until)
                ));
            });
        }
    }
}

fn format_cooldown(until: i64) -> String {
    if until <= 0 {
        return "shortly".to_string();
    }
    let Ok(timestamp) = time::OffsetDateTime::from_unix_timestamp(until) else {
        return "shortly".to_string();
    };
    let Ok(format) = time::format_description::parse("[hour]:[minute] UTC") else {
        return "shortly".to_string();
    };
    timestamp
        .format(&format)
        .unwrap_or_else(|_| "shortly".to_string())
}

impl Default for FailoverProviderBuilder {
    fn default() -> Self {
        Self::new(String::new())
    }
}

impl FailoverProvider {
    /// Attempt one request against one account. Returns the error together
    /// with the number of events already streamed when it fails partway.
    async fn attempt(
        &self,
        index: usize,
        request: &ProviderRequest,
        on_event: &mut (dyn FnMut(StreamEvent) -> Result<(), ProviderError> + Send),
    ) -> Result<Vec<StreamEvent>, (ProviderError, usize)> {
        let provider = &self.providers[index].1;
        let mut events = Vec::new();
        let result = provider
            .complete_streaming(request.clone(), &mut |event: StreamEvent| {
                events.push(event.clone());
                on_event(event.clone())
            })
            .await;
        match result {
            Ok(()) => Ok(events),
            Err(error) => Err((error, events.len())),
        }
    }

    /// Choose the account for this request under the pool lock, applying
    /// cooldowns and the voluntary-switch rate limit. `forced` marks a
    /// switch forced by an exhaustion signal on the current account, which
    /// bypasses the rate limit (availability first, still no flapping
    /// because the failed account enters a cooldown).
    fn choose(&self, now: i64, forced: bool) -> Option<(usize, FailoverDecision)> {
        let mut state = self.state.lock().expect("failover pool poisoned");
        let candidates: Vec<FailoverCandidate> = self
            .providers
            .iter()
            .map(|(account, _)| FailoverCandidate {
                account: account.clone(),
                auth: ResolvedAuth::ApiKey(String::new()),
                cooldown_until: state.cooldowns.get(account).copied().unwrap_or(0),
            })
            .collect();
        let previous = state.current.clone();
        let decision = select_account(&candidates, previous.as_deref(), now)?;
        if decision.switched && !forced {
            let last = state.last_switch_ms.load(Ordering::SeqCst);
            if monotonic_ms().saturating_sub(last) < self.min_switch_interval_ms {
                // Rate-limited voluntary switch: keep serving with the
                // previous account if it is still in the pool.
                if let Some(index) = self
                    .providers
                    .iter()
                    .position(|(account, _)| Some(account.as_str()) == previous.as_deref())
                {
                    return Some((
                        index,
                        FailoverDecision {
                            account: previous.unwrap_or_default(),
                            switched: false,
                        },
                    ));
                }
            }
        }
        if decision.switched {
            state.last_switch_ms.store(monotonic_ms(), Ordering::SeqCst);
        }
        state.current = Some(decision.account.clone());
        let index = self
            .providers
            .iter()
            .position(|(account, _)| *account == decision.account)?;
        Some((index, decision))
    }

    /// Record an exhaustion signal for an account, apply the cooldown, and
    /// emit the corresponding event. Returns the cooldown deadline.
    fn cooldown_after_rate_limit(&self, account: &str, status: u16, now: i64) -> i64 {
        let until = now + DEFAULT_COOLDOWN_SECONDS;
        if let Ok(mut state) = self.state.lock() {
            // Never shorten an existing longer cooldown.
            let current = state.cooldowns.get(account).copied().unwrap_or(0);
            state
                .cooldowns
                .insert(account.to_string(), current.max(until));
        }
        (self.on_event)(FailoverEvent::Cooldown {
            account: account.to_string(),
            until,
            status,
        });
        until
    }
}

#[async_trait]
impl Provider for FailoverProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<Vec<StreamEvent>, ProviderError> {
        let mut collected = Vec::new();
        self.complete_streaming(request, &mut |event| {
            collected.push(event.clone());
            Ok(())
        })
        .await?;
        Ok(collected)
    }

    async fn complete_streaming(
        &self,
        request: ProviderRequest,
        on_event: &mut (dyn FnMut(StreamEvent) -> Result<(), ProviderError> + Send),
    ) -> Result<(), ProviderError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or(0);
        let mut forced = false;
        let mut errors: Vec<String> = Vec::new();
        for _ in 0..self.providers.len().max(1) {
            let Some((index, decision)) = self.choose(now, forced) else {
                if errors.is_empty() {
                    return Err(ProviderError::Config(
                        "no accounts available for failover".to_string(),
                    ));
                }
                break;
            };
            (self.on_event)(FailoverEvent::Serving {
                account: decision.account.clone(),
                switched: decision.switched,
            });
            match self.attempt(index, &request, on_event).await {
                Ok(_events) => return Ok(()),
                Err((error, streamed)) if streamed > 0 => {
                    // Output already delivered to the caller; rotating now
                    // would duplicate or corrupt the in-flight response.
                    return Err(error);
                }
                Err((error, _)) if is_rate_limited(&error) => {
                    let account = self.providers[index].0.clone();
                    self.cooldown_after_rate_limit(&account, status_of(&error), now);
                    errors.push(format!("{account}: {error}"));
                    forced = true;
                    continue;
                }
                Err((error, _)) => {
                    // Non-exhaustion failures do not rotate: auth, network,
                    // and server errors are either account-independent or
                    // belong to the outer retry loop.
                    return Err(error);
                }
            }
        }
        Err(ProviderError::Config(format!(
            "all accounts exhausted (rate limited): {}",
            errors.join("; ")
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_config::AccountUsage;
    use pi_config::AuthCredential;
    use std::sync::Arc;

    fn candidate(account: &str, cooldown_until: i64) -> FailoverCandidate {
        FailoverCandidate {
            account: account.to_string(),
            auth: ResolvedAuth::ApiKey("key".to_string()),
            cooldown_until,
        }
    }

    fn api_key_auth(account: &str) -> AuthData {
        let mut auth = AuthData::default();
        auth.insert(
            "anthropic",
            account,
            AuthCredential::ApiKey {
                key: format!("{account}-key"),
            },
        );
        auth
    }

    fn usage_cache(window_pct: f64, resets_at: i64) -> UsageCache {
        let mut cache = UsageCache::default();
        cache.accounts.insert(
            "anthropic:work".to_string(),
            AccountUsage {
                plan: None,
                windows: vec![UsageWindow {
                    label: "5h".to_string(),
                    used_pct: window_pct,
                    resets_at: Some(resets_at),
                }],
                balance: None,
                fetched_at: 0,
            },
        );
        cache
    }

    #[test]
    fn selection_is_sticky_when_previous_is_available() {
        let candidates = vec![candidate("a", 0), candidate("b", 0)];
        let decision = select_account(&candidates, Some("b"), 1_000).expect("decision");
        assert_eq!(decision.account, "b");
        assert!(!decision.switched);
    }

    #[test]
    fn selection_moves_to_storage_order_when_previous_cooling() {
        let candidates = vec![candidate("a", 0), candidate("b", 5_000)];
        let decision = select_account(&candidates, Some("b"), 1_000).expect("decision");
        assert_eq!(decision.account, "a");
        assert!(decision.switched);
    }

    #[test]
    fn selection_returns_none_when_all_cooling() {
        let candidates = vec![candidate("a", 5_000), candidate("b", 5_000)];
        assert!(select_account(&candidates, Some("a"), 1_000).is_none());
    }

    #[test]
    fn selection_without_previous_uses_storage_order() {
        let candidates = vec![candidate("a", 0), candidate("b", 0), candidate("c", 0)];
        let decision = select_account(&candidates, None, 1_000).expect("decision");
        assert_eq!(decision.account, "a");
        assert!(!decision.switched);
    }

    #[test]
    fn candidates_come_from_stored_auth_in_storage_order() {
        let mut auth = AuthData::default();
        auth.insert(
            "anthropic",
            "work",
            AuthCredential::ApiKey {
                key: "work-key".to_string(),
            },
        );
        auth.insert(
            "anthropic",
            "personal",
            AuthCredential::ApiKey {
                key: "personal-key".to_string(),
            },
        );
        let candidates =
            failover_candidates(&auth, None, "anthropic", &FailoverConfig::default(), 1_000);
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.account.as_str())
                .collect::<Vec<_>>(),
            vec!["personal", "work"]
        );
    }

    #[test]
    fn cooldown_uses_five_hour_window_reset_time() {
        let auth = api_key_auth("work");
        let candidates = failover_candidates(
            &auth,
            Some(&usage_cache(95.0, 9_000)),
            "anthropic",
            &FailoverConfig::default(),
            1_000,
        );
        assert_eq!(candidates[0].cooldown_until, 9_000);
    }

    #[test]
    fn cooldown_zero_when_usage_below_threshold() {
        let auth = api_key_auth("work");
        let candidates = failover_candidates(
            &auth,
            Some(&usage_cache(42.0, 9_000)),
            "anthropic",
            &FailoverConfig::default(),
            1_000,
        );
        assert_eq!(candidates[0].cooldown_until, 0);
    }

    #[test]
    fn cooldown_zero_when_reset_time_already_passed() {
        let auth = api_key_auth("work");
        let candidates = failover_candidates(
            &auth,
            Some(&usage_cache(95.0, 500)),
            "anthropic",
            &FailoverConfig::default(),
            1_000,
        );
        assert_eq!(candidates[0].cooldown_until, 0);
    }

    #[test]
    fn explicit_cooldown_overrides_usage_cache() {
        let auth = api_key_auth("work");
        let config = FailoverConfig {
            cooldowns: BTreeMap::from([("work".to_string(), 7_000)]),
            ..FailoverConfig::default()
        };
        let candidates = failover_candidates(
            &auth,
            Some(&usage_cache(10.0, 2_000)),
            "anthropic",
            &config,
            1_000,
        );
        assert_eq!(candidates[0].cooldown_until, 7_000);
    }

    /// Fails with the given statuses in order, then streams "ok".
    struct FlakyProvider {
        statuses: Mutex<Vec<u16>>,
    }

    #[async_trait]
    impl Provider for FlakyProvider {
        async fn complete(
            &self,
            _request: ProviderRequest,
        ) -> Result<Vec<StreamEvent>, ProviderError> {
            let mut collected = Vec::new();
            self.complete_streaming(_request, &mut |event| {
                collected.push(event.clone());
                Ok(())
            })
            .await?;
            Ok(collected)
        }

        async fn complete_streaming(
            &self,
            _request: ProviderRequest,
            on_event: &mut (dyn FnMut(StreamEvent) -> Result<(), ProviderError> + Send),
        ) -> Result<(), ProviderError> {
            let mut statuses = self.statuses.lock().expect("statuses");
            if let Some(status) = statuses.first().copied() {
                statuses.remove(0);
                return Err(ProviderError::Status {
                    status: reqwest::StatusCode::from_u16(status).expect("status code"),
                    body: "rate limited".to_string(),
                });
            }
            on_event(StreamEvent::Text("ok".to_string()))?;
            Ok(())
        }
    }

    struct OkProvider;

    #[async_trait]
    impl Provider for OkProvider {
        async fn complete(
            &self,
            _request: ProviderRequest,
        ) -> Result<Vec<StreamEvent>, ProviderError> {
            Ok(vec![StreamEvent::Text("ok".to_string())])
        }

        async fn complete_streaming(
            &self,
            _request: ProviderRequest,
            on_event: &mut (dyn FnMut(StreamEvent) -> Result<(), ProviderError> + Send),
        ) -> Result<(), ProviderError> {
            on_event(StreamEvent::Text("ok".to_string()))?;
            Ok(())
        }
    }

    struct PartialStreamProvider;

    #[async_trait]
    impl Provider for PartialStreamProvider {
        async fn complete(
            &self,
            _request: ProviderRequest,
        ) -> Result<Vec<StreamEvent>, ProviderError> {
            unimplemented!("streaming path is the product path")
        }

        async fn complete_streaming(
            &self,
            _request: ProviderRequest,
            on_event: &mut (dyn FnMut(StreamEvent) -> Result<(), ProviderError> + Send),
        ) -> Result<(), ProviderError> {
            on_event(StreamEvent::Text("partial".to_string()))?;
            Err(ProviderError::Status {
                status: reqwest::StatusCode::from_u16(429).expect("status"),
                body: "mid-stream".to_string(),
            })
        }
    }

    struct AuthErrorProvider;

    #[async_trait]
    impl Provider for AuthErrorProvider {
        async fn complete(
            &self,
            _request: ProviderRequest,
        ) -> Result<Vec<StreamEvent>, ProviderError> {
            Err(ProviderError::MissingApiKey {
                provider: "anthropic".to_string(),
            })
        }

        async fn complete_streaming(
            &self,
            _request: ProviderRequest,
            _on_event: &mut (dyn FnMut(StreamEvent) -> Result<(), ProviderError> + Send),
        ) -> Result<(), ProviderError> {
            Err(ProviderError::MissingApiKey {
                provider: "anthropic".to_string(),
            })
        }
    }

    fn test_request() -> ProviderRequest {
        ProviderRequest {
            max_output_tokens: None,
            system_prompt: None,
            messages: Vec::new(),
            tools: Vec::new(),
        }
    }

    #[tokio::test]
    async fn wrapper_rotates_on_rate_limit_with_zero_streamed_output() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let seen = events.clone();
        // First constructed account (storage order "a") fails once with 429;
        // the second ("b") succeeds.
        let mut first = true;
        let provider = FailoverProviderBuilder::new("anthropic")
            .account("a", ProviderAuth::ApiKey("a".to_string()))
            .account("b", ProviderAuth::ApiKey("b".to_string()))
            .on_event(move |event| {
                seen.lock().expect("events").push(match event {
                    FailoverEvent::Serving { account, switched } => {
                        if switched {
                            format!("switch:{account}")
                        } else {
                            format!("serving:{account}")
                        }
                    }
                    FailoverEvent::Cooldown { account, .. } => format!("cooldown:{account}"),
                });
            })
            .build(move |_| {
                let provider: Box<dyn Provider> = if first {
                    first = false;
                    Box::new(FlakyProvider {
                        statuses: Mutex::new(vec![429]),
                    })
                } else {
                    Box::new(OkProvider)
                };
                provider
            });
        let mut collected = Vec::new();
        provider
            .complete_streaming(test_request(), &mut |event| {
                collected.push(event.clone());
                Ok(())
            })
            .await
            .expect("request should succeed after failover");
        assert_eq!(collected.len(), 1);
        let events = events.lock().expect("events").clone();
        assert!(events.contains(&"cooldown:a".to_string()), "{events:?}");
        assert!(events.contains(&"serving:a".to_string()), "{events:?}");
        assert!(events.contains(&"switch:b".to_string()), "{events:?}");
    }

    #[tokio::test]
    async fn wrapper_is_sticky_across_requests() {
        let served = Arc::new(Mutex::new(Vec::new()));
        let seen = served.clone();
        let provider = FailoverProviderBuilder::new("anthropic")
            .account("a", ProviderAuth::ApiKey("a".to_string()))
            .account("b", ProviderAuth::ApiKey("b".to_string()))
            .on_event(move |event| {
                if let FailoverEvent::Serving { account, .. } = event {
                    seen.lock().expect("served").push(account);
                }
            })
            .build(|_| Box::new(OkProvider));
        for _ in 0..3 {
            provider
                .complete_streaming(test_request(), &mut |_| Ok(()))
                .await
                .expect("request succeeds");
        }
        let served = served.lock().expect("served").clone();
        assert_eq!(served, vec!["a", "a", "a"]);
    }

    #[tokio::test]
    async fn wrapper_reports_error_when_all_accounts_rate_limited() {
        let provider = FailoverProviderBuilder::new("anthropic")
            .account("a", ProviderAuth::ApiKey("a".to_string()))
            .account("b", ProviderAuth::ApiKey("b".to_string()))
            .build(|_| {
                Box::new(FlakyProvider {
                    statuses: Mutex::new(vec![429, 429, 429, 429]),
                })
            });
        let error = provider
            .complete_streaming(test_request(), &mut |_| Ok(()))
            .await
            .expect_err("all accounts exhausted");
        let message = error.to_string();
        assert!(message.contains("all accounts exhausted"), "{message}");
    }

    #[tokio::test]
    async fn wrapper_does_not_rotate_after_partial_stream() {
        let provider = FailoverProviderBuilder::new("anthropic")
            .account("a", ProviderAuth::ApiKey("a".to_string()))
            .account("b", ProviderAuth::ApiKey("b".to_string()))
            .build(|_| Box::new(PartialStreamProvider));
        let mut collected = Vec::new();
        let error = provider
            .complete_streaming(test_request(), &mut |event| {
                collected.push(event.clone());
                Ok(())
            })
            .await
            .expect_err("mid-stream failures must not rotate");
        assert!(matches!(error, ProviderError::Status { .. }));
        assert_eq!(collected.len(), 1);
    }

    #[tokio::test]
    async fn wrapper_does_not_rotate_on_non_rate_limit_errors() {
        let provider = FailoverProviderBuilder::new("anthropic")
            .account("a", ProviderAuth::ApiKey("a".to_string()))
            .account("b", ProviderAuth::ApiKey("b".to_string()))
            .build(|_| Box::new(AuthErrorProvider));
        let error = provider
            .complete_streaming(test_request(), &mut |_| Ok(()))
            .await
            .expect_err("auth errors must not rotate");
        assert!(matches!(error, ProviderError::MissingApiKey { .. }));
    }

    #[tokio::test]
    async fn wrapper_honors_preseeded_cooldown_from_usage_cache() {
        // "a" is cooling down from a saturated 5h window; the first request
        // must be served by "b" without any 429 round trip on "a".
        let served = Arc::new(Mutex::new(Vec::new()));
        let seen = served.clone();
        let provider = FailoverProviderBuilder::new("anthropic")
            .account("a", ProviderAuth::ApiKey("a".to_string()))
            .account("b", ProviderAuth::ApiKey("b".to_string()))
            .cooldown("a", i64::MAX / 2)
            .on_event(move |event| {
                if let FailoverEvent::Serving { account, .. } = event {
                    seen.lock().expect("served").push(account);
                }
            })
            .build(|_| Box::new(OkProvider));
        provider
            .complete_streaming(test_request(), &mut |_| Ok(()))
            .await
            .expect("request succeeds on b");
        let served = served.lock().expect("served").clone();
        assert_eq!(served, vec!["b"]);
    }
}
