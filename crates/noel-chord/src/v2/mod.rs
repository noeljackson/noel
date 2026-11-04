//! Chord v2 (upstream `packages/chord` at `CHORD2_REFERENCE_REF`), ported a
//! layer at a time beside the v1 protocol noel speaks today
//! (`docs/chord-v2-port.md`). Nothing here is on a link yet.
//!
//! - [`contract`]: contracts, their schema normal form and fingerprints.

pub mod contract;

pub use contract::{hash64, Contract, ContractError, Declaration, HostKind};
