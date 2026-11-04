// End-to-end API test of the same WASM encoder and Ed25519 used by the page.
import { readFile, writeFile, mkdir } from "node:fs/promises";
import { webcrypto } from "node:crypto";
const config = JSON.parse(await readFile(new URL("./.local/deployment.json", import.meta.url)));
const origin = config.origin;
const get = async (path) => {
  const response = await fetch(origin + "/api/v1/" + path);
  if (!response.ok) throw Error(path + " " + response.status + " " + (await response.text()));
  return response.json();
};
const meta = await get("meta");
const pair = await webcrypto.subtle.generateKey("Ed25519", true, ["sign", "verify"]);
const hex = (b) => Buffer.from(b).toString("hex");
const address = hex(await webcrypto.subtle.exportKey("raw", pair.publicKey));
const {
  instance: { exports: wasm },
} = await WebAssembly.instantiate(await readFile(new URL("./public/codec.wasm", import.meta.url)));
async function sign(transaction) {
  const input = new TextEncoder().encode(
    JSON.stringify({ chain: [...Buffer.from(meta.chain_id, "hex")], transaction }),
  );
  const p = wasm.allocate(input.length);
  new Uint8Array(wasm.memory.buffer, p, input.length).set(input);
  if (wasm.encode(p, input.length) !== 1) throw Error("WASM rejected");
  const bytes = new Uint8Array(wasm.memory.buffer, wasm.output_ptr(), wasm.output_len()).slice();
  wasm.release(p, input.length);
  return {
    transaction,
    signature: hex(await webcrypto.subtle.sign("Ed25519", pair.privateKey, bytes)),
  };
}
const results = [];
let nonce = 0;
async function send(kind, fields) {
  const signed = await sign({ [kind]: { account_id: address, ...fields, nonce } });
  const started = performance.now();
  const response = await fetch(origin + "/api/v1/transactions", {
    method: "POST",
    headers: { "Content-Type": "application/json", Origin: origin },
    body: JSON.stringify(signed),
  });
  const admission = await response.json();
  if (!response.ok || !admission.accepted) throw Error("Admission " + JSON.stringify(admission));
  for (let n = 0; n < 80; n++) {
    const result = await get("transactions/" + admission.digest);
    if (result.status === "committed") {
      results.push({
        kind,
        success: result.receipt.success,
        error: result.receipt.error,
        latency_ms: Math.round(performance.now() - started),
        digest: admission.digest,
        block: result.receipt.block_sequence,
      });
      console.log(JSON.stringify(results.at(-1)));
      if (result.receipt.success !== true)
        throw Error("Execution failed " + JSON.stringify(result));
      nonce++;
      return result;
    }
    await new Promise((r) => setTimeout(r, 250));
  }
  throw Error("Confirmation timed out");
}
await send("ClaimAirdrop", {});
let account = await get("rpc/accounts/" + address);
if (account.nonce !== 1 || account.balances.length < 6) throw Error("Faucet balances missing");
const market = await get("rpc/markets/0");
const mark = Number(market.committed_mark_e8) / 1e8;
const bid = Math.floor(mark * 0.9 * 10);
await send("PlaceOrder", {
  market_id: 0,
  side: "Buy",
  order_type: "PostOnly",
  price: bid,
  quantity: 1,
});
account = await get("rpc/accounts/" + address);
const order = account.open_orders.find((o) => o.market_id === 0);
if (!order) throw Error("Resting order missing");
await send("CancelOrder", { market_id: 0, order_id: order.order_id });
await send("SetMarginMode", { mode: "Cross" });
const perp = meta.markets.find(
  (m) => m.config.base_asset === 1 && m.config.market_type.kind === "perp",
);
await send("DepositPerpCollateral", { market_id: perp.id, amount: 1000 * 1000000 });
await send("PlacePerpOrder", {
  market_id: perp.id,
  side: "Buy",
  order_type: "PostOnly",
  price: bid,
  quantity: 1,
  reduce_only: false,
});
account = await get("rpc/accounts/" + address);
const perpOrder = account.open_orders.find((o) => o.market_id === perp.id);
if (!perpOrder) throw Error("Perp order missing");
await send("CancelOrder", { market_id: perp.id, order_id: perpOrder.order_id });
await send("WithdrawPerpCollateral", { market_id: perp.id, amount: 100 * 1000000 });
for (const item of results) {
  const block = await get("rpc/explorer/blocks/" + item.block);
  if (block.sequence !== item.block && block.block?.sequence !== item.block)
    console.log("Block keys", Object.keys(block));
}
const invalid = await sign({
  Transfer: { from: address, to: address, asset: 0, amount: 1, nonce },
});
invalid.signature = "00".repeat(64);
const bad = await fetch(origin + "/api/v1/transactions", {
  method: "POST",
  headers: { "Content-Type": "application/json" },
  body: JSON.stringify(invalid),
});
if (bad.status !== 400) throw Error("Bad signature accepted " + bad.status);
for (const path of ["launch", "stop_all", "kill_node"]) {
  const r = await fetch(origin + "/api/v1/rpc/" + path);
  if (r.status !== 404) throw Error("Operator route exposed");
}
await mkdir(new URL("./.local/", import.meta.url), { recursive: true });
await writeFile(
  new URL("./.local/smoke.json", import.meta.url),
  JSON.stringify(
    {
      at: new Date().toISOString(),
      chain_id: meta.chain_id,
      results,
      bad_signature_status: bad.status,
      operator_routes: "404",
    },
    null,
    2,
  ),
);
console.log(
  "PASS: funded wallet, spot placement/cancel, cross collateral, perp placement/cancel/withdraw, explorer, invalid signatures and operator isolation",
);
