import assert from "node:assert";
import { getName } from "./mod.ts";

Deno.test("getName", () => {
  assert.strictEqual(getName({ name: "ann" }), "ann");
  assert.strictEqual(getName(undefined), "anonymous");
  assert.strictEqual(getName({}), "anonymous");
});
