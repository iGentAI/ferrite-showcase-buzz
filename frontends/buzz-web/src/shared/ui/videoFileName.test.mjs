import assert from "node:assert/strict";
import test from "node:test";

import { fileNameFromUrl } from "./videoFileName.ts";

test("fileNameFromUrl strips hash fragments and survives malformed percent sequences without throwing URIError", () => {
  assert.equal(
    fileNameFromUrl("https://cdn.example/clips/demo%E0%A4%A.mp4?token=1#t=5"),
    "demo%E0%A4%A.mp4",
  );
  assert.equal(
    fileNameFromUrl("https://cdn.example/clips/hello%20world.mp4#t=12"),
    "hello world.mp4",
  );
  assert.equal(fileNameFromUrl("https://cdn.example/#%ZZ"), "Video");
});
