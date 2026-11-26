//! # buzz — the whole workspace product as ONE constrained Ferrite app
//!
//! A signed-event workspace where humans and AI personas share the same rooms
//! — and ONE application: the relay protocol, the persona agent runtime, and
//! the model-provider boundary are modules of a single crate, built by one
//! attested source build and deployed as one app over the platform's core
//! planes: no sibling services, private seam queues, or seam tokens. Three
//! modules call each other in-process, with durability kept exactly where it
//! earns its cost.
//!
//! ## The composition
//!
//! * [`relay`] — the buzz workspace protocol: Schnorr-signed events, channels,
//!   DMs, threads, reactions, moderation, membership, SSE/WebSocket delivery,
//!   the operator plane, and the embedded browser client. Vendored safe-Rust
//!   cryptography (the code gate admits no second dependency).
//! * [`agents`] — the persona runtime: durable event-sourced sessions,
//!   compaction, prompt assembly, the tool loop, file workspaces, local
//!   memory, agent definitions/skills, subagents, and the Fabric A2A harness.
//! * [`llm`] — the provider boundary: the model catalog, per-model routing,
//!   Anthropic/OpenAI/Fireworks wire building and parsing, called as plain
//!   async functions over the app's audited `provider-egress` handle.
//! * [`mcp`] — Model Context Protocol 2026-07-28, composed from
//!   `packages/mcp`: the wire protocol, and the authorization machinery for
//!   consuming third-party MCP servers and for being one. A source package
//!   rather than a crate because the code gate admits no second dependency —
//!   the official Rust SDK requires tokio, futures and chrono, so it cannot
//!   run inside a constrained app at all.
//! * [`acp`] — the Agent Client Protocol v1, composed from `packages/acp`:
//!   the bidirectional JSON-RPC wire model, both roles, session lifecycle,
//!   and the HTTP/SSE + WebSocket transport binding. Perpendicular to MCP —
//!   that connects an agent to TOOLS, this connects an EDITOR to an agent —
//!   and a source package for the same reason: the official SDK pulls
//!   `futures`, `async-io`, `async-process` and `rustix`.
//!
//! ## Where the seams went
//!
//! * A mention becomes a durable [`agents::turns::TurnRequest`] on the
//!   app's own `agent-turns` queue — at-least-once delivery is what makes a
//!   mention survive a crash — and the session engine's admission (msg id =
//!   trigger event id) absorbs redeliveries into exactly one turn.
//! * A model step is a function call ([`llm::complete`]), not a request/reply
//!   queue pair with correlation polling and a cross-turn reply cache.
//! * A persona's workspace write is a function call
//!   ([`relay::persona::execute`]) that runs the SAME gates as any client
//!   event — registry, key check, membership, moderation, rate limits, the
//!   guarded ingest saga — with the deterministic event identity (the
//!   `agentcmd/` created-at reservation) preserved so retries stay absorbed.
//!
//! One deployment is one community, one origin, one upload.

#![forbid(unsafe_code)]

use ferrite_app::tracing;
use std::io;

use ferrite_app::AppContext;

/// The SSO contract, vendored by `tools/ferrite-pkg`. A client property
/// uses only the client half, so the broker's items are unused here.
#[allow(unused_imports, dead_code)]
pub mod ferrite_sso;
pub mod acp;
pub mod agents;
pub mod crypto;
pub mod crypto_kernel;
pub mod http_query;
pub mod llm;
pub mod mcp;
pub mod relay;

pub use relay::{APP_ID, DEFAULT_COMMUNITY};

/// The request headers in the SDK's egress shape: upstream Core takes
/// `EgressHeader { name, value }` pairs and returns no response head.
pub fn egress_headers(pairs: &[(String, String)]) -> Vec<ferrite_app::EgressHeader> {
    pairs
        .iter()
        .map(|(name, value)| ferrite_app::EgressHeader { name: name.clone(), value: value.clone() })
        .collect()
}

/// Run the whole product: the agent turn pipeline beside the relay's serve
/// loops, one process, one injected authority.
pub async fn run(ctx: AppContext) -> io::Result<()> {
    tracing::info!(app = APP_ID, "buzz starting (single bundle)");

    // The persona turn pipeline runs only where its queue is bound: a
    // deployment without the agents surface simply has no personas — nothing
    // else changes (the relay's dispatch hook is inert without the binding).
    if ctx.queue(agents::turns::TURNS).is_ok() {
        let turn_ctx = ctx.clone();
        ferrite_app::rt::spawn(async move {
            if let Err(e) = agents::turns::turn_loop(turn_ctx).await {
                tracing::error!("agent turn loop exited: {e}");
            }
        });
    } else {
        tracing::warn!(
            "no agent-turns binding; personas are not served on this deployment"
        );
    }

    relay::ingress::serve(ctx).await
}

/// Shared, credential-free password-change discovery.
pub mod http_change_password;

/// The cookie helpers, vendored by `tools/ferrite-pkg` from `packages/http-cookies`.
pub mod http_cookies;

/// Shared admission for matched HTTP routes.
mod http_route_budget;
