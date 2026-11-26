# The browser backend

buzz's desktop client talks to a Rust process over Tauri's IPC. This directory is
the part of that process a browser tab can honestly stand in for, and it sits
behind exactly one caller: `invoke()` in
[`../lib/desktop-bridge.ts`](../lib/desktop-bridge.ts).

- **`websocket.ts`** — the relay transport. buzz asks Rust to open the socket
  (`plugin:websocket|connect|send|disconnect`) and reads frames off an IPC
  channel; this serves those four commands with a real browser `WebSocket`,
  emitting the exact frame shapes the client already parses. Every line of
  subscription, reconnect, replay and backoff logic above it is buzz's own,
  unchanged.
- **`commands.ts`** — relay config, identity, signing (NIP-42 auth, event
  signing, NIP-44 self-encryption), profile publishing, keeping the machine
  awake via the Screen Wake Lock API, and the handful of boot-path questions a
  tab can answer with an empty value. It also owns
  `unavailableCommandMessage`, the sentence the UI shows when a command is
  genuinely desktop-only.
- **`identity.ts`** — the ONE key this tab signs with. The Tauri-command path
  here and the NIP-98 header path in `../lib/nostr-signer.ts` must resolve to
  the same identity — when they did not, a room created over HTTP was owned by
  a key the socket had never authenticated as. A NIP-07 extension, when
  present, owns the identity outright; otherwise the key persists in
  `localStorage` so a reload keeps the same person.
- **`channels.ts`** — the channel WRITE path. Creating a room is the one
  channel operation that is not a Nostr event: the relay owns creation on its
  HTTP face and then publishes the 39000/39002 the read path sees. Shared
  starter rooms derive a pinned slug so a taken id means "join, don't
  duplicate"; per-user room ids are minted by the relay.
- **`channelAdmin.ts`** — the commands that CHANGE a room: add/role-change/
  remove members (NIP-29 kind 9000/9001), archive/unarchive (a 9002 metadata
  edit carrying only `archived`), and message deletion (NIP-09 kind 5). Nothing
  here decides who may do what — the relay does, and its refusal sentence is
  what the caller shows, per pubkey where the relay answers per event.
- **`messageSend.ts`** — `send_channel_message` and `get_thread_replies`,
  rebuilt without the desktop's local cache: the write is one signed event on
  the app's own socket (the relay validates both faces through the same
  `accept_event`), the read is a `#e` REQ on the thread root filtered by
  NIP-10 markers.
- **`canvas.ts`** — `get_canvas` and `set_canvas`. A canvas is the newest
  kind-40100 event in the room; every save is a whole-document supersede and
  history stays on the relay.
- **`profileSearch.ts`** — the budgeted, resumable paged walk behind
  `search_users`. The relay refuses NIP-50 and caps a page at 500 events, so
  matching happens client-side over the kind-0 index, newest profile first,
  with an honest `next_cursor` instead of a silently truncated single page.

The seam has grown past that original set. `agentAdmin.ts`, `backup.ts`,
`catalogs.ts`, `channelDirectory.ts`, `channelsPayload.ts`, `cloudAgents.ts`,
`identityArchive.ts`, `media.ts`, `notes.ts`, `profileUpdate.ts`, `reads.ts`,
`search.ts`, `workflows.ts` and `workspace.ts` answer further command families
in the same shape, one module per family.

Modules keep their unit tests beside them (`*.test.mjs`); from the `buzz-web`
root they run with plain `node --test` through the repo's resolver hook:

```bash
node --import ./test-loader.mjs --test "src/**/*.test.mjs"
```

Anything not answered here is a native capability — a local agent harness, git,
the huddle audio pipeline, local model compute, the OS keychain. Those reject
with a human sentence rather than a fake success, and buzz's own error surfaces
render it. Nothing here fakes a success.
