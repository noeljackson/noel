//! Codemode for noel: upstream pi's `codemode` (`packages/codemode`, the
//! coding agent's `extensions/codemode`) on the djs engine, natively.
//!
//! The specification is `docs/codemode-port.md`.

pub mod declarations;
pub mod execution;
pub mod format;
pub mod identifier;
pub mod search;
pub mod source;

pub use declarations::{
    render_declarations, render_tool_output_type, render_tool_sample, render_tool_signature,
    schema_to_type, ToolDeclaration, DEFAULT_INPUT_SCHEMA_MAX_CHARS,
};
pub use identifier::to_codemode_identifier;
pub use source::{parse_codemode_source, CodemodeSourceError, ParsedCodemodeSource, SourceOptions};
pub mod vm;
