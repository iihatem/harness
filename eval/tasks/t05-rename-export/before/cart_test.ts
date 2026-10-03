import assert from "node:assert";
import { total } from "./cart.ts";
import { formatPrice } from "./price.ts";

Deno.test("prices", () => {
  assert.strictEqual(formatPrice(250), "$2.50");
  assert.strictEqual(total([100, 250]), "$3.50");
});
