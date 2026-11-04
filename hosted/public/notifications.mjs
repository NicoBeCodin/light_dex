export function transactionNotice(title) {
  const root = document.getElementById("tx-notifications");
  while (root.children.length >= 4) root.firstElementChild.remove();
  const card = document.createElement("section");
  card.className = "tx-toast pending";
  card.setAttribute("role", "status");
  const heading = document.createElement("strong");
  const detail = document.createElement("div");
  const controls = document.createElement("div");
  const dismiss = document.createElement("button");
  dismiss.className = "toast-dismiss";
  dismiss.textContent = "×";
  dismiss.setAttribute("aria-label", "Dismiss transaction notification");
  dismiss.onclick = () => card.remove();
  card.append(dismiss, heading, detail, controls);
  root.append(card);
  let timer;
  return {
    update(state, text, digest) {
      clearTimeout(timer);
      card.className = `tx-toast ${state}`;
      heading.textContent = `${title} · ${state === "success" ? "Confirmed" : state === "error" ? "Failed" : state === "waiting" ? "Confirmation pending" : "Sending"}`;
      detail.textContent = text;
      controls.replaceChildren();
      if (digest) {
        const explorer = document.createElement("button");
        explorer.textContent = "View transaction";
        explorer.dataset.transaction = digest;
        controls.append(explorer);
      }
      if (state === "waiting") {
        const retry = document.createElement("button");
        retry.textContent = "Check again";
        retry.onclick = () => document.getElementById("retry-pending").click();
        controls.append(retry);
      }
      if (state === "success" || state === "error" || state === "waiting")
        timer = setTimeout(() => card.remove(), state === "success" ? 8000 : 20000);
    },
  };
}
