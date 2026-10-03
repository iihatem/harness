import assert from "node:assert";
import { capitalize } from "./mod.ts";

Deno.test("capitalize", () => {
  assert.strictEqual(capitalize("hello"), "Hello");
  assert.strictEqual(capitalize("hELLO"), "HELLO");
  assert.strictEqual(capitalize(""), "");
});
