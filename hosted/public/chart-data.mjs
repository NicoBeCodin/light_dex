// Trade identity prevents history/snapshot overlap from double-counting volume.
// Time is gateway observation time, not a consensus-authenticated timestamp.
export class TradeHistory {
  constructor({ seconds = 300, maxTrades = 10000 } = {}) {
    this.seconds = seconds;
    this.maxTrades = maxTrades;
    this.markets = new Map();
  }
  entry(id) {
    if (!this.markets.has(id)) this.markets.set(id, { trades: new Map(), seen: new Map(), revision: 0 });
    return this.markets.get(id);
  }
  ingest(id, trades = [], now = Date.now() / 1000) {
    const entry = this.entry(id);
    for (const trade of trades) {
      if (trade.market_id !== undefined && trade.market_id !== id) continue;
      if (![trade.block_sequence, trade.transaction_index, trade.fill_index].every(Number.isSafeInteger)) continue;
      if (!Number.isSafeInteger(trade.price) || trade.price <= 0 || !Number.isSafeInteger(trade.quantity) || trade.quantity <= 0) continue;
      const key = `${trade.block_sequence}:${trade.transaction_index}:${trade.fill_index}`;
      const previous = entry.trades.get(key);
      const observed = Number.isFinite(trade.observed_at) && trade.observed_at > 0;
      const at = observed ? trade.observed_at : previous?.at ?? entry.seen.get(key) ?? now;
      entry.seen.set(key, at);
      if (at < now - this.seconds || at > now + 5) continue;
      if (!previous || (observed && previous.at !== at)) {
        entry.trades.set(key, {
          block_sequence: trade.block_sequence, transaction_index: trade.transaction_index,
          fill_index: trade.fill_index, price: trade.price, quantity: trade.quantity, at,
        });
        entry.revision++;
      }
    }
    for (const [key, trade] of entry.trades) {
      if (trade.at < now - this.seconds) { entry.trades.delete(key); entry.revision++; }
    }
    if (entry.trades.size > this.maxTrades) {
      const oldest = [...entry.trades.entries()].sort((a, b) => a[1].at - b[1].at);
      for (const [key] of oldest.slice(0, entry.trades.size - this.maxTrades)) entry.trades.delete(key);
      entry.revision++;
    }
    while (entry.seen.size > this.maxTrades * 2) entry.seen.delete(entry.seen.keys().next().value);
    return entry;
  }
}

export function buildCandles(trades, config, interval, now = Date.now() / 1000, seconds = 300) {
  if (![1, 5, 15, 60].includes(interval)) throw new Error('Unsupported candle interval');
  const sorted = [...trades].filter(t => t.at >= now - seconds && t.at <= now + 5)
    .sort((a, b) => a.at - b.at || a.block_sequence - b.block_sequence || a.transaction_index - b.transaction_index || a.fill_index - b.fill_index);
  const buckets = new Map();
  const priceScale = 10 ** config.price.decimals, quantityScale = 10 ** config.quantity.decimals;
  for (const trade of sorted) {
    const time = Math.floor(trade.at / interval) * interval;
    const price = trade.price / priceScale, quantity = trade.quantity / quantityScale;
    let bar = buckets.get(time);
    if (!bar) { bar = { time, open: price, high: price, low: price, close: price, volume: 0 }; buckets.set(time, bar); }
    bar.high = Math.max(bar.high, price);
    bar.low = Math.min(bar.low, price);
    bar.close = price;
    bar.volume += quantity;
  }
  const candles = [], volume = [];
  // Whitespace preserves the time axis across periods without trades; no fake OHLC.
  const start = Math.floor((now - seconds) / interval) * interval;
  const end = Math.floor(now / interval) * interval;
  for (let time = start; time <= end; time += interval) {
    const bar = buckets.get(time);
    if (bar) {
      const { volume: value, ...candle } = bar;
      candles.push(candle);
      volume.push({ time, value, color: bar.close >= bar.open ? '#54d5b080' : '#f0798880' });
    } else { candles.push({ time }); volume.push({ time }); }
  }
  return { candles, volume, last: [...buckets.values()].at(-1), count: sorted.length };
}
