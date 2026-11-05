import { sizeFromMargin, closeOrder, decimalInput } from "./trade-math.mjs";
import { transactionNotice } from "./notifications.mjs";
import { subscribeMarket } from "./market-feed.mjs";

const $ = (id) => document.getElementById(id),
  esc = (s) =>
    String(s ?? "").replace(
      /[&<>"']/g,
      (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c],
    );
const hex = (bytes) =>
  [...new Uint8Array(bytes)].map((b) => b.toString(16).padStart(2, "0")).join("");
const unhex = (s) => new Uint8Array(s.match(/../g).map((x) => parseInt(x, 16)));
const symbols = ["USD", "BTC", "ETH", "SOL", "BNB", "XRP"];
let meta,
  wallet,
  wasm,
  marketId = 0,
  market,
  account,
  accountTab = "orders",
  bookTab = "book",
  page = "markets",
  marketFeed,
  accountSocket,
  busy = false,
  nextNonce = 0,
  lastDigest,
  closeMarketId,
  leverage = 1,
  marketFilter = "all";
const charts = new Map(),
  transactions = [];
function pendingKey() {
  return wallet && meta ? `ldex-pending:${meta.chain_id}:${wallet.address}` : null;
}
function hasPending() {
  return !!(pendingKey() && localStorage.getItem(pendingKey()));
}
function syncPending() {
  $("retry-pending").hidden = !hasPending();
  $("submit-order").disabled = busy || hasPending() || !wallet || !account || !meta.writes_enabled;
  $("claim-assets").disabled = busy || hasPending() || !wallet || !!account || !meta.writes_enabled;
  for (const id of ["set-margin", "deposit", "withdraw"])
    $(id).disabled = busy || hasPending() || !wallet || !account || !meta.writes_enabled;
  document.querySelectorAll("[data-close-position], [data-cancel]").forEach((button) => {
    button.disabled = busy || hasPending() || !wallet || !meta.writes_enabled;
  });
  $("confirm-close").disabled = busy || hasPending() || !wallet || !meta.writes_enabled;
  $("import-wallet").disabled = busy || hasPending();
}
function message(text = "") {
  $("message").textContent = text;
}
async function api(path, options) {
  const response = await fetch("/api/v1/" + path, options);
  const data = await response.json();
  if (!response.ok) throw new Error(data.error || `Request failed (${response.status})`);
  return data;
}
function name(m) {
  return `${symbols[m.config.base_asset] || "Asset"}-USD ${m.config.market_type.kind === "perp" ? "perpetual" : "spot"}`;
}
function selected() {
  return meta.markets.find((m) => m.id === marketId);
}
function marketFavorites() {
  try {
    return new Set(JSON.parse(localStorage.getItem("ldex-market-favorites") || "[]"));
  } catch {
    return new Set();
  }
}
function marketIcon(m) {
  const symbol = symbols[m.config.base_asset];
  return `<span class="asset-icon asset-${esc(symbol?.toLowerCase())}">${esc({ BTC: "₿", ETH: "Ξ", SOL: "◎" }[symbol] || symbol?.[0])}</span>`;
}
function updateMarketButton() {
  const m = selected();
  $("market-select").innerHTML =
    `${marketIcon(m)}<span>${esc(name(m))}</span><span class="muted">⌄</span>`;
  $("market-select").setAttribute("aria-label", `Choose market, ${name(m)}`);
  $("market-select").disabled = false;
}
function renderMarketPicker() {
  const favorites = marketFavorites();
  const search = $("market-search").value.trim().toLowerCase();
  const rows = meta.markets.filter(
    (m) =>
      name(m).toLowerCase().includes(search) &&
      (marketFilter === "all" ||
        (marketFilter === "favorites" && favorites.has(m.id)) ||
        m.config.market_type.kind === marketFilter),
  );
  $("market-results").innerHTML =
    rows
      .map((m) => {
        const perp = m.config.market_type.kind === "perp";
        const favorite = favorites.has(m.id);
        return `<div class="market-row ${m.id === marketId ? "current" : ""}"><button data-select-market="${m.id}" ${m.id === marketId ? 'aria-current="true"' : ""}>${marketIcon(m)}<span><strong>${esc(name(m))}</strong><small>${perp ? "Perpetual" : "Spot"}</small></span><span class="market-leverage">${perp ? `${m.config.market_type.max_leverage}×` : "Spot"}</span></button><button class="market-star ${favorite ? "favorite" : ""}" data-favorite-market="${m.id}" aria-pressed="${favorite}" aria-label="${favorite ? "Remove" : "Add"} ${esc(name(m))} ${favorite ? "from" : "to"} favorites">${favorite ? "★" : "☆"}</button></div>`;
      })
      .join("") || '<p class="empty">No matching markets</p>';
}
function openMarketPicker() {
  $("market-search").value = "";
  renderMarketPicker();
  $("market-picker").showModal();
  $("market-search").focus();
}
function atoms(text, unit) {
  if (!/^\d+(\.\d+)?$/.test(text)) throw Error("Enter a positive decimal number");
  const [a, b = ""] = text.split(".");
  if (b.length > unit.decimals) throw Error(`This market allows ${unit.decimals} decimal places`);
  const n = BigInt(a) * 10n ** BigInt(unit.decimals) + BigInt(b.padEnd(unit.decimals, "0") || 0);
  if (n <= 0n || n % BigInt(unit.step) !== 0n || n > BigInt(Number.MAX_SAFE_INTEGER))
    throw Error("Amount is outside the valid tick, lot or display range");
  return Number(n);
}
function format(n, decimals = 0) {
  if (n === undefined || n === null) return "-";
  if (!Number.isSafeInteger(Number(n))) return "Outside display range";
  const value = BigInt(n),
    neg = value < 0n,
    abs = neg ? -value : value,
    scale = 10n ** BigInt(decimals);
  return `${neg ? "-" : ""}${(abs / scale).toLocaleString("en-US")}${decimals ? "." + String(abs % scale).padStart(decimals, "0") : ""}`;
}
function price(n, m = selected()) {
  return format(n, m.config.price.decimals);
}
function quantity(n, m = selected()) {
  return format(n, m.config.quantity.decimals);
}
function short(s) {
  return s ? `${s.slice(0, 6)}…${s.slice(-4)}` : "-";
}
function ms(n) {
  return n === null || n === undefined
    ? "Collecting samples"
    : `${Math.round(n).toLocaleString()} ms`;
}
async function db() {
  return new Promise((resolve, reject) => {
    const request = indexedDB.open("light-dex-wallet", 1);
    request.onupgradeneeded = () => request.result.createObjectStore("keys");
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
}
async function loadWallet() {
  const d = await db();
  return new Promise((resolve, reject) => {
    const r = d.transaction("keys").objectStore("keys").get("wallet");
    r.onsuccess = () => {
      d.close();
      resolve(r.result);
    };
    r.onerror = () => reject(r.error);
  });
}
async function saveWallet(value) {
  const d = await db();
  await new Promise((resolve, reject) => {
    const t = d.transaction("keys", "readwrite");
    t.objectStore("keys").put(value, "wallet");
    t.oncomplete = resolve;
    t.onerror = () => reject(t.error);
  });
  d.close();
  wallet = value;
  nextNonce = 0;
  account = null;
  await walletChanged();
}
async function createWallet() {
  try {
    if (wallet) return $("wallet-dialog").showModal();
    const pair = await crypto.subtle.generateKey({ name: "Ed25519" }, true, ["sign", "verify"]);
    const address = hex(await crypto.subtle.exportKey("raw", pair.publicKey));
    await saveWallet({ address, privateKey: pair.privateKey, publicKey: pair.publicKey });
    message("Wallet created.");
  } catch (e) {
    message(`Wallet creation failed: ${e.message}. Use a current browser over HTTPS.`);
  }
}
async function walletChanged() {
  $("create-wallet").hidden = !!wallet;
  $("wallet-menu").hidden = !wallet;
  $("claim-assets").disabled = !wallet || !meta.writes_enabled;
  $("submit-order").disabled = !wallet || !meta.writes_enabled;
  $("wallet-label").textContent = wallet
    ? `Trading as ${short(wallet.address)}`
    : "Create a wallet to start trading.";
  $("step-wallet").textContent = wallet ? "✓ Wallet created" : "1 · Create wallet";
  if (wallet) {
    $("full-address").textContent = wallet.address;
    watchAccount();
    await refreshAccount();
  }
  syncPending();
  loadLeverage();
}
async function sign(transaction) {
  if (!wasm) throw Error("Signing module is not ready");
  const input = new TextEncoder().encode(
    JSON.stringify({ chain: [...unhex(meta.chain_id)], transaction }),
  );
  const p = wasm.allocate(input.length);
  try {
    new Uint8Array(wasm.memory.buffer, p, input.length).set(input);
    if (wasm.encode(p, input.length) !== 1)
      throw Error("Transaction cannot be canonically encoded");
    const bytes = new Uint8Array(wasm.memory.buffer, wasm.output_ptr(), wasm.output_len()).slice();
    const signature = hex(await crypto.subtle.sign("Ed25519", wallet.privateKey, bytes));
    return { transaction, signature };
  } finally {
    wasm.release(p, input.length);
  }
}
async function send(kind, fields) {
  if (!wallet) throw Error("Create a wallet first");
  if (busy || hasPending()) throw Error("A transaction is pending. Use Retry confirmation.");
  busy = true;
  $("submit-order").disabled = true;
  const nonce = Math.max(nextNonce, account?.nonce || 0);
  const transaction = { [kind]: { ...fields, nonce } };
  let signed;
  try {
    signed = await sign(transaction);
    localStorage.setItem(`ldex-pending:${meta.chain_id}:${wallet.address}`, JSON.stringify(signed));
    syncPending();
    return await track(signed, nonce);
  } finally {
    busy = false;
    syncPending();
  }
}
async function track(signed, nonce) {
  const started = performance.now();
  const kind = Object.keys(signed.transaction)[0];
  const label =
    {
      PlacePerpOrder: "Perp order",
      PlaceOrder: "Order",
      CancelOrder: "Cancel order",
      ClaimAirdrop: "Test assets",
      DepositPerpCollateral: "Deposit",
      WithdrawPerpCollateral: "Withdrawal",
      SetMarginMode: "Margin mode",
    }[kind] || "Transaction";
  const notice = transactionNotice(label);
  notice.update("pending", "Submitting signed transaction");
  let submitted = false;
  try {
    $("receipt-panel").hidden = false;
    $("receipt-progress").innerHTML =
      '<span class="done">Signed in browser</span><span>Admitted</span><span>Committed</span><span>Executed</span>';
    let result;
    let lastError = "";
    for (let attempt = 0; attempt < 40; attempt++) {
      if (!submitted || attempt % 5 === 0) {
        try {
          result = await api("transactions", {
            method: "POST",
            headers: { "content-type": "application/json" },
            body: JSON.stringify(signed),
          });
          if (!result.accepted) throw Error(result.error || "Transaction rejected");
          lastDigest = result.digest;
          submitted = true;
          notice.update("pending", "Admitted. Waiting for commit.", lastDigest);
          $("receipt-progress").children[1].classList.add("done");
          $("receipt-detail").textContent =
            `Digest: ${lastDigest}\nAdmission confirmed. Waiting for durable commitment…`;
        } catch (e) {
          lastError = e.message;
          if (/signature|invalid|Daily|allowance|disabled|nonce|balance/i.test(lastError)) {
            const current = await api(`rpc/accounts/${wallet.address}`).catch(() => null);
            if (current && current.nonce > nonce) {
              localStorage.removeItem(pendingKey());
              account = current;
              nextNonce = current.nonce;
              renderAccount();
            }
            throw e;
          }
        }
      }
      if (submitted) {
        const outcome = await api(`transactions/${lastDigest}`).catch(() => null);
        if (outcome?.status === "committed") {
          if (typeof outcome.receipt?.success !== "boolean") {
            lastError = "Committed, but the execution result is not available yet";
            await new Promise((r) => setTimeout(r, 500));
            continue;
          }
          const elapsed = Math.round(performance.now() - started);
          $("receipt-clock").textContent = `${elapsed.toLocaleString()} ms this attempt`;
          $("receipt-progress").children[2].classList.add("done");
          if (outcome.receipt.success === true) {
            $("receipt-progress").children[3].classList.add("done");
            nextNonce = Math.max(nextNonce, nonce + 1);
          } else if (outcome.receipt.success === false) {
            $("receipt-progress").children[3].textContent = "Execution failed";
          }
          $("receipt-detail").textContent = JSON.stringify(outcome.receipt, null, 2);
          transactions.unshift({ ...outcome.receipt, latency: elapsed });
          renderAccount();
          localStorage.removeItem(`ldex-pending:${meta.chain_id}:${wallet.address}`);
          syncPending();
          await new Promise((r) => setTimeout(r, 600));
          await refreshAccount();
          if (outcome.receipt.success === false)
            throw Error(outcome.receipt.error || "Execution failed");
          const fields = signed.transaction.PlacePerpOrder;
          const remaining =
            fields?.reduce_only &&
            account?.positions?.find((p) => p.market_id === fields.market_id);
          const detail = fields?.reduce_only
            ? remaining
              ? "Executed. Position remains open; liquidity may have filled only part of the order."
              : "Executed. Position closed."
            : "Executed";
          notice.update("success", `${elapsed.toLocaleString()} ms · ${detail}`, lastDigest);
          message("Committed.");
          return outcome;
        }
      }
      $("receipt-clock").textContent =
        `Waiting · ${Math.round((performance.now() - started) / 1000)} s`;
      await new Promise((r) => setTimeout(r, 500));
    }
    throw Error(
      `Confirmation is still pending${lastError ? ": " + lastError : ""}. The signed transaction is saved and will be retried when you reload.`,
    );
  } catch (error) {
    notice.update(hasPending() ? "waiting" : "error", error.message, submitted ? lastDigest : null);
    error.txNotified = true;
    throw error;
  }
}
function watchAccount() {
  if (accountSocket) {
    accountSocket.onmessage = null;
    accountSocket.onclose = null;
    accountSocket.close();
  }
  if (!wallet) return;
  accountSocket = new WebSocket(
    `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/api/v1/stream/accounts/${wallet.address}`,
  );
  accountSocket.onmessage = (event) => {
    const value = JSON.parse(event.data);
    if (!value.error) {
      account = value;
      nextNonce = Math.max(nextNonce, account.nonce || 0);
      renderAccount();
      $("claim-assets").disabled = true;
      $("claim-assets").textContent = "✓ Test assets claimed";
    }
  };
  accountSocket.onclose = () => {
    if (wallet) setTimeout(watchAccount, 3000);
  };
}
async function refreshAccount() {
  if (!wallet) return;
  try {
    account = await api(`rpc/accounts/${wallet.address}`);
    nextNonce = Math.max(nextNonce, account.nonce || 0);
    $("claim-assets").disabled = true;
    $("claim-assets").textContent = "✓ Test assets claimed";
    renderAccount();
  } catch {
    renderAccount();
  }
}
async function watchMarket() {
  marketFeed?.stop();
  market = null;
  $("book-content").innerHTML = '<p class="empty">Loading order book</p>';
  $("chart").textContent = "Loading market";
  $("last-price").textContent = "-";
  $("mark-price").textContent = "-";
  $("spread").textContent = "-";
  $("source-label").textContent = "Connecting to market data";
  const m = selected();
  $("base-symbol").textContent = symbols[m.config.base_asset];
  $("collateral").hidden = m.config.market_type.kind !== "perp";
  $("reduce-row").hidden = m.config.market_type.kind !== "perp";
  $("leverage-button").hidden = m.config.market_type.kind !== "perp";
  $("order-margin-row").hidden = m.config.market_type.kind !== "perp";
  $("order-margin").value = "";
  $("order-estimate").textContent = "";
  loadLeverage();
  $("order-price").value = "";
  $("order-quantity").value = "";
  const id = marketId;
  marketFeed = subscribeMarket({
    id,
    read: () => api(`rpc/markets/${id}`, { signal: AbortSignal.timeout(8000) }),
    connect: () =>
      new WebSocket(
        `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/api/v1/stream/markets/${id}`,
      ),
    onData(value) {
      if (marketId !== id) return;
      market = value;
      renderMarket();
    },
    onStatus(state) {
      if (marketId !== id) return;
      $("market-feed-state").textContent = {
        connecting: "Connecting",
        live: "Live",
        snapshot: "Snapshot · reconnecting",
        unavailable: "Market data unavailable · retrying",
      }[state];
      $("refresh-market").hidden = state !== "unavailable";
      if (state === "unavailable" && !market)
        $("book-content").innerHTML =
          '<p class="empty">Market data unavailable. Retrying automatically.</p>';
    },
  });
  const ready = marketFeed.ready;
  try {
    const h = await api(`history/${id}`);
    if (id === marketId) {
      rememberTrades(h.trades);
      drawChart();
    }
  } catch {}
  await ready;
}
function rememberTrades(trades = []) {
  let points = charts.get(marketId);
  if (!points) {
    points = new Map();
    charts.set(marketId, points);
  }
  for (const t of trades) {
    const key = `${t.block_sequence}:${t.transaction_index}:${t.fill_index}`;
    if (!points.has(key))
      points.set(key, { ...t, at: (t.observed_at || Date.now() / 1000) * 1000 });
  }
  for (const [k, v] of points) {
    if (v.at < Date.now() - 300000) points.delete(k);
  }
  while (points.size > 10000) points.delete(points.keys().next().value);
}
function renderMarket() {
  const m = selected(),
    book = market.book;
  const trades = market.recent_trades || [];
  $("last-price").textContent = trades.length ? price(trades[0].price) : "-";
  $("mark-price").textContent = market.committed_mark_e8
    ? `$${(market.committed_mark_e8 / 1e8).toLocaleString("en-US", { maximumFractionDigits: m.config.price.decimals })}`
    : "Waiting for oracle";
  const bid = book?.bids?.[0]?.price,
    ask = book?.asks?.[0]?.price;
  $("spread").textContent = bid && ask ? `$${price(ask - bid)}` : "-";
  $("source-label").textContent =
    `Source validator ${market.source_validator ?? market.node_id} · Height ${market.height} · via Tokyo`;
  if (!$("order-price").value && document.activeElement !== $("order-price")) {
    const raw =
      ask ||
      bid ||
      (market.committed_mark_e8
        ? Math.round(
            ((market.committed_mark_e8 / 1e8) * 10 ** m.config.price.decimals) /
              m.config.price.step,
          ) * m.config.price.step
        : 0);
    if (raw) {
      $("order-price").value = price(raw).replaceAll(",", "");
      const q = Math.max(
        m.config.quantity.step,
        Math.floor(
          ((25 / (raw / 10 ** m.config.price.decimals)) * 10 ** m.config.quantity.decimals) /
            m.config.quantity.step,
        ) * m.config.quantity.step,
      );
      $("order-quantity").value = quantity(q).replaceAll(",", "");
    }
  }
  rememberTrades(trades);
  drawChart();
  renderBook();
}
function renderBook() {
  if (!market) return;
  if (bookTab === "trades") {
    const rows = (market.recent_trades || [])
      .map(
        (t) =>
          `<tr class="${t.aggressor_side === "Buy" ? "buy" : "sell"}"><td>${price(t.price)}${t.liquidation ? '<span class="tag">LIQ</span>' : ""}</td><td>${quantity(t.quantity)}</td><td><button data-copy="${esc(t.maker_account)}" title="Copy maker address">${short(t.maker_account)}</button></td><td><button data-copy="${esc(t.taker_account)}" title="Copy taker address">${short(t.taker_account)}</button></td></tr>`,
      )
      .join("");
    $("book-content").innerHTML =
      `<table><thead><tr><th>Price (USD)</th><th>Size</th><th>Maker</th><th>Taker</th></tr></thead><tbody>${rows || '<tr><td colspan="4">No committed trades yet</td></tr>'}</tbody></table>`;
    return;
  }
  const bids = market.book?.bids || [],
    asks = market.book?.asks || [];
  const sum = (xs) => xs.reduce((s, x) => s + x.quantity, 0);
  const max = Math.max(sum(bids), sum(asks), 1);
  const levels = (xs, buy) => {
    let cumulative = 0;
    const rows = xs.slice(0, 12).map((l) => {
      cumulative += l.quantity;
      return `<tr class="level ${buy ? "buy" : "sell"}" style="--depth:${Math.min(100, (cumulative / max) * 100)}%;--depth-color:${buy ? "#15352c" : "#391f2a"}"><td>${price(l.price)}</td><td>${quantity(l.quantity)}</td><td>${quantity(cumulative)}</td></tr>`;
    });
    return (buy ? rows : rows.reverse()).join("");
  };
  $("book-content").innerHTML =
    `<table><thead><tr><th>Price (USD)</th><th>Size</th><th>Total size</th></tr></thead><tbody>${levels(asks, false)}<tr class="spread-row"><td colspan="3">Spread ${esc($("spread").textContent)}</td></tr>${levels(bids, true)}</tbody></table>`;
}
function drawChart() {
  const points = [...(charts.get(marketId)?.values() || [])].sort((a, b) => a.at - b.at);
  if (!points.length) {
    $("chart").textContent = "Waiting for committed trades";
    return;
  }
  const w = 800,
    h = 350,
    min = Math.min(...points.map((p) => p.price)),
    max = Math.max(...points.map((p) => p.price)),
    range = Math.max(max - min, selected().config.price.step),
    start = Date.now() - 300000;
  const coords = points
    .map(
      (p) =>
        `${30 + Math.max(0, (p.at - start) / 300000) * 740},${300 - ((p.price - min) / range) * 250}`,
    )
    .join(" ");
  const last = points.at(-1);
  $("chart").innerHTML =
    `<svg viewBox="0 0 ${w} ${h}" role="img" aria-label="Committed execution prices over the last five minutes"><defs><linearGradient id="area" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#72e3bd" stop-opacity=".2"/><stop offset="1" stop-color="#72e3bd" stop-opacity="0"/></linearGradient></defs>${[60, 140, 220, 300].map((y) => `<line x1="30" x2="770" y1="${y}" y2="${y}" stroke="#202934"/>`).join("")}<polygon points="30,320 ${coords} 770,320" fill="url(#area)"/><polyline points="${coords}" fill="none" stroke="#72e3bd" stroke-width="2"/><text x="35" y="30" fill="#8995a5" font-size="13">$${price(max)}</text><text x="35" y="335" fill="#8995a5" font-size="13">$${price(min)}</text></svg>`;
  $("chart-range").textContent = `${points.length} fills · Last $${price(last.price)}`;
}
function table(head, rows, empty) {
  return `<table><thead><tr>${head.map((h) => `<th>${esc(h)}</th>`).join("")}</tr></thead><tbody>${rows.join("") || `<tr><td colspan="${head.length}" class="empty">${esc(empty)}</td></tr>`}</tbody></table>`;
}
function dollars(atoms) {
  return Number.isSafeInteger(Number(atoms))
    ? (Number(atoms) / 1e6).toLocaleString("en-US", {
        style: "currency",
        currency: "USD",
        minimumFractionDigits: 2,
        maximumFractionDigits: 2,
      })
    : "Outside display range";
}
function renderRisk() {
  const root = $("account-risk");
  root.hidden = !wallet || !account;
  if (root.hidden) return;
  const risks = account.risk || [];
  root.innerHTML = risks.length
    ? risks
        .map((r) => {
          const m = meta.markets.find((m) => m.id === r.market_id);
          const title = r.mode === "Cross" ? "Cross account" : `${m ? name(m) : "Perp"} · Isolated`;
          const equity = Number(r.equity_usd_atoms),
            maintenance = Number(r.maintenance_required_usd_atoms);
          const used =
            maintenance > 0
              ? equity > 0
                ? `${((maintenance / equity) * 100).toFixed(1)}%`
                : "Below maintenance"
              : "0%";
          const cards = [
            ["Equity", r.equity_usd_atoms],
            ["Collateral", r.collateral_usd_atoms],
            ["Unrealized PnL", equity - Number(r.collateral_usd_atoms)],
            ["Initial margin", r.initial_required_usd_atoms],
            ["Maintenance margin", r.maintenance_required_usd_atoms],
            ["Margin headroom", equity - Number(r.initial_required_usd_atoms)],
          ];
          return `<section class="risk-group"><div class="risk-title"><strong>${esc(title)}</strong><span class="${r.liquidatable ? "sell" : "muted"}">${r.liquidatable ? "Liquidatable" : `Maintenance / equity ${used}`}</span></div><div class="risk-grid">${cards.map(([label, value]) => `<div><small>${label}</small><strong class="${Number(value) < 0 ? "sell" : ""}">${esc(dollars(value))}</strong></div>`).join("")}</div></section>`;
        })
        .join("")
    : '<div class="risk-title"><strong>Perp collateral</strong><span>$0.00</span></div>';
}
function renderAccount() {
  syncPending();
  renderRisk();
  if (!wallet) {
    $("account-content").innerHTML =
      '<p class="empty">Create a demo wallet to see your balances and orders.</p>';
    return;
  }
  if (accountTab === "activity") {
    $("account-content").innerHTML = table(
      ["Transaction", "Outcome", "Browser time", "Block"],
      [
        ...new Map(
          [...transactions, ...(account?.recent_transaction_outcomes || [])].map((t) => [
            t.digest,
            t,
          ]),
        ).values(),
      ]
        .slice(0, 20)
        .map(
          (t) =>
            `<tr><td><button data-transaction="${t.digest}">${short(t.digest)}</button></td><td>${t.success ? "Executed" : esc(t.error || "Unknown")}</td><td>${t.latency === undefined ? "-" : ms(t.latency)}</td><td>${t.block_sequence ?? "-"}</td></tr>`,
        ),
      "Your committed transactions will appear here",
    );
    return;
  }
  if (!account) {
    $("account-content").innerHTML = '<p class="empty">Claim test assets to fund this address.</p>';
    return;
  }
  if (accountTab === "orders")
    $("account-content").innerHTML = table(
      ["Market", "Side", "Price", "Remaining", "Order", ""],
      (account.open_orders || []).map((o) => {
        const m = meta.markets.find((m) => m.id === o.market_id);
        return `<tr><td>${esc(name(m))}</td><td class="${o.side === "Buy" ? "buy" : "sell"}">${esc(o.side)}</td><td>${price(o.price, m)}</td><td>${quantity(o.remaining_quantity, m)}</td><td>${o.order_id}</td><td><button data-cancel="${o.order_id}" data-market="${o.market_id}">Cancel</button></td></tr>`;
      }),
      "No resting orders",
    );
  if (accountTab === "positions")
    $("account-content").innerHTML = table(
      ["Market", "Side", "Size", "Entry price", "Margin", "Realized PnL", ""],
      (account.positions || []).map((p) => {
        const m = meta.markets.find((m) => m.id === p.market_id);
        const long = Number(p.size_atoms) > 0;
        const risk = account.risk?.find((r) => r.mode === "Cross" || r.market_id === p.market_id);
        return `<tr><td>${esc(name(m))}</td><td class="${long ? "buy" : "sell"}">${long ? "Long" : "Short"}</td><td>${quantity(Math.abs(Number(p.size_atoms)), m)}</td><td>${price(p.entry_price, m)}</td><td>${esc(risk?.mode || "-")}</td><td class="${Number(p.realized_pnl_usd_atoms) < 0 ? "sell" : "buy"}">${esc(dollars(p.realized_pnl_usd_atoms))}</td><td><button data-close-position="${p.market_id}">Close at market</button></td></tr>`;
      }),
      "No perpetual positions",
    );
  if (accountTab === "balances")
    $("account-content").innerHTML = table(
      ["Asset", "Available", "Locked"],
      (account.balances || []).map((b) => {
        const decimals =
          b.asset_id === 0
            ? 6
            : meta.markets.find((m) => m.config.base_asset === b.asset_id)?.config.quantity
                .decimals || 4;
        return `<tr><td>${esc(symbols[b.asset_id] || b.asset_id)}</td><td>${format(b.available, decimals)}</td><td>${format(b.locked, decimals)}</td></tr>`;
      }),
      "No balances",
    );
  syncPending();
}
async function networkRefresh() {
  try {
    const n = await api("network");
    if (n.chain_id && n.chain_id !== meta.chain_id) {
      meta.writes_enabled = false;
      syncPending();
      message("The testnet restarted. Reconnect, then claim test assets again.");
      $("reload-network").hidden = false;
    }
    $("network-state").textContent = `${n.status || "Connecting"} · Tokyo entry`;
    $("live-dot").style.background = n.status === "online" ? "#72e3bd" : "#dca85d";
    $("market-tps").textContent = n.successful_order_tps?.toFixed(1) || "-";
    $("market-p95").textContent = ms(n.gateway_commit_p95_ms);
    const metrics = [
      ["Successful order TPS · 60s", n.successful_order_tps?.toFixed(1)],
      ["Tokyo gateway p95 · 5m", ms(n.gateway_commit_p95_ms)],
      ["Committed height", n.height],
      ["Gateway uptime", `${Math.floor((n.uptime_seconds || 0) / 60)} min`],
    ];
    $("network-metrics").innerHTML = metrics
      .map(
        ([title, value]) =>
          `<div class="metric"><span>${esc(title)}</span><strong>${esc(value ?? "-")}</strong></div>`,
      )
      .join("");
    $("validators").innerHTML = (n.validators || [])
      .map(
        (v) =>
          `<div class="validator"><h3>Validator ${v.id}</h3><p>${esc(v.region)} · <span class="${v.online ? "buy" : "sell"}">${v.online ? "Connected" : "Unavailable"}</span></p><div>Height ${v.height ?? "-"} · View ${v.view ?? "-"}</div><div class="muted">Pending ${v.backlog ?? "-"}</div></div>`,
      )
      .join("");
    $("network-details").textContent =
      `Chain ${meta.chain_id} · HotStuff2 · Rotation every 4 blocks · Quorum ${n.quorum ?? 3} · ${n.latency_samples ?? 0} latency samples`;
  } catch {
    $("network-state").textContent = "Gateway connection interrupted";
  }
}
async function explorerRefresh() {
  try {
    const data = await api("rpc/explorer/recent");
    $("explorer-source").textContent =
      `Validator ${data.source_validator ?? data.node_id} · Via Tokyo`;
    $("explorer-list").innerHTML = table(
      ["Sequence", "Transactions", "Leader", "Certificate votes", "Digest"],
      (data.blocks || []).map(
        (b) =>
          `<tr><td><button data-block="${b.sequence}">${b.sequence}</button></td><td>${b.transaction_count}</td><td>${b.leader ?? "-"}</td><td>${b.certificate_votes ?? "-"}</td><td>${short(b.digest)}</td></tr>`,
      ),
      "Waiting for blocks",
    );
  } catch (e) {
    message(e.message);
  }
}
async function inspect(key) {
  try {
    const value = /^\d+$/.test(key)
      ? await api(`rpc/explorer/blocks/${key}`)
      : await api(`rpc/explorer/transactions/${key}`);
    $("explorer-detail").hidden = false;
    $("explorer-json").textContent = JSON.stringify(value, null, 2);
  } catch (e) {
    message(e.message);
  }
}
function showPage(value) {
  page = value;
  for (const id of ["markets", "network", "explorer"]) $(`page-${id}`).hidden = id !== value;
  document
    .querySelectorAll("[data-page]")
    .forEach((b) => b.classList.toggle("selected", b.dataset.page === value));
  if (value === "explorer") explorerRefresh();
}
function action(fn) {
  return async (event) => {
    event?.preventDefault();
    message();
    const inlineError = event?.currentTarget?.closest("dialog")?.querySelector("[data-form-error]");
    if (inlineError) inlineError.hidden = true;
    try {
      await fn(event);
    } catch (e) {
      message(e.message);
      if (inlineError) {
        inlineError.textContent = e.message;
        inlineError.hidden = false;
      }
      if (!e.txNotified) transactionNotice("Request").update("error", e.message);
    }
  };
}
function leverageKey() {
  return `ldex-leverage:${meta.chain_id}:${wallet?.address || "guest"}:${marketId}`;
}
function loadLeverage() {
  if (!meta) return;
  const limit = selected()?.config.market_type.max_leverage || 1;
  let saved = 1;
  try {
    saved = Number(localStorage.getItem(leverageKey()) || 1);
  } catch {}
  leverage = Number.isInteger(saved) ? Math.max(1, Math.min(saved, limit)) : 1;
  $("leverage-button").textContent = `Leverage · ${leverage}×`;
}
function sizeOrder() {
  const m = selected();
  if (m.config.market_type.kind !== "perp" || !$("order-margin").value) return;
  const margin = atoms($("order-margin").value, { decimals: 6, step: 1 });
  const limit = atoms($("order-price").value, m.config.price);
  const size = sizeFromMargin(margin, leverage, limit, m.config);
  $("order-quantity").value = decimalInput(size, m.config.quantity.decimals);
  $("order-estimate").textContent = `${leverage}× sizing · ${dollars(margin)} order margin`;
}
$("leverage-button").onclick = action(() => {
  const max = selected().config.market_type.max_leverage;
  $("leverage-value").max = max;
  $("leverage-value").value = leverage;
  $("leverage-margin").value = $("order-margin").value || "100";
  $("leverage-limit").textContent = `Market limit: ${max}×`;
  $("leverage-dialog").showModal();
});
$("leverage-form").onsubmit = action(() => {
  const next = Number($("leverage-value").value),
    m = selected();
  const margin = atoms($("leverage-margin").value, { decimals: 6, step: 1 });
  const limit = atoms($("order-price").value, m.config.price);
  sizeFromMargin(margin, next, limit, m.config); // Validate before changing saved inputs.
  leverage = next;
  localStorage.setItem(leverageKey(), String(leverage));
  $("order-margin").value = $("leverage-margin").value;
  loadLeverage();
  sizeOrder();
  $("leverage-dialog").close();
});
for (const id of ["order-margin", "order-price"])
  $(id).addEventListener("input", () => {
    try {
      sizeOrder();
    } catch (e) {
      $("order-estimate").textContent = e.message;
    }
  });
$("order-quantity").addEventListener("input", () => {
  $("order-margin").value = "";
  $("order-estimate").textContent = "";
});
$("close-position-form").onsubmit = action(async () => {
  if (busy || hasPending()) throw Error("Wait for the pending transaction first");
  const id = closeMarketId;
  const m = meta.markets.find((m) => m.id === id);
  const slippage = Number($("close-slippage").value) * 100;
  const [fresh, book] = await Promise.all([
    api(`rpc/accounts/${wallet.address}`),
    api(`rpc/markets/${id}`),
  ]);
  const p = fresh.positions.find((p) => p.market_id === id);
  if (!p) throw Error("This position is already closed");
  account = fresh;
  nextNonce = Math.max(nextNonce, fresh.nonce || 0);
  const order = closeOrder(p, book.book, m.config, Math.round(slippage));
  $("close-position-dialog").close();
  await send("PlacePerpOrder", { account_id: wallet.address, market_id: id, ...order });
});
$("create-wallet").onclick = createWallet;
$("reload-network").onclick = () => location.reload();
$("wallet-menu").onclick = () => $("wallet-dialog").showModal();
$("close-wallet").onclick = () => $("wallet-dialog").close();
$("copy-address").onclick = action(() => navigator.clipboard.writeText(wallet.address));
$("export-wallet").onclick = action(async () => {
  const privateKey = hex(await crypto.subtle.exportKey("pkcs8", wallet.privateKey));
  const blob = new Blob(
    [JSON.stringify({ version: 1, address: wallet.address, privateKey }, null, 2)],
    { type: "application/json" },
  );
  const link = document.createElement("a");
  link.href = URL.createObjectURL(blob);
  link.download = "light-dex-demo-wallet.json";
  link.click();
  setTimeout(() => URL.revokeObjectURL(link.href), 1000);
});
$("import-wallet").onclick = () => $("wallet-file").click();
$("wallet-file").onchange = action(async (event) => {
  if (busy || hasPending()) throw Error("Confirm the pending transaction before changing wallets");
  const file = event.target.files[0];
  if (!file || file.size > 4096) throw Error("Invalid wallet backup");
  const data = JSON.parse(await file.text());
  if (
    data.version !== 1 ||
    !/^[a-f0-9]{64}$/.test(data.address) ||
    !/^[a-f0-9]+$/.test(data.privateKey)
  )
    throw Error("Invalid wallet backup");
  const privateKey = await crypto.subtle.importKey(
    "pkcs8",
    unhex(data.privateKey),
    "Ed25519",
    true,
    ["sign"],
  );
  const publicKey = await crypto.subtle.importKey("raw", unhex(data.address), "Ed25519", true, [
    "verify",
  ]);
  const probe = new TextEncoder().encode("Light DEX wallet check");
  if (
    !(await crypto.subtle.verify(
      "Ed25519",
      publicKey,
      await crypto.subtle.sign("Ed25519", privateKey, probe),
      probe,
    ))
  )
    throw Error("Backup address and private key do not match");
  await saveWallet({ address: data.address, privateKey, publicKey });
  $("wallet-dialog").close();
});
$("claim-assets").onclick = action(() => send("ClaimAirdrop", { account_id: wallet.address }));
$("market-select").onclick = openMarketPicker;
$("market-search").oninput = renderMarketPicker;
$("market-picker").addEventListener("click", (event) => {
  if (event.target === $("market-picker")) $("market-picker").close();
});
document.addEventListener("keydown", (event) => {
  if (
    event.key === "/" &&
    !event.target.closest("input, textarea, select, [contenteditable]") &&
    !document.querySelector("dialog[open]")
  ) {
    event.preventDefault();
    openMarketPicker();
  }
  if (["ArrowDown", "ArrowUp"].includes(event.key) && $("market-picker").open) {
    const rows = [...document.querySelectorAll("[data-select-market]")];
    if (!rows.length) return;
    const index = rows.indexOf(document.activeElement);
    event.preventDefault();
    rows[
      index < 0
        ? event.key === "ArrowDown"
          ? 0
          : rows.length - 1
        : (index + (event.key === "ArrowDown" ? 1 : -1) + rows.length) % rows.length
    ].focus();
  }
  if (event.key === "Enter" && event.target === $("market-search") && $("market-picker").open) {
    event.preventDefault();
    document.querySelector("[data-select-market]")?.click();
  }
});
$("refresh-market").onclick = () => {
  void marketFeed?.refresh();
};
$("order-form").onsubmit = action(async () => {
  const m = selected();
  sizeOrder();
  const fields = {
    account_id: wallet.address,
    market_id: marketId,
    order_type: $("order-type").value,
    side: new FormData($("order-form")).get("side"),
    price: atoms($("order-price").value, m.config.price),
    quantity: atoms($("order-quantity").value, m.config.quantity),
  };
  if (m.config.market_type.kind === "perp") fields.reduce_only = $("reduce-only").checked;
  await send(m.config.market_type.kind === "perp" ? "PlacePerpOrder" : "PlaceOrder", fields);
});
$("set-margin").onclick = action(() =>
  send("SetMarginMode", { account_id: wallet.address, mode: $("margin-mode").value }),
);
$("deposit").onclick = action(async () => {
  const target = $("margin-mode").value;
  if (!account?.risk?.some((r) => r.mode === target))
    await send("SetMarginMode", { account_id: wallet.address, mode: target });
  await send("DepositPerpCollateral", {
    account_id: wallet.address,
    market_id: marketId,
    amount: atoms($("collateral-amount").value, { decimals: 6, step: 1 }),
  });
});
$("withdraw").onclick = action(() =>
  send("WithdrawPerpCollateral", {
    account_id: wallet.address,
    market_id: marketId,
    amount: atoms($("collateral-amount").value, { decimals: 6, step: 1 }),
  }),
);
$("retry-pending").onclick = action(async () => {
  if (busy) return;
  const saved = localStorage.getItem(pendingKey());
  if (!saved) return;
  const signed = JSON.parse(saved);
  busy = true;
  syncPending();
  try {
    await track(signed, Object.values(signed.transaction)[0].nonce);
  } finally {
    busy = false;
    syncPending();
  }
});
$("search-form").onsubmit = action(() => inspect($("search-input").value.trim()));
$("open-receipt").onclick = () => {
  showPage("explorer");
  inspect(lastDigest);
};
document.addEventListener("click", async (e) => {
  try {
    const b = e.target.closest("button");
    if (!b) return;
    if (b.dataset.marketFilter) {
      marketFilter = b.dataset.marketFilter;
      document
        .querySelectorAll("[data-market-filter]")
        .forEach((x) => x.classList.toggle("selected", x === b));
      renderMarketPicker();
    }
    if (b.dataset.favoriteMarket !== undefined) {
      const id = Number(b.dataset.favoriteMarket),
        favorites = marketFavorites();
      if (favorites.has(id)) favorites.delete(id);
      else favorites.add(id);
      try {
        localStorage.setItem("ldex-market-favorites", JSON.stringify([...favorites]));
      } catch {}
      renderMarketPicker();
    }
    if (b.dataset.selectMarket !== undefined) {
      const id = Number(b.dataset.selectMarket);
      $("market-picker").close();
      if (id !== marketId) {
        marketId = id;
        updateMarketButton();
        await watchMarket();
      }
    }
    if (b.dataset.dismiss) $(b.dataset.dismiss).close();
    if (b.dataset.closePosition !== undefined) {
      closeMarketId = Number(b.dataset.closePosition);
      const m = meta.markets.find((m) => m.id === closeMarketId);
      const position = account?.positions.find((p) => p.market_id === closeMarketId);
      if (!position) throw Error("This position is no longer open");
      $("close-position-summary").textContent =
        `${name(m)} · ${Number(position.size_atoms) > 0 ? "Long" : "Short"} ${quantity(Math.abs(Number(position.size_atoms)), m)} ${symbols[m.config.base_asset]}`;
      $("close-position-dialog").showModal();
    }
    if (b.dataset.page) showPage(b.dataset.page);
    if (b.dataset.book) {
      bookTab = b.dataset.book;
      document
        .querySelectorAll("[data-book]")
        .forEach((x) => x.classList.toggle("selected", x === b));
      renderBook();
    }
    if (b.dataset.account) {
      accountTab = b.dataset.account;
      document
        .querySelectorAll("[data-account]")
        .forEach((x) => x.classList.toggle("selected", x === b));
      renderAccount();
    }
    if (b.dataset.copy) await navigator.clipboard.writeText(b.dataset.copy);
    if (b.dataset.cancel)
      await send("CancelOrder", {
        account_id: wallet.address,
        market_id: Number(b.dataset.market),
        order_id: Number(b.dataset.cancel),
      });
    if (b.dataset.transaction) {
      showPage("explorer");
      await inspect(b.dataset.transaction);
    }
    if (b.dataset.block) await inspect(b.dataset.block);
  } catch (error) {
    message(error.message);
    if (!error.txNotified) transactionNotice("Request").update("error", error.message);
  }
});
async function boot() {
  try {
    meta = await api("meta");
    $("revision").textContent = `${meta.revision.slice(0, 8)} · Protocol ${meta.protocol}`;
    updateMarketButton();
    await watchMarket();
    await networkRefresh();
    setInterval(networkRefresh, 1000);
    setInterval(() => {
      if (page === "explorer") explorerRefresh();
    }, 3000);
    try {
      const result = await WebAssembly.instantiateStreaming(fetch("/codec.wasm"));
      wasm = result.instance.exports;
      wallet = await loadWallet();
      await walletChanged();
    } catch (error) {
      message(`Wallet unavailable: ${error.message}`);
      return; // Market data and the explorer remain available.
    }
    if (wallet) {
      const saved = localStorage.getItem(`ldex-pending:${meta.chain_id}:${wallet.address}`);
      if (saved) {
        const signed = JSON.parse(saved),
          fields = Object.values(signed.transaction)[0];
        busy = true;
        track(signed, fields.nonce)
          .catch((e) => message(e.message))
          .finally(() => {
            busy = false;
            syncPending();
          });
      }
    }
  } catch (e) {
    message(`Could not initialize the demo: ${e.message}`);
  }
}
boot();
