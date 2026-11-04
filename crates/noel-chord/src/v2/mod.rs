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
//! - [`link`]: byte links; [`object`]: what a provider serves; [`host`]:
//!   hosts with both sides of each link and remote references.

pub mod binary;
pub mod channel;
pub mod check;
pub mod contract;
pub mod error;
pub mod host;
pub mod link;
pub mod message;
pub mod object;
pub mod part;

pub use channel::{BinaryCodec, Channel, ChannelOptions, Codec, ProtocolError, Received, Tick};
pub use contract::{hash64, Contract, ContractError, Declaration, HostKind};
pub use error::{ChordError, Code, PluginError, WireError, PROTOCOL_VERSION};
pub use host::{
    Host, HostEvent, HostOptions, LinkOptions, RefStatus, RemoteRef, StateStream, StateUpdate,
};
pub use link::Link;
pub use message::{Message, Part};
pub use object::{CallContext, Implementation};
pub use part::{Limits, PartLimits};
