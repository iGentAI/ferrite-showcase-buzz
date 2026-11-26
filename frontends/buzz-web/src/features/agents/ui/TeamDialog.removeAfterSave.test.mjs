/**
 * "Remove agents" in the team editor deletes the removed personas only after
 * the team save succeeded.
 *
 * `useTeamActions.handleTeamSubmit` catches a failed update and shows the
 * error, so its promise resolves either way. TeamDialog used to await it and
 * then delete the removed personas unconditionally: a rejected save left the
 * team still listing agents whose definitions were already gone. The submit
 * callback now resolves `true` only on success, and the dialog deletes
 * nothing otherwise.
 */

import assert from "node:assert/strict";
import { after, before, describe, it } from "node:test";
import { JSDOM } from "jsdom";

const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  url: "http://localhost",
});

Object.assign(globalThis, {
  IS_REACT_ACT_ENVIRONMENT: true,
  ResizeObserver: class {
    observe() {}
    unobserve() {}
    disconnect() {}
  },
  document: dom.window.document,
  localStorage: dom.window.localStorage,
  self: dom.window,
  window: dom.window,
});
Object.defineProperty(globalThis, "navigator", {
  configurable: true,
  value: dom.window.navigator,
});
dom.window.requestAnimationFrame = (callback) => setTimeout(callback, 0);
globalThis.requestAnimationFrame = dom.window.requestAnimationFrame;
dom.window.ResizeObserver = globalThis.ResizeObserver;
dom.window.matchMedia ??= (query) => ({
  matches: false,
  media: query,
  onchange: null,
  addListener: () => {},
  removeListener: () => {},
  addEventListener: () => {},
  removeEventListener: () => {},
  dispatchEvent: () => false,
});
globalThis.matchMedia = dom.window.matchMedia;
for (const key of Object.getOwnPropertyNames(dom.window)) {
  if (
    !(key in globalThis) &&
    (key.startsWith("HTML") ||
      key.startsWith("SVG") ||
      [
        "Node",
        "NodeFilter",
        "Event",
        "CustomEvent",
        "MouseEvent",
        "KeyboardEvent",
        "FocusEvent",
        "PointerEvent",
        "EventTarget",
        "Text",
        "DocumentFragment",
        "IntersectionObserver",
        "MutationObserver",
        "Range",
        "Selection",
      ].includes(key))
  ) {
    const val = dom.window[key];
    if (val !== undefined) globalThis[key] = val;
  }
}
globalThis.getComputedStyle = dom.window.getComputedStyle.bind(dom.window);
// Radix's layer bookkeeping dispatches plain objects; JSDOM rejects them.
const originalDispatch = dom.window.EventTarget.prototype.dispatchEvent;
dom.window.EventTarget.prototype.dispatchEvent = function (event) {
  if (!(event instanceof dom.window.Event)) return false;
  return originalDispatch.call(this, event);
};

let React, act, createRoot, TeamDialog, ThemeProvider;

before(async () => {
  ({ default: React, act } = await import("react"));
  ({ createRoot } = await import("react-dom/client"));
  ({ TeamDialog } = await import("./TeamDialog.tsx"));
  ({ ThemeProvider } = await import("@/shared/theme/ThemeProvider.tsx"));
});

after(() => dom.window.close());

function persona(id, displayName) {
  return {
    id,
    displayName,
    avatarUrl: null,
    systemPrompt: "",
    isBuiltIn: false,
  };
}

const PERSONAS = [persona("p1", "Ada"), persona("p2", "Grace")];

function buttonNamed(label) {
  return [...document.body.querySelectorAll("button")].find(
    (button) => button.textContent?.trim() === label,
  );
}

async function click(element) {
  await act(async () => {
    element.dispatchEvent(
      new dom.window.MouseEvent("click", { bubbles: true, cancelable: true }),
    );
    await new Promise((resolve) => setTimeout(resolve, 0));
  });
}

/** Edit a two-member team, drop Grace, save, and choose "Remove agent". */
async function removeGraceAndDelete(saveResult) {
  const submitted = [];
  const deleted = [];
  const container = document.createElement("div");
  document.body.appendChild(container);
  const root = createRoot(container);

  await act(async () => {
    root.render(
      React.createElement(
        ThemeProvider,
        null,
        React.createElement(TeamDialog, {
          open: true,
          title: "Edit team",
          description: "",
          submitLabel: "Save changes",
          initialValues: {
            id: "team-1",
            name: "Squad",
            description: "",
            personaIds: ["p1", "p2"],
          },
          personas: PERSONAS,
          error: null,
          isPending: false,
          onOpenChange: () => {},
          onSubmit: async (input) => {
            submitted.push(input);
            return saveResult;
          },
          onDeleteRemovedPersonas: async (ids) => {
            deleted.push(ids);
          },
        }),
      ),
    );
  });

  const grace = [...document.body.querySelectorAll('[role="option"]')].find(
    (option) => option.textContent?.includes("Grace"),
  );
  assert.ok(grace, "the Grace option renders");
  await click(grace);

  const save = buttonNamed("Save changes");
  assert.ok(save, "the save button renders");
  await click(save);

  const remove = buttonNamed("Remove agent");
  assert.ok(remove, "removing a member asks whether to delete the agent");
  await click(remove);

  await act(async () => {
    root.unmount();
  });
  container.remove();
  return { submitted, deleted };
}

describe("TeamDialog: Remove agents", () => {
  it("deletes nothing when the team save fails", async () => {
    const { submitted, deleted } = await removeGraceAndDelete(false);
    assert.equal(submitted.length, 1, "the save was attempted");
    assert.deepEqual(submitted[0].personaIds, ["p1"]);
    assert.deepEqual(
      deleted,
      [],
      "a failed save must not delete the removed agents",
    );
  });

  it("deletes the removed agents once the team save succeeds", async () => {
    const { submitted, deleted } = await removeGraceAndDelete(true);
    assert.equal(submitted.length, 1);
    assert.deepEqual(deleted, [["p2"]]);
  });
});
