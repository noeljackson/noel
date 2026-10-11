//! In-repo markdown rendering for transcript entries.
//!
//! Emits byte-ranged styled spans so the transcript view can apply styles
//! without disturbing selection anchors: every span records its source byte
//! range in the original entry text, and for inline emphasis the display
//! text is identical to the source text (only the style differs). Block
//! transformations (headings, lists, fences) may change display text and
//! rely on the view's source/display split used by folded previews.

use ratatui::style::{Modifier, Style};
use std::ops::Range;

/// Styling intent for a span; mapped to ratatui styles by the caller so the
/// parser stays free of theme knowledge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanStyle {
    Plain,
    Bold,
    Italic,
    Code,
    Strike,
    Link,
}

/// One styled run over assistant markdown text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyledSpan {
    /// Byte range in the source entry text this span renders.
    pub source: Range<usize>,
    pub style: SpanStyle,
    /// Displayed text. Equals the source slice for inline emphasis; block
    /// transforms (heading markers, fences) may replace it.
    pub display: String,
}

/// Parse inline emphasis on a single line into styled spans.
///
/// Supported, matching the common agentic-CLI subset:
/// - `` `code` ``         → Code
/// - `**bold**`           → Bold
/// - `*italic*`, `_it_`   → Italic
/// - `~~strike~~`         → Strike
/// - `[text](url)`        → Link (display: text)
///
/// Delimiters must open/close within the line; unmatched markers render
/// literally. Parsing is deliberately conservative: no nesting inside code
/// spans, no bold-in-italic combinations.
pub fn parse_inline_line(line: &str) -> Vec<StyledSpan> {
    let bytes = line.as_bytes();
    let mut spans: Vec<StyledSpan> = Vec::new();
    let mut plain_start = 0usize;
    let mut index = 0usize;

    let flush_plain = |spans: &mut Vec<StyledSpan>, start: usize, end: usize| {
        if end > start {
            spans.push(StyledSpan {
                source: start..end,
                style: SpanStyle::Plain,
                display: line[start..end].to_string(),
            });
        }
    };

    while index < bytes.len() {
        let rest = &line[index..];
        // Code spans take priority and suppress other emphasis inside.
        if rest.starts_with('`') {
            if let Some(close) = find_code_close(line, index) {
                flush_plain(&mut spans, plain_start, index);
                spans.push(StyledSpan {
                    source: index..close + 1,
                    style: SpanStyle::Code,
                    display: line[index..close + 1].to_string(),
                });
                index = close + 1;
                plain_start = index;
                continue;
            }
        }
        if rest.starts_with("**") {
            if let Some(content_end) = find_double_marker(line, index + 2, b'*') {
                flush_plain(&mut spans, plain_start, index);
                spans.push(StyledSpan {
                    source: index..content_end + 2,
                    style: SpanStyle::Bold,
                    display: line[index..content_end + 2].to_string(),
                });
                index = content_end + 2;
                plain_start = index;
                continue;
            }
        }
        if rest.starts_with("~~") {
            if let Some(content_end) = find_double_marker(line, index + 2, b'~') {
                flush_plain(&mut spans, plain_start, index);
                spans.push(StyledSpan {
                    source: index..content_end + 2,
                    style: SpanStyle::Strike,
                    display: line[index..content_end + 2].to_string(),
                });
                index = content_end + 2;
                plain_start = index;
                continue;
            }
        }
        if rest.starts_with('[') {
            if let Some((link_end, text_end)) = find_link(line, index) {
                flush_plain(&mut spans, plain_start, index);
                // Display only the link text; the URL stays selectable in
                // the source range.
                spans.push(StyledSpan {
                    source: index..link_end,
                    style: SpanStyle::Link,
                    display: line[index + 1..text_end].to_string(),
                });
                index = link_end;
                plain_start = index;
                continue;
            }
        }
        let single = rest
            .chars()
            .next()
            .map(|ch| (ch == '*' || ch == '_') && !rest.starts_with("**"))
            .unwrap_or(false);
        if single {
            if let Some(content_end) = find_single_emphasis_close(line, index) {
                flush_plain(&mut spans, plain_start, index);
                spans.push(StyledSpan {
                    source: index..content_end + 1,
                    style: SpanStyle::Italic,
                    display: line[index..content_end + 1].to_string(),
                });
                index = content_end + 1;
                plain_start = index;
                continue;
            }
        }
        index += rest.chars().next().map(char::len_utf8).unwrap_or(1);
    }
    flush_plain(&mut spans, plain_start, line.len());
    spans
}

/// Closing backtick for a code span opened at `open`, or None. The opening
/// marker must not be adjacent to whitespace on the inside.
fn find_code_close(line: &str, open: usize) -> Option<usize> {
    let bytes = line.as_bytes();
    let content_start = open + 1;
    if content_start >= bytes.len() || bytes[content_start].is_ascii_whitespace() {
        return None;
    }
    let mut index = content_start;
    while index < bytes.len() {
        if bytes[index] == b'`' {
            // Avoid `` ` `` empty or whitespace-adjacent closings.
            if index > content_start && !bytes[index - 1].is_ascii_whitespace() {
                return Some(index);
            }
            return None;
        }
        index += 1;
    }
    None
}

/// Find `marker marker` closing a double-delimiter run opened just before
/// `search_start` (i.e. content begins at search_start). Returns the byte
/// index of the first of the two closing markers.
fn find_double_marker(line: &str, search_start: usize, marker: u8) -> Option<usize> {
    let bytes = line.as_bytes();
    let content_start = search_start;
    if content_start >= bytes.len() || bytes[content_start].is_ascii_whitespace() {
        return None;
    }
    let mut index = content_start;
    while index + 1 < bytes.len() {
        if bytes[index] == marker && bytes[index + 1] == marker {
            if index > content_start && !bytes[index - 1].is_ascii_whitespace() {
                return Some(index);
            }
            return None;
        }
        index += 1;
    }
    None
}

/// Closing `*` or `_` for single-marker emphasis opened at `open`.
/// Intra-word underscores are identifiers (snake_case), not emphasis: the
/// character before the opener must not be alphanumeric.
fn find_single_emphasis_close(line: &str, open: usize) -> Option<usize> {
    let bytes = line.as_bytes();
    if open > 0 && bytes[open - 1].is_ascii_alphanumeric() {
        return None;
    }
    let search_start = open + 1;
    if search_start >= bytes.len() || bytes[search_start].is_ascii_whitespace() {
        return None;
    }
    let mut index = search_start;
    while index < bytes.len() {
        let ch = bytes[index];
        if ch == b'*' || ch == b'_' {
            // Closing marker must not be followed by a word character
            // (that would be the inside of snake_case or a*b math).
            if index + 1 < bytes.len() && bytes[index + 1].is_ascii_alphanumeric() {
                return None;
            }
            if index > search_start && !bytes[index - 1].is_ascii_whitespace() {
                return Some(index);
            }
            return None;
        }
        index += 1;
    }
    None
}

/// Match `[text](url)` starting at `open`. Returns (end_of_url_paren,
/// end_of_text).
fn find_link(line: &str, open: usize) -> Option<(usize, usize)> {
    let bytes = line.as_bytes();
    let mut index = open + 1;
    while index < bytes.len() && bytes[index] != b']' && bytes[index] != b'[' {
        index += 1;
    }
    if index >= bytes.len() || bytes[index] != b']' {
        return None;
    }
    let text_end = index;
    if text_end == open + 1 {
        return None; // empty link text
    }
    let url_start = text_end + 1;
    if url_start >= bytes.len() || bytes[url_start] != b'(' {
        return None;
    }
    let mut close = url_start + 1;
    while close < bytes.len() && bytes[close] != b')' {
        if bytes[close] == b' ' {
            return None;
        }
        close += 1;
    }
    if close >= bytes.len() {
        return None;
    }
    Some((close + 1, text_end))
}

/// Map a parsed span style to a ratatui style for a given accent color.
/// Kept here so the theme mapping lives next to the parser, but callers
/// pass their palette so themes stay in control of colors.
pub fn ratatui_style(style: SpanStyle, accent: ratatui::style::Color) -> Style {
    match style {
        SpanStyle::Plain => Style::default(),
        SpanStyle::Bold => Style::default().add_modifier(Modifier::BOLD),
        SpanStyle::Italic => Style::default().add_modifier(Modifier::ITALIC),
        SpanStyle::Code => Style::default().fg(accent),
        SpanStyle::Strike => Style::default().add_modifier(Modifier::CROSSED_OUT),
        SpanStyle::Link => Style::default()
            .fg(accent)
            .add_modifier(Modifier::UNDERLINED),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn styles(line: &str) -> Vec<(SpanStyle, String)> {
        parse_inline_line(line)
            .into_iter()
            .map(|span| (span.style, span.display))
            .collect()
    }

    #[test]
    fn plain_text_is_one_plain_span() {
        assert_eq!(
            styles("just words"),
            vec![(SpanStyle::Plain, "just words".into())]
        );
    }

    #[test]
    fn bold_italic_code_strike_and_links_parse() {
        assert_eq!(
            styles("**bold** rest"),
            vec![
                (SpanStyle::Bold, "**bold**".into()),
                (SpanStyle::Plain, " rest".into()),
            ]
        );
        assert_eq!(
            styles("a *it* b"),
            vec![
                (SpanStyle::Plain, "a ".into()),
                (SpanStyle::Italic, "*it*".into()),
                (SpanStyle::Plain, " b".into()),
            ]
        );
        assert_eq!(
            styles("`cargo build`"),
            vec![(SpanStyle::Code, "`cargo build`".into())]
        );
        assert_eq!(
            styles("~~gone~~"),
            vec![(SpanStyle::Strike, "~~gone~~".into())]
        );
        assert_eq!(
            styles("[docs](https://example.org)"),
            vec![(SpanStyle::Link, "docs".into())]
        );
    }

    #[test]
    fn unmatched_markers_stay_literal() {
        assert_eq!(styles("a * b"), vec![(SpanStyle::Plain, "a * b".into())]);
        assert_eq!(
            styles("snake_case_name"),
            vec![(SpanStyle::Plain, "snake_case_name".into())]
        );
        assert_eq!(
            styles("5 * 3 ** 2"),
            vec![(SpanStyle::Plain, "5 * 3 ** 2".into())]
        );
    }

    #[test]
    fn source_ranges_cover_the_line_exactly() {
        let line = "plain **bold** `code` tail";
        let spans = parse_inline_line(line);
        assert_eq!(spans.first().unwrap().source.start, 0);
        assert_eq!(spans.last().unwrap().source.end, line.len());
        for pair in spans.windows(2) {
            assert_eq!(pair[0].source.end, pair[1].source.start);
        }
        // Every display text equals its source slice (inline-only phase).
        for span in &spans {
            if span.style != SpanStyle::Link {
                assert_eq!(span.display, &line[span.source.clone()]);
            }
        }
    }
}
