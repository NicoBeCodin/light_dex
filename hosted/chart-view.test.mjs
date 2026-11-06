import test from 'node:test';
import assert from 'node:assert/strict';
import vm from 'node:vm';
import { readFile } from 'node:fs/promises';
import { TradeHistory, buildCandles } from './public/chart-data.mjs';

// Exercise the page controller with a chart API double. In a real empty chart,
// the library can report a two-bar viewport before the first history arrives.
test('first history opens five minutes and sizes the volume pane instead of inheriting an empty-chart zoom', async () => {
  const source = (await readFile(new URL('./public/market-chart.mjs', import.meta.url), 'utf8'))
    .replace(/^import .*;\n/gm, '').replace('export function', 'function');
  const makeElement = () => ({ textContent: '', append() {}, replaceChildren() {}, classList: { toggle() {} }, addEventListener() {}, clientHeight: 420 });
  let range = { from: -.5, to: 1.5 }, render, volumeHeight = 0;
  const scale = {
    getVisibleLogicalRange: () => range,
    getVisibleRange: () => ({ from: 700, to: 1000 }),
    setVisibleLogicalRange: value => { range = value; },
    setVisibleRange() {},
  };
  const series = () => ({ priceScale: () => ({ applyOptions() {} }), setData() {}, applyOptions() {}, createPriceLine: () => ({ applyOptions() {} }), removePriceLine() {} });
  const chart = {
    addSeries: series,
    timeScale: () => scale,
    subscribeCrosshairMove() {},
    panes: () => [{ setStretchFactor() {}, getHeight: () => 400 }, { setStretchFactor() {}, getHeight: () => volumeHeight, setHeight: h => { volumeHeight = h; } }],
    remove() {},
  };
  const now = Math.floor(Date.now() / 1000);
  const context = vm.createContext({
    createChart: () => chart, CandlestickSeries: {}, HistogramSeries: {}, LineStyle: { Dashed: 2 },
    TradeHistory, buildCandles, notice: 'TradingView', document: { createElement: makeElement },
    Date: class extends Date { static now() { return now * 1000; } },
    setInterval() {}, clearInterval() {}, clearTimeout() {}, setTimeout: fn => { render = fn; return 1; },
  });
  vm.runInContext(source + '\nglobalThis.makeChart = createMarketChart;', context);
  const view = context.makeChart(Object.fromEntries(['container', 'legend', 'rangeLabel', 'controls', 'attribution', 'title'].map(key => [key, makeElement()])));
  view.select({ id: 1, title: 'BTC-USD perpetual', baseSymbol: 'BTC', config: { price: { decimals: 1, step: 1 }, quantity: { decimals: 4, step: 1 } } });
  range = { from: -.5, to: 1.5 };
  view.ingest(1, [{ block_sequence: 1, transaction_index: 0, fill_index: 0, market_id: 1, price: 750000, quantity: 1, observed_at: now - 1 }]);
  render();
  assert.ok(range.to - range.from >= 60, 'Initial 5s candles should span the five-minute window');
  assert.ok(volumeHeight >= 55, 'Volume must not stay in a zero-height pane');
  view.destroy();
});
