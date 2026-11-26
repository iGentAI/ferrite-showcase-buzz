//! The buzz workspace protocol — the relay module of the single-bundle app.
//!
//! A signed-event workspace: every message, reaction, membership change, and
//! agent turn is a Schnorr-signed event in one durable log. Ported from `buzz`
//! (a Nostr-protocol relay over Postgres + Redis + S3) onto the Ferrite
//! constrained-app model, where the app holds no sockets, no filesystem, no
//! ambient clock, and exactly one dependency.
//!
//! ## Shape
//!
//! * **Ingest** (`ingest`) is a guarded saga: an APE `commit_n_if` admission
//!   envelope elects one writer per event id, effects are idempotent and
//!   self-recognizing, and the triggering queue envelope is the resume driver.
//! * **Reads** (`query`) walk ordered Store index keys with byte-budgeted pages.
//! * **Live delivery** (`delivery`) appends every accepted event to a durable
//!   per-community feed; SSE pumps tail it with `(generation, offset)` cursors.
//!   There is no fan-out registry: every connection reads the same log.
//! * **Personas** (`persona`) is the in-process face the agent runtime calls:
//!   the same restriction, membership, moderation, rate-limit, and admission
//!   chain a client write runs, entered by a function call instead of a seam
//!   queue — see `crate::agents::turns` for the other half.
//! * **Schedules** (`schedules`) wakes a persona at a future instant. There is no
//!   timer here and none is needed: the queue plane's delayed-delivery gate holds
//!   the same `TurnRequest` a mention publishes until it is due, so a scheduled
//!   turn arrives through the same consumer and the same admission.
//! * **ACP** (`acp_surface`) is the face an EDITOR drives: buzz as an Agent Client
//!   Protocol agent, where `session/prompt` runs the same persona turn a mention
//!   does. Perpendicular to `mcp_surface`, which connects an agent to tools.
//! * **Crypto** (`crypto`) is vendored safe Rust — the platform gate admits no
//!   second dependency and no C FFI, so SHA-256, HMAC, BIP-340 Schnorr over
//!   secp256k1, and bech32 are implemented here from their specifications.
//!
//! ## What this module does not do
//!
//! It opens no listener (the gateway turns HTTP into queue envelopes), reads no
//! clock but the injected one, and mints no keys (no platform entropy source).

#![deny(clippy::unwrap_used)]
#![deny(clippy::indexing_slicing)]
#![allow(clippy::result_large_err)]

pub use crate::crypto;

pub mod access;
pub mod acp_surface;
pub mod agent_approvals;
pub mod agents;
pub mod alerts;
pub mod archive;
pub mod assets;
pub mod audit;
pub mod auth;
pub mod b64;
pub mod channel;
pub mod delivery;
pub mod effects;
pub mod error;
pub mod event;
pub mod filter;
pub mod ingest;
pub mod ingress;
pub mod invites;
pub mod keys;
pub mod kind;
pub mod mcp_surface;
pub mod media;
pub mod operator;
pub mod persona;
pub mod planes;
pub mod query;
pub mod schedules;
pub mod search;
pub mod session;
pub mod store;
pub mod workflows;
pub mod ws;

pub use error::{BuzzError, Result};
pub use event::{Event, EventId, PubKey};
pub use kind::Kind;

/// The app identity: what `/buzz/api/metrics` reports and log lines carry.
pub const APP_ID: &str = "buzz";

/// The community this deployment serves. One buzz deployment is one community
/// (the gateway never forwards the matched Host, so a single instance cannot
/// serve many hosts); keys still carry the id so the model is N=1 of the
/// multi-community layout rather than a different layout.
pub const DEFAULT_COMMUNITY: &str = "main";
