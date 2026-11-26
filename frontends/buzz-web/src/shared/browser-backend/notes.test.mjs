// `publish_note` signs what the Pulse composer hands it: the mentions become
// `p` tags and the attachment metadata `imeta` tags. The browser seam used to
// read only `content` and `replyTo`, so a mention notified nobody (and never
// woke a mentioned relay agent) and attachments lost their metadata, while the
// publish still reported success.
import assert from "node:assert/strict";
import test from "node:test";

import { publishNote } from "./notes.ts";
import { getLikedNotes } from "../api/social.ts";
import { relayClient } from "../api/relayClient.ts";

const SELF = "a".repeat(64);
const ALICE = "b".repeat(64);
const BOB = "c".repeat(64);
const PARENT = "1".repeat(64);

class Signed extends Error {}

/**
 * An identity that records the event it is asked to sign and then stops the
 * command, so the test reads the tags without a relay to publish to.
 */
function recordingIdentity() {
  const signed = [];
  return {
    signed,
    identity: {
      pubkey: async () => SELF,
      sign: async (input) => {
        signed.push(input);
        throw new Signed();
      },
    },
  };
}

async function tagsFor(args) {
  const { signed, identity } = recordingIdentity();
  await assert.rejects(publishNote(identity, args), Signed);
  assert.equal(signed.length, 1);
  assert.equal(signed[0].kind, 1);
  return signed[0].tags;
}

test("mentions become p tags, normalised and without the author", async () => {
  const tags = await tagsFor({
    content: "hello @alice @bob",
    replyTo: PARENT,
    mentionPubkeys: [ALICE, BOB.toUpperCase(), ALICE, SELF],
    mediaTags: null,
  });
  assert.deepEqual(tags, [
    ["e", PARENT, "", "reply"],
    ["p", ALICE],
    ["p", BOB],
  ]);
});

test("attachment imeta tags are carried onto the note", async () => {
  const imeta = [
    "imeta",
    "url https://relay.example/media/abc",
    "m application/pdf",
    "filename report.pdf",
  ];
  const tags = await tagsFor({
    content: "see attached",
    replyTo: null,
    mentionPubkeys: [ALICE],
    mediaTags: [imeta],
  });
  assert.deepEqual(tags, [["p", ALICE], imeta]);
});

test("a non-imeta tag on the media argument and a malformed pubkey are refused", async () => {
  const { signed, identity } = recordingIdentity();
  await assert.rejects(
    publishNote(identity, { content: "x", mediaTags: [["p", ALICE]] }),
    /publish_note: only "imeta" tags/,
  );
  await assert.rejects(
    publishNote(identity, { content: "x", mentionPubkeys: ["alice"] }),
    /not a hex pubkey/,
  );
  assert.equal(signed.length, 0, "nothing was signed");
});

test("a note with neither mentions nor media carries only its reply tag", async () => {
  assert.deepEqual(await tagsFor({ content: "plain" }), []);
  assert.deepEqual(await tagsFor({ content: "reply", replyTo: PARENT }), [
    ["e", PARENT, "", "reply"],
  ]);
});

test("get_liked_notes resolves reactions authored by authorPubkey", async () => {
  const originalWindow = globalThis.window;
  const originalFetchEvents = relayClient.fetchEvents;
  globalThis.window = {};
  const NOTE_ID = "d".repeat(64);
  const queries = [];
  relayClient.fetchEvents = async (filter) => {
    queries.push(filter);
    if (filter.kinds?.includes(7)) {
      return [
        {
          id: "e".repeat(64),
          pubkey: ALICE,
          kind: 7,
          created_at: 1_700_000_010,
          content: "+",
          tags: [["e", NOTE_ID]],
          sig: "0".repeat(128),
        },
      ];
    }
    if (filter.kinds?.includes(1)) {
      return [
        {
          id: NOTE_ID,
          pubkey: BOB,
          kind: 1,
          created_at: 1_700_000_000,
          content: "liked note body",
          tags: [],
          sig: "0".repeat(128),
        },
      ];
    }
    return [];
  };
  try {
    const result = await getLikedNotes(ALICE, 10);
    assert.equal(result.notes.length, 1);
    assert.equal(result.notes[0].id, NOTE_ID);
    assert.equal(result.notes[0].content, "liked note body");
    assert.deepEqual(queries[0].authors, [ALICE]);
  } finally {
    if (originalWindow === undefined) delete globalThis.window;
    else globalThis.window = originalWindow;
    relayClient.fetchEvents = originalFetchEvents;
  }
});
