import type { MarketSnapshot } from "@/bindings/MarketSnapshot"

const assets: Record<number, string> = { 0: "USD", 1: "BTC", 2: "ETH", 3: "SOL", 4: "BNB", 5: "XRP" }

export function assetName(id: number): string { return assets[id] ?? `asset ${id}` }

export function marketName(market: MarketSnapshot): string {
  return `${assetName(market.config.base_asset)}-${assetName(market.config.quote_asset)} ${market.config.market_type.kind === "perp" ? "perp" : "spot"}`
}
