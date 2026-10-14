//! The parts of Chord link protocol 2 that need no link: delta ops and their
//! wire codec, canonical JSON, `hash64`, and contract descriptions with their
//! fingerprints and coverage.
//!
//! Each matches upstream's `packages/chord` at the commit pinned in the
//! repository's Makefile (`CHORD_REFERENCE_REF`), and
//! `tests/fixtures/ts-parity/chord-delta.json` holds the vectors that prove
//! it. Links, frames, hosts and transports come later.

pub mod contract;
pub mod delta;
mod js;
pub mod schema;

pub use contract::{
    coverage, fingerprint, ContractError, Description, MemberDescription, PropertyMap, Scope,
};
pub use schema::{canonical_json, fits, hash64};
