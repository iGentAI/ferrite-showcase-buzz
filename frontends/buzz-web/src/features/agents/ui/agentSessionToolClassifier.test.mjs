import assert from "node:assert/strict";
import test from "node:test";

import {
  findBuzzToolName,
  formatToolTitle,
  getBuzzToolInfo,
} from "./agentSessionToolCatalog.ts";
import {
  classifyTool,
  parseBuzzCliCommand,
  tokenizeShellCommand,
} from "./agentSessionToolClassifier.ts";

test("tokenizeShellCommand preserves quoted strings and command separators", () => {
  assert.deepEqual(
    tokenizeShellCommand(
      'echo "hello world" | buzz messages send --content - --channel agents; buzz feed get',
    ),
    [
      "echo",
      "hello world",
      "|",
      "buzz",
      "messages",
      "send",
      "--content",
      "-",
      "--channel",
      "agents",
      ";",
      "buzz",
      "feed",
      "get",
    ],
  );
});

test("parseBuzzCliCommand returns null preview for echo-piped stdin sends", () => {
  const descriptor = parseBuzzCliCommand(
    'echo "Permission wired" | buzz messages send --channel agents --content -',
  );

  assert.equal(descriptor?.renderClass, "message");
  assert.equal(descriptor?.label, "Send Message");
  assert.equal(descriptor?.preview, null);
  assert.equal(descriptor?.operation, "messages.send");
});

test("parseBuzzCliCommand returns null preview for printf-piped stdin sends", () => {
  const descriptor = parseBuzzCliCommand(
    "printf 'hello\\n\\nworld\\n' | buzz messages send --channel a6e0737c-4205-4bcc-9741-2aad800e613f --content -",
  );

  assert.equal(descriptor?.renderClass, "message");
  assert.equal(descriptor?.preview, null);
});

test("parseBuzzCliCommand returns null preview for heredoc/cat stdin sends", () => {
  const descriptor = parseBuzzCliCommand(
    'buzz messages send --channel some-uuid --content "$(cat /tmp/file)"',
  );

  assert.equal(descriptor?.renderClass, "message");
  assert.equal(descriptor?.preview, null);
});

test("parseBuzzCliCommand returns null preview for --content with embedded command substitution", () => {
  const descriptor = parseBuzzCliCommand(
    'buzz messages send --channel some-uuid --content "prefix $(cat /tmp/f)"',
  );

  assert.equal(descriptor?.renderClass, "message");
  assert.equal(descriptor?.preview, null);
});

test("parseBuzzCliCommand returns null preview for --content with a bare variable", () => {
  const descriptor = parseBuzzCliCommand(
    'buzz messages send --channel some-uuid --content "$MESSAGE"',
  );

  assert.equal(descriptor?.renderClass, "message");
  assert.equal(descriptor?.preview, null);
});

test("parseBuzzCliCommand returns null preview for --content with a prefixed variable", () => {
  const descriptor = parseBuzzCliCommand(
    'buzz messages send --channel some-uuid --content "prefix $MESSAGE"',
  );

  assert.equal(descriptor?.renderClass, "message");
  assert.equal(descriptor?.preview, null);
});

test("parseBuzzCliCommand preserves inline --content for sends", () => {
  const descriptor = parseBuzzCliCommand(
    'buzz messages send --channel agents --content "Hello from inline"',
  );

  assert.equal(descriptor?.renderClass, "message");
  assert.equal(descriptor?.preview, "Hello from inline");
});

test("parseBuzzCliCommand preserves --content=inline for sends", () => {
  const descriptor = parseBuzzCliCommand(
    "buzz messages send --channel agents --content=Acknowledged",
  );

  assert.equal(descriptor?.renderClass, "message");
  assert.equal(descriptor?.preview, "Acknowledged");
});

test("parseBuzzCliCommand never surfaces --channel as preview for sends", () => {
  const commands = [
    "printf 'msg' | buzz messages send --channel my-uuid --content -",
    'buzz messages send --channel my-uuid --content "$(cat /tmp/f)"',
    "buzz messages send --channel my-uuid --content -",
  ];

  for (const cmd of commands) {
    const descriptor = parseBuzzCliCommand(cmd);
    assert.equal(descriptor?.renderClass, "message");
    assert.notEqual(
      descriptor?.preview,
      "my-uuid",
      `send preview leaked --channel for: ${cmd}`,
    );
  }
});

test("parseBuzzCliCommand ignores a buzz token that is an argument of another program", () => {
  // The shell string is model-written; a `buzz` word that is not the invoked
  // program must not render as relay activity (fabricated "Sent message" rows).
  const commands = [
    'docker run buzz messages send --content "Shipped to prod"',
    "grep buzz messages send notes.txt",
    "echo buzz messages send --content Done",
    "cat buzz channels archive --channel ops",
  ];

  for (const cmd of commands) {
    assert.equal(
      parseBuzzCliCommand(cmd),
      null,
      `must not be rendered as a buzz CLI call: ${cmd}`,
    );
  }
});

test("parseBuzzCliCommand accepts buzz in command position after separators and env prefixes", () => {
  const commands = [
    'echo "hi" | buzz messages send --channel agents --content -',
    "cd /tmp && buzz feed get",
    "true || buzz feed get",
    "buzz feed get; echo done",
    "BUZZ_RELAY=wss://relay.example buzz feed get",
    "env BUZZ_RELAY=wss://relay.example buzz feed get",
    "sudo buzz feed get",
    "/usr/local/bin/buzz feed get",
    "echo buzz | buzz feed get",
  ];

  for (const cmd of commands) {
    const descriptor = parseBuzzCliCommand(cmd);
    assert.ok(descriptor, `must be recognised as a buzz CLI call: ${cmd}`);
    assert.equal(descriptor.source, "shell");
  }
});

test("parseBuzzCliCommand does not let a boolean global flag swallow the subcommand group", () => {
  const descriptor = parseBuzzCliCommand(
    'buzz --json messages send --channel c --content "x"',
  );

  assert.equal(descriptor?.renderClass, "message");
  assert.equal(descriptor?.operation, "messages.send");
  assert.equal(descriptor?.preview, "x");

  // A value-taking global flag keeps working.
  const withValue = parseBuzzCliCommand(
    'buzz --format compact messages send --channel channel-1 --content "y"',
  );
  assert.equal(withValue?.operation, "messages.send");
  assert.equal(withValue?.preview, "y");
});

test("findBuzzToolName matches whole Buzz tool names, not foreign tools containing one", () => {
  // Third-party MCP tools that merely contain a Buzz tool name.
  const foreign = [
    "web_search",
    "vector_search",
    "codebase_search",
    "crm_get_users",
    "mcp__slack__send_message",
    "mcp__linear__search",
  ];
  for (const name of foreign) {
    assert.equal(
      findBuzzToolName(name, true),
      null,
      `${name} must not resolve to a Buzz tool`,
    );
    assert.equal(
      getBuzzToolInfo(name),
      null,
      `${name} must have no Buzz info`,
    );
  }

  // Whole names, with the known `buzz_` and Buzz MCP mount wrappers, resolve.
  assert.equal(findBuzzToolName("search", true), "search");
  assert.equal(findBuzzToolName("buzz_search", true), "search");
  assert.equal(findBuzzToolName("Buzz: get_users", true), "get_users");
  assert.equal(
    findBuzzToolName("mcp__buzz__send_message", true),
    "send_message",
  );
  assert.equal(
    findBuzzToolName("mcp__buzz-relay__buzz_get_feed", true),
    "get_feed",
  );
  assert.equal(
    findBuzzToolName("Sending message to channel #general", true),
    "send_message",
  );
});

test("classifyTool keeps a foreign search tool out of the Buzz relay catalog", () => {
  const descriptor = classifyTool({
    title: "web_search",
    toolName: "web_search",
    buzzToolName: null,
    args: { query: "ferrite release notes" },
    result: "",
    isError: false,
  });

  assert.equal(descriptor.renderClass, "generic");
  assert.equal(descriptor.source, "fallback");
  assert.equal(descriptor.label, "Ran tool");
  // The transcript row keeps the tool's real name instead of "Search".
  assert.equal(formatToolTitle("web_search", "web_search"), "web_search");
});

test("classifyTool promotes load_skill to skill-read descriptors", () => {
  const descriptor = classifyTool({
    title: "load_skill",
    toolName: "load_skill",
    buzzToolName: null,
    args: { name: "block-safe-github" },
    result: "# Safe GitHub usage at Block\n",
    isError: false,
  });

  assert.equal(descriptor.renderClass, "skill-read");
  assert.equal(descriptor.label, "Read skill");
  assert.equal(descriptor.preview, "block-safe-github");
  assert.deepEqual(descriptor.action, {
    verb: "Read",
    object: "block-safe-github",
  });
  assert.equal(descriptor.groupKey, "skill:load");
});

test("classifyTool promotes supporting-file load_skill to skill-read file descriptors", () => {
  const descriptor = classifyTool({
    title: "load_skill",
    toolName: "load_skill",
    buzzToolName: null,
    args: { name: "block-safe-github/references/foo.md" },
    result: "# Reference\n",
    isError: false,
  });

  assert.equal(descriptor.renderClass, "skill-read");
  assert.equal(descriptor.label, "Read skill file");
  assert.equal(descriptor.groupKey, "skill:load-file");
});

test("classifyTool promotes buzz CLI shell commands to relay operations", () => {
  const descriptor = classifyTool({
    title: "Shell",
    toolName: "dev__shell",
    buzzToolName: null,
    args: { command: "buzz channels get --channel buzz-agent-observability" },
    result: "{}",
    isError: false,
  });

  assert.equal(descriptor.renderClass, "relay-op");
  assert.equal(descriptor.label, "Channels Get");
  assert.equal(descriptor.preview, "buzz-agent-observability");
  assert.equal(descriptor.groupKey, "buzz-cli:channels.get");
});

test("classifyTool falls back once to a generic descriptor", () => {
  const descriptor = classifyTool({
    title: "Mystery",
    toolName: "mcp__mystery",
    buzzToolName: null,
    args: { path: "notes.md" },
    result: "",
    isError: false,
  });

  assert.equal(descriptor.renderClass, "generic");
  assert.equal(descriptor.label, "Ran tool");
  assert.equal(descriptor.preview, "notes.md");
  assert.equal(descriptor.source, "fallback");
});

test("a failed tool that only the generic fallback recognizes is classified as an error", () => {
  const descriptor = classifyTool({
    title: "some_unknown_vendor_tool",
    toolName: "some_unknown_vendor_tool",
    buzzToolName: null,
    args: {},
    result: "boom",
    isError: true,
  });
  assert.equal(descriptor.renderClass, "error");
  assert.match(descriptor.label, /failed$/);
});

test("a mounted or prefixed Buzz tool name classifies like the bare name", () => {
  for (const toolName of ["send_message", "mcp__buzz__send_message", "buzz_send_message"]) {
    const descriptor = classifyTool({
      title: toolName,
      toolName,
      buzzToolName: null,
      args: { content: "hello" },
      result: "",
      isError: false,
    });
    assert.equal(descriptor.operation, "send_message", toolName);
    assert.equal(descriptor.renderClass, "message", toolName);
    assert.equal(descriptor.groupKey, "buzz:send_message", toolName);
  }
});
