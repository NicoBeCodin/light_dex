import test from 'node:test';
import assert from 'node:assert/strict';
import { TradeHistory, buildCandles } from './public/chart-data.mjs';

const config = { price: { decimals: 1, step: 1 }, quantity: { decimals: 4, step: 1 } };
const trade = (sequence, price, quantity, time, extra = {}) => ({ market_id: 1, block_sequence: sequence, transaction_index: 0, fill_index: 0, price, quantity, ...(time === undefined ? {} : { observed_at: time }), ...extra });

test('history and live overlap count a fill once, while separate fills in one transaction count separately', () => {
  const history = new TradeHistory();
  const t = trade(1, 750001, 1, 1001);
  history.ingest(1, [t], 1010);
  history.ingest(1, [t, { ...t, fill_index: 1, quantity: 2 }], 1011);
  const bars = buildCandles(history.entry(1).trades.values(), config, 5, 1011);
  assert.equal(bars.count, 2);
  assert.equal(bars.last.open, 75000.1);
  assert.ok(Math.abs(bars.last.volume - 0.0003) < 1e-12);
});

test('out-of-order snapshots use execution order for OHLC within a shared observation timestamp', () => {
  const history = new TradeHistory();
  history.ingest(1, [trade(3, 750010, 2, 1002), trade(1, 750000, 1, 1002), trade(2, 749990, 3, 1002)], 1010);
  const bars = buildCandles(history.entry(1).trades.values(), config, 5, 1010);
  assert.deepEqual(bars.candles.find(b => b.time === 1000), { time: 1000, open: 75000, high: 75001, low: 74999, close: 75001 });
  assert.ok(Math.abs(bars.last.volume - 0.0006) < 1e-12);
});

test('quiet-market snapshots do not resurrect expired fills with new timestamps', () => {
  const history = new TradeHistory();
  const t = trade(1, 750000, 1);
  history.ingest(1, [t], 1000);
  history.ingest(1, [t], 1301);
  history.ingest(1, [t], 1302);
  assert.equal(history.entry(1).trades.size, 0);
});

test('late history corrects observation time and can add an earlier candle without double volume', () => {
  const history = new TradeHistory();
  history.ingest(1, [trade(1, 750000, 1)], 1012);
  history.ingest(1, [trade(1, 750000, 1, 1001)], 1013);
  const data = buildCandles(history.entry(1).trades.values(), config, 5, 1013);
  assert.equal(data.count, 1);
  assert.equal(data.last.time, 1000);
});

test('markets retain separate histories, invalid data is ignored, and retention stays bounded', () => {
  const history = new TradeHistory({ maxTrades: 2 });
  history.ingest(1, [trade(1, 1, 1, 1001), trade(2, 2, 1, 1002), trade(3, 3, 1, 1003), trade(4, 0, 1, 1004), trade(5, 5, 1, 1005, { market_id: 9 })], 1010);
  history.ingest(9, [trade(1, 90, 1, 1001, { market_id: 9 })], 1010);
  assert.equal(history.entry(1).trades.size, 2);
  assert.equal(history.entry(9).trades.size, 1);
  assert.equal(history.entry(9).trades.values().next().value.price, 90);
});

test('empty buckets stay whitespace and interval changes preserve actual traded quantity', () => {
  const trades = [{ ...trade(1, 750000, 10000, 1001), at: 1001 }, { ...trade(2, 750010, 20000, 1016), at: 1016 }];
  for (const interval of [1, 5, 15, 60]) {
    const data = buildCandles(trades, config, interval, 1020);
    assert.equal(data.volume.reduce((sum, bar) => sum + (bar.value || 0), 0), 3);
    assert.ok(data.candles.some(bar => bar.open === undefined));
    assert.ok(data.candles.every((bar, i, all) => i === 0 || bar.time > all[i - 1].time));
  }
  assert.throws(() => buildCandles([], config, 3, 1020));
});
