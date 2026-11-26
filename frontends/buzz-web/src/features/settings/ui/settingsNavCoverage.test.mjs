/**
 * Every settings section is declared consistently and reachable from the nav, once.
 *
 * THE DEFECT THIS EXISTS FOR. A section is declared in THREE places and rendered
 * from a FOURTH:
 *
 *  1. the `SettingsSection` union — what the type system accepts;
 *  2. `SETTINGS_SECTION_VALUES` — what `isSettingsSection` accepts at runtime,
 *     which is what a persisted or URL-supplied section is validated against;
 *  3. `settingsSections` — the descriptors carrying each label and icon;
 *  4. `settingsNavGroups` (SettingsView.tsx) — what the sidebar actually renders.
 *
 * Nothing connects them, and every mismatch is silent. A descriptor with no nav
 * entry is registered and UNREACHABLE — wired, type-checked, in the bundle,
 * selectable by the panel switch, with no way for a person to open it. That was
 * true of Connectors when it shipped, and had been true of Moderation for longer.
 * A union member with no descriptor renders nothing; a descriptor with no union
 * member makes the runtime guard reject a live section.
 *
 * TypeScript catches none of it: every nav entry is a valid `SettingsSection`, and
 * the failures are all MISSING entries, which no type expresses.
 *
 * Both files are parsed as TEXT rather than imported, because `settingsNavGroups`
 * is not exported and the union exists only in the source. Reading the declarations
 * also keeps this honest about what the code says rather than about a duplicated
 * copy of it.
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const PANELS = readFileSync(join(HERE, "SettingsPanels.tsx"), "utf8");
const VIEW = readFileSync(join(HERE, "SettingsView.tsx"), "utf8");

/** The text of a declaration, from `name` to the first line that is exactly `];`. */
function block(source, name, what) {
  const start = source.indexOf(name);
  assert.notEqual(start, -1, `${what}: no declaration named ${name}`);
  const end = source.indexOf("\n];", start);
  assert.notEqual(end, -1, `${what}: ${name} has no closing '];'`);
  return source.slice(start, end);
}

/** 1. The type union's members. */
function unionMembers() {
  const start = PANELS.indexOf("export type SettingsSection =");
  assert.notEqual(start, -1, "SettingsPanels.tsx: no SettingsSection union");
  const end = PANELS.indexOf(";", start);
  return [...PANELS.slice(start, end).matchAll(/"([a-z0-9-]+)"/g)].map((m) => m[1]);
}

/** 2. What the runtime type guard accepts. */
function guardValues() {
  return [
    ...block(PANELS, "SETTINGS_SECTION_VALUES", "SettingsPanels.tsx").matchAll(
      /"([a-z0-9-]+)"/g,
    ),
  ].map((m) => m[1]);
}

/** 3. The descriptors that actually render, by their `value` field. */
function descriptorValues() {
  return [
    ...block(PANELS, "export const settingsSections", "SettingsPanels.tsx").matchAll(
      /value:\s*"([a-z0-9-]+)"/g,
    ),
  ].map((m) => m[1]);
}

/** 4. What the NAV names, in order, duplicates included. */
function navValues() {
  // Group LABELS are quoted strings too, and one could coincide with a section
  // value; dropping them first keeps a group's own name out of the section set.
  const text = block(VIEW, "const settingsNavGroups", "SettingsView.tsx").replace(
    /label:\s*"[^"]*"/g,
    "",
  );
  return [...text.matchAll(/"([a-z0-9-]+)"/g)].map((m) => m[1]);
}

test("all four lists are found and non-trivial", () => {
  // Guards the guard: a regex that silently matched nothing would make every
  // assertion below vacuous, which is worse than having no test.
  for (const [what, values] of [
    ["the union", unionMembers()],
    ["the runtime guard", guardValues()],
    ["the descriptors", descriptorValues()],
    ["the nav", navValues()],
  ]) {
    assert.ok(values.length >= 10, `${what}: only ${values.length} value(s) parsed`);
    assert.ok(values.includes("profile"), `${what}: a known section is missing`);
  }
});

test("the three declarations agree with each other", () => {
  // A descriptor with no union member cannot be selected; a union member with no
  // descriptor renders nothing; a value the runtime guard omits is rejected when
  // restored from a URL or from storage.
  const union = [...unionMembers()].sort();
  const guard = [...guardValues()].sort();
  const descriptors = [...descriptorValues()].sort();
  assert.deepEqual(guard, union, "SETTINGS_SECTION_VALUES has drifted from the union");
  assert.deepEqual(descriptors, union, "settingsSections has drifted from the union");
});

test("every declared section appears in exactly one nav group", () => {
  const nav = navValues();
  const missing = descriptorValues().filter((value) => !nav.includes(value));
  assert.deepEqual(
    missing,
    [],
    `registered and UNREACHABLE — declared in settingsSections but named by no ` +
      `group in settingsNavGroups, so no nav entry renders: ${missing.join(", ")}`,
  );

  const duplicated = [...new Set(nav.filter((v, i) => nav.indexOf(v) !== i))];
  assert.deepEqual(
    duplicated,
    [],
    "a section named by two groups renders two nav entries for one panel",
  );

  const declared = new Set(descriptorValues());
  const unknown = nav.filter((value) => !declared.has(value));
  assert.deepEqual(
    unknown,
    [],
    `a group names ${unknown.join(", ")}, which is not a declared section — the ` +
      `entry is silently dropped, so a rename leaves a hole rather than an error`,
  );
});
