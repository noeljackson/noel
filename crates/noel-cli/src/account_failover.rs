//! Failover-only multi-account provider wrapper.
//!
//! Intent: availability, not throughput. One "sticky" account carries the
//! session; other stored accounts are used only when the sticky one is rate
//! limited (HTTP 429 with no streamed output) or its 5h usage window is at
//! the exhaustion threshold. A rate limit lasts until the provider says it
//! resets, and a switch away from an account near its usage limit waits out
//! a minimum interval after the last switch, so the pool never flaps between
//! accounts. This deliberately does not load-balance, rebalance to the
//! lowest-usage account, or spread requests across the pool.
//!
//! Every request builds its own pool. What pools learn (the sticky account,
//! rate limits, the last switch) is saved in `failover-state.json` in the
//! agent directory, so it holds across requests and across processes.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use noel_ai::{Provider, ProviderAuth, ProviderError, ProviderRequest, StreamEvent};
use noel_config::{auth_for_provider, AuthData, UsageCache, UsageWindow};
use serde::{Deserialize, Serialize};

/// Minimum time between voluntary account switches, in milliseconds: those
/// away from an account whose usage window is at the threshold. A switch
/// away from a rate-limited account is never held back.
pub const MIN_SWITCH_INTERVAL_MS: u64 = 10_000;

/// Usage at or above this percentage of the 5h window sets an account aside
/// until the window resets, unless a provider's `exhaustionThresholdPct`
/// says otherwise.
pub const EXHAUSTION_THRESHOLD_PCT: f64 = 90.0;

/// How long a rate-limited account sits out when the provider's answer says
/// nothing about when it may ask again.
pub const DEFAULT_COOLDOWN_SECONDS: i64 = 300;

/// The pools' saved state, in the agent directory.
pub const STATE_FILE: &str = "failover-state.json";

/// A stored account that can join a provider's pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailoverCandidate {
    pub account: String,
    /// Until when its 5h usage window is at the exhaustion threshold, in
    /// epoch seconds; 0 when it is not.
    pub exhausted_until: i64,
}

/// The stored accounts of `provider` that have a credential, in storage
/// order (the sorted key order of `auth.json`), with what the usage cache
/// says of their 5h windows.
///
/// Env and imported-file accounts are excluded: they are not stored under an
/// account name, so they cannot participate in named rotation, and mixing
/// them in would produce ambiguous resolution.
pub fn failover_candidates(
    auth: &AuthData,
    usage_cache: Option<&UsageCache>,
    provider: &str,
    threshold_pct: f64,
    now: i64,
) -> Vec<FailoverCandidate> {
    auth.accounts_for_provider(provider)
        .into_iter()
        .filter(|account| auth_for_provider(auth, provider, Some(account)).is_some())
        .map(|account| {
            let windows = usage_cache
                .and_then(|cache| cache.accounts.get(&format!("{provider}:{account}")))
                .map(|usage| usage.windows.as_slice());
            FailoverCandidate {
                exhausted_until: exhausted_until(windows, threshold_pct, now),
                account,
            }
        })
        .collect()
}

/// When the 5h usage window resets, once its usage is at `threshold_pct` or
/// above. The weekly window is deliberately ignored: a weekly-capped account
/// has no useful failover target, and setting it aside for days would defeat
/// the availability goal.
fn exhausted_until(windows: Option<&[UsageWindow]>, threshold_pct: f64, now: i64) -> i64 {
    let Some(short_window) =
        windows.and_then(|windows| windows.iter().find(|window| window.label == "5h"))
    else {
        return 0;
    };
    if short_window.used_pct < threshold_pct {
        return 0;
    }
    short_window
        .resets_at
        .filter(|resets_at| *resets_at > now)
        .unwrap_or(0)
}

/// Where an account stands for the next request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Standing<'a> {
    pub account: &'a str,
    /// Until when the provider refuses it, in epoch seconds.
    pub limited_until: i64,
    /// Until when its 5h usage window is at the exhaustion threshold.
    pub exhausted_until: i64,
}

/// Pick the account that should serve the next request.
///
/// Sticky by design: `previous` wins unless it is set aside, and then the
/// first account in storage order that is not. When every account the
/// provider still takes is at its usage threshold, the sticky one goes on
/// serving: such an account still answers. `None` when the provider refuses
/// every account.
pub fn select_account<'a>(
    accounts: &[Standing<'a>],
    previous: Option<&str>,
    now: i64,
) -> Option<&'a str> {
    let sticky = accounts
        .iter()
        .filter(|standing| Some(standing.account) == previous);
    let rest = accounts
        .iter()
        .filter(|standing| Some(standing.account) != previous);
    let ordered = sticky.chain(rest);
    let taken = |standing: &&Standing| standing.limited_until <= now;
    ordered
        .clone()
        .find(|standing| taken(standing) && standing.exhausted_until <= now)
        .or_else(|| ordered.clone().find(taken))
        .map(|standing| standing.account)
}

/// Only a rate limit moves a request to another account. A 529 says the
/// provider is overloaded for everyone, so it stays the run loop's to retry.
fn is_rate_limited(error: &ProviderError) -> bool {
    matches!(error, ProviderError::Status { status, .. } if status.as_u16() == 429)
}

/// When the provider says a rate-limited account may ask again, in epoch
/// seconds: the wait its answer asks for, or the `error.resets_at` an
/// `openai-codex` usage limit carries, whichever is later.
fn limit_resets_at(error: &ProviderError, now: i64) -> Option<i64> {
    let ProviderError::Status {
        body, retry_after, ..
    } = error
    else {
        return None;
    };
    let wait = retry_after.map(|wait| now.saturating_add(wait.as_secs_f64().ceil() as i64));
    let resets_at = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|body| body.pointer("/error/resets_at")?.as_i64());
    wait.into_iter().chain(resets_at).max()
}

/// What `failover-state.json` holds for one provider.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedProvider {
    /// The account that carries the provider's requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    /// When a pool last switched accounts, in epoch milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_switch_ms: Option<u64>,
    /// Rate-limited accounts, and the epoch second each may ask again.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub limited_until: BTreeMap<String, i64>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SavedState {
    #[serde(default)]
    providers: BTreeMap<String, SavedProvider>,
}

fn read_state(path: &Path) -> SavedState {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// What the pools of `provider` saved in `path`; nothing when the file is
/// missing or unreadable.
pub fn saved_state(path: &Path, provider: &str) -> SavedProvider {
    read_state(path)
        .providers
        .remove(provider)
        .unwrap_or_default()
}

/// Applies `change` to what `path` holds for `provider`, drops rate limits
/// that have ended, and writes the file back. Processes write it without a
/// lock: one that writes between another's read and write undoes that
/// change, which costs at most one request to an account the provider then
/// refuses again.
fn save_state(path: &Path, provider: &str, now: i64, change: impl FnOnce(&mut SavedProvider)) {
    let mut state = read_state(path);
    change(state.providers.entry(provider.to_string()).or_default());
    for saved in state.providers.values_mut() {
        saved.limited_until.retain(|_, until| *until > now);
    }
    let mut bytes = serde_json::to_vec_pretty(&state).expect("failover state serializes");
    bytes.push(b'\n');
    let parent = path.parent().unwrap_or(Path::new(""));
    let written = noel_config::create_private_dir_all(parent)
        .and_then(|()| noel_config::write_file_atomic(path, &bytes));
    if let Err(error) = written {
        notice_once(
            format!("save {}", path.display()),
            format!("account failover could not save its state: {error}"),
        );
    }
}

/// Notices for the person running noel, which each mode shows its way: the
/// TUI after the turn, print mode on stderr, RPC as a notification.
struct Notices {
    pending: Vec<String>,
    /// Keys of the notices given once per process.
    given: BTreeSet<String>,
    /// Every notice, for tests: they run in parallel, and a drain takes
    /// every test's notices.
    #[cfg(test)]
    log: Vec<String>,
}

static NOTICES: Mutex<Notices> = Mutex::new(Notices {
    pending: Vec::new(),
    given: BTreeSet::new(),
    #[cfg(test)]
    log: Vec::new(),
});

fn notices() -> std::sync::MutexGuard<'static, Notices> {
    NOTICES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Notices {
    fn give(&mut self, text: String) {
        #[cfg(test)]
        self.log.push(text.clone());
        self.pending.push(text);
    }
}

fn notice(text: String) {
    notices().give(text);
}

fn notice_once(key: String, text: String) {
    let mut notices = notices();
    if notices.given.insert(key) {
        notices.give(text);
    }
}

/// Notices given since the last drain, oldest first.
pub fn drain_notices() -> Vec<String> {
    std::mem::take(&mut notices().pending)
}

/// Every notice this process gave that contains `text`.
#[cfg(test)]
pub fn notices_given(text: &str) -> Vec<String> {
    notices()
        .log
        .iter()
        .filter(|notice| notice.contains(text))
        .cloned()
        .collect()
}

/// Says once per process that an account is left out of the pool, and why.
pub fn report_left_out(provider: &str, account: &str, reason: &str) {
    notice_once(
        format!("left out {provider}:{account}"),
        format!("{provider} account {account} is left out of failover: {reason}"),
    );
}

/// `until` (epoch seconds) as a UTC time, with the date unless it is within
/// a day of `now`.
fn format_time(until: i64, now: i64) -> String {
    let description = if until - now < 20 * 60 * 60 {
        "[hour]:[minute] UTC"
    } else {
        "[year]-[month]-[day] [hour]:[minute] UTC"
    };
    time::OffsetDateTime::from_unix_timestamp(until)
        .ok()
        .zip(time::format_description::parse(description).ok())
        .and_then(|(time, format)| time.format(&format).ok())
        .unwrap_or_else(|| format!("{until} (epoch seconds)"))
}

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// Signals for tests to observe the wrapper with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailoverEvent {
    /// Serving with this account; `switched_from` names the sticky account
    /// it replaced.
    Serving {
        account: String,
        switched_from: Option<String>,
    },
    /// The provider refused an account until the given epoch second.
    Limited { account: String, until: i64 },
}

/// What one pool knows, from the saved state and the usage cache.
struct PoolState {
    current: Option<String>,
    last_switch_ms: Option<u64>,
    limited_until: BTreeMap<String, i64>,
    exhausted_until: BTreeMap<String, i64>,
}

impl PoolState {
    fn limited_until(&self, account: &str) -> i64 {
        self.limited_until.get(account).copied().unwrap_or(0)
    }

    fn exhausted_until(&self, account: &str) -> i64 {
        self.exhausted_until.get(account).copied().unwrap_or(0)
    }
}

/// The account a request goes to.
struct Choice {
    index: usize,
    switched_from: Option<String>,
    /// Whether the sticky account changed, so the saved state must.
    sticky_changed: bool,
}

pub struct FailoverProvider {
    provider_name: String,
    /// Per-account constructed providers, in candidate order.
    providers: Vec<(String, Box<dyn Provider>)>,
    state: Mutex<PoolState>,
    min_switch_interval_ms: u64,
    /// Where the state is saved; none in tests of the pool alone.
    state_path: Option<PathBuf>,
    on_event: Box<dyn Fn(FailoverEvent) + Send + Sync>,
}

impl std::fmt::Debug for FailoverProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FailoverProvider")
            .field("provider", &self.provider_name)
            .field(
                "accounts",
                &self.providers.iter().map(|(a, _)| a).collect::<Vec<_>>(),
            )
            .field("min_switch_interval_ms", &self.min_switch_interval_ms)
            .finish()
    }
}

pub struct FailoverProviderBuilder {
    provider_name: String,
    accounts: Vec<(String, ProviderAuth)>,
    exhausted_until: BTreeMap<String, i64>,
    min_switch_interval_ms: u64,
    state_path: Option<PathBuf>,
    on_event: Option<Box<dyn Fn(FailoverEvent) + Send + Sync>>,
}

impl FailoverProviderBuilder {
    pub fn new(provider_name: impl Into<String>) -> Self {
        Self {
            provider_name: provider_name.into(),
            accounts: Vec::new(),
            exhausted_until: BTreeMap::new(),
            min_switch_interval_ms: MIN_SWITCH_INTERVAL_MS,
            state_path: None,
            on_event: None,
        }
    }

    pub fn account(mut self, account: impl Into<String>, auth: ProviderAuth) -> Self {
        self.accounts.push((account.into(), auth));
        self
    }

    /// The account's 5h usage window is at the threshold until `until`.
    pub fn exhausted_until(mut self, account: impl Into<String>, until: i64) -> Self {
        self.exhausted_until.insert(account.into(), until);
        self
    }

    pub fn min_switch_interval_ms(mut self, ms: u64) -> Self {
        self.min_switch_interval_ms = ms;
        self
    }

    /// Start from the state saved in `path`, and save what changes there.
    pub fn saved_in(mut self, path: PathBuf) -> Self {
        self.state_path = Some(path);
        self
    }

    #[cfg(test)]
    pub fn on_event(mut self, callback: impl Fn(FailoverEvent) + Send + Sync + 'static) -> Self {
        self.on_event = Some(Box::new(callback));
        self
    }

    /// Build the wrapper. `factory` constructs the concrete provider for one
    /// account; the wrapper stays agnostic to provider APIs.
    pub fn build(
        self,
        mut factory: impl FnMut(ProviderAuth) -> Box<dyn Provider>,
    ) -> FailoverProvider {
        let saved = self
            .state_path
            .as_deref()
            .map(|path| saved_state(path, &self.provider_name))
            .unwrap_or_default();
        FailoverProvider {
            providers: self
                .accounts
                .into_iter()
                .map(|(account, auth)| (account, factory(auth)))
                .collect(),
            state: Mutex::new(PoolState {
                current: saved.current,
                last_switch_ms: saved.last_switch_ms,
                limited_until: saved.limited_until,
                exhausted_until: self.exhausted_until,
            }),
            provider_name: self.provider_name,
            min_switch_interval_ms: self.min_switch_interval_ms,
            state_path: self.state_path,
            on_event: self.on_event.unwrap_or_else(|| Box::new(|_| {})),
        }
    }
}

impl FailoverProvider {
    fn lock(&self) -> std::sync::MutexGuard<'_, PoolState> {
        self.state.lock().expect("failover pool poisoned")
    }

    fn save(&self, now: i64, change: impl FnOnce(&mut SavedProvider)) {
        if let Some(path) = &self.state_path {
            save_state(path, &self.provider_name, now, change);
        }
    }

    /// Choose the account for this request. A switch away from a sticky
    /// account the provider refuses is forced. One away from a sticky
    /// account at its usage threshold is voluntary, and waits until
    /// `min_switch_interval_ms` has passed since the last switch.
    fn choose(&self, now_ms: u64) -> Option<Choice> {
        let now = (now_ms / 1000) as i64;
        let mut state = self.lock();
        let accounts = self
            .providers
            .iter()
            .map(|(account, _)| Standing {
                account: account.as_str(),
                limited_until: state.limited_until(account),
                exhausted_until: state.exhausted_until(account),
            })
            .collect::<Vec<_>>();
        let previous = state
            .current
            .as_deref()
            .and_then(|current| accounts.iter().find(|standing| standing.account == current));
        let mut pick = select_account(&accounts, previous.map(|standing| standing.account), now)?;
        if let Some(previous) = previous.filter(|previous| previous.account != pick) {
            let voluntary = previous.limited_until <= now;
            let recent = state
                .last_switch_ms
                .is_some_and(|last| now_ms.saturating_sub(last) < self.min_switch_interval_ms);
            if voluntary && recent {
                pick = previous.account;
            }
        }
        let switched_from = previous
            .filter(|previous| previous.account != pick)
            .map(|previous| previous.account.to_string());
        if switched_from.is_some() {
            state.last_switch_ms = Some(now_ms);
        }
        let sticky_changed = state.current.as_deref() != Some(pick);
        state.current = Some(pick.to_string());
        let index = self.providers.iter().position(|(name, _)| name == pick)?;
        Some(Choice {
            index,
            switched_from,
            sticky_changed,
        })
    }

    /// Sets an account aside until the provider says it may ask again, and
    /// never for less than it already is.
    fn limit(&self, account: &str, error: &ProviderError, now: i64) {
        let until = limit_resets_at(error, now)
            .unwrap_or(now + DEFAULT_COOLDOWN_SECONDS)
            .max(now + 1);
        let until = {
            let mut state = self.lock();
            let until = state.limited_until(account).max(until);
            state.limited_until.insert(account.to_string(), until);
            until
        };
        self.save(now, |saved| {
            let saved_until = saved.limited_until.entry(account.to_string()).or_default();
            *saved_until = (*saved_until).max(until);
        });
        (self.on_event)(FailoverEvent::Limited {
            account: account.to_string(),
            until,
        });
    }

    /// Says why the pool moved off `from` and where to.
    fn report_switch(&self, from: &str, to: &str, now: i64) {
        let state = self.lock();
        let (why, until) = match state.limited_until(from) {
            until if until > now => ("is rate limited", until),
            _ => ("is near its 5h usage limit", state.exhausted_until(from)),
        };
        drop(state);
        notice(format!(
            "{} account {from} {why} until {}; switched to {to}",
            self.provider_name,
            format_time(until, now)
        ));
    }

    /// The error when no account is left for a request: when the first
    /// account may ask again, and each account's answer this request got.
    fn exhausted(&self, now: i64, answers: &[String]) -> ProviderError {
        let first_reset = {
            let state = self.lock();
            self.providers
                .iter()
                .map(|(account, _)| state.limited_until(account))
                .filter(|until| *until > now)
                .min()
        };
        let mut message = match first_reset {
            Some(until) => format!(
                "every {} account is rate limited; the first may be asked again at {}",
                self.provider_name,
                format_time(until, now)
            ),
            None => format!("no {} account is available", self.provider_name),
        };
        if !answers.is_empty() {
            message.push_str(": ");
            message.push_str(&answers.join("; "));
        }
        ProviderError::Config(message)
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
        let now_ms = epoch_ms();
        let now = (now_ms / 1000) as i64;
        let mut answers: Vec<String> = Vec::new();
        for _ in 0..self.providers.len() {
            let Some(choice) = self.choose(now_ms) else {
                break;
            };
            let (account, provider) = &self.providers[choice.index];
            if choice.sticky_changed {
                self.save(now, |saved| {
                    saved.current = Some(account.clone());
                    if choice.switched_from.is_some() {
                        saved.last_switch_ms = Some(now_ms);
                    }
                });
            }
            if let Some(from) = &choice.switched_from {
                self.report_switch(from, account, now);
            }
            (self.on_event)(FailoverEvent::Serving {
                account: account.clone(),
                switched_from: choice.switched_from,
            });
            let mut streamed = false;
            let result = provider
                .complete_streaming(request.clone(), &mut |event: StreamEvent| {
                    streamed = true;
                    on_event(event)
                })
                .await;
            match result {
                Ok(()) => return Ok(()),
                // Output already reached the caller; another account would
                // duplicate or corrupt the reply in flight.
                Err(error) if streamed => return Err(error),
                Err(error) if is_rate_limited(&error) => {
                    self.limit(account, &error, now);
                    answers.push(format!("{account}: {error}"));
                }
                // Anything else is either the same on every account (auth,
                // network, an overloaded provider) or the run loop's to
                // retry.
                Err(error) => return Err(error),
            }
        }
        Err(self.exhausted(now, &answers))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use noel_config::AccountUsage;
    use noel_config::AuthCredential;
    use std::sync::Arc;
    use std::time::Duration;

    fn standing(account: &str, limited_until: i64, exhausted_until: i64) -> Standing<'_> {
        Standing {
            account,
            limited_until,
            exhausted_until,
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
        let accounts = [standing("a", 0, 0), standing("b", 0, 0)];
        assert_eq!(select_account(&accounts, Some("b"), 1_000), Some("b"));
    }

    #[test]
    fn selection_moves_to_storage_order_when_previous_is_set_aside() {
        for previous in [standing("b", 5_000, 0), standing("b", 0, 5_000)] {
            let accounts = [standing("a", 0, 0), previous];
            assert_eq!(select_account(&accounts, Some("b"), 1_000), Some("a"));
        }
    }

    #[test]
    fn selection_returns_none_when_every_account_is_rate_limited() {
        let accounts = [standing("a", 5_000, 0), standing("b", 5_000, 0)];
        assert_eq!(select_account(&accounts, Some("a"), 1_000), None);
    }

    #[test]
    fn selection_without_previous_uses_storage_order() {
        let accounts = [
            standing("a", 0, 0),
            standing("b", 0, 0),
            standing("c", 0, 0),
        ];
        assert_eq!(select_account(&accounts, None, 1_000), Some("a"));
    }

    /// An account at its usage threshold still answers: when no other is
    /// free it serves, the sticky one first, and a rate-limited one never.
    #[test]
    fn selection_falls_back_to_an_account_at_its_usage_threshold() {
        let accounts = [
            standing("a", 0, 5_000),
            standing("b", 0, 5_000),
            standing("c", 5_000, 0),
        ];
        assert_eq!(select_account(&accounts, Some("b"), 1_000), Some("b"));
        assert_eq!(select_account(&accounts, Some("c"), 1_000), Some("a"));
        assert_eq!(select_account(&accounts, None, 1_000), Some("a"));
    }

    #[test]
    fn candidates_come_from_stored_auth_in_storage_order() {
        let mut auth = api_key_auth("work");
        auth.insert(
            "anthropic",
            "personal",
            AuthCredential::ApiKey {
                key: "personal-key".to_string(),
            },
        );
        let candidates = failover_candidates(&auth, None, "anthropic", 90.0, 1_000);
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.account.as_str())
                .collect::<Vec<_>>(),
            vec!["personal", "work"]
        );
    }

    #[test]
    fn the_5h_window_sets_an_account_aside_from_the_threshold_until_it_resets() {
        let auth = api_key_auth("work");
        let exhausted = |pct: f64, resets_at: i64, threshold: f64| {
            failover_candidates(
                &auth,
                Some(&usage_cache(pct, resets_at)),
                "anthropic",
                threshold,
                1_000,
            )[0]
            .exhausted_until
        };
        assert_eq!(exhausted(95.0, 9_000, 90.0), 9_000);
        assert_eq!(exhausted(90.0, 9_000, 90.0), 9_000);
        assert_eq!(exhausted(42.0, 9_000, 90.0), 0);
        // A provider's own threshold.
        assert_eq!(exhausted(85.0, 9_000, 80.0), 9_000);
        assert_eq!(exhausted(85.0, 9_000, 90.0), 0);
        // A window that already reset.
        assert_eq!(exhausted(95.0, 500, 90.0), 0);
    }

    fn status(status: u16, body: &str, retry_after: Option<Duration>) -> ProviderError {
        ProviderError::Status {
            status: reqwest::StatusCode::from_u16(status).expect("status code"),
            body: body.to_string(),
            retry_after,
        }
    }

    #[test]
    fn a_rate_limit_lasts_until_the_provider_says_it_resets() {
        let now = 1_000;
        let wait = |seconds| Some(Duration::from_secs_f64(seconds));
        assert_eq!(
            limit_resets_at(&status(429, "", wait(30.0)), now),
            Some(1_030)
        );
        assert_eq!(
            limit_resets_at(&status(429, "", wait(0.2)), now),
            Some(1_001)
        );
        let codex = r#"{"error":{"type":"usage_limit_reached","resets_at":9000}}"#;
        assert_eq!(limit_resets_at(&status(429, codex, None), now), Some(9_000));
        // The later of the two.
        assert_eq!(
            limit_resets_at(&status(429, codex, wait(30.0)), now),
            Some(9_000)
        );
        assert_eq!(limit_resets_at(&status(429, "slow down", None), now), None);
    }

    /// Fails with the given statuses in order, then streams "ok".
    struct FlakyProvider {
        statuses: Mutex<Vec<(u16, Option<Duration>)>>,
    }

    impl FlakyProvider {
        fn new(statuses: &[u16]) -> Self {
            Self {
                statuses: Mutex::new(statuses.iter().map(|status| (*status, None)).collect()),
            }
        }
    }

    #[async_trait]
    impl Provider for FlakyProvider {
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
            let next = {
                let mut statuses = self.statuses.lock().expect("statuses");
                (!statuses.is_empty()).then(|| statuses.remove(0))
            };
            if let Some((code, retry_after)) = next {
                return Err(status(code, "rate limited", retry_after));
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
            Err(status(429, "mid-stream", None))
        }
    }

    struct AuthErrorProvider;

    #[async_trait]
    impl Provider for AuthErrorProvider {
        async fn complete(
            &self,
            _request: ProviderRequest,
        ) -> Result<Vec<StreamEvent>, ProviderError> {
            unimplemented!("streaming path is the product path")
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

    /// A fresh state file for one test.
    fn state_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "noel-failover-{name}-{}-{}",
            std::process::id(),
            epoch_ms()
        ));
        std::fs::create_dir_all(&dir).expect("state dir");
        dir.join(STATE_FILE)
    }

    fn serving(events: &Arc<Mutex<Vec<FailoverEvent>>>) -> Vec<String> {
        events
            .lock()
            .expect("events")
            .iter()
            .filter_map(|event| match event {
                FailoverEvent::Serving {
                    account,
                    switched_from: None,
                } => Some(account.clone()),
                FailoverEvent::Serving {
                    account,
                    switched_from: Some(from),
                } => Some(format!("{from}->{account}")),
                FailoverEvent::Limited { .. } => None,
            })
            .collect()
    }

    /// A pool of accounts "a" and "b" that reports to `events`, saving to
    /// `path` when given.
    fn pool(
        path: Option<&Path>,
        events: &Arc<Mutex<Vec<FailoverEvent>>>,
    ) -> FailoverProviderBuilder {
        let seen = events.clone();
        let builder = FailoverProviderBuilder::new("test")
            .account("a", ProviderAuth::ApiKey("a".to_string()))
            .account("b", ProviderAuth::ApiKey("b".to_string()))
            .on_event(move |event| seen.lock().expect("events").push(event));
        match path {
            Some(path) => builder.saved_in(path.to_path_buf()),
            None => builder,
        }
    }

    /// Builds the pool with one provider per account, in order.
    fn build(
        builder: FailoverProviderBuilder,
        providers: Vec<Box<dyn Provider>>,
    ) -> FailoverProvider {
        let mut providers = providers.into_iter();
        builder.build(move |_| providers.next().expect("one provider per account"))
    }

    async fn ask(pool: &FailoverProvider) -> Result<(), ProviderError> {
        pool.complete_streaming(test_request(), &mut |_| Ok(()))
            .await
    }

    #[tokio::test]
    async fn wrapper_rotates_on_rate_limit_with_zero_streamed_output() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let pool = build(
            pool(None, &events),
            vec![Box::new(FlakyProvider::new(&[429])), Box::new(OkProvider)],
        );
        let mut collected = Vec::new();
        pool.complete_streaming(test_request(), &mut |event| {
            collected.push(event);
            Ok(())
        })
        .await
        .expect("request should succeed after failover");
        assert_eq!(collected.len(), 1);
        assert_eq!(serving(&events), ["a", "a->b"]);
        assert!(events.lock().expect("events").iter().any(
            |event| matches!(event, FailoverEvent::Limited { account, .. } if account == "a")
        ));
    }

    /// A 529 or a 503 is the provider overloaded or down for every
    /// account: it is returned for the run loop to retry, and no account is
    /// set aside.
    #[tokio::test]
    async fn wrapper_does_not_rotate_on_an_overloaded_provider() {
        for code in [529, 503] {
            let events = Arc::new(Mutex::new(Vec::new()));
            let pool = build(
                pool(None, &events),
                vec![Box::new(FlakyProvider::new(&[code])), Box::new(OkProvider)],
            );
            let error = ask(&pool).await.expect_err("not failed over");
            assert!(
                matches!(error, ProviderError::Status { status, .. } if status.as_u16() == code)
            );
            assert_eq!(serving(&events), ["a"]);
            assert!(pool.lock().limited_until.is_empty());
        }
    }

    #[tokio::test]
    async fn wrapper_is_sticky_across_requests() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let pool = build(
            pool(None, &events),
            vec![Box::new(OkProvider), Box::new(OkProvider)],
        );
        for _ in 0..3 {
            ask(&pool).await.expect("request succeeds");
        }
        assert_eq!(serving(&events), ["a", "a", "a"]);
    }

    #[tokio::test]
    async fn wrapper_reports_when_every_account_is_rate_limited() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let hour = Some(Duration::from_secs(3_600));
        let pool = build(
            pool(None, &events),
            vec![
                Box::new(FlakyProvider {
                    statuses: Mutex::new(vec![(429, hour)]),
                }),
                Box::new(FlakyProvider::new(&[429])),
            ],
        );
        let message = ask(&pool)
            .await
            .expect_err("all accounts limited")
            .to_string();
        assert!(
            message.contains("every test account is rate limited; the first may be asked again at")
                && message.contains("a: request failed with status 429")
                && message.contains("b: request failed with status 429"),
            "{message}"
        );
        // Asked again, the pool sends nothing.
        let message = ask(&pool).await.expect_err("still limited").to_string();
        assert!(message.ends_with(" UTC"), "{message}");
        assert_eq!(serving(&events), ["a", "a->b"]);
    }

    #[tokio::test]
    async fn wrapper_does_not_rotate_after_partial_stream() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let pool = build(
            pool(None, &events),
            vec![Box::new(PartialStreamProvider), Box::new(OkProvider)],
        );
        let mut collected = Vec::new();
        let error = pool
            .complete_streaming(test_request(), &mut |event| {
                collected.push(event);
                Ok(())
            })
            .await
            .expect_err("mid-stream failures must not rotate");
        assert!(matches!(error, ProviderError::Status { .. }));
        assert_eq!(collected.len(), 1);
        assert_eq!(serving(&events), ["a"]);
    }

    #[tokio::test]
    async fn wrapper_does_not_rotate_on_non_rate_limit_errors() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let pool = build(
            pool(None, &events),
            vec![Box::new(AuthErrorProvider), Box::new(OkProvider)],
        );
        let error = ask(&pool).await.expect_err("auth errors must not rotate");
        assert!(matches!(error, ProviderError::MissingApiKey { .. }));
    }

    /// "a" is at its usage threshold: the first request goes to "b" without
    /// asking "a" first.
    #[tokio::test]
    async fn wrapper_passes_over_an_account_at_its_usage_threshold() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let pool = build(
            pool(None, &events).exhausted_until("a", i64::MAX / 2),
            vec![Box::new(AuthErrorProvider), Box::new(OkProvider)],
        );
        ask(&pool).await.expect("request succeeds on b");
        assert_eq!(serving(&events), ["b"]);
    }

    /// What one pool learns, the next starts from: the sticky account and
    /// the rate limit, until the time the provider gave.
    #[tokio::test]
    async fn the_saved_state_carries_over_to_the_next_pool() {
        let path = state_path("carry-over");
        let events = Arc::new(Mutex::new(Vec::new()));
        let first = build(
            pool(Some(&path), &events),
            vec![
                Box::new(FlakyProvider {
                    statuses: Mutex::new(vec![(429, Some(Duration::from_secs(3_600)))]),
                }),
                Box::new(OkProvider),
            ],
        );
        let before = (epoch_ms() / 1000) as i64;
        ask(&first).await.expect("b answers");
        let saved = saved_state(&path, "test");
        assert_eq!(saved.current.as_deref(), Some("b"));
        assert!(saved.last_switch_ms.is_some());
        let until = saved.limited_until["a"];
        assert!(
            (before + 3_600..=before + 3_602).contains(&until),
            "{until}"
        );

        // A new pool, as the next request builds, does not ask "a".
        let second = build(
            pool(Some(&path), &events),
            vec![Box::new(AuthErrorProvider), Box::new(OkProvider)],
        );
        ask(&second).await.expect("b answers again");
        assert_eq!(serving(&events), ["a", "a->b", "b"]);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    /// A switch away from an account at its usage threshold waits for
    /// `min_switch_interval_ms` after the last switch, also one another pool
    /// made. A switch away from a rate-limited account never waits.
    #[tokio::test]
    async fn only_voluntary_switches_wait_out_the_interval() {
        for (limited, last_switch_ago_ms, expected) in [
            (false, Some(1_000), "a"),
            (false, Some(60_000), "a->b"),
            (false, None, "a->b"),
            (true, Some(1_000), "a->b"),
        ] {
            let path = state_path("interval");
            let now = epoch_ms();
            let mut saved = SavedProvider {
                current: Some("a".into()),
                last_switch_ms: last_switch_ago_ms.map(|ago| now - ago),
                ..SavedProvider::default()
            };
            if limited {
                saved.limited_until.insert("a".into(), i64::MAX / 2);
            }
            save_state(&path, "test", 0, |state| *state = saved);
            let events = Arc::new(Mutex::new(Vec::new()));
            let pool = build(
                pool(Some(&path), &events).exhausted_until("a", i64::MAX / 2),
                vec![Box::new(OkProvider), Box::new(OkProvider)],
            );
            ask(&pool).await.expect("a request is served");
            assert_eq!(
                serving(&events),
                [expected],
                "limited {limited}, last switch {last_switch_ago_ms:?} ms ago"
            );
            std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
        }
    }
}
