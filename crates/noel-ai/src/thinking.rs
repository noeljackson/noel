//! Thinking levels and reasoning effort, per model.

#[allow(unused_imports)]
use super::*;

/// The reasoning effort a request names for the configured thinking level:
/// the model's own name for the level where its catalog entry has one
/// (upstream `model.thinkingLevelMap?.[level] ?? level`), `none` for `off`,
/// and otherwise the level itself. `None` without a thinking level.
pub(crate) fn reasoning_effort(config: &ProviderConfig) -> Option<String> {
    let level = request_thinking_level(config)?;
    if let Some(mapped) = mapped_thinking_level(config, level) {
        return Some(mapped.to_string());
    }
    Some(
        match level {
            "off" => "none",
            // Only a model whose entry names `max` is set to it; a caller
            // that asks anyway gets the highest effort the API defines.
            "max" => "xhigh",
            level => level,
        }
        .to_string(),
    )
}

/// The thinking level a request is built for. A model known to have no
/// reasoning gets none, and a level a known model does not support becomes
/// the nearest one it does (upstream `clampThinkingLevel`, which its request
/// builders apply too). A model nothing is known about keeps the level as
/// configured.
pub(crate) fn request_thinking_level(config: &ProviderConfig) -> Option<&'static str> {
    let level = normalized_thinking_level(config.thinking_level.as_deref())?;
    match config.traits.reasoning {
        Some(false) => None,
        Some(true) => Some(catalog::clamp_thinking_level(
            true,
            config.traits.thinking_level_map.as_ref(),
            level,
        )),
        None => Some(level),
    }
}

/// The level for the request builders that do not clamp (Anthropic and
/// Bedrock, as upstream's): the configured one, or none for a model known
/// to have no reasoning or that cannot turn thinking off when asked to.
pub(crate) fn unclamped_thinking_level(config: &ProviderConfig) -> Option<&'static str> {
    let level = normalized_thinking_level(config.thinking_level.as_deref())?;
    let cannot_turn_off = || {
        config
            .traits
            .thinking_level_map
            .as_ref()
            .is_some_and(|map| map.get("off") == Some(&None))
    };
    if config.traits.reasoning == Some(false) || (level == "off" && cannot_turn_off()) {
        return None;
    }
    Some(level)
}

/// The model's own name for `level`, if its catalog entry gives one.
pub(crate) fn mapped_thinking_level<'a>(
    config: &'a ProviderConfig,
    level: &str,
) -> Option<&'a str> {
    config
        .traits
        .thinking_level_map
        .as_ref()?
        .get(level)?
        .as_deref()
}

pub(crate) fn thinking_is_off(config: &ProviderConfig) -> bool {
    request_thinking_level(config) == Some("off")
}

pub(crate) fn normalized_thinking_level(level: Option<&str>) -> Option<&'static str> {
    match level?.trim().to_ascii_lowercase().as_str() {
        "off" | "none" | "disabled" => Some("off"),
        "minimal" | "min" => Some("minimal"),
        "low" => Some("low"),
        "medium" | "med" => Some("medium"),
        "high" => Some("high"),
        "xhigh" | "extra-high" | "extra_high" => Some("xhigh"),
        "max" => Some("max"),
        _ => None,
    }
}
