// F433 / F434: markdown media helpers driven by relay-authored metadata.
import assert from "node:assert/strict";
import test from "node:test";

import {
  aspectRatioFromDim,
  clearDecodedImageDimensions,
  DECODED_IMAGE_DIMENSIONS_LIMIT,
  decodedImageDimensionsSize,
  dimensionsFromDim,
  getDecodedImageDimensions,
  rememberDecodedImageDimensions,
} from "./utils.ts";

test("aspectRatioFromDim refuses a zero width like dimensionsFromDim", () => {
  assert.equal(aspectRatioFromDim("0x720"), undefined);
  assert.equal(dimensionsFromDim("0x720"), undefined);
  assert.equal(aspectRatioFromDim("1280x0"), undefined);
  assert.equal(aspectRatioFromDim("1280x720"), 1280 / 720);
  assert.equal(aspectRatioFromDim("garbage"), undefined);
  assert.equal(aspectRatioFromDim(undefined), undefined);
});

test("the decoded image size cache is bounded and evicts the least recent", () => {
  clearDecodedImageDimensions();
  rememberDecodedImageDimensions("https://relay.example/keep.png", 10, 20);
  for (let n = 0; n < DECODED_IMAGE_DIMENSIONS_LIMIT + 50; n += 1) {
    if (n % 100 === 0) {
      // Reading an entry marks it recently used.
      getDecodedImageDimensions("https://relay.example/keep.png");
    }
    rememberDecodedImageDimensions(`https://relay.example/${n}.png`, 100, 50);
  }
  assert.equal(decodedImageDimensionsSize(), DECODED_IMAGE_DIMENSIONS_LIMIT);
  assert.equal(getDecodedImageDimensions("https://relay.example/0.png"), undefined);
  assert.deepEqual(getDecodedImageDimensions("https://relay.example/keep.png"), {
    height: 20,
    width: 10,
  });
});

test("clearDecodedImageDimensions forgets every learned size", () => {
  rememberDecodedImageDimensions("https://relay.example/a.png", 1, 1);
  clearDecodedImageDimensions();
  assert.equal(decodedImageDimensionsSize(), 0);
  assert.equal(getDecodedImageDimensions("https://relay.example/a.png"), undefined);
});
