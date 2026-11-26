// The shipped preview-features manifest must survive the loader that reads it.
//
// `preview-features.json` is validated at RUNTIME by `manifest.ts`, and a file
// that fails is not an error: it falls back to an empty manifest and logs a
// console warning. One unknown platform value therefore discards every feature,
// which leaves previews ungated (an unknown feature id fails OPEN) and the
// Experiments settings card empty.
import { readFileSync } from "node:fs";
import test from "node:test";
import assert from "node:assert/strict";

import { allFeatures } from "./manifest.ts";

const shipped = JSON.parse(
  readFileSync(new URL("../../../preview-features.json", import.meta.url), "utf8"),
);

test("every shipped feature survives the runtime schema with its platforms", () => {
  assert.ok(shipped.features.length > 0, "an empty manifest gates nothing");
  const loaded = new Map(allFeatures.map((feature) => [feature.id, feature]));
  for (const feature of shipped.features) {
    assert.deepEqual(
      loaded.get(feature.id)?.platforms,
      feature.platforms,
      `${feature.id} did not reach the app; a validation failure empties the whole manifest`,
    );
    assert.ok(loaded.has(feature.id), `${feature.id} is missing from the loaded manifest`);
  }
});

test("ids are unique", () => {
  // `getFeature` returns the FIRST match, so a duplicate id silently shadows.
  const seen = new Set();
  const duplicates = [];
  for (const feature of shipped.features) {
    if (seen.has(feature.id)) duplicates.push(feature.id);
    seen.add(feature.id);
  }
  assert.deepEqual(duplicates, [], `duplicate feature ids: ${duplicates.join(", ")}`);
});
