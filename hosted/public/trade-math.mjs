const safe = (value) => {
  if (value <= 0n || value > BigInt(Number.MAX_SAFE_INTEGER))
    throw Error("Amount is outside the supported range");
  return Number(value);
};

export function sizeFromMargin(marginAtoms, leverage, priceAtoms, config) {
  if (!Number.isInteger(leverage) || leverage < 1 || leverage > config.market_type.max_leverage)
    throw Error("Choose a leverage within the market limit");
  if (BigInt(marginAtoms) <= 0n || BigInt(priceAtoms) <= 0n) throw Error("Enter margin and price");
  const step = BigInt(config.quantity.step);
  const size =
    (BigInt(marginAtoms) *
      BigInt(leverage) *
      10n ** BigInt(config.price.decimals + config.quantity.decimals)) /
    (1_000_000n * BigInt(priceAtoms));
  return safe((size / step) * step);
}

export function closeOrder(position, book, config, slippageBps) {
  if (!Number.isSafeInteger(Number(position.size_atoms)) || Number(position.size_atoms) === 0)
    throw Error("This position is no longer open");
  if (!Number.isInteger(slippageBps) || slippageBps < 0 || slippageBps > 500)
    throw Error("Slippage must be between 0 and 5%");
  const long = BigInt(position.size_atoms) > 0n;
  const levels = long ? book?.bids : book?.asks;
  if (!levels?.length) throw Error("No opposing liquidity. Try again when the book has quotes.");
  const best = BigInt(levels[0].price);
  const tick = BigInt(config.price.step);
  // Round toward the best quote, never beyond the specified slippage bound.
  const numerator = best * BigInt(long ? 10_000 - slippageBps : 10_000 + slippageBps);
  const denominator = 10_000n * tick;
  const limit =
    (long ? (numerator + denominator - 1n) / denominator : numerator / denominator) * tick;
  const size = BigInt(position.size_atoms);
  const quantity = safe(size < 0n ? -size : size);
  if (quantity % config.quantity.step) throw Error("Position size does not match the market lot");
  return {
    side: long ? "Sell" : "Buy",
    price: safe(limit),
    quantity,
    order_type: "IOC",
    reduce_only: true,
  };
}

export function decimalInput(value, decimals) {
  const n = BigInt(value),
    scale = 10n ** BigInt(decimals);
  return `${n / scale}${decimals ? "." + String(n % scale).padStart(decimals, "0") : ""}`;
}
