export function parseAtoms(text: string, decimals: number, step = 1): number {
  if (!Number.isInteger(decimals) || decimals < 0 || decimals > 18 || !Number.isSafeInteger(step) || step < 1) {
    throw new Error("Invalid market unit configuration")
  }
  const match = /^(\d+)(?:\.(\d+))?$/.exec(text.trim())
  if (!match || (match[2]?.length ?? 0) > decimals) {
    throw new Error(`Enter a positive number with at most ${decimals} decimal places`)
  }
  const atoms = BigInt(match[1]) * 10n ** BigInt(decimals)
    + BigInt((match[2] ?? "").padEnd(decimals, "0") || "0")
  if (atoms <= 0n || atoms % BigInt(step) !== 0n || atoms > BigInt(Number.MAX_SAFE_INTEGER)) {
    throw new Error("Amount is zero, off-step, or too large")
  }
  return Number(atoms)
}

export function formatAtoms(value: number, decimals: number): string {
  if (!Number.isSafeInteger(value)) return String(value)
  const sign = value < 0 ? "-" : ""
  const digits = BigInt(Math.abs(value)).toString().padStart(decimals + 1, "0")
  if (decimals === 0) return sign + digits
  return `${sign}${digits.slice(0, -decimals)}.${digits.slice(-decimals)}`
}
