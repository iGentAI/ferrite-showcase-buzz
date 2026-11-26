// The composer's autofocus must not take focus out of an open overlay.
//
// Measured on staging 2026-09-12: with the profile popover open, closing the
// relay socket made the client reconnect, the composer's editor was rebuilt,
// autofocus re-fired, focus moved from `profile-popover-set-status` to
// `message-input`, and Radix dismissed the popover as a focus-outside. A
// journey clicking the popover's Settings item found it animating out
// ("not stable") and then gone.
import assert from "node:assert/strict";
import test from "node:test";

import { composerMayTakeFocus } from "./useComposerAutofocus.ts";

const body = { tagName: "BODY" };

function element(tagName, { editable = false, inside = [] } = {}) {
  return {
    tagName,
    isContentEditable: editable,
    closest: (selector) =>
      inside.some((marker) => selector.split(",").some((part) => part.trim() === marker)) ? {} : null,
  };
}

test("nothing focused: the composer takes focus", () => {
  assert.equal(composerMayTakeFocus(null, body), true);
  assert.equal(composerMayTakeFocus(body, body), true);
  assert.equal(composerMayTakeFocus(element("BUTTON"), body), true, "a sidebar button outside any overlay");
});

test("a text-entry surface keeps focus", () => {
  for (const tag of ["INPUT", "TEXTAREA", "SELECT"]) {
    assert.equal(composerMayTakeFocus(element(tag), body), false, tag);
  }
  assert.equal(composerMayTakeFocus(element("DIV", { editable: true }), body), false);
});

test("a menu item inside an open popover keeps focus", () => {
  const menuItem = element("BUTTON", { inside: ["[data-radix-popper-content-wrapper]", '[role="menu"]'] });
  assert.equal(composerMayTakeFocus(menuItem, body), false);
  assert.equal(composerMayTakeFocus(element("BUTTON", { inside: ['[role="dialog"]'] }), body), false);
  assert.equal(composerMayTakeFocus(element("BUTTON", { inside: ['[role="alertdialog"]'] }), body), false);
});
