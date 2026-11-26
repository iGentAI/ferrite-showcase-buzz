// F428: grammar slots are released when the blocks using them unmount.
import assert from "node:assert/strict";
import test from "node:test";

import { createShikiLanguagePool } from "./shikiLanguagePool.ts";

const THEME = "github-dark";
const KNOWN = new Set(Array.from({ length: 60 }, (_, n) => `lang${n}`));

function fakeFactory() {
  const created = [];
  const create = async () => {
    const highlighter = {
      disposed: false,
      languages: new Set(),
      themes: new Set(),
      dispose() {
        this.disposed = true;
      },
      async loadLanguage(language) {
        if (!KNOWN.has(language)) throw new Error(`unknown ${language}`);
        this.languages.add(language);
      },
      async loadTheme(theme) {
        this.themes.add(theme);
      },
    };
    created.push(highlighter);
    return highlighter;
  };
  return { create, created };
}

function pool(max = 3) {
  const factory = fakeFactory();
  return {
    ...factory,
    pool: createShikiLanguagePool({
      create: factory.create,
      isKnownLanguage: (language) => KNOWN.has(language),
      maxLanguages: max,
    }),
  };
}

test("filling every slot once does not disable highlighting for the session", async () => {
  const { pool: p, created } = pool(3);
  // One message renders three fenced blocks in distinct languages, then scrolls away.
  for (const language of ["lang0", "lang1", "lang2"]) {
    const release = p.acquire(language);
    assert.equal(await p.ensure(language, THEME), true);
    release();
  }
  // A later block in a new language still highlights.
  const release = p.acquire("lang3");
  assert.equal(await p.ensure("lang3", THEME), true);
  assert.equal(p.has("lang3", THEME), true);
  assert.deepEqual(p.loadedLanguages(), ["lang3"]);
  assert.equal(created.length, 2, "the highlighter was rebuilt once");
  assert.equal(created[0].disposed, true, "the old grammars were freed");
  assert.equal(p.current(), created[1]);
  assert.ok(created[1].themes.has(THEME), "loaded themes carry over");
  release();
});

test("languages still on screen survive a rebuild", async () => {
  const { pool: p, created } = pool(3);
  const keep = p.acquire("lang0");
  await p.ensure("lang0", THEME);
  for (const language of ["lang1", "lang2"]) {
    const release = p.acquire(language);
    await p.ensure(language, THEME);
    release();
  }
  const release = p.acquire("lang3");
  assert.equal(await p.ensure("lang3", THEME), true);
  assert.deepEqual(p.loadedLanguages().sort(), ["lang0", "lang3"]);
  assert.ok(created.at(-1).languages.has("lang0"));
  assert.equal(p.has("lang0", THEME), true);
  keep();
  release();
});

test("when every slot is in use, a new language waits for a release", async () => {
  const { pool: p } = pool(2);
  const a = p.acquire("lang0");
  const b = p.acquire("lang1");
  await p.ensure("lang0", THEME);
  await p.ensure("lang1", THEME);
  const c = p.acquire("lang2");
  assert.equal(await p.ensure("lang2", THEME), false);
  let notified = 0;
  p.subscribe(() => {
    notified += 1;
  });
  a();
  assert.equal(notified, 1);
  assert.equal(await p.ensure("lang2", THEME), true);
  assert.deepEqual(p.loadedLanguages().sort(), ["lang1", "lang2"]);
  b();
  c();
});

test("unknown language names never take a slot or force a rebuild", async () => {
  const { pool: p, created } = pool(2);
  for (const language of ["lang0", "lang1"]) {
    const release = p.acquire(language);
    await p.ensure(language, THEME);
    release();
  }
  assert.equal(await p.ensure("not-a-grammar", THEME), false);
  assert.equal(created.length, 1);
  assert.deepEqual(p.loadedLanguages().sort(), ["lang0", "lang1"]);
});

test("a language loaded by another block is reported ready to later blocks", async () => {
  const { pool: p } = pool(3);
  const first = p.acquire("lang5");
  await p.ensure("lang5", THEME);
  const second = p.acquire("lang5");
  assert.equal(p.has("lang5", THEME), true);
  assert.equal(await p.ensure("lang5", THEME), true);
  first();
  second();
});
