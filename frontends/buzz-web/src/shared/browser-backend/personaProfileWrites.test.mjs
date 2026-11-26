// The two writes in `reads.ts` that change something the user owns: "Save and
// publish" for a persona (F420) and the deferred avatar save (F421). Each is
// driven with an injected relay so no network is involved.
import test from "node:test";
import assert from "node:assert/strict";

import { updatePersonaAndPublish, updateProfileAtRelay } from "./reads.ts";

const ME = "a".repeat(64);

function fakeCreate() {
  const calls = [];
  const create = async (input) => {
    calls.push(input);
    return {
      id: input.agent ?? "new",
      display_name: input.displayName ?? "",
      runtime: input.runtime ?? "ferrite-cloud",
      model: input.model ?? null,
    };
  };
  return { calls, create };
}

test("save and publish refuses a per-thread session policy before any write, as save does", async () => {
  const { calls, create } = fakeCreate();
  await assert.rejects(
    updatePersonaAndPublish(
      { input: { id: "ada", displayName: "Ada", behavior: { sessionPolicy: "thread" } } },
      create,
    ),
    /one conversation per channel/,
  );
  assert.equal(calls.length, 0);
});

test("save and publish forwards the runtime the user chose", async () => {
  const { calls, create } = fakeCreate();
  const result = await updatePersonaAndPublish(
    {
      input: {
        id: "ada",
        displayName: "Ada",
        systemPrompt: "Help.",
        runtime: "custom-harness",
        model: "m1",
        namePool: [],
        behavior: { sessionPolicy: "channel" },
      },
    },
    create,
  );
  assert.deepEqual(calls, [
    {
      agent: "ada",
      displayName: "Ada",
      systemPrompt: "Help.",
      runtime: "custom-harness",
      model: "m1",
    },
  ]);
  assert.equal(result.publicationStatus, "published");
  assert.equal(result.relayMessage, undefined, "nothing unstored was filled");
});

test("save and publish names the filled fields the workspace does not store", async () => {
  const { create } = fakeCreate();
  const result = await updatePersonaAndPublish(
    {
      input: {
        id: "ada",
        displayName: "Ada",
        avatarUrl: "https://relay.example/ada.png",
        description: "A helper.",
        namePool: ["Ada", "Grace"],
        envVars: {},
      },
    },
    create,
  );
  assert.equal(result.publicationStatus, "published");
  assert.match(result.relayMessage, /avatar, description, name pool/);
  assert.doesNotMatch(result.relayMessage, /environment variables/);
});

function identity() {
  const signed = [];
  return {
    signed,
    pubkey: async () => ME,
    sign: async (event) => {
      signed.push(event);
      return { id: "e".repeat(64), pubkey: ME, ...event };
    },
  };
}

function relayWith(profile) {
  const published = [];
  return {
    published,
    events: async () => (profile ? [profile] : []),
    publish: async (event) => {
      published.push(event);
    },
  };
}

const currentProfile = (picture, tags = []) => ({
  id: "p".repeat(64),
  pubkey: ME,
  kind: 0,
  created_at: 100,
  content: JSON.stringify({ name: "ada", about: "hi", ...(picture ? { picture } : {}) }),
  tags,
});

test("a deferred avatar save is refused when the avatar changed since it was queued", async () => {
  const me = identity();
  const relay = relayWith(currentProfile("https://relay.example/newer.png"));
  await assert.rejects(
    updateProfileAtRelay(
      me,
      {
        expectedPubkey: ME,
        expectedAvatarUrl: null,
        avatarUrl: "https://relay.example/stale.png",
      },
      relay,
    ),
    /avatar changed/,
  );
  assert.equal(relay.published.length, 0, "the newer avatar is not reverted");
});

test("a deferred avatar save applies when the avatar is still the expected one, keeping the tags", async () => {
  const me = identity();
  const tags = [["i", "github:ada", "proof"]];
  const relay = relayWith(currentProfile("https://relay.example/old.png", tags));
  const profile = await updateProfileAtRelay(
    me,
    {
      expectedPubkey: ME,
      expectedAvatarUrl: "https://relay.example/old.png",
      avatarUrl: "https://relay.example/new.png",
    },
    relay,
  );
  assert.equal(profile.avatar_url, "https://relay.example/new.png");
  assert.equal(relay.published.length, 1);
  assert.deepEqual(me.signed[0].tags, tags, "the kind-0 tags survive the republish");
  assert.deepEqual(JSON.parse(me.signed[0].content), {
    name: "ada",
    about: "hi",
    picture: "https://relay.example/new.png",
  });
});

test("a retried avatar save whose first attempt already landed is not refused", async () => {
  const me = identity();
  const relay = relayWith(currentProfile("https://relay.example/new.png"));
  await updateProfileAtRelay(
    me,
    {
      expectedPubkey: ME,
      expectedAvatarUrl: null,
      avatarUrl: "https://relay.example/new.png",
    },
    relay,
  );
  assert.equal(relay.published.length, 1);
});
