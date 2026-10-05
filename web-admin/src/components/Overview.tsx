import { useEffect, useState, type ReactNode } from "react"
import { ArrowUpCircle, CalendarClock, CircleAlert } from "lucide-react"

import { api } from "@/lib/api"
import type { Node } from "@/lib/api"
import { bytes } from "@/lib/format"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardAction, CardContent, CardHeader, CardTitle } from "@/components/ui/card"

// `daysUntil` 与 `Expiry` 仍在 `Admin.tsx`（节点表也在用）。从那里 import 会形成**循环引用**，
// 但两者都是函数声明 —— 声明会提升，且只在渲染时调用，所以这个环是安全的；比把它们复制一份好。
import { Expiry, Help, daysUntil } from "./Admin"

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
function HoverOverlay({ rows, index, series }: {
  rows: SeriesPoint[]
  index: number
  series: { c: string; t: string; v: string }[]
}) {
  const pct = ((index + 0.5) / rows.length) * 100
  return (
    <div className="pointer-events-none absolute inset-0">
      <span className="absolute top-0 bottom-5 w-px bg-foreground/25" style={{ left: `${pct}%` }} />
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
function TrafficChart({ rows }: { rows: SeriesPoint[] }) {
  const top = Math.max(1, ...rows.map((r) => r.rx + r.tx))
  const cum = rows.map((_, i) => rows.slice(0, i + 1).reduce((n, r) => n + r.rx + r.tx, 0))
  const cumTop = Math.max(1, ...cum)
  const bw = CHART_W / Math.max(1, rows.length)
  // 柱子**限宽**：只取到 4 天时，按比例分到的宽度会把柱子拉成砖块。
  const bar = Math.min(bw * 0.7, CHART_BAR_MAX)
  const bottom = CHART_H - CHART_PAD
  const yOf = (v: number, m: number) => bottom - (v / m) * (CHART_H - CHART_PAD * 2)
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
            <rect x={x} y={mid} width={bar} height={Math.max(0, bottom - mid)} className="fill-primary" />
            <rect x={x} y={hi} width={bar} height={Math.max(0, mid - hi)} className="fill-primary/45" />
            {/* 两段是同色系的不同明度，深色主题下交界会糊 —— 用一条 1px 分界线靠结构说清楚，而不是靠色差。 */}
            <line x1={x} y1={mid} x2={x + bar} y2={mid} className="stroke-background" strokeWidth="1" />
          </g>
        )
      })}
      <polyline fill="none" className="stroke-warn-fg" strokeWidth="1.5"
        points={cum.map((v, i) => `${i * bw + bw / 2},${yOf(v, cumTop)}`).join(" ")} />
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
function BandwidthChart({ rows }: { rows: SeriesPoint[] }) {
  const rate = (v: number, r: SeriesPoint) => (r.covered > 0 ? v / r.covered : 0)
  const inRate = (r: SeriesPoint) => rate(r.rx, r)
  const outRate = (r: SeriesPoint) => rate(r.tx, r)
  const top = Math.max(1, ...rows.map((r) => Math.max(inRate(r), outRate(r))))
  const bw = CHART_W / Math.max(1, rows.length)
  const yOf = (v: number) => CHART_H - CHART_PAD - (v / top) * (CHART_H - CHART_PAD * 2)
  const line = (f: (r: SeriesPoint) => number) => rows.map((r, i) => `${i * bw + bw / 2},${yOf(f(r))}`).join(" ")
  const step = Math.max(1, Math.ceil(rows.length / 6))
  return (
    <svg viewBox={`0 0 ${CHART_W} ${CHART_H}`} className="w-full" role="img" aria-label="每日入站与出站带宽速率">
      <ChartTicks left={top} format={(v) => `${bytes(v)}/s`} />
      <polyline fill="none" className="stroke-ok-fg" strokeWidth="1.5" points={line(inRate)} />
      <polyline fill="none" className="stroke-primary" strokeWidth="1.5" points={line(outRate)} />
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

/// 资源：cpu / 内存 / 硬盘 三条线，都是百分比，共用 0–100 的轴。
function ResourceChart({ rows }: { rows: SeriesPoint[] }) {
  const yOf = (v: number) => CHART_H - CHART_PAD - (Math.min(100, Math.max(0, v)) / 100) * (CHART_H - CHART_PAD * 2)
  const line = (key: "cpu" | "mem" | "disk") =>
    rows.map((r, i) => `${(i + 0.5) * (CHART_W / Math.max(1, rows.length))},${yOf(r[key])}`).join(" ")
  return (
    <svg viewBox={`0 0 ${CHART_W} ${CHART_H}`} className="w-full" role="img" aria-label="全队 cpu 内存 硬盘 占用率">
      <ChartTicks left={100} format={(v) => `${Math.round(v)}%`} />
      <ChartAxis rows={rows} />
      <polyline fill="none" className="stroke-primary" strokeWidth="1.5" points={line("cpu")} />
      <polyline fill="none" className="stroke-warn-fg" strokeWidth="1.5" points={line("mem")} />
      <polyline fill="none" className="stroke-ok-fg" strokeWidth="1.5" points={line("disk")} />
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
  const pick = (key: "rx" | "tx" | "rx_peak" | "tx_peak" | "cpu" | "mem" | "disk") => {
    if (list.length === 0) return { max: 0, day: "" }
    let idx = 0
    list.forEach((r, i) => {
      if (r[key] > list[idx][key]) idx = i
    })
    return { max: list[idx][key], day: dayLabel(list[idx].day_ts) }
  }
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
            { label: "CPU 峰值", value: `${pick("cpu").max.toFixed(1)}%`, hint: pick("cpu").day },
            { label: "内存峰值", value: `${pick("mem").max.toFixed(1)}%`, hint: pick("mem").day },
            { label: "硬盘峰值", value: `${pick("disk").max.toFixed(1)}%`, hint: pick("disk").day },
          ]

  // 悬停索引。放在趋势卡这一层，三张图共用同一套交互 —— 三张图本身不用改。
  const [hover, setHover] = useState<number | null>(null)
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
            { c: "bg-primary", t: "入站", v: bytes(rows[i].rx) },
            { c: "bg-primary/45", t: "出站", v: bytes(rows[i].tx) },
            { c: "bg-warn-fg", t: "累计", v: bytes(cumAt(i)) },
          ]
        : tab === "bandwidth"
          ? [
              { c: "bg-ok-fg", t: "入站", v: `${bytes(rows[i].covered > 0 ? rows[i].rx / rows[i].covered : 0)}/s` },
              { c: "bg-primary", t: "出站", v: `${bytes(rows[i].covered > 0 ? rows[i].tx / rows[i].covered : 0)}/s` },
            ]
          : [
              { c: "bg-primary", t: "CPU", v: `${rows[i].cpu.toFixed(1)}%` },
              { c: "bg-warn-fg", t: "内存", v: `${rows[i].mem.toFixed(1)}%` },
              { c: "bg-ok-fg", t: "硬盘", v: `${rows[i].disk.toFixed(1)}%` },
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
        <div className="flex flex-wrap items-center justify-end gap-3 text-xs text-muted-foreground">
          {legend.map((l) => (
            <span key={l.t} className="flex items-center gap-1.5">
              <span className={`size-2 rounded-full ${l.c}`} />
              {l.t}
            </span>
          ))}
        </div>
        {!rows || rows.length === 0 ? (
          <p className="py-10 text-center text-xs text-muted-foreground">
            {rows ? "这段时间还没有指标数据。" : "正在读取…"}
          </p>
        ) : (
          <div className="relative" onMouseMove={onMove} onMouseLeave={() => setHover(null)}>
            {tab === "traffic" ? (
              <TrafficChart rows={rows} />
            ) : tab === "bandwidth" ? (
              <BandwidthChart rows={rows} />
            ) : (
              <ResourceChart rows={rows} />
            )}
            {hover !== null && <HoverOverlay rows={rows} index={hover} series={hoverSeries(hover)} />}
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
export function Overview({ nodes, agentLatest }: { nodes: Node[]; agentLatest: string | null }) {
  const [month, setMonth] = useState(() => {
    const d = new Date()
    return { y: d.getFullYear(), m: d.getMonth() }
  })
  const [pickedDay, setPickedDay] = useState<string | null>(null)
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
  const notices: { text: string; hint: string; icon: ReactNode; href?: string }[] = []
  if (soon.length > 0) {
    const total = soon.reduce((n, [, list]) => n + list.length, 0)
    notices.push({
      text: `未来 30 天有 ${total} 台到期`,
      hint: busiest && busiest[1].length > 1 ? `其中 ${busiest[1].length} 台集中在 ${busiest[0].slice(5)}` : `最早 ${soon[0][0].slice(5)}`,
      icon: <CalendarClock className="size-4" />,
      href: "/admin/nodes",
    })
  }
  if (expired > 0) notices.push({ text: `已经有 ${expired} 台过期`, hint: "续费或下线", icon: <CircleAlert className="size-4" />, href: "/admin/nodes" })
  if (outdated > 0) notices.push({ text: `${outdated} 台 agent 落后`, hint: `最新 ${agentLatest ?? "—"}`, icon: <ArrowUpCircle className="size-4" />, href: "/admin/update" })

  const shift = (delta: number) => {
    const d = new Date(month.y, month.m + delta, 1)
    setMonth({ y: d.getFullYear(), m: d.getMonth() })
    setPickedDay(null)
  }


  return (
    <div className="space-y-4">
      {/* 三张复合卡，而不是六个平铺数字：在线 + 离线 = 总数，并列三项本身就是数学冗余；
          而且平铺会把卡片横向拉长、大面积留白。这里按「存活 / 生命周期 / 维护」三件事各归一卡。 */}
      <div className="grid grid-cols-1 gap-4 md:grid-cols-3">
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
      </div>

      {/* 宽屏两栏：这两张卡都不高，单列平铺会把右半边整片留白。 */}
      <div className="grid grid-cols-1 gap-4 lg:grid-cols-2">
        <Card>
          <CardHeader>
            <CardTitle className="text-sm">近期事项</CardTitle>
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
              return (
                <button
                  key={day}
                  type="button"
                  onClick={() => setPickedDay(on ? null : day)}
                  aria-label={`${day}${list.length ? `：${list.length} 台到期` : ""}`}
                  className={`flex min-h-[54px] flex-col items-center justify-start gap-0.5 bg-background p-1.5 transition-colors hover:bg-muted ${on ? "ring-2 ring-inset ring-primary" : ""}`}
                >
                  {/* 今天用**浅底圆角**而不是实心方块：后者像打卡签到，且会把日期压得很小。 */}
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
                </button>
              )
            })}
          </div>

          {pickedDay && (
            <div className="rounded-lg bg-muted p-3">
              <div className="flex flex-wrap items-baseline justify-between gap-2">
                <h4 className="text-sm font-medium">{pickedDay} 到期</h4>
                <button type="button" onClick={() => setPickedDay(null)} className="text-xs text-primary underline underline-offset-2">
                  收起
                </button>
              </div>
              <div className="mt-2 space-y-1">
                {(byDay.get(pickedDay) ?? []).map((n) => (
                  <div key={n.id} className="flex flex-wrap items-baseline gap-x-3 text-sm">
                    <span className="font-medium">{n.name}</span>
                    <span className="text-xs text-muted-foreground">{n.group || "未分组"}</span>
                    <span className="tnum ml-auto text-xs text-muted-foreground">
                      {n.price > 0 ? `${n.currency} ${n.price} / ${n.billing_cycle === "yearly" ? "年" : n.billing_cycle === "quarterly" ? "季" : "月"}` : "未记价格"}
                    </span>
                    <Expiry date={n.expires_at} />
                  </div>
                ))}
              </div>
            </div>
          )}

          {peak === 0 && <p className="text-xs text-muted-foreground">这个月没有节点到期。往前后翻可以看到别的月份。</p>}
        </CardContent>
      </Card>
      </div>
    </div>
  )
}

