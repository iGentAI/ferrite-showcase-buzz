# buzz-sdk

Build an agent that *lives* in a workspace — with its own keypair, its own
channel memberships, and the same affordances a human teammate has — in about
twenty lines.

```rust
use buzz_sdk::{Agent, Identity, Workspace};

#[tokio::main]
async fn main() -> buzz_sdk::Result<()> {
    let workspace = Workspace::builder("https://buzz.example.com")
        .identity(Identity::from_env("BUZZ_AGENT_KEY")?)
        .connect()
        .await?;

    Agent::builder(workspace)
        .name("release-bot")
        .instructions("You manage releases. Be terse and cite event ids.")
        .model("claude-sonnet-5")
        .tool(buzz_sdk::tool("cut_release", "Cut a release for a tag", cut_release))
        // Model access is explicit — build() refuses without a backend, because
        // a silent default would hold a provider key in this process. For local
        // dev; production agents pass a gateway-backed Backend instead.
        .backend(buzz_sdk::backend::AnthropicBackend::from_env()?)
        .run()
        .await
}
```

That agent joins its channels, watches for `@release-bot`, shows a typing
indicator while it thinks, calls the model with its instructions plus the
thread's context and its own memory, runs any tools the model asks for, and
replies in-thread — signed with its own key, auditable like any member's
message. Everything past `.run()` is the SDK's problem.

## Status

Built alongside the buzz relay, which now lives as the `relay` module of the
single-bundle `buzz` crate. The
ergonomics above are the API contract this crate is written to, and the two
programs in `examples/` compile against it unmodified.

What is proven, and by what:

| Capability | Proof |
|---|---|
| identity from `nsec`/hex/env, npub round trip | `identity.rs` unit tests |
| NIP-98 request signing | verified against `buzz::relay::auth::verify_header`, including the method/path binding and the skew window |
| SSE framing | `sse.rs` unit tests: heartbeat, reset, multi-line data, and a frame re-split at every byte boundary |
| per-channel single-flight + batching | `agent.rs` unit tests on the pure state machine, and `tests/agent_turns.rs` through the live run loop |
| tools: registration, typed dispatch, failure, hallucinated names, loop bound | `tool.rs` unit tests + `tests/agent_turns.rs` with `MockBackend` |
| memory write/recall as workspace events | `memory.rs` unit tests + `tests/agent_turns.rs` (recall reaches the system prompt) |
| cron schedules (UTC, fire-once-per-minute) | `cron.rs` unit tests, including the day-of-month/day-of-week OR rule |
| the core is I/O-free | the whole suite passes with `--no-default-features` |
| a live agent — real model, real relay, real SSE feed, private-channel authorization | `tests/live_agent.rs`: two `#[ignore]`d gates run with a real `ANTHROPIC_API_KEY` |

What the live gates do **not** prove, stated plainly: they run one live
`buzz::relay::ingress::serve` instance over `ferrite-app-test`'s in-process fleet
— real NIP-98 auth burned on use, real private-channel authorization, the real
guarded ingest saga, frames off the relay's real SSE pump — but the transport
hands the gateway's own envelope shape straight to the app's request queue, so
the HTTP hop and the gateway process are bypassed, and no run against a deployed
fleet is recorded. The in-process fake
(`buzz_sdk::testing::MemoryTransport`) reproduces the shapes agent code depends
on — publish/read-back, filters on reads and on live delivery, monotonic cursors
— and deliberately not the relay's access control, byte budgets, or reaction
channel resolution.

## Ferrite Platform Shape

Node-tier client library — **not** a constrained app, and it does not pretend to
be one. It depends on `buzz` (the single-bundle app crate) for the protocol
vocabulary (events, kinds,
filters, canonical serialization, BIP-340 signing) so the tree holds exactly one
implementation of the wire format rather than a client copy that drifts.

Transport is a trait:

- `HttpTransport` (feature `http`, on by default) — for agents that run as their
  own process anywhere: it speaks the relay's `/buzz/api/` routes with NIP-98
  signed requests and tails `/buzz/sub/` over SSE with durable cursors.
- Any `Transport` you supply — including one built from a constrained app's
  injected queue handles, so the same agent logic can run *inside* Ferrite as a
  deployed app instead of beside it.

The agent runtime is transport-agnostic on purpose: the mention loop, the
per-channel single-flight discipline, batching, the tool registry, and prompt
assembly are pure logic with no ambient I/O.

## Not Claimed

- Not a general Nostr client library. It speaks the buzz workspace protocol.
- The `http` feature is the only place network access lives; the core is I/O-free.
- Model access is the caller's, and explicit: `build()` refuses without a
  `.backend(...)` — a silent default would hold an ambient provider credential.
  `AnthropicBackend` (feature `http`) keeps a key in-process and is for local
  development. It is the only bundled backend: the former `gateway` feature,
  which linked the AI gateway's in-process engine library, is retired with that
  library. An agent that should hold no provider key runs inside an
  app that reaches the gateway over its queue binding.
- No key generation service — an `Identity` comes from a key you already hold.
- **Tool schemas are not derived from your argument type.** There is no schema
  crate here, so a tool advertises `{"type":"object"}` unless you attach a schema.
  Bad arguments come back to the model as a tool error it can correct; write the
  argument shape into the tool's description, or pass an explicit schema.
- **`search()` and memory `recall()` are bounded history scans**, ranked by
  prefix-tolerant word overlap. Not a search engine, no embeddings, no synonyms.
  Recall pages the global timeline and filters by kind and author client-side,
  because the relay's read route pages timelines and does not expose its
  by-author/by-kind indexes over HTTP.
- **Memory is durable, not private.** Engrams are not `#p`-gated or encrypted by
  the relay, so memory content is readable by the community. Do not put anything
  in a memory that you would not post in a channel.
- **`Answer::mentions_escalation()` is a lexical heuristic**, not a structured
  signal. For a decision that matters, give the agent an `escalate` tool and
  branch on whether it ran.
- **One process per agent identity.** Two replicas will both answer the same
  mention: the relay makes the *events* idempotent, not the model calls.
- Schedules are UTC, minute-granularity, and not backfilled across a restart.

## Validation

```bash
cargo test -p buzz-sdk                          # unit + integration + doc tests
cargo test -p buzz-sdk --no-default-features    # the core is genuinely I/O-free
cargo clippy -p buzz-sdk --all-targets
cargo build -p buzz-sdk --examples              # the two examples are the contract

# The live gates: a real model, paid per run, so they are #[ignore]d.
ANTHROPIC_API_KEY=... cargo test -p buzz-sdk --test live_agent -- --ignored
```

The test-only dependencies are the SDK crates `apps/buzz` links
(`ferrite-app` and `ferrite-app-test`, from the public SDK checkout beside this
repository), so the crate builds with that checkout in place and nothing else.
The four ignored tests are the two live gates and two doctests. The live fleet injects a disposable
SSO shared secret and signs every request in, because the relay runs the Ferrite
SSO property gate before any route; NIP-98 still decides who is calling.
