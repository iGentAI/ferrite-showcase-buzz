# The thin desktop shell

`buzz-desktop` is a Tauri 2 shell around the SAME `buzz-web` bundle a browser
tab runs. It implements only the command families the browser seam cannot
answer; everything else keeps flowing through that seam, unchanged.

That is the whole design, and it is what keeps the shell small enough to
maintain against an upstream that lands ~790 commits a month:

| the shell answers | the seam still answers |
|---|---|
| local git against a working copy | relay I/O: events, queries, subscriptions |
| the OS keychain and a git credential helper | identity, signing, NIP-98 |
| opening a terminal at a repository | channels, profiles, feed, presence, search |
| local ACP harnesses (Claude Code, Codex, Goose) | cloud agents on the workspace |
| REMOTE ACP agents over HTTP (`RemoteAcp`) | media, canvas, workflows, moderation |
| a fail-closed permission dialog for agent tool use | |
| opening a link in the user's real browser, and the dock badge | notifications, and everything else a tab does well |

Relay I/O needs **no native code**: the SPA's relay socket is a browser
`WebSocket`, and a browser `WebSocket` inside a Tauri webview is the same
object. Upstream's `relay.rs`, `native_websocket.rs` and `event_sync.rs` are
deliberately not vendored.

## Layout

```
buzz-desktop/
  core/                 buzz-desktop-core — all the logic, NO tauri dependency
  src-tauri/            the shell: #[tauri::command] wrappers over core
  native-commands.json  the registry (see below)
```

`core` has no Tauri dependency on purpose: the decisions worth testing — which
room a repository maps to, whether a push is safe, whether an agent may run a
tool — are then testable with `cargo test`, on any machine, with no webview and
no display. `src-tauri` is a wiring layer thin enough to read in one sitting.

It is its OWN cargo workspace, excluded from the root one: the root
workspace holds constrained Ferrite apps, which are admitted one runtime
dependency each. A desktop client is not one of those, and letting its
dependency tree into that workspace would blur the rule that keeps the apps
auditable.

## Two agent transports, one permission broker

A local harness is a process this shell owns and speaks to over pipes; a remote
agent is an HTTPS endpoint it calls (`RemoteAcp`). The protocol is the
same either way, and so is the answer to "may this agent run this tool?" — one
fail-closed broker serves both, because two implementations of that question
would be two chances to get it wrong.

The transports differ in one way that matters. On stdio a refusal can be written
to the agent's pipe at any moment, so the broker's own expiry sweep delivers it.
Over HTTP the binding is POST-only: an agent's request arrives inside a reply and
its answer has to ride a NEW post, so the remote driver watches the deadline
itself and sends the refusal when it passes. Both directions end the same way —
a request nobody answered is refused, on time — and neither leaves an agent
waiting on a dialog somebody walked away from.

## `native-commands.json` — the contract with the client

The dispatch contract: the shell injects
`window.__BUZZ_DESKTOP__ = { version, commands }` **before any bundle code
runs**, and the SPA's bridge routes exactly those commands to native IPC. This
file is that list, checked in, and:

* the shell asserts the compiled registry equals it (`registry::tests`), so the
  injected marker cannot drift from what the shell actually implements;
* `buzz-web`'s `commandCoverage.test.mjs` asserts no command is answered by
  BOTH the seam and the shell, and that every registered command is one the
  client genuinely invokes.

So a command can only ever have one owner, and a registry entry that implements
nothing fails a test rather than becoming a silent black hole.

With exactly one declared exception, which the same gate polices. A few answers
are truthfully FACE-DEPENDENT: `list_managed_agents` is `[]` in a tab, which
hosts no agent processes, and a real list here, which supervises them. The shell
declares such a command in the registry's `overrides`, and the gate then requires
that the override be registered AND genuinely override something the seam
answers — so a deliberate override stays distinguishable from an accidental
double answer, and a stale one fails a test.

## Identity, credentials, harnesses

**Your identity stays in the client.** The shell does not hold YOUR signing key:
the key is the one the SPA uses, so one person has one identity across the
browser and the desktop. That is why this shell cannot sign a pull request's
status event — see the section below.

**An agent signs as itself.** A local ACP adapter is not a buzz client: it speaks
ACP to whoever spawned it and has no relay connection, no key, and no idea a
workspace exists. So when it answers, the SHELL puts those words in the room —
under the AGENT's own key, held in the keychain as `agent:<harness>` and
generated once from the OS entropy a desktop has and a constrained app does not.
Not your key: those are not your words, and the key is not the shell's to use.
Not the relay's either, which holds no member's key.

That gives a local agent the same trust shape a cloud persona has — its own
pubkey, its own membership, its own moderation history — and it has one
consequence worth knowing before you wonder why an agent is silent:

> **A moderator has to add the agent to a room.** The shell can tell you the
> pubkey (`get_runtime_file_config`, and the agents list) but can never grant
> itself access. Until somebody adds it, the relay refuses the answer with
> `restricted: not a member of this channel`, and the shell reports exactly that
> rather than dropping it.

**Which workspace, though?** The shell serves the SPA from `tauri://localhost`,
so there is no page origin to derive a relay from. Name one with the
`BUZZ_WORKSPACE_ORIGIN` environment variable, or with a `workspace.json` in the
app data directory:

```json
{ "origin": "https://buzz.example" }
```

https only, except an exact loopback host for development. Until it is set,
posting is refused with a message naming both ways — an agent's answer is never
silently dropped.

An answer goes back into the THREAD that asked, not to the top of the channel:
this relay resolves a reply's ancestry from NIP-10's marked `e` tag alone, so a
bare tag would file the reply as a new conversation.

### What the client's agent dialog does here

The dialog is upstream's, unchanged, and it was written for upstream's desktop —
so the shell answers its calls in the client's own vocabulary rather than in its
own. Five mappings are worth knowing, because each one is a decision:

| the dialog sends | the shell does |
|---|---|
| a free-text **name** ("Claude Code") and no id | slugs it into a harness id (`claude-code`) and keeps the name as the label — `Harness::validate` accepts no spaces, so refusing would make the dialog unusable |
| the command as **`agentCommand`** or **`acpCommand`** | reads either, plus `args`/`agentArgs` and `env`/`envVars`; a field list read only one way is a field somebody filled in and nothing used |
| a **relay URL** | learns the workspace origin from it (`wss://host/buzz/` → `https://host`), create-if-absent, so an origin somebody set deliberately is never replaced |
| an update as **`{pubkey, …}`** with only what changed | resolves the agent by that pubkey and merges the named fields into what is stored — `{pubkey, model}` is an ordinary update, and reading it as a whole definition refused it |
| a **rename** | changes the label and NOT the id |

That last one is the important one. An agent's signing key is kept under its
harness id, so moving the id on a rename would hand the agent a **new identity**:
a new pubkey, no membership, and every message it had already posted orphaned
from the next. A rename changes the name the workspace sees, which is what a
person means by one — and the shell republishes the agent's profile so everybody
else sees it.

On create the shell also publishes that profile (without it a client renders a
truncated pubkey and its mention picker never offers the agent), honours
"spawn after create", and returns the agent's own `nsec` for the backup panel —
the one place a secret leaves this shell, and it goes to the person rather than
over a wire.

**Git credentials: yours are used if you have them.** Before refusing a push the
shell asks two questions, in order:

1. does its own keychain hold a token for this host (`git:<host>` under the
   `buzz-desktop` service)?
2. if not, can **git** supply one? `git credential fill` is asked with the whole
   clone URL, so a `credential.helper` you already use — `osxkeychain`,
   `manager`, `store`, your own script — answers, and a path-scoped credential
   matches.

So on a machine where `git push` works, pushing from here works, with nothing to
configure. And a token the keychain holds is actually HANDED to git, not merely
counted: every command that touches a remote runs with a credential helper
scoped to that remote's origin (`credential.https://<host>.helper`), added
beside your own configuration rather than replacing it. The scoping matters —
an unscoped helper answers for EVERY host, so a merge whose source named a
hostile remote would be handed the target's token — and the secret itself rides
an environment variable the helper reads back, never the command line a process
listing could show.

A transport that does not use a password at all is never gated on one:
every SSH spelling (`ssh://`, `git+ssh://`, `ssh+git://`, `git@host:owner/repo`),
`file://`, and a local path go straight through — an SSH key is not something
this app can inspect, and if it is missing, git's own error says so far better
than a guess would.

The one remote deliberately still refused is **Ferrite RCS** from a member's
machine, because its git password IS the platform account owner token. The
refusal names the alternative: a project room's process commands do the same
branch, merge and land work through the workspace, as you.

**Harnesses** are configured through the client's existing agent settings, which
reach `create_managed_agent` / `save_custom_harness`. A local one carries a
command, arguments and environment; a remote one carries an HTTPS endpoint and a
secret REFERENCE — the token itself lives in the keychain under `harness:<id>`,
never in the config file. Reserved variables (`BUZZ_RELAY_URL`, `BUZZ_AGENT_NSEC`
and the rest) are refused rather than silently dropped, because a setting the app
ignores is an hour of somebody's debugging.

## What this shell deliberately does NOT do

Native huddle audio, local mesh-LLM hosting and device pairing stay refused by
the seam, with the seam's own sentences. They are real work with no half
measure, and a shell that answered them badly would be worse than one that says
so.

Three PLUGIN families are deliberately left to the seam as well, and the reasons
are specific rather than general:

* **Notifications** keep the browser's `Notification` API, which a webview has.
  Routing them natively would take client changes — the bridge calls that API
  directly rather than invoking a command — and editing a vendored upstream file
  to gain a notification is the wrong trade (`docs/upstream-sync-playbook.md`).
* **The event system** is all-or-nothing: `listen`, `unlisten` AND `emit` must
  route together, or a same-page emit and a native listen would silently miss
  each other. This shell needs none of it — its permission dialog is native, and
  an agent's answers reach the client through the relay like everyone else's.
* **The updater** has no release manifest or signing key to check against yet
  (see "Shipping installers"), so the seam's refusal is the truthful answer and
  the client already renders its own card for it.

What IS wired is what a tab genuinely cannot do: `plugin:opener|open_url`,
because Tauri intercepts `window.open` and an external link would otherwise
silently do nothing, and the `plugin:window|*` badge and focus commands, because
a tab cannot badge a dock or raise itself.

## Building

```sh
cd frontends/buzz-desktop
cargo test -p buzz-desktop-core      # the logic, no system deps needed
cargo build -p buzz-desktop          # the shell (needs the Tauri system deps)
```

The shell's `frontendDist` points at `../buzz-web/dist`, so build the SPA first
(`cd ../buzz-web && npx --yes pnpm@10.31.0 install --frozen-lockfile && npx --yes pnpm@10.31.0 run build`;
buzz-web pins pnpm in its `packageManager`, and CI prepares this crate with the
same command). There is no second bundle configuration:
the desktop ships the same `dist/` the relay serves.

On Debian/Ubuntu the shell needs:

```sh
sudo apt-get install -y libwebkit2gtk-4.1-dev libgtk-3-dev librsvg2-dev \
  libayatana-appindicator3-dev patchelf
```

## Shipping installers

A dedicated `buzz-desktop` workflow that runs the core logic on macOS, Windows
and Linux on every push, runs the client contracts, and bundles installers for
all three platforms on a tag or on demand (`.deb` and `.AppImage`, a `.dmg`,
and an NSIS `.exe`) is intended but not yet in this repository. This repository has no CI
workflow yet.

The icons are the PRODUCT's own published set (`icon.icns`, `icon.ico` and the
PNG sizes), taken from buzz's `desktop/src-tauri/icons` at the tracked upstream
pin. They are artwork rather than code: there is no behaviour to drift, and the
shell already shipped buzz's app icon — this is the same artwork at the
resolutions a `.dmg` and an `.msi` require.

What is deliberately NOT wired yet, and what each one needs:

| gap | what it needs |
|---|---|
| signing and notarization | an Apple Developer identity plus `APPLE_*` secrets, and a Windows signing certificate. Bundles built without them are unsigned, so they install with a warning on macOS and Windows — which is why none is published as a release. |
| the in-app updater | a published release manifest and its signing key (`TAURI_SIGNING_PRIVATE_KEY`). The client already routes `plugin:updater|check` through the bridge, and the seam refuses it with a sentence the UI renders — so an unsigned build degrades honestly rather than pretending it can update. |

Neither of these changes the shell's behaviour; the list is here rather than in a
plan so it stays with the thing it describes.

## What this shell cannot do, and says so

One case is worth stating up front because it looks like a bug and is not: this
shell **cannot sign a pull request's merged-status event**. The merge itself is
real — a git merge and a push — but the identity that would sign the status event
lives in the client (the same key the browser build uses, so one person has one
key across both faces), and a shell that holds no signing key cannot sign. The
merge therefore reports success with
`project::STATUS_NOT_SIGNED` attached, and the status is published from the pull
request view. Upstream's desktop signs here because it owns the keystore; this
one deliberately does not.


## Status

Desktop shell for the buzz client: a Tauri 2 wrapper around the same
`buzz-web` bundle a browser tab runs, adding only the command families the
browser seam cannot answer. Its own Cargo workspace, excluded from the labs
workspace by design.

## Ferrite Platform Shape

Local/operator face. The shell is a client of deployed surfaces — relay,
workspace, agents — and holds only client-side authority (working copies,
the OS keychain, local ACP harnesses, a fail-closed permission broker). It
is not a constrained app, not a sibling service, and adds no server-side
path.

## Not Claimed

- Nothing here is a deployed Ferrite component; the shell ships as an
  installer, not an app spec.
- No bypass of the browser seam: relay I/O, identity, and signing flow
  through the same client code a tab runs ("What this shell deliberately
  does NOT do").
- The shell cannot sign a pull request's merged-status event and says so
  (`project::STATUS_NOT_SIGNED`); that is a design boundary, not a bug.

## Validation

```bash
# from the repository root
cargo build --manifest-path frontends/buzz-desktop/Cargo.toml
```

See "Building" and "Shipping installers" for the full desktop build and the
`native-commands.json` contract check against the client bundle.
