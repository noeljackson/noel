//! Session exports: JSONL in upstream's format and HTML, importing an upstream JSONL export, and branch summaries.

use super::*;

pub fn write_session_export(state: &SessionState, path: &Path) -> Result<(), SessionError> {
    match path.extension().and_then(|value| value.to_str()) {
        Some("html") | Some("htm") => write_html_export(state, path),
        Some("jsonl") => write_jsonl_export(state, path),
        _ => {
            let content =
                serde_json::to_string_pretty(&SessionExport::from(state)).map_err(|source| {
                    SessionError::Parse {
                        path: path.to_path_buf(),
                        source,
                    }
                })?;
            fs::write(path, content).map_err(|source| SessionError::Write {
                path: path.to_path_buf(),
                source,
            })
        }
    }
}

pub(crate) fn write_jsonl_export(state: &SessionState, path: &Path) -> Result<(), SessionError> {
    let mut file = File::create(path).map_err(|source| SessionError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    for record in ts_jsonl_export_records(state) {
        let line = serde_json::to_string(&record).map_err(|source| SessionError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        writeln!(file, "{line}").map_err(|source| SessionError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    }
    Ok(())
}

pub(crate) fn ts_jsonl_export_records(state: &SessionState) -> Vec<Value> {
    let timestamp = ts_jsonl_timestamp();
    let mut records = vec![json!({
        "type": "session",
        "version": 3,
        "id": state.session_id,
        "timestamp": timestamp.clone(),
        "cwd": state.cwd,
        "parentSession": state.parent_session_id,
    })];
    let mut parent_id: Option<String> = None;
    macro_rules! next_entry {
        ($entry_type:expr_2021, $fields:expr_2021 $(,)?) => {
            push_ts_jsonl_entry(
                &mut records,
                &mut parent_id,
                &timestamp,
                $entry_type,
                $fields,
            );
        };
    }

    next_entry!(
        "custom",
        json!({
            "customType": "rust_session_state",
            "data": {
                "labels": state.labels,
                "disabled_tools": state.disabled_tools,
                "queued_messages": state.queued_messages(),
                "tool_history": state.tool_history,
                "usage": state.usage,
            },
        }),
    );
    if let Some(name) = &state.name {
        next_entry!("session_info", json!({ "name": name }));
    }
    if let Some(model) = &state.active_model {
        next_entry!(
            "model_change",
            json!({
                "provider": model.provider,
                "modelId": model.id,
            }),
        );
    }
    if let Some(level) = &state.active_thinking_level {
        next_entry!("thinking_level_change", json!({ "thinkingLevel": level }),);
    }
    for message in &state.messages {
        next_entry!("message", json!({ "message": ts_jsonl_message(message) }),);
    }
    for label in &state.labels {
        next_entry!("label", json!({ "label": label }),);
    }
    for record in &state.compactions {
        let first_kept_entry_id = parent_id.clone();
        next_entry!(
            "compaction",
            json!({
                "summary": record.summary,
                "firstKeptEntryId": first_kept_entry_id,
                "tokensBefore": record.omitted_messages,
                "details": {
                    "kind": record.kind,
                    "retained_messages": record.retained_messages,
                },
            }),
        );
    }
    for summary in &state.branch_summaries {
        next_entry!(
            "branch_summary",
            json!({
                "fromId": summary.from_session_id,
                "summary": summary.summary,
                "details": {
                    "to_session_id": summary.to_session_id,
                },
            }),
        );
    }
    records
}

pub(crate) fn ts_jsonl_timestamp() -> String {
    time::OffsetDateTime::from(SystemTime::now())
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

pub(crate) fn push_ts_jsonl_entry(
    records: &mut Vec<Value>,
    parent_id: &mut Option<String>,
    timestamp: &str,
    entry_type: &str,
    mut fields: Value,
) {
    let id = format!("e{}", records.len());
    let object = fields
        .as_object_mut()
        .expect("session export fields must be an object");
    object.insert("type".to_string(), json!(entry_type));
    object.insert("id".to_string(), json!(id.clone()));
    object.insert("parentId".to_string(), json!(parent_id));
    object.insert("timestamp".to_string(), json!(timestamp));
    *parent_id = Some(id);
    records.push(fields);
}

pub(crate) fn ts_jsonl_message(message: &ConversationMessage) -> Value {
    let role = match message.role {
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "toolResult",
        MessageRole::System => "system",
    };
    let mut value = json!({
        "role": role,
        "content": message.content,
        "thinking": message.thinking,
        "media": message.media,
        "toolCallId": message.tool_call_id,
        "toolName": message.tool_name,
        "toolCalls": message.tool_calls,
    });
    if message.aborted {
        value["stopReason"] = json!("aborted");
    }
    value
}

pub(crate) fn jsonl_is_ts_session_export(content: &[u8]) -> bool {
    content
        .split(|byte| *byte == b'\n')
        .find(|line| !is_blank_line(line))
        .and_then(|line| serde_json::from_slice::<Value>(line).ok())
        .and_then(|value| {
            value
                .get("type")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .as_deref()
        == Some("session")
}

/// Reads a TS-style JSONL session. As for Rust journals, an unterminated final
/// segment that does not parse is an interrupted write and is ignored.
pub(crate) fn read_ts_jsonl_export(
    path: &Path,
    content: &[u8],
) -> Result<SessionExport, SessionError> {
    let (complete, tail) = noel_session::split_jsonl_tail(content);
    let mut records = complete
        .split(|byte| *byte == b'\n')
        .filter(|line| !is_blank_line(line))
        .map(|line| {
            serde_json::from_slice::<Value>(line).map_err(|source| SessionError::Parse {
                path: path.to_path_buf(),
                source,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if let Ok(record) = serde_json::from_slice::<Value>(tail) {
        records.push(record);
    }
    let header = records.first().ok_or_else(|| SessionError::Open {
        path: path.to_path_buf(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, "missing session header"),
    })?;
    let session_id = header
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("imported")
        .to_string();
    let cwd = header
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let parent_session_id = header
        .get("parentSession")
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let mut state = SessionState::new(session_id, cwd);
    state.parent_session_id = parent_session_id;

    for entry in ts_jsonl_active_path(&records) {
        reduce_ts_jsonl_entry(&mut state, entry);
    }
    Ok(SessionExport::from(&state))
}

pub(crate) fn ts_jsonl_active_path(records: &[Value]) -> Vec<&Value> {
    let entries = records
        .iter()
        .skip(1)
        .filter(|entry| entry.get("id").and_then(Value::as_str).is_some())
        .collect::<Vec<_>>();
    let Some(leaf_id) = entries.iter().rev().find_map(|entry| {
        if entry.get("type").and_then(Value::as_str) == Some("leaf") {
            return entry
                .get("targetId")
                .and_then(Value::as_str)
                .map(ToString::to_string);
        }
        entry
            .get("id")
            .and_then(Value::as_str)
            .map(ToString::to_string)
    }) else {
        return Vec::new();
    };
    let mut path = Vec::new();
    let mut current = Some(leaf_id);
    while let Some(id) = current {
        let Some(entry) = entries
            .iter()
            .find(|entry| entry.get("id").and_then(Value::as_str) == Some(id.as_str()))
        else {
            break;
        };
        path.push(*entry);
        current = entry
            .get("parentId")
            .and_then(Value::as_str)
            .map(ToString::to_string);
    }
    path.reverse();
    path
}

pub(crate) fn reduce_ts_jsonl_entry(state: &mut SessionState, entry: &Value) {
    match entry.get("type").and_then(Value::as_str) {
        Some("message") => {
            if let Some(message) = entry.get("message").and_then(ts_jsonl_conversation_message) {
                state.messages.push(message);
            }
        }
        Some("session_info") => {
            state.name = entry
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(ToString::to_string);
        }
        Some("model_change") => {
            if let (Some(provider), Some(id)) = (
                entry.get("provider").and_then(Value::as_str),
                entry.get("modelId").and_then(Value::as_str),
            ) {
                state.active_model = Some(ModelRef {
                    provider: provider.to_string(),
                    id: id.to_string(),
                });
            }
        }
        Some("thinking_level_change") => {
            state.active_thinking_level = entry
                .get("thinkingLevel")
                .and_then(Value::as_str)
                .map(ToString::to_string);
        }
        Some("custom")
            if entry.get("customType").and_then(Value::as_str) == Some("rust_session_state") =>
        {
            restore_rust_session_state(state, entry.get("data").unwrap_or(&Value::Null));
        }
        Some("compaction") => {
            state.compactions.push(CompactionRecord {
                kind: entry
                    .pointer("/details/kind")
                    .cloned()
                    .and_then(|value| serde_json::from_value(value).ok())
                    .unwrap_or(CompactionKind::Manual),
                omitted_messages: entry
                    .get("tokensBefore")
                    .and_then(Value::as_u64)
                    .unwrap_or_default() as usize,
                retained_messages: entry
                    .pointer("/details/retained_messages")
                    .and_then(Value::as_u64)
                    .unwrap_or_default() as usize,
                summary: entry
                    .get("summary")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            });
        }
        Some("branch_summary") => {
            state.branch_summaries.push(BranchSummary {
                from_session_id: entry
                    .get("fromId")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
                to_session_id: entry
                    .pointer("/details/to_session_id")
                    .and_then(Value::as_str)
                    .unwrap_or(&state.session_id)
                    .to_string(),
                summary: entry
                    .get("summary")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            });
        }
        Some("label") => {
            if let Some(label) = entry.get("label").and_then(Value::as_str)
                && !label.trim().is_empty()
            {
                state.labels.insert(label.trim().to_string());
            }
        }
        _ => {}
    }
}

pub(crate) fn restore_rust_session_state(state: &mut SessionState, data: &Value) {
    if let Some(usage) = data
        .get("usage")
        .and_then(|value| serde_json::from_value(value.clone()).ok())
    {
        state.usage = usage;
    }
    if let Some(labels) = data.get("labels").and_then(Value::as_array) {
        state.labels = labels
            .iter()
            .filter_map(Value::as_str)
            .map(ToString::to_string)
            .collect();
    }
    if let Some(tools) = data.get("disabled_tools").and_then(Value::as_array) {
        state.disabled_tools = tools
            .iter()
            .filter_map(Value::as_str)
            .map(ToString::to_string)
            .collect();
        state.active_tool_names = active_tools_with(&state.disabled_tools);
    }
    if let Some(messages) = data.get("queued_messages").and_then(Value::as_array) {
        state.queue = messages
            .iter()
            .filter_map(Value::as_str)
            .map(QueuedEntry::follow_up)
            .collect();
    }
    if let Some(history) = data.get("tool_history").and_then(Value::as_array) {
        state.tool_history = history
            .iter()
            .filter_map(|event| serde_json::from_value(event.clone()).ok())
            .collect();
    }
}

pub(crate) fn ts_jsonl_conversation_message(value: &Value) -> Option<ConversationMessage> {
    // Upstream keeps an aborted reply, with this stop reason.
    let aborted = value.get("stopReason").and_then(Value::as_str) == Some("aborted");
    if let Ok(mut message) = serde_json::from_value::<ConversationMessage>(value.clone()) {
        message.aborted |= aborted;
        return Some(message);
    }
    let role = match value.get("role").and_then(Value::as_str)? {
        "user" => MessageRole::User,
        "assistant" => MessageRole::Assistant,
        "toolResult" | "tool" => MessageRole::Tool,
        "system" => MessageRole::System,
        _ => return None,
    };
    Some(ConversationMessage {
        thinking: value
            .get("thinking")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| {
                value
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|parts| {
                        parts
                            .iter()
                            .filter(|part| {
                                part.get("type").and_then(Value::as_str) == Some("thinking")
                            })
                            .filter_map(|part| part.get("thinking").and_then(Value::as_str))
                            .collect::<String>()
                    })
                    .unwrap_or_default()
            }),
        role,
        content: ts_jsonl_text_content(value.get("content").unwrap_or(&Value::Null)),
        media: value
            .get("media")
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default(),
        tool_call_id: value
            .get("toolCallId")
            .or_else(|| value.get("tool_call_id"))
            .and_then(Value::as_str)
            .map(ToString::to_string),
        tool_name: value
            .get("toolName")
            .or_else(|| value.get("tool_name"))
            .and_then(Value::as_str)
            .map(ToString::to_string),
        tool_calls: value
            .get("toolCalls")
            .or_else(|| value.get("tool_calls"))
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default(),
        aborted: aborted && value.get("role").and_then(Value::as_str) == Some("assistant"),
    })
}

pub(crate) fn ts_jsonl_text_content(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_string();
    }
    value
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| {
                    part.get("text")
                        .or_else(|| part.get("content"))
                        .and_then(Value::as_str)
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

pub(crate) fn write_html_export(state: &SessionState, path: &Path) -> Result<(), SessionError> {
    let mut content = String::new();
    content.push_str("<!doctype html><html><head><meta charset=\"utf-8\">");
    content.push_str("<title>noel session export</title>");
    content.push_str("<style>body{font-family:system-ui,sans-serif;line-height:1.45;margin:2rem;max-width:960px}pre{white-space:pre-wrap;background:#f5f5f5;padding:1rem;border-radius:6px}.message,.tool{border-top:1px solid #ddd;padding:1rem 0}.role{font-weight:700;text-transform:uppercase;font-size:.8rem;color:#555}.meta{color:#555}</style>");
    content.push_str("</head><body>");
    content.push_str("<h1>noel session export</h1>");
    content.push_str(&format!(
        "<p class=\"meta\">session: {}<br>cwd: {}</p>",
        escape_html(&state.session_id),
        escape_html(&state.cwd.display().to_string())
    ));
    if let Some(name) = &state.name {
        content.push_str(&format!(
            "<p class=\"meta\">name: {}</p>",
            escape_html(name)
        ));
    }
    if !state.labels.is_empty() {
        content.push_str(&format!(
            "<p class=\"meta\">labels: {}</p>",
            escape_html(&state.labels.iter().cloned().collect::<Vec<_>>().join(", "))
        ));
    }
    for message in &state.messages {
        content.push_str("<section class=\"message\">");

        if !message.thinking.is_empty() {
            content.push_str(&format!(
                "<details class=\"thinking\"><summary>Thinking</summary><pre>{}</pre></details>",
                escape_html(&message.thinking)
            ));
        }
        content.push_str(&format!(
            "<div class=\"role\">{:?}{}</div><pre>{}</pre>",
            message.role,
            if message.aborted { " (aborted)" } else { "" },
            escape_html(&message.content)
        ));
        content.push_str("</section>");
    }
    if !state.tool_history.is_empty() {
        content.push_str("<h2>tool history</h2>");
        for event in &state.tool_history {
            content.push_str("<section class=\"tool\">");
            content.push_str(&format!(
                "<div class=\"role\">{}</div><pre>{}</pre>",
                escape_html(&event.name),
                escape_html(&event.result)
            ));
            content.push_str("</section>");
        }
    }
    if !state.compactions.is_empty() {
        content.push_str("<h2>compactions</h2>");
        for record in &state.compactions {
            content.push_str("<section class=\"message\">");
            content.push_str(&format!(
                "<div class=\"role\">{:?}</div><pre>{}</pre>",
                record.kind,
                escape_html(&record.summary)
            ));
            content.push_str("</section>");
        }
    }
    if !state.branch_summaries.is_empty() {
        content.push_str("<h2>branch summaries</h2>");
        for summary in &state.branch_summaries {
            content.push_str("<section class=\"message\">");
            content.push_str(&format!(
                "<div class=\"role\">{} to {}</div><pre>{}</pre>",
                escape_html(&summary.from_session_id),
                escape_html(&summary.to_session_id),
                escape_html(&summary.summary)
            ));
            content.push_str("</section>");
        }
    }
    content.push_str("</body></html>");
    fs::write(path, content).map_err(|source| SessionError::Write {
        path: path.to_path_buf(),
        source,
    })
}

pub(crate) fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

pub(crate) fn summarize_branch(source: &SessionState) -> String {
    let name = source.name.as_deref().unwrap_or("-");
    let labels = if source.labels.is_empty() {
        "-".to_string()
    } else {
        source.labels.iter().cloned().collect::<Vec<_>>().join(", ")
    };
    let last_message = source
        .messages
        .last()
        .map(|message| trim_summary_text(&message.content))
        .unwrap_or_else(|| "-".to_string());
    format!(
        "Branched from {} with name {name}, labels {labels}, {} message(s), and last message: {last_message}.",
        source.session_id,
        source.messages.len()
    )
}

pub(crate) fn trim_summary_text(value: &str) -> String {
    let collapsed = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= 160 {
        return collapsed;
    }
    let mut trimmed = collapsed.chars().take(157).collect::<String>();
    trimmed.push_str("...");
    trimmed
}
