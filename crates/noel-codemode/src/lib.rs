//! Codemode for noel: upstream pi's `codemode` (`packages/codemode`, the
//! coding agent's `extensions/codemode`) on the djs engine, natively.
//!
//! The specification is `docs/codemode-port.md`.

pub mod identifier;
pub mod source;

pub use identifier::to_codemode_identifier;
pub use source::{parse_codemode_source, CodemodeSourceError, ParsedCodemodeSource, SourceOptions};
pub mod vm;
