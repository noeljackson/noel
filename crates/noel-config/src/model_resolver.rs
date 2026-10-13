//! Model resolution, scoping, and initial selection: a port of upstream's
//! `core/model-resolver.ts`. Every function is pure over a list of models
//! in catalog order and a predicate that says which providers have
//! credentials; `model-resolution.json` holds upstream's answers.

use std::cmp::Ordering;

use noel_ai::catalog::{default_models, thinking_level};
use noel_ai::ModelRef;

use crate::ModelDefinition;

/// A model in a scope, with the thinking level its pattern named
/// (`pattern:level`), if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedModel {
    pub model: ModelRef,
    pub thinking_level: Option<String>,
}

/// What a list of scope patterns selects, and a warning for each pattern
/// that selects nothing or names a thinking level that does not exist.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelScope {
    pub models: Vec<ScopedModel>,
    pub diagnostics: Vec<String>,
}

/// What `--model` (with `--provider` and `--thinking`) resolves to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CliModel {
    /// The model, when there is no `error`. With `custom` it is not one of
    /// the known models: its id was built onto the provider's default model.
    pub model: Option<ModelDefinition>,
    pub custom: bool,
    /// The level a `:<thinking>` suffix named.
    pub thinking_level: Option<&'static str>,
    pub warning: Option<String>,
    pub error: Option<String>,
}

/// Upstream `isAlias`: an id without a `-YYYYMMDD` date suffix, or one
/// ending in `-latest`.
fn is_alias(id: &str) -> bool {
    if id.ends_with("-latest") {
        return true;
    }
    let bytes = id.as_bytes();
    let dated = bytes.len() >= 9
        && bytes[bytes.len() - 9] == b'-'
        && bytes[bytes.len() - 8..].iter().all(u8::is_ascii_digit);
    !dated
}

fn same_model(left: &ModelDefinition, right: &ModelDefinition) -> bool {
    left.provider == right.provider && left.id == right.id
}

/// The one element of `matches`, if it has exactly one.
fn only<'a>(mut matches: impl Iterator<Item = &'a ModelDefinition>) -> Option<&'a ModelDefinition> {
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

/// Upstream `findExactModelReferenceMatch`: a model named by `provider/id`
/// or by a bare id, ignoring case. A reference several models answer to
/// matches none.
pub fn find_exact_model_reference_match<'a>(
    reference: &str,
    models: &[&'a ModelDefinition],
) -> Option<&'a ModelDefinition> {
    let reference = reference.trim();
    if reference.is_empty() {
        return None;
    }
    let normalized = reference.to_lowercase();
    let mut canonical = models
        .iter()
        .copied()
        .filter(|model| format!("{}/{}", model.provider, model.id).to_lowercase() == normalized)
        .peekable();
    if canonical.peek().is_some() {
        return only(canonical);
    }
    if let Some((provider, id)) = reference.split_once('/') {
        let (provider, id) = (provider.trim().to_lowercase(), id.trim().to_lowercase());
        if !provider.is_empty() && !id.is_empty() {
            let mut of_provider = models
                .iter()
                .copied()
                .filter(|model| {
                    model.provider.to_lowercase() == provider && model.id.to_lowercase() == id
                })
                .peekable();
            if of_provider.peek().is_some() {
                return only(of_provider);
            }
        }
    }
    only(
        models
            .iter()
            .copied()
            .filter(|model| model.id.to_lowercase() == normalized),
    )
}

/// Upstream `tryMatchModel`: an exact reference, else the best model whose
/// id or name contains the pattern: an alias before a dated id, and among
/// those the id that sorts last.
fn try_match_model<'a>(
    pattern: &str,
    models: &[&'a ModelDefinition],
) -> Option<&'a ModelDefinition> {
    if let Some(exact) = find_exact_model_reference_match(pattern, models) {
        return Some(exact);
    }
    let needle = pattern.to_lowercase();
    let matches = models
        .iter()
        .copied()
        .filter(|model| {
            model.id.to_lowercase().contains(&needle)
                || model
                    .name
                    .as_deref()
                    .is_some_and(|name| name.to_lowercase().contains(&needle))
        })
        .collect::<Vec<_>>();
    // The id that sorts last; of ids that sort alike, the first in catalog
    // order (upstream's sort is stable).
    let highest = |alias: bool| {
        matches
            .iter()
            .copied()
            .filter(|model| is_alias(&model.id) == alias)
            .reduce(|best, model| {
                if locale_compare(&model.id, &best.id) == Ordering::Greater {
                    model
                } else {
                    best
                }
            })
    };
    highest(true).or_else(|| highest(false))
}

struct ParsedModel<'a> {
    model: Option<&'a ModelDefinition>,
    thinking_level: Option<&'static str>,
    warning: Option<String>,
}

/// Upstream `parseModelPattern`: the pattern as a model, else with its
/// last `:suffix` taken as a thinking level. Model ids may contain colons,
/// so the whole pattern is tried first. With `lenient`, a suffix that is
/// not a thinking level is dropped with a warning (scope patterns);
/// without, the pattern matches nothing (`--model`).
fn parse_model_pattern<'a>(
    pattern: &str,
    models: &[&'a ModelDefinition],
    lenient: bool,
) -> ParsedModel<'a> {
    let unmatched = || ParsedModel {
        model: None,
        thinking_level: None,
        warning: None,
    };
    if let Some(model) = try_match_model(pattern, models) {
        return ParsedModel {
            model: Some(model),
            thinking_level: None,
            warning: None,
        };
    }
    let Some((prefix, suffix)) = pattern.rsplit_once(':') else {
        return unmatched();
    };
    if let Some(level) = thinking_level(suffix) {
        let parsed = parse_model_pattern(prefix, models, lenient);
        // The level counts only if nothing further in was dropped.
        let thinking_level = (parsed.model.is_some() && parsed.warning.is_none()).then_some(level);
        return ParsedModel {
            thinking_level,
            ..parsed
        };
    }
    if !lenient {
        return unmatched();
    }
    let parsed = parse_model_pattern(prefix, models, lenient);
    if parsed.model.is_none() {
        return parsed;
    }
    ParsedModel {
        model: parsed.model,
        thinking_level: None,
        warning: Some(format!(
            "Invalid thinking level \"{suffix}\" in pattern \"{pattern}\". Using default instead."
        )),
    }
}

/// Upstream `resolveModelScopeFromModels`: what `--models` or the
/// `enabledModels` setting selects from the models with credentials. A
/// pattern is an exact reference, a fuzzy match, or (with `*`, `?`, or `[`)
/// a glob over `provider/id` and over the id, ignoring case; each may end
/// in `:<thinking>`.
pub fn resolve_model_scope(patterns: &[String], available: &[&ModelDefinition]) -> ModelScope {
    let mut scope = ModelScope::default();
    let add = |scope: &mut ModelScope, model: &ModelDefinition, level: Option<&str>| {
        let model = model.model_ref();
        if !scope.models.iter().any(|scoped| scoped.model == model) {
            scope.models.push(ScopedModel {
                model,
                thinking_level: level.map(str::to_string),
            });
        }
    };
    let no_match = |pattern: &str| format!("No models match pattern \"{pattern}\"");
    for pattern in patterns {
        if pattern.contains(['*', '?', '[']) {
            let (glob, level) = match pattern.rsplit_once(':') {
                Some((glob, suffix)) if thinking_level(suffix).is_some() => {
                    (glob, thinking_level(suffix))
                }
                _ => (pattern.as_str(), None),
            };
            if let Some(exact) = find_exact_model_reference_match(glob, available) {
                add(&mut scope, exact, level);
                continue;
            }
            let mut matched = false;
            for model in available {
                let full_id = format!("{}/{}", model.provider, model.id);
                if glob_match(glob, &full_id) || glob_match(glob, &model.id) {
                    add(&mut scope, model, level);
                    matched = true;
                }
            }
            if !matched {
                scope.diagnostics.push(no_match(pattern));
            }
            continue;
        }
        let parsed = parse_model_pattern(pattern, available, true);
        scope.diagnostics.extend(parsed.warning);
        match parsed.model {
            Some(model) => add(&mut scope, model, parsed.thinking_level),
            None => scope.diagnostics.push(no_match(pattern)),
        }
    }
    scope
}

/// Upstream `buildFallbackModel`: `id` as a model of `provider`, with
/// everything else taken from the provider's default model (or its first).
/// `None` for a provider without models.
pub fn custom_model(
    models: &[ModelDefinition],
    provider: &str,
    id: &str,
) -> Option<ModelDefinition> {
    let of_provider = || models.iter().filter(|model| model.provider == provider);
    let base = default_models()
        .iter()
        .find(|(candidate, _)| candidate == provider)
        .and_then(|(_, default_id)| of_provider().find(|model| model.id == *default_id))
        .or_else(|| of_provider().next())?;
    Some(ModelDefinition {
        id: id.to_string(),
        name: Some(id.to_string()),
        ..base.clone()
    })
}

/// Upstream `resolveCliModel`: the model `--model` names, among all models
/// (not only those with credentials, so `--api-key` works on first use).
///
/// `provider/pattern` prefers the provider reading over an id that itself
/// contains the slash; a bare id several providers have goes to the one
/// provider with credentials, else it is an error; a pattern may be a
/// fuzzy match and may end in `:<thinking>`; and an id a known provider
/// does not list becomes a custom model with a warning.
pub fn resolve_cli_model(
    cli_provider: Option<&str>,
    cli_model: &str,
    cli_thinking: Option<&str>,
    models: &[ModelDefinition],
    has_auth: &dyn Fn(&str) -> bool,
) -> CliModel {
    if cli_model.is_empty() {
        return CliModel::default();
    }
    let error = |error: String| CliModel {
        error: Some(error),
        ..CliModel::default()
    };
    let found = |model: &ModelDefinition| CliModel {
        model: Some(model.clone()),
        ..CliModel::default()
    };
    if models.is_empty() {
        return error(
            "No models available. Check your installation or add models to models.json."
                .to_string(),
        );
    }
    let all = models.iter().collect::<Vec<_>>();
    // The provider id as the catalog spells it; the last spelling wins, as
    // in upstream's map.
    let canonical_provider = |name: &str| {
        let name = name.to_lowercase();
        all.iter()
            .rev()
            .find(|model| model.provider.to_lowercase() == name)
            .map(|model| model.provider.as_str())
    };
    let is_exact = |model: &&ModelDefinition| {
        let lower = cli_model.to_lowercase();
        model.id.to_lowercase() == lower
            || format!("{}/{}", model.provider, model.id).to_lowercase() == lower
    };

    let mut provider = match cli_provider {
        Some(name) => match canonical_provider(name) {
            Some(provider) => Some(provider),
            None => {
                return error(format!(
                "Unknown provider \"{name}\". Use --list-models to see available providers/models."
            ))
            }
        },
        None => None,
    };
    let mut pattern = cli_model;
    let mut inferred_provider = false;
    if provider.is_none() {
        if let Some((prefix, rest)) = cli_model.split_once('/') {
            if let Some(canonical) = canonical_provider(prefix) {
                provider = Some(canonical);
                pattern = rest;
                inferred_provider = true;
            }
        }
    }
    // No provider: an exact id or `provider/id`. Several providers may have
    // the id; only credentials can choose between them.
    if provider.is_none() {
        let exact = all.iter().copied().filter(is_exact).collect::<Vec<_>>();
        if let [model] = exact[..] {
            return found(model);
        }
        if exact.len() > 1 {
            let authenticated = exact
                .iter()
                .copied()
                .filter(|model| has_auth(&model.provider))
                .collect::<Vec<_>>();
            if let [model] = authenticated[..] {
                return found(model);
            }
            let mut names = exact
                .iter()
                .map(|model| format!("{}/{}", model.provider, model.id))
                .collect::<Vec<_>>();
            names.sort_by(|left, right| locale_compare(left, right));
            let hint = if authenticated.is_empty() {
                "No matching provider is authenticated."
            } else {
                "More than one matching provider is authenticated."
            };
            return error(format!(
                "Model \"{cli_model}\" is ambiguous across providers: {}. {hint} Use --provider or provider/model.",
                names.join(", ")
            ));
        }
    }
    if let (Some(_), Some(provider)) = (cli_provider, provider) {
        // `--provider p --model p/pattern` is tolerated.
        let prefix = format!("{provider}/");
        if cli_model
            .get(..prefix.len())
            .is_some_and(|start| start.to_lowercase() == prefix.to_lowercase())
        {
            pattern = &cli_model[prefix.len()..];
        }
    }

    let candidates = all
        .iter()
        .copied()
        .filter(|model| provider.is_none_or(|provider| model.provider == provider))
        .collect::<Vec<_>>();
    let parsed = parse_model_pattern(pattern, &candidates, false);
    if let Some(model) = parsed.model {
        // The provider reading matched a provider without credentials: an
        // id that literally starts with that provider's name, at the one
        // provider with credentials, is the better reading.
        if inferred_provider && !has_auth(&model.provider) {
            let lower = cli_model.to_lowercase();
            let raw = all
                .iter()
                .copied()
                .filter(|other| other.id.to_lowercase() == lower && !same_model(other, model))
                .filter(|other| has_auth(&other.provider))
                .collect::<Vec<_>>();
            if let [other] = raw[..] {
                return found(other);
            }
        }
        return CliModel {
            model: Some(model.clone()),
            custom: false,
            thinking_level: parsed.thinking_level,
            warning: parsed.warning,
            error: None,
        };
    }
    // Nothing under the inferred provider: the whole text may be an id
    // that merely starts with a provider's name.
    if inferred_provider {
        if let Some(model) = all.iter().copied().find(is_exact) {
            return found(model);
        }
        let fallback = parse_model_pattern(cli_model, &all, false);
        if let Some(model) = fallback.model {
            return CliModel {
                model: Some(model.clone()),
                custom: false,
                thinking_level: fallback.thinking_level,
                warning: fallback.warning,
                error: None,
            };
        }
    }
    if let Some(provider) = provider {
        // A `:<thinking>` suffix is read here only without `--thinking`.
        let (id, suffix_level) = match pattern.rsplit_once(':') {
            Some((id, suffix)) if cli_thinking.is_none() && thinking_level(suffix).is_some() => {
                (id, thinking_level(suffix))
            }
            _ => (pattern, None),
        };
        if let Some(mut model) = custom_model(models, provider, id) {
            // Asking for thinking says the model can think.
            if cli_thinking
                .or(suffix_level)
                .is_some_and(|level| level != "off")
            {
                model.reasoning = Some(true);
            }
            let not_found = format!(
                "Model \"{id}\" not found for provider \"{provider}\". Using custom model id."
            );
            return CliModel {
                model: Some(model),
                custom: true,
                thinking_level: suffix_level,
                warning: Some(match parsed.warning {
                    Some(warning) => format!("{warning} {not_found}"),
                    None => not_found,
                }),
                error: None,
            };
        }
    }
    let display = match provider {
        Some(provider) => format!("{provider}/{pattern}"),
        None => cli_model.to_string(),
    };
    CliModel {
        warning: parsed.warning,
        ..error(format!(
            "Model \"{display}\" not found. Use --list-models to see available models."
        ))
    }
}

/// Upstream `findInitialModel` without a `--model` or a scope: the saved
/// default if it exists and its provider has credentials, else a model with
/// credentials, the first provider's default model (in upstream's provider
/// order) before the first model in catalog order.
pub fn find_initial_model<'a>(
    models: &[&'a ModelDefinition],
    default_provider: Option<&str>,
    default_model: Option<&str>,
    has_auth: &dyn Fn(&str) -> bool,
) -> Option<&'a ModelDefinition> {
    if let (Some(provider), Some(id)) = (default_provider, default_model) {
        let saved = models
            .iter()
            .copied()
            .find(|model| model.provider == provider && model.id == id);
        if let Some(model) = saved.filter(|model| has_auth(&model.provider)) {
            return Some(model);
        }
    }
    let available = models
        .iter()
        .copied()
        .filter(|model| has_auth(&model.provider))
        .collect::<Vec<_>>();
    default_models()
        .iter()
        .find_map(|(provider, id)| {
            available
                .iter()
                .copied()
                .find(|model| model.provider == *provider && model.id == *id)
        })
        .or_else(|| available.first().copied())
}

/// Orders text as upstream's `localeCompare` does (the Unicode collation
/// order): punctuation before digits before letters, case decided last,
/// lowercase first. Other characters sort after ASCII by code point; model
/// ids are ASCII.
fn locale_compare(left: &str, right: &str) -> Ordering {
    const ASCII_ORDER: &str =
        " _-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$0123456789abcdefghijklmnopqrstuvwxyz";
    let rank = |character: char| {
        let lower = character.to_ascii_lowercase();
        match ASCII_ORDER.find(lower) {
            Some(rank) => rank as u32,
            None if character.is_ascii() => 0,
            None => ASCII_ORDER.len() as u32 + character as u32,
        }
    };
    left.chars()
        .map(rank)
        .cmp(right.chars().map(rank))
        .then_with(|| {
            // Equal apart from case: lowercase sorts first.
            left.chars()
                .map(|character| character.is_ascii_uppercase())
                .cmp(
                    right
                        .chars()
                        .map(|character| character.is_ascii_uppercase()),
                )
        })
}

/// Matches `text` against a glob as upstream's `minimatch(text, pattern,
/// { nocase: true })` does for the syntax model patterns use: `*` and `?`
/// within one `/`-separated segment, `**` for any number of segments,
/// `[...]` classes, `{a,b}` and `{1..3}` alternatives, and a leading `!` to
/// negate. Extended globs (`+(a|b)`) are not supported.
fn glob_match(pattern: &str, text: &str) -> bool {
    if pattern.starts_with('#') {
        return false;
    }
    let negations = pattern
        .chars()
        .take_while(|character| *character == '!')
        .count();
    let pattern = &pattern[negations..];
    let text = text.to_lowercase();
    let segments = text.split('/').collect::<Vec<_>>();
    let matched = expand_braces(&pattern.to_lowercase())
        .iter()
        .any(|pattern| match_segments(&pattern.split('/').collect::<Vec<_>>(), &segments));
    matched != (negations % 2 == 1)
}

/// `a{b,c}d` as `abd` and `acd`, and `a{1..3}` as `a1`, `a2`, `a3`; other
/// braces are literal.
fn expand_braces(pattern: &str) -> Vec<String> {
    let bytes = pattern.as_bytes();
    let mut open = None;
    let mut depth = 0;
    let mut commas = Vec::new();
    for (index, byte) in bytes.iter().enumerate() {
        match byte {
            b'{' => {
                if depth == 0 {
                    open = Some(index);
                    commas.clear();
                }
                depth += 1;
            }
            b',' if depth == 1 => commas.push(index),
            b'}' if depth > 0 => {
                depth -= 1;
                if depth > 0 {
                    continue;
                }
                let start = open.expect("an open brace");
                let with = |alternative: &str| {
                    expand_braces(&format!(
                        "{}{alternative}{}",
                        &pattern[..start],
                        &pattern[index + 1..]
                    ))
                };
                if !commas.is_empty() {
                    let mut bounds = vec![start];
                    bounds.extend(&commas);
                    bounds.push(index);
                    return bounds
                        .windows(2)
                        .flat_map(|pair| with(&pattern[pair[0] + 1..pair[1]]))
                        .collect();
                }
                let range = pattern[start + 1..index]
                    .split_once("..")
                    .and_then(|(low, high)| {
                        Some((low.parse::<i64>().ok()?, high.parse::<i64>().ok()?))
                    });
                if let Some((low, high)) = range {
                    let numbers = low.min(high)..=low.max(high);
                    return numbers
                        .take(1000)
                        .flat_map(|number| with(&number.to_string()))
                        .collect();
                }
            }
            _ => {}
        }
    }
    vec![pattern.to_string()]
}

fn match_segments(pattern: &[&str], text: &[&str]) -> bool {
    let visible = |segments: &[&str]| segments.iter().all(|segment| !segment.starts_with('.'));
    match pattern.split_first() {
        None => text.is_empty(),
        // Text has run out with pattern left, even a `**`.
        Some(_) if text.is_empty() => false,
        // A last `**` takes the rest; one before more pattern takes any
        // number of segments. It never takes a hidden one.
        Some((&"**", [])) => visible(text),
        Some((&"**", rest)) => (0..text.len())
            .take_while(|count| visible(&text[..*count]))
            .any(|count| match_segments(rest, &text[count..])),
        Some((first, rest)) => match_segment(first, text[0]) && match_segments(rest, &text[1..]),
    }
}

/// One segment. A pattern with `*`, `?`, or `[` needs a character to match
/// and never matches a leading dot it does not spell out.
fn match_segment(pattern: &str, text: &str) -> bool {
    let pattern = pattern.chars().collect::<Vec<_>>();
    let text = text.chars().collect::<Vec<_>>();
    if text.first() == Some(&'.') && pattern.first() != Some(&'.') {
        return false;
    }
    if text.is_empty() && pattern.iter().any(|character| "*?[".contains(*character)) {
        return false;
    }
    match_chars(&pattern, &text)
}

fn match_chars(pattern: &[char], text: &[char]) -> bool {
    match pattern.split_first() {
        None => text.is_empty(),
        Some(('*', rest)) => (0..=text.len()).any(|skip| match_chars(rest, &text[skip..])),
        Some(('?', rest)) => !text.is_empty() && match_chars(rest, &text[1..]),
        Some(('[', rest)) => match (parse_class(rest), text.split_first()) {
            (Some((class, rest)), Some((character, text))) => {
                class.contains(*character) && match_chars(rest, text)
            }
            (Some(_), None) => false,
            // An unclosed bracket is a literal one.
            (None, _) => text.first() == Some(&'[') && match_chars(rest, &text[1..]),
        },
        Some(('\\', rest)) if !rest.is_empty() => {
            text.first() == Some(&rest[0]) && match_chars(&rest[1..], &text[1..])
        }
        Some((literal, rest)) => text.first() == Some(literal) && match_chars(rest, &text[1..]),
    }
}

struct CharClass {
    negated: bool,
    ranges: Vec<(char, char)>,
}

impl CharClass {
    fn contains(&self, character: char) -> bool {
        self.ranges
            .iter()
            .any(|(low, high)| (*low..=*high).contains(&character))
            != self.negated
    }
}

/// The class after a `[`, and the pattern after its `]`.
fn parse_class(pattern: &[char]) -> Option<(CharClass, &[char])> {
    let negated = matches!(pattern.first(), Some('!' | '^'));
    let body = &pattern[usize::from(negated)..];
    // A `]` right after the opening is a member, not the end.
    let end = body
        .iter()
        .skip(1)
        .position(|character| *character == ']')?
        + 1;
    let (members, rest) = (&body[..end], &body[end + 1..]);
    let mut ranges = Vec::new();
    let mut index = 0;
    while index < members.len() {
        if index + 2 < members.len() && members[index + 1] == '-' {
            ranges.push((members[index], members[index + 2]));
            index += 3;
        } else {
            ranges.push((members[index], members[index]));
            index += 1;
        }
    }
    Some((CharClass { negated, ranges }, rest))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProviderApi;
    use serde_json::{json, Value};

    fn fixture() -> Value {
        serde_json::from_str(include_str!(
            "../../../tests/fixtures/ts-parity/model-resolution.json"
        ))
        .expect("parse model-resolution.json")
    }

    /// Upstream's whole catalog in upstream's order, the list upstream's
    /// functions ran on. Radius models, which noel does not offer, stand in
    /// with a placeholder API: resolution reads only provider, id, and name.
    fn upstream_models() -> Vec<ModelDefinition> {
        let catalog: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/ts-parity/model-catalog.json"
        ))
        .expect("parse model-catalog.json");
        let mut models = Vec::new();
        for provider in catalog["providers"].as_array().expect("providers") {
            for model in provider["models"].as_array().expect("models") {
                let (provider, id) = (
                    model["provider"].as_str().expect("provider"),
                    model["id"].as_str().expect("id"),
                );
                models.push(match noel_ai::catalog::builtin_model(provider, id) {
                    Some(model) => ModelDefinition::from(model),
                    None => {
                        assert_eq!(provider, "radius");
                        ModelDefinition {
                            provider: provider.to_string(),
                            id: id.to_string(),
                            name: model["name"].as_str().map(str::to_string),
                            ..ModelDefinition::default()
                        }
                    }
                });
            }
        }
        models
    }

    fn strings(value: &Value) -> Vec<String> {
        value
            .as_array()
            .expect("array")
            .iter()
            .map(|entry| entry.as_str().expect("string").to_string())
            .collect()
    }

    fn model_ref(model: Option<&ModelDefinition>) -> Value {
        match model {
            Some(model) => json!({ "provider": model.provider, "id": model.id }),
            None => Value::Null,
        }
    }

    /// A model as upstream's object, with the fields noel keeps.
    fn model_fields(model: &ModelDefinition) -> Value {
        let mut fields = json!({
            "id": model.id,
            "name": model.name,
            "api": model.api,
            "provider": model.provider,
            "baseUrl": model.base_url.clone().unwrap_or_default(),
            "reasoning": model.reasoning(),
            "input": model.input,
            "cost": model.cost,
            "contextWindow": model.context_window,
            "maxTokens": model.max_tokens,
        });
        if let Some(map) = &model.thinking_level_map {
            fields["thinkingLevelMap"] = json!(map);
        }
        fields
    }

    #[test]
    fn cli_model_resolution_matches_ts() {
        let models = upstream_models();
        let cases = fixture()["cliModel"].clone();
        let cases = cases.as_array().expect("cliModel cases");
        assert!(cases.len() > 40, "{} cases", cases.len());
        for case in cases {
            let input = &case["input"];
            let authenticated = strings(&input["authenticated"]);
            let resolved = resolve_cli_model(
                input["provider"].as_str(),
                input["model"].as_str().expect("model"),
                input["thinking"].as_str(),
                &models,
                &|provider| authenticated.iter().any(|name| name == provider),
            );
            let mut actual = json!({
                "model": model_ref(resolved.model.as_ref()),
                "thinkingLevel": resolved.thinking_level,
                "warning": resolved.warning,
                "error": resolved.error,
            });
            if resolved.custom {
                actual["custom"] = model_fields(resolved.model.as_ref().expect("custom model"));
            }
            assert_eq!(actual, case["expected"], "{input}");
        }
    }

    #[test]
    fn model_scope_resolution_matches_ts() {
        let models = upstream_models();
        let cases = fixture()["modelScope"].clone();
        let cases = cases.as_array().expect("modelScope cases");
        assert!(cases.len() > 10, "{} cases", cases.len());
        for case in cases {
            let input = &case["input"];
            let authenticated = strings(&input["authenticated"]);
            let available = models
                .iter()
                .filter(|model| authenticated.contains(&model.provider))
                .collect::<Vec<_>>();
            let scope = resolve_model_scope(&strings(&input["patterns"]), &available);
            let actual = json!({
                "models": scope.models.iter().map(|scoped| json!({
                    "provider": scoped.model.provider,
                    "id": scoped.model.id,
                    "thinkingLevel": scoped.thinking_level,
                })).collect::<Vec<_>>(),
                "diagnostics": scope.diagnostics,
            });
            assert_eq!(actual, case["expected"], "{input}");
        }
    }

    #[test]
    fn exact_model_references_match_ts() {
        let models = upstream_models();
        let cases = fixture()["exactReference"].clone();
        for case in cases.as_array().expect("exactReference cases") {
            let input = &case["input"];
            let authenticated = strings(&input["authenticated"]);
            let custom = input["custom"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|model| ModelDefinition {
                    provider: model["provider"].as_str().expect("provider").to_string(),
                    id: model["id"].as_str().expect("id").to_string(),
                    ..ModelDefinition::default()
                })
                .collect::<Vec<_>>();
            let available = models
                .iter()
                .filter(|model| authenticated.contains(&model.provider))
                .chain(&custom)
                .collect::<Vec<_>>();
            let found = find_exact_model_reference_match(
                input["reference"].as_str().expect("reference"),
                &available,
            );
            assert_eq!(model_ref(found), case["expected"], "{input}");
        }
    }

    #[test]
    fn initial_model_selection_matches_ts() {
        let models = upstream_models();
        let all = models.iter().collect::<Vec<_>>();
        let cases = fixture()["initialModel"].clone();
        for case in cases.as_array().expect("initialModel cases") {
            let input = &case["input"];
            let authenticated = strings(&input["authenticated"]);
            let found = find_initial_model(
                &all,
                input["defaultProvider"].as_str(),
                input["defaultModel"].as_str(),
                &|provider| authenticated.iter().any(|name| name == provider),
            );
            assert_eq!(model_ref(found), case["expected"], "{input}");
        }
    }

    #[test]
    fn text_sorts_in_collation_order() {
        let mut ids = vec![
            "gpt-5",
            "gpt-5.5",
            "gpt-5-mini",
            "gpt-5.5-pro",
            "GPT-5",
            "gpt_5",
            "gpt-realtime",
            "gpt 5",
            "gpt-50",
        ];
        ids.sort_by(|left, right| locale_compare(left, right));
        assert_eq!(
            ids,
            [
                "gpt 5",
                "gpt_5",
                "gpt-5",
                "GPT-5",
                "gpt-5-mini",
                "gpt-5.5",
                "gpt-5.5-pro",
                "gpt-50",
                "gpt-realtime",
            ]
        );
    }

    #[test]
    fn globs_match_like_minimatch() {
        let cases = fixture()["globs"].clone();
        let cases = cases.as_array().expect("glob cases");
        assert!(cases.len() > 40, "{} cases", cases.len());
        for case in cases {
            let (pattern, text) = (
                case["pattern"].as_str().expect("pattern"),
                case["text"].as_str().expect("text"),
            );
            assert_eq!(
                glob_match(pattern, text),
                case["matches"].as_bool().expect("matches"),
                "{pattern} on {text}"
            );
        }
        // Not supported: minimatch's extended globs.
        assert!(!glob_match("+(a|b)*", "ab"));
    }

    #[test]
    fn a_custom_model_takes_the_providers_default() {
        let models = upstream_models();
        let custom = custom_model(&models, "anthropic", "claude-next").expect("custom model");
        let default_id = default_models()
            .iter()
            .find(|(provider, _)| provider == "anthropic")
            .map(|(_, id)| id)
            .expect("anthropic's default model");
        let base = models
            .iter()
            .find(|model| model.provider == "anthropic" && model.id == *default_id)
            .expect("default model");
        assert_eq!(custom.id, "claude-next");
        assert_eq!(custom.name.as_deref(), Some("claude-next"));
        assert_eq!(custom.max_tokens, base.max_tokens);
        assert_eq!(custom.thinking_level_map, base.thinking_level_map);
        // A provider without a default model: its first model.
        let local = [ModelDefinition {
            provider: "local".to_string(),
            id: "first".to_string(),
            api: ProviderApi::OpenAi,
            context_window: Some(8192),
            ..ModelDefinition::default()
        }];
        let custom = custom_model(&local, "local", "second").expect("custom model");
        assert_eq!(
            (custom.id.as_str(), custom.context_window),
            ("second", Some(8192))
        );
        assert_eq!(custom_model(&local, "elsewhere", "x"), None);
    }
}
