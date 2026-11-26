//! # buzz-sdk — build an agent that *lives* in a workspace
//!
//! An agent with its own keypair, its own channel memberships, and the same
//! affordances a human teammate has, in about twenty lines:
//!
//! ```no_run
//! use buzz_sdk::{Agent, Identity, Result, Workspace};
//!
//! #[tokio::main]
//! async fn main() -> Result<()> {
//!     let workspace = Workspace::builder(std::env::var("BUZZ_URL")?)
//!         .identity(Identity::from_env("BUZZ_AGENT_KEY")?)
//!         .connect()
//!         .await?;
//!
//!     Agent::builder(workspace)
//!         .name("hello-bot")
//!         .instructions("You are a friendly teammate. Answer briefly and cite event ids.")
//!         .model("claude-sonnet-5")
//!         .run()
//!         .await
//! }
//! ```
//!
//! That agent watches for mentions of its own key, shows a typing indicator while
//! it thinks, calls the model with its instructions plus the thread's context and
//! its own memory, runs any tools the model asks for, and replies in-thread —
//! signed with its own key, auditable like any member's message.
//!
//! ## The shape of the crate
//!
//! | Module | What it owns |
//! |---|---|
//! | [`identity`] | the agent's keypair, and NIP-98 request signing |
//! | [`workspace`] | sign / publish / read / subscribe, plus the clock |
//! | [`transport`] | the four-operation seam to the wire |
//! | [`http`] | the bundled HTTP + SSE transport (feature `http`) |
//! | [`agent`] | the mention loop, single-flight batching, schedules, cursors |
//! | [`turn`] | what a handler gets: typing, search, memory, think, reply |
//! | [`tool`](mod@tool) | typed async functions the model may call |
//! | [`backend`] | model access: the [`Backend`] trait, a mock, and the local-dev Anthropic client |
//! | [`memory`] | durable memory stored as workspace events |
//! | [`cron`] | the five-field UTC schedule matcher |
//! | [`sse`] | SSE framing, as pure logic |
//! | [`testing`] | an in-process fake workspace for your tests |
//!
//! ## Ferrite platform shape
//!
//! Node-tier **client library** — not a constrained app, and it does not pretend
//! to be one. It depends on `buzz` (the single-bundle app crate) for the protocol
//! vocabulary (events,
//! kinds, filters, canonical serialization, BIP-340 signing) so the tree holds
//! exactly one implementation of the wire format rather than a client copy that
//! drifts. Frames, cursors, filters and the dedup ring are the relay's own types,
//! re-exported here.
//!
//! Transport is a trait. The bundled [`HttpTransport`] is feature-gated, so the
//! agent runtime — the mention loop, the single-flight discipline, batching, the
//! tool registry, prompt assembly — is I/O-free and can be driven from inside a
//! constrained app over injected handles instead of beside it over HTTP.
//!
//! ## Not claimed
//!
//! * **Not a general Nostr client.** It speaks the buzz workspace protocol.
//! * **No key generation.** An [`Identity`] comes from a key you already hold.
//! * **No embedded credentials, and no provider default.** A backend is never
//!   chosen for you — [`AgentBuilder::build`](agent::AgentBuilder::build) refuses
//!   without an explicit one. The one bundled backend,
//!   [`backend::AnthropicBackend`] (feature `http`), is a hand-rolled
//!   single-provider client for out-of-process node-tier agents and holds a
//!   provider key in-process. It is a local-development client, not the
//!   platform's inference path: an agent that should hold no provider key runs
//!   inside an app that reaches the AI gateway over its queue binding.
//! * **Search and memory recall are bounded history scans**, not a search engine
//!   and not a vector store — see [`turn::SearchBuilder`] and [`memory`] for
//!   exactly what they do and what they cost.
//! * **Memory is durable, not private.** Engrams are not encrypted or `#p`-gated
//!   by the relay; read [`memory`] before putting anything sensitive in one.
//! * **One process per agent identity.** Two replicas of one agent will both
//!   answer the same mention; coordinating them is the relay's dispatcher's job,
//!   not a client library's.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::indexing_slicing)]
#![warn(missing_docs)]

pub mod agent;
pub mod backend;
pub mod cron;
pub mod error;
pub mod identity;
pub mod memory;
pub mod sse;
pub mod testing;
pub mod tool;
pub mod transport;
pub mod turn;
pub mod workspace;

#[cfg(feature = "http")]
pub mod http;

pub use agent::{
    Agent, AgentBuilder, AgentCore, CursorStore, Dispatcher, FileCursor, MemoryCursor, Skip,
};
pub use backend::{
    Backend, Completion, CompletionRequest, Message, MockBackend, Role, StopReason, ToolCall,
    ToolResult, ToolSpec, Usage,
};
pub use error::{Result, SdkError};
pub use identity::Identity;
pub use memory::{Memory, MemoryNote};
pub use tool::{tool, tool_with_schema, Tool, ToolRegistry};
pub use transport::{
    mention_filters, BoxFuture, Cursor, Frame, QueryPage, QueryRequest, SessionHandle, Transport,
};
pub use turn::{Answer, SearchBuilder, Trigger, Turn};
pub use workspace::{Clock, FixedClock, SystemClock, Workspace, WorkspaceBuilder};

#[cfg(feature = "http")]
pub use backend::AnthropicBackend;
#[cfg(feature = "http")]
pub use http::HttpTransport;

/// The protocol vocabulary, re-exported from the relay.
///
/// One implementation of the wire format, not a client copy: an [`Event`] built
/// here is the same type the relay ingests, canonicalizes and verifies.
pub use buzz::relay::{
    event::Event, event::EventId, event::PubKey, filter::Filter, kind, kind::Kind, BuzzError,
};

/// The crate version, for an agent that wants to announce itself.
///
/// A literal rather than `env!("CARGO_PKG_VERSION")`: build-time environment
/// reads are refused by the platform's code gate, and a client library should not
/// adopt a habit its sibling apps cannot. A unit test in this module keeps it in
/// step with the workspace manifest.
pub const VERSION: &str = "0.1.0";

#[cfg(test)]
mod tests {
    /// The one thing a hand-written version string can get wrong.
    #[test]
    fn version_matches_the_manifest() {
        let manifest = include_str!("../Cargo.toml");
        assert!(
            manifest.contains("version.workspace = true"),
            "this crate inherits the workspace version; update the check if that changes"
        );
        // Repo root is two levels up now that this crate lives under tools/.
        let workspace = include_str!("../../../Cargo.toml");
        let declared = workspace
            .lines()
            .find_map(|line| line.trim().strip_prefix("version = \""))
            .and_then(|rest| rest.split('"').next())
            .expect("the labs workspace declares a version");
        assert_eq!(declared, super::VERSION);
    }
}
