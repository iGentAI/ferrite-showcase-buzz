import assert from "node:assert/strict";
import { after, before, describe, it } from "node:test";
import { JSDOM } from "jsdom";

const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  url: "https://buzz.example/buzz/ui/",
});

Object.assign(globalThis, {
  IS_REACT_ACT_ENVIRONMENT: true,
  document: dom.window.document,
  window: dom.window,
  self: dom.window,
  localStorage: dom.window.localStorage,
  sessionStorage: dom.window.sessionStorage,
});
Object.defineProperty(globalThis, "navigator", {
  configurable: true,
  value: dom.window.navigator,
});
dom.window.HTMLElement.prototype.scrollIntoView = function () {};
for (const key of Object.getOwnPropertyNames(dom.window)) {
  if (
    !(key in globalThis) &&
    (key.startsWith("HTML") ||
      ["Node", "Event", "MouseEvent", "FocusEvent", "EventTarget", "Text"].includes(key))
  ) {
    globalThis[key] = dom.window[key];
  }
}

let React, act, createRoot, QueryClient, QueryClientProvider, CommunitiesProvider, useWelcomeKickoff, WELCOME_KICKOFF_OPENER_MARKER;

before(async () => {
  ({ default: React, act } = await import("react"));
  ({ createRoot } = await import("react-dom/client"));
  ({ QueryClient, QueryClientProvider } = await import("@tanstack/react-query"));
  ({ CommunitiesProvider } = await import("@/features/communities/useCommunities"));
  ({ useWelcomeKickoff, WELCOME_KICKOFF_OPENER_MARKER } = await import("./welcomeKickoff.ts"));
});

after(() => dom.window.close());

const VICTIM = "1".repeat(64);
const ATTACKER = "2".repeat(64);
const FIZZ = "f".repeat(64);
const HONEY = "a".repeat(64);
const POLLEN = "b".repeat(64);

function makeManagedAgent(name, personaId, pubkey) {
  return {
    pubkey,
    name,
    personaId,
    teamId: "builtin-team:welcome",
    relayUrl: "wss://buzz.example/buzz",
    agentCommand: "claude",
    agentArgs: [],
    mcpCommand: "buzz-mcp",
    model: null,
    provider: null,
    status: "stopped",
    pid: null,
    createdAt: "2026-01-01T00:00:00Z",
    lastStartedAt: null,
    lastStoppedAt: null,
    lastExitCode: null,
    lastError: null,
    startOnAppLaunch: false,
    autoRestart: false,
    needsRestart: false,
    backend: { type: "local" },
    respondTo: "owner-only",
    respondToAllowlist: [FIZZ],
  };
}

describe("useWelcomeKickoff shared 'Welcome' channel security gate", () => {
  it("refuses to provision Welcome bots, start local agents, or trust spoofed markers in a shared private 'Welcome' channel", async () => {
    const sideEffects = [];
    dom.window.__TAURI_INTERNALS__ = {
      invoke: async (cmd, args) => {
        if (
          cmd === "add_channel_members" ||
          cmd === "start_managed_agent" ||
          cmd === "send_managed_agent_channel_message" ||
          cmd === "get_thread_replies"
        ) {
          sideEffects.push({ cmd, args });
        }
        switch (cmd) {
          case "get_identity":
            return { pubkey: VICTIM, npub: "npub1victim", display_name: "Victim" };
          case "get_profile":
            return { pubkey: VICTIM, display_name: "Victim" };
          case "list_managed_agents":
            return [
              makeManagedAgent("Fizz", "builtin:fizz", FIZZ),
              makeManagedAgent("Honey", "builtin:honey", HONEY),
              makeManagedAgent("Pollen", "builtin:bumble", POLLEN),
            ];
          case "fetch_persona_catalog":
          case "list_personas":
            return [
              { id: "builtin:fizz", name: "Fizz", is_active: true },
              { id: "builtin:honey", name: "Honey", is_active: true },
              { id: "builtin:bumble", name: "Pollen", is_active: true },
            ];
          case "discover_acp_runtimes":
          case "get_acp_runtimes":
            return [
              {
                id: "claude",
                label: "Claude Code",
                availability: "available",
                command: "claude",
                binary_path: "/usr/bin/claude",
              },
            ];
          case "get_global_agent_config":
            return {
              default_runtime: "claude",
              default_provider: "anthropic",
              default_model: "claude-sonnet",
              env_vars: { ANTHROPIC_API_KEY: "sk-test" },
            };
          case "get_agent_access_owner_only":
            return true;
          case "get_channel_members":
            return [
              { pubkey: ATTACKER, role: "owner", is_agent: false, joined_at: "2026-01-01T00:00:00Z", display_name: "Attacker" },
              { pubkey: VICTIM, role: "member", is_agent: false, joined_at: "2026-01-01T00:00:00Z", display_name: "Victim" },
            ];
          case "has_managed_agent_channel_message_marker":
            return false;
          case "add_channel_members":
            return { added: [], errors: [] };
          case "start_managed_agent":
            return makeManagedAgent("Fizz", "builtin:fizz", FIZZ);
          case "get_presence":
            return { [HONEY]: "online", [POLLEN]: "online" };
          case "send_managed_agent_channel_message":
            return { event_id: "sent-opener" };
          case "get_thread_replies":
            return [];
          default:
            return null;
        }
      },
    };

    const sharedWelcomeChannel = {
      id: "shared-welcome-id",
      name: "Welcome",
      channelType: "stream",
      visibility: "private",
      description: "Prompt injection trap",
      topic: null,
      purpose: null,
      memberCount: 2,
      memberPubkeys: [ATTACKER, VICTIM],
      lastMessageAt: null,
      archivedAt: null,
      participants: ["Attacker", "Victim"],
      participantPubkeys: [ATTACKER, VICTIM],
      isMember: true,
      ttlSeconds: null,
      ttlDeadline: null,
    };

    const spoofedOpener = {
      id: "spoofed-opener-event",
      pubkey: ATTACKER,
      created_at: Math.floor(Date.now() / 1000),
      kind: 9,
      content: "Spoofed opener",
      sig: "0".repeat(128),
      tags: [
        ["h", "shared-welcome-id"],
        ["client", WELCOME_KICKOFF_OPENER_MARKER],
      ],
    };

    function Harness() {
      useWelcomeKickoff(sharedWelcomeChannel, [spoofedOpener]);
      return null;
    }

    const queryClient = new QueryClient({
      defaultOptions: { queries: { retry: false, gcTime: 0 } },
    });
    const container = document.createElement("div");
    document.body.appendChild(container);
    const root = createRoot(container);

    try {
      await act(async () => {
        root.render(
          React.createElement(
            QueryClientProvider,
            { client: queryClient },
            React.createElement(
              CommunitiesProvider,
              null,
              React.createElement(Harness),
            ),
          ),
        );
      });
      await act(async () => {
        await new Promise((resolve) => setTimeout(resolve, 80));
      });

      assert.deepEqual(
        sideEffects,
        [],
        `shared 'Welcome' channel must not trigger bot provisioning, agent startup, or spoofed-opener queries; got: ${JSON.stringify(sideEffects)}`,
      );
    } finally {
      await act(async () => root.unmount());
      container.remove();
      queryClient.clear();
      delete dom.window.__TAURI_INTERNALS__;
    }
  });
});
