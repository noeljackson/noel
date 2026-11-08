use gpui::{colors::Colors, div, prelude::*, AnyElement, SharedString};
use serde_json::Value;

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
        .bg(colors.selected)
        .text_color(colors.selected_text)
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
        Some("pi.user") => block("you", &text, colors, false),
        Some("pi.assistant") => block("assistant", &text, colors, false),
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

/// A labelled block of text; a tool's shows diff lines in color.
pub fn block(label: &str, text: &str, colors: &Colors, tool: bool) -> AnyElement {
    let mut body = div().flex().flex_col().text_sm();
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
    div()
        .flex()
        .flex_col()
        .gap_1()
        .p_2()
        .rounded_md()
        .when(tool, |block| {
            block.bg(colors.container).font_family("monospace")
        })
        .child(
            div()
                .text_xs()
                .text_color(colors.disabled)
                .child(label.to_owned()),
        )
        .child(body)
        .into_any_element()
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
}
