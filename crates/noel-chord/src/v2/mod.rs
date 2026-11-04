//! Chord v2 (upstream `packages/chord` at `CHORD2_REFERENCE_REF`), ported a
//! layer at a time beside the v1 protocol noel speaks today
//! (`docs/chord-v2-port.md`). Nothing here is on a link yet.
//!
//! - [`contract`]: contracts, their schema normal form and fingerprints.
//! - [`message`] and [`binary`]: the messages and the binary codec.

pub mod binary;
pub mod contract;
pub mod message;

pub use contract::{hash64, Contract, ContractError, Declaration, HostKind};
pub use message::{Message, Part};
