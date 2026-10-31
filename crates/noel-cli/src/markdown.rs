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
use unicode_width::UnicodeWidthStr as _;

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
    BoldItalic,
}

/// One styled run over assistant markdown text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyledSpan {
    /// Byte range in the source entry text this span renders.
    pub source: Range<usize>,
    pub style: SpanStyle,
    /// Displayed text. For emphasis spans this is the *content* without
    /// markers (`**bold**` displays `bold`); `source` still covers the full
    /// marked range so selection copies the raw markdown.
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
        if rest.starts_with('`')
            && let Some(close) = find_code_close(line, index)
        {
            flush_plain(&mut spans, plain_start, index);
            spans.push(StyledSpan {
                source: index..close + 1,
                style: SpanStyle::Code,
                display: line[index + 1..close].to_string(),
            });
            index = close + 1;
            plain_start = index;
            continue;
        }
        if rest.starts_with("***")
            && let Some(content_end) = find_triple_marker(line, index + 3, b'*')
        {
            flush_plain(&mut spans, plain_start, index);
            spans.push(StyledSpan {
                source: index..content_end + 3,
                style: SpanStyle::BoldItalic,
                display: line[index + 3..content_end].to_string(),
            });
            index = content_end + 3;
            plain_start = index;
            continue;
        }
        if rest.starts_with("**")
            && let Some(content_end) = find_double_marker(line, index + 2, b'*')
        {
            flush_plain(&mut spans, plain_start, index);
            spans.push(StyledSpan {
                source: index..content_end + 2,
                style: SpanStyle::Bold,
                display: line[index + 2..content_end].to_string(),
            });
            index = content_end + 2;
            plain_start = index;
            continue;
        }
        if rest.starts_with("~~")
            && let Some(content_end) = find_double_marker(line, index + 2, b'~')
        {
            flush_plain(&mut spans, plain_start, index);
            spans.push(StyledSpan {
                source: index..content_end + 2,
                style: SpanStyle::Strike,
                display: line[index + 2..content_end].to_string(),
            });
            index = content_end + 2;
            plain_start = index;
            continue;
        }
        if rest.starts_with('[')
            && let Some((link_end, text_end)) = find_link(line, index)
        {
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
        let single = rest
            .chars()
            .next()
            .map(|ch| (ch == '*' || ch == '_') && !rest.starts_with("**"))
            .unwrap_or(false);
        if single && let Some(content_end) = find_single_emphasis_close(line, index) {
            flush_plain(&mut spans, plain_start, index);
            spans.push(StyledSpan {
                source: index..content_end + 1,
                style: SpanStyle::Italic,
                display: line[index + 1..content_end].to_string(),
            });
            index = content_end + 1;
            plain_start = index;
            continue;
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

/// Closing `***` for a triple-delimiter run opened at `open`.
fn find_triple_marker(line: &str, search_start: usize, marker: u8) -> Option<usize> {
    let bytes = line.as_bytes();
    let content_start = search_start;
    if content_start >= bytes.len() || bytes[content_start].is_ascii_whitespace() {
        return None;
    }
    let mut index = content_start;
    while index + 2 < bytes.len() {
        if bytes[index] == marker && bytes[index + 1] == marker && bytes[index + 2] == marker {
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

/// A parsed GFM pipe table: its rows of cells and per-column alignment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    /// Header cells, already unescaped (`\|` → `|`).
    pub header: Vec<String>,
    /// Data rows, one per source line after the delimiter.
    pub rows: Vec<Vec<String>>,
    /// Per-column alignment from the delimiter row (`:---` left, `:--:`
    /// center, `---:` right, `---` left).
    pub align: Vec<Alignment>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alignment {
    Left,
    Center,
    Right,
}

impl Table {
    /// Column display widths (unicode width), header included, capped at
    /// [`TABLE_MAX_COLUMN_WIDTH`]; longer cells truncate with an ellipsis
    /// when rendered.
    pub fn column_widths(&self) -> Vec<usize> {
        let columns = self.header.len();
        let mut widths = vec![0usize; columns];
        for row in std::iter::once(&self.header).chain(self.rows.iter()) {
            for (index, cell) in row.iter().enumerate().take(columns) {
                widths[index] = widths[index].max(truncate_cell(cell).width());
            }
        }
        widths
    }

    /// The rendered lines of the table, one per source line, in source
    /// order: the header row, the separator (from the delimiter line), one
    /// row per data line. Cell text is padded to the column width; inline
    /// emphasis inside cells is applied by the caller over each cell
    /// segment's text.
    pub fn render(&self) -> Vec<Vec<TableSegment>> {
        let widths = self.column_widths();
        let mut lines = vec![self.data_row(&self.header, &widths, true)];
        lines.push(self.border(&widths));
        for row in &self.rows {
            lines.push(self.data_row(row, &widths, false));
        }
        lines
    }

    fn border(&self, widths: &[usize]) -> Vec<TableSegment> {
        let inner = widths
            .iter()
            .map(|width| "─".repeat(width + 2))
            .collect::<Vec<_>>()
            .join("┼");
        vec![TableSegment::border(format!("  ├{inner}┤"))]
    }

    fn data_row(&self, cells: &[String], widths: &[usize], header: bool) -> Vec<TableSegment> {
        let mut segments = vec![TableSegment::border("  │".to_string())];
        for (index, width) in widths.iter().enumerate() {
            let cell = cells.get(index).map(String::as_str).unwrap_or("");
            let cell = truncate_cell(cell);
            let padding = width.saturating_sub(cell.width());
            let (left, right) = match self.align.get(index).copied().unwrap_or(Alignment::Left) {
                Alignment::Left => (1, padding + 1),
                Alignment::Right => (padding + 1, 1),
                Alignment::Center => (padding / 2 + 1, padding - padding / 2 + 1),
            };
            segments.push(TableSegment::cell(
                format!("{}{}{}", " ".repeat(left), cell, " ".repeat(right)),
                header,
            ));
            segments.push(TableSegment::border("│".to_string()));
        }
        segments
    }
}
/// One styled run of a rendered table line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableSegment {
    pub text: String,
    /// The header row is bold.
    pub header: bool,
    /// A border/padding run (`│ ├ ┤ ─` and the spaces around cells), drawn
    /// dim by the caller; a cell's own padding is part of its segment.
    pub border: bool,
}

impl TableSegment {
    fn border(text: String) -> Self {
        Self {
            text,
            header: false,
            border: true,
        }
    }
    fn cell(text: String, header: bool) -> Self {
        Self {
            text,
            header,
            border: false,
        }
    }
}

/// Widest cell a column shows before it is capped (a longer cell renders
/// truncated with an ellipsis; wrapping inside a grid cell needs row
/// geometry the transcript's line wrapper does not have).
pub const TABLE_MAX_COLUMN_WIDTH: usize = 28;

/// A cell longer than [`TABLE_MAX_COLUMN_WIDTH`] cuts at a grapheme
/// boundary before that width, with an ellipsis. (The transcript wrapper
/// breaks lines at the terminal edge; a grid cell must keep its row to one
/// line.)
fn truncate_cell(cell: &str) -> String {
    use unicode_width::UnicodeWidthChar;
    if cell.width() <= TABLE_MAX_COLUMN_WIDTH {
        return cell.to_string();
    }
    let mut width = 0usize;
    let mut end = cell.len();
    for (index, ch) in cell.char_indices() {
        let used = ch.width().unwrap_or(0);
        // Reserve one column for the ellipsis.
        if width + used > TABLE_MAX_COLUMN_WIDTH - 1 {
            end = index;
            break;
        }
        width += used;
    }
    format!("{}…", &cell[..end])
}

/// Parses a GFM pipe table starting at `lines[start]`, if it is one: a
/// header row of `|`-separated cells, a delimiter row of `---` cells, and
/// at least those two lines. Returns the table and the index of the first
/// line after it.
pub fn parse_table(lines: &[impl AsRef<str>], start: usize) -> Option<(Table, usize)> {
    let header_line = lines.get(start)?.as_ref();
    let delimiter_line = lines.get(start + 1)?.as_ref();
    let header = split_row(header_line)?;
    if header.is_empty() {
        return None;
    }
    let align = parse_delimiter(delimiter_line, header.len())?;
    let mut rows = Vec::new();
    let mut end = start + 2;
    while let Some(line) = lines.get(end) {
        let line = line.as_ref();
        if line.trim().is_empty() {
            break;
        }
        match split_row(line) {
            Some(cells) => {
                rows.push(cells);
                end += 1;
            }
            None => break,
        }
    }
    if rows.is_empty() {
        // A delimiter with no data rows is not a table.
        return None;
    }
    Some((
        Table {
            header,
            rows,
            align,
        },
        end,
    ))
}

/// Splits one `| a | b |` row into cells. A leading or trailing pipe is
/// optional; `\|` is a literal pipe. `None` when the line has no pipes
/// (not a table row).
fn split_row(line: &str) -> Option<Vec<String>> {
    let trimmed = line.trim();
    if !trimmed.contains('|') {
        return None;
    }
    let inner = trimmed
        .strip_prefix('|')
        .map(str::to_string)
        .unwrap_or_else(|| trimmed.to_string());
    let inner = inner.strip_suffix('|').map(str::to_string).unwrap_or(inner);
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' && chars.peek() == Some(&'|') {
            cell.push('|');
            chars.next();
        } else if ch == '|' {
            cells.push(cell.trim().to_string());
            cell = String::new();
        } else {
            cell.push(ch);
        }
    }
    cells.push(cell.trim().to_string());
    Some(cells)
}

/// The delimiter row `| --- | :---: | ---: |`: each cell only dashes and
/// at most one leading and trailing colon. `None` when any cell breaks the
/// rule or the count differs from the header.
fn parse_delimiter(line: &str, columns: usize) -> Option<Vec<Alignment>> {
    let cells = split_row(line)?;
    if cells.len() != columns {
        return None;
    }
    cells
        .into_iter()
        .map(|cell| {
            let (left, right) = (cell.starts_with(':'), cell.ends_with(':') && cell.len() > 1);
            // Only dashes and at most the two colons: anything else is not
            // a delimiter cell.
            if !cell.replace(['-', ':'], "").is_empty() {
                return None;
            }
            Some(match (left, right) {
                (true, true) => Alignment::Center,
                (false, true) => Alignment::Right,
                _ => Alignment::Left,
            })
        })
        .collect()
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
        // Some terminals (kitty among them) don't visually combine SGR 1+3
        // distinctly on all fonts; tinting with the accent keeps bold-italic
        // recognizable even when the font renders both weights identically.
        SpanStyle::BoldItalic => Style::default()
            .fg(accent)
            .add_modifier(Modifier::BOLD)
            .add_modifier(Modifier::ITALIC),
    }
}

/// A block-level construct detected on a single markdown source line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockKind {
    /// `#{1,6} text` heading. Level 1–6; display drops the hashes.
    Heading(u8),
    /// ``` or ~~~ fence open/close. Content lines are opaque (no inline
    /// emphasis) until the matching fence.
    Fence {
        language: String,
        closing: bool,
    },
    /// `- ` or `* ` bullet, or `1. ` ordered. `marker` carries the source's
    /// ordered-list number (`1. ` → `Some(1)`) so the display can number the
    /// list; `None` for bullets.
    ListItem {
        ordered: bool,
        marker: Option<u64>,
    },
    /// `> text` blockquote.
    Blockquote,
    Plain,
}

/// The fence marker a line opens or closes a code block with: ``` or
/// ~~~ (#158: both, as upstream's renderer reads them).
pub fn fence_marker(line: &str) -> Option<&'static str> {
    let trimmed = line.trim_start();
    ["```", "~~~"]
        .into_iter()
        .find(|marker| trimmed.starts_with(marker))
}

/// Classify one source line for block rendering.
pub fn classify_line(line: &str) -> BlockKind {
    let trimmed = line.trim_start();
    if let Some(marker) = fence_marker(trimmed) {
        return BlockKind::Fence {
            language: trimmed[marker.len()..].trim().to_string(),
            closing: false,
        };
    }
    // Heading: 1-6 '#' followed by a space.
    if let Some(after_hashes) = trimmed.strip_prefix('#') {
        let hashes = after_hashes.len() - after_hashes.trim_start_matches('#').len();
        let total = hashes + 1;
        let rest = &after_hashes[hashes..];
        if (1..=6).contains(&total) && (rest.starts_with(' ') || rest.is_empty()) {
            return BlockKind::Heading(total as u8);
        }
    }
    if trimmed == ">" || trimmed.starts_with("> ") {
        return BlockKind::Blockquote;
    }
    if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
        return BlockKind::ListItem {
            ordered: false,
            marker: None,
        };
    }
    // Ordered list: digits followed by `. ` — but not decimals in prose
    // ("3.14") because those have no space after the dot.
    let digits: String = trimmed
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect();
    if !digits.is_empty() && trimmed[digits.len()..].starts_with(". ") {
        let number = digits.parse::<u64>().ok().filter(|n| *n > 0);
        return BlockKind::ListItem {
            ordered: true,
            marker: number,
        };
    }
    BlockKind::Plain
}

/// How a source line should render at block level: display prefix and base
/// style hint, plus whether inline emphasis applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockRender {
    /// Prefix replacing the source's leading markers (e.g. "  • ").
    pub prefix: String,
    /// Bytes of source text after the marker to render as content.
    pub content_byte: usize,
    /// Bold for headings, dim for quotes, plain otherwise.
    pub emphasis_blocks: bool,
    /// Inline emphasis (bold/code/links) applies to the content.
    pub inline: bool,
}

/// Render instructions for a classified line inside an assistant entry.
pub fn block_render(kind: &BlockKind, line: &str) -> BlockRender {
    match kind {
        BlockKind::Heading(level) => BlockRender {
            prefix: "".to_string(),
            content_byte: line.len() - line.trim_start().len() + (1 + *level as usize),
            emphasis_blocks: true,
            inline: true,
        },
        BlockKind::Fence { closing, .. } => BlockRender {
            prefix: "  ┆ ".to_string(),
            content_byte: if *closing { line.len() } else { 0 },
            emphasis_blocks: false,
            inline: false,
        },
        BlockKind::ListItem { ordered, marker } => BlockRender {
            // Ordered items display the number the caller supplies (the
            // list's sequence, like upstream's list renderer, which numbers
            // items from the first marker and ignores later source numbers).
            prefix: if *ordered {
                format!("  {}. ", marker.unwrap_or(1))
            } else {
                "  • ".to_string()
            },
            // Skip the marker: two characters for a bullet, or the digits,
            // dot, and space for an ordered item.
            content_byte: line.len() - line.trim_start().len()
                + if *ordered {
                    line.trim_start()
                        .chars()
                        .take_while(|ch| ch.is_ascii_digit())
                        .count()
                        + 2
                } else {
                    2
                },
            emphasis_blocks: false,
            inline: true,
        },
        BlockKind::Blockquote => BlockRender {
            prefix: "  │ ".to_string(),
            content_byte: line.find('>').map(|i| i + 1).unwrap_or(0),
            emphasis_blocks: false,
            inline: true,
        },
        BlockKind::Plain => BlockRender {
            prefix: "  ".to_string(),
            content_byte: 0,
            inline: true,
            emphasis_blocks: false,
        },
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
                (SpanStyle::Bold, "bold".into()),
                (SpanStyle::Plain, " rest".into()),
            ]
        );
        assert_eq!(
            styles("***both***"),
            vec![(SpanStyle::BoldItalic, "both".into())]
        );
        assert_eq!(
            styles("a *it* b"),
            vec![
                (SpanStyle::Plain, "a ".into()),
                (SpanStyle::Italic, "it".into()),
                (SpanStyle::Plain, " b".into()),
            ]
        );
        assert_eq!(
            styles("`cargo build`"),
            vec![(SpanStyle::Code, "cargo build".into())]
        );
        assert_eq!(styles("~~gone~~"), vec![(SpanStyle::Strike, "gone".into())]);
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
        // Emphasis spans display content only; their source range still
        // covers the raw markers so selection copies the original markdown.
        for span in &spans {
            if matches!(
                span.style,
                SpanStyle::Bold
                    | SpanStyle::Italic
                    | SpanStyle::Code
                    | SpanStyle::Strike
                    | SpanStyle::BoldItalic
            ) {
                assert!(span.display.len() < span.source.len());
            }
        }
    }
}

#[cfg(test)]
mod block_tests {
    use super::*;

    #[test]
    fn classifies_common_blocks() {
        assert_eq!(classify_line("## Title"), BlockKind::Heading(2));
        assert_eq!(classify_line("###### Deep"), BlockKind::Heading(6));
        assert_eq!(classify_line("#NotHeading"), BlockKind::Plain);
        assert_eq!(
            classify_line("```rust"),
            BlockKind::Fence {
                language: "rust".into(),
                closing: false
            }
        );
        assert_eq!(
            classify_line("```"),
            BlockKind::Fence {
                language: String::new(),
                closing: false
            }
        );
        // #158: tilde fences too; each closes only on its own marker.
        assert_eq!(
            classify_line("~~~ python"),
            BlockKind::Fence {
                language: "python".into(),
                closing: false
            }
        );
        assert_eq!(fence_marker("  ~~~"), Some("~~~"));
        assert_eq!(fence_marker("```js"), Some("```"));
        assert_eq!(fence_marker("~~ not"), None);
        assert_eq!(
            classify_line("- item"),
            BlockKind::ListItem {
                ordered: false,
                marker: None
            }
        );
        assert_eq!(
            classify_line("1. step"),
            BlockKind::ListItem {
                ordered: true,
                marker: Some(1)
            }
        );
        assert_eq!(classify_line("> quoted"), BlockKind::Blockquote);
        assert_eq!(classify_line("plain text"), BlockKind::Plain);
    }

    #[test]
    fn ordered_lists_carry_their_source_number() {
        assert_eq!(
            classify_line("1. single"),
            BlockKind::ListItem {
                ordered: true,
                marker: Some(1)
            }
        );
        assert_eq!(
            classify_line("10. ten"),
            BlockKind::ListItem {
                ordered: true,
                marker: Some(10)
            }
        );
        // A zero or overflowing number still renders as the first item.
        assert_eq!(
            classify_line("0. zero"),
            BlockKind::ListItem {
                ordered: true,
                marker: None
            }
        );
        assert_eq!(classify_line("3.14 pi"), BlockKind::Plain);
    }

    #[test]
    fn block_render_prefixes_and_content() {
        let heading = block_render(&BlockKind::Heading(2), "## Title");
        assert_eq!(heading.content_byte, 3);
        let item = block_render(
            &BlockKind::ListItem {
                ordered: false,
                marker: None,
            },
            "- thing",
        );
        assert_eq!(item.prefix, "  • ");
        assert_eq!(item.content_byte, 2);
        let ordered = block_render(
            &BlockKind::ListItem {
                ordered: true,
                marker: Some(3),
            },
            "10. ten",
        );
        assert_eq!(ordered.prefix, "  3. ");
        assert_eq!(ordered.content_byte, 4);
        let quote = block_render(&BlockKind::Blockquote, "> wisdom");
        assert_eq!(quote.prefix, "  │ ");
        assert_eq!(quote.content_byte, 1);
    }
}

#[cfg(test)]
mod table_tests {
    use super::*;

    fn plain(line: &[TableSegment]) -> String {
        line.iter().map(|segment| segment.text.as_str()).collect()
    }

    #[test]
    fn parses_and_renders_a_table() {
        let lines = [
            "| Feature | noel | Claude |",
            "|:--------|:----:|------:|",
            "| speed   | fast | ok    |",
            "| price   | free | paid  |",
        ];
        let (table, end) = parse_table(&lines, 0).expect("table");
        assert_eq!(end, 4);
        assert_eq!(table.header, ["Feature", "noel", "Claude"]);
        assert_eq!(
            table.align,
            [Alignment::Left, Alignment::Center, Alignment::Right]
        );
        assert_eq!(table.rows.len(), 2);
        let rendered = table.render();
        assert_eq!(rendered.len(), 4);
        assert_eq!(plain(&rendered[0]), "  │ Feature │ noel │ Claude │");
        assert_eq!(plain(&rendered[1]), "  ├─────────┼──────┼────────┤");
        assert_eq!(plain(&rendered[2]), "  │ speed   │ fast │     ok │");
    }

    #[test]
    fn escaped_pipes_and_missing_cells() {
        let lines = ["| a \\| b | c |", "| --- | --- |", "| only-one |"];
        let (table, _) = parse_table(&lines, 0).expect("table");
        assert_eq!(table.header, ["a | b", "c"]);
        assert_eq!(table.rows, [["only-one"]]);
        // The short row pads; the missing cell is empty.
        let rendered = table.render();
        assert!(plain(&rendered[2]).contains("only-one"));
        assert!(plain(&rendered[2]).ends_with("│   │"));
    }

    #[test]
    fn a_delimiter_without_data_rows_is_not_a_table() {
        let lines = ["| a | b |", "| --- | --- |", "", "prose after"];
        assert!(parse_table(&lines, 0).is_none());
    }

    #[test]
    fn a_bad_delimiter_is_not_a_table() {
        let lines = ["| a | b |", "| x | --- |", "| 1 | 2 |"];
        assert!(parse_table(&lines, 0).is_none());
    }

    #[test]
    fn wide_cells_truncate_with_an_ellipsis() {
        let long = "x".repeat(TABLE_MAX_COLUMN_WIDTH + 10);
        let lines = ["| col |", "| --- |", &format!("| {long} |")];
        let (table, _) = parse_table(&lines, 0).expect("table");
        let rendered = table.render();
        let row = plain(&rendered[2]);
        assert!(row.contains('…'));
        assert!(row.chars().count() < long.chars().count() + 8);
    }

    #[test]
    fn header_segments_carry_their_flag() {
        let lines = ["| h |", "| --- |", "| d |"];
        let (table, _) = parse_table(&lines, 0).expect("table");
        let rendered = table.render();
        assert!(rendered[0].iter().any(|segment| segment.header));
        assert!(rendered[2].iter().all(|segment| !segment.header));
    }
}
