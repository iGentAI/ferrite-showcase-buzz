// No `switch` in the browser backend may repeat a `case` label.
//
// A switch takes the FIRST matching label, so a repeated one is dead code that
// compiles silently (TypeScript does not flag it). `handleAppCommand` once
// answered `list_personas` and `list_relay_agents` twice each; the later
// branches, including a whole kind-10100 directory read, never ran, while the
// comments beside them described them as the live implementation.
//
// Parsed with the TypeScript compiler rather than by pattern, so a label that
// appears in a comment or a string elsewhere cannot confuse the check.
import { readdirSync, readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import test from "node:test";
import assert from "node:assert/strict";
import ts from "typescript";

const here = dirname(fileURLToPath(import.meta.url));

/** `[{ line, label, firstLine }]` for every repeated literal case label. */
export function duplicateCaseLabels(fileName, source) {
  const file = ts.createSourceFile(
    fileName,
    source,
    ts.ScriptTarget.Latest,
    true,
    ts.ScriptKind.TS,
  );
  const duplicates = [];
  const visit = (node) => {
    if (ts.isSwitchStatement(node)) {
      const seen = new Map();
      for (const clause of node.caseBlock.clauses) {
        if (!ts.isCaseClause(clause)) continue;
        const expression = clause.expression;
        if (
          !ts.isStringLiteralLike(expression) &&
          !ts.isNumericLiteral(expression)
        ) {
          continue;
        }
        const label = expression.text;
        const line =
          file.getLineAndCharacterOfPosition(clause.getStart(file)).line + 1;
        if (seen.has(label)) {
          duplicates.push({ label, line, firstLine: seen.get(label) });
        } else {
          seen.set(label, line);
        }
      }
    }
    ts.forEachChild(node, visit);
  };
  visit(file);
  return duplicates;
}

test("the detector finds a repeated label and ignores distinct ones", () => {
  const source = [
    "function f(x: string) {",
    "  switch (x) {",
    '    case "a": return 1;',
    '    case "b": return 2;',
    '    case "a": return 3;',
    "  }",
    "  switch (x) {",
    '    case "a": return 4;',
    "  }",
    "}",
  ].join("\n");
  assert.deepEqual(duplicateCaseLabels("sample.ts", source), [
    { label: "a", line: 5, firstLine: 3 },
  ]);
});

test("no browser-backend switch repeats a case label", () => {
  const files = readdirSync(here).filter(
    (name) => name.endsWith(".ts") && !name.endsWith(".d.ts"),
  );
  assert.ok(files.includes("commands.ts"));
  const found = [];
  for (const name of files) {
    const source = readFileSync(resolve(here, name), "utf8");
    for (const duplicate of duplicateCaseLabels(name, source)) {
      found.push(
        `${name}:${duplicate.line} repeats case "${duplicate.label}" (first at line ${duplicate.firstLine})`,
      );
    }
  }
  assert.deepEqual(found, []);
});
