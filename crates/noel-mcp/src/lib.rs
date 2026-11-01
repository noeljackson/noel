//! A Model Context Protocol client for noel, ported from upstream pi's
//! `packages/mcp` (the commit the Makefile's parity pin names):
//!
//! - [`protocol`]: JSON-RPC 2.0, the initialize handshake's types, tool
//!   listings and tool result content, and the conversion of a result to
//!   the text and images a model consumes — upstream's `protocol/`;
//! - [`stdio`]: the stdio transport — newline-delimited JSON-RPC over a
//!   server's stdin/stdout, the server in its own process group, shutdown
//!   per the spec (stdin close, SIGTERM, SIGKILL) — upstream's
//!   `transports/stdio.ts`;
//! - [`client`]: the [`McpClient`] — connect with the handshake, list
//!   tools through every page, call tools — upstream's `client.ts`.
//!
//! Not ported (yet): the streamable-HTTP transport and OAuth, which
//! upstream's `transports/streamable-http.ts` and `oauth/` hold; and
//! server push (notifications other than to ignore), resources and
//! prompts, which noel's tool surface does not use.

pub mod client;
pub mod protocol;
pub mod stdio;

pub use client::{McpClient, McpError, DEFAULT_REQUEST_TIMEOUT};
pub use protocol::{to_llm_content, CallToolResult, LlmContent, Tool};
pub use stdio::{StdioOptions, StdioTransport, TransportEvent};
