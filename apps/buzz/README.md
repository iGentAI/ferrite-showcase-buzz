# buzz

The whole Buzz product as one constrained Ferrite application: the signed-event
workspace relay, the persona agent runtime and the model-provider boundary are
modules of a single crate, built by one attested source build and deployed as
one app over the platform's core services.

| Module | What it is |
| --- | --- |
| `relay` | The Buzz workspace protocol: Schnorr-signed events, channels, DMs, threads, reactions, moderation, membership, SSE and WebSocket delivery, the operator plane, and the embedded browser client |
| `agents` | The persona runtime: durable event-sourced sessions, compaction, prompt assembly, the tool loop, file workspaces, local memory, definitions and skills, subagents, and the A2A client for an external agent service |
| `llm` | The provider boundary: the model catalogue, per-model routing, Anthropic, OpenAI and Fireworks wire building and parsing, prompt and reply bounds, cache accounting |
| `mcp` | Model Context Protocol, composed from the `mcp` source package: the wire protocol, OAuth 2.1 and the resource-server side |
| `acp` | Agent Client Protocol v1, composed from the `acp` source package: the JSON-RPC wire model, both roles and the transport binding |
| `crypto` | Pure-Rust secp256k1, BIP-340 Schnorr signatures and bech32, written from the specifications under the one-dependency rule |

The authority a persona write needs is decided by the relay's own gates, and
the only caller of the model boundary is this application.

## Platform shape

A source-deployable constrained app: no sockets, no filesystem, no ambient
clock, no entropy source and exactly one dependency. Every capability is an
injected handle named by `ferrite.json`:

- **Store**: events and indexes (`events`), operator config and sessions
  (`config`), model routing config (`gateway-config`), agent workspace index
  (`agent-workspace-index`), agent memory (`agent-memory`).
- **APE**: admission sagas, the audit chain, channel and thread guards
  (`guards`); durable agent sessions (`agent-sessions`).
- **Queue**: the gateway's request, reply, stream and WebSocket envelopes, and
  the app's own `agent-turns` trigger queue.
- **Log**: the live delivery feed and the changefeed (`feed`).
- **Blob**: media (`media`), the web client (`web-assets`), agent workspace
  content (`agent-workspace-content`).
- **Egress**: `provider-egress` scoped to the model providers,
  `fabric-a2a-egress` scoped to the one A2A peer, `link-preview-egress` scoped
  to the hosts this deployment previews links for, and the MCP connector
  catalogue across two handles, because a handle carries at most sixteen
  destinations.

The A2A peer in `ferrite.json` is the placeholder `https://agents.example.com/a2a/`.
Replace it, and the matching default in `src/agents/turns.rs`, with your own
agent service before you use the `a2a` or `fabric` harness, or leave it and use
the in-process `ferrite-cloud` harness only.

## Check, test and deploy

The crate takes `ferrite-app` and `ferrite-app-test` by path from a checkout
of the public SDK beside this repository (see the root
[README](../../README.md), "Setup"). From this directory:

```bash
ferrite check                       # code gate, ferrite.json statics, cargo check
ferrite test                        # the unit tests and every gate under tests/
cargo test --locked --no-fail-fast  # the same suite; keeps going after a failing binary
ferrite deploy                      # submit the source build with the signed-in account
```

`ferrite check` lists the secrets a deployment needs before it reports ready.
Set each with `ferrite secrets set <name>` before the deploy; the root README's
"Deploy" section says what each one holds.

Two Cargo features are off by default because the released platform builder
does not carry the methods they call: `platform-queue-cancel` (queue-level
recall of a published schedule wake) and `platform-topic-read-wait` (parking
the feed pumps in the log's `read_wait`). `tests/topic_retention_gate.rs` is
compiled out of the default build for the same reason.

## Upstream

The relay follows the wire contract of [block/buzz](https://github.com/block/buzz).
[UPSTREAM-CONTRACT.md](UPSTREAM-CONTRACT.md) records the pinned kind table and
every upstream relay change that was ported, registered or declined, and
`upstream-contract/kind.rs` is the unmodified upstream file the next comparison
starts from. The repository's [NOTICE](../../NOTICE) lists the modules derived
from upstream.
