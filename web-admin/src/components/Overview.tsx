import { useEffect, useState, type ReactNode } from "react"
import { ArrowUpCircle, CalendarClock, CircleAlert } from "lucide-react"

import { api, behind } from "@/lib/api"
import type { Node } from "@/lib/api"
import { bytes, monthUsage } from "@/lib/format"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardAction, CardContent, CardHeader, CardTitle } from "@/components/ui/card"

// `daysUntil` 与 `Expiry` 仍在 `Admin.tsx`（节点表也在用）。从那里 import 会形成**循环引用**，
// 但两者都是函数声明 —— 声明会提升，且只在渲染时调用，所以这个环是安全的；比把它们复制一份好。
import { Expiry, Help, daysUntil } from "./Admin"
import { Dialog, DialogContent, DialogHeader, DialogTitle } from "@/components/ui/dialog"

/// 全队按天的趋势（对应 hub 的 `Db::overview_daily`）。
type SeriesPoint = {
  day_ts: number
  rx: number
  tx: number
  rx_peak: number
  tx_peak: number
  cpu: number
  mem: number
  disk: number
  /** 当天**最热的那一台**（逐节点算的），给「均值 + 分布带」用。 */
  cpu_max: number
  mem_max: number
  disk_max: number
  /** 当天**有数据覆盖的秒数**：平均速率要除以它，不能除以 86400。 */
  covered: number
}

/// 按档位缓存已取到的趋势。
///
/// **放在模块作用域是必要的**：总览页切走会卸载组件，state 随之丢失，于是每次切回来都重新请求 ——
/// 这正是维护者报的「反复加载」。缓存按档位存，切回来立刻有数，只有没取过的档位才发请求。
const seriesCache = new Map<number, SeriesPoint[]>()

/// 取全队趋势；`days` 变了就取那一档（已取过的档位直接给缓存）。
function useOverviewSeries(days: number) {
  const [rows, setRows] = useState<SeriesPoint[] | null>(() => seriesCache.get(days) ?? null)
  useEffect(() => {
    const hit = seriesCache.get(days)
    if (hit) {
      setRows(hit)
      return
    }
    let stop = false
    api<{ days: SeriesPoint[] }>(`/overview/series?days=${days}`)
      .then((d) => {
        seriesCache.set(days, d.days)
        if (!stop) setRows(d.days)
      })
      .catch(() => {
        if (!stop) setRows([])
      })
    return () => {
      stop = true
    }
  }, [days])
  return rows
}

/// 日期短标签。hub 给的是 **UTC 日**的时间戳，这里按**本地**时区显示 —— 服务端若转成日期字符串，
/// 东八区会把「日」整体挪错一天（日历那里已经踩过一次）。
function dayLabel(ts: number) {
  const d = new Date(ts * 1000)
  return `${d.getMonth() + 1}/${d.getDate()}`
}

/// 轴上限：按 tab 各自的算法算**唯一一次**。三张图与悬浮胶囊共用 —— 两处各算一遍迟早会错开。
function chartTop(tab: "traffic" | "bandwidth" | "resource", rows: SeriesPoint[]): number {
  if (rows.length === 0) return 1
  if (tab === "traffic") return Math.max(1, ...rows.map((r) => r.rx + r.tx))
  if (tab === "bandwidth")
    return Math.max(1, ...rows.map((r) => Math.max(r.covered > 0 ? r.rx / r.covered : 0, r.covered > 0 ? r.tx / r.covered : 0)))
  const values = rows.flatMap((r) => [r.cpu, r.mem, r.disk, r.cpu_max, r.mem_max, r.disk_max])
  return Math.max(10, Math.ceil((Math.max(...values) * 1.1) / 10) * 10)
}

/// 值 → viewBox 里的 y。**唯一**的一份映射。
function chartY(v: number, top: number): number {
  const t = Math.min(top, Math.max(0, v)) / top
  return CHART_H - CHART_PAD - t * (CHART_H - CHART_PAD * 2)
}

const CHART_W = 640
/// 柱子上限。**三张图共用**：只取到 4 天时，按比例分到的宽度会把柱子拉成砖块。
/// 上一轮我把它只写进流量图，带宽图就漏了 —— 所以提到这里，谁画柱子谁用它。
const CHART_BAR_MAX = 16
const CHART_H = 150
const CHART_PAD = 18

/// 右上角的状态徽章。卡片外框保持统一，状态只在这里和主数字上出现。
/// 颜色一律走 token（`ok-fg` / `warn-fg` / `danger-fg`），不写死调色板 —— 暗色主题才成立。
function StatusPill({ tone, text }: { tone: "ok" | "warn" | "bad" | "muted"; text: string }) {
  const cls = {
    ok: "bg-ok-fg/12 text-ok-fg",
    warn: "bg-warn-fg/15 text-warn-fg",
    bad: "bg-danger-fg/12 text-danger-fg",
    muted: "bg-muted text-muted-foreground",
  }[tone]
  return <span className={`shrink-0 rounded-full px-2 py-0.5 text-[11px] leading-tight font-medium ${cls}`}>{text}</span>
}

/// 悬停浮层：一条竖准星 + 一张跟随鼠标的小卡。
///
/// 用 **HTML** 而不是 SVG 文本有两个好处：token 与 Tailwind 直接可用；贴边时不会像 SVG 那样被
/// viewBox 裁掉（`left` 夹在 12%–88% 之间，卡片永远完整）。
function HoverOverlay({ rows, index, series, top, single }: {
  rows: SeriesPoint[]
  index: number
  series: { c: string; t: string; v: string; raw: number }[]
  top: number
  /** 这张图是不是单轴（双轴时 Y 胶囊会让人分不清指向哪根轴）。 */
  single: boolean
}) {
  const pct = ((index + 0.5) / rows.length) * 100
  return (
    <div className="pointer-events-none absolute inset-0">
      <span className="absolute top-0 bottom-5 w-px border-l border-dashed border-foreground/30" style={{ left: `${pct}%` }} />
      {/* Y 轴数值胶囊：贴在左边轴上，位置由**同一份映射**（chartY）算出 —— 胶囊与线永远对齐。
          显示用格式化后的文本（`2.34 TB/s`），不是 p2 里那个原始字节数（`16,914,893.62`）——
          那个读者读不出量级。取第一条**未被隐藏**的序列。
          **流量 tab 不显示**：那张图是双 Y 轴（柱用左轴、累计线用右轴），一个胶囊说不清它指的是哪根轴。 */}
      {single && series[0] && (
        <span
          className="tnum absolute -translate-y-1/2 rounded-md bg-foreground px-1.5 py-0.5 text-[11px] leading-tight font-medium text-background"
          style={{ top: `${(chartY(series[0].raw, top) / CHART_H) * 100}%` }}
        >
          {series[0].v}
        </span>
      )}
      {/* X 轴上的日期胶囊：准星最有用的部分 —— 竖线指到哪一天，轴上就写哪一天，
          不用回头去找浮层。用 token（foreground/background 反色），暗色主题下自动成立。 */}
      <span
        className="tnum absolute bottom-0 -translate-x-1/2 rounded-md bg-foreground px-1.5 py-0.5 text-[11px] leading-tight font-medium text-background"
        style={{ left: `${Math.min(Math.max(pct, 5), 95)}%` }}
      >
        {dayLabel(rows[index].day_ts)}
      </span>
      <div
        className="absolute top-1 z-10 min-w-32 rounded-lg border bg-background p-2.5 shadow-lg"
        style={{ left: `${Math.min(Math.max(pct, 12), 88)}%`, transform: "translateX(-50%)" }}
      >
        <div className="tnum mb-1.5 text-xs font-medium">{dayLabel(rows[index].day_ts)}</div>
        <div className="space-y-1">
          {series.map((s) => (
            <div key={s.t} className="flex items-center gap-2 whitespace-nowrap text-xs">
              <span className={`size-2 shrink-0 rounded-full ${s.c}`} />
              <span className="text-muted-foreground">{s.t}</span>
              <span className="tnum ml-auto font-medium">{s.v}</span>
            </div>
          ))}
        </div>
      </div>
    </div>
  )
}

/// 三档刻度（0 / 一半 / 满）与左右轴数值。**三张图共用**：
/// 上一轮我只在流量图里画了刻度，带宽图与资源图就漏了 —— 根因是每张图各写了一遍。
function ChartTicks({ left, right, format }: { left: number; right?: number; format: (v: number) => string }) {
  const y = (t: number) => CHART_H - CHART_PAD - t * (CHART_H - CHART_PAD * 2)
  return (
    <>
      {[0, 0.5, 1].map((t) => (
        <g key={t}>
          <line x1={0} y1={y(t)} x2={CHART_W} y2={y(t)} className="stroke-border" strokeWidth="1" strokeDasharray={t === 0 ? undefined : "2 3"} />
          <text x={0} y={y(t) - 2} fontSize="9" className="fill-muted-foreground">{format(left * t)}</text>
          {right !== undefined && (
            <text x={CHART_W} y={y(t) - 2} fontSize="9" textAnchor="end" className="fill-warn-fg">{format(right * t)}</text>
          )}
        </g>
      ))}
    </>
  )
}

/// 面板里三种图共用的横轴：一条基线 + 首尾日期。
function ChartAxis({ rows }: { rows: SeriesPoint[] }) {
  return (
    <>
      <line x1={0} y1={CHART_H - CHART_PAD} x2={CHART_W} y2={CHART_H - CHART_PAD} className="stroke-border" strokeWidth="1" />
      {/* 标签落在**首尾桶的中心**上：折线/柱子的落点就是桶中心，画在两端会看起来对不齐。 */}
      <text x={CHART_W / rows.length / 2} y={CHART_H - 4} fontSize="9" textAnchor="middle" className="fill-muted-foreground">
        {dayLabel(rows[0].day_ts)}
      </text>
      <text x={CHART_W - CHART_W / rows.length / 2} y={CHART_H - 4} fontSize="9" textAnchor="middle" className="fill-muted-foreground">
        {dayLabel(rows[rows.length - 1].day_ts)}
      </text>
    </>
  )
}

/// 流量：堆叠柱（入在下、出在上）+ **右轴**累积折线，**两侧各有刻度**。
///
/// 两根轴量纲差一个数量级（当天 vs 累计）。只标一个最大值时，那条折线看着像浮在空中，读者没法
/// 核对它落在哪一档 —— 所以左右各画三条刻度线并标数值。
function TrafficChart({ rows, hidden }: { rows: SeriesPoint[]; hidden?: Set<string> }) {
  const top = chartTop("traffic", rows)
  const cum = rows.map((_, i) => rows.slice(0, i + 1).reduce((n, r) => n + r.rx + r.tx, 0))
  const cumTop = Math.max(1, ...cum)
  const bw = CHART_W / Math.max(1, rows.length)
  // 柱子**限宽**：只取到 4 天时，按比例分到的宽度会把柱子拉成砖块。
  const bar = Math.min(bw * 0.7, CHART_BAR_MAX)
  const bottom = CHART_H - CHART_PAD
  const yOf = (v: number, m: number) => chartY(v, m)
  // 30/90 天时每格都标会糊在一起，按数量抽稀。
  const step = Math.max(1, Math.ceil(rows.length / 6))
  return (
    <svg viewBox={`0 0 ${CHART_W} ${CHART_H}`} className="w-full" role="img" aria-label="每日入出站流量与周期累计">
      <ChartTicks left={top} right={cumTop} format={bytes} />
      {rows.map((r, i) => {
        const x = i * bw + (bw - bar) / 2
        const mid = yOf(r.rx, top)
        const hi = yOf(r.rx + r.tx, top)
        return (
          <g key={r.day_ts}>
            {!hidden?.has("入站") && <rect x={x} y={mid} width={bar} height={Math.max(0, bottom - mid)} className="fill-primary" />}
            {!hidden?.has("出站") && <rect x={x} y={hi} width={bar} height={Math.max(0, mid - hi)} className="fill-primary/45" />}
            {/* 两段是同色系的不同明度，深色主题下交界会糊 —— 用一条 1px 分界线靠结构说清楚，而不是靠色差。 */}
            <line x1={x} y1={mid} x2={x + bar} y2={mid} className="stroke-background" strokeWidth="1" />
          </g>
        )
      })}
      {!hidden?.has("累计") && (
        <polyline fill="none" className="stroke-warn-fg" strokeWidth="1.5"
          points={cum.map((v, i) => `${i * bw + bw / 2},${yOf(v, cumTop)}`).join(" ")} />
      )}
      {/* 高峰标记：当天总量最大的那根柱子上方标出来 —— 运维第一眼想看的就是它。 */}
      {(() => {
        let peak = 0
        rows.forEach((r, i) => {
          if (r.rx + r.tx > rows[peak].rx + rows[peak].tx) peak = i
        })
        const x = peak * bw + bw / 2
        const yy = yOf(rows[peak].rx + rows[peak].tx, top) - 6
        return (
          <g>
            <rect x={x - 15} y={yy - 11} width={30} height={13} rx={6} className="fill-primary" />
            <text x={x} y={yy - 1} fontSize="9" textAnchor="middle" className="fill-primary-foreground">高峰</text>
          </g>
        )
      })()}
      {rows.map((r, i) =>
        i % step === 0 || i === rows.length - 1 ? (
          <text key={r.day_ts} x={i * bw + bw / 2} y={CHART_H - 4} fontSize="9" textAnchor="middle" className="fill-muted-foreground">
            {dayLabel(r.day_ts)}
          </text>
        ) : null,
      )}
    </svg>
  )
}

/// 带宽：**入站与出站两条日均速率线**（照维护者给的原型）。
///
/// 两条线都是**同一个量纲**（字节/秒）：各自都是「当天总字节 ÷ **有数据覆盖的秒数**」。
/// 之前的柱子画的是当天总字节、线是瞬时峰值，两个量纲画在一根轴上 —— 那才是这张图原来看着别扭的原因。
/// 颜色用面板自己的 token（入站 ok-fg / 出站 primary），**不写死调色板**，暗色主题才成立。
function BandwidthChart({ rows, hidden }: { rows: SeriesPoint[]; hidden?: Set<string> }) {
  const rate = (v: number, r: SeriesPoint) => (r.covered > 0 ? v / r.covered : 0)
  const inRate = (r: SeriesPoint) => rate(r.rx, r)
  const outRate = (r: SeriesPoint) => rate(r.tx, r)
  const top = chartTop("bandwidth", rows)
  const bw = CHART_W / Math.max(1, rows.length)
  const yOf = (v: number) => chartY(v, top)
  const line = (f: (r: SeriesPoint) => number) => rows.map((r, i) => `${i * bw + bw / 2},${yOf(f(r))}`).join(" ")
  const step = Math.max(1, Math.ceil(rows.length / 6))
  return (
    <svg viewBox={`0 0 ${CHART_W} ${CHART_H}`} className="w-full" role="img" aria-label="每日入站与出站带宽速率">
      <ChartTicks left={top} format={(v) => `${bytes(v)}/s`} />
      {!hidden?.has("入站") && <polyline fill="none" className="stroke-ok-fg" strokeWidth="1.5" points={line(inRate)} />}
      {/* 出站用**虚线**：两条速率相等时（预览夹具就是）实线会完全重合，看不出是两条。
          改线型是**画法**上的区分，不动数据 —— 不能把其中一条挪开，那是伪造差异。 */}
      {!hidden?.has("出站") && (
        <polyline fill="none" className="stroke-primary" strokeWidth="1.5" strokeDasharray="5 3" points={line(outRate)} />
      )}
      <ChartAxis rows={rows} />
      {rows.map((r, i) =>
        i % step === 0 || i === rows.length - 1 ? (
          <text key={r.day_ts} x={i * bw + bw / 2} y={CHART_H - 4} fontSize="9" textAnchor="middle" className="fill-muted-foreground">
            {dayLabel(r.day_ts)}
          </text>
        ) : null,
      )}
    </svg>
  )
}

/// 资源：cpu / 内存 / 硬盘 三条线，都是百分比。
///
/// **Y 轴跟着数据走**，不再死锁 0–100%：集群的平均负载通常只有二三成，锁死在 100% 会把实际波动
/// 压成一根直线 —— 那时「系统很稳」和「探针没采到数」看起来一模一样。上取整到 10 的倍数。
function ResourceChart({ rows, hidden }: { rows: SeriesPoint[]; hidden?: Set<string> }) {
  // 轴上限要**含分布带的上沿** —— 只看均值的话带会溢出轴（这是 A 与 B 唯一真正的耦合处）。
  const top = chartTop("resource", rows)
  const yOf = (v: number) => chartY(v, top)
  const line = (key: "cpu" | "mem" | "disk") =>
    rows.map((r, i) => `${(i + 0.5) * (CHART_W / Math.max(1, rows.length))},${yOf(r[key])}`).join(" ")
  // 分布带：从**均值线**铺到**最热那台** —— 平均值会掩盖「99 台闲置、1 台打满」，
  // 一层带就能把这件事摆出来。带跟着它那条线的图例开关走（关掉 CPU，它的带也一起关）。
  const band = (avgKey: "cpu" | "mem" | "disk", maxKey: "cpu_max" | "mem_max" | "disk_max") => {
    const up = rows.map((r, i) => `${(i + 0.5) * (CHART_W / Math.max(1, rows.length))},${yOf(r[maxKey])}`)
    const down = [...rows].reverse().map((r, i) => `${(rows.length - 1 - i + 0.5) * (CHART_W / Math.max(1, rows.length))},${yOf(r[avgKey])}`)
    return [...up, ...down].join(" ")
  }
  return (
    <svg viewBox={`0 0 ${CHART_W} ${CHART_H}`} className="w-full" role="img" aria-label="全队 cpu 内存 硬盘 占用率">
      <ChartTicks left={top} format={(v) => `${Math.round(v)}%`} />
      {/* 80% 阈值线**有条件地画**：轴上限低于 80 时它根本不在画面里，画了只会让人以为没数据。
          动态轴之后这条才有意义 —— 它出现的那一刻，正是负载真的接近危险区的时候。 */}
      {top > 80 && (
        <g>
          <line
            x1={0}
            y1={yOf(80)}
            x2={CHART_W}
            y2={yOf(80)}
            className="stroke-danger-fg"
            strokeWidth="1"
            strokeDasharray="4 3"
          />
          <text x={CHART_W - 2} y={yOf(80) - 3} fontSize="9" textAnchor="end" className="fill-danger-fg">80% 阈值</text>
        </g>
      )}
      <ChartAxis rows={rows} />
      {!hidden?.has("CPU") && (
        <>
          <polygon className="fill-primary/15" points={band("cpu", "cpu_max")} />
          <polyline fill="none" className="stroke-primary" strokeWidth="1.5" points={line("cpu")} />
        </>
      )}
      {!hidden?.has("内存") && (
        <>
          <polygon className="fill-warn-fg/15" points={band("mem", "mem_max")} />
          <polyline fill="none" className="stroke-warn-fg" strokeWidth="1.5" points={line("mem")} />
        </>
      )}
      {!hidden?.has("硬盘") && (
        <>
          <polygon className="fill-ok-fg/15" points={band("disk", "disk_max")} />
          <polyline fill="none" className="stroke-ok-fg" strokeWidth="1.5" points={line("disk")} />
        </>
      )}
    </svg>
  )
}

/// 趋势卡：三张摘要卡（只对流量有意义）+ 三个 tab + 三档范围（同一套胶囊样式）。
function TrendCard({ rows, tab, setTab, range, setRange }: {
  rows: SeriesPoint[] | null
  tab: "traffic" | "bandwidth" | "resource"
  setTab: (t: "traffic" | "bandwidth" | "resource") => void
  range: number
  setRange: (r: number) => void
}) {
  const tabs = [
    { key: "traffic", label: "流量" },
    { key: "bandwidth", label: "带宽" },
    { key: "resource", label: "资源" },
  ] as const
  const legend =
    tab === "traffic"
      ? [{ c: "bg-primary", t: "入站" }, { c: "bg-primary/45", t: "出站" }, { c: "bg-warn-fg", t: "累计" }]
      : tab === "bandwidth"
        ? [{ c: "bg-ok-fg", t: "入站" }, { c: "bg-primary", t: "出站" }]
        : [{ c: "bg-primary", t: "CPU" }, { c: "bg-warn-fg", t: "内存" }, { c: "bg-ok-fg", t: "硬盘" }]

  // 摘要卡：**每个 tab 都有三项**，各是按那个 tab 真正要看的数。都由已取到的序列算出，不额外请求。
  // 三个 tab 都给三项的另一个理由：卡片高度不会因为切 tab 而跳（截图里「脚很轻」的真因就是这个跳）。
  const list = rows ?? []
  // 「最高流量日」要的是**当天总量最大**的那一天，而不是「rx 最大的那天 + tx 最大的那天」——
  // 后者的两个最大值可能不在同一天，加出来的数从未发生过。
  const totals = list.map((r) => r.rx + r.tx)
  const totalOf = (key: "rx" | "tx" | "rx_peak" | "tx_peak") => list.map((r) => r[key])
  const trafficTotal = totals
  const busiest = totals.length > 0 ? totals.indexOf(Math.max(...totals)) : -1
  // 日均出站 = 区间总字节 ÷ 区间覆盖秒数。**不是**逐日平均后再相加，也不是除以天数 —— 那是错的。
  const rxSum = totalOf("rx").reduce((n, v) => n + v, 0)
  const txSum = totalOf("tx").reduce((n, v) => n + v, 0)
  const coveredSum = list.reduce((n, r) => n + r.covered, 0)
  // 平均速率 = **区间总字节 ÷ 区间覆盖秒数**（不是逐日平均再相加，也不除以天数）。
  const rxAvg = coveredSum > 0 ? rxSum / coveredSum : 0
  const txAvg = coveredSum > 0 ? txSum / coveredSum : 0
  // 最高日均带宽：逐日算「当天日均（入+出）」，取最大的那天。
  const dailyRate = list.map((r) => (r.covered > 0 ? (r.rx + r.tx) / r.covered : 0))
  const busiestIdx = dailyRate.length > 0 ? dailyRate.indexOf(Math.max(...dailyRate)) : -1
  const busiestSum = busiestIdx >= 0 ? dailyRate[busiestIdx] : 0
  const busiestSumDay = busiestIdx >= 0 ? dayLabel(list[busiestIdx].day_ts) : ""
  // 按覆盖秒数加权：覆盖长的日子更能代表这段时间。与带宽的平均速率同一套口径。
  const weighted = (key: "cpu" | "mem" | "disk") =>
    coveredSum > 0 ? list.reduce((n, r) => n + r[key] * r.covered, 0) / coveredSum : 0
  const stats =
    tab === "traffic"
      ? [
          { label: "区间累计", value: bytes(trafficTotal.reduce((n, v) => n + v, 0)), hint: `近 ${range} 天` },
          { label: "日均流量", value: bytes(list.length > 0 ? trafficTotal.reduce((n, v) => n + v, 0) / list.length : 0), hint: "按有数据的逻辑日" },
          { label: "最高流量日", value: busiest >= 0 ? bytes(totals[busiest]) : "—", hint: busiest >= 0 ? dayLabel(list[busiest].day_ts) : "" },
        ]
      : tab === "bandwidth"
        ? [
            { label: "平均入站", value: `${bytes(rxAvg)}/s`, hint: "节点日均速率汇总" },
            { label: "平均出站", value: `${bytes(txAvg)}/s`, hint: "节点日均速率汇总" },
            // 「最高日均带宽」= **当天日均（入+出）最高**的那一天。注意它不是「峰值速率」——
            // 峰值是小时级的瞬时值，这里要的是日平均的量级，两者别混。
            { label: "最高日均带宽", value: `${bytes(busiestSum)}/s`, hint: busiestSumDay },
          ]
        : [
            // 集群大盘要看的是**水位**（平均），不是某一天的尖峰 —— 尖峰由「均值 + 分布带」
            // 在图上表达（方案 B，需要端点新增字段，下一步做）。
            // 权重用 covered：覆盖时间长的日子更有代表性，与带宽那边的口径一致。
            { label: "平均 CPU", value: `${weighted("cpu").toFixed(1)}%`, hint: `近 ${range} 天` },
            { label: "平均内存", value: `${weighted("mem").toFixed(1)}%`, hint: `近 ${range} 天` },
            { label: "平均硬盘", value: `${weighted("disk").toFixed(1)}%`, hint: `近 ${range} 天` },
          ]

  // 悬停索引。放在趋势卡这一层，三张图共用同一套交互 —— 三张图本身不用改。
  const [hover, setHover] = useState<number | null>(null)
  // 图例开关：点一下隐藏/恢复某条序列。不能靠「挪开一条线」来区分重合的序列 —— 那是伪造数据。
  const [hidden, setHidden] = useState<Set<string>>(new Set())
  const toggle = (t: string) =>
    setHidden((prev) => {
      const next = new Set(prev)
      if (next.has(t)) next.delete(t)
      else next.add(t)
      return next
    })
  const onMove = (e: React.MouseEvent<HTMLDivElement>) => {
    if (!rows || rows.length === 0) return
    const r = e.currentTarget.getBoundingClientRect()
    const x = ((e.clientX - r.left) / r.width) * CHART_W
    setHover(Math.max(0, Math.min(rows.length - 1, Math.floor(x / (CHART_W / rows.length)))))
  }
  // 浮层里要显示的序列：按 tab 取，和图上画的是同一批数（累计在这里现算）。
  const cumAt = (i: number) => (rows ?? []).slice(0, i + 1).reduce((n, r) => n + r.rx + r.tx, 0)
  const hoverSeries = (i: number) =>
    !rows
      ? []
      : tab === "traffic"
        ? [
            { c: "bg-primary", t: "入站", v: bytes(rows[i].rx), raw: rows[i].rx },
            { c: "bg-primary/45", t: "出站", v: bytes(rows[i].tx), raw: rows[i].tx },
            { c: "bg-warn-fg", t: "累计", v: bytes(cumAt(i)), raw: cumAt(i) },
          ]
        : tab === "bandwidth"
          ? [
              { c: "bg-ok-fg", t: "入站", v: `${bytes(rows[i].covered > 0 ? rows[i].rx / rows[i].covered : 0)}/s`, raw: rows[i].covered > 0 ? rows[i].rx / rows[i].covered : 0 },
              { c: "bg-primary", t: "出站", v: `${bytes(rows[i].covered > 0 ? rows[i].tx / rows[i].covered : 0)}/s`, raw: rows[i].covered > 0 ? rows[i].tx / rows[i].covered : 0 },
            ]
          : [
              { c: "bg-primary", t: "CPU", v: `${rows[i].cpu.toFixed(1)}% · 最热 ${rows[i].cpu_max.toFixed(1)}%`, raw: rows[i].cpu },
              { c: "bg-warn-fg", t: "内存", v: `${rows[i].mem.toFixed(1)}% · 最热 ${rows[i].mem_max.toFixed(1)}%`, raw: rows[i].mem },
              { c: "bg-ok-fg", t: "硬盘", v: `${rows[i].disk.toFixed(1)}% · 最热 ${rows[i].disk_max.toFixed(1)}%`, raw: rows[i].disk },
            ]

  const pill = (on: boolean) =>
    `tnum rounded-full px-3 py-1 text-xs transition-colors ${on ? "bg-primary font-medium text-primary-foreground" : "text-muted-foreground hover:text-foreground"}`

  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-sm">
          趋势
          <Help>
            日流量由每小时的平均速率乘以时长累加得到（积分近似），不是精确的字节计数。
            「累计」是所选周期内的累加&#8203;，<b>不是各节点的计费周期</b> —— 每台机器有自己的
            traffic_reset_day，两个口径不能混。日期按本地时区显示（服务端按 UTC 日分组），跨日边界会有
            小时级的偏移。
          </Help>
        </CardTitle>
        <CardAction>
          <div className="flex flex-wrap items-center gap-2">
            {/* 两个控件都是「选一个」，所以共用同一种胶囊样式：长得不一样只会让人犹豫。 */}
            <div className="flex items-center rounded-full bg-muted p-0.5">
              {tabs.map((t) => (
                <button key={t.key} type="button" onClick={() => setTab(t.key)} aria-pressed={tab === t.key} className={pill(tab === t.key)}>
                  {t.label}
                </button>
              ))}
            </div>
            <div className="flex items-center rounded-full bg-muted p-0.5">
              {[7, 30, 90].map((d) => (
                <button key={d} type="button" onClick={() => setRange(d)} aria-pressed={range === d} className={pill(range === d)}>
                  {d} 天
                </button>
              ))}
            </div>
          </div>
        </CardAction>
      </CardHeader>
      <CardContent className="space-y-3">
        {rows && rows.length > 0 && (
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-3">
            {stats.map((st) => (
              <div key={st.label} className="rounded-lg bg-muted px-3 py-2">
                <div className="text-xs text-muted-foreground">{st.label}</div>
                <div className="tnum mt-0.5 text-lg leading-tight font-medium">{st.value}</div>
                <div className="text-[11px] text-muted-foreground">{st.hint}</div>
              </div>
            ))}
          </div>
        )}
        <div className="flex flex-wrap items-center justify-end gap-1 text-xs text-muted-foreground">
          {/* 图例可点：这是图表该有的能力，也是两条序列重不重合时唯一能自己分辨的办法。 */}
          {legend.map((l) => {
            const off = hidden.has(l.t)
            return (
              <button
                key={l.t}
                type="button"
                onClick={() => toggle(l.t)}
                aria-pressed={!off}
                title={off ? `显示${l.t}` : `隐藏${l.t}`}
                className={`flex items-center gap-1.5 rounded-full px-2 py-0.5 transition-colors hover:bg-muted ${off ? "opacity-40" : ""}`}
              >
                <span className={`size-2 rounded-full ${l.c}`} />
                <span className={off ? "line-through" : ""}>{l.t}</span>
              </button>
            )
          })}
        </div>
        {!rows || rows.length === 0 ? (
          <p className="py-10 text-center text-xs text-muted-foreground">
            {rows ? "这段时间还没有指标数据。" : "正在读取…"}
          </p>
        ) : (
          <div className="relative" onMouseMove={onMove} onMouseLeave={() => setHover(null)}>
            {tab === "traffic" ? (
              <TrafficChart rows={rows} hidden={hidden} />
            ) : tab === "bandwidth" ? (
              <BandwidthChart rows={rows} hidden={hidden} />
            ) : (
              <ResourceChart rows={rows} hidden={hidden} />
            )}
            {hover !== null && (
              <HoverOverlay
                rows={rows}
                index={hover}
                series={hoverSeries(hover)}
                top={chartTop(tab, rows)}
                single={tab !== "traffic"}
              />
            )}
          </div>
        )}
      </CardContent>
    </Card>
  )
}

/// 总览：一个页面的 KPI + 近期事项 + Agent 版本 + 续费日历。
///
/// 只读 `nodes` 已有的字段（在线、到期日、价格、agent 版本），**不加接口、不加请求**，
/// 也不碰任何既有页面。图表与日历都是手写 —— 面板不引图表库（首屏体积是量过的）。
///
/// 结构统一用面板自己的复合并发件（`Card` + `CardHeader`/`CardTitle`/`CardAction`/`CardContent`），
/// 而不是手写 div —— 同一套槽位才有同一套内边距与标题排版。颜色全部走 token（没有一处写死的调色板
/// 颜色），所以暗色主题自动成立。
export function Overview({ nodes, agentLatest, hub, hubLatest }: { nodes: Node[]; agentLatest: string | null; hub: string; hubLatest: string }) {
  // hub 自身待升级：与第 3 张「agent 待升级」成对。判据复用更新页那一套 `behind()`。
  const hubBehind = behind(hub, hubLatest)

  // 用量榜：**只列设了额度的节点** —— 没设额度的谈"额度用量"没有意义。
  // 按「已用 ÷ 额度」降序：运维要的就是"谁离上限最近"。
  // 口径用 `monthUsage()`（面板已有的那个），所以与「近期事项」里的"流量已达额度"
  // 是**同一把尺**，不会两处各说各的。
  const quota = nodes
    .filter((n) => n.traffic_limit > 0)
    .map((n) => {
      const used = monthUsage(n)
      return { node: n, used, pct: used / n.traffic_limit }
    })
    .sort((a, b) => b.pct - a.pct)
    .slice(0, 5)
  const [month, setMonth] = useState(() => {
    const d = new Date()
    return { y: d.getFullYear(), m: d.getMonth() }
  })
  const [pickedDay, setPickedDay] = useState<string | null>(null)
  // 「查看详情」弹窗：卡片只显示最急的 2 条、高度固定；全貌在弹窗里看。
  // 不用「卡内滚动」：同行两卡高度会互相牵扯，而滚轮落在卡片上会先滚卡片再滚页面，体验很碎。
  const [noticesOpen, setNoticesOpen] = useState(false)
  const [tab, setTab] = useState<"traffic" | "bandwidth" | "resource">("traffic")
  const [range, setRange] = useState(30)
  const series = useOverviewSeries(range)

  const online = nodes.filter((n) => n.online).length
  const outdated = nodes.filter((n) => n.agent_old).length
  const expiring = nodes.filter((n) => {
    const d = daysUntil(n.expires_at)
    return d !== null && d >= 0 && d <= 30
  }).length
  const expired = nodes.filter((n) => {
    const d = daysUntil(n.expires_at)
    return d !== null && d < 0
  }).length

  // 版本 → 台数。空字符串是「从没上报过」，不是 0.0.0，所以排在最后并原样显示。
  const versionCount = new Map<string, number>()
  for (const n of nodes) versionCount.set(n.agent_version ?? "", (versionCount.get(n.agent_version ?? "") ?? 0) + 1)
  // 三段互斥且覆盖全部节点。`agent_old` 是 hub 按每台机器判的，**对没连过的节点不设** ——
  // 所以「未上报」必须单独一段，不能并进「落后」。
  const agentBuckets = [
    { key: "latest", label: "最新版", version: agentLatest ?? "", count: nodes.filter((n) => !n.agent_old && (n.agent_version ?? "") !== "").length, color: "bg-ok-fg" },
    { key: "old", label: "落后", version: "", count: nodes.filter((n) => n.agent_old).length, color: "bg-warn-fg" },
    { key: "none", label: "未上报", version: "", count: nodes.filter((n) => (n.agent_version ?? "") === "").length, color: "bg-muted-foreground/40" },
  ]

  // 到期日按「本地日历天」归组：`expires_at` 是日期字符串，不要经 Date 解析后再取 UTC 天，
  // 那样在东八区会把日子整体挪错一天。
  const byDay = new Map<string, Node[]>()
  for (const n of nodes) {
    if (!n.expires_at) continue
    const day = n.expires_at.slice(0, 10)
    byDay.set(day, [...(byDay.get(day) ?? []), n])
  }
  const peak = Math.max(0, ...[...byDay.values()].map((v) => v.length))

  const first = new Date(month.y, month.m, 1)
  const days = new Date(month.y, month.m + 1, 0).getDate()
  // 周一起始（中文日历的习惯，也是竞品那张图的样子）。
  const lead = (first.getDay() + 6) % 7
  const cells: (string | null)[] = [
    ...Array(lead).fill(null),
    ...Array.from({ length: days }, (_, i) => {
      const d = new Date(month.y, month.m, i + 1)
      return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`
    }),
  ]
  const today = new Date()
  const todayKey = `${today.getFullYear()}-${String(today.getMonth() + 1).padStart(2, "0")}-${String(today.getDate()).padStart(2, "0")}`

  // 「近期事项」：日历要自己看，这里直接说成一句话。全部来自 nodes。
  // 注意：这一段必须在 `todayKey` **之后** —— 之前版本把它插在前面，TDZ 崩溃、整页白屏。
  const soon = [...byDay.entries()].filter(([d]) => {
    const n = (Date.parse(d + "T00:00:00") - Date.parse(todayKey + "T00:00:00")) / 86_400_000
    return n >= 0 && n <= 30
  }).sort((a, b) => a[0].localeCompare(b[0]))
  const busiest = soon.reduce<[string, Node[]] | null>((best, cur) => (!best || cur[1].length > best[1].length ? cur : best), null)
  // 严重度分级：**要立刻动手**的排在前面。原来按 push 顺序（到期在前），但「装完就没连上」
  // 和「流量已达额度」比「30 天后到期」急得多 —— 顺序本身也是信息。
  const urgent: { text: string; hint: string; icon: ReactNode; href?: string }[] = []
  const routine: { text: string; hint: string; icon: ReactNode; href?: string }[] = []
  const notices: { text: string; hint: string; icon: ReactNode; href?: string }[] = []
  if (soon.length > 0) {
    const total = soon.reduce((n, [, list]) => n + list.length, 0)
    routine.push({
      text: `未来 30 天有 ${total} 台到期`,
      hint: busiest && busiest[1].length > 1 ? `其中 ${busiest[1].length} 台集中在 ${busiest[0].slice(5)}` : `最早 ${soon[0][0].slice(5)}`,
      icon: <CalendarClock className="size-4" />,
      href: "/admin/nodes",
    })
  }
  if (expired > 0) urgent.push({ text: `已经有 ${expired} 台过期`, hint: "续费或下线", icon: <CircleAlert className="size-4" />, href: "/admin/nodes" })
  if (outdated > 0) routine.push({ text: `${outdated} 台 agent 落后`, hint: `最新 ${agentLatest ?? "—"}`, icon: <ArrowUpCircle className="size-4" />, href: "/admin/update" })
  // 「异常」按维护者定的口径**并入这里** —— 这两条都是「要动手的事」，正合这张卡的意义，
  // 不必再占一张卡。判据面板手上就有，不需要新接口。
  // 「从未上报」与「离线」是两件事：离线是曾经在线、现在断了；从未上报是**装完就没上来过**。
  const never = nodes.filter((n) => (n.agent_version ?? "") === "").length
  if (never > 0)
    urgent.push({ text: `${never} 台从未上报`, hint: "装完就没连上，先查安装命令与网络", icon: <CircleAlert className="size-4" />, href: "/admin/nodes" })
  // 已达额度**就计入**（维护者的口径）：运维要提前知道，而不是等超了才看到。
  const overQuota = nodes.filter((n) => n.traffic_limit > 0 && monthUsage(n) >= n.traffic_limit).length
  if (overQuota > 0)
    urgent.push({ text: `${overQuota} 台流量已达额度`, hint: "已达本月上限，注意限速或停机", icon: <CircleAlert className="size-4" />, href: "/admin/nodes" })
  // 最多显示三条：这个卡与右边「版本分布」同处一行，全展开会让两栏高度差一大截。
  // 其余的用一行汇总 —— 卡片高度可预期，最要紧的三条仍然一眼看到。
  const shown = [...urgent, ...routine]
  const NOTICE_LIMIT = 2
  notices.push(...shown.slice(0, NOTICE_LIMIT))

  const shift = (delta: number) => {
    const d = new Date(month.y, month.m + delta, 1)
    setMonth({ y: d.getFullYear(), m: d.getMonth() })
    setPickedDay(null)
  }


  return (
    <div className="space-y-4">
      {/* 三张复合卡，而不是六个平铺数字：在线 + 离线 = 总数，并列三项本身就是数学冗余；
          而且平铺会把卡片横向拉长、大面积留白。这里按「存活 / 生命周期 / 维护」三件事各归一卡。 */}
      <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-4">
        {/* 卡片外框一律统一，状态只出现在两处：右上角的状态徽章 + 主数字的颜色。
            三轮下来方向一直是「更克制」，这一版最克制的一处就是**不再给外框上色**。 */}
        <Card>
          <CardContent>
            <div className="flex items-start justify-between gap-2">
              <div className="text-xs text-muted-foreground">节点总数</div>
              <StatusPill
                tone={online === nodes.length && nodes.length > 0 ? "ok" : online === 0 && nodes.length > 0 ? "bad" : "muted"}
                text={nodes.length === 0 ? "无节点" : online === 0 ? "全部离线" : online === nodes.length ? "全部在线" : "部分离线"}
              />
            </div>
            <div className="tnum mt-2 text-3xl leading-none font-semibold tracking-tight">
              {nodes.length}
              <span className="ml-1 align-baseline text-xs font-normal text-muted-foreground">台</span>
            </div>
            <div className="mt-2.5 flex flex-wrap items-center gap-x-3 gap-y-1 text-xs">
              <span className={online > 0 ? "flex items-center gap-1.5 text-ok-fg" : "flex items-center gap-1.5 text-muted-foreground"}>
                <span className="size-1.5 rounded-full bg-current" />
                <span className="tnum">{online}</span> 在线
              </span>
              <span className={nodes.length - online > 0 ? "flex items-center gap-1.5 font-semibold text-danger-fg" : "flex items-center gap-1.5 text-muted-foreground"}>
                <span className="size-1.5 rounded-full bg-current" />
                <span className="tnum">{nodes.length - online}</span> 离线
              </span>
            </div>
          </CardContent>
        </Card>

        <Card>
          <CardContent>
            <div className="flex items-start justify-between gap-2">
              <div className="text-xs text-muted-foreground">30 天内到期</div>
              <StatusPill tone={expired > 0 ? "bad" : expiring > 0 ? "warn" : "ok"} text={expired > 0 ? "已有过期" : expiring > 0 ? "需续费" : "无临期"} />
            </div>
            <div className={`tnum mt-2 text-3xl leading-none font-semibold tracking-tight ${expiring > 0 ? "text-warn-fg" : ""}`}>
              {expiring}
              <span className="ml-1 align-baseline text-xs font-normal text-muted-foreground">台</span>
            </div>
            <div className="mt-2.5 flex items-center gap-1.5 text-xs">
              <CalendarClock className="size-3.5 shrink-0 text-muted-foreground" />
              <span className={expired > 0 ? "font-semibold text-danger-fg" : "text-muted-foreground"}>
                已过期 <span className="tnum">{expired}</span> 台
              </span>
            </div>
          </CardContent>
        </Card>

        <Card>
          <CardContent>
            <div className="flex items-start justify-between gap-2">
              <div className="text-xs text-muted-foreground">待升级 agent</div>
              <StatusPill tone={outdated > 0 ? "warn" : "ok"} text={outdated > 0 ? "有落后" : "正常"} />
            </div>
            <div className={`tnum mt-2 text-3xl leading-none font-semibold tracking-tight ${outdated > 0 ? "text-warn-fg" : ""}`}>
              {outdated}
              <span className="ml-1 align-baseline text-xs font-normal text-muted-foreground">台</span>
            </div>
            <div className="mt-2.5 flex items-center gap-1.5 text-xs">
              <ArrowUpCircle className="size-3.5 shrink-0 text-muted-foreground" />
              <span className="text-muted-foreground">
                最新 <span className="font-mono">{agentLatest ?? "—"}</span>
              </span>
            </div>
          </CardContent>
        </Card>

        {/* 第 4 张：hub 自身。与第 3 张「agent 待升级」成对 —— 升级时两样都要看。 */}
        <Card>
          <CardContent>
            <div className="flex items-start justify-between gap-2">
              <div className="text-xs text-muted-foreground">待升级 hub</div>
              <StatusPill tone={hubBehind ? "warn" : "ok"} text={hubBehind ? "有新版" : "最新"} />
            </div>
            <div className={`tnum mt-2 text-3xl leading-none font-semibold tracking-tight ${hubBehind ? "text-warn-fg" : ""}`}>
              {hubBehind ? 1 : 0}
              <span className="ml-1 align-baseline text-xs font-normal text-muted-foreground">个</span>
            </div>
            <div className="mt-2.5 flex items-center gap-1.5 text-xs">
              <ArrowUpCircle className="size-3.5 shrink-0 text-muted-foreground" />
              <span className="text-muted-foreground">
                当前 <span className="font-mono">{hub || "—"}</span>
                {hubLatest && <span> · 最新 <span className="font-mono">{hubLatest}</span></span>}
              </span>
            </div>
          </CardContent>
        </Card>
      </div>

      {/* 宽屏两栏：这两张卡都不高，单列平铺会把右半边整片留白。 */}
      <div className="grid grid-cols-1 gap-4 lg:grid-cols-2">
        <Card>
          <CardHeader className="flex flex-row items-center justify-between gap-2">
            <CardTitle className="text-sm">近期事项</CardTitle>
            {shown.length > NOTICE_LIMIT && (
              <Button size="sm" variant="ghost" onClick={() => setNoticesOpen(true)}>
                查看详情（{shown.length}）
              </Button>
            )}
          </CardHeader>
          <CardContent className="space-y-2">
            {notices.length === 0 ? (
              // 只说我们**知道**的事。「告警通道运行正常」「SSL 证书充足」这类话写不出来 —— 面板没有
              // 那两份数据，印出来就是编造。
              <p className="text-sm text-muted-foreground">暂无其它待处理项。</p>
            ) : (
              <div className="divide-y divide-border">
                {notices.map((n) => (
                  <div key={n.text} className="flex items-start gap-2.5 py-3 first:pt-0 last:pb-0">
                    {/* 维护者要求：这一整块**不留任何颜色**（图标也洗掉）。所以严重度只由**文字**表达
                        —— 「6 台到期」「3 台集中在 10-12」本身就说清楚了要做什么。 */}
                    <span className="mt-0.5 shrink-0 text-muted-foreground">{n.icon}</span>
                    <div className="min-w-0 flex-1">
                      <div className="text-sm">{n.text}</div>
                      <div className="text-xs text-muted-foreground">{n.hint}</div>
                    </div>
                    {/* 行尾给**真的能去**的地方：到期去节点页，落后去更新页。
                        证书那条没地方可去 —— 这也是它做不成的原因之一（面板没有那份数据）。 */}
                    {n.href && (
                      <Button size="sm" variant="ghost" asChild className="shrink-0 self-center">
                        <a href={n.href}>查看</a>
                      </Button>
                    )}
                  </div>
                ))}
              </div>
            )}
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle className="text-sm">Agent 版本分布</CardTitle>
            <CardAction>
              <Badge variant="outline" className="font-mono">最新 {agentLatest ?? "—"}</Badge>
            </CardAction>
          </CardHeader>
          <CardContent className="space-y-3">
            {/* 分段堆叠条：绿 = 最新、琥珀 = 落后（hub 的 agent_old）、灰 = 未上报。
                三者互斥且覆盖全部节点，「未上报」不并进落后 —— 没连过的机器不该被说成旧版本。 */}
            <div className="flex h-2.5 overflow-hidden rounded-full bg-muted">
              {agentBuckets.map((b) => (
                <span
                  key={b.key}
                  className={b.color}
                  style={{ width: `${nodes.length ? (b.count / nodes.length) * 100 : 0}%` }}
                  title={`${b.label} ${b.count} 台`}
                />
              ))}
            </div>
            <div className="space-y-1.5">
              {agentBuckets.map((b) => (
                <div key={b.key} className="flex items-baseline justify-between gap-3 text-xs">
                  <span className="flex items-center gap-1.5 text-muted-foreground">
                    <span className={`size-2 rounded-sm ${b.color}`} />
                    {b.label}
                    <span className="font-mono">{b.version || "—"}</span>
                  </span>
                  <span className="shrink-0">
                    <span className="tnum font-medium">{b.count} 台</span>
                    <span className="tnum ml-1.5 text-muted-foreground">{nodes.length ? Math.round((b.count / nodes.length) * 100) : 0}%</span>
                  </span>
                </div>
              ))}
            </div>
          </CardContent>
        </Card>
      </div>

      {/* 某一天的到期明细：**弹窗**而不是在卡内向下展开 —— 展开会把卡片和同行另一张卡一起撑高。 */}
      <Dialog open={pickedDay !== null} onOpenChange={(v) => !v && setPickedDay(null)}>
        <DialogContent className="flex max-h-[calc(100dvh-4rem)] flex-col overflow-hidden sm:max-w-lg">
          <DialogHeader>
            <DialogTitle className="text-sm">
              {pickedDay ?? ""} 到期（{(pickedDay && byDay.get(pickedDay)?.length) ?? 0} 台）
            </DialogTitle>
          </DialogHeader>
          <div className="min-h-0 flex-1 divide-y divide-border overflow-y-auto overscroll-contain pr-1">
            {(pickedDay ? byDay.get(pickedDay) ?? [] : []).map((n) => (
              <div key={n.id} className="flex flex-wrap items-baseline gap-x-3 py-3">
                <span className="font-medium">{n.name}</span>
                <span className="text-xs text-muted-foreground">{n.group || "未分组"}</span>
                <span className="tnum ml-auto text-xs text-muted-foreground">
                  {n.price > 0
                    ? `${n.currency} ${n.price} / ${n.billing_cycle === "yearly" ? "年" : n.billing_cycle === "quarterly" ? "季" : "月"}`
                    : "未记价格"}
                </span>
                <Expiry date={n.expires_at} />
              </div>
            ))}
          </div>
        </DialogContent>
      </Dialog>

      {/* 全部事项的弹窗。卡片只显示最急的 2 条，全貌在这里 —— 同行两张卡的高度都固定。 */}
      <Dialog open={noticesOpen} onOpenChange={setNoticesOpen}>
        <DialogContent className="flex max-h-[calc(100dvh-4rem)] flex-col overflow-hidden sm:max-w-lg">
          <DialogHeader>
            <DialogTitle className="text-sm">全部待处理事项（{shown.length} 条）</DialogTitle>
          </DialogHeader>
          <div className="min-h-0 flex-1 divide-y divide-border overflow-y-auto overscroll-contain pr-1">
            {shown.map((n) => (
              <div key={n.text} className="flex items-start gap-2.5 py-3">
                <span className="mt-0.5 shrink-0 text-muted-foreground">{n.icon}</span>
                <div className="min-w-0 flex-1">
                  <div className="text-sm">{n.text}</div>
                  <div className="text-xs text-muted-foreground">{n.hint}</div>
                </div>
                {n.href && (
                  <Button size="sm" variant="ghost" asChild className="shrink-0 self-center">
                    <a href={n.href}>查看</a>
                  </Button>
                )}
              </div>
            ))}
          </div>
        </DialogContent>
      </Dialog>

      {/* 用量榜：只报**现状**（谁用得最满），不做"还能用几天"这类预测。 */}
      <Card>
        <CardHeader className="flex flex-row items-center justify-between gap-2">
          <CardTitle className="text-sm">用量榜</CardTitle>
          <span className="text-xs text-muted-foreground">按本月已用额度排序</span>
        </CardHeader>
        <CardContent className="space-y-3">
          {quota.length === 0 ? (
            <p className="text-sm text-muted-foreground">没有节点设置流量额度。</p>
          ) : (
            quota.map(({ node: n, used, pct }) => {
              const over = pct >= 1
              return (
                <div key={n.id} className="flex flex-wrap items-center gap-x-3 gap-y-1 text-sm">
                  <span className="w-40 shrink-0 truncate font-medium">{n.name}</span>
                  <span className="w-24 shrink-0 truncate text-xs text-muted-foreground">{n.group || "未分组"}</span>
                  {/* 进度条复用版本分布那条的形状与高度 */}
                  <span className="min-w-24 flex-1 overflow-hidden rounded-full bg-muted">
                    <span
                      className={`block h-2.5 rounded-full ${over ? "bg-danger-fg" : "bg-primary"}`}
                      style={{ width: `${Math.min(100, pct * 100)}%` }}
                    />
                  </span>
                  <span className="tnum shrink-0 text-xs text-muted-foreground">
                    {bytes(used)} / {bytes(n.traffic_limit)}
                  </span>
                  <span className={`tnum w-14 shrink-0 text-right text-xs font-medium ${over ? "text-danger-fg" : ""}`}>
                    {Math.round(pct * 100)}%
                  </span>
                </div>
              )
            })
          )}
        </CardContent>
      </Card>

      {/* 左图右历：照维护者给的参考，趋势在左、日历在右，同一行；窄屏自动上下堆叠。 */}
      <div className="grid items-stretch gap-4 lg:grid-cols-[minmax(0,2fr)_minmax(0,1fr)]">
      	<TrendCard rows={series} tab={tab} setTab={setTab} range={range} setRange={setRange} />
      <Card>
        {/* 标题与导航放在**同一个 flex 行**里。
            不用 `CardAction` 了：dump 出来才看清 —— `CardHeader` 是 grid，标题占第一行（20px），
            而 `CardAction` 是 `row-span-2` + `self-center`，它的中心落在「两行加起来」的区域上，
            于是两者中心天然差 6px。我先后改过 `CardHeader` 的 `items-center` 与 `CardAction` 的
            `self-center`，**都压不过它**（计算结果仍是 align-items: flex-start）。
            一行 flex + items-center 是确定的解法，不再跟原语较劲。 */}
        <CardHeader className="flex flex-row items-center justify-between gap-2">
          <CardTitle className="text-sm">续费日历</CardTitle>
          <div className="flex items-center gap-2">
            <span aria-hidden className="h-5 w-px bg-border" />
            <Button size="sm" variant="ghost" onClick={() => shift(-1)}>上月</Button>
            <button
              type="button"
              onClick={() => {
                const d = new Date()
                setMonth({ y: d.getFullYear(), m: d.getMonth() })
                setPickedDay(null)
              }}
              className="tnum min-w-24 text-center text-base font-semibold hover:text-primary"
              title="回到本月"
            >
              {month.y} 年 {month.m + 1} 月
            </button>
            <Button size="sm" variant="ghost" onClick={() => shift(1)}>下月</Button>
          </div>
        </CardHeader>
        <CardContent className="space-y-4">
          {/* 保留框线（维护者要求）。同事那一轮建议去掉，但这是维护者的取舍 —— 日历的框线帮助
              逐格定位，尤其在有到期副标的日子里。 */}
          <div className="grid grid-cols-7 gap-px overflow-hidden rounded-lg border bg-border text-center text-xs">
            {["一", "二", "三", "四", "五", "六", "日"].map((w) => (
              <div key={w} className="bg-background py-1.5 font-medium text-muted-foreground">{w}</div>
            ))}
            {cells.map((day, i) => {
              if (!day) return <div key={`lead-${i}`} className="bg-background" />
              const list = byDay.get(day) ?? []
              const isPeak = peak > 1 && list.length === peak
              const on = pickedDay === day
              // **当天没有到期节点时，这个格子不是按钮**：
              // 点了没反应的按钮是最糟的一种控件（我一路拒绝的就是这类），而没有事项的日子
              // 本来也不是可操作对象。所以它降级成普通格子 —— 不响应点击，也不给悬停高亮，
              // 免得暗示「这里能点」。
              const cellClass = `flex min-h-[54px] flex-col items-center justify-start gap-0.5 bg-background p-1.5 transition-colors ${on ? "ring-2 ring-inset ring-primary" : ""}`
              const inner = (
                <>
                  <span
                    className={`tnum flex size-5 items-center justify-center rounded-lg text-xs ${
                      day === todayKey ? "bg-primary/10 font-semibold text-primary ring-1 ring-primary/40" : "text-muted-foreground"
                    }`}
                  >
                    {Number(day.slice(8))}
                  </span>
                  {list.length > 0 && (
                    <span className={`rounded px-1 text-[10px] leading-tight ${isPeak ? "bg-warn-fg/15 text-warn-fg" : "bg-muted text-muted-foreground"}`}>
                      {list.length} 台
                    </span>
                  )}
                </>
              )
              if (list.length === 0) {
                return (
                  <div key={day} className={cellClass}>
                    {inner}
                  </div>
                )
              }
              return (
                <button
                  key={day}
                  type="button"
                  onClick={() => setPickedDay(day)}
                  aria-label={`${day}：${list.length} 台到期`}
                  className={`${cellClass} hover:bg-muted`}
                >
                  {inner}
                </button>
              )
            })}
          </div>

          {peak === 0 && <p className="text-xs text-muted-foreground">这个月没有节点到期。往前后翻可以看到别的月份。</p>}
        </CardContent>
      </Card>
      </div>
    </div>
  )
}

