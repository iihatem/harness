import assert from "node:assert";
import { range } from "./mod.ts";

Deno.test("range is inclusive", () => {
  assert.deepStrictEqual(range(1, 3), [1, 2, 3]);
  assert.deepStrictEqual(range(2, 2), [2]);
});
