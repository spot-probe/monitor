/**
 * 成本口径的**纯算术** —— 从 `Overview.tsx` 抽出来的，理由有二：
 *
 * 1. 它此前没法测：`costRow` 是组件内的模块私有函数，而这段算术直接决定"每月开销"与
 *    "剩余价值"两个 KPI 的数值。一次真实 bug（一次性买断被当成月付、剩余价值被放大到
 *    原价三倍）就是在这里发生的，却没有任何用例能挡住。
 * 2. 仓里原本有**两份** `cycleMonths`（`lib/format.ts` 那份对 `once` 返回 0，`Overview`
 *    这份回落到 1）。两份都各自有道理，但成本口径只该有一个真相 —— 这里的这份是成本用的，
 *    `format.ts` 那份服务于"续费后的到期日"，语义不同，不要互相替换。
 */

/** 一次性买断：**没有周期** —— 不是"认不出"，是**已知的没有**。
 *
 *  必须与"认不出"分开：`cycleMonths` / `cycleDays` 的兜底（按 1 个月 / 30 天）是为了
 *  "宁可少算一项，也不要静默漏掉整台机器"，而 `once` 是明确的语义 —— 它压根没有周期可言。
 */
export function isOneOff(cycle: string): boolean {
  return (cycle || "").trim().toLowerCase() === "once"
}

/** 计费周期 → 月数。认不出的一律按 1 个月（宁可少算一项，也不要静默漏掉整台机器）。 */
export function cycleMonths(cycle: string): number {
  switch ((cycle || "").toLowerCase()) {
    case "quarterly": case "quarter": case "3m": return 3
    case "semiannual": case "semi-annual": case "halfyear": case "half-yearly": case "6m": return 6
    case "yearly": case "annual": case "annually": case "year": case "12m": return 12
    default: return 1
  }
}

/** 计费周期的**天数**，用于"剩余价值 = 价格 ÷ 周期天数 × 剩余天数"（维护者给的口径）。
 *  与 `cycleMonths` 分开：那个用来摊平成"每月"，这个用来按天折算剩余。 */
export function cycleDays(cycle: string): number {
  switch ((cycle || "").toLowerCase()) {
    case "quarterly": case "quarter": case "3m": return 91
    case "semiannual": case "semi-annual": case "halfyear": case "half-yearly": case "6m": return 182
    case "yearly": case "annual": case "annually": case "year": case "12m": return 365
    default: return 30
  }
}

/**
 * **每月开销**（已换算成本位币）。
 *
 * ⚠️ 一次性买断返回 **0**，而不是 `price * rate`：它不是每月重复支出。此前它走兜底被当成
 * 月付，一台 ¥500 的买断让月开销凭空多出 ¥500。
 */
export function monthlyCost(price: number, cycle: string, rate: number): number {
  if (isOneOff(cycle)) return 0
  return (price / cycleMonths(cycle)) * rate
}

/**
 * **剩余价值**（已换算成本位币）：价格 ÷ 周期天数 × 剩余天数。
 *
 * ⚠️ 一次性买断返回 **0**：它没有周期当分母，此前走兜底（30 天）后，剩 90 天时会算出
 * **原价的三倍** —— 比漏算更糟，因为它看起来像"钱还在"。买断也没有"已付但尚未用到的时间
 * 价值"这回事（买断即拥有），所以它本就不该进这个口径。
 */
export function residualValue(price: number, cycle: string, remainingDays: number, rate: number): number {
  if (isOneOff(cycle)) return 0
  return (price / cycleDays(cycle)) * remainingDays * rate
}
