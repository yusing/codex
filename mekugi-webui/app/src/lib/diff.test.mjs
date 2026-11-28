import assert from "node:assert/strict";
import { test } from "node:test";
import { diffRows, diffTokens, diffWords, matchReviewedFile } from "./diff.ts";
import { diffSyntax } from "./highlight.ts";

const changed = (rows) => {
  const words = diffWords(rows);
  return rows.map((row, i) =>
    (words.get(i) ?? []).map(({ start, end }) => row.text.slice(start, end)).join(""),
  );
};

test("replacement blocks retain words across rows and emphasize identifiers and operators", () => {
  const rows = diffRows(
    "@@ -1,2 +1,3 @@\n-const old_name = 1;\n-return old_name;\n+const new_name = 1;\n+// inserted\n+return new_name + 1;\n",
  );
  assert.deepEqual(changed(rows), [
    "",
    "old_name",
    "old_name",
    "new_name",
    "// inserted",
    "new_name + 1",
  ]);
  assert.deepEqual(changed(diffRows("@@ -1 +1 @@\n-a <= b\n+a >= b\n")), ["", "<", ">"]);
});

test("provisional hunks preserve patch-like text without guessing line numbers", () => {
  assert.deepEqual(
    diffRows("--- a/file\n+++ b/file\n@@\n--- old\n+++ new\n@@ anchor\n context\n").map(
      ({ kind, text, old, new: next }) => [kind, text, old, next],
    ),
    [
      ["hunk", "@@", undefined, undefined],
      ["remove", "-- old", undefined, undefined],
      ["add", "++ new", undefined, undefined],
      ["hunk", "@@ anchor", undefined, undefined],
      ["context", "context", undefined, undefined],
    ],
  );
});

test("Unicode words, whitespace, graphemes, notes and separate hunks preserve source", () => {
  const rows = diffRows(
    "@@ -1 +1 @@\n-\tcafé 👩‍💻 ok  \n\\ No newline at end of file\n+\tcafè 👨‍💻 ok \n@@ -8 +9 @@\n-pure removal\n@@ -12,0 +13 @@\n+pure addition\n",
  );
  assert.deepEqual(changed(rows), ["", "café 👩‍💻  ", "", "cafè 👨‍💻 ", "", "", "", ""]);
  for (const [i, spans] of diffWords(rows)) {
    const tokens = diffTokens(rows[i].text, undefined, spans);
    assert.equal(tokens.map((token) => token.text).join(""), rows[i].text);
  }
});

test("diff rows omit file headers without hiding source or capture notes", () => {
  assert.deepEqual(
    diffRows(
      "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\nPartial capture\n@@ -1 +1 @@\n-old\n+diff --git source text\n",
    ),
    [
      { kind: "note", text: "Partial capture" },
      { kind: "hunk", text: "@@ -1 +1 @@" },
      { kind: "remove", text: "old", old: 1 },
      { kind: "add", text: "diff --git source text", new: 1 },
    ],
  );
});

test("large replacement blocks keep row-only fills and leave smaller later blocks usable", () => {
  for (const source of ["x".repeat(65536), "a + ".repeat(520)]) {
    const rows = diffRows(`@@ -1,3 +1,3 @@\n-${source}\n+changed\n context\n-old\n+new\n`);
    assert.equal(diffWords(rows).has(1), false);
    assert.equal(diffWords(rows).has(2), false);
    assert.deepEqual(changed(rows).slice(-2), ["old", "new"]);
  }
});

test("small repeated-token blocks exhaust matching work without losing syntax or later words", async () => {
  const before = `const xs = [${"0, ".repeat(330)}1];`;
  const after = `const xs = [${"0, ".repeat(330)}2];`;
  const rows = diffRows(`@@ -1,3 +1,3 @@\n-${before}\n+${after}\n context\n-old\n+new\n`);
  const words = diffWords(rows);
  assert.equal(words.has(1), false);
  assert.equal(words.has(2), false);
  assert.deepEqual(changed(rows).slice(-2), ["old", "new"]);
  const syntax = await diffSyntax(rows, "file.ts", "file.ts");
  assert.ok(syntax.get(2).some((token) => token.style));
  assert.equal(
    diffTokens(after, syntax.get(2), words.get(2))
      .map((token) => token.text)
      .join(""),
    after,
  );
});

test("syntax keeps separate old/new multiline states, line numbers, words and escaped text", async () => {
  const rows = diffRows(
    '@@ -10,3 +10,3 @@\n-/* removed opener\n+const value = "<script>";\n shared();\n-*/\n+return value;\n@@ -30 +40 @@\n-const old = 1;\n+const fresh = 2;\n',
  );
  const syntax = await diffSyntax(rows, "before.ts", "after.ts");
  const words = diffWords(rows);
  for (const [i, line] of syntax) {
    assert.equal(line.map((token) => token.text).join(""), rows[i].text);
    assert.ok(line.some((token) => token.style?.includes("--shiki-light")));
    const painted = diffTokens(rows[i].text, line, words.get(i));
    assert.equal(painted.map((token) => token.text).join(""), rows[i].text);
    for (const token of painted.filter((part) => part.changed)) assert.ok(token.style);
  }
  assert.equal(syntax.size, 7);
  assert.notEqual(syntax.get(3)[0].style, syntax.get(1)[0].style);
  assert.equal(rows.at(-1).new, 40);
});

test("unknown, oversized and CRLF-normalized syntax falls back without losing word emphasis", async () => {
  const rows = diffRows("@@ -1 +1 @@\n-old <tag>\n+new <tag>\n");
  assert.equal((await diffSyntax(rows, "file.unknown", "file.unknown")).size, 0);
  assert.deepEqual(changed(rows), ["", "old", "new"]);
  const oversized = diffRows(`@@ -1 +1 @@\n-${"x".repeat(65536)}\n+const x = 1;\n`);
  assert.equal((await diffSyntax(oversized, "file.ts", "file.ts")).size, 0);
  const crlf = diffRows("@@ -1,2 +1,2 @@\n-const old = 1;\r\n+const next = 2;\r\n context\r\n");
  const syntax = await diffSyntax(crlf, "file.ts", "file.ts");
  for (const [i, line] of syntax)
    assert.equal(line.map((token) => token.text).join(""), crlf[i].text);
});

test("matchReviewedFile prefers exact workspace-relative matches over earlier suffix matches", () => {
  const files = [
    { key: "0", path: "/workspace/docs/README.md" },
    { key: "1", path: "/workspace/README.md" },
  ];
  assert.equal(matchReviewedFile(files, "README.md", "/workspace")?.key, "1");
  assert.equal(matchReviewedFile(files, "./README.md", "/workspace")?.key, "1");
  assert.equal(matchReviewedFile(files, "docs/README.md", "/workspace")?.key, "0");
  // Unambiguous suffix matches when workspace root is absent.
  assert.equal(matchReviewedFile(files, "docs/README.md", "")?.key, "0");
  // Ambiguous suffix returns undefined rather than picking arbitrarily.
  assert.equal(matchReviewedFile(files, "README.md", ""), undefined);

  // Native direction: absolute patch path matched against workspace-relative saved path.
  const relFiles = [
    { key: "0", path: "docs/README.md" },
    { key: "1", path: "src/app.ts" },
  ];
  assert.equal(matchReviewedFile(relFiles, "/workspace/src/app.ts", "/workspace")?.key, "1");
  // Unmatched external absolute path must not fall back to a suffix match.
  assert.equal(matchReviewedFile(relFiles, "/other/src/app.ts", "/workspace"), undefined);
});
