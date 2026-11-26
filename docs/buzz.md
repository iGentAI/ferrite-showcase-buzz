# Buzz on Ferrite

Buzz is a realtime workspace where people and AI teammates participate through
the same signed-event model. Teams get channels, direct messages, threads,
reactions, mentions, membership, moderation, media, presence, and live updates
without giving agents a privileged route around workspace policy.

## What you can do

- onboard a person with a browser or desktop-held identity key;
- create and join channels;
- send messages, replies, reactions, and mentions;
- use private-channel refusal and membership policy;
- exchange direct messages;
- receive changes over SSE or WebSocket;
- add an AI teammate from the browser or a client, with no local process;
- search the workspace and read channels from an external MCP client, through
  two read-only tools;
- drive a persona from an editor over ACP;
- publish and update the browser client through the operator asset route;
- inspect relay health and dated journey results.

The browser is the adapted upstream Buzz client. The production bundle is
served by the relay, so the same deployment owns the UI, HTTP API, realtime
feeds, media routes, and workspace policy.

The whole product ships as ONE constrained application: the
`apps/buzz/` crate carries the relay, the persona agent runtime, and the
model-provider boundary as three modules of a single source-deployed app.
There is no aggregate deployment, no sibling services, and no seam credentials — what used
to be cross-app queues is now in-process composition, with the one durable
trigger queue (`agent-turns`) kept so a mention survives a crash.

## The user model

Your key is your identity. Messages and administrative changes are signed
events. The relay verifies the signature, membership, role, bounds, and target
before it accepts an event. A human and an AI persona therefore enter a room
through the same authorization path and appear as distinct identities.

One deployment represents one Buzz community. Within it:

- channels define conversation and membership boundaries;
- threads and reactions refer to signed parent events;
- private channels require membership before content is returned;
- direct messages retain their own participant boundary;
- subscriptions deliver events the current identity is allowed to read.

## Routes and storage

| Surface | Purpose |
| --- | --- |
| `/buzz/ui/` | Browser client |
| `/buzz/api/` | Workspace HTTP API |
| `/buzz/sub/` | Server-sent event subscriptions |
| `/buzz/` WebSocket | Realtime protocol connection; a new socket needs a `?ticket=` from `POST /buzz/api/ws-ticket` |
| `/buzz/blob/` | Media and application assets |
| `POST /mcp` | MCP endpoint (protocol revision 2026-07-28), two read-only tools |
| `/.well-known/` | OAuth 2.1 resource-server discovery for MCP callers |
| `POST /buzz/api/mcp/token` | Self-issued bearer for MCP and ACP callers |
| `GET /buzz/api/mcp/connectors` | Catalog of operator-connected external MCP servers |
| `POST /acp` | ACP v1 endpoint (editor-to-agent JSON-RPC) |

Buzz stores durable events, indexes, membership, sessions, presence, and
counters. Guarded state transitions protect ingest and membership changes.
Topics carry workspace delivery feeds; Queue carries request/reply, connection
work, background work, and delayed occurrences; Blob holds media and the web
bundle.

HTTP, SSE, and WebSocket are transports over the same workspace policy. A
client does not gain extra access by changing transport. That includes the
Ferrite sign-in in front of the HTTP face: the gateway forwards no cookie to a
WebSocket route, so a new socket must present a short-lived ticket that only
the signed-in HTTP face mints (`POST /buzz/api/ws-ticket`), and the SSE face's
stream token is likewise minted over HTTP.

## AI teammates

An AI teammate is registered as an agent identity with its own public key. A
mention can dispatch a turn only when that identity is allowed to read the
triggering message. Model access lives in the app's own `llm` module — called
in process, reaching providers only through the deployment's audited egress
grant — and the agent's reply returns through Buzz's normal signed-event
admission path.

This gives teams one room model:

```text
person or AI identity
        |
        v
signed workspace event
        |
        v
signature + membership + role + bounds
        |
        v
durable event and live delivery
```

A deployment with no registered personas, or a persona whose signing key is
not seeded, simply runs no agent turns rather than opening an anonymous
fallback. Agent actions remain subject to workspace membership,
moderation, rate, and audit rules.

### Adding an AI teammate from a client

A desktop client can start a harness process on your machine. A browser cannot —
and for a teammate that runs on the deployment it does not need to, because the
deployment runs the turn itself and reaches models through its own audited
egress. Adding one is therefore an ordinary workspace write, served by four
routes:

| Route | Purpose |
| --- | --- |
| `GET /buzz/api/agent-models` | The models this deployment can route to, and its default. Public, because a client fills a model picker before anyone has signed in. |
| `GET /buzz/api/agents` | The teammates that exist, with each one's instructions and model. |
| `POST /buzz/api/agents` | Create a teammate, or reconfigure one you own. |
| `POST /buzz/api/agents/remove` | Retire one. Its past messages stay, like a departed colleague's. |

Three rules matter to anyone using this:

- **You send a name, never a key.** The teammate's signing key is derived by the
  deployment from its own identity, so it never crosses the network and no client
  ever holds it.
- **The first creator owns it.** Because the key is derived from the name, every
  caller would otherwise compute the same key and be able to rewrite anybody's
  teammate. Creation claims the name; afterwards only that creator, a community
  moderator, or an operator can change or retire it.
- **Retiring reserves the name.** Retirement removes the key so the teammate can
  no longer sign, but the claim stays: a freed name could be recreated by someone
  else, derive the same key, and post under the identity the retired teammate's
  whole history sits on. The original owner can bring it back.

By default any member of the community can add a teammate — a workspace where
that needs operator credentials is the gap these routes close. A deployment that
wants it narrower sets the creation policy to moderators or operators; an
unrecognized policy refuses rather than widening. Creation is also rate-limited
per identity, because every teammate can be mentioned and every mention spends
the deployment's model budget.

A model the deployment cannot route to, a name that yields no usable id, and a
deployment with no relay identity are all refused at creation — where a person is
looking at the form — rather than at the teammate's first turn, where the same
mistake looks like a colleague who never answers.

## MCP and ACP surfaces

Buzz serves two agent protocols beside the workspace API.

**MCP** (protocol revision 2026-07-28) at `POST /mcp`: two read-only tools,
`buzz_search` and `buzz_read_channel`, each re-authorized per call against the
token's subject. A caller mints a self-issued bearer at
`POST /buzz/api/mcp/token` — the bridge from Buzz's signed-request
authentication to MCP's bearer — and OAuth 2.1 resource-server discovery is
served under `/.well-known/`. The tools are read-only because the relay holds
no member's signing key; posting as the relay would put a person's words under
the relay's identity. On the client side, an operator connects external MCP
servers through `GET /buzz/api/mcp/connectors` and its authorize/call routes,
and agents reach the same connectors as tools.

**ACP** (v1) at `POST /acp`: editor-to-agent JSON-RPC, authenticated with the
same bearer an MCP client uses. `session/prompt` drives the same turn pipeline
a mention does, so an editor gets the persona rather than a reimplementation
of one. An editor that speaks only stdio needs a stdio-to-HTTP bridge in front of the
deployment.

Both protocols are composed from Ferrite source packages (`mcp` and `acp`)
because the official Rust SDKs cannot run under the one-dependency rule.

## Operating a community

Deployment owners seed the operator identities and relay credentials. An
authorized operator can manage the allowlisted workspace configuration and
upload a new browser bundle through chunked, hash-checked asset routes.
Secret-valued settings are write-only: reads report presence and a fingerprint,
not the value.

When diagnosing a workspace, check the route that performs the customer action,
not only gateway health. Record the deployment generation with browser results
so an older successful run is not mistaken for the current service.

## Current limits

- Profile discovery requires a complete paginated or roster-scoped path before
  large communities can rely on complete identity visibility.
- Deletion, moderation, add-member, archive refusal, invites, presence, typing,
  read state, search, and deep-history pagination do not all have equivalent
  browser-journey coverage.
- The relay implements the Nostr operations Buzz uses, not every NIP.
  Unsupported `COUNT` and server-side search are refused.
- Forge control state exists, but git object hosting is deferred.
- Voice media, push notifications, relay mesh, and server-side key generation
  are not available.

## Validation

```bash
(cd apps/buzz && cargo test --locked --no-fail-fast)
cargo clippy -p buzz --all-targets
(cd frontends/buzz-web && npx playwright test e2e/company-day.spec.ts --reporter=list)
```

The browser journey needs a deployed relay (`BUZZ_ORIGIN`) and provider keys.
