//! The MCP wire protocol, ported from upstream pi's `packages/mcp`
//! (`src/protocol/jsonrpc.ts`, `src/protocol/types.ts`,
//! `src/protocol/content.ts`, at the reference the Makefile pins):
//! JSON-RPC 2.0 messages, the initialize handshake's types, tool
//! listings and tool results, and the conversion of a tool result to the
//! text and images a model consumes.
//!
//! Everything a server may omit is optional; everything upstream's
//! validators check, [`crate::client`] checks on receipt too.

pub mod content;
pub mod jsonrpc;
pub mod types;

pub const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";
/// Versions the client accepts from a server. Servers that do not support
/// the requested version answer with their own latest one, so older
/// versions stay accepted for servers built on older SDKs.
pub const SUPPORTED_PROTOCOL_VERSIONS: [&str; 4] = [
    LATEST_PROTOCOL_VERSION,
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
];

pub use content::{to_llm_content, CallToolResult, LlmContent};
pub use types::{Implementation, ServerCapabilities, Tool};
