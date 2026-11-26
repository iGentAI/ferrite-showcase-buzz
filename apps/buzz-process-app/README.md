# buzz-process-app — the buzz process cell

Branch/PR process rooms and member-authorized RCS lifecycle verbs, as ONE
constrained Ferrite app deployed in two provider-isolated profiles. The two
cells hold DIFFERENT credentials: the native cell holds the platform account
owner token and polls with no ingress at all; the GitHub cell holds a webhook
secret and a provider token and declares the one inbound route. Each cell has
its own app-scoped state so one cell's cursors can never be read or clobbered
by the other.

## Deploy specs

The two cells deploy separately as single applications, one spec per profile:

- [`ferrite.native.json`](./ferrite.native.json) — the `buzz-process-native`
  app: owner-token holder, its own state store, control/buzz egress, no route.
- [`ferrite.github.json`](./ferrite.github.json) — the `buzz-process-github`
  app: webhook/provider-token holder, its own state/requests/replies, buzz/
  github egress, and the one `/process/github/hook` route.

The released `ferrite` CLI reads a spec named `ferrite.json`, so deploy one
profile at a time from a working copy in which that profile's file has been
copied to `ferrite.json`, and set the `buzz-process-profile` secret to match
(`native` or `github`):

```bash
cp ferrite.native.json ferrite.json   # or ferrite.github.json
ferrite check
ferrite secrets set buzz-process-profile
ferrite deploy
```

Neither profile has been deployed this way; treat the commands as the
intended path.

## Status

Source-deployable constrained app crate. It is not yet the subject of a
recorded live deployment run; treat it as gate-proven, not live-proven.

## Ferrite Platform Shape

A source-deployed app: one crate, exactly one runtime dependency
(`ferrite-app`, the SDK crate from the public SDK checkout beside this
repository, by path), `#![forbid(unsafe_code)]`, effects
only through injected plane handles (store, queues) and declared scoped
egress. The cryptography under `src/crypto/` and `src/b64.rs` is vendored
source from buzz (`buzz/src/crypto/` and `buzz/src/relay/b64.rs`, verbatim
but for one mechanical path rewrite),
held byte-identical by `tests/vendor_drift.rs` — the one-dependency rule
admits no second crate, and two hand-maintained copies of a signing primitive
would be two chances to disagree about a signature.

## Not Claimed

- No live deployment claim: no recorded run has deployed either cell to a
  fleet and read back durable state.
- The native cell claims no ingress surface at all; nothing on the internet
  can reach it, and the only thing it consumes is its own timer.
- Neither cell can reach the other's credentials or state; do not add a
  shared binding.
- Not a second implementation of RCS: lifecycle verbs go through the
  platform's surfaces, and git transport authority is the platform's,
  not something this app works around.

## Validation

The crate is a member of this repository's root workspace and links only the
SDK crate from the public SDK checkout beside it, so plain cargo runs the
suite from this directory:

```bash
cargo test --locked --no-fail-fast
```

`vendor_drift.rs` proves the vendored
crypto is byte-identical to its `buzz` source; `binding_fixtures.rs` proves
the binding-v1 fixture corpus (valid and invalid) is judged correctly.
