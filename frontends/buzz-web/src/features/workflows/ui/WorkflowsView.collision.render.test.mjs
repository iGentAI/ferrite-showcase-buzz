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
dom.window.matchMedia = () => ({
  matches: false,
  media: "",
  onchange: null,
  addListener: () => {},
  removeListener: () => {},
  addEventListener: () => {},
  removeEventListener: () => {},
  dispatchEvent: () => false,
});
globalThis.matchMedia = dom.window.matchMedia;
dom.window.ResizeObserver = class ResizeObserver {
  observe() {}
  unobserve() {}
  disconnect() {}
};
globalThis.ResizeObserver = dom.window.ResizeObserver;
for (const key of Object.getOwnPropertyNames(dom.window)) {
  if (
    !(key in globalThis) &&
    (key.startsWith("HTML") ||
      ["Node", "Event", "MouseEvent", "FocusEvent", "EventTarget", "Text"].includes(key))
  ) {
    globalThis[key] = dom.window[key];
  }
}

let React, act, createRoot, QueryClient, QueryClientProvider, CommunitiesProvider, ThemeProvider, TooltipProvider, WorkflowsView, resolveEditorWorkflowHint;

before(async () => {
  ({ default: React, act } = await import("react"));
  ({ createRoot } = await import("react-dom/client"));
  ({ QueryClient, QueryClientProvider } = await import("@tanstack/react-query"));
  ({ CommunitiesProvider } = await import("@/features/communities/useCommunities"));
  ({ ThemeProvider } = await import("@/shared/theme/ThemeProvider"));
  ({ TooltipProvider } = await import("@/shared/ui/tooltip"));
  ({ WorkflowsView, resolveEditorWorkflowHint } = await import("./WorkflowsView.tsx"));
});

after(() => dom.window.close());

const VICTIM = "1".repeat(64);
const ATTACKER = "2".repeat(64);

const CHANNELS = [
  {
    id: "chan-general",
    name: "general",
    channelType: "stream",
    visibility: "open",
    description: "",
    topic: null,
    purpose: null,
    memberCount: 2,
    memberPubkeys: [VICTIM, ATTACKER],
    lastMessageAt: null,
    archivedAt: null,
    participants: [],
    participantPubkeys: [],
    isMember: true,
    ttlSeconds: null,
    ttlDeadline: null,
  },
  {
    id: "chan-ops",
    name: "ops",
    channelType: "stream",
    visibility: "private",
    description: "",
    topic: null,
    purpose: null,
    memberCount: 1,
    memberPubkeys: [VICTIM],
    lastMessageAt: null,
    archivedAt: null,
    participants: [],
    participantPubkeys: [],
    isMember: true,
    ttlSeconds: null,
    ttlDeadline: null,
  },
];

describe("WorkflowsView cross-channel and cross-author workflow ID collision protection", () => {
  it("disables toggle/delete on foreign colliding cards and resolves editorWorkflowHint to the viewer's own workflow instead of a newer foreign collision", async () => {
    const mutations = [];
    let getWorkflowCalls = 0;

    const attackerWorkflow = {
      id: "deploy-shared",
      revision: "rev-attacker",
      name: "Attacker Poison Workflow",
      owner_pubkey: ATTACKER,
      channel_id: "chan-general",
      definition: {
        name: "Attacker Poison Workflow",
        status: "active",
        steps: [{ id: "s1", type: "post", text: "pwned" }],
      },
      status: "active",
      created_at: 100,
      updated_at: 200,
    };

    const victimWorkflow = {
      id: "deploy-shared",
      revision: "rev-victim",
      name: "Victim Ops Workflow",
      owner_pubkey: VICTIM,
      channel_id: "chan-ops",
      definition: {
        name: "Victim Ops Workflow",
        status: "active",
        steps: [{ id: "s1", type: "post", text: "legit" }],
      },
      status: "active",
      created_at: 90,
      updated_at: 100,
    };

    dom.window.__TAURI_INTERNALS__ = {
      invoke: async (cmd, args) => {
        if (cmd === "update_workflow" || cmd === "delete_workflow") {
          mutations.push({ cmd, args });
        }
        switch (cmd) {
          case "get_identity":
            return { pubkey: VICTIM, npub: "npub1victim", display_name: "Victim" };
          case "get_channels_workflows":
            // Ordered newest-first: attacker's cell comes BEFORE victim's cell!
            return [attackerWorkflow, victimWorkflow];
          case "get_workflow":
            getWorkflowCalls += 1;
            return victimWorkflow;
          case "get_users_batch":
            return { profiles: {} };
          case "get_workflow_runs":
            return { runs: [], next: null };
          default:
            return [];
        }
      },
    };

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
              ThemeProvider,
              null,
              React.createElement(
                TooltipProvider,
                null,
                React.createElement(
                  CommunitiesProvider,
                  null,
                  React.createElement(WorkflowsView, {
                    channels: CHANNELS,
                    editor: null,
                    onCloseEditor: () => {},
                onCreateWorkflow: () => {},
                onDuplicateWorkflow: () => {},
                onEditWorkflow: () => {},
                onViewWorkflow: () => {},
                  onEditorPaneChange: () => {},
                  }),
                ),
              ),
            ),
          ),
        );
      });
      await act(async () => {
        await new Promise((resolve) => setTimeout(resolve, 80));
      });

      const cards = [
        ...container.querySelectorAll('[data-testid="workflow-card-deploy-shared"]'),
      ];
      assert.equal(cards.length, 2, "both distinct (owner, channel, id) cards render without duplicate key clobbering");

      const attackerCard = cards.find((el) =>
        el.textContent?.includes("Attacker Poison Workflow"),
      );
      assert.ok(attackerCard, "attacker card rendered");
      const attackerSwitch = attackerCard.querySelector('[role="switch"]');
      assert.ok(attackerSwitch, "switch rendered on card");
      assert.equal(
        attackerSwitch.hasAttribute("disabled"),
        true,
        "foreign unowned workflow card must not allow toggling status",
      );

      await act(async () => {
        attackerSwitch.dispatchEvent(
          new dom.window.MouseEvent("click", { bubbles: true }),
        );
      });
      assert.deepEqual(
        mutations,
        [],
        "clicking foreign card toggle must never invoke update_workflow",
      );

      // Verify resolveEditorWorkflowHint selects Victim's own cell rather than Attacker's newer cell,
      // and returns undefined for a third-party viewer on a multi-author collision so F165 getWorkflow runs.
      const list = [
        {
          workflow: {
            id: "deploy-shared",
            revision: "rev-attacker",
            name: "Attacker Poison Workflow",
            ownerPubkey: ATTACKER,
            channelId: "chan-general",
            definition: attackerWorkflow.definition,
            status: "active",
            createdAt: 100,
            updatedAt: 200,
          },
        },
        {
          workflow: {
            id: "deploy-shared",
            revision: "rev-victim",
            name: "Victim Ops Workflow",
            ownerPubkey: VICTIM,
            channelId: "chan-ops",
            definition: victimWorkflow.definition,
            status: "active",
            createdAt: 90,
            updatedAt: 100,
          },
        },
      ];
      const hintForOwner = resolveEditorWorkflowHint(list, "deploy-shared", VICTIM);
      assert.equal(hintForOwner?.ownerPubkey, VICTIM);
      assert.equal(hintForOwner?.name, "Victim Ops Workflow");

      const hintForStranger = resolveEditorWorkflowHint(
        list,
        "deploy-shared",
        "3".repeat(64),
      );
      assert.equal(
        hintForStranger,
        undefined,
        "multi-author collision for third-party viewer must return undefined so F165 resolveWorkflowVersion handles it",
      );
      void getWorkflowCalls;
    } finally {
      await act(async () => root.unmount());
      container.remove();
      queryClient.clear();
      delete dom.window.__TAURI_INTERNALS__;
    }
  });
});
