//! Which model a session runs and at which thinking level: startup
//! selection from `--model`, `--provider`, `--models`, `--thinking`, and the
//! settings, and the rules a model switch and a thinking-level change
//! follow. The resolution itself is `noel_config`'s port of upstream's
//! `model-resolver.ts`; this module applies it to a session the way
//! upstream's `main.ts`, `sdk.ts`, and `agent-session.ts` do.

use std::borrow::Cow;

use anyhow::{anyhow, Result};
use noel_ai::catalog::thinking_level;
use noel_ai::ModelRef;
use noel_config::{
    canonical_provider, custom_model, find_exact_model_reference_match, find_initial_model,
    has_auth_for_provider, resolve_cli_model, resolve_model_scope, CliModel, LoadedConfig,
    ModelDefinition, ModelScope, DEFAULT_THINKING_LEVEL, LEGACY_AZURE_PROVIDER, THINKING_LEVELS,
};
use noel_core::Runtime;

use crate::Cli;

/// Whether `provider`'s models can run now: it has credentials (upstream's
/// "available" models), or it is the local faux provider. Offline, only
/// faux can. Named accounts without a `default` count when account failover
/// rotates between them, because the pool resolves which one serves.
pub(crate) fn provider_usable(config: &LoadedConfig, provider: &str, offline: bool) -> bool {
    provider == "faux"
        || (!offline
            && (has_auth_for_provider(&config.auth, provider, None)
                || crate::failover_enabled_for(config, provider)))
}

/// The models that can run now, in catalog order.
pub(crate) fn available_models(config: &LoadedConfig, offline: bool) -> Vec<&ModelDefinition> {
    config
        .models
        .iter()
        .filter(|model| provider_usable(config, &model.provider, offline))
        .collect()
}

/// The models a model picker or `/model <reference>` chooses from: the
/// scope (`--models`, `enabledModels`) when there is one, else every model
/// that can run now.
pub(crate) fn selectable_models(config: &LoadedConfig, offline: bool) -> Vec<&ModelDefinition> {
    if config.model_scope.is_empty() {
        return available_models(config, offline);
    }
    config
        .model_scope
        .iter()
        .filter_map(|scoped| configured_model(config, &scoped.model))
        .collect()
}

pub(crate) fn configured_model<'a>(
    config: &'a LoadedConfig,
    model: &ModelRef,
) -> Option<&'a ModelDefinition> {
    config
        .models
        .iter()
        .find(|candidate| candidate.provider == model.provider && candidate.id == model.id)
}

/// What requests for the session's model are built from: the configured
/// model, or, for an id its provider does not list (a session saved when
/// the catalog had the model), that id on the provider's default model, as
/// `--model provider/new-id` builds one.
pub(crate) fn model_definition<'a>(
    config: &'a LoadedConfig,
    model: &ModelRef,
) -> Option<Cow<'a, ModelDefinition>> {
    if let Some(definition) = configured_model(config, model) {
        return Some(Cow::Borrowed(definition));
    }
    custom_model(
        &config.models,
        canonical_provider(&model.provider),
        &model.id,
    )
    .map(Cow::Owned)
}

/// A thinking level by name. Besides upstream's seven names this accepts
/// the spellings earlier versions of this port did.
pub(crate) fn normalized_thinking_level(level: &str) -> Option<&'static str> {
    match level.trim().to_ascii_lowercase().as_str() {
        "none" | "disabled" => Some("off"),
        "min" => Some("minimal"),
        "med" => Some("medium"),
        "extra-high" | "extra_high" => Some("xhigh"),
        level => thinking_level(level),
    }
}

/// The level a session of `model` starts on when nothing set one (upstream
/// `createAgentSession`): the model's level in `modelThinkingLevels`, else
/// `defaultThinkingLevel`, else `medium`.
fn configured_thinking_level(config: &LoadedConfig, model: &ModelRef) -> &'static str {
    [
        config
            .settings
            .model_thinking_level(&model.provider, &model.id),
        config.settings.default_thinking_level.as_deref(),
    ]
    .into_iter()
    .flatten()
    .find_map(normalized_thinking_level)
    .unwrap_or(DEFAULT_THINKING_LEVEL)
}

/// The thinking level in effect for the session's model: the level the
/// session was set to, else the configured one, clamped to what the model
/// supports. `off` without a model, and for a model without reasoning.
pub(crate) fn effective_thinking_level(runtime: &Runtime, config: &LoadedConfig) -> &'static str {
    let session = runtime.session();
    let Some(model) = &session.active_model else {
        return "off";
    };
    let Some(definition) = model_definition(config, model) else {
        return "off";
    };
    let requested = session
        .active_thinking_level
        .as_deref()
        .and_then(normalized_thinking_level)
        .unwrap_or_else(|| configured_thinking_level(config, model));
    definition.clamp_thinking_level(requested)
}

/// The level in effect as the status line and `/thinking` show it: `None`
/// for a model that has no thinking levels.
pub(crate) fn thinking_label(runtime: &Runtime, config: &LoadedConfig) -> Option<String> {
    let model = runtime.session().active_model.as_ref()?;
    model_definition(config, model)?
        .reasoning()
        .then(|| effective_thinking_level(runtime, config).to_string())
}

/// Sets the session's thinking level to `level` clamped to its model
/// (upstream `setThinkingLevel`), saving it only when it changes. Returns
/// the level now in effect.
pub(crate) fn set_thinking_level(
    runtime: &mut Runtime,
    config: &LoadedConfig,
    level: &str,
) -> Result<&'static str> {
    let clamped = match runtime
        .session()
        .active_model
        .as_ref()
        .and_then(|model| model_definition(config, model))
    {
        Some(definition) => definition.clamp_thinking_level(level),
        // Nothing is known of the model: the level as asked for.
        None => normalized_thinking_level(level).unwrap_or("off"),
    };
    if runtime.session().active_thinking_level.as_deref() != Some(clamped) {
        runtime.set_active_thinking_level(Some(clamped.to_string()))?;
    }
    Ok(clamped)
}

/// Makes `model` the session's model and gives the session the thinking
/// level a model switch gives it upstream (`setModel`): `explicit` when the
/// caller names one, else the model's level in `modelThinkingLevels`, else
/// `defaultThinkingLevel`, else the level in effect before, clamped to the
/// new model. Returns the level now in effect.
pub(crate) fn switch_model(
    runtime: &mut Runtime,
    config: &LoadedConfig,
    model: &ModelRef,
    explicit: Option<&str>,
) -> Result<&'static str> {
    let before = effective_thinking_level(runtime, config);
    let level = [
        explicit,
        config
            .settings
            .model_thinking_level(&model.provider, &model.id),
        config.settings.default_thinking_level.as_deref(),
    ]
    .into_iter()
    .flatten()
    .find_map(normalized_thinking_level)
    .unwrap_or(before);
    runtime.set_active_model(Some(model.clone()))?;
    set_thinking_level(runtime, config, level)
}

/// The model `/model <reference>` and a queued mid-turn switch name: an
/// exact `provider/id` or id among the selectable models, ignoring case
/// (upstream `findExactModelMatch`).
pub(crate) fn find_selectable_model(
    config: &LoadedConfig,
    reference: &str,
    offline: bool,
) -> Option<ModelRef> {
    find_exact_model_reference_match(reference, &selectable_models(config, offline))
        .map(ModelDefinition::model_ref)
}

/// `--provider` and `--model` with the Azure provider id earlier versions
/// of this port used rewritten to upstream's.
fn canonical_cli_model(cli: &Cli) -> (Option<String>, Option<String>) {
    let provider = cli
        .provider
        .as_deref()
        .map(|provider| canonical_provider(provider).to_string());
    let prefix = format!("{LEGACY_AZURE_PROVIDER}/");
    let model = cli
        .model
        .as_deref()
        .map(|model| match model.get(..prefix.len()) {
            Some(start) if start.eq_ignore_ascii_case(&prefix) => {
                format!(
                    "{}/{}",
                    canonical_provider(LEGACY_AZURE_PROVIDER),
                    &model[prefix.len()..]
                )
            }
            _ => model.to_string(),
        });
    (provider, model)
}

/// `--thinking`, if it names a level.
fn cli_thinking(cli: &Cli) -> Option<&'static str> {
    cli.thinking.as_deref().and_then(normalized_thinking_level)
}

/// What `--model` (with `--provider` and `--thinking`) resolves to among
/// every known model; `None` without `--model`.
pub(crate) fn resolve_cli(cli: &Cli, config: &LoadedConfig, offline: bool) -> Option<CliModel> {
    let (provider, model) = canonical_cli_model(cli);
    Some(resolve_cli_model(
        provider.as_deref(),
        &model?,
        cli_thinking(cli),
        &config.models,
        &|provider| provider_usable(config, provider, offline),
    ))
}

/// What `--models`, or without it the `enabledModels` setting, selects of
/// the models that can run now.
pub(crate) fn resolve_scope(cli: &Cli, config: &LoadedConfig, offline: bool) -> ModelScope {
    let patterns = if cli.models.is_empty() {
        config.settings.enabled_models.clone().unwrap_or_default()
    } else {
        cli.models.clone()
    };
    resolve_model_scope(&patterns, &available_models(config, offline))
}

/// The startup model when neither the command line, the scope, nor the
/// session names one, in upstream's order (`findInitialModel`): the saved
/// default if its provider has credentials, then a model with credentials (a
/// provider's usual model first), and a faux model only when it is the saved
/// default or nothing else can run. Offline, only faux can run.
///
/// The saved default counts whether or not a model list has its id, as it
/// always has here: a default saved when the catalog listed the model keeps
/// selecting it, and requests build it on its provider's default model
/// ([`model_definition`]). Upstream ignores a saved default it cannot find.
pub(crate) fn default_model(config: &LoadedConfig, offline: bool) -> Option<ModelRef> {
    let settings = &config.settings;
    if let (Some(provider), Some(id)) = (&settings.default_provider, &settings.default_model) {
        if provider_usable(config, provider, offline) {
            return Some(ModelRef {
                provider: provider.clone(),
                id: id.clone(),
            });
        }
    }
    let (faux, real): (Vec<&ModelDefinition>, Vec<&ModelDefinition>) = config
        .models
        .iter()
        .partition(|model| model.provider == "faux");
    find_initial_model(&real, None, None, &|provider| {
        provider_usable(config, provider, offline)
    })
    .or_else(|| faux.first().copied())
    .map(ModelDefinition::model_ref)
}

/// What the command line says about the model and thinking level.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CliSelection {
    /// The model `--model` resolved to.
    pub model: Option<ModelRef>,
    /// `--thinking`, or the level a `--model pattern:<thinking>` names.
    pub thinking: Option<&'static str>,
}

/// Adds the custom model `--model provider/new-id` names to the config, so
/// requests and state queries find it. Startup and `/reload` both do this.
pub(crate) fn register_cli_model(
    cli: &Cli,
    config: &mut LoadedConfig,
    offline: bool,
) -> Option<CliModel> {
    let resolved = resolve_cli(cli, config, offline)?;
    if let (true, Some(model)) = (resolved.custom, &resolved.model) {
        config.models.push(model.clone());
    }
    Some(resolved)
}

/// Reads `--provider`, `--model`, `--thinking`, and the scope's patterns as
/// upstream's `buildSessionOptions` does: `--model` is an exact or fuzzy
/// match, `provider/pattern`, `pattern:<thinking>`, or a custom id of a
/// known provider. Warnings go to stderr as upstream prints them, and an
/// error ends startup.
pub(crate) fn resolve_cli_selection(
    cli: &Cli,
    config: &mut LoadedConfig,
    offline: bool,
) -> Result<CliSelection> {
    let mut selection = CliSelection {
        model: None,
        thinking: cli_thinking(cli),
    };
    if let (Some(level), None) = (&cli.thinking, selection.thinking) {
        warn(&format!(
            "Invalid thinking level \"{level}\". Valid values: {}",
            THINKING_LEVELS.join(", ")
        ));
    }
    if let (Some(provider), None) = (&cli.provider, &cli.model) {
        return Err(anyhow!(
            "--provider requires --model (for example: --provider {provider} --model <pattern>)"
        ));
    }
    for diagnostic in &resolve_scope(cli, config, offline).diagnostics {
        warn(diagnostic);
    }
    if let Some(resolved) = register_cli_model(cli, config, offline) {
        if let Some(warning) = &resolved.warning {
            warn(warning);
        }
        if let Some(error) = resolved.error {
            return Err(anyhow!(error));
        }
        selection.model = resolved.model.as_ref().map(ModelDefinition::model_ref);
        selection.thinking = selection.thinking.or(resolved.thinking_level);
    }
    Ok(selection)
}

/// Chooses the session's model and thinking level at startup, as upstream's
/// `buildSessionOptions` and `createAgentSession` do:
///
/// - `--model` wins.
/// - Otherwise a session that already has a model keeps it.
/// - Otherwise the scope chooses: the saved default if it is in scope, else
///   the scope's first model, with the thinking level its pattern names.
/// - Otherwise the default model (see [`default_model`]).
///
/// `--thinking` sets the session's level, then a `:<thinking>` suffix, then
/// the scope's; without any the session keeps its saved level, or starts on
/// the configured one.
pub(crate) fn select_initial_model(
    runtime: &mut Runtime,
    config: &LoadedConfig,
    selection: &CliSelection,
    offline: bool,
) -> Result<()> {
    let mut thinking = selection.thinking;
    if let Some(model) = &selection.model {
        runtime.set_active_model(Some(model.clone()))?;
    } else if let Some(model) = runtime.session().active_model.clone() {
        // A session saved under the Azure id this port used to use.
        if model.provider == LEGACY_AZURE_PROVIDER {
            runtime.set_active_model(Some(ModelRef {
                provider: canonical_provider(&model.provider).to_string(),
                id: model.id,
            }))?;
        }
    } else if let Some(first) = config.model_scope.first() {
        let saved = config.model_scope.iter().find(|scoped| {
            Some(scoped.model.provider.as_str()) == config.settings.default_provider.as_deref()
                && Some(scoped.model.id.as_str()) == config.settings.default_model.as_deref()
        });
        let scoped = saved.unwrap_or(first);
        runtime.set_active_model(Some(scoped.model.clone()))?;
        thinking = thinking.or(scoped
            .thinking_level
            .as_deref()
            .and_then(normalized_thinking_level));
    } else {
        runtime.set_active_model(default_model(config, offline))?;
    }
    if let Some(level) = thinking {
        set_thinking_level(runtime, config, level)?;
    }
    Ok(())
}

/// A startup warning, as upstream prints one.
fn warn(message: &str) {
    eprintln!("Warning: {message}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use noel_config::{AuthCredential, ProviderApi, DEFAULT_ACCOUNT_NAME};
    use noel_core::{ReloadableSystems, SessionState};
    use serde_json::json;
    use std::path::Path;

    /// A reasoning model that supports `off` through `high`, and `xhigh`
    /// when asked for.
    fn thinker(provider: &str, id: &str, xhigh: bool) -> ModelDefinition {
        ModelDefinition {
            provider: provider.to_string(),
            id: id.to_string(),
            name: Some(format!("{id} display")),
            api: ProviderApi::Anthropic,
            reasoning: Some(true),
            thinking_level_map: xhigh
                .then(|| serde_json::from_value(json!({ "xhigh": "xhigh" })).unwrap()),
            max_tokens: Some(1000),
            ..ModelDefinition::default()
        }
    }

    /// Made-up providers, so only the config's own keys decide which ones
    /// have credentials, whatever the environment of the test run holds:
    /// `alpha` and `beta` have keys, `gamma` has none.
    fn config() -> LoadedConfig {
        let mut config = crate::tests::minimal_test_config();
        config.models = vec![
            ModelDefinition {
                provider: "faux".to_string(),
                id: "echo".to_string(),
                api: ProviderApi::Faux,
                ..ModelDefinition::default()
            },
            thinker("alpha", "small", false),
            thinker("alpha", "large", true),
            thinker("beta", "large", true),
            ModelDefinition {
                reasoning: Some(false),
                ..thinker("beta", "plain", false)
            },
            thinker("gamma", "large", true),
        ];
        for provider in ["alpha", "beta"] {
            config.auth.insert(
                provider,
                DEFAULT_ACCOUNT_NAME,
                AuthCredential::ApiKey {
                    key: "test-key".to_string(),
                },
            );
        }
        config
    }

    fn runtime() -> Runtime {
        Runtime::new(
            SessionState::new("session-1", ".".into()),
            ReloadableSystems::default(),
        )
    }

    fn model(provider: &str, id: &str) -> ModelRef {
        ModelRef {
            provider: provider.to_string(),
            id: id.to_string(),
        }
    }

    /// Startup as `run` does it: the flags folded into the config, then the
    /// selection. Returns the session's model and the level in effect.
    fn start(
        config: &mut LoadedConfig,
        runtime: &mut Runtime,
        args: &[&str],
    ) -> Result<(String, &'static str)> {
        let cli = Cli::try_parse_from(["noel"].into_iter().chain(args.iter().copied())).unwrap();
        crate::apply_cli_overrides(&cli, Path::new("."), config)?;
        let selection = resolve_cli_selection(&cli, config, false)?;
        select_initial_model(runtime, config, &selection, false)?;
        let active = runtime.session().active_model.clone().unwrap();
        Ok((
            format!("{}/{}", active.provider, active.id),
            effective_thinking_level(runtime, config),
        ))
    }

    #[test]
    fn the_default_thinking_level_is_medium_and_settings_rank_above_it() {
        let mut config = config();
        let mut runtime = runtime();
        assert_eq!(
            start(&mut config, &mut runtime, &["--model", "alpha/large"]).unwrap(),
            ("alpha/large".to_string(), "medium")
        );
        // Nothing was saved: the level follows the settings.
        assert_eq!(runtime.session().active_thinking_level, None);
        config.settings.default_thinking_level = Some("high".to_string());
        assert_eq!(effective_thinking_level(&runtime, &config), "high");
        config
            .settings
            .model_thinking_levels
            .insert("alpha/large".to_string(), "xhigh".to_string());
        assert_eq!(effective_thinking_level(&runtime, &config), "xhigh");
        assert_eq!(thinking_label(&runtime, &config).as_deref(), Some("xhigh"));
        // A level the session was set to ranks above both, and text that is
        // not a level counts as unset.
        runtime
            .set_active_thinking_level(Some("low".to_string()))
            .unwrap();
        assert_eq!(effective_thinking_level(&runtime, &config), "low");
        runtime
            .set_active_thinking_level(Some("loud".to_string()))
            .unwrap();
        assert_eq!(effective_thinking_level(&runtime, &config), "xhigh");
    }

    #[test]
    fn a_level_the_model_lacks_is_clamped_not_dropped() {
        let mut config = config();
        let mut runtime = runtime();
        // `max` on a model that stops at xhigh, and at high.
        assert_eq!(
            start(
                &mut config,
                &mut runtime,
                &["--model", "alpha/large", "--thinking", "max"]
            )
            .unwrap()
            .1,
            "xhigh"
        );
        assert_eq!(
            runtime.session().active_thinking_level.as_deref(),
            Some("xhigh")
        );
        let mut runtime = self::runtime();
        assert_eq!(
            start(&mut config, &mut runtime, &["--model", "alpha/small:xhigh"])
                .unwrap()
                .1,
            "high"
        );
        // A model without reasoning has no level, whatever was asked.
        let mut runtime = self::runtime();
        assert_eq!(
            start(
                &mut config,
                &mut runtime,
                &["--model", "beta/plain", "--thinking", "high"]
            )
            .unwrap()
            .1,
            "off"
        );
        assert_eq!(thinking_label(&runtime, &config), None);
        // `--thinking` beats a `:<thinking>` suffix.
        let mut runtime = self::runtime();
        assert_eq!(
            start(
                &mut config,
                &mut runtime,
                &["--model", "alpha/large:low", "--thinking", "high"]
            )
            .unwrap()
            .1,
            "high"
        );
    }

    #[test]
    fn a_model_switch_takes_the_settings_level_else_keeps_the_current_one() {
        let mut config = config();
        let mut runtime = runtime();
        start(
            &mut config,
            &mut runtime,
            &["--model", "alpha/large", "--thinking", "xhigh"],
        )
        .unwrap();
        // No setting: the level in effect, clamped to the new model, and it
        // does not come back on the way back.
        assert_eq!(
            switch_model(&mut runtime, &config, &model("alpha", "small"), None).unwrap(),
            "high"
        );
        assert_eq!(
            switch_model(&mut runtime, &config, &model("alpha", "large"), None).unwrap(),
            "high"
        );
        // `defaultThinkingLevel` ranks above the current level, and the
        // model's own setting above that.
        config.settings.default_thinking_level = Some("low".to_string());
        config
            .settings
            .model_thinking_levels
            .insert("beta/large".to_string(), "xhigh".to_string());
        assert_eq!(
            switch_model(&mut runtime, &config, &model("beta", "large"), None).unwrap(),
            "xhigh"
        );
        assert_eq!(
            switch_model(&mut runtime, &config, &model("alpha", "large"), None).unwrap(),
            "low"
        );
        // A level the caller names ranks above all of them.
        assert_eq!(
            switch_model(
                &mut runtime,
                &config,
                &model("beta", "large"),
                Some("medium")
            )
            .unwrap(),
            "medium"
        );
        assert_eq!(runtime.session().active_model, Some(model("beta", "large")));
    }

    #[test]
    fn setting_a_level_clamps_it_to_the_model() {
        let config = config();
        let mut runtime = runtime();
        runtime
            .set_active_model(Some(model("alpha", "small")))
            .unwrap();
        assert_eq!(
            set_thinking_level(&mut runtime, &config, "max").unwrap(),
            "high"
        );
        assert_eq!(
            runtime.session().active_thinking_level.as_deref(),
            Some("high")
        );
        assert_eq!(
            set_thinking_level(&mut runtime, &config, "off").unwrap(),
            "off"
        );
        // Nothing is known of the model: the level as asked for.
        runtime
            .set_active_model(Some(model("nowhere", "model")))
            .unwrap();
        assert_eq!(
            set_thinking_level(&mut runtime, &config, "xhigh").unwrap(),
            "xhigh"
        );
        assert_eq!(effective_thinking_level(&runtime, &config), "off");
    }

    #[test]
    fn a_scope_chooses_the_startup_model_without_removing_models() {
        let mut config = config();
        let mut runtime = runtime();
        // The first model the scope selects, at the level its pattern names;
        // `gamma` has no credentials, so the scope cannot select it.
        assert_eq!(
            start(
                &mut config,
                &mut runtime,
                &["--models", "gamma/*,beta/lar*:low,alpha/*"]
            )
            .unwrap(),
            ("beta/large".to_string(), "low")
        );
        assert_eq!(
            config
                .model_scope
                .iter()
                .map(|scoped| format!("{}/{}", scoped.model.provider, scoped.model.id))
                .collect::<Vec<_>>(),
            ["beta/large", "alpha/small", "alpha/large"]
        );
        assert_eq!(config.models.len(), 6);
        // The saved default, when it is in scope.
        config.settings.default_provider = Some("alpha".to_string());
        config.settings.default_model = Some("large".to_string());
        let mut runtime = self::runtime();
        assert_eq!(
            start(
                &mut config,
                &mut runtime,
                &["--models", "beta/*:low,alpha/*:high"]
            )
            .unwrap(),
            ("alpha/large".to_string(), "high")
        );
        // `--model` wins over the scope, over every known model.
        let mut runtime = self::runtime();
        assert_eq!(
            start(
                &mut config,
                &mut runtime,
                &["--models", "beta/*:low", "--model", "gamma/large"]
            )
            .unwrap(),
            ("gamma/large".to_string(), "medium")
        );
        // The setting is the scope when the flag is absent, and a session
        // that has a model keeps it.
        config.settings.enabled_models = Some(vec!["beta/plain".to_string()]);
        assert_eq!(
            start(&mut config, &mut runtime, &[]).unwrap().0,
            "gamma/large"
        );
        let mut runtime = self::runtime();
        assert_eq!(
            start(&mut config, &mut runtime, &[]).unwrap().0,
            "beta/plain"
        );
    }

    #[test]
    fn model_commands_choose_from_the_scope_or_the_models_that_can_run() {
        let mut config = config();
        let ids = |models: Vec<&ModelDefinition>| {
            models
                .iter()
                .map(|model| format!("{}/{}", model.provider, model.id))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ids(selectable_models(&config, false)),
            [
                "faux/echo",
                "alpha/small",
                "alpha/large",
                "beta/large",
                "beta/plain"
            ]
        );
        assert_eq!(ids(selectable_models(&config, true)), ["faux/echo"]);
        // An exact reference, ignoring case; an id two providers have names
        // neither, and a fuzzy pattern names none.
        assert_eq!(
            find_selectable_model(&config, "ALPHA/Small", false),
            Some(model("alpha", "small"))
        );
        assert_eq!(
            find_selectable_model(&config, "small", false),
            Some(model("alpha", "small"))
        );
        assert_eq!(find_selectable_model(&config, "large", false), None);
        assert_eq!(find_selectable_model(&config, "sma", false), None);
        assert_eq!(find_selectable_model(&config, "gamma/large", false), None);
        config.model_scope = vec![noel_config::ScopedModel {
            model: model("beta", "large"),
            thinking_level: None,
        }];
        assert_eq!(
            find_selectable_model(&config, "large", false),
            Some(model("beta", "large"))
        );
        assert_eq!(find_selectable_model(&config, "alpha/small", false), None);
    }

    #[test]
    fn startup_reports_what_upstream_reports() {
        let mut config = config();
        let error = |config: &mut LoadedConfig, args: &[&str]| {
            start(config, &mut runtime(), args)
                .expect_err("startup fails")
                .to_string()
        };
        assert_eq!(
            error(&mut config, &["--provider", "alpha"]),
            "--provider requires --model (for example: --provider alpha --model <pattern>)"
        );
        assert_eq!(
            error(&mut config, &["--provider", "nowhere", "--model", "x"]),
            "Unknown provider \"nowhere\". Use --list-models to see available providers/models."
        );
        assert_eq!(
            error(&mut config, &["--model", "nowhere/x"]),
            "Model \"nowhere/x\" not found. Use --list-models to see available models."
        );
        // `large` is at two providers with credentials.
        assert_eq!(
            error(&mut config, &["--model", "large"]),
            "Model \"large\" is ambiguous across providers: alpha/large, beta/large, gamma/large. More than one matching provider is authenticated. Use --provider or provider/model."
        );
    }

    #[test]
    fn a_custom_model_id_is_registered_on_its_providers_default_model() {
        let mut config = config();
        let mut runtime = runtime();
        // The provider's first model stands in for a default model.
        assert_eq!(
            start(&mut config, &mut runtime, &["--model", "beta/next:xhigh"]).unwrap(),
            ("beta/next".to_string(), "xhigh")
        );
        let custom = configured_model(&config, &model("beta", "next")).expect("registered");
        assert_eq!(custom.name.as_deref(), Some("next"));
        assert_eq!(custom.max_tokens, Some(1000));
        // Asking for thinking makes a custom model a reasoning one.
        let mut config = self::config();
        config.models.retain(|model| model.id != "large");
        let mut runtime = self::runtime();
        assert_eq!(
            start(
                &mut config,
                &mut runtime,
                &["--model", "beta/next", "--thinking", "high"]
            )
            .unwrap(),
            ("beta/next".to_string(), "high")
        );
        assert!(configured_model(&config, &model("beta", "next"))
            .unwrap()
            .reasoning());
    }

    #[test]
    fn a_session_model_the_catalog_no_longer_lists_still_runs() {
        let config = config();
        let mut runtime = runtime();
        runtime
            .set_active_model(Some(model("alpha", "retired")))
            .unwrap();
        assert_eq!(configured_model(&config, &model("alpha", "retired")), None);
        let definition = model_definition(&config, &model("alpha", "retired")).expect("built");
        assert_eq!(
            (
                definition.id.as_str(),
                definition.api.clone(),
                definition.max_tokens
            ),
            ("retired", ProviderApi::Anthropic, Some(1000))
        );
        assert_eq!(effective_thinking_level(&runtime, &config), "medium");
        // Nothing can be built for a provider nothing is known of.
        assert!(model_definition(&config, &model("nowhere", "model")).is_none());
    }

    #[test]
    fn the_former_azure_provider_id_is_read_as_azure() {
        let mut config = config();
        config.models.push(ModelDefinition {
            api: ProviderApi::AzureOpenAiResponses,
            ..thinker("azure", "gpt-test", true)
        });
        // On the command line.
        for args in [
            &["--model", "azure-openai-responses/gpt-test"][..],
            &[
                "--provider",
                "azure-openai-responses",
                "--model",
                "gpt-test",
            ][..],
        ] {
            assert_eq!(
                start(&mut config, &mut runtime(), args).unwrap().0,
                "azure/gpt-test"
            );
        }
        // In a saved session.
        let mut runtime = runtime();
        runtime
            .set_active_model(Some(model("azure-openai-responses", "gpt-test")))
            .unwrap();
        assert_eq!(
            start(&mut config, &mut runtime, &[]).unwrap().0,
            "azure/gpt-test"
        );
    }

    #[test]
    fn without_a_choice_startup_takes_a_model_with_credentials() {
        let mut config = config();
        // No provider default among these models: the first with a key.
        assert_eq!(default_model(&config, false), Some(model("alpha", "small")));
        // A saved default counts when its provider has a key, even one
        // saved when the catalog still listed the model.
        config.settings.default_provider = Some("beta".to_string());
        config.settings.default_model = Some("large".to_string());
        assert_eq!(default_model(&config, false), Some(model("beta", "large")));
        config.settings.default_model = Some("retired".to_string());
        assert_eq!(
            default_model(&config, false),
            Some(model("beta", "retired"))
        );
        assert!(model_definition(&config, &model("beta", "retired")).is_some());
        config.settings.default_provider = Some("gamma".to_string());
        config.settings.default_model = Some("large".to_string());
        assert_eq!(default_model(&config, false), Some(model("alpha", "small")));
        // Faux runs only when asked for, offline, or when nothing else can.
        assert_eq!(default_model(&config, true), Some(model("faux", "echo")));
        config.settings.default_provider = Some("faux".to_string());
        config.settings.default_model = Some("echo".to_string());
        assert_eq!(default_model(&config, false), Some(model("faux", "echo")));
        config.settings.default_provider = None;
        config.auth = noel_config::AuthData::default();
        assert_eq!(default_model(&config, false), Some(model("faux", "echo")));
    }

    #[test]
    fn thinking_levels_have_upstreams_names_and_this_ports_former_ones() {
        for (name, level) in [
            ("off", "off"),
            ("XHIGH", "xhigh"),
            (" max ", "max"),
            ("none", "off"),
            ("min", "minimal"),
            ("med", "medium"),
            ("extra-high", "xhigh"),
        ] {
            assert_eq!(normalized_thinking_level(name), Some(level), "{name}");
        }
        assert_eq!(normalized_thinking_level("loud"), None);
    }
}
