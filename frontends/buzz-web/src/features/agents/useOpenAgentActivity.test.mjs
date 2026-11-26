import assert from "node:assert/strict";
import { describe, it } from "node:test";

import {
  decideExplicitActivityOpen,
  isChannelOpenable,
  isReferenceLookupSettled,
  resolveOpenableActivityChannelId,
} from "./useOpenAgentActivity.ts";

// A click on a "Working in #channel" badge during cold start used to warn
// "you haven't joined" for a channel the viewer is in, because the channel
// map is empty until the member list loads. The open now waits for the
// channels (and the per-id reference lookup) instead.
describe("decideExplicitActivityOpen", () => {
  const member = { isMember: true, visibility: "private" };
  const privateNonMember = { isMember: false, visibility: "private" };

  it("waits while the channel list is still loading", () => {
    assert.equal(
      decideExplicitActivityOpen({
        channel: undefined,
        areChannelsReady: false,
        isReferencePending: true,
      }),
      "wait",
    );
    assert.equal(
      decideExplicitActivityOpen({
        channel: undefined,
        areChannelsReady: false,
        isReferencePending: false,
      }),
      "wait",
    );
  });

  it("waits for an unknown channel's reference lookup after the list loads", () => {
    assert.equal(
      decideExplicitActivityOpen({
        channel: undefined,
        areChannelsReady: true,
        isReferencePending: true,
      }),
      "wait",
    );
  });

  it("opens a channel the viewer can open, even before the list settles", () => {
    assert.equal(
      decideExplicitActivityOpen({
        channel: member,
        areChannelsReady: false,
        isReferencePending: true,
      }),
      "open",
    );
  });

  it("reports inaccessible only once the answer is known", () => {
    assert.equal(
      decideExplicitActivityOpen({
        channel: undefined,
        areChannelsReady: true,
        isReferencePending: false,
      }),
      "inaccessible",
    );
    assert.equal(
      decideExplicitActivityOpen({
        channel: privateNonMember,
        areChannelsReady: true,
        isReferencePending: true,
      }),
      "inaccessible",
    );
  });
});

describe("isReferenceLookupSettled", () => {
  it("is false before the lookup starts or while it runs", () => {
    assert.equal(isReferenceLookupSettled(undefined), false);
    assert.equal(
      isReferenceLookupSettled({ status: "pending", fetchStatus: "idle" }),
      false,
    );
    assert.equal(
      isReferenceLookupSettled({ status: "pending", fetchStatus: "fetching" }),
      false,
    );
    assert.equal(
      isReferenceLookupSettled({ status: "success", fetchStatus: "fetching" }),
      false,
    );
  });

  it("is true for a result, a cached miss, or an error", () => {
    assert.equal(
      isReferenceLookupSettled({ status: "success", fetchStatus: "idle" }),
      true,
    );
    assert.equal(
      isReferenceLookupSettled({ status: "error", fetchStatus: "idle" }),
      true,
    );
  });
});

describe("isChannelOpenable", () => {
  it("allows joined channels regardless of visibility", () => {
    assert.equal(
      isChannelOpenable({ isMember: true, visibility: "private" }),
      true,
    );
    assert.equal(
      isChannelOpenable({ isMember: true, visibility: "open" }),
      true,
    );
  });

  it("allows open channels the viewer hasn't joined (read-only)", () => {
    assert.equal(
      isChannelOpenable({ isMember: false, visibility: "open" }),
      true,
    );
  });

  it("rejects private channels the viewer hasn't joined", () => {
    assert.equal(
      isChannelOpenable({ isMember: false, visibility: "private" }),
      false,
    );
  });

  it("rejects channels missing from the viewer's channel list", () => {
    assert.equal(isChannelOpenable(undefined), false);
  });
});

describe("resolveOpenableActivityChannelId", () => {
  it("prefers the first openable working channel", () => {
    assert.equal(
      resolveOpenableActivityChannelId({
        agentChannelIds: ["member-1"],
        openableChannelIds: new Set(["working-2", "member-1"]),
        workingChannelIds: ["working-1", "working-2"],
      }),
      "working-2",
    );
  });

  it("falls back to the agent's first openable member channel", () => {
    assert.equal(
      resolveOpenableActivityChannelId({
        agentChannelIds: ["hidden-2", "member-1"],
        openableChannelIds: new Set(["member-1"]),
        workingChannelIds: ["hidden-1"],
      }),
      "member-1",
    );
  });

  it("returns null when the agent is only active in inaccessible rooms", () => {
    assert.equal(
      resolveOpenableActivityChannelId({
        agentChannelIds: ["hidden-2"],
        openableChannelIds: new Set(["unrelated"]),
        workingChannelIds: ["hidden-1"],
      }),
      null,
    );
  });

  it("returns null with no candidate channels at all", () => {
    assert.equal(
      resolveOpenableActivityChannelId({
        agentChannelIds: [],
        openableChannelIds: new Set(),
        workingChannelIds: [],
      }),
      null,
    );
  });
});
