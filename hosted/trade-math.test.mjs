import test from "node:test";
import assert from "node:assert/strict";
import { sizeFromMargin, closeOrder } from "./public/trade-math.mjs";

const config = {
  price: { decimals: 1, step: 1 },
  quantity: { decimals: 4, step: 1 },
  market_type: { max_leverage: 20 },
};
test("USD margin sizing respects units and rounds down to lots", () => {
  assert.equal(sizeFromMargin(100_000_000, 5, 1_000_000, config), 50); // $500 / $100k = .005 BTC
  assert.equal(
    sizeFromMargin(100_000_000, 5, 999_999, { ...config, quantity: { decimals: 4, step: 10 } }),
    50,
  );
  assert.throws(() => sizeFromMargin(1, 1, 1_000_000, config));
  assert.throws(() => sizeFromMargin(100_000_000, 21, 1_000_000, config));
});
test("closing a long sells reduce-only; closing a short buys with bounded ticks", () => {
  const book = { bids: [{ price: 1001 }], asks: [{ price: 1003 }] };
  assert.deepEqual(closeOrder({ size_atoms: 20 }, book, config, 100), {
    side: "Sell",
    price: 991,
    quantity: 20,
    order_type: "IOC",
    reduce_only: true,
  });
  assert.equal(closeOrder({ size_atoms: -20 }, book, config, 100).price, 1013);
  assert.equal(closeOrder({ size_atoms: -20 }, book, config, 100).side, "Buy");
  assert.throws(() => closeOrder({ size_atoms: 0 }, book, config, 100));
  assert.throws(() => closeOrder({ size_atoms: 20 }, { bids: [] }, config, 100));
  assert.throws(() => closeOrder({ size_atoms: 20 }, book, config, 501));
});
