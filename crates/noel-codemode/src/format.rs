//! The codemode tool's result as the model reads it (upstream coding agent
//! `extensions/codemode/execute.ts`, `executeCodemode` from the script's
//! result on): the header, the script's output laid out item by item, the
//! error, the token budget with a spill file, and saved images.
//!
//! Lengths and cuts count UTF-16 code units, as JavaScript's do.

use std::path::PathBuf;

use serde_json::Value;

use crate::execution::{CodemodeResult, ErrorKind};
use crate::vm::OutputItem;

/// Default token budget for script output.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 10_000;
/// Characters per token when estimating.
const CHARS_PER_TOKEN: u64 = 4;

/// One block of the tool result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Text(String),
    /// Base64 data and its type.
    Image {
        data: String,
        mime_type: String,
    },
}

/// Where the full output and images are written (upstream `writeOutputFile`).
pub trait OutputFiles {
    /// Saves the full text of truncated output.
    fn write_text(&self, text: &str) -> Result<PathBuf, String>;
    /// Saves one image's bytes with `extension` (`.png`, ...).
    fn write_image(&self, bytes: &[u8], extension: &str) -> Result<PathBuf, String>;
}

/// A nested call as the failure summary lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallSummary {
    pub name: String,
    /// `ok`, `error` or `cancelled`.
    pub status: String,
}

/// The formatted result: the blocks, and the spill file when the output was
/// truncated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Formatted {
    pub blocks: Vec<Block>,
    pub full_output_path: Option<PathBuf>,
    pub is_error: bool,
}

/// Like the script's `text()`: strings as is, other values as compact JSON.
fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Lays the output out so the model can tell items apart: with more than
/// one text item each starts with `==> text N/M <==`; `console.*` lines
/// follow everything else in one `<console_output>` block.
fn format_output(output: &[OutputItem]) -> Vec<Block> {
    let total = output
        .iter()
        .filter(|item| matches!(item, OutputItem::Text { console: false, .. }))
        .count();
    let mut items = Vec::new();
    let mut console_lines = Vec::new();
    let mut index = 0;
    for item in output {
        match item {
            OutputItem::Image { data, mime_type } => items.push(Block::Image {
                data: data.clone(),
                mime_type: mime_type.clone(),
            }),
            OutputItem::Text {
                text,
                console: true,
            } => console_lines.push(text.as_str()),
            OutputItem::Text {
                text,
                console: false,
            } => {
                index += 1;
                items.push(Block::Text(if total > 1 {
                    format!("==> text {index}/{total} <==\n{text}")
                } else {
                    text.clone()
                }));
            }
        }
    }
    if !console_lines.is_empty() {
        items.push(Block::Text(format!(
            "<console_output>\n{}\n</console_output>",
            console_lines.join("\n")
        )));
    }
    items
}

/// Joins adjacent text blocks into one, each part starting on its own line.
fn join_adjacent_text(items: Vec<Block>) -> Vec<Block> {
    let mut joined = Vec::new();
    let mut parts = String::new();
    let mut open = false;
    let mut last_part = String::new();
    for item in items {
        match item {
            Block::Text(text) => {
                if !last_part.is_empty() && !last_part.ends_with('\n') {
                    parts.push('\n');
                    last_part = "\n".to_owned();
                }
                parts.push_str(&text);
                if !text.is_empty() {
                    last_part = text;
                }
                open = true;
            }
            image => {
                if open {
                    joined.push(Block::Text(std::mem::take(&mut parts)));
                    open = false;
                    last_part.clear();
                }
                joined.push(image);
            }
        }
    }
    if open {
        joined.push(Block::Text(parts));
    }
    joined
}

fn format_call_summary(calls: &[CallSummary]) -> String {
    if calls.is_empty() {
        return "No tool calls were made.".to_owned();
    }
    let list: Vec<String> = calls
        .iter()
        .map(|call| format!("{} ({})", call.name, call.status))
        .collect();
    format!(
        "Tool calls made before the failure (they are not undone): {}",
        list.join(", ")
    )
}

/// Applies the token budget: over it, the text blocks become one block that
/// keeps the start and end, images follow, and the full text is saved.
fn truncate_output(
    items: Vec<Block>,
    max_tokens: u64,
    files: &dyn OutputFiles,
) -> (Vec<Block>, Option<PathBuf>) {
    let texts: Vec<&str> = items
        .iter()
        .filter_map(|item| match item {
            Block::Text(text) => Some(text.as_str()),
            Block::Image { .. } => None,
        })
        .collect();
    let combined = texts.join("\n");
    let budget = max_tokens.saturating_mul(CHARS_PER_TOKEN);
    let units: Vec<u16> = combined.encode_utf16().collect();
    let length = units.len() as u64;
    if texts.is_empty() || length <= budget {
        return (items, None);
    }
    let head_chars = budget / 2;
    let tail_chars = budget - head_chars;
    let removed = length - head_chars - tail_chars;
    let head = String::from_utf16_lossy(&units[..head_chars as usize]);
    let tail = String::from_utf16_lossy(&units[(length - tail_chars) as usize..]);
    let lines = combined.split('\n').count();
    let mut text = format!(
        "Warning: truncated output (original token count: {})\nTotal output lines: {lines}\n\n{head}…{} tokens truncated…{tail}",
        length.div_ceil(CHARS_PER_TOKEN),
        removed.div_ceil(CHARS_PER_TOKEN),
    );
    let spilled = files.write_text(&combined);
    match &spilled {
        Ok(path) => text.push_str(&format!(
            "\n\n[Full output: {} (read with offset/limit)]",
            path.display()
        )),
        Err(error) => text.push_str(&format!("\n\n[Could not save the full output: {error}]")),
    }
    let mut blocks = vec![Block::Text(text)];
    blocks.extend(
        items
            .into_iter()
            .filter(|item| matches!(item, Block::Image { .. })),
    );
    (blocks, spilled.ok())
}

/// File extensions of the image types `image()` accepts.
fn image_extension(mime_type: &str) -> Option<&'static str> {
    match mime_type {
        "image/png" => Some(".png"),
        "image/jpeg" => Some(".jpg"),
        "image/gif" => Some(".gif"),
        "image/webp" => Some(".webp"),
        _ => None,
    }
}

/// `value.toFixed(1)`: halves round up, as JavaScript rounds them.
pub fn to_fixed_1(value: f64) -> String {
    format!("{:.1}", (value * 10.0).round() / 10.0)
}

/// Upstream's `formatSize`.
pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{}KB", to_fixed_1(bytes as f64 / 1024.0))
    } else {
        format!("{}MB", to_fixed_1(bytes as f64 / (1024.0 * 1024.0)))
    }
}

/// Saves each image and puts a text block with its path before it; an image
/// shown more than once is saved once.
fn save_images(items: Vec<Block>, files: &dyn OutputFiles) -> Vec<Block> {
    use base64::Engine as _;
    let mut labels: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut out = Vec::new();
    for item in items {
        let Block::Image { data, mime_type } = &item else {
            out.push(item);
            continue;
        };
        let label = labels
            .entry(data.clone())
            .or_insert_with(|| {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .unwrap_or_default();
                let kind = format!("{mime_type}, {}", format_size(bytes.len()));
                let saved = match image_extension(mime_type) {
                    Some(extension) => files.write_image(&bytes, extension),
                    None => Err(format!("No file extension for image type {mime_type}")),
                };
                match saved {
                    Ok(path) => format!("[Image saved to {} ({kind})]", path.display()),
                    Err(error) => format!("[Image ({kind}) could not be saved: {error}]"),
                }
            })
            .clone();
        out.push(Block::Text(label));
        out.push(item);
    }
    out
}

/// The tool result for `result`: `calls` are the nested calls for the
/// failure summary, `wall_seconds` the time since the tool started.
pub fn format_result(
    result: &CodemodeResult,
    calls: &[CallSummary],
    max_output_tokens: Option<u64>,
    wall_seconds: f64,
    files: &dyn OutputFiles,
) -> Formatted {
    let mut output = result.output.clone();
    if let Ok(completed) = &result.outcome {
        // pi's extension: a returned value is appended like `text()`.
        if let Some(value) = &completed.value {
            // Compact JSON in the script's key order, when it is known.
            let text = match (value, &completed.value_json) {
                (Value::String(_), _) | (_, None) => value_text(value),
                (_, Some(json)) => json.clone(),
            };
            output.push(OutputItem::Text {
                text,
                console: false,
            });
        }
    }
    let mut items = format_output(&output);
    if let Err(error) = &result.outcome {
        let head = match error.kind {
            ErrorKind::Script => error.stack.clone().unwrap_or_else(|| {
                format!(
                    "{}: {}",
                    error.name.as_deref().unwrap_or("Error"),
                    error.message
                )
            }),
            ErrorKind::Timeout => format!("Script timed out: {}", error.message),
            ErrorKind::Aborted => format!("Script aborted: {}", error.message),
            ErrorKind::Sandbox => format!("Script sandbox failed: {}", error.message),
        };
        items.push(Block::Text(format!(
            "Script error:\n{head}\n\n{}",
            format_call_summary(calls)
        )));
    }
    let (truncated, full_output_path) = truncate_output(
        join_adjacent_text(items),
        max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS),
        files,
    );
    let body = join_adjacent_text(save_images(truncated, files));
    let ok = result.outcome.is_ok();
    let header = format!(
        "{}\nWall time {} seconds\nOutput:\n",
        if ok {
            "Script completed"
        } else {
            "Script failed"
        },
        to_fixed_1(wall_seconds)
    );
    let mut blocks = vec![Block::Text(header)];
    blocks.extend(body);
    Formatted {
        blocks,
        full_output_path,
        is_error: !ok,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::{CodemodeError, Completed};
    use serde_json::json;
    use std::cell::RefCell;

    #[derive(Default)]
    struct Files {
        written: RefCell<Vec<String>>,
    }

    impl OutputFiles for Files {
        fn write_text(&self, text: &str) -> Result<PathBuf, String> {
            self.written.borrow_mut().push(text.to_owned());
            Ok(PathBuf::from("/tmp/pi-codemode-1.txt"))
        }
        fn write_image(&self, bytes: &[u8], extension: &str) -> Result<PathBuf, String> {
            self.written
                .borrow_mut()
                .push(format!("{} bytes", bytes.len()));
            Ok(PathBuf::from(format!("/tmp/pi-codemode-2{extension}")))
        }
    }

    fn text(text: &str) -> OutputItem {
        OutputItem::Text {
            text: text.to_owned(),
            console: false,
        }
    }

    fn console(text: &str) -> OutputItem {
        OutputItem::Text {
            text: text.to_owned(),
            console: true,
        }
    }

    fn ok(output: Vec<OutputItem>, value: Option<Value>) -> CodemodeResult {
        CodemodeResult {
            outcome: Ok(Completed {
                value,
                ..Completed::default()
            }),
            output,
            calls: Vec::new(),
        }
    }

    fn texts(formatted: &Formatted) -> Vec<String> {
        formatted
            .blocks
            .iter()
            .map(|block| match block {
                Block::Text(text) => text.clone(),
                Block::Image { mime_type, .. } => format!("<image {mime_type}>"),
            })
            .collect()
    }

    #[test]
    fn several_text_items_are_numbered_and_console_lines_go_last() {
        let result = ok(
            vec![text("one"), console("log a"), text("two")],
            Some(json!({"n": 1})),
        );
        let formatted = format_result(&result, &[], None, 0.04, &Files::default());
        assert_eq!(
            texts(&formatted),
            [
                "Script completed\nWall time 0.0 seconds\nOutput:\n",
                "==> text 1/3 <==\none\n==> text 2/3 <==\ntwo\n==> text 3/3 <==\n{\"n\":1}\n<console_output>\nlog a\n</console_output>",
            ]
        );
        assert!(!formatted.is_error);
    }

    #[test]
    fn a_single_value_is_not_numbered() {
        let formatted = format_result(
            &ok(vec![], Some(json!("plain"))),
            &[],
            None,
            1.25,
            &Files::default(),
        );
        assert_eq!(
            texts(&formatted),
            [
                "Script completed\nWall time 1.3 seconds\nOutput:\n",
                "plain"
            ]
        );
    }

    #[test]
    fn failures_name_their_kind_and_the_calls_made() {
        let failed = |kind, message: &str, stack: Option<&str>| CodemodeResult {
            outcome: Err(CodemodeError {
                kind,
                name: Some("TypeError".to_owned()),
                message: message.to_owned(),
                stack: stack.map(str::to_owned),
            }),
            output: vec![text("partial")],
            calls: Vec::new(),
        };
        let calls = [
            CallSummary {
                name: "read".to_owned(),
                status: "ok".to_owned(),
            },
            CallSummary {
                name: "bash".to_owned(),
                status: "error".to_owned(),
            },
        ];
        let script = format_result(
            &failed(ErrorKind::Script, "bad", None),
            &calls,
            None,
            0.0,
            &Files::default(),
        );
        assert_eq!(
            texts(&script)[1],
            "partial\nScript error:\nTypeError: bad\n\nTool calls made before the failure (they are not undone): read (ok), bash (error)"
        );
        assert!(script.is_error);
        assert!(texts(&script)[0].starts_with("Script failed\n"));
        let with_stack = format_result(
            &failed(
                ErrorKind::Script,
                "bad",
                Some("TypeError: bad\n    at codemode.js:2:3"),
            ),
            &[],
            None,
            0.0,
            &Files::default(),
        );
        assert!(texts(&with_stack)[1]
            .ends_with("TypeError: bad\n    at codemode.js:2:3\n\nNo tool calls were made."));
        for (kind, head) in [
            (ErrorKind::Timeout, "Script timed out: m"),
            (ErrorKind::Aborted, "Script aborted: m"),
            (ErrorKind::Sandbox, "Script sandbox failed: m"),
        ] {
            let formatted =
                format_result(&failed(kind, "m", None), &[], None, 0.0, &Files::default());
            assert!(
                texts(&formatted)[1].contains(&format!("Script error:\n{head}\n\n")),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn output_over_the_budget_keeps_its_ends_and_is_saved() {
        let long = "a".repeat(30) + &"b".repeat(30);
        let files = Files::default();
        let formatted = format_result(&ok(vec![text(&long)], None), &[], Some(10), 0.0, &files);
        // Budget 40: 20 from the head, 20 from the tail, 20 removed (5 tokens).
        assert_eq!(
            texts(&formatted)[1],
            format!(
                "Warning: truncated output (original token count: 15)\nTotal output lines: 1\n\n{}…5 tokens truncated…{}\n\n[Full output: /tmp/pi-codemode-1.txt (read with offset/limit)]",
                "a".repeat(20),
                "b".repeat(20)
            )
        );
        assert_eq!(
            formatted.full_output_path,
            Some(PathBuf::from("/tmp/pi-codemode-1.txt"))
        );
        assert_eq!(files.written.borrow()[0], long);
    }

    #[test]
    fn images_are_saved_once_and_labelled_where_they_appear() {
        let image = OutputItem::Image {
            data: "iVBORw0KGgo=".to_owned(),
            mime_type: "image/png".to_owned(),
        };
        let files = Files::default();
        let formatted = format_result(
            &ok(vec![text("look"), image.clone(), image], None),
            &[],
            None,
            0.0,
            &files,
        );
        assert_eq!(
            texts(&formatted),
            [
                "Script completed\nWall time 0.0 seconds\nOutput:\n",
                "look\n[Image saved to /tmp/pi-codemode-2.png (image/png, 8B)]",
                "<image image/png>",
                "[Image saved to /tmp/pi-codemode-2.png (image/png, 8B)]",
                "<image image/png>",
            ]
        );
        assert_eq!(files.written.borrow().len(), 1);
    }

    #[test]
    fn sizes_are_formatted_as_upstream_does() {
        assert_eq!(format_size(512), "512B");
        assert_eq!(format_size(1536), "1.5KB");
        assert_eq!(format_size(3 * 1024 * 1024), "3.0MB");
    }
}
