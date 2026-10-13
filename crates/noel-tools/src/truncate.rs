//! Output limits shared by the tools (upstream `truncate.ts`): a line limit
//! and a byte limit, whichever is hit first. Sizes count UTF-8 bytes.

pub const DEFAULT_MAX_LINES: usize = 2000;
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;
/// Characters (UTF-16 units, as upstream counts them) kept of a grep line.
pub const GREP_MAX_LINE_LENGTH: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Truncation {
    pub content: String,
    pub truncated: bool,
    /// Whether the line limit was the one hit; otherwise the byte limit.
    pub by_lines: bool,
    pub total_lines: usize,
    pub total_bytes: usize,
    pub output_lines: usize,
    pub output_bytes: usize,
    /// The kept text is the end of one line longer than the byte limit.
    pub last_line_partial: bool,
    /// The first line alone exceeds the byte limit, so nothing was kept.
    pub first_line_exceeds_limit: bool,
}

/// Lines as upstream counts them: a final newline does not start a line.
fn lines_for_counting(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines = content.split('\n').collect::<Vec<_>>();
    if content.ends_with('\n') {
        lines.pop();
    }
    lines
}

fn untruncated(content: &str, total_lines: usize) -> Truncation {
    Truncation {
        content: content.to_string(),
        truncated: false,
        by_lines: false,
        total_lines,
        total_bytes: content.len(),
        output_lines: total_lines,
        output_bytes: content.len(),
        last_line_partial: false,
        first_line_exceeds_limit: false,
    }
}

/// Keeps the first complete lines that fit both limits.
pub fn truncate_head(content: &str, max_lines: usize, max_bytes: usize) -> Truncation {
    let lines = lines_for_counting(content);
    if lines.len() <= max_lines && content.len() <= max_bytes {
        return untruncated(content, lines.len());
    }
    let mut truncation = Truncation {
        content: String::new(),
        truncated: true,
        by_lines: false,
        total_lines: lines.len(),
        total_bytes: content.len(),
        output_lines: 0,
        output_bytes: 0,
        last_line_partial: false,
        first_line_exceeds_limit: lines[0].len() > max_bytes,
    };
    if truncation.first_line_exceeds_limit {
        return truncation;
    }
    let mut kept = 0;
    let mut bytes = 0;
    truncation.by_lines = true;
    for (index, line) in lines.iter().take(max_lines).enumerate() {
        let line_bytes = line.len() + usize::from(index > 0);
        if bytes + line_bytes > max_bytes {
            truncation.by_lines = false;
            break;
        }
        bytes += line_bytes;
        kept += 1;
    }
    truncation.content = lines[..kept].join("\n");
    truncation.output_lines = kept;
    truncation.output_bytes = truncation.content.len();
    truncation
}

/// Keeps the last complete lines that fit both limits. When the last line
/// alone exceeds the byte limit, keeps the end of that line.
pub fn truncate_tail(content: &str, max_lines: usize, max_bytes: usize) -> Truncation {
    let lines = lines_for_counting(content);
    if lines.len() <= max_lines && content.len() <= max_bytes {
        return untruncated(content, lines.len());
    }
    let mut kept = Vec::new();
    let mut bytes = 0;
    let mut by_lines = true;
    let mut last_line_partial = false;
    for line in lines.iter().rev() {
        if kept.len() >= max_lines {
            break;
        }
        let line_bytes = line.len() + usize::from(!kept.is_empty());
        if bytes + line_bytes > max_bytes {
            by_lines = false;
            if kept.is_empty() {
                kept.push(tail_within_bytes(line, max_bytes));
                last_line_partial = true;
            }
            break;
        }
        kept.push(line);
        bytes += line_bytes;
    }
    kept.reverse();
    let kept_text = kept.join("\n");
    Truncation {
        truncated: true,
        by_lines,
        total_lines: lines.len(),
        total_bytes: content.len(),
        output_lines: kept.len(),
        output_bytes: kept_text.len(),
        last_line_partial,
        first_line_exceeds_limit: false,
        content: kept_text,
    }
}

/// The longest suffix of `text` of at most `max_bytes` bytes.
fn tail_within_bytes(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut start = text.len() - max_bytes;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// A size as upstream prints it: `512B`, `58.6KB`, `1.2MB`.
pub fn format_size(bytes: usize) -> String {
    // One decimal, halves rounded up, like JavaScript's `toFixed(1)`.
    let tenths = |unit: usize| {
        let tenths = (bytes * 20 + unit) / (unit * 2);
        format!("{}.{}", tenths / 10, tenths % 10)
    };
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{}KB", tenths(1024))
    } else {
        format!("{}MB", tenths(1024 * 1024))
    }
}

/// Cuts a grep line to [`GREP_MAX_LINE_LENGTH`] characters. Returns the text
/// and whether it was cut.
pub fn truncate_line(line: &str) -> (String, bool) {
    let mut units = 0;
    for (index, character) in line.char_indices() {
        units += character.len_utf16();
        if units > GREP_MAX_LINE_LENGTH {
            return (format!("{}... [truncated]", &line[..index]), true);
        }
    }
    (line.to_string(), false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_keeps_whole_lines_within_both_limits() {
        let text = "one\ntwo\nthree\n";
        let all = truncate_head(text, 10, 100);
        assert!(!all.truncated);
        assert_eq!((all.content.as_str(), all.total_lines), (text, 3));

        let by_lines = truncate_head(text, 2, 100);
        assert!(by_lines.truncated && by_lines.by_lines);
        assert_eq!(
            (by_lines.content.as_str(), by_lines.output_lines),
            ("one\ntwo", 2)
        );

        // "one\ntwo" is 7 bytes; "three" does not fit in 8.
        let by_bytes = truncate_head(text, 10, 8);
        assert!(by_bytes.truncated && !by_bytes.by_lines);
        assert_eq!(by_bytes.content, "one\ntwo");

        let first_too_long = truncate_head("abcdef\nx", 10, 3);
        assert!(first_too_long.first_line_exceeds_limit);
        assert_eq!(first_too_long.content, "");
    }

    #[test]
    fn tail_keeps_the_last_lines_or_the_end_of_one_long_line() {
        let text = "one\ntwo\nthree\n";
        let by_lines = truncate_tail(text, 2, 100);
        assert!(by_lines.truncated && by_lines.by_lines);
        assert_eq!(by_lines.content, "two\nthree");

        let by_bytes = truncate_tail(text, 10, 9);
        assert!(!by_bytes.by_lines && !by_bytes.last_line_partial);
        assert_eq!(by_bytes.content, "two\nthree");

        let partial = truncate_tail("abc\nd\u{e9}fgh", 10, 4);
        assert!(partial.last_line_partial);
        // The cut never splits a character.
        assert_eq!(partial.content, "fgh");
    }

    #[test]
    fn sizes_round_halves_up_like_upstream() {
        assert_eq!(format_size(512), "512B");
        assert_eq!(format_size(50 * 1024), "50.0KB");
        assert_eq!(format_size(60_000), "58.6KB");
        assert_eq!(format_size(1280), "1.3KB");
        assert_eq!(format_size(3 * 1024 * 1024 / 2), "1.5MB");
    }

    #[test]
    fn long_grep_lines_are_cut_at_500_utf16_units() {
        assert_eq!(truncate_line("short"), ("short".to_string(), false));
        let (cut, was_cut) = truncate_line(&"x".repeat(501));
        assert!(was_cut);
        assert_eq!(cut, format!("{}... [truncated]", "x".repeat(500)));
        // An astral character counts as two units and is never split.
        let (cut, _) = truncate_line(&format!("{}\u{1f600}tail", "x".repeat(499)));
        assert_eq!(cut, format!("{}... [truncated]", "x".repeat(499)));
    }
}
