import test from "node:test";
import assert from "node:assert/strict";
import vm from "node:vm";
import { readFile } from "node:fs/promises";

// Exercise the page's real confirmation loop with controlled network outcomes.
// No wallet, faucet claim or validator is touched by this test.
const source = (await readFile(new URL("./public/app.js", import.meta.url), "utf8"))
  .replace(/^import .*;\n/gm, "")
  .replaceAll("import.meta.url", JSON.stringify("http://localhost/app.js"))
  .replace(
    /boot\(\);\s*$/,
    `
    meta={chain_id:"test",writes_enabled:true,markets:[]};
    wallet={address:"test-address"}; account={nonce:0,risk:[],positions:[],open_orders:[]};
    accountTab="positions";
    globalThis.confirmTransaction=track;
  `,
  );

async function fixture(outcome) {
  const storage = new Map([["ldex-pending:test:test-address", "signed"]]);
  const elements = new Map();
  const updates = [];
  const element = () => ({
    hidden: false,
    value: "",
    textContent: "",
    innerHTML: "",
    children: Array.from({ length: 4 }, () => ({ classList: { add() {} } })),
    addEventListener() {},
    querySelector() {
      return null;
    },
  });
  const context = vm.createContext({
    document: {
      getElementById(id) {
        if (!elements.has(id)) elements.set(id, element());
        return elements.get(id);
      },
      querySelectorAll() {
        return [];
      },
      addEventListener() {},
    },
    localStorage: { getItem: (k) => storage.get(k) || null, removeItem: (k) => storage.delete(k) },
    performance: { now: () => 1000 },
    setTimeout: (fn) => fn(),
    transactionNotice: () => ({ update: (...value) => updates.push(value) }),
    fetch: async (path) => {
      if (outcome === "offline") throw Error("Network unavailable");
      const body =
        path === "/api/v1/transactions"
          ? { accepted: true, digest: "abcd" }
          : path === "/api/v1/transactions/abcd"
            ? {
                status: "committed",
                receipt: {
                  success: outcome,
                  error: outcome ? null : "InsufficientBalance",
                  digest: "abcd",
                },
              }
            : { nonce: outcome ? 1 : 0, positions: [], risk: [], open_orders: [] };
      return { ok: true, json: async () => body };
    },
  });
  vm.runInContext(source, context);
  return {
    run: () => context.confirmTransaction({ transaction: { PlaceOrder: { nonce: 0 } } }, 0),
    storage,
    updates,
  };
}
test("successful confirmation clears pending state and reports success", async () => {
  const f = await fixture(true);
  await f.run();
  assert.equal(f.storage.size, 0);
  assert.equal(f.updates.at(-1)[0], "success");
  assert.equal(f.updates.at(-1)[2], "abcd");
});
test("failed execution reports the actual error and releases the pending request", async () => {
  const f = await fixture(false);
  await assert.rejects(f.run(), /InsufficientBalance/);
  assert.equal(f.storage.size, 0);
  assert.equal(f.updates.at(-1)[0], "error");
  assert.equal(f.updates.at(-1)[1], "InsufficientBalance");
});
test("an ambiguous network failure preserves the signed request and never claims execution failed", async () => {
  const f = await fixture("offline");
  await assert.rejects(f.run(), /still pending/);
  assert.equal(f.storage.size, 1);
  assert.equal(f.updates.at(-1)[0], "waiting");
});
