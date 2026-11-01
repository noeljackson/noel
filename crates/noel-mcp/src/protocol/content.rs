//! Tool result content, as upstream's `content.ts`: the block types a
//! server may return, and their conversion to the text and images a
//! model consumes.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One content block of a tool result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Image {
        /// Base64.
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    Audio {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    ResourceLink {
        uri: String,
        name: String,
    },
    Resource {
        resource: EmbeddedResource,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EmbeddedResource {
    Text {
        uri: String,
        #[serde(rename = "mimeType", default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
        text: String,
    },
    Blob {
        uri: String,
        #[serde(rename = "mimeType", default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
        /// Base64.
        blob: String,
    },
}

/// A `tools/call` answer. `content` is required by the spec, but servers
/// that return only `structuredContent` omit it; the client defaults it
/// to empty on receipt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallToolResult {
    #[serde(default)]
    pub content: Vec<ContentBlock>,
    /// Any JSON value (MCP 2026-07-28; earlier revisions allowed only
    /// objects).
    #[serde(
        rename = "structuredContent",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub structured_content: Option<Value>,
    #[serde(rename = "isError", default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

/// Tool result content in the shape a model consumes: text, and base64
/// images with their mime type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LlmContent {
    Text(String),
    Image { data: String, mime_type: String },
}

/// Converts a tool result for a model, as upstream `toLlmContent`: text
/// and images pass through, embedded text resources become text,
/// embedded image resources become images, and other blocks (audio,
/// resource links, binary resources) become a short placeholder. A
/// result without content blocks but with `structuredContent` becomes
/// its JSON, since servers should, but do not always, mirror structured
/// results as text.
pub fn to_llm_content(result: &CallToolResult) -> Vec<LlmContent> {
    let mut content = result.content.iter().map(block_to_llm).collect::<Vec<_>>();
    if content.is_empty() {
        if let Some(structured) = &result.structured_content {
            content.push(LlmContent::Text(
                serde_json::to_string_pretty(structured).unwrap_or_default(),
            ));
        }
    }
    content
}

fn block_to_llm(block: &ContentBlock) -> LlmContent {
    match block {
        ContentBlock::Text { text } => LlmContent::Text(text.clone()),
        ContentBlock::Image { data, mime_type } => LlmContent::Image {
            data: data.clone(),
            mime_type: mime_type.clone(),
        },
        ContentBlock::Audio { mime_type, .. } => {
            LlmContent::Text(format!("[audio {mime_type} omitted]"))
        }
        ContentBlock::ResourceLink { name, uri } => LlmContent::Text(format!("{name}: {uri}")),
        ContentBlock::Resource { resource } => match resource {
            EmbeddedResource::Text { text, .. } => LlmContent::Text(text.clone()),
            EmbeddedResource::Blob {
                uri,
                mime_type,
                blob,
            } => {
                if mime_type
                    .as_deref()
                    .is_some_and(|mime| mime.starts_with("image/"))
                {
                    LlmContent::Image {
                        data: blob.clone(),
                        mime_type: mime_type.clone().unwrap_or_default(),
                    }
                } else {
                    LlmContent::Text(format!(
                        "[binary resource {uri} ({}) omitted]",
                        mime_type.as_deref().unwrap_or("unknown type")
                    ))
                }
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_and_images_pass_through() {
        let result: CallToolResult = serde_json::from_value(json!({
            "content": [
                {"type": "text", "text": "hello"},
                {"type": "image", "data": "aGk=", "mimeType": "image/png"}
            ]
        }))
        .unwrap();
        assert_eq!(
            to_llm_content(&result),
            vec![
                LlmContent::Text("hello".into()),
                LlmContent::Image {
                    data: "aGk=".into(),
                    mime_type: "image/png".into()
                },
            ]
        );
    }

    #[test]
    fn structured_content_stands_in_for_empty_content() {
        let result: CallToolResult = serde_json::from_value(json!({
            "structuredContent": {"rows": 3}
        }))
        .unwrap();
        assert_eq!(
            to_llm_content(&result),
            vec![LlmContent::Text("{\n  \"rows\": 3\n}".into())]
        );
    }

    #[test]
    fn audio_links_and_binary_become_placeholders() {
        let result: CallToolResult = serde_json::from_value(json!({
            "content": [
                {"type": "audio", "data": "aGk=", "mimeType": "audio/wav"},
                {"type": "resource_link", "uri": "file:///x", "name": "x"},
                {"type": "resource", "resource": {"uri": "file:///bin", "blob": "aGk=", "mimeType": "application/pdf"}},
                {"type": "resource", "resource": {"uri": "file:///img", "blob": "aGk=", "mimeType": "image/png"}}
            ]
        }))
        .unwrap();
        let content = to_llm_content(&result);
        assert_eq!(
            content[0],
            LlmContent::Text("[audio audio/wav omitted]".into())
        );
        assert_eq!(content[1], LlmContent::Text("x: file:///x".into()));
        assert_eq!(
            content[2],
            LlmContent::Text("[binary resource file:///bin (application/pdf) omitted]".into())
        );
        assert!(matches!(content[3], LlmContent::Image { .. }));
    }
}
