//! The built-in model catalog and what a model's entry says about thinking.
//!
//! The catalog is data. `tests/fixtures/ts-parity/model-catalog.json` is
//! upstream's whole built-in chat catalog, written by
//! `scripts/ts-parity/model-catalog-fixture.ts` from upstream's own model
//! runtime, and compiled in here; regenerating that fixture updates the
//! models noel knows. `catalog_port_models.json`, next to this file, is written
//! by hand: the providers only this port has (`faux`, `zai-coding`,
//! `zai-anthropic`, `kimi-coding-openai`), in the same shape.
//!
//! Upstream models whose API noel has no adapter for (`pi-messages`, upstream's
//! Radius gateway) are in the file but not in [`builtin_models`].

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use serde_json::Number;

use crate::ProviderApi;

/// Upstream's thinking levels, lowest first (`THINKING_LEVEL_OPTIONS`).
pub const THINKING_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// The level a session starts on when nothing sets one
/// (upstream `DEFAULT_THINKING_LEVEL`).
pub const DEFAULT_THINKING_LEVEL: &str = "medium";

/// Upstream's `thinkingLevelMap`. A level mapped to `None` (JSON `null`) is
/// one the model does not support; a level mapped to a string is supported
/// and that string is what the provider is sent for it. A level without an
/// entry is supported as itself, except `xhigh` and `max`, which a model
/// supports only when its map names them.
pub type ThinkingLevelMap = BTreeMap<String, Option<String>>;

/// The canonical name of a thinking level, or `None` for any other text.
pub fn thinking_level(name: &str) -> Option<&'static str> {
    THINKING_LEVELS.iter().copied().find(|level| *level == name)
}

/// Upstream `getSupportedThinkingLevels`: the levels a model can be set to,
/// lowest first. A model without reasoning supports only `off`.
pub fn supported_thinking_levels(
    reasoning: bool,
    map: Option<&ThinkingLevelMap>,
) -> Vec<&'static str> {
    if !reasoning {
        return vec!["off"];
    }
    THINKING_LEVELS
        .iter()
        .copied()
        .filter(|level| match map.and_then(|map| map.get(*level)) {
            Some(None) => false,
            Some(Some(_)) => true,
            None => !matches!(*level, "xhigh" | "max"),
        })
        .collect()
}

/// Upstream `clampThinkingLevel`: `level` if the model supports it, else the
/// nearest supported level above it, else the nearest below. Text that is
/// not a thinking level gives the model's lowest level.
pub fn clamp_thinking_level(
    reasoning: bool,
    map: Option<&ThinkingLevelMap>,
    level: &str,
) -> &'static str {
    clamp_to_supported(&supported_thinking_levels(reasoning, map), level)
}

/// [`clamp_thinking_level`] for a model whose supported levels are known.
pub fn clamp_to_supported(supported: &[&'static str], level: &str) -> &'static str {
    let lowest = supported.first().copied().unwrap_or("off");
    let Some(requested) = THINKING_LEVELS.iter().position(|name| *name == level) else {
        return lowest;
    };
    THINKING_LEVELS[requested..]
        .iter()
        .chain(THINKING_LEVELS[..requested].iter().rev())
        .copied()
        .find(|candidate| supported.contains(candidate))
        .unwrap_or(lowest)
}

/// What a provider is sent for `level`: the model's mapped value, or the
/// level's own name when its map has none (upstream
/// `model.thinkingLevelMap?.[level] ?? level`).
pub fn mapped_thinking_level<'a>(map: Option<&'a ThinkingLevelMap>, level: &'a str) -> &'a str {
    match map.and_then(|map| map.get(level)) {
        Some(Some(mapped)) => mapped,
        _ => level,
    }
}

/// Prices in US dollars per million tokens, as upstream records them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCost {
    pub input: Number,
    pub output: Number,
    pub cache_read: Number,
    pub cache_write: Number,
    /// Prices that replace the ones above for requests with more input
    /// tokens than a tier's `inputTokensAbove`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tiers: Vec<ModelCostTier>,
}

impl Default for ModelCost {
    fn default() -> Self {
        Self {
            input: Number::from(0),
            output: Number::from(0),
            cache_read: Number::from(0),
            cache_write: Number::from(0),
            tiers: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCostTier {
    pub input_tokens_above: u64,
    pub input: Number,
    pub output: Number,
    pub cache_read: Number,
    pub cache_write: Number,
}

/// One model of the built-in catalog: the fields of upstream's model object
/// that noel reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogModel {
    pub provider: String,
    pub id: String,
    pub name: String,
    pub api: ProviderApi,
    /// `None` where upstream's entry has no URL (Azure: it comes from the
    /// environment).
    pub base_url: Option<String>,
    pub reasoning: bool,
    pub thinking_level_map: Option<ThinkingLevelMap>,
    /// What the model accepts: `text`, `image`.
    pub input: Vec<String>,
    pub cost: ModelCost,
    pub context_window: u64,
    /// The most output tokens one reply may have.
    pub max_tokens: u64,
}

impl CatalogModel {
    pub fn supported_thinking_levels(&self) -> Vec<&'static str> {
        supported_thinking_levels(self.reasoning, self.thinking_level_map.as_ref())
    }
}

/// The fields of a catalog file's model object that [`CatalogModel`] keeps.
/// Upstream's other fields (`compat`, `headers`, `inputLimits`,
/// `promptCache`, `type`, and Radius's `lab`, `enabled`, `providers`) stay in
/// the file until something reads them.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelRecord {
    id: String,
    name: String,
    api: String,
    provider: String,
    base_url: String,
    reasoning: bool,
    #[serde(default)]
    thinking_level_map: Option<ThinkingLevelMap>,
    input: Vec<String>,
    cost: ModelCost,
    context_window: u64,
    max_tokens: u64,
}

#[derive(Deserialize)]
struct ProviderRecord {
    id: String,
    name: String,
    models: Vec<ModelRecord>,
}

#[derive(Deserialize)]
struct DefaultModelRecord {
    provider: String,
    id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogFile {
    default_models: Vec<DefaultModelRecord>,
    providers: Vec<ProviderRecord>,
}

struct Catalog {
    models: Vec<CatalogModel>,
    /// `(provider, model id)` in the order startup tries them.
    default_models: Vec<(String, String)>,
    /// `(provider id, display name)`.
    providers: Vec<(String, String)>,
}

const UPSTREAM_CATALOG: &str = include_str!("../../../tests/fixtures/ts-parity/model-catalog.json");
const PORT_CATALOG: &str = include_str!("catalog_port_models.json");

fn catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG.get_or_init(|| {
        let mut catalog = Catalog {
            models: Vec::new(),
            default_models: Vec::new(),
            providers: Vec::new(),
        };
        // The port's own providers come first, so `faux/echo` stays the
        // first model; upstream's follow in upstream's order.
        let (port_defaults, upstream_defaults) = (
            load(&mut catalog, PORT_CATALOG, "catalog_port_models.json"),
            load(&mut catalog, UPSTREAM_CATALOG, "model-catalog.json"),
        );
        // Startup tries upstream's providers in upstream's order, then the
        // port's.
        catalog.default_models = upstream_defaults.into_iter().chain(port_defaults).collect();
        catalog
    })
}

/// Adds a catalog file's models and providers; returns its default models.
fn load(catalog: &mut Catalog, text: &str, name: &str) -> Vec<(String, String)> {
    let file: CatalogFile = serde_json::from_str(text)
        .unwrap_or_else(|error| panic!("built-in model catalog {name} is invalid: {error}"));
    for provider in file.providers {
        let before = catalog.models.len();
        catalog
            .models
            .extend(provider.models.into_iter().filter_map(|model| {
                Some(CatalogModel {
                    api: ProviderApi::from_upstream_name(&model.api)?,
                    provider: model.provider,
                    id: model.id,
                    name: model.name,
                    base_url: Some(model.base_url).filter(|url| !url.is_empty()),
                    reasoning: model.reasoning,
                    thinking_level_map: model.thinking_level_map,
                    input: model.input,
                    cost: model.cost,
                    context_window: model.context_window,
                    max_tokens: model.max_tokens,
                })
            }));
        if catalog.models.len() > before {
            catalog.providers.push((provider.id, provider.name));
        }
    }
    file.default_models
        .into_iter()
        .map(|entry| (entry.provider, entry.id))
        .collect()
}

/// Every built-in model noel can run: the port's own providers first, then
/// upstream's catalog in upstream's order.
pub fn builtin_models() -> &'static [CatalogModel] {
    &catalog().models
}

pub fn builtin_model(provider: &str, id: &str) -> Option<&'static CatalogModel> {
    builtin_models()
        .iter()
        .find(|model| model.provider == provider && model.id == id)
}

/// Upstream `defaultModelPerProvider`: the model each provider starts on, in
/// the order startup tries the providers, followed by the port's own.
pub fn default_models() -> &'static [(String, String)] {
    &catalog().default_models
}

/// The built-in providers that have a model noel can run, with their display
/// names, in catalog order.
pub fn builtin_providers() -> &'static [(String, String)] {
    &catalog().providers
}

/// The model a custom id under `provider` is built from (upstream
/// `buildFallbackModel`): the provider's default model, or its first one.
pub fn provider_base_model(provider: &str) -> Option<&'static CatalogModel> {
    let models = || {
        builtin_models()
            .iter()
            .filter(|model| model.provider == provider)
    };
    default_models()
        .iter()
        .find(|(candidate, _)| candidate == provider)
        .and_then(|(_, id)| models().find(|model| model.id == *id))
        .or_else(|| models().next())
}

/// What a request builder needs to know about its model beyond the id: the
/// output cap, the window the cap must fit in, whether the model reasons,
/// and the provider's names for the thinking levels. Every field is `None`
/// for a model nothing is known about.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelTraits {
    pub context_window: Option<u64>,
    pub max_tokens: Option<u64>,
    pub reasoning: Option<bool>,
    pub thinking_level_map: Option<ThinkingLevelMap>,
    /// The model's catalog `input` modalities (`text`, `image`); `None`
    /// when the model is not in the catalog and the answer is unknown.
    pub input_modalities: Option<Vec<String>>,
}

impl ModelTraits {
    /// Whether the model's catalog entry lists image input. `None` means
    /// unknown (a model outside the catalog), and callers decide by API.
    pub fn accepts_images(&self) -> Option<bool> {
        self.input_modalities
            .as_ref()
            .map(|input| input.iter().any(|entry| entry == "image"))
    }
}

impl From<&CatalogModel> for ModelTraits {
    fn from(model: &CatalogModel) -> Self {
        Self {
            context_window: Some(model.context_window),
            max_tokens: Some(model.max_tokens),
            reasoning: Some(model.reasoning),
            thinking_level_map: model.thinking_level_map.clone(),
            input_modalities: Some(model.input.clone()),
        }
    }
}

impl ModelTraits {
    /// The traits of a built-in model; empty for any other.
    pub fn builtin(provider: &str, id: &str) -> Self {
        builtin_model(provider, id)
            .map(Self::from)
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn fixture(text: &str) -> Value {
        serde_json::from_str(text).expect("parse fixture")
    }

    fn upstream_records() -> Vec<Value> {
        fixture(UPSTREAM_CATALOG)["providers"]
            .as_array()
            .expect("providers")
            .iter()
            .flat_map(|provider| provider["models"].as_array().expect("models").clone())
            .collect()
    }

    /// A catalog model as upstream's object, with the fields noel keeps.
    fn as_upstream(model: &CatalogModel) -> Value {
        let mut record = json!({
            "id": model.id,
            "name": model.name,
            "api": model.api.upstream_name(),
            "provider": model.provider,
            "baseUrl": model.base_url.clone().unwrap_or_default(),
            "reasoning": model.reasoning,
            "input": model.input,
            "cost": model.cost,
            "contextWindow": model.context_window,
            "maxTokens": model.max_tokens,
        });
        if let Some(map) = &model.thinking_level_map {
            record["thinkingLevelMap"] = json!(map);
        }
        record
    }

    /// The whole of `model-catalog.json`, not a sample: every upstream model
    /// is a built-in model with the same values, in the same order, except
    /// the ones whose API noel has no adapter for; and every field upstream
    /// has is either compared or named here as not read yet.
    #[test]
    fn builtin_catalog_is_upstreams_catalog_in_full() {
        const NOT_READ: [&str; 8] = [
            "compat",
            "headers",
            "inputLimits",
            "promptCache",
            "type",
            "lab",
            "enabled",
            "providers",
        ];
        let port_providers = ["faux", "zai-coding", "zai-anthropic", "kimi-coding-openai"];
        let mut builtin = builtin_models()
            .iter()
            .filter(|model| !port_providers.contains(&model.provider.as_str()));
        let mut compared = 0;
        let mut without_adapter = Vec::new();
        for mut record in upstream_records() {
            let fields = record.as_object_mut().expect("model object");
            if ProviderApi::from_upstream_name(fields["api"].as_str().expect("api")).is_none() {
                without_adapter.push(fields["api"].clone());
                continue;
            }
            fields.retain(|key, _| !NOT_READ.contains(&key.as_str()));
            let model = builtin.next().expect("a built-in model per upstream model");
            assert_eq!(as_upstream(model), record);
            compared += 1;
        }
        assert_eq!(builtin.next(), None, "a built-in model upstream lacks");
        assert!(compared > 1000, "{compared} models compared");
        // Radius is the only provider noel cannot run.
        assert!(!without_adapter.is_empty());
        assert!(without_adapter.iter().all(|api| api == "pi-messages"));
        assert!(builtin_models()
            .iter()
            .all(|model| model.provider != "radius"));
    }

    #[test]
    fn default_models_follow_upstream_then_the_port() {
        let upstream = fixture(UPSTREAM_CATALOG)["defaultModels"]
            .as_array()
            .expect("defaultModels")
            .iter()
            .map(|entry| {
                (
                    entry["provider"].as_str().expect("provider").to_string(),
                    entry["id"].as_str().expect("id").to_string(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(default_models()[..upstream.len()], upstream[..]);
        assert_eq!(
            default_models()[upstream.len()..]
                .iter()
                .map(|(provider, _)| provider.as_str())
                .collect::<Vec<_>>(),
            ["faux", "zai-coding", "zai-anthropic", "kimi-coding-openai"]
        );
        // Every default model of a provider noel can run exists.
        for (provider, id) in default_models() {
            if provider != "radius" {
                assert!(builtin_model(provider, id).is_some(), "{provider}/{id}");
            }
        }
    }

    #[test]
    fn port_only_providers_sit_beside_upstreams() {
        let models = builtin_models();
        assert_eq!(
            (models[0].provider.as_str(), models[0].id.as_str()),
            ("faux", "echo")
        );
        for (provider, id) in [
            ("faux", "script"),
            ("zai-coding", "glm-5.3"),
            ("zai-anthropic", "glm-5.1"),
            ("kimi-coding-openai", "kimi-for-coding"),
        ] {
            assert!(builtin_model(provider, id).is_some(), "{provider}/{id}");
        }
        // The port's file adds providers; it never redefines upstream's.
        let upstream = fixture(UPSTREAM_CATALOG);
        let upstream_ids = upstream["providers"]
            .as_array()
            .expect("providers")
            .iter()
            .map(|provider| provider["id"].as_str().expect("id"))
            .collect::<Vec<_>>();
        for provider in fixture(PORT_CATALOG)["providers"]
            .as_array()
            .expect("providers")
        {
            let id = provider["id"].as_str().expect("id");
            assert!(!upstream_ids.contains(&id), "{id} is upstream's");
        }
        let providers = builtin_providers()
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>();
        assert!(providers.contains(&"anthropic") && providers.contains(&"faux"));
        assert!(!providers.contains(&"radius") && !providers.contains(&"typesafe"));
    }

    #[test]
    fn a_custom_id_is_built_from_the_providers_default_model() {
        // Every provider with a default model that exists is built from it.
        let mut with_default = 0;
        for (provider, id) in default_models() {
            if builtin_model(provider, id).is_some() {
                assert_eq!(
                    provider_base_model(provider).map(|model| &model.id),
                    Some(id),
                    "{provider}"
                );
                with_default += 1;
            }
        }
        assert!(with_default > 30, "{with_default} providers");
        // Without one, from the provider's first model.
        let stray = builtin_providers()
            .iter()
            .map(|(provider, _)| provider.as_str())
            .find(|provider| default_models().iter().all(|(name, _)| name != provider));
        if let Some(provider) = stray {
            let first = builtin_models()
                .iter()
                .find(|model| model.provider == provider);
            assert_eq!(provider_base_model(provider), first);
        }
        assert_eq!(provider_base_model("no-such-provider"), None);
    }

    /// `model-thinking.json`: upstream's answers for every shape of model in
    /// its catalog.
    #[test]
    fn thinking_levels_match_ts_for_every_catalog_model() {
        let fixture = fixture(include_str!(
            "../../../tests/fixtures/ts-parity/model-thinking.json"
        ));
        assert_eq!(fixture["levels"], json!(THINKING_LEVELS));
        assert_eq!(fixture["defaultLevel"], DEFAULT_THINKING_LEVEL);
        let cases = fixture["cases"].as_array().expect("cases");
        assert!(cases.len() > 20, "{} cases", cases.len());
        let shape = |reasoning: bool, map: Option<&ThinkingLevelMap>| json!([reasoning, map]);
        let mut shapes = BTreeMap::new();
        for case in cases {
            let reasoning = case["reasoning"].as_bool().expect("reasoning");
            let map: Option<ThinkingLevelMap> =
                serde_json::from_value(case["thinkingLevelMap"].clone()).expect("map");
            let examples = &case["examples"];
            assert_eq!(
                json!(supported_thinking_levels(reasoning, map.as_ref())),
                case["supported"],
                "{examples}"
            );
            for (level, expected) in case["clamp"].as_object().expect("clamp") {
                assert_eq!(
                    clamp_thinking_level(reasoning, map.as_ref(), level),
                    expected,
                    "{examples} at {level}"
                );
            }
            shapes.insert(
                shape(reasoning, map.as_ref()).to_string(),
                case["supported"].clone(),
            );
        }
        // Every upstream model has one of the recorded shapes, so the cases
        // cover the catalog.
        for model in builtin_models() {
            if matches!(
                model.provider.as_str(),
                "faux" | "zai-coding" | "zai-anthropic" | "kimi-coding-openai"
            ) {
                continue;
            }
            let key = shape(model.reasoning, model.thinking_level_map.as_ref()).to_string();
            assert_eq!(
                shapes.get(&key),
                Some(&json!(model.supported_thinking_levels())),
                "{}/{}",
                model.provider,
                model.id
            );
        }
    }

    #[test]
    fn a_level_is_sent_as_the_model_names_it() {
        let map: ThinkingLevelMap = serde_json::from_value(json!({
            "off": "none", "minimal": "low", "xhigh": null
        }))
        .expect("map");
        assert_eq!(mapped_thinking_level(Some(&map), "off"), "none");
        assert_eq!(mapped_thinking_level(Some(&map), "minimal"), "low");
        assert_eq!(mapped_thinking_level(Some(&map), "high"), "high");
        assert_eq!(mapped_thinking_level(None, "max"), "max");
        assert_eq!(thinking_level("xhigh"), Some("xhigh"));
        assert_eq!(thinking_level("XHIGH"), None);
    }
}
