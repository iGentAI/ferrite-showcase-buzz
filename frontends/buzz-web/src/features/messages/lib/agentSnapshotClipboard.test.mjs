import assert from "node:assert/strict";
import test from "node:test";

import {
  buildAgentSnapshotClipboardHtml,
  buildTeamSnapshotClipboardHtml,
  handleAgentSnapshotPaste,
  parseAgentSnapshotClipboardHtml,
} from "./agentSnapshotClipboard.ts";
import { beginRelayOriginFetch } from "@/shared/lib/mediaUrl";

// Snapshots live in the active relay's media store.
beginRelayOriginFetch()("https://relay.example");

const SHA256 = "a".repeat(64);
const URL = `https://relay.example/media/${SHA256}.png`;

function buildHtml(overrides = {}) {
  return buildAgentSnapshotClipboardHtml({
    attachment: {
      filename: "animation-auditor.agent.png",
      sha256: SHA256,
      size: 1234,
      type: "image/png",
      uploaded: 1,
      url: URL,
      ...overrides,
    },
    displayName: "Animation Auditor",
  });
}

test("copied agent HTML restores a labeled snapshot attachment", () => {
  assert.deepEqual(parseAgentSnapshotClipboardHtml(buildHtml()), {
    displayLabel: "Animation Auditor",
    filename: "animation-auditor.agent.png",
    sha256: SHA256,
    size: 1234,
    type: "image/png",
    uploaded: 0,
    url: URL,
  });
});

test("copied team HTML restores a labeled snapshot attachment", () => {
  const html = buildTeamSnapshotClipboardHtml({
    attachment: {
      filename: "design-review.team.png",
      sha256: SHA256,
      size: 4321,
      type: "image/png",
      uploaded: 1,
      url: URL,
    },
    displayName: "Design Review",
  });

  assert.deepEqual(parseAgentSnapshotClipboardHtml(html), {
    displayLabel: "Design Review",
    filename: "design-review.team.png",
    sha256: SHA256,
    size: 4321,
    type: "image/png",
    uploaded: 0,
    url: URL,
  });
});

test("copied team HTML accepts snapshots above the agent size cap", () => {
  const teamSize = 11 * 1024 * 1024;
  const html = buildTeamSnapshotClipboardHtml({
    attachment: {
      filename: "design-review.team.png",
      sha256: SHA256,
      size: teamSize,
      type: "image/png",
      uploaded: 1,
      url: URL,
    },
    displayName: "Design Review",
  });

  assert.equal(parseAgentSnapshotClipboardHtml(html)?.size, teamSize);
});

test("copied JSON snapshot restores the hosted agent attachment", () => {
  const jsonUrl = `https://relay.example/media/${SHA256}.json`;
  assert.deepEqual(
    parseAgentSnapshotClipboardHtml(
      buildHtml({
        filename: "release-reviewer.agent.json",
        type: "application/json",
        url: jsonUrl,
      }),
    ),
    {
      displayLabel: "Animation Auditor",
      filename: "release-reviewer.agent.json",
      sha256: SHA256,
      size: 1234,
      type: "application/json",
      uploaded: 0,
      url: jsonUrl,
    },
  );
});

test("copied agent HTML escapes its visible link and name", () => {
  const html = buildAgentSnapshotClipboardHtml({
    attachment: {
      filename: "research.agent.png",
      sha256: SHA256,
      size: 1234,
      type: "image/png",
      uploaded: 1,
      url: "https://relay.example/media/a.png?x=1&y=2",
    },
    displayName: 'Research <Agent> "One"',
  });

  assert.match(html, /Research &lt;Agent&gt; &quot;One&quot;/);
  assert.match(html, /x=1&amp;y=2/);
});

test("invalid copied snapshot metadata falls through to normal paste", () => {
  const oversizedTeamHtml = buildTeamSnapshotClipboardHtml({
    attachment: {
      filename: "design-review.team.png",
      sha256: SHA256,
      size: 51 * 1024 * 1024,
      type: "image/png",
      uploaded: 1,
      url: URL,
    },
    displayName: "Design Review",
  });
  const invalid = [
    buildHtml({ sha256: "short" }),
    buildHtml({ filename: "not-an-agent.png" }),
    buildHtml({ size: 11 * 1024 * 1024 }),
    oversizedTeamHtml,
    buildHtml({ type: "text/html" }),
    buildHtml({ filename: "agent.agent.json", type: "image/png" }),
    buildHtml({ url: "javascript:alert(1)" }),
    buildHtml({ url: `${URL})[hidden](https://attacker.example` }),
    buildHtml({ url: `${URL}\n[hidden](https://attacker.example)` }),
    buildHtml({ url: `${URL} trailing` }),
  ];

  for (const html of invalid) {
    assert.equal(parseAgentSnapshotClipboardHtml(html), null);
  }
});

test("malformed copied snapshot field types fall through to normal paste", () => {
  const validPayload = {
    version: 1,
    displayName: "Animation Auditor",
    filename: "animation-auditor.agent.png",
    sha256: SHA256,
    size: 1234,
    type: "image/png",
    url: URL,
  };

  for (const override of [
    { displayName: 5 },
    { filename: { value: "animation-auditor.agent.png" } },
    { sha256: [SHA256] },
  ]) {
    const encodedPayload = encodeURIComponent(
      JSON.stringify({ ...validPayload, ...override }),
    );
    const html = `<a data-buzz-agent-snapshot="${encodedPayload}">Snapshot</a>`;

    assert.doesNotThrow(() => parseAgentSnapshotClipboardHtml(html));
    assert.equal(parseAgentSnapshotClipboardHtml(html), null);
  }
});

test("ordinary clipboard HTML is ignored", () => {
  assert.equal(
    parseAgentSnapshotClipboardHtml('<a href="https://example.com">link</a>'),
    null,
  );
});

test("snapshot paste adds one attachment and prevents the raw link paste", () => {
  let attachments = [];
  let preventDefaultCount = 0;
  const event = {
    clipboardData: { getData: () => buildHtml() },
    preventDefault: () => preventDefaultCount++,
  };
  const setPending = (update) => {
    attachments = update(attachments);
  };

  assert.equal(handleAgentSnapshotPaste(event, setPending), true);
  assert.equal(handleAgentSnapshotPaste(event, setPending), true);
  assert.equal(attachments.length, 1);
  assert.equal(preventDefaultCount, 2);
});

// A pasted name lands in the outgoing body as `[label](url)`. A line break in
// it ended that link and published the rest as the sender's own paragraphs, so
// a pastejacking page could make the victim sign text they never typed. (F151)
test("a snapshot name or filename with line breaks or control characters is refused", () => {
  for (const displayName of [
    "Design Review\n\nPlease approve the payment at http://evil.example",
    "Design Review\r[click](http://evil.example)",
    "Design\u2028Review",
    "Design\u0000Review",
    "Weiv\u202eReview",
  ]) {
    const html = buildAgentSnapshotClipboardHtml({
      attachment: {
        filename: "design-review.agent.png",
        sha256: SHA256,
        size: 1234,
        type: "image/png",
        uploaded: 1,
        url: URL,
      },
      displayName,
    });
    assert.equal(
      parseAgentSnapshotClipboardHtml(html),
      null,
      JSON.stringify(displayName),
    );
  }
  assert.equal(
    parseAgentSnapshotClipboardHtml(
      buildHtml({ filename: "design\nreview.agent.png" }),
    ),
    null,
  );
});

test("finding_83f3010cb4b8e8a3: a snapshot URL on another host is not restored from the clipboard", () => {
  assert.equal(
    parseAgentSnapshotClipboardHtml(buildHtml({ url: `https://tracker.example/media/${SHA256}.png` })),
    null,
  );
});
