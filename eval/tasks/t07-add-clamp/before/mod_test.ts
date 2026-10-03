import assert from "node:assert";
import { clamp } from "./mod.ts";

Deno.test("clamp", () => {
  assert.strictEqual(clamp(5, 0, 10), 5);
  assert.strictEqual(clamp(-1, 0, 10), 0);
  assert.strictEqual(clamp(11, 0, 10), 10);
});
