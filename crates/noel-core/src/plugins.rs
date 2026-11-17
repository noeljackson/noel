//! Tools that plugins contribute (roadmap phase 7). A plugin is a process
//! `noel server` supervises; its tools reach a session through a
//! [`PluginToolHost`], which the session's worker gives its runtime. They
//! are offered as extension tools with the protocol `plugin`, and kept
//! across reloads.

use std::future::Future;
use std::pin::Pin;

use serde_json::Value;

/// What a plugin tool's call gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginToolOutput {
    pub output: String,
    pub is_error: bool,
}

/// Runs plugin tools for a runtime.
pub trait PluginToolHost: Send + Sync {
    fn call(
        &self,
        plugin: &str,
        tool: &str,
        args: Value,
    ) -> Pin<Box<dyn Future<Output = PluginToolOutput> + Send>>;
}

/// A tool a plugin contributes.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginTool {
    pub plugin: String,
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub replay_safe: bool,
}
