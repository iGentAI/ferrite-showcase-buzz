import assert from "node:assert/strict";
import test from "node:test";

import { createFocusDrawerEscapeHandler } from "./focusDrawerEscapeOwnership.ts";

/**
 * A mounted thread drawer must claim only the Escape that comes from its own
 * subtree. An Escape belonging to another layer stays completely untouched.
 *
 * `contains` is modelled over a parent chain rather than a real DOM (this
 * package has no jsdom; unit tests here drive stub nodes — see
 * useThreadViewModeSwitch.test.mjs). The model is `Node.contains`'s spec
 * sentence: true for the node itself and any descendant. Nothing subtler about
 * the DOM is being relied on — the property under test is which subtree the key
 * came from.
 */
function node(name, parent = null) {
  return {
    name,
    nodeType: 1,
    parent,
    contains(other) {
      for (let cursor = other; cursor; cursor = cursor.parent) {
        if (cursor === this) return true;
      }
      return false;
    },
  };
}

function escapeEvent(target, key = "Escape") {
  const calls = { close: 0, preventDefault: 0, stopImmediatePropagation: 0 };
  const event = {
    key,
    target,
    preventDefault() {
      calls.preventDefault += 1;
    },
    stopImmediatePropagation() {
      calls.stopImmediatePropagation += 1;
    },
  };
  return { calls, event };
}

function run({ drawer, target, key }) {
  const { calls, event } = escapeEvent(target, key);
  const handler = createFocusDrawerEscapeHandler(
    () => drawer,
    () => {
      calls.close += 1;
    },
  );
  handler(event);
  return calls;
}

/* ── the drawer's own Escape still works ─────────────────────────────────── */

test("claims Escape dispatched at the drawer itself", () => {
  const drawer = node("drawer");
  assert.deepEqual(run({ drawer, target: drawer }), {
    close: 1,
    preventDefault: 1,
    stopImmediatePropagation: 1,
  });
});

test("claims Escape from a control inside the drawer, ahead of that control", () => {
  // The reason the listener is at window-capture: the thread composer's mention
  // autocomplete would otherwise preventDefault this in the bubble phase and
  // the drawer would never close. stopImmediatePropagation is what keeps it
  // from running, so it has to still be called for in-drawer targets.
  const drawer = node("drawer");
  const panel = node("thread-panel", drawer);
  const editor = node("composer-editor", panel);
  assert.deepEqual(run({ drawer, target: editor }), {
    close: 1,
    preventDefault: 1,
    stopImmediatePropagation: 1,
  });
});

test("ignores every key that is not Escape", () => {
  const drawer = node("drawer");
  for (const key of ["Enter", "Tab", "Esc", "escape", "a"]) {
    assert.deepEqual(
      run({ drawer, key, target: drawer }),
      { close: 0, preventDefault: 0, stopImmediatePropagation: 0 },
      `key ${key} must not close the drawer`,
    );
  }
});

/* ── other layers keep their Escape ────────────────────────── */

test("leaves a portalled dialog's Escape entirely alone", () => {
  // Every Radix layer portals to document.body, so a dialog opened while the
  // drawer is mounted is a sibling of the app root, not a descendant of the
  // drawer. Its Escape must reach Radix's document-capture dismiss untouched —
  // and `preventDefault` matters as much as `stopImmediatePropagation`, because
  // that handler refuses to dismiss an already-defaultPrevented event.
  const body = node("body");
  const appRoot = node("app-root", body);
  const drawer = node("drawer", appRoot);
  const dialogContent = node("members-dialog", body);
  const closeButton = node("dialog-close", dialogContent);

  assert.deepEqual(run({ drawer, target: dialogContent }), {
    close: 0,
    preventDefault: 0,
    stopImmediatePropagation: 0,
  });
  assert.deepEqual(run({ drawer, target: closeButton }), {
    close: 0,
    preventDefault: 0,
    stopImmediatePropagation: 0,
  });
});

test("leaves Escape alone when focus sits nowhere", () => {
  // A menu that closes with onCloseAutoFocus prevented leaves activeElement on
  // <body>, so the key is dispatched at body. The drawer must yield: whatever
  // layer is topmost gets it, and if none is, MessageThreadPanel's bubble-phase
  // useEscapeKey closes the thread anyway.
  const body = node("body");
  const drawer = node("drawer", body);
  assert.deepEqual(run({ drawer, target: body }), {
    close: 0,
    preventDefault: 0,
    stopImmediatePropagation: 0,
  });
});

test("yields instead of throwing on a non-Node target", () => {
  // window.dispatchEvent(new KeyboardEvent("keydown")) targets `window`, which
  // is an EventTarget but not a Node; Node.prototype.contains throws a
  // TypeError on one. A throw inside a capture listener would lose the press.
  const drawer = node("drawer");
  const windowLike = { addEventListener() {} };
  assert.deepEqual(run({ drawer, target: windowLike }), {
    close: 0,
    preventDefault: 0,
    stopImmediatePropagation: 0,
  });
  assert.deepEqual(run({ drawer, target: null }), {
    close: 0,
    preventDefault: 0,
    stopImmediatePropagation: 0,
  });
});

test("yields while the drawer ref is unattached", () => {
  assert.deepEqual(run({ drawer: null, target: node("anything") }), {
    close: 0,
    preventDefault: 0,
    stopImmediatePropagation: 0,
  });
});

/* ── the predicate on its own ────────────────────────────────────────────── */

test("the drawer element is read at event time, not at attach time", () => {
  // The listener is attached in an effect keyed on onClose; the ref may attach
  // or re-attach independently, and a stale capture would silently stop
  // claiming.
  let drawer = null;
  const calls = [];
  const handler = createFocusDrawerEscapeHandler(
    () => drawer,
    () => calls.push("close"),
  );

  const first = node("drawer-1");
  const { event: beforeAttach } = escapeEvent(first);
  handler(beforeAttach);
  assert.deepEqual(calls, []);

  drawer = first;
  const { event: afterAttach } = escapeEvent(first);
  handler(afterAttach);
  assert.deepEqual(calls, ["close"]);
});
