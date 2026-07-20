import assert from "node:assert/strict";
import test from "node:test";

import { total } from "../src/cart.ts";

test("totals line items", () => {
  assert.equal(
    total([
      { price: 5, quantity: 2 },
      { price: 3, quantity: 1 },
    ]),
    13,
  );
});

test("empty cart has zero total", () => {
  assert.equal(total([]), 0);
});
