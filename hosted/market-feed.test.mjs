import test from "node:test";
import assert from "node:assert/strict";
import { subscribeMarket } from "./public/market-feed.mjs";

function setup(read) {
  let tick,
    now = 1000;
  const sockets = [],
    data = [],
    states = [];
  const clock = {
    Date: { now: () => now },
    setInterval(fn) {
      tick = fn;
      return 1;
    },
    clearInterval() {},
    setTimeout() {
      return 2;
    },
    clearTimeout() {},
  };
  const feed = subscribeMarket({
    id: 0,
    read,
    clock,
    connect() {
      const s = { close() {} };
      sockets.push(s);
      return s;
    },
    onData: (v) => data.push(v),
    onStatus: (v) => states.push(v),
  });
  return {
    feed,
    sockets,
    data,
    states,
    tick: () => {
      now += 6000;
      tick();
    },
  };
}
const snapshot = (height) => ({ market_id: 0, height, book: { bids: [], asks: [] } });
test("a silent initial stream still renders HTTP market data", async () => {
  const f = setup(async () => snapshot(10));
  await f.feed.ready;
  assert.equal(f.data[0].height, 10);
  assert.equal(f.states.at(-1), "snapshot");
  f.feed.stop();
});
test("older HTTP data and responses from an abandoned market cannot overwrite the stream", async () => {
  let reply;
  const f = setup(() => new Promise((r) => (reply = r)));
  f.sockets[0].onmessage({ data: JSON.stringify(snapshot(20)) });
  reply(snapshot(19));
  await f.feed.ready;
  assert.deepEqual(
    f.data.map((v) => v.height),
    [20],
  );
  f.feed.stop();
  const g = setup(() => new Promise((r) => (reply = r)));
  g.feed.stop();
  reply(snapshot(21));
  await g.feed.ready;
  assert.equal(g.data.length, 0);
});
test("initial failure is visible and retries recover without switching markets", async () => {
  let calls = 0;
  const f = setup(async () => {
    if (++calls === 1) throw Error("Temporary failure");
    return snapshot(30);
  });
  await f.feed.ready;
  assert.equal(f.states.at(-1), "unavailable");
  f.tick();
  await new Promise((r) => setImmediate(r));
  assert.equal(f.data.at(-1).height, 30);
  assert.equal(f.states.at(-1), "snapshot");
  f.feed.stop();
});
