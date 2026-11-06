import { createChart, CandlestickSeries, HistogramSeries, LineStyle } from './lightweight-charts-5.2.1.mjs';
import { TradeHistory, buildCandles } from './chart-data.mjs';
import { notice } from './chart-attribution.mjs';

export function createMarketChart({ container, legend, rangeLabel, controls, attribution, title }) {
  attribution.textContent = notice.trim();
  const legendValues = new Map();
  for (const field of ['O', 'H', 'L', 'C', 'Vol']) {
    const item = document.createElement('span');
    const label = document.createElement('small');
    const value = document.createElement('strong');
    label.textContent = field; value.textContent = '-';
    item.append(label, value); legendValues.set(field, value);
    legend.append(item);
  }
  container.replaceChildren();
  const history = new TradeHistory();
  const chart = createChart(container, {
    autoSize: true,
    layout: { background: { color: '#101317' }, textColor: '#9299a3', fontFamily: 'Arial, Helvetica, sans-serif', fontSize: 11, attributionLogo: false, panes: { separatorColor: '#24282e', separatorHoverColor: '#36434c' } },
    grid: { vertLines: { visible: false }, horzLines: { color: '#1b2026' } },
    rightPriceScale: { borderVisible: false, minimumWidth: 76, scaleMargins: { top: 0.14, bottom: 0.1 } },
    timeScale: {
      borderVisible: false, timeVisible: true, secondsVisible: true, rightOffset: 3,
      ticksVisible: false, minimumHeight: 28, tickMarkMaxCharacterLength: 10,
      shiftVisibleRangeOnNewBar: true,
      tickMarkFormatter: time => typeof time === 'number' ? new Date(time * 1000).toISOString().slice(11, 19) : '',
    },
    crosshair: { mode: 0, vertLine: { color: '#52616d', labelBackgroundColor: '#29343e' }, horzLine: { color: '#52616d', labelBackgroundColor: '#29343e' } },
    localization: { locale: 'en-US' },
  });
  const candles = chart.addSeries(CandlestickSeries, {
    upColor: '#54d5b0', downColor: '#f07988', borderVisible: false,
    wickUpColor: '#54d5b0', wickDownColor: '#f07988', priceLineStyle: LineStyle.Dashed,
  });
  const volume = chart.addSeries(HistogramSeries, { priceFormat: { type: 'volume' }, lastValueVisible: false, priceLineVisible: false }, 1);
  volume.priceScale().applyOptions({ scaleMargins: { top: 0.15, bottom: 0 } });
  chart.panes()[0].setStretchFactor(5000);
  chart.panes()[1].setStretchFactor(1000);
  let market, interval = 5, snapshot, markLine, latest, timer, signature, fit = true;
  const views = new Map();
  const key = () => `${market?.id}:${interval}`;
  const format = value => value.toLocaleString('en-US', { minimumFractionDigits: market.config.price.decimals, maximumFractionDigits: market.config.price.decimals });
  function showLegend(bar, size) {
    const values = bar?.open !== undefined
      ? [format(bar.open), format(bar.high), format(bar.low), format(bar.close), `${(size || 0).toLocaleString('en-US', { maximumFractionDigits: market.config.quantity.decimals })} ${market.baseSymbol}`]
      : ['-', '-', '-', '-', '-'];
    let index = 0;
    for (const value of legendValues.values()) value.textContent = values[index++];
    legend.classList.toggle('is-up', !!bar && bar.close >= bar.open);
    legend.classList.toggle('is-down', !!bar && bar.close < bar.open);
  }
  chart.subscribeCrosshairMove(param => {
    if (!market) return;
    const bar = param.seriesData.get(candles), size = param.seriesData.get(volume);
    if (param.time && bar?.open !== undefined) showLegend(bar, size?.value);
    else showLegend(latest, latest?.volume);
  });
  function saveView() {
    if (!market || !latest) return;
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
      // Panes created before the first layout can initially have zero height.
      if (chart.panes()[1].getHeight() < 30 && chart.panes()[0].getHeight() > 120)
        chart.panes()[1].setHeight(Math.max(55, Math.min(80, Math.round(container.clientHeight * 0.18))));
      latest = data.last;
      showLegend(latest, latest?.volume);
      rangeLabel.textContent = `${data.count.toLocaleString()} fills · 5 min · UTC`;
      const view = views.get(key());
      if (view?.range && !view.follow) chart.timeScale().setVisibleRange(view.range);
      else if (view?.logical) {
        const to = data.candles.length + 1;
        chart.timeScale().setVisibleLogicalRange({ from: to - (view.logical.to - view.logical.from), to });
      } else chart.timeScale().setVisibleLogicalRange({ from: 0, to: data.candles.length + 2 });
      signature = next;
      fit = false;
    }
    const mark = snapshot?.committed_mark_e8 / 1e8;
    if (Number.isFinite(mark) && mark > 0) {
      if (markLine) markLine.applyOptions({ price: mark });
      else markLine = candles.createPriceLine({ price: mark, color: '#92aaf8', lineWidth: 1, lineStyle: LineStyle.Dashed, axisLabelVisible: false, title: 'Mark' });
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
      saveView(); market = value; title.textContent = value.title; snapshot = undefined; latest = undefined; signature = undefined; fit = true;
      candles.applyOptions({ priceFormat: { type: 'price', precision: value.config.price.decimals, minMove: value.config.price.step / 10 ** value.config.price.decimals } });
      if (markLine) { candles.removePriceLine(markLine); markLine = undefined; }
      render();
    },
    ingest(id, trades) { history.ingest(id, trades); if (market?.id === id) schedule(); },
    update(value) { snapshot = value; history.ingest(value.market_id, value.recent_trades); schedule(); },
    destroy() { clearInterval(clock); clearTimeout(timer); chart.remove(); },
  };
}
