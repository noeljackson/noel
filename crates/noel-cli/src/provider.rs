//! The provider a run uses: credentials refreshed per request, account failover, and the thinking budget.

use super::*;

pub(crate) async fn run_prompt_media(
    runtime: &mut Runtime,
    config: &LoadedConfig,
    prompt: String,
    media: Vec<MediaInput>,
    offline: bool,
) -> Result<String> {
    let provider = provider_for_runtime(runtime, config, offline).await?;
    let response = if media.is_empty() {
        run_user_turn(runtime, provider.as_ref(), prompt).await
    } else {
        run_user_turn_streaming_with_media(
            runtime,
            provider.as_ref(),
            prompt,
            media,
            &SteeringMailbox::default(),
            |_| {},
        )
        .await
    };
    // An account switch is reported whether or not the turn succeeded.
    for notice in account_failover::drain_notices() {
        eprintln!("noel: {notice}");
    }
    response.map_err(Into::into)
}

pub(crate) fn auto_compaction_enabled(config: &LoadedConfig) -> bool {
    config
        .settings
        .compaction
        .as_ref()
        .and_then(|compaction| compaction.enabled)
        .unwrap_or(true)
}

/// Sets the factory a subagent's `model` frontmatter builds its provider
/// with: the same construction `provider_for_runtime` does, for the named
/// model, with the config's defaults and the failover of that provider.
pub(crate) fn install_subagent_provider_factory(
    runtime: &mut Runtime,
    config: &LoadedConfig,
    offline: bool,
) {
    let config = config.clone();
    let offline = offline.to_string();
    runtime.set_subagent_provider_factory(Arc::new(move |model: &ModelRef| {
        let config = config.clone();
        let model = model.clone();
        let offline = offline.clone();
        Box::pin(async move {
            if offline == "true" && model.provider != "faux" {
                return Err(format!(
                    "offline mode only allows local faux models; the subagent's model is {}/{}",
                    model.provider, model.id
                ));
            }
            let Some(definition) = model_definition(&config, &model) else {
                return Err(format!(
                    "the subagent's model is not in models config: {}/{}",
                    model.provider, model.id
                ));
            };
            let thinking_level = definition
                .reasoning()
                .then(|| configured_thinking_level(&config, &model).to_string());
            let provider = FreshAuthProvider {
                config: ProviderConfig {
                    thinking_budget_tokens: thinking_budget_tokens(
                        &config,
                        thinking_level.as_deref(),
                    ),
                    thinking_level,
                    model: definition.model_ref(),
                    api: AiProviderApi::from(&definition.api),
                    base_url: definition.base_url.clone(),
                    auth: ProviderAuth::None,
                    session_id: None,
                    traits: definition.traits(),
                },
                client: noel_ai::provider_http_client(),
                refresh_client: reqwest::Client::builder()
                    .timeout(Duration::from_secs(10))
                    .build()
                    .unwrap_or_else(|_| reqwest::Client::new()),
                auth_path: config.paths.auth_path.clone(),
                auth: config.auth.clone(),
                account: None,
                failover: failover_for_model(&config, &model.provider),
            };
            Ok(Box::new(provider) as Box<dyn noel_ai::Provider>)
        })
    }));
}

pub(crate) async fn provider_for_runtime(
    runtime: &Runtime,
    config: &LoadedConfig,
    offline: bool,
) -> Result<Box<dyn noel_ai::Provider>> {
    let model = runtime
        .session()
        .active_model
        .clone()
        .ok_or_else(|| anyhow!("no active model; configure auth or use --model faux/echo"))?;
    if offline && model.provider != "faux" {
        return Err(anyhow!(
            "offline mode only allows local faux models; active model is {}/{}",
            model.provider,
            model.id
        ));
    }
    let definition = model_definition(config, &model).ok_or_else(|| {
        anyhow!(
            "active model is not in models config: {}/{}",
            model.provider,
            model.id
        )
    })?;
    // The level in effect, clamped to the model; a model without reasoning
    // is sent no thinking parameter.
    let thinking_level = definition
        .reasoning()
        .then(|| effective_thinking_level(runtime, config).to_string());
    let provider = FreshAuthProvider {
        config: ProviderConfig {
            thinking_budget_tokens: thinking_budget_tokens(config, thinking_level.as_deref()),
            thinking_level,
            model: definition.model_ref(),
            api: AiProviderApi::from(&definition.api),
            base_url: definition.base_url.clone(),
            auth: ProviderAuth::None,
            session_id: Some(runtime.session().session_id.clone()),
            traits: definition.traits(),
        },
        // The client `noel_ai::create_provider` uses: its idle timeout ends a
        // stalled request instead of hanging the turn.
        client: noel_ai::provider_http_client(),
        refresh_client: reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new()),
        auth_path: config.paths.auth_path.clone(),
        auth: config.auth.clone(),
        account: runtime.session().active_account.clone(),
        failover: failover_for(runtime, config, &model.provider),
    };
    // Fail before the turn starts when there is no usable credential.
    if provider.failover_pool().await?.is_none() {
        provider.auth().await?;
    }
    Ok(Box::new(provider))
}

/// Whether failover rotation is active for a provider: globally enabled,
/// provider allowed (empty map means all), and more than one stored account.
/// An explicitly pinned session account always disables rotation for that
/// session.
pub(crate) fn failover_enabled_for(config: &LoadedConfig, provider: &str) -> bool {
    let Some(settings) = config.settings.failover.as_ref() else {
        return false;
    };
    if !settings.enabled.unwrap_or(false) {
        return false;
    }
    if !settings.providers.is_empty() && !settings.providers.contains_key(provider) {
        return false;
    }
    config.auth.accounts_for_provider(provider).len() > 1
}

/// What a turn's requests need to be served by the provider's account pool
/// instead of one account.
pub(crate) struct FailoverPoolSettings {
    pub(crate) min_switch_interval_ms: u64,
    pub(crate) exhaustion_threshold_pct: f64,
    pub(crate) usage_cache_path: PathBuf,
    pub(crate) state_path: PathBuf,
}

/// The pool settings of a turn when failover is enabled for the provider and
/// no explicit account is pinned for the session. `None` when the
/// single-account path should be used.
pub(crate) fn failover_for(
    runtime: &Runtime,
    config: &LoadedConfig,
    provider: &str,
) -> Option<FailoverPoolSettings> {
    let settings = config.settings.failover.as_ref();
    if !failover_enabled_for(config, provider) || runtime.session().active_account.is_some() {
        return None;
    }
    failover_pool_settings(config, provider, settings)
}

/// The pool settings of a subagent's provider, which pins no account.
fn failover_for_model(config: &LoadedConfig, provider: &str) -> Option<FailoverPoolSettings> {
    let settings = config.settings.failover.as_ref();
    if !failover_enabled_for(config, provider) {
        return None;
    }
    failover_pool_settings(config, provider, settings)
}

fn failover_pool_settings(
    config: &LoadedConfig,
    provider: &str,
    settings: Option<&noel_config::FailoverSettings>,
) -> Option<FailoverPoolSettings> {
    let provider_settings = settings
        .and_then(|settings| settings.providers.get(provider))
        .cloned()
        .unwrap_or_default();
    Some(FailoverPoolSettings {
        min_switch_interval_ms: provider_settings
            .min_switch_interval_ms
            .unwrap_or(account_failover::MIN_SWITCH_INTERVAL_MS),
        exhaustion_threshold_pct: provider_settings
            .exhaustion_threshold_pct
            .map_or(account_failover::EXHAUSTION_THRESHOLD_PCT, |pct| pct as f64),
        usage_cache_path: config.paths.agent_dir.join("usage-cache.json"),
        state_path: config.paths.agent_dir.join(account_failover::STATE_FILE),
    })
}

/// A provider that resolves its credentials for every request, so a turn
/// that outlives an OAuth access token refreshes it instead of sending the
/// token the turn started with.
///
/// With failover on, every request gets its own account pool, built from
/// the credentials valid then. The pool does not outlive the request: the
/// sticky account and the cooldowns are `account_failover`'s process-wide
/// memory, which each pool starts from and reports to.
pub(crate) struct FreshAuthProvider {
    /// The config of every request; `auth` is resolved per request.
    pub(crate) config: ProviderConfig,
    pub(crate) client: reqwest::Client,
    pub(crate) refresh_client: reqwest::Client,
    pub(crate) auth_path: PathBuf,
    pub(crate) auth: AuthData,
    pub(crate) account: Option<String>,
    /// Set when the turn's requests fail over between the stored accounts.
    pub(crate) failover: Option<FailoverPoolSettings>,
}

impl FreshAuthProvider {
    pub(crate) async fn auth(&self) -> Result<ProviderAuth> {
        let resolved = oauth_refresh::refresh_expiring_auth(
            &self.refresh_client,
            &self.auth_path,
            &self.auth,
            &self.config.model.provider,
            self.account.as_deref(),
            None,
            unix_seconds().unwrap_or(0),
        )
        .await?;
        Ok(map_provider_auth(resolved))
    }

    /// Build a failover pool for one request when the turn fails over and
    /// more than one stored account has a credential. Returns `None` when
    /// the single-account path should be used.
    ///
    /// An account whose credential cannot be refreshed is left out, once
    /// with a notice, and the others serve; only when none is left does the
    /// request fail, with the first account's error.
    pub(crate) async fn failover_pool(&self) -> Result<Option<Box<dyn noel_ai::Provider>>> {
        let Some(failover) = &self.failover else {
            return Ok(None);
        };
        let provider = self.config.model.provider.as_str();
        let now = unix_seconds().unwrap_or(0);
        let usage_cache = noel_config::read_usage_cache(&failover.usage_cache_path)
            .ok()
            .flatten();
        let candidates = account_failover::failover_candidates(
            &self.auth,
            usage_cache.as_ref(),
            provider,
            failover.exhaustion_threshold_pct,
            now as i64,
        );
        if candidates.len() < 2 {
            return Ok(None);
        }
        let mut pool = account_failover::FailoverProviderBuilder::new(provider)
            .min_switch_interval_ms(failover.min_switch_interval_ms)
            .saved_in(failover.state_path.clone());
        let saved_sticky = account_failover::saved_state(&failover.state_path, provider).current;
        let (mut accounts, mut first_error, mut sticky) = (0, None, None);
        for candidate in candidates {
            let refreshed = oauth_refresh::refresh_expiring_auth(
                &self.refresh_client,
                &self.auth_path,
                &self.auth,
                provider,
                Some(&candidate.account),
                None,
                now,
            )
            .await;
            let auth = match refreshed {
                Ok(Some(auth)) => auth,
                Ok(None) => continue,
                Err(error) => {
                    account_failover::report_left_out(
                        provider,
                        &candidate.account,
                        &format!("{error:#}"),
                    );
                    first_error.get_or_insert(error);
                    continue;
                }
            };
            accounts += 1;
            if sticky.is_none() || saved_sticky.as_ref() == Some(&candidate.account) {
                sticky = Some((candidate.account.clone(), auth.clone()));
            }
            pool = pool
                .account(candidate.account.clone(), map_provider_auth(Some(auth)))
                .exhausted_until(candidate.account, candidate.exhausted_until);
        }
        if accounts == 0 {
            return first_error.map_or(Ok(None), Err);
        }
        // The 5h window only helps while the usage cache is fresh, so keep
        // the sticky account's fresh; the others are read as last cached.
        if let Some((account, auth)) = sticky {
            usage::refresh_stale_in_background(
                failover.usage_cache_path.clone(),
                provider,
                &account,
                auth,
                self.config.base_url.clone(),
                usage_cache
                    .as_ref()
                    .and_then(|cache| cache.accounts.get(&format!("{provider}:{account}"))),
                now as i64,
            );
        }
        let pool = pool.build(|auth| {
            create_provider_with_client(
                ProviderConfig {
                    auth,
                    ..self.config.clone()
                },
                self.client.clone(),
            )
        });
        Ok(Some(Box::new(pool)))
    }

    /// The provider for one request, with the credentials valid now: the
    /// account pool when the turn fails over, else the session's account.
    pub(crate) async fn provider(
        &self,
    ) -> std::result::Result<Box<dyn noel_ai::Provider>, ProviderError> {
        let config_error = |error: anyhow::Error| ProviderError::Config(error.to_string());
        if let Some(pool) = self.failover_pool().await.map_err(config_error)? {
            return Ok(pool);
        }
        let auth = self.auth().await.map_err(config_error)?;
        Ok(create_provider_with_client(
            ProviderConfig {
                auth,
                ..self.config.clone()
            },
            self.client.clone(),
        ))
    }
}

#[async_trait::async_trait]
impl noel_ai::Provider for FreshAuthProvider {
    async fn complete(
        &self,
        request: ProviderRequest,
    ) -> std::result::Result<Vec<StreamEvent>, ProviderError> {
        self.provider().await?.complete(request).await
    }

    async fn complete_streaming(
        &self,
        request: ProviderRequest,
        on_event: &mut (dyn FnMut(StreamEvent) -> std::result::Result<(), ProviderError> + Send),
    ) -> std::result::Result<(), ProviderError> {
        self.provider()
            .await?
            .complete_streaming(request, on_event)
            .await
    }

    fn thinking_level(&self) -> Option<String> {
        self.config.thinking_level.clone()
    }

    fn output_limit(&self) -> Option<u64> {
        noel_ai::model_output_limit(&self.config)
    }
}

pub(crate) fn thinking_budget_tokens(config: &LoadedConfig, level: Option<&str>) -> Option<u64> {
    let budgets = config.settings.thinking_budgets.as_ref()?;
    match level {
        Some("minimal") => budgets.minimal,
        Some("low") => budgets.low,
        Some("medium") => budgets.medium,
        Some("high") | Some("xhigh") | Some("max") => budgets.high,
        _ => None,
    }
}

pub(crate) fn format_model_selection(model: &ModelRef, thinking: Option<&str>) -> String {
    match thinking {
        Some(level) => format!("{}/{} {level}", model.provider, model.id),
        None => format!("{}/{}", model.provider, model.id),
    }
}

pub(crate) fn map_provider_auth(auth: Option<ResolvedAuth>) -> ProviderAuth {
    match auth {
        Some(ResolvedAuth::ApiKey(api_key)) => ProviderAuth::ApiKey(api_key),
        Some(ResolvedAuth::ClaudeCodeOAuth { access_token, .. }) => {
            ProviderAuth::ClaudeCodeOAuth { access_token }
        }
        Some(ResolvedAuth::ChatGptOAuth {
            access_token,
            account_id,
            ..
        }) => ProviderAuth::ChatGptOAuth {
            access_token,
            account_id,
        },
        None => ProviderAuth::None,
    }
}
