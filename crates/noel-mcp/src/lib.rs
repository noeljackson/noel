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
//! - [`http`]: the streamable-HTTP transport — POST answered as JSON or
//!   SSE, the server-to-client GET stream, session and protocol-version
//!   headers, `Last-Event-ID` resumption, reconnect with backoff —
//!   upstream's `transports/streamable-http.ts`;
//! - [`oauth`]: metadata discovery with issuer validation, dynamic
//!   client registration, the PKCE authorization-code flow, token
//!   exchange and refresh, and the loopback callback server —
//!   upstream's `oauth/` (adapted from the MCP TypeScript SDK).
//!
//! Not ported (yet): server push (notifications other than to ignore),
//! resources and prompts, which noel's tool surface does not use.

pub mod client;
pub mod http;
pub mod oauth;
pub mod protocol;
pub mod stdio;

pub use client::{McpClient, McpError, DEFAULT_REQUEST_TIMEOUT};
pub use http::{consume_sse_stream, HttpOptions, HttpTransport};
pub use oauth::{
    authorization_url, discover, exchange_code, parse_www_authenticate, pkce, refresh,
    register_client, AuthorizationServerMetadata, CallbackServer, ClientInformation,
    ClientMetadata, OAuthChallenge, OAuthError, OAuthTokens, ProtectedResourceMetadata,
};
pub use protocol::{to_llm_content, CallToolResult, LlmContent, Tool, LATEST_PROTOCOL_VERSION};
pub use stdio::{StdioOptions, StdioTransport, TransportEvent};
