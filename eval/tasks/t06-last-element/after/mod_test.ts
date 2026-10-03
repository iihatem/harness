import assert from "node:assert";
import { last } from "./mod.ts";

Deno.test("last", () => {
  assert.strictEqual(last([1, 2, 3]), 3);
  assert.strictEqual(last([]), undefined);
});
