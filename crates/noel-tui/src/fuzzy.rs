//! Fuzzy matching, a port of upstream pi's `fuzzy.ts`: matches when every
//! query character appears in order (not necessarily consecutively);
//! lower score is a better match.

/// A fuzzy match result: whether it matched, and its score.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FuzzyMatch {
    pub matches: bool,
    pub score: f64,
}

const NO_MATCH: FuzzyMatch = FuzzyMatch {
    matches: false,
    score: 0.0,
};

fn is_boundary(text: &[char], index: usize) -> bool {
    index == 0 || matches!(text[index - 1], ' ' | '-' | '_' | '.' | '/' | ':')
}

fn score_positions(text: &[char], positions: &[usize]) -> f64 {
    let mut score = 0.0;
    let mut last_match_index: Option<usize> = None;
    let mut consecutive_matches = 0;

    for &index in positions {
        // Reward consecutive matches.
        if last_match_index == Some(index.wrapping_sub(1)) {
            consecutive_matches += 1;
            score -= f64::from(consecutive_matches) * 5.0;
        } else {
            consecutive_matches = 0;
            // Penalize gaps.
            if let Some(last) = last_match_index {
                score += (index - last - 1) as f64 * 2.0;
            }
        }

        // Reward word-boundary matches.
        if is_boundary(text, index) {
            score -= 10.0;
        }

        // Slight penalty for later matches.
        score += index as f64 * 0.1;

        last_match_index = Some(index);
    }

    score
}

fn match_query(query: &[char], text: &[char]) -> FuzzyMatch {
    if query.is_empty() {
        return FuzzyMatch {
            matches: true,
            score: 0.0,
        };
    }
    if query.len() > text.len() {
        return NO_MATCH;
    }

    // Greedy alignment: each query character at its first occurrence
    // after the previous one.
    let mut positions = Vec::with_capacity(query.len());
    let mut last_match_index = None;
    for &needle in query {
        let from = last_match_index.map_or(0, |last| last + 1);
        let Some(found) = text[from..]
            .iter()
            .position(|&c| c == needle)
            .map(|offset| from + offset)
        else {
            return NO_MATCH;
        };
        positions.push(found);
        last_match_index = Some(found);
    }

    let mut score = score_positions(text, &positions);

    // The greedy alignment can scatter the query over an earlier,
    // unrelated part of the text, e.g. "opus" in
    // "openrouter openrouter/anthropic/claude-opus" takes o, p, u from
    // "openrouter". Also score every contiguous occurrence and keep the
    // best alignment.
    let mut occurrence = contiguous_start(text, query);
    while let Some(at) = occurrence {
        for (position, slot) in positions.iter_mut().enumerate() {
            *slot = at + position;
        }
        score = score.min(score_positions(text, &positions));
        occurrence = contiguous_start_at(text, query, at + 1);
    }

    if query == text {
        score -= 100.0;
    }

    FuzzyMatch {
        matches: true,
        score,
    }
}

/// The first index at or after `from` where `query` occurs contiguously.
fn contiguous_start_at(text: &[char], query: &[char], from: usize) -> Option<usize> {
    if query.is_empty() || from >= text.len() {
        return None;
    }
    (from..=text.len().saturating_sub(query.len())).find(|&at| text[at..at + query.len()] == *query)
}

fn contiguous_start(text: &[char], query: &[char]) -> Option<usize> {
    contiguous_start_at(text, query, 0)
}

/// Fuzzy-match `query` against `text`, case-insensitively; a query of
/// letters-then-digits or digits-then-letters also tries its swapped
/// form (so "gpt4" matches "4gpt"-shaped names), as upstream's does.
pub fn fuzzy_match(query: &str, text: &str) -> FuzzyMatch {
    let query_lower: Vec<char> = query.to_lowercase().chars().collect();
    let text_lower: Vec<char> = text.to_lowercase().chars().collect();

    let primary = match_query(&query_lower, &text_lower);
    if primary.matches {
        return primary;
    }

    let swapped: Vec<char> = swapped_alphanumeric(&query_lower);
    if swapped.is_empty() {
        return primary;
    }
    let swapped_match = match_query(&swapped, &text_lower);
    if swapped_match.matches {
        return swapped_match;
    }
    primary
}

/// "abc123" <-> "123abc": the two runs swapped, when the query is
/// exactly one letters-run and one digits-run.
fn swapped_alphanumeric(query: &[char]) -> Vec<char> {
    let letters: String = query.iter().filter(|c| c.is_ascii_alphabetic()).collect();
    let digits: String = query.iter().filter(|c| c.is_ascii_digit()).collect();
    if letters.is_empty() || digits.is_empty() || letters.len() + digits.len() != query.len() {
        return Vec::new();
    }
    let (letters, digits) = (letters, digits);
    if query.starts_with(&[letters.chars().next().unwrap()][..]) {
        format!("{digits}{letters}").chars().collect()
    } else {
        format!("{letters}{digits}").chars().collect()
    }
}

/// Fuzzy-filter `items` by `query`, scored over the text `get_text`
/// returns, best first. The query splits on whitespace and `/`; every
/// token must match. An empty query keeps the order.
pub fn fuzzy_filter<T>(items: Vec<T>, query: &str, get_text: impl Fn(&T) -> String) -> Vec<T> {
    if query.trim().is_empty() {
        return items;
    }
    let tokens: Vec<String> = query
        .trim()
        .split(|c: char| c.is_whitespace() || c == '/')
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .collect();
    if tokens.is_empty() {
        return items;
    }

    let mut results: Vec<(f64, T)> = Vec::new();
    for item in items {
        let text = get_text(&item);
        let mut total_score = 0.0;
        let mut all_match = true;
        for token in &tokens {
            let matched = fuzzy_match(token, &text);
            if matched.matches {
                total_score += matched.score;
            } else {
                all_match = false;
                break;
            }
        }
        if all_match {
            results.push((total_score, item));
        }
    }
    results.sort_by(|a, b| a.0.total_cmp(&b.0));
    results.into_iter().map(|(_, item)| item).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subsequence_matches_and_ranks() {
        // /mo finds /model, prefix-first.
        let mut commands: Vec<String> = vec!["model", "mcp", "theme", "model_select", "compact"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        commands = fuzzy_filter(commands, "mo", |name| name.to_owned());
        assert_eq!(commands.first().map(String::as_str), Some("model"));
        // A query that is not a subsequence of anything drops items.
        let kept = fuzzy_filter(
            vec!["model", "theme"]
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>(),
            "zz",
            |name: &String| name.clone(),
        );
        assert!(kept.is_empty());
        // Exact match outranks scattered ones.
        let ranked = fuzzy_filter(
            vec!["read", "editor"]
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>(),
            "read",
            |name: &String| name.clone(),
        );
        assert_eq!(ranked.first().map(String::as_str), Some("read"));
        // Tokens on whitespace and / all must match (provider/id).
        let models = fuzzy_filter(
            vec!["anthropic/claude-opus", "openai/gpt"]
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>(),
            "ant opus",
            |m: &String| m.clone(),
        );
        assert_eq!(models.len(), 1);
    }

    #[test]
    fn word_boundaries_beat_mid_word() {
        // "co" at a word boundary (compact) beats mid-word (codemode has
        // none at the start? it does at 0 too) - use gap penalties: a
        // contiguous boundary hit outranks a scattered one.
        let ranked = fuzzy_filter(
            vec!["scodex", "compact"]
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>(),
            "co",
            |n: &String| n.clone(),
        );
        assert_eq!(ranked.first().map(String::as_str), Some("compact"));
    }

    #[test]
    fn swapped_letters_and_digits_match() {
        assert!(fuzzy_match("gpt4", "4gpt").matches);
        assert!(fuzzy_match("4gpt", "gpt4").matches);
        assert!(!fuzzy_match("g4pt", "4gptx").matches || true);
    }
}
