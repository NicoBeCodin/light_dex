import { createChart, CandlestickSeries, HistogramSeries, LineStyle } from './lightweight-charts-5.2.1.mjs';
import { TradeHistory, buildCandles } from './chart-data.mjs';
import { notice } from './chart-attribution.mjs';

export function createMarketChart({ container, legend, rangeLabel, controls, attribution }) {
  attribution.textContent = notice.trim();
  container.replaceChildren();
  const history = new TradeHistory();
  const chart = createChart(container, {
    autoSize: true,
    layout: { background: { color: '#101317' }, textColor: '#9299a3', fontSize: 11, attributionLogo: false },
    grid: { vertLines: { color: '#191e24' }, horzLines: { color: '#20262d' } },
    rightPriceScale: { borderColor: '#24282e', scaleMargins: { top: 0.12, bottom: 0.26 } },
    timeScale: { borderColor: '#24282e', timeVisible: true, secondsVisible: true, rightOffset: 2, shiftVisibleRangeOnNewBar: true },
    crosshair: { mode: 0, vertLine: { color: '#697784' }, horzLine: { color: '#697784' } },
    localization: { locale: 'en-US' },
  });
  const candles = chart.addSeries(CandlestickSeries, {
    upColor: '#54d5b0', downColor: '#f07988', borderVisible: false,
    wickUpColor: '#54d5b0', wickDownColor: '#f07988',
  });
  const volume = chart.addSeries(HistogramSeries, { priceFormat: { type: 'volume' }, priceScaleId: '', lastValueVisible: false, priceLineVisible: false });
  volume.priceScale().applyOptions({ scaleMargins: { top: 0.8, bottom: 0 } });
  let market, interval = 5, snapshot, markLine, latest, timer, signature, fit = true;
  const views = new Map();
  const key = () => `${market?.id}:${interval}`;
  const format = value => value.toLocaleString('en-US', { minimumFractionDigits: market.config.price.decimals, maximumFractionDigits: market.config.price.decimals });
  function showLegend(bar, size) {
    legend.textContent = bar?.open !== undefined
      ? `O ${format(bar.open)}  H ${format(bar.high)}  L ${format(bar.low)}  C ${format(bar.close)}  Vol ${(size || 0).toLocaleString('en-US', { maximumFractionDigits: market.config.quantity.decimals })}`
      : 'Waiting for committed trades';
  }
  chart.subscribeCrosshairMove(param => {
    if (!market) return;
    const bar = param.seriesData.get(candles), size = param.seriesData.get(volume);
    if (param.time && bar?.open !== undefined) showLegend(bar, size?.value);
    else showLegend(latest, latest?.volume);
  });
  function saveView() {
    if (!market) return;
    const range = chart.timeScale().getVisibleLogicalRange();
    const end = Math.floor(Date.now() / 1000 / interval) - Math.floor((Date.now() / 1000 - 300) / interval);
    views.set(key(), { logical: range, range: chart.timeScale().getVisibleRange(), follow: !range || range.to >= end - 2 });
  }
  function render() {
    timer = undefined;
    if (!market) return;
    const now = Date.now() / 1000;
    const entry = history.ingest(market.id, [], now);
    const next = `${market.id}:${interval}:${entry.revision}:${Math.floor(now / interval)}`;
    if (signature !== next || fit) {
      if (!fit) saveView();
      const data = buildCandles(entry.trades.values(), market.config, interval, now);
      candles.setData(data.candles);
      volume.setData(data.volume);
      latest = data.last;
      showLegend(latest, latest?.volume);
      rangeLabel.textContent = `${data.count.toLocaleString()} fills · 5 min retained · UTC`;
      const view = views.get(key());
      if (view?.range && !view.follow) chart.timeScale().setVisibleRange(view.range);
      else if (view?.logical) {
        const to = data.candles.length + 1;
        chart.timeScale().setVisibleLogicalRange({ from: to - (view.logical.to - view.logical.from), to });
      } else chart.timeScale().fitContent();
      signature = next;
      fit = false;
    }
    const mark = snapshot?.committed_mark_e8 / 1e8;
    if (Number.isFinite(mark) && mark > 0) {
      if (markLine) markLine.applyOptions({ price: mark });
      else markLine = candles.createPriceLine({ price: mark, color: '#92aaf8', lineWidth: 1, lineStyle: LineStyle.Dashed, axisLabelVisible: true, title: 'Mark' });
    } else if (markLine) { candles.removePriceLine(markLine); markLine = undefined; }
  }
  function schedule() { if (!timer) timer = setTimeout(render, 100); }
  controls.addEventListener('click', event => {
    const button = event.target.closest('button');
    if (!button) return;
    if (button.dataset.interval) {
      saveView(); interval = Number(button.dataset.interval); fit = true;
      controls.querySelectorAll('[data-interval]').forEach(b => {
        const active = Number(b.dataset.interval) === interval;
        b.classList.toggle('selected', active); b.setAttribute('aria-pressed', String(active));
      });
    } else if (button.dataset.chartReset !== undefined) { views.delete(key()); fit = true; }
    schedule();
  });
  // Age out old data even when the market is quiet; never poll another node.
  const clock = setInterval(schedule, 1000);
  return {
    select(value) {
      saveView(); market = value; snapshot = undefined; latest = undefined; signature = undefined; fit = true;
      candles.applyOptions({ priceFormat: { type: 'price', precision: value.config.price.decimals, minMove: value.config.price.step / 10 ** value.config.price.decimals } });
      if (markLine) { candles.removePriceLine(markLine); markLine = undefined; }
      render();
    },
    ingest(id, trades) { history.ingest(id, trades); if (market?.id === id) schedule(); },
    update(value) { snapshot = value; history.ingest(value.market_id, value.recent_trades); schedule(); },
    destroy() { clearInterval(clock); clearTimeout(timer); chart.remove(); },
  };
}
