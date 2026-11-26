# Upstream relay contract — pin and disposition ledger

The relay-side half of upstream tracking (playbook §4, phase 2): which
`block/buzz` behavioural changes each sync ported, registered, or declined —
and the vendored contract snapshot the next diff runs against.

`upstream-contract/kind.rs` is upstream's `crates/buzz-core/src/kind.rs` at the
pin below, copied verbatim and never compiled (it lives outside `src/` and is a
text anchor, not code). The next sync's kind-table diff is two files on disk:

```sh
diff <(grep -oE 'pub const KIND_[A-Z0-9_]+: u32 = [0-9]+' buzz/upstream-contract/kind.rs) \
     <(grep -oE 'pub const KIND_[A-Z0-9_]+: u32 = [0-9]+' ~/buzz-upstream/crates/buzz-core/src/kind.rs)
```

No upstream history is required — clones here are depth-1 (playbook §2), so
this snapshot is what makes the next delta computable at all.

## Pin

| | |
|---|---|
| upstream commit | `4cd82f513214aad11c2b742ce7cc7c681e8e32a0` (sync 004, recovered 2026-09-20) |
| contract file | `crates/buzz-core/src/kind.rs` — 1,088 lines, 130 `KIND_*` constants (unchanged since `051c3a27`) |
| our registry after the port | 133 entries in `ALL_KINDS` (`buzz/src/relay/kind.rs`) |
| synced | 2026-09-20, recovered sync 004 (`813bbd14` → `4cd82f51`); 2026-09-11, sync 003 (`051c3a27` → `813bbd14`); 2026-09-10, sync 002 (`f86dfc58` → `051c3a27`); 2026-08-04, sync 001 (`070fb6a1` → `f86dfc58`) |

The registries are not expected to be equal in COUNT — ours registers kinds
upstream tracks elsewhere (e.g. push leases through their own module) and vice
versa; what the diff surfaces is *new constants*, which are then dispositioned
here. At this pin the name/value table diff is clean: every upstream constant
ours also defines carries the identical number.

## Sync 004 dispositions

The recovered frontend advances to `4cd82f51`. The upstream `kind.rs` at this
commit is byte-identical to the retained contract snapshot, so the kind table
and relay production source do not change.

Upstream #7562 adds GET and POST `/git/{owner}/{repo}/default-branch`. This
port does not host Git repository objects and deliberately declines both
routes. The wire fixture records both methods, and
`severe_every_artifact_path_is_driven_in_both_directions` sends authenticated
requests and requires the relay's exact route-fallthrough refusal. The existing
seven served routes and four declined routes remain covered.

The source recovery and its validation limits are recorded in sync entry 004
in `docs/upstream-sync-playbook.md`.

## Sync 003 dispositions

**Kind-table diff (`051c3a27` → `813bbd14`):** none — `kind.rs` is byte-identical
upstream, so the vendored snapshot needed no refresh. The window is four
commits; no CHANGELOG release was cut (it still heads at v0.5.23), so the
commits themselves are the enumeration.

| upstream change | bucket | disposition |
|---|---|---|
| #7532 fix(relay): reject presence updates when Redis storage fails | **port** | `ingress.rs`: the kind-20001 presence TTL key is now written BEFORE the heartbeat is admitted and driven onto the feed, and a failed write refuses the heartbeat with a retryable `error:` (`BuzzError::Storage`, "presence storage unavailable") instead of being discarded with `let _ =` after fan-out. Same producer contract as upstream: delivered live presence follows a successful mutation of the state `POST /buzz/api/presence` reads. Gates: `severe_presence_rate_gate` (heartbeat observable online, heartbeats outside the message budget) holds on the reordered path. **Not falsifiable here:** the refusal branch needs a store fault, and the fleet harness has no store fault injection — upstream proves theirs against a dead Redis. Recorded rather than papered over with a test of a helper. |
| #7552 fix(acp): integrate the Buzz Pi adapter fork | **N/A — product line** (Rust) + **mechanical** (client) | Their `buzz-acp` harness launching Pi through their fork (track 3, never synced). The client half — harness copy now names `buzz-pi-acp`, the session transcript reads `_meta.systemPrompt` in both shapes — came down with `--apply`. |
| #7550 fix(desktop): inbox title and author names truncate under narrow panes | **mechanical** | Client-only; re-copied. `MessageActionBar.tsx` was both-touched (upstream added a measurable `ref`); upstream's content taken and our `!message.pending` reply gate re-applied. |
| #7501 fix(markdown): align mention chip wrapping | **mechanical** | Client-only CSS/markup; re-copied. |

## Sync 002 dispositions

**Kind-table diff (`f86dfc58` → `051c3a27`):** exactly two new constants, both
ported. Everything else in `kind.rs` upstream is unchanged at the name/value
level (the file grew by 18 lines: the two constants, their doc comments, the
`AUTHOR_ONLY_KINDS` reformat and one compile assert).

| upstream change | bucket | disposition |
|---|---|---|
| `KIND_PRIVATE_MANAGED_AGENT = 30179` (NIP-PMA; #4593 defines the wire protocol, #5133 accepts it at ingest as `AUTHOR_ONLY`, global-only, `UsersWrite`; #6822 excludes its ciphertext from FTS) | **port** | `kind.rs`: registered, in `AUTHOR_ONLY_KINDS`, in `is_global_only_kind`; compile-asserted parameterized-replaceable, global-only, not relay-only, not shared-gated. Before this an unregistered 30179 was ACCEPTED here as an ordinary world-readable parameterized-replaceable event — the owner-encrypted content was safe but the outer tags (agent coordinate, generation, active/deleted) were not author-scoped. Search never indexed it (`is_searchable_kind` is a whitelist). Gate: `severe_access_gate::severe_a_private_managed_agent_aggregate_stays_with_its_owner` — owner reads it back, a non-owner gets neither the global scan nor a by-id read. What is NOT ported: the aggregate's CAS generation/predecessor semantics — upstream's own codec says relays must not act on them until the dedicated transactions exist, and nothing this deployment ships publishes a 30179 (the desktop's managed-agent store does; the browser client and the desktop shell here do not). |
| `KIND_HUDDLE_LIVENESS = 48104` (relay-synthesized on REQ from audio-room / mesh lease state, signed by the relay, never stored; `handlers/req.rs`) | **port** (refusal half) + **register** (mint half) | `kind.rs`: registered as **relay-only** (`is_relay_only_kind`), channel-scoped like 48100–48103, regular. Before this a client could SUBMIT a 48104 and it was stored as an ordinary channel event — a forged "this call is live" every synced client's `huddlePresenceRuntime` subscribes to. Gate: `severe_inert_verbs_gate::severe_huddle_liveness_cannot_be_forged_by_a_client` — refused 403 naming the kind, nothing stored, 48100 still lands, a REQ for 48104 answers empty. **Register row:** this relay mints NO 48104. Upstream's liveness attests an audio room its relay/mesh is mixing; this port has no audio rooms (huddle audio is the desktop shell's, and the browser build gates every huddle control on `isTauri()`), so the truthful answer to a liveness REQ is the empty one. Upstream additionally CLOSEs a liveness REQ lacking an authorized `#h` (`restricted: huddle liveness requires an authorized #h channel`); ours answers EOSE with nothing, which the client treats identically (no live session). Converge if huddle audio ever runs on this relay. |

**Curated changelog rows (v0.5.5 → v0.5.23, the phase-2 `awk`/`grep`
extraction; upstream's CHANGELOG repeats the v0.5.22 block under v0.5.23, so
each row is listed once):**

| upstream change | bucket | disposition |
|---|---|---|
| #5133 accept kind:30179 at ingest; #6822 exclude 30179 ciphertext from FTS | **port** | above |
| #7285 refactor(relay): extract NIP-29 membership authorization | **N/A — infra** | A code motion inside their handlers; the authorization RULES are unchanged and ours are pinned by `severe_membership_gate`, `severe_join_role_gate`, `severe_nip29_*`. |
| #6961 fix(relay): reject a frame on its own acknowledgement channel | **N/A — infra** | Their WebSocket frame/ack plumbing; this port's socket face has no acknowledgement channel construct. |
| #7258 early startup lifecycle logs; #7149 detailed readiness metrics | **N/A — infra** | Their process's `/_readiness`; ours is the platform's app lifecycle and the operator metrics tier (`severe_operator_metrics_gate`). The root alias `/_readiness` stays as-is (`severe_upstream_wire_gate`). |
| #7278, #7268, #7254, #7214 docs(nip-fi); #7221 drop Phase-A NIP-FI ledger; #6994 NIP-FI identity/final-admission schema; #6776 feat(auth) NIP-FI canonical assertion verifier | **N/A — product line** | NIP-FI is upstream's hosted-tenant identity/final-admission scheme (Builderlab accounts, deny-until-TTL, Blossom/Git credential exemptions). This deployment's identity is the tab's own key plus the operator's invite/membership planes; there is no hosted account to assert. Same reasoning as sync 001's NIP-OA row. |
| #6987 extract domain stores; #6782 split channel membership store; #6777 finish replaceable event store extraction; #6819 canonical roster fixtures; #6229 configurable writer session timeouts; #6898 disable heartbeat vacuum truncation | **N/A — infra** | Their Postgres runtime. This port rides the platform's Store/queue planes with one dependency. |
| #3777 refactor(relay): NIP-98 admin auth with Operator/Moderator roles and NIP-11 discovery | **N/A — already true** | The operator/moderator split over NIP-98 is this relay's existing authority model (operator plane, `buzz-community-moderators`, `POST /buzz/api/members/role` operator-only), and NIP-11 is served from the enforcing constants. |
| #6728 docs(security): route reports through private advisories | **N/A — infra** | Their repository policy. |
| #6058 fix(media): accept portrait video resolutions | **N/A — product line** | Their media pipeline decodes video dimensions; ours proves a declared type against the bytes' magic number and stores the object content-addressed — no resolution check exists to fix. |
| #4610 fix(media): require authenticated reads | **register** | Ours is deliberately PUBLIC by digest: `/buzz/blob/` is the gateway's content route and the 256-bit content hash IS the capability (README, "The digest is the capability"). Keep: the gateway derives `Content-Type` from the key and the relay never proxies bytes, so authenticated reads would put the relay back in the byte path. Re-examine if channel-scoped media is ever required. |
| #5294 fix(relay): stop panicking the ingest worker on reactions to project events | **N/A — already true** | Reactions here are gated on the channel the TARGET inherits (`severe_reaction_gate`, `severe_ws_gate`); a reaction to a global-only kind (a project) resolves to no channel and is stored globally, no panic path exists. |
| #5291 fix(relay): log event kind on the HTTP bridge `/events` line | **N/A — infra** | Logging. |
| #5480 fix(search): surface exact short profile names | **N/A — product line** | Upstream's fix is in its Postgres FTS ranking. Profile search here is a name match the browser seam performs over kind-0 events read from the relay (`browser-backend/profileSearch.ts` + `search_users` in `commands.ts`, pinned by `profileSearch.test.mjs`); there is no FTS ranking step to short-circuit. |
| #5404 ci(security): allow retired relay pool advisory | **N/A — infra** | Their `cargo-deny` allowlist. |
| #4607 fix(workflow): bind trigger author to the signed event | **N/A — already true** | A trigger here IS the admitted signed event: `workflows.rs` matches `trigger.matches(&event.content)` on the event ingest handed it, refuses the relay's own events as triggers, and the run is keyed by that event's id (`run_id(&id, &trigger_hex)`) — no caller-supplied author field exists to bind (`severe_workflow_gate`). |
| #4647 perf(relay): index channel-id lookups | **N/A — infra** | Postgres index. |
| #4632 docs(nip-am): cache SHOULD/MUST + pricingIdentity + consumer cost guidance | **N/A — product line** | NIP-AM turn metrics here are emitted by our own agent runtime (`agents::turns`), and the accounting ladder that consumes them is the desktop's local archive (`get_agent_usage_series`, refused in the seam with that reason). |

## Sync 001 dispositions

Every relay-relevant row from upstream's curated changelog window
(v0.4.26 → v0.5.4, the phase-2 `awk` extraction), one bucket each (playbook §6).
An absent row means the filter did not match it; client-side rows (the other
~180) are track 1's business and are accounted in the playbook §8 log.

| upstream change | bucket | disposition |
|---|---|---|
| #3358 — kind 30178 team-catalog reads gated behind the `shared` tag | **port** | `kind.rs`: `KIND_TEAM_CATALOG` registered, `SHARED_GATED_KINDS`/`is_shared_gated` generalize the persona gate exactly as upstream did; `access.rs`: `is_unshared_gated` is the per-event half, `may_deliver` gates on it; ingest refuses a non-canonical `shared` spelling on any shared-gated kind (`check_shared_tag_contract`). Pinned by compile asserts, registry tests, and an `access.rs` unit twin of the persona test. |
| #3171 — kind 30621 multi-repo projects accepted at ingest | **port** | `KIND_PROJECT` registered: parameterized-replaceable by range (NIP-33 replacement is generic here), global-only with the rest of the git family it groups. |
| #3783 — presence heartbeat slowed (client 60 s, expiry three windows) | **port** | `PRESENCE_TTL_MS` 90 s → 180 s. The contract is three heartbeat windows; at the new cadence the old TTL meant ONE delayed beat flickered a present member offline. |
| #2871 — search operators `from:` / `after:` / `before:` passed in the filter | **port** | `search::SearchScope`, honoured where each bound is cheapest (`until` = start position on the newest-first index, `since` = walk stop that makes the in-range answer complete, `authors` = body-read filter). Parsed and VALIDATED at `POST /buzz/api/search` — a present-but-malformed field is a 400 naming the field, never an unbounded search. Forwarded by the browser seam (`browser-backend/search.ts`). Gated by `severe_search_operators_narrow_the_answer`. |
| #3635 — NIP-11 `max_limit` aligned with the REQ ceiling | **N/A — already true** | Our `relay-info` reads every advertised limit from the `ws` constants that enforce it, by construction. |
| #3998 — open relays may set the NIP-11 workspace icon (kind 9033) | **register** | Our 9033 is INERT by register (strategy doc §6.2 #11): accepted, stored, applied by nothing. The workspace icon here is an operator-plane concern, not a client event. If 9033 is ever applied, follow upstream's rule that an open relay's admin may set it. |
| #4203 — NIP-OA accepts the raw Nostr tag form in `parse_json_array` | **N/A — product line** | No NIP-OA verifier exists here by design: an agent's owner is RECORDED at creation behind the ownership claim, which is stronger than re-verifying a profile's assertion. |
| #4124 — read-replica membership checks; RUSTSEC dependency bumps | **N/A — infra** | Their Postgres read-replica and their dependency tree. This port has one dependency and no replicas — the store cluster is the platform's concern. |
| feat(acp) ×3, feat(agent) ×4, mesh/mobile/discovery/release fixes | **N/A — product line** | Our ACP, MCP and agent runtime are native (playbook §1, track 3): upstream's harness processes and ACP crates are a different product riding the same protocol. Their protocol-level conformance is covered by our own suites (27/27 ACP, MCP CORE 34/0). |

## Ours-only kind block — 47000–47099

This port reserves 47000–47099 for kinds THIS PORT defines and upstream has never
seen. Verified unclaimed against the vendored `upstream-contract/kind.rs` at the pin above
(re-verified at sync 002: upstream's additions were 30179 and 48104, neither in the block). The
next sync's kind-table diff must disposition any 47xxx row on OUR side as ours-only rather than
upstream drift — and if upstream ever claims a 47xxx number, reallocate ours BEFORE applying the
sync and update this section plus `kind.rs`.

| kind | constant | classification |
|---|---|---|
| 47001 | `KIND_PROCESS_COMMAND` | client-signed, `h`-scoped, regular — a member's request that the process cell perform an RCS lifecycle verb; the event id is the idempotency key |
| 47002 | `KIND_PROCESS_RESULT` | client-signed by the process cell's enrolled identity, `h`-scoped, regular — **authorship-gated at ingest** to the pubkeys in the `buzz-process-cells` operator document, failing closed when none is registered; deliberately NOT relay-only (the cell is a client, the relay never signs results) |

## How to refresh this file

At every sync (playbook phase 2):

1. Run the kind-table diff above; disposition every NEW constant into the table.
2. Run the changelog extraction; disposition every matching row.
3. `cp ~/buzz-upstream/crates/buzz-core/src/kind.rs buzz/upstream-contract/kind.rs`
4. Update the pin table, implement the `port` rows, and run
   `cargo test -p buzz --no-fail-fast`.
