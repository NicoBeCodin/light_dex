// A compact, non-modal market table. Only explicit opens fetch other markets.
export function createMarketPicker({ markets, getSelected, select, label, price, read }) {
  const button = document.getElementById("market-select");
  const panel = document.getElementById("market-picker");
  const search = document.getElementById("market-search");
  const results = document.getElementById("market-results");
  const status = document.getElementById("market-prices-status");
  const quotes = new Map();
  let filter = "all",
    loading = false,
    starred;
  const esc = (value) =>
    String(value).replace(
      /[&<>"']/g,
      (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c],
    );
  const pair = (market) => label(market).replace(/ (spot|perpetual)$/, "");
  const type = (market) => (market.config.market_type.kind === "perp" ? "PERP" : "SPOT");
  function favorites() {
    if (starred) return starred;
    try {
      starred = new Set(JSON.parse(localStorage.getItem("ldex-market-favorites") || "[]"));
    } catch {
      starred = new Set();
    }
    return starred;
  }
  function updateSelected() {
    const m = markets.find((m) => m.id === getSelected());
    button.innerHTML = `<strong>${esc(pair(m))}</strong><span class="contract-tag">${type(m)}</span><span class="picker-chevron" aria-hidden="true"></span>`;
    button.setAttribute("aria-label", `Choose market, ${label(m)}`);
    button.title = "Switch market (/)";
    button.disabled = false;
  }
  function quoteText(m) {
    const q = quotes.get(m.id);
    return q?.last != null ? price(q.last, m) : "-";
  }
  function render() {
    const stars = favorites();
    const tokens = search.value
      .toLowerCase()
      .split(/\s+/)
      .filter(Boolean)
      .map((t) => t.replace(/[^a-z0-9]/g, ""));
    const rows = markets.filter(
      (m) =>
        tokens.every((t) =>
          label(m)
            .toLowerCase()
            .replace(/[^a-z0-9]/g, "")
            .includes(t),
        ) &&
        (filter === "all" ||
          (filter === "favorites" && stars.has(m.id)) ||
          m.config.market_type.kind === filter),
    );
    const focused = document.activeElement?.dataset;
    const focusMarket = focused?.selectMarket,
      focusStar = focused?.favoriteMarket;
    results.innerHTML =
      rows
        .map((m) => {
          const starred = stars.has(m.id);
          return `<div class="market-row ${m.id === getSelected() ? "current" : ""}" role="listitem"><button class="market-star ${starred ? "favorite" : ""}" data-favorite-market="${m.id}" aria-pressed="${starred}" aria-label="${starred ? "Remove" : "Add"} ${esc(label(m))} ${starred ? "from" : "to"} favorites">${starred ? "★" : "☆"}</button><button class="market-choice" data-select-market="${m.id}" aria-label="${esc(label(m))}" ${m.id === getSelected() ? 'aria-current="true"' : ""}><span class="market-pair">${esc(pair(m))}<span class="contract-tag">${type(m)}</span></span><span class="market-price" data-market-price="${m.id}">${quoteText(m)}</span><span class="market-leverage">${m.config.market_type.max_leverage ? m.config.market_type.max_leverage + "×" : "-"}</span></button></div>`;
        })
        .join("") ||
      `<p class="empty">${filter === "favorites" && !tokens.length ? "No favorites yet" : "No matching markets"}</p>`;
    if (focusMarket !== undefined)
      results
        .querySelector(`[data-select-market="${focusMarket}"]`)
        ?.focus({ preventScroll: true });
    if (focusStar !== undefined)
      results
        .querySelector(`[data-favorite-market="${focusStar}"]`)
        ?.focus({ preventScroll: true });
  }
  function observeSnapshot(value) {
    const old = quotes.get(value.market_id);
    if (old && old.height > value.height) return;
    quotes.set(value.market_id, {
      height: value.height,
      last: value.recent_trades?.[0]?.price,
      at: Date.now(),
    });
    const cell = results.querySelector(`[data-market-price="${value.market_id}"]`);
    const m = markets.find((m) => m.id === value.market_id);
    if (cell && m) cell.textContent = quoteText(m);
  }
  async function refreshPrices() {
    if (loading) return;
    loading = true;
    status.textContent = "Updating prices";
    await Promise.allSettled(
      markets
        .filter((m) => Date.now() - (quotes.get(m.id)?.at || 0) > 15000)
        .map(async (m) => observeSnapshot(await read(m.id))),
    );
    loading = false;
    status.textContent = "Snapshot prices";
  }
  function position() {
    if (!panel.open) return;
    const rect = button.getBoundingClientRect();
    const width = Math.min(560, window.innerWidth - 24);
    const below = window.innerHeight - rect.bottom - 16;
    const above = rect.top - 16;
    const useAbove = below < 260 && above > below;
    const height = Math.min(440, useAbove ? above : below);
    panel.style.width = `${width}px`;
    panel.style.maxHeight = `${Math.max(180, height)}px`;
    panel.style.left = `${Math.max(12, Math.min(rect.left, window.innerWidth - width - 12))}px`;
    panel.style.top = `${useAbove ? Math.max(8, rect.top - panel.getBoundingClientRect().height - 6) : rect.bottom + 6}px`;
  }
  function close(focus = false) {
    panel.close();
    button.setAttribute("aria-expanded", "false");
    if (focus) button.focus();
  }
  function open() {
    if (!panel.open) {
      search.value = "";
      render();
      panel.show();
      position();
      button.setAttribute("aria-expanded", "true");
    }
    search.focus();
    void refreshPrices();
  }
  button.setAttribute("aria-expanded", "false");
  button.onclick = () => (panel.open ? close() : open());
  search.oninput = render;
  panel.addEventListener("click", async (event) => {
    const b = event.target.closest("button");
    if (!b) return;
    if (b.dataset.dismiss === "market-picker") close(true);
    if (b.dataset.marketFilter) {
      filter = b.dataset.marketFilter;
      panel.querySelectorAll("[data-market-filter]").forEach((x) => {
        x.classList.toggle("selected", x === b);
        x.setAttribute("aria-pressed", String(x === b));
      });
      render();
    }
    if (b.dataset.favoriteMarket !== undefined) {
      const stars = favorites(),
        id = Number(b.dataset.favoriteMarket);
      if (stars.has(id)) stars.delete(id);
      else stars.add(id);
      try {
        localStorage.setItem("ldex-market-favorites", JSON.stringify([...stars]));
      } catch {}
      render();
    }
    if (b.dataset.selectMarket !== undefined) {
      close(true);
      const changed = select(Number(b.dataset.selectMarket));
      updateSelected();
      await changed;
    }
  });
  document.addEventListener("pointerdown", (event) => {
    if (panel.open && !panel.contains(event.target) && !button.contains(event.target)) close();
  });
  document.addEventListener("keydown", (event) => {
    if (
      event.key === "/" &&
      !event.target.closest("input,textarea,select,[contenteditable]") &&
      !document.querySelector("dialog[open]")
    ) {
      event.preventDefault();
      open();
    }
    if (!panel.open) return;
    if (event.key === "Escape") {
      event.preventDefault();
      close(true);
    }
    if (event.key === "Enter" && event.target === search) {
      event.preventDefault();
      results.querySelector("[data-select-market]")?.click();
    }
    if (["ArrowDown", "ArrowUp"].includes(event.key)) {
      const rows = [...results.querySelectorAll("[data-select-market]")];
      if (!rows.length) return;
      const i = rows.indexOf(document.activeElement),
        down = event.key === "ArrowDown";
      event.preventDefault();
      rows[
        i < 0 ? (down ? 0 : rows.length - 1) : (i + (down ? 1 : -1) + rows.length) % rows.length
      ].focus();
    }
  });
  window.addEventListener("resize", position);
  window.addEventListener(
    "scroll",
    () => {
      if (panel.open) close();
    },
    { passive: true },
  );
  updateSelected();
  return { updateSelected, observeSnapshot };
}
