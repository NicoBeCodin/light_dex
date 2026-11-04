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
  marketSocket,
  accountSocket,
  busy = false,
  nextNonce = 0,
  lastDigest;
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
  $("receipt-panel").hidden = false;
  $("receipt-progress").innerHTML =
    '<span class="done">Signed in browser</span><span>Admitted</span><span>Committed</span><span>Executed</span>';
  let result,
    submitted = false;
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
  if (marketSocket) {
    marketSocket.onmessage = null;
    marketSocket.onclose = null;
    marketSocket.close();
  }
  market = null;
  const m = selected();
  $("base-symbol").textContent = symbols[m.config.base_asset];
  $("collateral").hidden = m.config.market_type.kind !== "perp";
  $("reduce-row").hidden = m.config.market_type.kind !== "perp";
  $("order-price").value = "";
  $("order-quantity").value = "";
  marketSocket = new WebSocket(
    `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/api/v1/stream/markets/${marketId}`,
  );
  marketSocket.onmessage = (e) => {
    const v = JSON.parse(e.data);
    if (v.error) return;
    market = v;
    renderMarket();
  };
  const id = marketId;
  marketSocket.onclose = () => {
    if (id === marketId) setTimeout(watchMarket, 3000);
  };
  try {
    const h = await api(`history/${id}`);
    if (id === marketId) {
      rememberTrades(h.trades);
      drawChart();
    }
  } catch {}
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
function renderAccount() {
  syncPending();
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
    $("account-content").innerHTML =
      table(
        ["Market", "Position", "Entry price", "Realized PnL (USD)"],
        (account.positions || []).map((p) => {
          const m = meta.markets.find((m) => m.id === p.market_id);
          return `<tr><td>${esc(name(m))}</td><td>${quantity(p.size_atoms, m)}</td><td>${price(p.entry_price, m)}</td><td>${format(p.realized_pnl_usd_atoms, 6)}</td></tr>`;
        }),
        "No perpetual positions",
      ) +
      `<div class="empty">${(account.risk || []).map((r) => `${esc(r.mode)} equity $${format(r.equity_usd_atoms, 6)} · Initial margin $${format(r.initial_required_usd_atoms, 6)} · Maintenance $${format(r.maintenance_required_usd_atoms, 6)}`).join("<br>")}</div>`;
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
}
async function networkRefresh() {
  try {
    const n = await api("network");
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
    try {
      await fn(event);
    } catch (e) {
      message(e.message);
    }
  };
}
$("create-wallet").onclick = createWallet;
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
$("market-select").onchange = action(async () => {
  marketId = Number($("market-select").value);
  await watchMarket();
});
$("order-form").onsubmit = action(async () => {
  const m = selected();
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
  }
});
async function boot() {
  try {
    meta = await api("meta");
    $("revision").textContent = `${meta.revision.slice(0, 8)} · Protocol ${meta.protocol}`;
    $("market-select").innerHTML = meta.markets
      .map((m) => `<option value="${m.id}">${esc(name(m))}</option>`)
      .join("");
    const result = await WebAssembly.instantiateStreaming(fetch("/codec.wasm"));
    wasm = result.instance.exports;
    wallet = await loadWallet();
    await walletChanged();
    await watchMarket();
    await networkRefresh();
    setInterval(networkRefresh, 1000);
    setInterval(() => {
      if (page === "explorer") explorerRefresh();
    }, 3000);
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
