//! Claude Code transcripts: reading one as a session to import, and listing those of a directory.

use super::*;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};

/// How much of a transcript's end and start a listing reads for its title.
const TITLE_WINDOW: u64 = 256 * 1024;

/// The reply an import ends a turn with that Claude Code had not finished.
pub(crate) const UNFINISHED_NOTE: &str =
    "(Imported from Claude Code while this turn was unfinished there. Nothing more of it ran.)";

/// Whether a JSONL file is a Claude Code transcript: its records carry a
/// camel-case `sessionId`, which neither noel's journal nor pi's export has.
pub(crate) fn jsonl_is_claude_code_transcript(content: &[u8]) -> bool {
    content
        .split(|byte| *byte == b'\n')
        .filter(|line| !is_blank_line(line))
        .take(4)
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .any(|record| record.get("sessionId").is_some_and(Value::is_string))
}

/// What every transcript line is read as first: the conversation's records
/// are a tree by `uuid` and `parentUuid`, and most lines are not part of it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Link {
    #[serde(rename = "type", default)]
    kind: String,
    uuid: Option<String>,
    parent_uuid: Option<String>,
    #[serde(default)]
    is_sidechain: bool,
}

struct Node {
    parent: Option<String>,
    /// A `user` or `assistant` record; other records only link the tree.
    record: Option<Value>,
}

/// A compaction that kept its newest records: they were written before the
/// boundary, and belong after `anchor` (the summary), `head` first.
struct PreservedSegment {
    head: String,
    tail: String,
}

struct ToolResult {
    content: String,
    media: Vec<MediaInput>,
    is_error: bool,
}

/// Reads a Claude Code transcript as an export: the conversation on its
/// active branch since its last compaction, whose summary opens it. Tool
/// calls keep Claude Code's names and arguments; thinking is kept as text.
/// A call without a result gets one saying so, and a turn left unfinished
/// is ended with [`UNFINISHED_NOTE`].
/// Subagent records, attachments and injected (meta) messages are left out.
/// The export has no id, so each import is a new session.
pub(crate) fn read_claude_code_transcript(
    path: &Path,
    content: &[u8],
) -> Result<SessionExport, SessionError> {
    let mut nodes = Vec::<Node>::new();
    let mut by_uuid = HashMap::<String, usize>::new();
    let mut segments = HashMap::<String, PreservedSegment>::new();
    let mut replies = HashMap::<String, Vec<usize>>::new();
    let mut results = HashMap::<String, ToolResult>::new();
    let mut leaf = None;
    let mut custom_title = None;
    let mut ai_title = None;
    for line in content.split(|byte| *byte == b'\n') {
        // An unterminated or damaged line is skipped: the rest still reads.
        let Ok(link) = serde_json::from_slice::<Link>(line) else {
            continue;
        };
        let value = || serde_json::from_slice::<Value>(line).unwrap_or(Value::Null);
        match link.kind.as_str() {
            "custom-title" => custom_title = text_field(&value(), "customTitle"),
            "ai-title" => ai_title = text_field(&value(), "aiTitle"),
            "user" | "assistant" | "system" | "attachment" if !link.is_sidechain => {
                let Some(uuid) = link.uuid else {
                    continue;
                };
                let index = nodes.len();
                let record = match link.kind.as_str() {
                    "user" => {
                        let record = value();
                        collect_tool_results(&record, &mut results);
                        Some(record)
                    }
                    "assistant" => {
                        let record = value();
                        replies
                            .entry(reply_id(&record, &uuid))
                            .or_default()
                            .push(index);
                        Some(record)
                    }
                    "system" => {
                        if let Some((anchor, segment)) = preserved_segment(&value()) {
                            segments.insert(anchor, segment);
                        }
                        None
                    }
                    _ => None,
                };
                nodes.push(Node {
                    parent: link.parent_uuid,
                    record,
                });
                by_uuid.insert(uuid.clone(), index);
                leaf = Some(uuid);
            }
            _ => {}
        }
    }

    let mut messages = Vec::new();
    let mut tool_history = Vec::new();
    let mut cwd = None;
    let mut placed_replies = HashSet::new();
    for index in active_path(&nodes, &by_uuid, &segments, leaf) {
        let Some(record) = &nodes[index].record else {
            continue;
        };
        if let Some(dir) = record.get("cwd").and_then(Value::as_str) {
            cwd = Some(PathBuf::from(dir));
        }
        if record["type"] == "user" {
            messages.extend(user_message(record));
            continue;
        }
        // A reply is one record per content block, and parallel tool calls
        // fork the tree, so a reply is gathered from every record of it.
        let id = reply_id(record, "");
        if !placed_replies.insert(id.clone()) {
            continue;
        }
        let blocks = replies.get(&id).cloned().unwrap_or_else(|| vec![index]);
        let Some(reply) =
            assistant_message(blocks.iter().filter_map(|i| nodes[*i].record.as_ref()))
        else {
            continue;
        };
        let calls = reply.tool_calls.clone();
        messages.push(reply);
        for call in calls {
            // A call that was interrupted has no result; requests need one.
            let result = results.remove(&call.id).unwrap_or(ToolResult {
                content: "(no result recorded)".to_string(),
                media: Vec::new(),
                is_error: true,
            });
            tool_history.push(ToolEvent {
                id: call.id.clone(),
                name: call.name.clone(),
                result: result.content.clone(),
                is_error: result.is_error,
            });
            messages.push(ConversationMessage {
                role: MessageRole::Tool,
                content: result.content,
                thinking: String::new(),
                media: result.media,
                tool_call_id: Some(call.id),
                tool_name: Some(call.name),
                tool_calls: Vec::new(),
                aborted: false,
            });
        }
    }
    if messages.is_empty() {
        return Err(SessionError::Open {
            path: path.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "no conversation in this Claude Code transcript",
            ),
        });
    }
    let mut state = SessionState::new(String::new(), cwd.unwrap_or_else(|| PathBuf::from(".")));
    state.name = custom_title.or(ai_title);
    state.labels.insert("claude-code".to_string());
    state.messages = messages;
    state.tool_history = tool_history;
    // Opening a session with a turn unfinished continues it. Claude Code may
    // still be running this one, so the import ends it and says so.
    if state.has_unfinished_turn() {
        state.messages.push(ConversationMessage {
            role: MessageRole::Assistant,
            content: UNFINISHED_NOTE.to_string(),
            thinking: String::new(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
            aborted: false,
        });
    }
    Ok(SessionExport::from(&state))
}

/// The records from the conversation's start (or its last compaction) to
/// its last record, oldest first.
fn active_path(
    nodes: &[Node],
    by_uuid: &HashMap<String, usize>,
    segments: &HashMap<String, PreservedSegment>,
    leaf: Option<String>,
) -> Vec<usize> {
    let mut path = Vec::new();
    let mut seen = HashSet::new();
    let mut next = leaf;
    while let Some(uuid) = next {
        if let Some(segment) = segments.get(&uuid) {
            let mut kept = Some(segment.tail.clone());
            while let Some(uuid) = kept {
                let Some(&index) = by_uuid.get(&uuid) else {
                    break;
                };
                if !seen.insert(index) {
                    break;
                }
                path.push(index);
                kept = (uuid != segment.head)
                    .then(|| nodes[index].parent.clone())
                    .flatten();
            }
        }
        let Some(&index) = by_uuid.get(&uuid) else {
            break;
        };
        if !seen.insert(index) {
            break;
        }
        path.push(index);
        next = nodes[index].parent.clone();
    }
    path.reverse();
    path
}

fn text_field(record: &Value, key: &str) -> Option<String> {
    record
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(ToString::to_string)
}

/// The API message an assistant record is a block of, or the record itself.
fn reply_id(record: &Value, uuid: &str) -> String {
    record
        .pointer("/message/id")
        .and_then(Value::as_str)
        .or_else(|| record.get("uuid").and_then(Value::as_str))
        .unwrap_or(uuid)
        .to_string()
}

fn preserved_segment(record: &Value) -> Option<(String, PreservedSegment)> {
    let segment = record.pointer("/compactMetadata/preservedSegment")?;
    let uuid = |key: &str| segment.get(key).and_then(Value::as_str).map(str::to_string);
    Some((
        uuid("anchorUuid")?,
        PreservedSegment {
            head: uuid("headUuid")?,
            tail: uuid("tailUuid")?,
        },
    ))
}

fn content_blocks(record: &Value) -> &[Value] {
    record
        .pointer("/message/content")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn image_block(block: &Value) -> Option<MediaInput> {
    let source = block.get("source")?;
    Some(MediaInput {
        mime_type: source.get("media_type")?.as_str()?.to_string(),
        data_base64: source.get("data")?.as_str()?.to_string(),
        path: None,
        width: None,
        height: None,
    })
}

/// The text and images of a prompt's or a tool result's content.
fn text_and_images(content: &Value) -> (String, Vec<MediaInput>) {
    if let Some(text) = content.as_str() {
        return (text.to_string(), Vec::new());
    }
    let mut texts = Vec::new();
    let mut media = Vec::new();
    for block in content.as_array().into_iter().flatten() {
        match block["type"].as_str() {
            Some("text") => texts.extend(block["text"].as_str()),
            Some("image") => media.extend(image_block(block)),
            _ => {}
        }
    }
    (texts.join("\n"), media)
}

fn collect_tool_results(record: &Value, results: &mut HashMap<String, ToolResult>) {
    for block in content_blocks(record) {
        if block["type"] != "tool_result" {
            continue;
        }
        let Some(id) = block["tool_use_id"].as_str() else {
            continue;
        };
        let (content, media) = text_and_images(&block["content"]);
        results.entry(id.to_string()).or_insert(ToolResult {
            content,
            media,
            is_error: block["is_error"] == true,
        });
    }
}

/// A prompt, or the summary a compaction left. Tool results are placed
/// after their calls; what Claude Code injected (meta) is left out.
fn user_message(record: &Value) -> Option<ConversationMessage> {
    let flag = |key: &str| record[key] == true;
    if !flag("isCompactSummary") && (flag("isMeta") || flag("isVisibleInTranscriptOnly")) {
        return None;
    }
    let (content, media) = text_and_images(record.pointer("/message/content")?);
    if content.is_empty() && media.is_empty() {
        return None;
    }
    Some(ConversationMessage {
        role: MessageRole::User,
        content,
        thinking: String::new(),
        media,
        tool_call_id: None,
        tool_name: None,
        tool_calls: Vec::new(),
        aborted: false,
    })
}

/// One reply from its records. An API error Claude Code wrote as a reply,
/// and a reply with nothing in it, are none.
fn assistant_message<'a>(records: impl Iterator<Item = &'a Value>) -> Option<ConversationMessage> {
    let mut texts = Vec::new();
    let mut thinking = String::new();
    let mut tool_calls = Vec::<ChatToolCall>::new();
    for record in records {
        if record["isApiErrorMessage"] == true || record["message"]["model"] == "<synthetic>" {
            continue;
        }
        for block in content_blocks(record) {
            match block["type"].as_str() {
                Some("text") => {
                    texts.extend(block["text"].as_str().filter(|text| !text.is_empty()))
                }
                Some("thinking") => {
                    thinking.push_str(block["thinking"].as_str().unwrap_or_default())
                }
                Some("tool_use") => {
                    let (Some(id), Some(name)) = (block["id"].as_str(), block["name"].as_str())
                    else {
                        continue;
                    };
                    if tool_calls.iter().all(|call| call.id != id) {
                        tool_calls.push(ChatToolCall {
                            id: id.to_string(),
                            name: name.to_string(),
                            arguments: block["input"].to_string(),
                        });
                    }
                }
                _ => {}
            }
        }
    }
    if texts.is_empty() && thinking.is_empty() && tool_calls.is_empty() {
        return None;
    }
    Some(ConversationMessage {
        role: MessageRole::Assistant,
        content: texts.join("\n\n"),
        thinking,
        media: Vec::new(),
        tool_call_id: None,
        tool_name: None,
        tool_calls,
        aborted: false,
    })
}

/// A Claude Code transcript on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeCodeSession {
    pub path: PathBuf,
    /// Claude Code's session id, the file's name.
    pub id: String,
    /// Its title, else its last or first prompt, else its id.
    pub title: String,
    pub modified: Option<SystemTime>,
}

/// Where Claude Code keeps its transcripts: `projects` under
/// `CLAUDE_CONFIG_DIR`, or under `~/.claude`.
pub fn claude_code_projects_dir() -> Option<PathBuf> {
    let config = match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => std::env::home_dir()?.join(".claude"),
    };
    Some(config.join("projects"))
}

/// The transcripts Claude Code keeps for `cwd`, newest first. Claude Code
/// names a directory's folder by its path, each character that is not a
/// letter or a digit replaced by `-`.
pub fn claude_code_sessions(projects: &Path, cwd: &Path) -> Vec<ClaudeCodeSession> {
    let folder = cwd
        .to_string_lossy()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect::<String>();
    let Ok(entries) = fs::read_dir(projects.join(folder)) else {
        return Vec::new();
    };
    let mut sessions = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("jsonl"))
        .filter_map(|path| {
            let id = path.file_stem()?.to_str()?.to_string();
            let modified = fs::metadata(&path).ok()?.modified().ok();
            let title = transcript_title(&path).unwrap_or_else(|| id.clone());
            Some(ClaudeCodeSession {
                path,
                id,
                title,
                modified,
            })
        })
        .collect::<Vec<_>>();
    sessions.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.id.cmp(&b.id)));
    sessions
}

/// The session `reference` names among `sessions`: its place in the list
/// (from 1), its id, or a prefix of its id that no other shares.
pub fn find_claude_code_session<'a>(
    sessions: &'a [ClaudeCodeSession],
    reference: &str,
) -> Result<&'a ClaudeCodeSession, String> {
    if let Some(session) = reference
        .parse::<usize>()
        .ok()
        .and_then(|place| sessions.get(place.checked_sub(1)?))
    {
        return Ok(session);
    }
    let mut matching = sessions
        .iter()
        .filter(|session| session.id.starts_with(reference));
    match (matching.next(), matching.next()) {
        (Some(session), None) => Ok(session),
        (Some(_), Some(_)) => Err(format!(
            "more than one Claude Code session starts with {reference}"
        )),
        _ => Err(format!(
            "no Claude Code session {reference} for this directory"
        )),
    }
}

/// A transcript's title without reading all of it: Claude Code writes the
/// title and the last prompt again as the file grows, so its end has them.
fn transcript_title(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    let mut window = |start: u64| {
        let mut bytes = Vec::new();
        file.seek(SeekFrom::Start(start)).ok()?;
        Read::by_ref(&mut file)
            .take(TITLE_WINDOW)
            .read_to_end(&mut bytes)
            .ok()?;
        Some(bytes)
    };
    let end = window(length.saturating_sub(TITLE_WINDOW))?;
    let records = |bytes: &[u8]| {
        bytes
            .split(|byte| *byte == b'\n')
            .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
            .collect::<Vec<_>>()
    };
    let last = |records: &[Value], kind: &str, key: &str| {
        records
            .iter()
            .rev()
            .filter(|record| record["type"] == kind)
            .find_map(|record| text_field(record, key))
    };
    let end = records(&end);
    let title = last(&end, "custom-title", "customTitle")
        .or_else(|| last(&end, "ai-title", "aiTitle"))
        .or_else(|| last(&end, "last-prompt", "lastPrompt"))
        .or_else(|| {
            records(&window(0)?)
                .iter()
                .filter(|record| record["type"] == "user")
                .filter_map(|record| Some(user_message(record)?.content))
                // Not a command's markup, which Claude Code writes as a prompt.
                .find(|prompt| !prompt.starts_with('<'))
        })?;
    Some(trim_summary_text(&title))
}
