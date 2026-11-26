# Buzz on Ferrite

This repository is licensed under the Apache License 2.0 ([LICENSE](LICENSE)) for the material derived from [block/buzz](https://github.com/block/buzz) and OpenAI Codex, and under the MIT License ([LICENSE-MIT](LICENSE-MIT)) for iGent AI's own work. [NOTICE](NOTICE) says which files fall under which licence.

Buzz is a team workspace where people and AI teammates share the same rooms. Channels, direct messages, threads, reactions, mentions, membership, moderation, media, search, presence, and live updates all run inside one Ferrite application. Every message and administrative change is a signed event, and people and AI teammates enter rooms through the same authorization path, so an agent has no backdoor around workspace policy.

Buzz is a full-scale proof of what a single constrained Ferrite application can carry: the server, the browser client it serves, a Tauri desktop shell, a Rust SDK for workspace agents, and the conformance and verification tooling around them. It builds and tests from this checkout with the public Ferrite SDK beside it.

## Upstream lineage

Buzz builds on the open-source [block/buzz](https://github.com/block/buzz) workspace (Apache License 2.0), a Nostr-based team client with a Rust relay over Postgres, Redis, and S3, a TypeScript client in a Tauri shell, and separate agent harnesses.

Three pieces carry over from upstream:

- The browser client in `frontends/buzz-web/src`, pinned to upstream `desktop/src` (`4cd82f51`), with recorded adaptations for browser hosting.
- The wire protocol, including event kinds, access rules, and HTTP and WebSocket shapes, so the copied client and upstream CLI tools work against the Ferrite server.
- Core relay domain logic (kind registry, invite tokens, audit chain, filter matching, and access gate) ported from upstream Rust and attributed in [NOTICE](NOTICE).

Everything beneath the protocol was rebuilt for Ferrite:

- Storage, admission, and live delivery run on Ferrite's managed store, queue, guarded transaction (APE), log, and blob services with zero direct sockets or external databases.
- The agent runtime and model boundary run directly inside the application, handling teammate mentions, durable sessions, and provider calls without separate harness processes.
- MCP and ACP interfaces are served directly from the application through shared Ferrite source packages.
- A thin browser seam maps Tauri shell APIs, transport, and asset loading to a standard browser tab, while `frontends/buzz-desktop` wraps the same bundle in a lightweight Tauri shell.

Upstream synchronization is tracked by four files and executed via `docs/upstream-sync-playbook.md`:

| Record | Contents |
| --- | --- |
| `frontends/buzz-web/UPSTREAM-PIN.md` | Pinned upstream commit and tree mapping for the client |
| `frontends/buzz-web/scripts/upstream-manifest.json` | SHA-256 digests for every pinned upstream file for offline drift checks |
| `frontends/buzz-web/scripts/upstream-allowlist.json` | Recorded local modifications and additions with rationale |
| `apps/buzz/UPSTREAM-CONTRACT.md` and `apps/buzz/upstream-contract/kind.rs` | Pinned kind table and ledger of ported, registered, or declined relay changes |

## Architecture on Ferrite

### One constrained application

A Ferrite application compiles from source under a single crate dependency, `ferrite-app`, with no ambient sockets, filesystem, clock, or randomness. Every capability arrives as an injected handle declared in `apps/buzz/ferrite.json`.

`apps/buzz` puts the entire Buzz backend into one crate of roughly 90,000 lines of Rust across 152 files (including about 17,000 lines composed from shared Ferrite source packages). An earlier architecture split the relay, agent runtime, and model gateway into three separate services; consolidating them into one crate lets modules call one another as ordinary Rust functions while enforcing one unified authorization boundary for both people and AI teammates.

| Module | Responsibility |
| --- | --- |
| `relay` | Workspace protocol: event validation and signatures, channels, DMs, threads, reactions, membership, moderation, invites, media, search, schedules, workflows, operator routes, and SSE and WebSocket delivery |
| `agents` | AI teammate runtime: durable sessions, prompt assembly, tool loop, compaction, file workspaces, memory, and A2A client for external agent services |
| `llm` | Model boundary: model catalogue, routing, and request/response translation for supported providers |
| `mcp`, `acp` | Model Context Protocol and Agent Client Protocol servers composed from Ferrite source packages |
| `crypto` | Pure-Rust secp256k1, BIP-340 Schnorr signatures, and bech32 built from the specifications under the one-dependency rule |
| `ferrite_sso`, `http_query`, `http_cookies`, `http_change_password`, `http_route_budget`, `crypto_kernel` | Shared Ferrite source packages |

### Platform services and routes

Seventeen runtime bindings are declared in `apps/buzz/ferrite.json`:

| Kind | Count | Purpose |
| --- | --- | --- |
| Queue | 5 | Four carry gateway traffic (requests, replies, stream opens, WebSocket frames); `agent-turns` buffers durable agent turns |
| Store | 6 | Events and indexes, config and sessions, model routing config, agent workspace index, agent memory, and single-use sign-in nonces |
| APE (guarded transactions) | 2 | Atomic event admission, audit chain, and channel state; durable agent sessions |
| Log | 1 | Live event delivery feed read per subscriber cursor |
| Blob | 3 | Uploaded media, pushed web client bundles, and agent file workspaces |

Sixteen gateway routes sit in front of the application: twelve API routes (including root aliases `/events`, `/query`, `/count`, `/info`, `/health`, `/_liveness`, `/_readiness` for upstream tools plus a `/` catch-all), one SSE stream route (`/buzz/sub/`), one WebSocket route (`/buzz/`), and two content routes (`/buzz/ui/` and `/buzz/blob/`).

### Identity, queues, storage, and egress

Every member holds a Schnorr key pair generated in the browser on first visit. Because consecutive HTTP requests can land on any replica, each HTTP call carries a short-lived signed NIP-98 authorization event validated against a sixty-second freshness window and replay table. WebSockets authenticate via a NIP-42 challenge after presenting an HTTP-issued ticket, and SSE streams open with a signed expiring token. AI teammates receive key pairs derived deterministically from the relay's identity secret, with ownership bound to the first creator of a teammate name.

Because Ferrite queues deliver at least once, every mutating path is idempotent. Event admission runs inside a guarded transaction keyed on event ID so duplicate deliveries return the recorded outcome. Teammate mentions enqueue durable turn requests on `agent-turns` so a process restart never drops a mention, while session admission deduplicates redeliveries. Scheduled wakes reuse delayed queue delivery with zero background timers.

Events sit under ordered store keys so timelines, threads, and inverted-index searches (capped at 32 index keys per message) execute as bounded range walks. Uploaded media is validated against its declared byte signature and stored content-addressed in blob storage under an HMAC-derived name. Every accepted event appends to a community log, and live stream readers re-check channel membership at delivery time so removing a member immediately cuts off open streams.

Outbound access is locked to the hosts declared in `ferrite.json` across five egress handles: model providers, an external agent service (a placeholder address in this repository), nine link-preview hosts, and two handles covering seventeen external MCP server origins. Only the application calls a model provider through `llm::complete` with injected provider secrets; browsers, SDK agents, and external MCP or ACP clients never hold provider credentials and can only trigger authorized, audited agent turns.

### Web client delivery

The relay serves `/buzz/ui/` from two sources (`apps/buzz/src/relay/ingress/ui_assets.rs`). A self-contained text build with inlined images and fonts is embedded directly in `apps/buzz/src/relay/static/ui` via `scripts/embed-buzz-ui.py` so every fresh deployment serves immediately. Once live, operators can push updated frontend bundles directly to the `web-assets` blob bucket using `push-ui.mjs` in `frontends/buzz-web`, which chunks files, verifies SHA-256 hashes, and commits a manifest so UI updates ship without redeploying the server.

## Why Buzz matters architecturally

A toy example proves an application can deploy; a 90,000-line real-time workspace tests whether Ferrite's constraints hold up for real production software. Buzz demonstrates four architectural properties directly in code:

- Full real-time collaboration runs cleanly with zero ambient sockets, zero ambient clock, and one crate dependency (`ferrite-app`), with cryptography and wire protocols compiled as audited source.
- At-least-once queue delivery composes cleanly with guarded transactions and admission records so event ingest, agent turns, and scheduled wakes remain safe under redelivery.
- Manifest-scoped egress turns model providers, link previews, and connector endpoints into platform-enforced policy rather than application convention.
- Collapsing relay, agent runtime, and model gateway into one constrained crate simplifies authorization, putting human and AI writes through one identical security path.

## Repository layout

| Path | Contents |
| --- | --- |
| `apps/buzz` | Server crate, `ferrite.json`, test suite, and upstream contract ledger |
| `apps/buzz-process-app` | Companion application linking branch and pull-request activity to Buzz rooms |
| `frontends/buzz-web` | Browser client (`src/`), browser seam, Playwright specs (`e2e/`), upstream sync scripts, and operator scripts |
| `frontends/buzz-desktop` | Tauri desktop shell (standalone Cargo workspace) |
| `tools/buzz-sdk` | Rust SDK for agents joining a workspace under their own key |
| `scripts/buzz-conformance.sh`, `scripts/buzz-conformance/` | Runner executing upstream `block/buzz` end-to-end suites against a deployed relay |
| `scripts/embed-buzz-ui.py` | Regenerates the embedded web client bundle |
| `docs/buzz.md`, `docs/upstream-sync-playbook.md` | Product documentation and the upstream sync procedure |

## Setup

Requires Rust 1.89+, Node.js 22+, pnpm, and the `ferrite` CLI. Clone the public SDK ([iGentAI/ferrite-sdk](https://github.com/iGentAI/ferrite-sdk)) beside this repository; the crates take `ferrite-app` and `ferrite-app-test` from it by path. A `block/buzz` checkout is only needed for upstream syncs and upstream conformance runs.

```bash
git clone https://github.com/iGentAI/ferrite-sdk
git clone https://github.com/iGentAI/ferrite-showcase-buzz
curl -fsSL https://downloads.ferrite.cloud/install.sh | sh
```

```text
<parent>/
  ferrite-showcase-buzz/   this repository
  ferrite-sdk/             iGentAI/ferrite-sdk
  buzz/                    block/buzz (optional)
```

## Build and test

```bash
# The server: code gate, manifest statics, cargo check, then the test suite
cd apps/buzz
ferrite check
ferrite test

# Process cell and agent SDK (plain cargo; --no-fail-fast runs every test binary)
(cd ../buzz-process-app && cargo test --no-fail-fast)
(cd ../../tools/buzz-sdk && cargo test --no-fail-fast)

# Desktop shell core
cargo test --manifest-path ../../frontends/buzz-desktop/Cargo.toml -p buzz-desktop-core

# Browser client build, unit tests, and upstream pin check
cd ../../frontends/buzz-web
pnpm install --frozen-lockfile
pnpm build
node --import ./test-loader.mjs --test "src/**/*.test.mjs"
npm run sync:check-offline
```

To update the embedded web bundle after editing `frontends/buzz-web`:

```bash
(cd frontends/buzz-web && npm run build:hosted) && python3 scripts/embed-buzz-ui.py
```

All local suites run with zero provider credentials, using a scripted model in place of live inference. Live-model tests (`tools/buzz-sdk/tests/live_agent.rs` and the live-reply Playwright specs) opt in when provider keys are configured.

### Vendored packages

Files under `apps/buzz/src` and `apps/buzz-process-app/src` marked `// @generated by ferrite-pkg` are copies of Ferrite source packages, and `ferrite-packages.lock.json` records the digest of each file. Treat them as generated: change the application code around them rather than the copies.

At this snapshot the server suite passes 1,431 tests and five fail, one in each of `severe_bounds_gate`, `severe_private_removal_notice_gate`, `severe_process_contract_gate`, `severe_protocol_gate` and `severe_reply_header_gate`. They are adversarial cases the server does not yet handle; `ferrite check` is clean.

## Deploy

The server deploys from `apps/buzz` with the `ferrite` CLI:

```bash
cd apps/buzz
ferrite login --ephemeral --endpoint https://mcp.ferrite.cloud/mcp   # a trial account; skip it if `ferrite account` already shows yours
ferrite check                       # also lists the secrets the deployment needs
ferrite secrets set <name>          # once per secret below; the value is read from stdin
ferrite deploy
```

A deployment reports ready only when its required secrets are set, and each one gates a whole surface:

| Secret | Value | Used for |
| --- | --- | --- |
| `buzz-relay-identity` | 32 random bytes as hex (`openssl rand -hex 32`) | The relay's signing identity and the root every AI teammate's key is derived from |
| `buzz-stream-token-key` | 32 or more random bytes | Signing the expiring tokens that open live streams |
| `buzz-invite-key` | 32 or more random bytes | Invite minting, claims, and policy-acceptance receipts |
| `buzz-mcp-key` | 32 or more random bytes | The MCP face |
| `buzz-mcp-resource` | The deployment's MCP URL, `https://<host>/mcp` | The MCP face |
| `sso_broker_origin`, `sso_shared_secret` | The origin of a sign-in broker and the secret shared with it | Sign-in in front of every route |
| `provider-key-<provider>` | A model provider API key, for example `provider-key-anthropic` | AI teammates; optional |

Set `buzz-relay-identity` before any AI teammate is created, and never change it afterwards: teammate keys derive from it, and a teammate created under a different identity cannot post its replies.

What you can and cannot do from this repository alone: everything under "Build and test" works without an account, and a deployment goes to your own account. Sign-in is the part you have to supply. The relay runs the `ferrite-sso` client (`apps/buzz/src/ferrite_sso`) in front of every route, which expects a broker that completes the provider sign-in and hands back a signed assertion. This repository contains the client half only, so a deployment needs a broker that speaks that protocol before a person can sign in.

`apps/buzz-process-app` deploys separately under two profiles, `ferrite.native.json` and `ferrite.github.json`. Its [README](apps/buzz-process-app/README.md) describes them.

## Verification

| Asset | Location | Coverage |
| --- | --- | --- |
| Server suites | `apps/buzz/src`, `apps/buzz/tests/` (`hosted_suite`, `severe_*` gates, pool test) | Full application against the in-process SDK harness: ingest, access control, live delivery, scripted-model agents, MCP, ACP, schedules, and workflows |
| Process cell, SDK, desktop | `apps/buzz-process-app`, `tools/buzz-sdk`, `frontends/buzz-desktop/core` | Process-cell profiles, workspace agent SDK, and Tauri shell core |
| Web client tests | `frontends/buzz-web/src/**/*.test.mjs`, `e2e/*.test.mjs`, `scripts/` | Client unit tests, e2e helper tests, and offline upstream drift check |
| Browser specs | `frontends/buzz-web/e2e/` (`*.spec.ts`, `probe-*.mjs`) | End-to-end Playwright flows against a live `BUZZ_ORIGIN`: identity, channels, DMs, invites, projects, reminders, connectors, live updates, and AI teammates |
| Upstream conformance | `scripts/buzz-conformance.sh`, `scripts/buzz-conformance/` | Unmodified `block/buzz` end-to-end suites compared against `expectations.tsv` |

See `docs/buzz.md` for the product guide and its current limits.

## Licensing

Material derived from two Apache-2.0 projects is under the Apache License 2.0; see [LICENSE](LICENSE):

- Material derived from `block/buzz` (`frontends/buzz-web`, the embedded web bundle, desktop shell icons, `apps/buzz/upstream-contract`, and ported relay modules under `apps/buzz/src/relay`).
- Patch parser and base agent prompts derived from OpenAI Codex under `apps/buzz/src/agents`.

iGent AI's original work in this repository is licensed under the MIT License; see [LICENSE-MIT](LICENSE-MIT). [NOTICE](NOTICE) lists exact file attribution and provenance. The Buzz name and artwork belong to Block, Inc.

The hosted Ferrite platform is proprietary and not open source.

## Support and security

Questions and support: info@igent.ai. Report a security issue privately to security@ferrite.cloud.
