import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { between, compareSiblings, isValid } from "./position.ts";

interface Vectors {
  between: { before: string | null; after: string | null; expected: string }[];
  invalid: { before: string | null; after: string | null }[];
}
const vectors: Vectors = JSON.parse(
  readFileSync(new URL("../../testvectors/position.json", import.meta.url), "utf8"),
);

test("shared vectors", () => {
  for (const c of vectors.between) {
    assert.equal(between(c.before, c.after), c.expected, JSON.stringify(c));
  }
});

test("shared invalid vectors are rejected", () => {
  for (const c of vectors.invalid) {
    assert.throws(() => between(c.before, c.after), JSON.stringify(c));
  }
});

test("random insertions keep order", () => {
  const keys: string[] = [];
  let seed = 12345;
  for (let n = 0; n < 2000; n++) {
    seed = (seed * 1103515245 + 12345) & 0x7fffffff;
    const i = seed % (keys.length + 1);
    const k = between(keys[i - 1] ?? null, keys[i] ?? null);
    assert.ok(isValid(k));
    keys.splice(i, 0, k);
  }
  for (let i = 1; i < keys.length; i++) assert.ok(keys[i - 1]! < keys[i]!);
});

test("repeated insertion into the same gap", () => {
  let b = "2";
  for (let n = 0; n < 300; n++) {
    const m = between("1", b);
    assert.ok("1" < m && m < b);
    b = m;
  }
});

test("ties break by id", () => {
  assert.equal(compareSiblings(["a1", "x"], ["a1", "y"]), -1);
  assert.equal(compareSiblings(["a2", "a"], ["a1", "z"]), 1);
});
