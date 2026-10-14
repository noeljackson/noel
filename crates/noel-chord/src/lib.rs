//! Chord link protocol 2 for noel, ported from upstream pi's
//! `packages/chord`:
//!
//! - [`wire`]: every frame, checked on receipt as upstream's frame schema
//!   checks it;
//! - [`link`]: in-memory, WebSocket and NDJSON links;
//! - [`host`]: hosts serving object types per principal, connections with
//!   `hello`, holds, calls, subscriptions, pings, acknowledgements and the
//!   flow-control window, principal stamping, routes and relay, and
//!   `invalidate_principal`;
//! - [`delta`], [`schema`] and [`contract`]: delta ops and their wire codec,
//!   canonical JSON, `hash64`, and contract descriptions with their
//!   fingerprints and coverage.
//!
//! Each matches upstream at the commit pinned in the repository's Makefile
//! (`CHORD_REFERENCE_REF`). `tests/fixtures/ts-parity/chord-delta.json` and
//! `chord-link.json` hold the vectors and frames that prove it.

pub mod contract;
pub mod delta;
pub mod error;
pub mod host;
mod js;
pub mod link;
pub mod object;
pub mod schema;
pub mod validate;
pub mod wire;

pub use contract::{
    coverage, fingerprint, ContractError, Description, MemberDescription, PropertyMap, Scope,
};
pub use error::{ChordError, ErrorCode, WireError};
pub use host::{
    AcceptOptions, Host, HostEvent, HostOptions, LinkOptions, PrincipalMatch, RefStatus, RemoteRef,
    Route, Shape, StateStream, StateUpdate,
};
pub use link::{Delivery, Link, Transport, WebSocketOptions};
pub use object::{AbortSignal, BoxFuture, CallContext, Cell, Implementation};
pub use schema::{canonical_json, fits, hash64};
pub use wire::{Frame, Kind, PROTOCOL_VERSION};
