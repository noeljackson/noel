pub mod design;
pub mod icons;
pub mod sessions;

use crate::icons::{icon, Icon};
use gpui::{
    colors::Colors, div, prelude::*, px, AnyElement, ClipboardItem, FontStyle, FontWeight,
    HighlightStyle, SharedString, StrikethroughStyle, StyledText, UnderlineStyle,
};
use noel_markdown::{
    block_render, classify_line, display_inline, fence_marker, BlockKind, DisplayLine, SpanStyle,
};
use serde_json::Value;
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    ops::Range,
    sync::OnceLock,
};
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle as SyntectFontStyle, ThemeSet},
    parsing::SyntaxSet,
};

pub fn button(
    id: impl Into<SharedString>,
    label: &str,
    colors: &Colors,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id.into())
        .px_3()
        .py_1()
        .rounded_md()
        .bg(colors.container)
        .text_color(colors.text)
        .text_sm()
        .cursor_pointer()
        .hover(|style| style.opacity(0.9))
        .active(|style| style.opacity(0.8))
        .child(label.to_owned())
}

pub fn session_title(session: &Value) -> String {
    session["name"]
        .as_str()
        .or(session["firstPrompt"].as_str())
        .map(|text| text.chars().take(40).collect())
        .unwrap_or_else(|| session["sessionId"].as_str().unwrap_or_default().to_owned())
}

pub fn text_of_parts(parts: &[Value]) -> String {
    parts
        .iter()
        .filter_map(|part| match part["type"].as_str() {
            Some("text") => part["text"].as_str().map(str::to_owned),
            Some("toolCall") => Some(format!(
                "-> {}({})",
                part["name"].as_str().unwrap_or_default(),
                part["arguments"]
            )),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// An entry of the history (Durable's entry shape).
pub fn entry_element(entry: &Value, colors: &Colors) -> AnyElement {
    let message = &entry["model"][0];
    let text = match &message["content"] {
        Value::String(text) => text.clone(),
        Value::Array(parts) => text_of_parts(parts),
        _ => String::new(),
    };
    match entry["kind"].as_str() {
        Some("pi.user") => plain_block("", &text, colors),
        Some("pi.assistant") => block("", &text, colors, false),
        Some("pi.tool-result") => {
            let label = format!(
                "{}{}",
                message["toolName"].as_str().unwrap_or("tool"),
                if message["isError"] == Value::Bool(true) {
                    " (error)"
                } else {
                    ""
                }
            );
            block(&label, &text, colors, true)
        }
        Some("pi.compaction") => div()
            .text_xs()
            .text_color(colors.disabled)
            .child(format!(
                "compacted: {}",
                entry["data"]["summary"].as_str().unwrap_or_default()
            ))
            .into_any_element(),
        Some(other) => div()
            .text_xs()
            .text_color(colors.disabled)
            .child(other.to_owned())
            .into_any_element(),
        None => div().into_any_element(),
    }
}

/// A labelled Markdown block, or a diff-aware plain-text block for a tool.
pub fn block(label: &str, text: &str, colors: &Colors, tool: bool) -> AnyElement {
    let body = if tool {
        plain_body(text, true)
    } else {
        markdown_body(text, colors)
    };
    labelled_block(label, body, colors, tool)
}

fn plain_block(label: &str, text: &str, colors: &Colors) -> AnyElement {
    labelled_block(label, plain_body(text, false), colors, false)
}

fn labelled_block(label: &str, body: gpui::Div, colors: &Colors, tool: bool) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .rounded_md()
        .when(tool, |block| {
            block
                .gap_1()
                .p_2()
                .bg(colors.container)
                .font_family("monospace")
        })
        .when(!label.is_empty(), |block| {
            block.child(
                div()
                    .text_xs()
                    .text_color(colors.disabled)
                    .child(label.to_owned()),
            )
        })
        .child(body)
        .into_any_element()
}

fn plain_body(text: &str, tool: bool) -> gpui::Div {
    let mut body = div()
        .flex()
        .flex_col()
        .text_size(px(14.))
        .line_height(px(22.));
    for line in text.lines() {
        let color = if tool && line.starts_with('+') && !line.starts_with("+++") {
            Some(gpui::rgb(0x3fa65a))
        } else if tool && line.starts_with('-') && !line.starts_with("---") {
            Some(gpui::rgb(0xcc4d4d))
        } else {
            None
        };
        let row = div().child(if line.is_empty() {
            " ".to_owned()
        } else {
            line.to_owned()
        });
        body = body.child(match color {
            Some(color) => row.text_color(color),
            None => row,
        });
    }
    body
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MarkdownSegment {
    Line { kind: BlockKind, text: String },
    Fence { language: String, code: String },
}

#[derive(Debug)]
struct OpenFence {
    marker: &'static str,
    language: String,
    lines: Vec<String>,
}

fn markdown_segments(text: &str) -> Vec<MarkdownSegment> {
    let mut segments = Vec::new();
    let mut open_fence: Option<OpenFence> = None;
    for source_line in text.split('\n') {
        let line = source_line.strip_suffix('\r').unwrap_or(source_line);
        if let Some(fence) = open_fence.as_mut() {
            if fence_marker(line) == Some(fence.marker) {
                let fence = open_fence.take().expect("fence was just borrowed");
                segments.push(MarkdownSegment::Fence {
                    language: fence.language,
                    code: fence.lines.join("\n"),
                });
            } else if !(fence.lines.is_empty() && line.is_empty()) {
                fence.lines.push(line.to_owned());
            }
            continue;
        }
        if let Some(marker) = fence_marker(line) {
            let language = line.trim_start()[marker.len()..].trim().to_owned();
            open_fence = Some(OpenFence {
                marker,
                language,
                lines: Vec::new(),
            });
            continue;
        }
        segments.push(MarkdownSegment::Line {
            kind: classify_line(line),
            text: line.to_owned(),
        });
    }
    if let Some(fence) = open_fence {
        segments.push(MarkdownSegment::Fence {
            language: fence.language,
            code: fence.lines.join("\n"),
        });
    }
    segments
}

fn markdown_body(text: &str, colors: &Colors) -> gpui::Div {
    let mut body = div()
        .flex()
        .flex_col()
        .gap_2()
        .text_size(px(14.))
        .line_height(px(22.));
    for segment in markdown_segments(text) {
        body = body.child(match segment {
            MarkdownSegment::Line { kind, text } => markdown_line(&kind, &text, colors),
            MarkdownSegment::Fence { language, code } => code_card(&language, &code, colors),
        });
    }
    body
}

fn markdown_line(kind: &BlockKind, line: &str, colors: &Colors) -> gpui::Div {
    let render = block_render(kind, line);
    let content_byte = render.content_byte.min(line.len());
    let display = display_inline(&render.prefix, &line[content_byte..]);
    let row = div().child(inline_text(display, colors));
    match kind {
        BlockKind::Heading(1) => row.text_2xl().font_weight(FontWeight::BOLD),
        BlockKind::Heading(2) => row.text_xl().font_weight(FontWeight::BOLD),
        BlockKind::Heading(3) => row.text_lg().font_weight(FontWeight::SEMIBOLD),
        BlockKind::Heading(4) => row.text_base().font_weight(FontWeight::SEMIBOLD),
        BlockKind::Heading(5 | 6) => row.text_sm().font_weight(FontWeight::SEMIBOLD),
        BlockKind::Blockquote => row.text_color(colors.disabled),
        _ => row,
    }
}

fn inline_text(display: DisplayLine, colors: &Colors) -> StyledText {
    let DisplayLine { mut text, spans } = display;
    let accent = gpui::rgb_to_hsla(colors.selected);
    let mut code_ranges = Vec::new();
    let highlights = spans
        .iter()
        .filter_map(|span| {
            let range = span.range.clone();
            let style = match span.style {
                SpanStyle::Plain => return None,
                SpanStyle::Bold => HighlightStyle {
                    font_weight: Some(FontWeight::BOLD),
                    ..Default::default()
                },
                SpanStyle::Italic => HighlightStyle {
                    font_style: Some(FontStyle::Italic),
                    ..Default::default()
                },
                SpanStyle::Code => {
                    code_ranges.push((range.clone(), SharedString::from("monospace")));
                    HighlightStyle {
                        color: Some(accent),
                        background_color: Some(gpui::rgb_to_hsla(colors.container)),
                        ..Default::default()
                    }
                }
                SpanStyle::Strike => HighlightStyle {
                    strikethrough: Some(StrikethroughStyle {
                        thickness: px(1.),
                        color: None,
                    }),
                    ..Default::default()
                },
                SpanStyle::Link => HighlightStyle {
                    color: Some(accent),
                    underline: Some(UnderlineStyle {
                        thickness: px(1.),
                        color: Some(accent),
                        wavy: false,
                    }),
                    ..Default::default()
                },
                SpanStyle::BoldItalic => HighlightStyle {
                    color: Some(accent),
                    font_weight: Some(FontWeight::BOLD),
                    font_style: Some(FontStyle::Italic),
                    ..Default::default()
                },
            };
            Some((range, style))
        })
        .collect::<Vec<_>>();
    if text.is_empty() {
        text.push(' ');
    }
    StyledText::new(text)
        .with_highlights(highlights)
        .with_font_family_overrides(code_ranges)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CodeSpan {
    range: Range<usize>,
    color: u32,
    bold: bool,
    italic: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HighlightedLine {
    text: String,
    spans: Vec<CodeSpan>,
}

fn syntax_set() -> &'static SyntaxSet {
    static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme_set() -> &'static ThemeSet {
    static THEME_SET: OnceLock<ThemeSet> = OnceLock::new();
    THEME_SET.get_or_init(ThemeSet::load_defaults)
}

fn highlight_code(language: &str, code: &str, dark: bool) -> Vec<HighlightedLine> {
    let syntax_set = syntax_set();
    let token = language
        .split(|ch: char| ch.is_whitespace() || ch == ',')
        .next()
        .unwrap_or_default();
    let syntax = syntax_set
        .find_syntax_by_token(token)
        .unwrap_or_else(|| syntax_set.find_syntax_plain_text());
    let theme_name = if dark {
        "base16-ocean.dark"
    } else {
        "base16-ocean.light"
    };
    let theme = &theme_set().themes[theme_name];
    let mut highlighter = HighlightLines::new(syntax, theme);
    code.split('\n')
        .map(|line| {
            let mut offset = 0;
            let spans = highlighter
                .highlight_line(line, syntax_set)
                .map(|regions| {
                    regions
                        .into_iter()
                        .filter_map(|(style, region)| {
                            let start = offset;
                            offset += region.len();
                            if region.is_empty() {
                                return None;
                            }
                            Some(CodeSpan {
                                range: start..offset,
                                color: u32::from_be_bytes([
                                    style.foreground.r,
                                    style.foreground.g,
                                    style.foreground.b,
                                    style.foreground.a,
                                ]),
                                bold: style.font_style.contains(SyntectFontStyle::BOLD),
                                italic: style.font_style.contains(SyntectFontStyle::ITALIC),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            HighlightedLine {
                text: line.to_owned(),
                spans,
            }
        })
        .collect()
}

fn code_card(language: &str, code: &str, colors: &Colors) -> gpui::Div {
    let background = gpui::rgb_to_hsla(colors.background);
    let dark = background.lightness < 0.5;
    let highlighted = highlight_code(language, code, dark);
    let mut hasher = DefaultHasher::new();
    code.hash(&mut hasher);
    let id = hasher.finish();
    let mut lines = div()
        .id(SharedString::from(format!("code-body-{id}")))
        .flex()
        .flex_col()
        .w_full()
        .overflow_x_scroll()
        .px_4()
        .pb_4()
        .font_family("monospace")
        .text_size(px(14.))
        .line_height(px(22.))
        .whitespace_nowrap();
    for line in highlighted {
        let highlights = line
            .spans
            .iter()
            .map(|span| {
                (
                    span.range.clone(),
                    HighlightStyle {
                        color: Some(gpui::rgb_to_hsla(gpui::rgba(span.color))),
                        font_weight: span.bold.then_some(FontWeight::BOLD),
                        font_style: span.italic.then_some(FontStyle::Italic),
                        ..Default::default()
                    },
                )
            })
            .collect::<Vec<_>>();
        let text = if line.text.is_empty() {
            " ".to_owned()
        } else {
            line.text
        };
        lines = lines.child(
            div()
                .w_full()
                .child(StyledText::new(text).with_highlights(highlights)),
        );
    }
    let copied = code.to_owned();
    div()
        .flex()
        .flex_col()
        .my_3()
        .rounded_xl()
        .border_1()
        .border_color(colors.border)
        .bg(if dark {
            gpui::rgb(0x292929)
        } else {
            colors.container
        })
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .pl_4()
                .pr_2()
                .py_2()
                .child(icon(Icon::Code, 14., colors.disabled))
                .child(div().text_size(px(14.)).text_color(colors.text).child(
                    if language.is_empty() {
                        "Code".to_owned()
                    } else {
                        language.to_owned()
                    },
                ))
                .child(div().flex_1())
                .child(
                    div()
                        .id(SharedString::from(format!("code-copy-{id}")))
                        .w(px(26.))
                        .h(px(26.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_md()
                        .cursor_pointer()
                        .hover(|button| button.bg(colors.selected))
                        .child(icon(Icon::Copy, 14., colors.disabled))
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copied.clone()));
                        }),
                ),
        )
        .child(lines)
}

/// A plugin panel's view, in the widget vocabulary of spec §13.
pub fn panel_element(
    view: &Value,
    plugin: &str,
    panel: &str,
    colors: &Colors,
    on_action: impl Fn(String) + Clone + 'static,
) -> AnyElement {
    match view["type"].as_str() {
        Some("text") => div()
            .text_sm()
            .child(view["text"].as_str().unwrap_or_default().to_owned())
            .into_any_element(),
        Some("button") => {
            let action = view["action"].as_str().unwrap_or_default().to_owned();
            let (plugin, panel) = (plugin.to_owned(), panel.to_owned());
            button(
                format!("panel-{plugin}-{panel}-{action}"),
                view["label"].as_str().unwrap_or_default(),
                colors,
            )
            .on_click(move |_, _, _| on_action(action.clone()))
            .into_any_element()
        }
        Some("list") => {
            let mut list = div().flex().flex_col().gap_1();
            for item in view["items"].as_array().into_iter().flatten() {
                list = list.child(div().flex().gap_1().child("•").child(panel_element(
                    item,
                    plugin,
                    panel,
                    colors,
                    on_action.clone(),
                )));
            }
            list.into_any_element()
        }
        Some("stack") => {
            let mut stack = div().flex().flex_col().gap_1();
            for child in view["children"].as_array().into_iter().flatten() {
                stack = stack.child(panel_element(
                    child,
                    plugin,
                    panel,
                    colors,
                    on_action.clone(),
                ));
            }
            stack.into_any_element()
        }
        _ => div()
            .text_xs()
            .text_color(colors.disabled)
            .child(view.to_string())
            .into_any_element(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn session_title_prefers_name_then_prompt_then_id() {
        assert_eq!(
            super::session_title(&json!({"name":"Named","sessionId":"s-1"})),
            "Named"
        );
        assert_eq!(
            super::session_title(&json!({"firstPrompt":"Prompt","sessionId":"s-1"})),
            "Prompt"
        );
        assert_eq!(super::session_title(&json!({"sessionId":"s-1"})), "s-1");
    }

    #[test]
    fn text_parts_include_text_and_tool_calls() {
        let parts = vec![
            json!({"type":"text","text":"before"}),
            json!({"type":"toolCall","name":"bash","arguments":{"command":"pwd"}}),
            json!({"type":"image","data":"ignored"}),
        ];
        assert_eq!(
            super::text_of_parts(&parts),
            "before\n-> bash({\"command\":\"pwd\"})"
        );
    }

    #[test]
    fn public_entry_points_construct_markdown_plain_and_tool_elements() {
        let colors = gpui::colors::Colors::dark();
        let markdown = "# Title\n- **hé** and `code`\n> [link](https://example.com)\n```rust\nfn main() {\n\tprintln!(\"hi\");\n}\n```";
        let assistant = json!({"kind":"pi.assistant","model":[{"content":markdown}]});
        let user = json!({"kind":"pi.user","model":[{"content":"**plain**"}]});
        let tool = json!({"kind":"pi.tool-result","model":[{"content":"+ added\n- removed","toolName":"edit"}]});
        let _ = super::entry_element(&assistant, &colors);
        let _ = super::entry_element(&user, &colors);
        let _ = super::entry_element(&tool, &colors);
        let _ = super::block(
            "assistant (streaming)",
            "```rust\nlet partial =",
            &colors,
            false,
        );
    }

    #[test]
    fn fences_group_code_and_require_matching_markers() {
        let segments =
            super::markdown_segments("before\n~~~rust\n\nlet x = 1;\n```not a close\n~~~\nafter");
        assert_eq!(
            segments,
            vec![
                super::MarkdownSegment::Line {
                    kind: noel_markdown::BlockKind::Plain,
                    text: "before".into(),
                },
                super::MarkdownSegment::Fence {
                    language: "rust".into(),
                    code: "let x = 1;\n```not a close".into(),
                },
                super::MarkdownSegment::Line {
                    kind: noel_markdown::BlockKind::Plain,
                    text: "after".into(),
                },
            ]
        );
    }

    #[test]
    fn unterminated_streaming_fence_becomes_a_code_card() {
        assert_eq!(
            super::markdown_segments("```python\nprint('partial')"),
            vec![super::MarkdownSegment::Fence {
                language: "python".into(),
                code: "print('partial')".into(),
            }]
        );
    }

    #[test]
    fn syntax_highlighting_preserves_whitespace_and_display_ranges() {
        let code = "fn main() {\n\tprintln!(\"hé\");\n}";
        let lines = super::highlight_code("rust", code, true);
        assert_eq!(
            lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            code.split('\n').collect::<Vec<_>>()
        );
        let mut colors = std::collections::HashSet::new();
        for line in &lines {
            let mut previous_end = 0;
            for span in &line.spans {
                assert_eq!(span.range.start, previous_end);
                assert!(span.range.end <= line.text.len());
                assert!(line.text.is_char_boundary(span.range.start));
                assert!(line.text.is_char_boundary(span.range.end));
                previous_end = span.range.end;
                colors.insert(span.color);
            }
            assert_eq!(previous_end, line.text.len());
        }
        assert!(colors.len() > 1, "Rust tokens should have distinct colors");
    }

    #[test]
    fn unknown_languages_keep_code_verbatim() {
        let code = "  keep\tthis  ";
        let lines = super::highlight_code("not-a-language", code, true);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, code);
    }
}
