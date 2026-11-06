//! Tool discovery (upstream coding agent's `extensions/tool-search/tool.ts`):
//! a BM25 ranker over tool metadata, shared by `searchTools()` in codemode
//! scripts and the `tool_search` tool.
//!
//! Text that is not plain ASCII is split into words by Unicode's word
//! boundaries (UAX #29), where upstream uses `Intl.Segmenter`. They agree for
//! scripts written with spaces; Chinese, Japanese and Thai, which ICU splits
//! with dictionaries, come out as one word per ideograph or per run here. See
//! the non-parity register.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;
use unicode_segmentation::UnicodeSegmentation;

pub const DEFAULT_TOOL_SEARCH_LIMIT: usize = 8;

const STOP_WORDS: [&str; 22] = [
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "in", "is", "it", "of", "on",
    "or", "that", "the", "this", "to", "with", "",
];

/// A tool's namespace (upstream `ToolNamespace`), such as an MCP server.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolNamespace {
    pub name: String,
    pub description: Option<String>,
    pub instructions: Option<String>,
    pub aliases: Vec<String>,
}

/// A tool as the ranker sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSearchDocument {
    pub name: String,
    /// Description, schema descriptions and property names.
    pub text: String,
    /// Name, description and instructions of the tool's namespace.
    pub namespace_text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolSearchMatch {
    pub name: String,
    pub score: f64,
}

/// Naive singular form, so `issues` matches `issue`.
fn stem(term: &str) -> String {
    let len = term.len();
    if len > 4 && term.ends_with("ies") {
        return format!("{}y", &term[..len - 3]);
    }
    if len > 4
        && ["ches", "shes", "sses", "xes", "zes"]
            .iter()
            .any(|s| term.ends_with(s))
    {
        return term[..len - 2].to_string();
    }
    if len > 3 && term.ends_with('s') && !term.ends_with("ss") {
        return term[..len - 1].to_string();
    }
    term.to_string()
}

fn regex(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("valid pattern"))
}

/// Unicode words with identifier boundaries, English stop words and
/// singular forms (upstream `tokenize`).
pub fn tokenize(text: &str) -> Vec<String> {
    static LOWER_UPPER: OnceLock<Regex> = OnceLock::new();
    static ACRONYM: OnceLock<Regex> = OnceLock::new();
    static NON_WORD: OnceLock<Regex> = OnceLock::new();
    let normalized = icu_normalizer::ComposingNormalizerBorrowed::new_nfc().normalize(text);
    let split = regex(&LOWER_UPPER, "([a-z0-9])([A-Z])").replace_all(&normalized, "$1 $2");
    let split = regex(&ACRONYM, "([A-Z]+)([A-Z][a-z])").replace_all(&split, "$1 $2");
    let lowered = split.to_lowercase();
    let cleaned = regex(&NON_WORD, r"[^\p{L}\p{M}\p{N}]+").replace_all(&lowered, " ");
    let words: Vec<&str> = if cleaned
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b' ')
    {
        cleaned.split(' ').collect()
    } else {
        cleaned.unicode_words().collect()
    };
    words
        .into_iter()
        .filter(|word| !STOP_WORDS.contains(word))
        .map(|word| {
            if word
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            {
                stem(word)
            } else {
                word.to_string()
            }
        })
        .collect()
}

/// Schema descriptions and property names, recursively, following local
/// `$ref`s into `root` (upstream 6d3e342b). `seen` holds the subschemas
/// already reached through a reference, so recursive and shared
/// definitions are read once, and definitions no property references are
/// not read.
fn schema_text(
    schema: &Value,
    root: &Value,
    parts: &mut Vec<String>,
    seen: &mut Vec<*const Value>,
) {
    let Some(object) = schema.as_object() else {
        return;
    };
    if let Some(Value::String(reference)) = object.get("$ref") {
        if let Some(target) = crate::declarations::resolve_schema_ref(reference, root) {
            let pointer: *const Value = target;
            if target.is_object() && !seen.contains(&pointer) {
                seen.push(pointer);
                schema_text(target, root, parts, seen);
            }
        }
    }
    if let Some(Value::String(description)) = object.get("description") {
        parts.push(description.clone());
    }
    if let Some(Value::Object(properties)) = object.get("properties") {
        for (name, property) in properties {
            parts.push(name.clone());
            schema_text(property, root, parts, seen);
        }
    }
    if let Some(items) = object.get("items") {
        schema_text(items, root, parts, seen);
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(Value::Array(variants)) = object.get(key) {
            for variant in variants {
                schema_text(variant, root, parts, seen);
            }
        }
    }
}

/// A tool's search text: its description, schema descriptions and property
/// names, and separately its namespace's name, description and instructions.
pub fn create_tool_search_document(
    name: &str,
    description: &str,
    parameters: Option<&Value>,
    namespace: Option<&ToolNamespace>,
) -> ToolSearchDocument {
    let mut parts = vec![description.to_string()];
    if let Some(parameters) = parameters {
        let root: *const Value = parameters;
        schema_text(parameters, parameters, &mut parts, &mut vec![root]);
    }
    let namespace_text = namespace.map_or_else(String::new, |namespace| {
        [
            namespace.name.as_str(),
            namespace.description.as_deref().unwrap_or(""),
            namespace.instructions.as_deref().unwrap_or(""),
        ]
        .join(" ")
    });
    let text = parts
        .into_iter()
        .filter(|part| !js_trim(part).is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    ToolSearchDocument {
        name: name.to_string(),
        text,
        namespace_text,
    }
}

fn js_trim(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

/// Lowercase with characters invalid in identifiers as `_` (one per UTF-16
/// unit, as upstream's regular expression replaces them).
fn normalize_name(name: &str) -> String {
    let mut out = String::new();
    for c in js_trim(name).to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '$' {
            out.push(c);
        } else {
            out.extend(std::iter::repeat_n('_', c.len_utf16()));
        }
    }
    out
}

/// Whether the whole query is the tool's name, or its suffix after `__`.
fn is_exact_name_query(query: &str, name: &str) -> bool {
    let normalized = normalize_name(name);
    normalized == query || normalized.ends_with(&format!("__{query}"))
}

struct Indexed<'a> {
    name: &'a str,
    name_terms: HashSet<String>,
    local_name_terms: Vec<String>,
    text_counts: HashMap<String, usize>,
    text_length: usize,
    namespace_terms: HashSet<String>,
}

fn unique(terms: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    terms
        .into_iter()
        .filter(|term| seen.insert(term.clone()))
        .collect()
}

fn index(document: &ToolSearchDocument) -> Indexed<'_> {
    let mut text_counts = HashMap::new();
    let mut text_length = 0;
    for term in tokenize(&document.text) {
        *text_counts.entry(term).or_insert(0) += 1;
        text_length += 1;
    }
    let local = document.name.rsplit("__").next().unwrap_or("");
    Indexed {
        name: &document.name,
        name_terms: tokenize(&document.name).into_iter().collect(),
        local_name_terms: unique(tokenize(local)),
        text_counts,
        text_length,
        namespace_terms: tokenize(&document.namespace_text).into_iter().collect(),
    }
}

/// A query term in the name counts this many times its IDF.
const NAME_WEIGHT: f64 = 3.0;
/// A query term in the namespace counts this many times its IDF.
const NAMESPACE_WEIGHT: f64 = 0.5;

/// Okapi BM25 over the description, with the name and the namespace as
/// separate fields; see upstream's `Bm25Ranker` for the ordering rules.
#[derive(Debug, Clone, Copy)]
pub struct Bm25Ranker {
    pub k1: f64,
    pub b: f64,
}

impl Default for Bm25Ranker {
    fn default() -> Self {
        Self { k1: 1.2, b: 0.75 }
    }
}

impl Bm25Ranker {
    pub fn rank(
        &self,
        query: &str,
        documents: &[ToolSearchDocument],
        limit: usize,
    ) -> Vec<ToolSearchMatch> {
        let query_terms = unique(tokenize(query));
        if query_terms.is_empty() || documents.is_empty() || limit == 0 {
            return Vec::new();
        }
        let indexed: Vec<Indexed<'_>> = documents.iter().map(index).collect();
        let total = indexed.len() as f64;
        let average = indexed.iter().map(|d| d.text_length).sum::<usize>() as f64 / total;
        let average = if average == 0.0 || average.is_nan() {
            1.0
        } else {
            average
        };
        let idf: Vec<f64> = query_terms
            .iter()
            .map(|term| {
                let frequency = indexed
                    .iter()
                    .filter(|d| {
                        d.name_terms.contains(term)
                            || d.text_counts.contains_key(term)
                            || d.namespace_terms.contains(term)
                    })
                    .count() as f64;
                (1.0 + (total - frequency + 0.5) / (frequency + 0.5)).ln()
            })
            .collect();
        let full_name_bonus: f64 = idf.iter().sum();
        let exact_query = normalize_name(query);
        let mut matches: Vec<(ToolSearchMatch, usize, bool)> = Vec::new();
        for document in &indexed {
            let norm = self.k1 * (1.0 - self.b + (self.b * document.text_length as f64) / average);
            let mut score = 0.0;
            let mut covered = 0;
            for (term, idf) in query_terms.iter().zip(&idf) {
                let count = document.text_counts.get(term).copied().unwrap_or(0) as f64;
                let weight = (if document.name_terms.contains(term) {
                    NAME_WEIGHT
                } else {
                    0.0
                }) + (if document.namespace_terms.contains(term) {
                    NAMESPACE_WEIGHT
                } else {
                    0.0
                }) + (count * (self.k1 + 1.0)) / (count + norm);
                if weight > 0.0 {
                    covered += 1;
                }
                score += idf * weight;
            }
            if score == 0.0 {
                continue;
            }
            let local = &document.local_name_terms;
            if !local.is_empty() && local.iter().all(|term| query_terms.contains(term)) {
                score += full_name_bonus;
            }
            matches.push((
                ToolSearchMatch {
                    name: document.name.to_string(),
                    score,
                },
                covered,
                is_exact_name_query(&exact_query, document.name),
            ));
        }
        matches.sort_by(|(a, a_covered, a_exact), (b, b_covered, b_exact)| {
            b_exact.cmp(a_exact).then(b_covered.cmp(a_covered)).then(
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
        });
        matches.into_iter().take(limit).map(|(m, _, _)| m).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn golden() -> Value {
        serde_json::from_str(include_str!(
            "../../../tests/fixtures/ts-parity/tool-search.json"
        ))
        .unwrap()
    }

    /// Scripts that ICU segments with dictionaries; see the module docs.
    fn dictionary_segmented(text: &str) -> bool {
        text.chars().any(|c| {
            matches!(c as u32, 0x0E00..=0x0E7F | 0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF)
        })
    }

    /// A field behind a local `$ref` is searched, a recursive or shared
    /// definition once, and an unreferenced one not at all (upstream
    /// 6d3e342b, MCP servers that put models in `$defs`).
    #[test]
    fn search_text_follows_local_refs_once() {
        let parameters = serde_json::json!({
            "properties": {
                "options": {"$ref": "#/$defs/options"},
                "again": {"$ref": "#/$defs/options"},
                "remote": {"$ref": "https://example.com/schema"}
            },
            "$defs": {
                "options": {
                    "properties": {
                        "mode": {"description": "reconciliation"},
                        "child": {"$ref": "#/$defs/options"}
                    }
                },
                "unused": {"description": "never read"}
            }
        });
        let document =
            create_tool_search_document("inspect", "Inspect records", Some(&parameters), None);
        // Upstream's text, as words (BM25 ignores their order; this map
        // keeps keys sorted): `options` is read once though referenced
        // three times, and `unused` not at all.
        let mut words: Vec<&str> = document.text.split(' ').collect();
        words.sort_unstable();
        assert_eq!(
            words,
            [
                "Inspect",
                "again",
                "child",
                "mode",
                "options",
                "reconciliation",
                "records",
                "remote"
            ]
        );
    }

    #[test]
    fn tokenize_matches_upstream() {
        let golden = golden();
        for case in golden["tokenize"].as_array().unwrap() {
            let text = case["text"].as_str().unwrap();
            if dictionary_segmented(text) {
                continue;
            }
            let expected: Vec<String> = serde_json::from_value(case["terms"].clone()).unwrap();
            assert_eq!(tokenize(text), expected, "tokenize({text:?})");
        }
    }

    #[test]
    fn documents_and_rankings_match_upstream() {
        let golden = golden();
        assert_eq!(golden["defaultLimit"], DEFAULT_TOOL_SEARCH_LIMIT);
        for case in golden["rank"].as_array().unwrap() {
            let documents: Vec<ToolSearchDocument> = case["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| {
                    let namespace = tool.get("namespace").map(|ns| ToolNamespace {
                        name: ns["name"].as_str().unwrap().to_string(),
                        description: ns
                            .get("description")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        instructions: ns
                            .get("instructions")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        aliases: Vec::new(),
                    });
                    create_tool_search_document(
                        tool["name"].as_str().unwrap(),
                        tool["description"].as_str().unwrap(),
                        tool.get("parameters"),
                        namespace.as_ref(),
                    )
                })
                .collect();
            for (document, expected) in documents.iter().zip(case["documents"].as_array().unwrap())
            {
                // Property order differs (sorted keys here); the ranker counts terms.
                let mut words: Vec<&str> = document.text.split(' ').collect();
                let mut expected_words: Vec<&str> =
                    expected["text"].as_str().unwrap().split(' ').collect();
                words.sort_unstable();
                expected_words.sort_unstable();
                assert_eq!(words, expected_words);
                assert_eq!(
                    document.namespace_text,
                    expected["namespaceText"].as_str().unwrap()
                );
            }
            let query = case["query"].as_str().unwrap();
            let limit = case["limit"].as_u64().unwrap() as usize;
            let matches = Bm25Ranker::default().rank(query, &documents, limit);
            let expected = case["matches"].as_array().unwrap();
            let names: Vec<&str> = matches.iter().map(|m| m.name.as_str()).collect();
            let expected_names: Vec<&str> = expected
                .iter()
                .map(|m| m["name"].as_str().unwrap())
                .collect();
            assert_eq!(names, expected_names, "rank({query:?})");
            for (actual, expected) in matches.iter().zip(expected) {
                let expected = expected["score"].as_f64().unwrap();
                assert!(
                    (actual.score - expected).abs() <= 1e-9 * expected.abs(),
                    "{query}: {} vs {expected}",
                    actual.score
                );
            }
        }
    }

    #[test]
    fn dictionary_scripts_still_find_their_words() {
        // Upstream finds `问题` in `搜索仓库问题`; per-ideograph words find it too.
        let documents = [create_tool_search_document(
            "localized",
            "搜索仓库问题",
            None,
            None,
        )];
        assert_eq!(Bm25Ranker::default().rank("问题", &documents, 8).len(), 1);
    }
}
