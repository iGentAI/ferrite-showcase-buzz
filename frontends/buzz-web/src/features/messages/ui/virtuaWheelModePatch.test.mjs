import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

// Four levels up, not upstream's five: this port's package root (where
// patches/ lives) is what upstream reaches through its extra desktop/ level.
const patch = await readFile(
  new URL("../../../../patches/virtua@0.49.3.patch", import.meta.url),
  "utf8",
);

test("reader wheel retires Virtua shift mode without publishing scroll end", () => {
  // Virtua 0.49.3 has no public transition for leaving SCROLL_BY_SHIFT.
  // Keep the CJS and ESM patch paths symmetric and deliberately narrower than
  // ACTION_SCROLL_END (2), which also idles direction, clears frozen range,
  // flushes pending jumps, and emits UPDATE_SCROLL_END_EVENT.
  const addedActionBodies = [
    ...patch.matchAll(/\+\s+case 9:\n((?:\+.*\n)+?)(?=\s*})/g),
  ].map(([, body]) =>
    body
      .split("\n")
      .map((line) => line.replace(/^\+\s*/, "").trim())
      .filter(Boolean),
  );
  assert.deepEqual(addedActionBodies, [["I = 0;"], ["w = 0;"]]);
  assert.match(
    patch,
    /if \(!e\.M\(\) \|\| t\.ctrlKey\) return;\n\+\s+e\.q\(9\);\n\+\s+if \(f\) return;/,
  );
  assert.match(
    patch,
    /if \(!e\.M\(\) \|\| t\.ctrlKey\) return;\n\+\s+e\.B\(9\);\n\+\s+if \(c\) return;/,
  );
  assert.doesNotMatch(
    patch,
    /\+\s+if \((?:f|c) \|\| !e\.M\(\) \|\| t\.ctrlKey\) return;/,
  );
  assert.doesNotMatch(patch, /\+\s+e\.(?:q|B)\(2\);/);
});

test("constant item sizes stay numeric when the Virtua row count changes", () => {
  // The array-aware patch receives either the expanded per-row estimate array
  // or the original numeric `itemSize`. Calling `.slice()` on the numeric
  // form throws during a concurrent append, after Virtua has partially moved
  // its cache; React then retries synchronously and reports an invalid NaN
  // `top`. Guard both the CJS and ESM copies before slicing.
  const guardedSlices = [
    ...patch.matchAll(
      /\+\s+if \(Array\.isArray\(r\)\) \{\n\+\s+const t = o \? r\.slice\(0, (?:s|n)\) : r\.slice\(r\.length - (?:s|n)\);/g,
    ),
  ];
  assert.equal(guardedSlices.length, 2);
  assert.doesNotMatch(patch, /\+\s+if \(r\) \{\n\+\s+const t = o \? r\.slice/);
});
