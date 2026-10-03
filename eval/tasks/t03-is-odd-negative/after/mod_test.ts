import assert from "node:assert";
import { isOdd } from "./mod.ts";

Deno.test("isOdd", () => {
  assert.strictEqual(isOdd(7), true);
  assert.strictEqual(isOdd(4), false);
  assert.strictEqual(isOdd(-3), true);
  assert.strictEqual(isOdd(-2), false);
});
