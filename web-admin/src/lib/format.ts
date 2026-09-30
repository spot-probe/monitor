const UNITS = ["B", "KB", "MB", "GB", "TB", "PB"]

const unitOf = (n: number) => Math.min(Math.floor(Math.log(n) / Math.log(1024)), UNITS.length - 1)

/**
 * 1024-based, as VPS dashboards and `df` report bytes, but labelled MB/GB the way
 * `df -h` and hosting plans write them. Three significant digits by default. Kept
 * in step with the theme's copy of this file.
 */
export function bytes(n: number, digits?: number): string {
  // `< 1` rather than `< 0`: a fraction of a byte puts `unitOf` at -1 and prints
  // "512 undefined".
  if (!n || n < 1) return "0 B"
  const i = unitOf(n)
  const v = n / 1024 ** i
  return `${v.toFixed(i === 0 ? 0 : (digits ?? (v >= 100 ? 0 : v >= 10 ? 1 : 2)))} ${UNITS[i]}`
}

export function uptime(seconds: number): string {
  if (!seconds) return "—"
  const d = Math.floor(seconds / 86400)
  const h = Math.floor((seconds % 86400) / 3600)
  const m = Math.floor((seconds % 3600) / 60)
  return d > 0 ? `${d} 天 ${h} 小时` : h > 0 ? `${h} 小时 ${m} 分` : `${m} 分`
}

/**
 * No expiry and no traffic cap are both rendered as the absence of a ceiling.
 * U+221E rather than the emoji, which arrives as a coloured tile from whatever
 * font the browser provides; this inherits the text colour and size.
 */
export const FOREVER = "∞"

/// The five this panel has always shown a symbol for. Intl's narrow form is
/// exactly the symbol we used (`$`, `¥`, `€`, `£`, `¥`); its full form would
/// prefix `US$`/`JP¥` instead, which is a change nobody asked for.
const NARROW = new Set(["USD", "CNY", "EUR", "GBP", "JPY"])

/// Built once per currency: a formatter holds the locale data it needs, and this
/// runs for every row of the node table.
const FORMATTERS = new Map<string, Intl.NumberFormat>()

function formatter(currency: string): Intl.NumberFormat | undefined {
  const known = FORMATTERS.get(currency)
  if (known) return known
  try {
    const made = new Intl.NumberFormat("zh-CN", {
      style: "currency",
      currency,
      // Everything else takes the full symbol: the narrow one collapses SGD, AUD
      // and HKD into a bare `$`, which is actively wrong in a price column that
      // can hold several currencies at once.
      currencyDisplay: NARROW.has(currency) ? "narrowSymbol" : "symbol",
    })
    FORMATTERS.set(currency, made)
    return made
  } catch {
    // Not a code Intl knows. The hub only stores three letters, so this is a
    // value from before that check existed.
    return undefined
  }
}

/// A price with the currency in front of the number, always. The code used to
/// trail the amount for every currency outside a five-entry table -- `100.00
/// HKD` in the same column as `$100.00` -- which read as two styles once any
/// three-letter code could be set (#71, upstream issue #76).
export function money(amount: number, currency: string): string {
  const f = formatter(currency)
  if (f) return f.format(amount)
  // Grouped like the rest: one ungrouped amount in a column of grouped ones is
  // the same inconsistency in a different disguise.
  const plain = new Intl.NumberFormat("zh-CN", { minimumFractionDigits: 2, maximumFractionDigits: 2 }).format(amount)
  return currency ? `${currency}\u00A0${plain}` : plain
}

// The hub stores these lengths under a name and any other as `<n>m`.
const NAMED_CYCLES: Record<string, number> = { monthly: 1, quarterly: 3, semiannual: 6, yearly: 12, biennial: 24, triennial: 36 }

/** A billing cycle in months: 0 for one-off, NaN when unrecognized. */
export function cycleMonths(cycle: string): number {
  return cycle === "once" ? 0 : NAMED_CYCLES[cycle] ?? Number(/^(\d+)m$/.exec(cycle)?.[1])
}

/** The unit the billing form's cycle controls offer. */
export type CycleUnit = "months" | "years" | "once"

/**
 * What the two cycle controls start at for a stored cycle, and whether the panel
 * could read it at all. `readable: false` means the stored string is neither a
 * named length nor `<n>m` -- an older hub accepted any string, so a database may
 * hold `weekly` or `2y` -- and the controls start somewhere neutral while the
 * raw value is shown beside them.
 */
export function cycleFields(stored: string): { unit: CycleUnit; count: string; readable: boolean } {
  const months = cycleMonths(stored)
  if (!Number.isFinite(months)) return { unit: "months", count: "1", readable: false }
  const wholeYears = months !== 0 && months % 12 === 0
  return {
    unit: months === 0 ? "once" : wholeYears ? "years" : "months",
    count: String(wholeYears ? months / 12 : months || 1),
    readable: true,
  }
}

/** The `<n>m` spelling of what the controls hold, or `once`. */
export function cycleValue(unit: CycleUnit, count: string): string {
  return unit === "once" ? "once" : `${Number(count) * (unit === "years" ? 12 : 1)}m`
}

/** Whether what the controls hold is something the hub will store: a whole
 * number of months or years, from one month to the hundred years it caps at. */
export function cycleOk(unit: CycleUnit, count: string): boolean {
  if (unit === "once") return true
  const months = Number(count) * (unit === "years" ? 12 : 1)
  return Number.isInteger(months) && months >= 1 && months <= 1_200
}

/**
 * The `billing_cycle` to send back. A pair of controls nobody touched means
 * "leave the stored length alone" -- which is also what keeps a stored length
 * this panel cannot read from being rewritten into one the form invented.
 * Where the spelling changed but the length did not, the stored spelling stands:
 * the hub keeps named lengths under their names.
 */
export function cyclePatch(
  stored: string,
  started: { unit: CycleUnit; count: string },
  now: { unit: CycleUnit; count: string },
): string {
  if (now.unit === started.unit && now.count === started.count) return stored
  return cycleValue(now.unit, now.count)
}

/**
 * Usage counted as the plan bills it: summing both directions unconditionally
 * would measure a node billed on upload alone against the wrong figure.
 */
export function monthUsage(node: { month_rx: number; month_tx: number; traffic_mode: string }): number {
  switch (node.traffic_mode) {
    case "up":
      return node.month_tx
    case "down":
      return node.month_rx
    case "max":
      return Math.max(node.month_rx, node.month_tx)
    default:
      return node.month_rx + node.month_tx
  }
}
