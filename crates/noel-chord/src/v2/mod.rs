//! Chord v2 (upstream `packages/chord` at `CHORD2_REFERENCE_REF`), ported a
//! layer at a time beside the v1 protocol noel speaks today
//! (`docs/chord-v2-port.md`). Nothing here is on a link yet.
//!
//! - [`contract`]: contracts, their schema normal form and fingerprints.
//! - [`message`] and [`binary`]: the messages and the binary codec;
//!   [`check`]: the message schema check.
//! - [`error`]: the error table, `ChordError`, `PluginError`, wire errors.
//! - [`part`]: limits per part, compression and opening parts.
//! - [`channel`]: a host's end of a link (handshake, checks, heartbeat).

pub mod binary;
pub mod channel;
pub mod check;
pub mod contract;
pub mod error;
pub mod message;
pub mod part;

pub use channel::{BinaryCodec, Channel, ChannelOptions, Codec, ProtocolError, Received, Tick};
pub use contract::{hash64, Contract, ContractError, Declaration, HostKind};
pub use error::{ChordError, Code, PluginError, WireError, PROTOCOL_VERSION};
pub use message::{Message, Part};
pub use part::{Limits, PartLimits};
