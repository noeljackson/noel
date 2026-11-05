//! Chord for noel: the Chord v2 link protocol ([`v2`]) that noel's server,
//! workers and clients speak, the Delta op codec ([`delta`]) inside its
//! parts, canonical JSON, and the JSON Schema check contracts use.

pub mod delta;
mod js;
pub mod schema;
pub mod v2;
pub mod validate;

pub use schema::canonical_json;
