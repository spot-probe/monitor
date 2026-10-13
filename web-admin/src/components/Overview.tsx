import { useEffect, useState, type ReactNode, useCallback } from "react"
import { ArrowUpCircle, CalendarClock, ChevronDown, ChevronRight, CircleAlert, Search } from "lucide-react"

import { api, behind, changes, probeHistory } from "@/lib/api"
import type { Node, ProbeHistoryRow } from "@/lib/api"
import { bytes, monthUsage, renewedExpiry } from "@/lib/format"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardAction, CardContent, CardHeader, CardTitle } from "@/components/ui/card"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { Input } from "@/components/ui/input"

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
const seriesCache = new Map<string, SeriesPoint[]>()

/// 缓存的键必须**带上分组** —— 否则切换分组会拿到另一组的曲线（"半新半旧"那一类问题）。
const seriesKey = (days: number, group: string) => `${days}:${group}`

/// 取全队趋势；`days` 变了就取那一档（已取过的档位直接给缓存）。
function useOverviewSeries(days: number, group: string) {
  const key = seriesKey(days, group)
  const [rows, setRows] = useState<SeriesPoint[] | null>(() => seriesCache.get(key) ?? null)
  useEffect(() => {
    const hit = seriesCache.get(key)
    if (hit) {
      setRows(hit)
      return
    }
    let stop = false
    api<{ days: SeriesPoint[] }>(`/overview/series?days=${days}&group=${encodeURIComponent(group)}`)
      .then((d) => {
        seriesCache.set(key, d.days)
        if (!stop) setRows(d.days)
      })
      .catch(() => {
        if (!stop) setRows([])
      })
    return () => {
      stop = true
    }
  }, [days, group])
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
    ok: "bg-muted text-muted-foreground",
    warn: "bg-muted text-muted-foreground",
    bad: "bg-muted text-muted-foreground",
    muted: "bg-muted text-muted-foreground",
  }[tone]
  return <span className={`shrink-0 rounded-full px-2 py-0.5 text-[11px] leading-tight font-medium ${cls}`}>{text}</span>
}

/// 「多选一」控件的**唯一**样子：灰底轨道 + 选中项实心。
/// 这类控件本来散在各处、各写一遍，就会慢慢长出第二种样子 —— 所以提到模块级，共用。
/// （**开关**不在此列：那是"开/关"，不是"多选一"，硬塞进轨道才是错的。）
// 分段控制器的两个原语 ✓ —— 全页 4 处 `TRACK`、8 处 `pill(...)` 共用 ✓✓，
// 所以**改这里就等于整页统一** ✓（不必一处一处改 ✓，也就不会改漏 ✓）。
//
// ⚠️ 尺寸是**对齐**过的 ✗：`h-9`(36px) 的槽 = `p-1`(8px) + 项高 28px ✓，
//    而项高 = `text-sm`(20px 行高) + `py-1`(8px) ✓ ⇒ 正正好，没有多余的缝 ✓。
//    （"浮在半空的两颗小药丸"那个观感，根子是**没有底座槽** ✓：两项直接躺在页面白底上 ✓。）
//
// 颜色一律走 token ✓（`muted` / `primary` / `muted-foreground` ✓）—— 换肤时不会漏色 ✓。
// ⚠️ **不写死高度** ✗：原来写的是 `h-9`(36px) ✓，而"36 = p-1×2 + 项高 28"只在 `text-sm` 时成立 ✓；
// 字号一调（比如 13px ✓），那 2px 就会变成**槽里的缝** ✓ —— 让内容自己撑高才不会露馅 ✓。
/// 国家码 → 国旗 emoji ✓✓（两个 ASCII 字母映射到"区域指示符"两个码点 ✓）。
/// ⚠️ **不是处处能显示** ✗：macOS 与 Win11 出旗子 ✓，Win10 与部分 Linux 出的是两个字母（`AU` ✓）——
/// 两种都**读得出来** ✓，所以它是一次"多数人更好、少数人不坏"的增强 ✓（用户已确认要 ✓）。
/// 非两位字母一律返回空串 ✓（空国家码、`T1` 这类特殊码 ✓）—— 宁可不出旗，也不出错码 ✓。
/// 风险档位 → 观感 ✓✓。⚠️ **按 hub 给的档位文字映射** ✗，而**不是**在这里按分数重判一次 ✓：
/// 阈值只写在 hub 的 `risk.rs` 里 ✓ —— 面板再判一遍就会有**两处阈值** ✓，
/// 而"两处各写一份"今天已经吃过好几次亏 ✓（改一处忘一处，且不报错 ✓）。
/// 认不出的档位一律走中性色 ✓（不猜 ✓）。
const riskTone = (label?: string | null) =>
  label === "高风险"
    ? "bg-destructive/10 text-destructive"
    : label === "较高风险"
      ? "bg-warn/10 text-warn"
      : label === "低风险"
        ? "bg-ok/10 text-ok"
        : "bg-muted text-muted-foreground"

const flag = (cc?: string | null) =>
  cc && /^[A-Za-z]{2}$/.test(cc)
    ? String.fromCodePoint(...[...cc.toUpperCase()].map((c) => 0x1f1e6 + c.charCodeAt(0) - 65))
    : ""

const TRACK = "inline-flex items-center rounded-lg bg-muted p-1"
const pill = (on: boolean) =>
  // 字号 **13px** ✓（用户要"小一点点"✓）—— 不回到 `text-xs`(12px ✓)：那是**一整级** ✓，不是一点点 ✓。
  `tnum rounded-md px-3.5 py-1 text-[13px] transition-all ${
    on
      ? "bg-primary font-medium text-primary-foreground shadow-xs"
      : "text-muted-foreground hover:bg-muted-foreground/10 hover:text-foreground"
  }`

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
            <div className={TRACK}>
              {tabs.map((t) => (
                <button key={t.key} type="button" onClick={() => setTab(t.key)} aria-pressed={tab === t.key} className={pill(tab === t.key)}>
                  {t.label}
                </button>
              ))}
            </div>
            <div className={TRACK}>
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
export function Overview({ nodes: allNodes, agentLatest, hub, hubLatest, refresh }: {
  nodes: Node[]
  agentLatest: string | null
  hub: string
  hubLatest: string
  refresh: () => void
}) {
  // hub 自身待升级：与第 3 张「agent 待升级」成对。判据复用更新页那一套 `behind()`。
  const hubBehind = behind(hub, hubLatest)

  // **在入口处过滤一次**，下游（KPI / 事项 / 版本分布 / 日历）全部自动跟随 ——
  // 比在每个消费者里各写一次 filter 稳：那种写法迟早漏掉一处，变成"一半按分组、一半不按"。
  // 趋势另说：它的数是在 hub 侧聚合的，所以还要把分组传给接口（见 useOverviewSeries）。
  const [group, setGroup] = useState("")
  // 「更多」面板（分组太多时用）。**没有引入新依赖**：用本仓已有的 Dialog。
  const [moreOpen, setMoreOpen] = useState(false)
  const [groupQ, setGroupQ] = useState("")
  // 分组按**节点数降序**取前 3（字母序是任意的，节点多的才是常用的）。
  // 选中的那个**必须始终可见** —— 若它不在前 3，就额外补一个（否则看不出正在筛什么）。
  const groupCount = new Map<string, number>()
  for (const n of allNodes) if (n.group) groupCount.set(n.group, (groupCount.get(n.group) ?? 0) + 1)
  const ranked = [...groupCount.entries()].sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0])).map(([g]) => g)
  const topGroups = ranked.slice(0, 3)
  const overflow = ranked.length > 3
  const chips = !overflow || topGroups.includes(group) ? topGroups : group ? [...topGroups.slice(0, 2), group] : topGroups
  // 顶层视图。**故意不叫 `tab`**：趋势卡内部已经用 `tab`/`setTab` 表示"流量/带宽/资源"，
  // 同名会撞车。选择记在 localStorage，和 `overview-auto` 同一个习惯。
  // ⚠️ **视图选择现在不存了** ✗（原来是存的 ✓ —— 见下面那段说明 ✓）。
  // 不是机队级的设置 ✓）。加了第三项之后，读取要从**两路**改成**三路** ✗：
  // 原来写的是 `=== "cost" ? "cost" : "resource"` ✓ ⇒ 第三项会被**静默读成 resource** ✓✗。
  // ⚠️⚠️ **不再记住视图** ✗（用户要求 ✓："点进来是默认'资源'视图" ✓）——
  // 原来是记在 `localStorage` 里的 ✓ ⇒ 于是"默认值"其实**只在第一次生效** ✗：
  // 谁点过一次「IP 质量」✓，以后每次进来都落在那一页 ✓✗（用户遇到的正是这个 ✓ ——
  // 而代码里的默认值**本来就是"资源"** ✓，所以问题从来不在默认值上 ✓）。
  // ⇒ 去掉记忆 ✓：每次进总览都从「资源」开始 ✓✓（想切随时切 ✓，只是不留痕 ✓）。
  // ⚠️ 而这是**取舍** ✓：常看「成本」的人每次要多点一下 ✗ ——
  // 若以后觉得烦 ✓，正确做法是**记住但把默认值的含义说清** ✓，而不是现在这样悄悄记着 ✓。
  const [page, setPage] = useState<"resource" | "cost" | "quality">("resource")
  const pickPage = (p: "resource" | "cost" | "quality") => {
    setPage(p)
  }
  const nodes = group ? allNodes.filter((n) => n.group === group) : allNodes

  // ⭐ IP 质量表的**筛选** ✓（用户要求 ✓）：一个"只看有数据"开关 ✓ + 一个搜索框 ✓。
  // 判据"有数据" = 这台节点**有 `quality`** ✓（即 hub 到底查没查过它 ✓）。
  // ⭐ **点阵卡** ✓（用户提的 ✓）：点「节点实测」那一格 ⇒ 弹出这台节点的探测历史 ✓。
  // ⚠️ 三态分开写 ✓：加载中 ✓ / 失败（人话 + 重试 ✓）/ 拿到数据 ✓ ——
  // 失败时**不把浏览器的原文摆出来** ✗（"Failed to fetch" 是 `fetch()` 的原话 ✓，
  // 对用户没有任何意义 ✓✗；而且它**不会自己消失** ✗ ⇒ 必须给一个重试 ✓✓）。
  const [probeNode, setProbeNode] = useState<Node | null>(null)
  const [probeRows, setProbeRows] = useState<ProbeHistoryRow[] | null>(null)
  const [probeErr, setProbeErr] = useState(false)
  const loadProbe = useCallback((node: Node) => {
    setProbeRows(null)
    setProbeErr(false)
    probeHistory(node.id, 30)
      .then((r) => setProbeRows(r.rows))
      .catch(() => setProbeErr(true))
  }, [])

  const [onlyData, setOnlyData] = useState(false)
  const [q, setQ] = useState("")
  // ⚠️ **不能去过滤 `nodes`** ✗ —— 资源与成本两个视图也在用它 ✓，
  // 过滤了会让那两个视图**悄悄少几台** ✓✗（而且没人会立刻发现 ✓）。
  // ⇒ 只派生一份**质量表专用**的列表 ✓✓。
  // ⚠️ 名字避开 `shown` ✗（这个组件里**已经有一个同名变量** ✓ —— 我上一版就撞上了 ✓）。
  // ⚠️ 也**不用 `useMemo`** ✗：几十台节点的过滤，memo 的收益近乎零 ✓，却要多一个导入与依赖数组 ✓。
  const needle = q.trim().toLowerCase()
  const qualityRows = nodes.filter((n) => {
    if (onlyData && !n.quality) return false
    if (!needle) return true
    // 名称 / IP / ASN / 组织 / 城市 / 国家 都能搜 ✓（都是人真会拿来定位一台机器的字段 ✓）
    return [n.name, n.ipv4, n.ip, n.quality?.asn, n.quality?.org, n.quality?.city, n.quality?.country]
      .filter(Boolean)
      .some((v) => String(v).toLowerCase().includes(needle))
  })
  const hiddenRows = nodes.length - qualityRows.length

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
  // ⭐ "我已续费"那张卡 ✓ + **正在确认的那一台** ✓（两步按钮：先点"我已续费" ⇒ 变成"确认" ✓）
  const [renewOpen, setRenewOpen] = useState(false)
  const [renewAsk, setRenewAsk] = useState<number | null>(null)
  const [renewing, setRenewing] = useState(false)
  // ⚠️ **"今天"必须按本地日历天取** ✗ —— 本文件里归组那张日历就是这么做的 ✓
  // （绝不能用 `toISOString()` ✗：那是 UTC ✓，在东八区会差一天 ✓✗）。
  const todayLocal = (() => {
    const d = new Date()
    return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`
  })()
  // 「查看详情」弹窗：卡片只显示最急的 2 条、高度固定；全貌在弹窗里看。
  // 不用「卡内滚动」：同行两卡高度会互相牵扯，而滚轮落在卡片上会先滚卡片再滚页面，体验很碎。
  const [noticesOpen, setNoticesOpen] = useState(false)
  const [tab, setTab] = useState<"traffic" | "bandwidth" | "resource">("traffic")
  const [range, setRange] = useState(30)
  const series = useOverviewSeries(range, group)

  // 「需要处理的节点」：三条榜由 hub 一次算好（延迟/丢包要在 SQL 里按 `answered` 加权，
  // 前端拿不到那种聚合），所以只取一次、切换榜不再发请求。
  type RiskRow = { task: string; node: string; value: number; samples: number }
  type Risk = { latency: RiskRow[]; loss: RiskRow[]; down: { node: string; last_seen: number }[] }
  // 汇率：给第 4 张公共卡用。成本 tab 自己也会取一份，两个请求打同一个端点、结果一致 ——
  // **关键是算法只在一处**：两边都用模块级的 `costRow()`，所以不会各算一套。
  const [fx, setFx] = useState<{ per_unit: Record<string, number> } | null>(null)
  useEffect(() => {
    let stop = false
    api<{ per_unit: Record<string, number> }>("/fx")
      .then((d) => { if (!stop) setFx(d) })
      .catch(() => { if (!stop) setFx(null) })
    return () => { stop = true }
  }, [])
  const perUnitTop = fx?.per_unit ?? {}
  // **口径（维护者定）**：这两个数只统计"**有到期日且未过期**"的机器 ——
  // 没有到期日（∞）的没有"剩余"可言，已过期的也不在计费里。
  // 注意因此它与成本 tab 的「每月成本（全部机器）」**是两个不同的问题**，不该相等。
  // 缺汇率的机器一律跳过（绝不 1:1）。
  // ⚠️ 用 **`nodes`（跟着分组走的那个集合 ✓）**，不是 `allNodes` ✗ ——
  // 用户报的正是这里 ✓：切分组时"3 台"那几张卡跟着变 ✓，而**成本这两张不变** ✗
  //（同文件里别的 KPI 全用 `nodes` ✓，只有这一处用了全部 ✓✗）。
  // ⚠️ 上面那句注释讲的是"与**成本 tab** 的口径不同" ✓（"有到期日且未过期" ✓）——
  // 那是**统计范围**的差别 ✓，与**分组**无关 ✓ ⇒ 改成 `nodes` 不违背它的本意 ✓✓。
  // ⚠️ 顺带 ✓：`noExpiry` / `expiredCount` / `noRate` 也由它派生 ✓ ⇒ 一并跟着分组走 ✓（本就应该 ✓）。
  const dated = nodes.map((n) => ({ n, r: costRow(n, perUnitTop), d: daysUntil(n.expires_at) }))
  const inScope = dated.filter((x) => x.r.monthly !== null && x.d !== null && x.d >= 0)
  const monthlyNow = inScope.reduce((a, x) => a + (x.r.monthly ?? 0), 0)
  // 剩余价值 = 价格 ÷ 周期天数 × 剩余天数（例：600/年、剩 180 天 → 600/365×180 = ¥295.89）
  const residual = inScope.reduce((a, x) => {
    const days = cycleDays(x.n.billing_cycle)
    return a + (x.n.price / days) * (x.d as number) * (perUnitTop[x.n.currency] ?? 1)
  }, 0)
  const noExpiry = dated.filter((x) => x.d === null).length
  // 名字不能叫 `expired`：上面「30 天内到期」那张卡已经用了这个变量。
  const expiredCount = dated.filter((x) => x.d !== null && x.d < 0).length
  const noRate = dated.filter((x) => x.r.monthly === null).length

  const [risk, setRisk] = useState<Risk | null>(null)
  const [riskTab, setRiskTab] = useState<"latency" | "loss" | "down">("latency")
  useEffect(() => {
    let stop = false
    api<Risk>(`/nodes/at-risk?days=7&limit=5&group=${encodeURIComponent(group)}`)
      .then((d) => { if (!stop) setRisk(d) })
      .catch(() => { if (!stop) setRisk(null) })
    return () => { stop = true }
  }, [group])
  // 秒 → 「3 天 2 小时」；只在"离线"那一栏用，口径写在一处。
  const span = (sec: number) =>
    sec >= 86400 ? `${Math.floor(sec / 86400)} 天 ${Math.floor((sec % 86400) / 3600)} 小时` : `${Math.max(1, Math.floor(sec / 3600))} 小时`
  // 自动刷新：**默认关闭**。一块会自己重载的仪表盘会让人意外（正在读的数忽然变了），
  // 而且它会持续产生请求 —— 所以要不要开，交给使用者，并把选择记住。
  const [auto, setAuto] = useState(() => localStorage.getItem("overview-auto") === "1")
  const [lastAt, setLastAt] = useState<number | null>(null)
  useEffect(() => {
    if (!auto) return
    const tick = () => {
      // 两件事一起做，缺一不可：KPI/事项/日历来自 `nodes`（refresh 触发重取）；
      // 趋势来自按档位缓存的接口 —— 不清缓存的话，曲线会**纹丝不动**，变成"一半新一半旧"。
      seriesCache.delete(seriesKey(range, group))
      refresh()
      setLastAt(Date.now())
    }
    tick()
    const id = setInterval(tick, 30_000)
    return () => clearInterval(id)
  }, [auto, range, refresh])

  const online = nodes.filter((n) => n.online).length
  const outdated = nodes.filter((n) => n.agent_old).length
  // ⚠️ **留数组、台数由它派生** ✓（原来只有 `.length` ✗）——
  // "我已续费"那张卡要**逐台列出** ✓ ⇒ 需要数组 ✓；而两处各 filter 一遍迟早不一致 ✓✗。
  const expiringNodes = nodes.filter((n) => {
    const d = daysUntil(n.expires_at)
    return d !== null && d >= 0 && d <= 30
  })
  const expiring = expiringNodes.length
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
    {
      key: "old",
      label: "落后",
      // **带上台数与具体版本**，而不是只给一个总数：核对"要升几台、升到哪个"时，
      // 光有总数还得去节点页数。**不写成区间**（1.1.3~1.1.5）：区间会读成"这三版都有人用"，
      // 而实际可能只有 1.1.3 与 1.1.5。按台数从多到少，超过三个版本就省略，写明还有几个。
      version: (() => {
        const by = new Map<string, number>()
        for (const n of nodes) {
          if (!n.agent_old) continue
          const v = n.agent_version || "未上报"
          by.set(v, (by.get(v) ?? 0) + 1)
        }
        const sorted = [...by].sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]))
        const head = sorted.slice(0, 3).map(([v, c]) => `${v} ×${c}`).join(" · ")
        return sorted.length > 3 ? `${head} 等 ${sorted.length} 个版本` : head
      })(),
      count: nodes.filter((n) => n.agent_old).length,
      color: "bg-warn-fg",
    },
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
              <span className={online > 0 ? "flex items-center gap-1.5 text-muted-foreground" : "flex items-center gap-1.5 text-muted-foreground"}>
                <span className="size-1.5 rounded-full bg-current" />
                <span className="tnum">{online}</span> 在线
              </span>
              <span className={nodes.length - online > 0 ? "flex items-center gap-1.5 text-muted-foreground" : "flex items-center gap-1.5 text-muted-foreground"}>
                <span className="size-1.5 rounded-full bg-current" />
                <span className="tnum">{nodes.length - online}</span> 离线
              </span>
            </div>
          </CardContent>
        </Card>

        {/* ⭐ **这张卡同时是入口** ✓（用户提的 ✓）：点开列出临近到期的节点 ✓，
            每行一个"我已续费" ✓。⚠️ 类**照抄隔壁那张可点的卡** ✓
            （`block h-full rounded-xl` + `hover:ring-1 hover:ring-primary` ✓✓）——
            本仓的做法就是把 `<Card>` 包在一个可点元素里 ✓，不发明新的 ✓。
            ⚠️ 没有可续的机器时**不给点** ✓（点开一个空卡没有意义 ✓）。 */}
        <button
          type="button"
          disabled={expiringNodes.length === 0}
          onClick={() => setRenewOpen(true)}
          className={`block h-full w-full rounded-xl text-left ${
            expiringNodes.length > 0 ? "cursor-pointer transition-shadow hover:ring-1 hover:ring-primary" : "cursor-default"
          }`}
        >
        <Card>
          <CardContent>
            <div className="flex items-start justify-between gap-2">
              <div className="text-xs text-muted-foreground">30 天内到期</div>
              <StatusPill tone={expired > 0 ? "bad" : expiring > 0 ? "warn" : "ok"} text={expired > 0 ? "已有过期" : expiring > 0 ? "需续费" : "无临期"} />
            </div>
            <div className={`tnum mt-2 text-3xl leading-none font-semibold tracking-tight ${expiring > 0 ? "" : ""}`}>
              {expiring}
              <span className="ml-1 align-baseline text-xs font-normal text-muted-foreground">台</span>
            </div>
            <div className="mt-2.5 flex items-center gap-1.5 text-xs">
              <CalendarClock className="size-3.5 shrink-0 text-muted-foreground" />
              <span className={expired > 0 ? "text-muted-foreground" : "text-muted-foreground"}>
                已过期 <span className="tnum">{expired}</span> 台
              </span>
            </div>
          </CardContent>
        </Card>
        </button>

        <a
          href={outdated > 0 || hubBehind ? "/admin/update" : undefined}
          className={`block h-full rounded-xl ${
            outdated > 0 || hubBehind ? "transition-shadow hover:ring-1 hover:ring-primary" : ""
          }`}
        >
          {/* agent 与 hub 合成**一张卡**：它们回答的是同一个问题（"我要不要去做升级这件事"），
              分成两张时视觉上像两件事，实际点进去是同一个页面。
              主数字给**合计**，逐个的数量放小字 —— 维护者指定的形式。
              颜色保持中性（这一页早先刻意去掉了 KPI 数字上的红），只由右上角徽章带 tone。 */}
          <Card>
            <CardContent>
              <div className="flex items-start justify-between gap-2">
                <div className="text-xs text-muted-foreground">待升级</div>
                <StatusPill
                  tone={outdated > 0 || hubBehind ? "warn" : "ok"}
                  text={outdated > 0 || hubBehind ? "有更新" : "都是最新"}
                />
              </div>
              <div className="tnum mt-2 text-3xl leading-none font-semibold tracking-tight">
                {outdated + (hubBehind ? 1 : 0)}
                <span className="ml-1 align-baseline text-xs font-normal text-muted-foreground">项</span>
              </div>
              {/* 版号**并进同一行**，不另起一行：另起一行会让这张卡比邻卡高一行，
                  整行网格跟着变高（维护者指出的问题）。而"都是最新"时那行版号本来就是噪音 ——
                  所以只有**确实有更新**时才带上目标版本，平时这一行就是三行里的第三行。 */}
              <div className="mt-2.5 flex items-center gap-1.5 text-xs">
                <ArrowUpCircle className="size-3.5 shrink-0 text-muted-foreground" />
                <span className="truncate text-muted-foreground">
                  agent <span className="tnum">{outdated}</span> · hub{" "}
                  <span className="tnum">{hubBehind ? 1 : 0}</span>
                  {(outdated > 0 || hubBehind) && (
                    <>
                      {" · "}
                      <span className="font-mono">
                        ↑{hubBehind && outdated === 0 ? hub || "—" : agentLatest ?? "—"}
                      </span>
                    </>
                  )}
                </span>
              </div>
            </CardContent>
          </Card>
        </a>

        {/* 第 4 张：当月开销与剩余价值。两个数都走 `costRow()`，与成本 tab 同源。
            "剩余价值"是**估算**（假设当前计费周期的终点就是 `expires_at`），所以写明估算；
            没有到期的机器与缺汇率的机器都不计入，旁边标出计入了多少台。 */}
        {/* **三行同构**：与上面三张卡一样是「标签+徽章 / 主数字 / 一行小字」。
            口径细节（排除了哪些）并进第三行尾部的 `title`，**不占第四行** ——
            多一行会把这整行网格撑高，反过来把前三张拉出空白（维护者两次指出的问题）。 */}
        <Card className="h-full">
          <CardContent>
            <div className="flex items-start justify-between gap-2">
              <div className="text-xs text-muted-foreground">未到期当月开销</div>
              <StatusPill tone="ok" text="估算" />
            </div>
            <div className="tnum mt-2 text-3xl leading-none font-semibold tracking-tight">
              {cny(monthlyNow)}
            </div>
            <div
              className="mt-2.5 flex items-center gap-1.5 text-xs"
              title={`已排除：无到期日 ${noExpiry} 台 · 已过期 ${expiredCount} 台${noRate > 0 ? ` · 缺汇率 ${noRate} 台` : ""}`}
            >
              <CalendarClock className="size-3.5 shrink-0 text-muted-foreground" />
              <span className="truncate text-muted-foreground">
                剩余价值 <span className="tnum">{cny(residual)}</span>
                <span className="ml-1">（{inScope.length} 台计入）</span>
              </span>
            </div>
          </CardContent>
        </Card>

        
      </div>

      {/* 分组与自动刷新**同一行**：左边是"看什么"（页面级控件，影响一整页），右边是"多久看一次"。
          分成两行会让右边那个孤零零占一行，左边却挤在一起 —— 看着像排版错位。 */}
      <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-2">
      {/* 分组选择器：**页面级**控件，因为它影响的是一整页（KPI / 事项 / 版本分布 / 日历 / 趋势）。
          只在一处过滤（组件入口的那个 `nodes`），所以不存在"一半按分组、一半不按"。 */}
      <div className="flex flex-wrap items-center gap-x-2 gap-y-1 text-xs">
        <span className="text-muted-foreground">分组</span>
        <span aria-hidden className="h-4 w-px bg-border" />
        <div className={TRACK}>
          <button type="button" onClick={() => setGroup("")} aria-pressed={group === ""} className={pill(group === "")}>
            全部
          </button>
          {chips.map((g) => (
            <button key={g} type="button" onClick={() => setGroup(g)} aria-pressed={group === g} className={pill(group === g)}>
              {g}
            </button>
          ))}
          {overflow && (
            <button
              type="button"
              onClick={() => { setGroupQ(""); setMoreOpen(true) }}
              aria-pressed={!topGroups.includes(group) && group !== ""}
              className={`${pill(!topGroups.includes(group) && group !== "")} inline-flex items-center gap-0.5`}
            >
              更多
              <ChevronDown className="size-3.5" />
            </button>
          )}
        </div>
        {group && (
          <span className="text-muted-foreground">
            只统计 <span className="font-medium">{nodes.length}</span> 台（共 {allNodes.length} 台）
          </span>
        )}
      </div>

      {/* 自动刷新那一条：细、右对齐，不占卡片。开关本身就是"暂停"—— 再点一下即停。 */}
      <div className="flex flex-wrap items-center justify-end gap-x-3 gap-y-1 text-xs text-muted-foreground">
        <button
          type="button"
          onClick={() => {
            const next = !auto
            setAuto(next)
            localStorage.setItem("overview-auto", next ? "1" : "0")
          }}
          aria-pressed={auto}
          className={`inline-flex items-center gap-1.5 rounded-full px-2.5 py-1 transition-colors hover:bg-muted ${auto ? "bg-primary/10 font-medium text-primary" : ""}`}
        >
          ⟳ 自动刷新{auto ? "（30 秒）" : ""}
        </button>
        {auto && lastAt && <span className="tnum">最后更新 {new Date(lastAt).toLocaleTimeString()}</span>}
      </div>

      </div>

      {/* 顶层 tlb：**4 张公共卡与分组筛选之下**，把"资源"与"成本"分成两个视图。
          资源那一边的内容一个字没改，只是被包进这个分支里。
          复用页面既有的 `TRACK`/`pill`，与其它分段控件同一套语汇；为了和内层
          （趋势卡里的"流量/带宽/资源"）区分，这里前面带一个「视图」标签。 */}
      {/* 「更多」面板：分组太多时用它。**用本仓已有的 Dialog**（没有引入 command/popover 依赖），
          带搜索、按节点数降序、每组标出台数 —— 点选即关闭。 */}
      <Dialog open={moreOpen} onOpenChange={setMoreOpen}>
        <DialogContent className="max-h-[70vh] max-w-md overflow-y-auto">
          <DialogHeader>
            <DialogTitle className="text-sm">选择分组</DialogTitle>
          </DialogHeader>
          <Input
            autoFocus
            value={groupQ}
            onChange={(e) => setGroupQ(e.target.value)}
            placeholder="搜索分组"
            className="h-8 text-xs"
          />
          <div className="mt-1 flex flex-col">
            {[{ g: "", c: allNodes.length }, ...ranked.map((g) => ({ g, c: groupCount.get(g) ?? 0 }))]
              .filter((x) => (groupQ ? x.g.toLowerCase().includes(groupQ.toLowerCase()) : true))
              .map(({ g, c }) => (
                <button
                  key={g || "all"}
                  type="button"
                  onClick={() => { setGroup(g); setMoreOpen(false) }}
                  className={`flex items-center justify-between rounded-md px-2 py-1.5 text-left text-sm transition-colors hover:bg-muted ${
                    group === g ? "font-medium text-primary" : ""
                  }`}
                >
                  <span>{g || "全部"}</span>
                  <span className="tnum text-xs text-muted-foreground">{c} 台</span>
                </button>
              ))}
          </div>
        </DialogContent>
      </Dialog>

      <div className="flex flex-wrap items-center gap-x-2 gap-y-1 text-xs">
        <span className="text-muted-foreground">视图</span>
        <span aria-hidden className="h-4 w-px bg-border" />
        <div className={TRACK}>
          {([["resource", "资源"], ["cost", "成本"], ["quality", "IP 质量"]] as const).map(([k, label]) => (
            <button key={k} type="button" onClick={() => pickPage(k)} aria-pressed={page === k} className={pill(page === k)}>
              {label}
            </button>
          ))}
        </div>
        {/* 筛选控件 ✓（用户要求 ✓）—— 放在同一行的右端 ✓（`ml-auto` ✓），不新起一行 ✓
            （免得又多一条横向带 ✓）。
            ⚠️ 只在「IP 质量」这一栏显示 ✗：另外两栏没有『有数据 / 无数据』这个概念 ✓，
            放上去会让人以为它对资源与成本也生效 ✓。 */}
        {page === "quality" ? (
          <div className="ml-auto flex flex-wrap items-center gap-2">
            {/* ⚠️ **它原来是个"看起来像文字"的按钮** ✗（用户报的 ✓）：
                用的是 `pill(false)` —— 未激活态只有灰字、**没有边框也没有底色** ✓ ⇒
                与旁边一串说明文字长得一样 ✓，一眼看不出能点 ✓。
                ⇒ 改成**标准外框按钮** ✓：常驻边框 ✓ · 激活时蓝底 ✓ · 前缀点指示状态 ✓ ·
                带**计数徽章**（当前会显示几台 ✓）· 高度 `h-7` 与右侧搜索框**对齐** ✓。 */}
            <button
              type="button"
              onClick={() => setOnlyData((v) => !v)}
              aria-pressed={onlyData}
              className={`inline-flex h-7 items-center gap-1.5 rounded-md border px-2.5 text-xs transition-colors ${
                onlyData
                  ? "border-primary bg-primary/10 font-medium text-primary"
                  : "border-border text-muted-foreground hover:bg-muted hover:text-foreground"
              }`}
            >
              {/* 前缀点 ✓：亮 = 正在筛选 ✓、暗 = 未启用 ✓（同事说的"前缀点"正是这个作用 ✓） */}
              <span className={`size-1.5 rounded-full ${onlyData ? "bg-primary" : "bg-muted-foreground/40"}`} />
              只看有数据
              {/* 计数徽章 ✓（用户要求 ✓）—— 只在激活时显示 ✓：
                  它顺便回答了"筛掉了几台" ✓，与表下那句"已隐藏 N 台"是同一个数 ✓（`qualityRows` ✓）。 */}
              {onlyData ? (
                <span className="rounded bg-primary/15 px-1 font-mono text-[11px]">{qualityRows.length}</span>
              ) : null}
            </button>
            <Input
              value={q}
              onChange={(e) => setQ(e.target.value)}
              placeholder="搜名称 / IP / ASN / 城市"
              className="h-7 w-56 text-xs"
            />
          </div>
        ) : null}
      </div>

      {/* ⭐ 「IP 质量」视图 ✓ —— 用户要的：把 IP 质量/风险从"逐台点开编辑框"搬到一个
          **能横向比较**的地方 ✓（"哪几台同 ASN / 哪个风险高"在单台对话框里根本比不出来 ✓）。
          ⚠️ 插在资源分支**之前** ✓ ⇒ 不必去动那个三元的长尾巴 ✓（一个插入点 ✓）。

          列只放 6 个 ✓：全放会到 9+ 列、屏幕装不下 ✓；坐标与时区留在编辑框 ✓
          （那本来就是"点开看细节"的东西 ✓）。
          ⚠️ **空单元格一律写"未知"** ✓✗：留空会被读成"没问题" ✓ —— 与今天修过的两处假信息同类 ✓。
          ⚠️ 风险与 Tor **各自标明来源** ✓：风险是在线库（第三方 · 要开关）✓、
          Tor 是本地清单（默认有效 · 不外发）✓ —— 两者说的可能是同一件事，但**不是同一条路** ✓。 */}
      {page === "quality" ? (
        <div className="space-y-4">
          {/* ⚠️ **白底是这一处改动的全部要点** ✗✓：行分隔线**本来就有** ✓（每行都带 `border-b` ✓），
                但卡片**没有自己的底色** ✗ ⇒ 页面灰底透出来 ✓ ⇒ 整张表看起来是"浮着的行" ✓✗，
                而不是 p2 那种"一份白底列表" ✓✓。 */}
            {/* ⚠️⚠️ **必须是 `bg-card`，不是 `bg-background`** ✗✗（我第一版就写错了 ✓）：
                这个主题里 `--background` = `#f7f8fa`（**就是那片浅灰** ✗）·
                `--card` = `#ffffff`（**白** ✓✓）—— 所以"加了底色却没变化" ✓✓。
                本仓的卡片一律用 `bg-card` ✓（`Login.tsx` 就是这么写的 ✓）。 */}
            <div className="overflow-x-auto rounded-lg border border-border bg-card">
            <div className="min-w-[68rem]">
              <div className="grid grid-cols-[minmax(0,1fr)_minmax(0,1.2fr)_minmax(0,1.5fr)_minmax(0,1.2fr)_minmax(0,1.3fr)] gap-x-3 border-b border-border border-b border-border bg-muted/40 px-3 py-2 text-[13px] font-semibold">
                <span>节点</span>
                <span>IP 与归属</span>
                <span>地理位置</span>
                <span>安全态势</span>
                <span>节点实测</span>
              </div>
              {qualityRows.map((n) => (
                <div
                  key={n.id}
                  className="grid grid-cols-[minmax(0,1fr)_minmax(0,1.2fr)_minmax(0,1.5fr)_minmax(0,1.2fr)_minmax(0,1.3fr)] items-start gap-x-3 border-b border-border px-3 py-2 text-[13px] last:border-b-0"
                >
                  <div className="min-w-0 space-y-0.5">
                    <div className="flex items-center gap-1.5">
                    <div className="min-w-0 truncate font-medium">{n.name}</div>
                    {/* 分组徽标 ✓ —— **逐字照抄**节点列表那套 ✓（用户指的第二张图 ✓）：同一套语汇 ✓，不新造样式 ✓。 */}
                    {n.group ? (
                      <Badge variant="outline" className="shrink-0 border-transparent bg-tag font-normal text-tag-foreground">
                        {n.group}
                      </Badge>
                    ) : null}
                  </div>
                    {/* ⭐ **"为什么没有"第一次看得见** ✓（同事 mock 里那句的位置 ✓）——
                        此前只有悬停 title ✓✗，而你明确说过不该只活在悬停里 ✓。
                        ⚠️ 措辞**只说事实** ✗：面板分不清"没查过 / 查了没收录 / 库没配" ✓
                        ⇒ 只写"暂无解析数据" ✓，**不编原因** ✗（同事写的"待探针上报"就是编的 ✓）。 */}
                    {/* ⚠️⚠️ **两种"没有"必须分开说** ✗✗（这是第三处同类的地方 ✓）：
                      ① **没允许检测** ✓ ⇒ 显示"未允许检测" ✓ —— 那是**人可以处置**的状态 ✓
                         （去纳管对话框勾上 ✓，或在那台机器上重跑安装命令 ✓）；
                      ② **允许了、但 hub 还没查到** ✓ ⇒ 才是"暂无解析数据" ✓ —— 那要**等 hub 干活** ✓。
                      ⇒ 都写成"暂无解析数据"会让人以为"查过了、结果是空" ✓✗，
                        而那会让人去做**完全无关**的排查 ✓（查库、查网络 ✓ —— 其实只是没允许 ✓）。 */}
                  {n.allow_ip_quality === false ? (
                    <Badge variant="outline" className="border-transparent bg-warn/10 font-medium text-warn">
                      未允许检测
                    </Badge>
                  ) : n.quality ? null : (
                    <div className="text-xs text-muted-foreground/70">暂无解析数据</div>
                  )}
                  </div>
                  <div className="min-w-0 space-y-0.5">
                    {/* ⚠️ 显示 **hub 实际查询的那个地址** ✓（`lookup_ip` ✓），不是 `ipv4` ✗✓ ——
                        生产上见过：`ipv4` 是内网 `10.x` ✓，而右边写着「Los Angeles」✓
                        ⇒ 看起来像"内网地址查出了洛杉矶" ✓✗。没有这个字段时（老 hub ✓）才退回 `ipv4 || ip` ✓。 */}
                    {/* 主要信息要强：IP 是这一行最要紧的东西，原来是灰的，于是整张表看着一片灰。 */}
                    <div className="truncate font-mono text-xs font-medium text-foreground">
                      {n.lookup_ip || n.ipv4 || n.ip || "未知"}
                    </div>
                    <div className="truncate text-xs text-muted-foreground">
                      {n.quality?.asn || n.quality?.org ? (
                        <>
                          {n.quality?.asn ? <span className="font-mono">AS{n.quality.asn}</span> : null}
                          {n.quality?.asn && n.quality?.org ? " · " : null}
                          {n.quality?.org ?? ""}
                        </>
                      ) : (
                        <span className="text-muted-foreground/40" title="未查到或未启用">—</span>
                      )}
                    </div>
                  </div>
                  <div className="min-w-0 space-y-0.5">
                    <div className="truncate">
                      {n.quality?.city || n.quality?.country ? (
                        <>
                          {flag(n.quality?.country) ? flag(n.quality?.country) + " " : null}
                          <span className="font-medium text-foreground">{n.quality?.city ?? ""}</span>
                          {n.quality?.city && n.quality?.country ? <span className="text-muted-foreground"> · </span> : null}
                          {/* 国家做成胶囊 ✓（用户选"甲" ✓）—— 用 `bg-tag` ✓ 与 p2 里分组徽标**同一套** ✓✓。 */}
                          {n.quality?.country ? (
                            <Badge
                              variant="outline"
                              className="shrink-0 border-transparent bg-tag font-normal text-tag-foreground"
                            >
                              {n.quality.country}
                            </Badge>
                          ) : null}
                        </>
                      ) : (
                        <span className="text-muted-foreground/40" title="未查到或未启用">—</span>
                      )}
                    </div>
                    <div className="truncate text-xs text-muted-foreground">
                      {n.quality?.time_zone ?? null}
                      {n.quality?.time_zone && n.quality?.latitude != null ? " · " : null}
                      {n.quality?.latitude != null && n.quality?.longitude != null ? (
                        <a
                          className="font-mono text-primary underline underline-offset-2"
                          href={`https://www.openstreetmap.org/?mlat=${n.quality.latitude}&mlon=${n.quality.longitude}#map=15/${n.quality.latitude}/${n.quality.longitude}`}
                          target="_blank"
                          rel="noreferrer"
                        >
                          {n.quality.latitude.toFixed(2)}, {n.quality.longitude.toFixed(2)}
                        </a>
                      ) : null}
                      {n.quality && !n.quality.time_zone && n.quality.latitude == null ? (
                        <span className="text-muted-foreground/40">—</span>
                      ) : null}
                    </div>
                  </div>
                  {/* 坐标 ✓：**可点开地图** ✓ —— 与编辑框里那个链接同一形式 ✓（`check.place` ✓），
                      一处改了另一处也要改的那种地方 ⇐ 记在这里 ✓。 */}
                  {/* 时区 ✓ —— ⚠️ **DB-IP 的 City 库没有这一项** ✗ ⇒ 用 DB-IP 时这一列会**全是"未知"** ✓。
                      那不是坏了 ✓，是那份库确实不提供 ✓（空比错好 ✓；编辑框那边也是这么显示的 ✓）。 */}
                  <div className="min-w-0 space-y-0.5">
                    <div className="truncate">
                      {n.risk ? (
                        <span className={`inline-flex items-baseline gap-1.5 rounded-full px-2 py-0.5 text-xs font-medium ${riskTone(n.risk.label)}`}>
                          <span className="font-mono">{n.risk.score ?? "—"}</span>
                          {n.risk.label ?? "未分档"}
                        </span>
                      ) : (
                        <Badge variant="outline" className="border-transparent bg-muted font-normal text-muted-foreground">
                        未启用或未查到
                      </Badge>
                      )}
                    </div>
                    {/* 第二行 = Tor 出口 ✓（**本地清单** ✓ 与上面的在线库**不是一条路** ✓ —— 表下声明说清了 ✓）。
                        ⚠️ **三种情形都保留** ✗：是 ⇒ 高亮 ✓ · 否 ⇒ 很淡 ✓ · 未知 ⇒ 明说"未知" ✓
                        （同事建议"否就不展示" ✓ —— 但那样"未知"会跟着消失 ✗✓，而那是两件事 ✓）。 */}
                    <div className="truncate text-xs">
                      {/* Tor 也做成徽标 ✓（与第二张图里「可远程升级」那个灰胶囊同形 ✓）——
                          ⚠️ 而**三种情形仍然分开写** ✗✓（是 / 否 / 未知 ✓）：原判断一字未改 ✓，只换外观 ✓。 */}
                      {n.tor_exit === true ? (
                        <Badge variant="outline" className="border-transparent bg-destructive/10 font-medium text-destructive">
                          Tor 出口
                        </Badge>
                      ) : n.tor_exit === false ? (
                        <Badge variant="outline" className="border-transparent bg-muted font-normal text-muted-foreground">
                          非 Tor
                        </Badge>
                      ) : (
                        <Badge variant="outline" className="border-transparent bg-muted font-normal text-muted-foreground/70">
                          Tor 未知
                        </Badge>
                      )}
                    </div>
                  </div>
                {/* ⭐ 「节点实测」✓（第一期：Netflix ✓）—— 这一格的命是**分辨三种"没有"** ✓✓：
                    ① **未允许检测** ✓（人可以去勾上 ✓）
                    ② **版本不支持** ✓（agent 太旧 ✓ —— **永远不会有** ✓，只能升级 ✓）
                    ③ **还没测过** ✓（再等等就有了 ✓）
                    ⇒ 混成一句会让人**去等一个永远不会来的结果** ✓✗（或去查一个其实没坏的东西 ✓）。
                    ⚠️ 判据顺序是刻意的 ✗：**先问"允许吗"、再问"会吗"** ✓ ——
                    因为"没允许"是**这台机器自己的政策** ✓，比"版本支不支持"更该先说 ✓。 */}
                {/* ⚠️ **这一格同时是入口** ✓（用户提的"点击弹出 card" ✓）——
                    所以它是个 `button` ✓（键盘可达 ✓、有焦点环 ✓），而**内容一字未改** ✓。
                    ⚠️ 行高不变 ✓：点阵卡是弹出的 ✓，不占表格的地方 ✓✓。 */}
                <button
                  type="button"
                  onClick={() => {
                    setProbeNode(n)
                    loadProbe(n)
                  }}
                  className="min-w-0 space-y-0.5 rounded-md text-left transition-colors hover:bg-muted/60 focus-visible:ring-2 focus-visible:ring-ring focus-visible:outline-none"
                  title="查看这台节点的探测历史"
                >
                  {/* ⚠️ **能点的样子必须看得见** ✗✓：它此前只有 hover 底色 ✓ ——
                      而白卡片上的 hover 底色几乎看不出来 ✓✗ ⇒ 静态时**毫无提示** ✓
                      （用户原话："节点实测没有点击的按钮" ✓）。⇒ 挂一个常驻的箭头 ✓，
                      与节点列表右侧那排图标是同一套语汇 ✓✓。 */}
                  <div className="flex items-center gap-1 truncate text-xs">
                    {n.stream_probe === false ? (
                      <Badge variant="outline" className="border-transparent bg-muted font-normal text-muted-foreground">
                        版本不支持
                      </Badge>
                    ) : n.allow_ip_quality === false ? (
                      <Badge variant="outline" className="border-transparent bg-warn/10 font-medium text-warn">
                        未允许检测
                      </Badge>
                    ) : probeSummary(n.probe) ? (
                      <span className="truncate font-medium">{probeSummary(n.probe)}</span>
                    ) : (
                      <Badge variant="outline" className="border-transparent bg-muted font-normal text-muted-foreground">
                        还没测过
                      </Badge>
                    )}
                    <ChevronRight className="size-3.5 shrink-0 text-muted-foreground/50" />
                  </div>
                  {/* ⚠️ **时效** ✓✓ —— 这一格的命就是它：没有"多久之前"，
                      一条三周前的"解锁"看起来和刚测的一样 ✓✗。 */}
                  {n.probe_at ? (
                    <div className="truncate text-[11px] text-muted-foreground/70">
                      {(() => {
                        const mins = Math.max(0, Math.round((Date.now() / 1000 - n.probe_at) / 60))
                        if (mins < 2) return "刚刚测的"
                        if (mins < 90) return `${mins} 分钟前`
                        const hours = Math.round(mins / 60)
                        return hours < 48 ? `${hours} 小时前` : `${Math.round(hours / 24)} 天前`
                      })()}
                    </div>
                  ) : null}
                </button>
                </div>
              ))}
            </div>
          </div>
          {/* ⚠️ **筛掉的东西必须说出来** ✗✓：不说的话，『3 台』看起来就像『整个机队只有 3 台』✓ ——
              这与『空单元格被读成没问题』是同一类错 ✓（**沉默的省略 = 假信息** ✓）。 */}
          {hiddenRows > 0 ? (
            <p className="text-xs text-muted-foreground">
              按当前筛选隐藏了 {hiddenRows} 台（共 {nodes.length} 台）
              <button
                type="button"
                className="ml-2 text-primary hover:underline"
                onClick={() => {
                  setOnlyData(false)
                  setQ("")
                }}
              >
                显示全部
              </button>
            </p>
          ) : null}
          {/* 表下声明 ✓✓ —— **不能省** ✗：一个彩色 Badge 很容易被当成
              "服务商的官方结论" ✓，而档位文字（低/较高/高 ✓）**是本面板按 25/75 划的** ✓。
              同理，四列数据来自**三条不同的路** ✓（本地库 ✓ 在线库 ✓ 官方清单 ✓），
              它们的隐私与开关行为都不一样 ✓ —— 一句话说清，比事后再解释便宜得多 ✓。 */}
          <p className="text-xs leading-relaxed text-muted-foreground">
            档位文字由本面板给出（按分数 25 / 75 划档），各家口径不同 · 风险来自在线库（第三方，需要开关）·
            ASN 与位置来自本地库（不外发）· Tor 出口来自官方出口清单（不外发，每小时更新）。
          </p>
          {/* 图例 ✓ —— 表格里那一列用了几个人话词 ✓，这里解释它们分别是什么意思 ✓。
              ⚠️ 它**不是**"预留说明"了 ✗：流媒体检测**已经上线** ✓（agent 每 6 小时自己探一次 ✓），
              所以旧版那句"等待 agent 支持"现在**是假话** ✓✗ —— 已改掉 ✓。 */}
          <div className="rounded-lg border border-dashed border-border bg-muted/30 p-3.5 text-xs leading-relaxed text-muted-foreground">
            <p className="font-medium text-foreground">关于「节点实测」</p>
            <ul className="mt-1 list-disc space-y-0.5 pl-4">
              <li>
                <span className="text-warn">未允许检测</span> —— 这台机器没开「允许 IP 质量检测」✓，
                允许之后 hub 才会为它查询与接收探测 ✓。
              </li>
              <li>
                <span className="text-muted-foreground/70">版本不支持</span> —— 它的 agent 还没有探测能力 ✓，
                升级 agent 之后才会有结果 ✓（在那之前等多久都不会有 ✓）。
              </li>
              <li>
                <span className="text-muted-foreground/70">还没测过</span> —— 有能力、也允许了 ✓，
                只是还没到它那一次（每 6 小时一次 ✓，各台按自己的时点错开 ✓）。
              </li>
              <li>
                <span className="text-warn">解锁（机房）</span> · <span className="text-warn">仅网页可用</span> ·
                <span className="text-warn">仅 App 可用</span> —— 这三种都是"能用，但有限制" ✓：
                机房 IP 在很多服务上吃亏 ✓；后两种是只通一边 ✓（例如网页能开、App 打不开 ✓）。
                ⚠️ 它们既不是"解锁"也不是"未解锁" ✗ ⇒ 单独列出来 ✓，不合并 ✓。
              </li>
              <li>
                <span className="text-warn">中国区</span> · <span className="text-warn">可用·无会员</span> ——
                这两档来自 YouTube ✓：前者是响应来自中国版（`www.google.cn` ✓）；后者是
                "视频能看，但 Premium 会员在这个国家买不到" ✓ —— ⚠️ 它能用 ✓，只是少一样东西 ✓
                ⇒ 不要把它当成"没解锁" ✗。
              </li>
              <li>
                结论带时效 ✓：今天解锁不等于下周解锁 ✓，所以每格下面都写着"多久之前测的" ✓。
                探测从节点自己发出 ✓（hub 代不了 ✓），而失败不等于未解锁 ✓ ——
                取不到页面时不会报结论，你会看到上一次的结论与它的年龄 ✓。
              </li>
            </ul>
          </div>
        </div>
      ) : page === "resource" ? (
        <>
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
                <a
                  key={b.key}
                  // 「落后」这一行点进去是**节点页、并且已经筛好"只看待升级"** ——
                  // 因为下一步多半就是动手（批量升级按钮在那一页），而不是再读一遍说明。
                  // 原来指向更新页：那里只是把同样的信息再讲一次。
                  href={b.key === "old" && b.count > 0 ? "/admin/nodes?outdated=1" : undefined}
                  className="flex items-baseline justify-between gap-3 rounded text-xs transition-colors hover:bg-muted/40"
                >
                  <span className="flex items-center gap-1.5 text-muted-foreground">
                    <span className={`size-2 rounded-sm ${b.color}`} />
                    {b.label}
                    <span className="font-mono">{b.version || "—"}</span>
                  </span>
                  <span className="shrink-0">
                    <span className="tnum font-medium">{b.count} 台</span>
                    <span className="tnum ml-1.5 text-muted-foreground">{nodes.length ? Math.round((b.count / nodes.length) * 100) : 0}%</span>
                  </span>
                </a>
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

      {/* 「需要处理的节点」与「用量榜」并排：两张都是五行的榜，高度天然相近，
          用同行的 items-stretch 对齐（与上面「近期事项 | Agent 版本分布」一致）。 */}
      <div className="grid grid-cols-1 items-stretch gap-4 lg:grid-cols-2">
        <Card>
          <CardHeader className="flex flex-row items-center justify-between gap-2">
            <CardTitle className="text-sm">需要处理的节点</CardTitle>
            <div className={TRACK}>
              {([["latency", "延迟最差"], ["loss", "丢包最多"], ["down", "离线最久"]] as const).map(([k, label]) => (
                <button key={k} type="button" onClick={() => setRiskTab(k)} aria-pressed={riskTab === k} className={pill(riskTab === k)}>
                  {label}
                </button>
              ))}
            </div>
          </CardHeader>
          <CardContent className="flex min-h-[9rem] flex-col space-y-1.5">
            {risk === null ? (
              <p className="text-sm text-muted-foreground">取不到数据。</p>
            ) : riskTab === "down" ? (
              (() => {
                // hub 给的是「`last_seen` 最旧且非 0」的几台；**在不在线仍以面板已有的 `online`
                // 标志为准**（阈值只该有一处）。按名字对上 —— 这一列的名字在界面上是唯一的。
                const offline = risk.down.filter((d) => nodes.some((n) => n.name === d.node && !n.online))
                const nowSec = Math.floor(Date.now() / 1000)
                return offline.length === 0 ? (
                  <p className="text-sm text-muted-foreground">没有离线的节点。</p>
                ) : (
                  offline.map((d, idx) => (
                    <div key={d.node} className="flex items-center gap-3 py-0.5 text-sm">
                      <span className="min-w-0 flex-1 truncate font-medium">{d.node}</span>
                      <span className={`tnum w-40 shrink-0 text-right text-xs ${idx === 0 ? "font-medium" : "font-medium"}`}>
                        已离线 {span(nowSec - d.last_seen)}
                      </span>
                    </div>
                  ))
                )
              })()
            ) : (
              (() => {
                const rows = riskTab === "latency" ? risk.latency : risk.loss
                return rows.length === 0 ? (
                  <p className="text-sm text-muted-foreground">
                    {riskTab === "latency" ? "近 7 天没有探测数据。" : "近 7 天没有丢包。"}
                  </p>
                ) : (
                  rows.map((r, idx) => (
                    // 定宽 + 不换行：数字列各自对齐，行高一致。**最差那行同时用加粗与语义色**
                    // 两种信号（颜色不单独承担含义）。
                    <div key={`${r.task}-${r.node}`} className="flex items-center gap-3 py-0.5 text-sm">
                      <span className="min-w-0 flex-1 truncate font-medium">{r.node}</span>
                      <span className="w-16 shrink-0 truncate text-xs text-muted-foreground">{r.task}</span>
                      <span className={`tnum w-24 shrink-0 text-right text-xs ${idx === 0 ? "font-medium" : "font-medium"}`}>
                        {riskTab === "latency" ? `平均 ${Math.round(r.value)} ms` : `丢包 ${(r.value * 100).toFixed(1)}%`}
                      </span>
                      <span className="tnum w-16 shrink-0 text-right text-xs text-muted-foreground">{r.samples} 次</span>
                    </div>
                  ))
                )
              })()
            )}
          </CardContent>
        </Card>

      {/* 用量榜：只报**现状**（谁用得最满），不做"还能用几天"这类预测。 */}
      <Card>
        <CardHeader className="flex flex-row items-center justify-between gap-2">
          <CardTitle className="text-sm">用量榜</CardTitle>
          <span className="text-xs text-muted-foreground">按本月已用额度排序</span>
        </CardHeader>
        <CardContent className="flex min-h-[9rem] flex-col space-y-1.5">
          {quota.length === 0 ? (
            <p className="text-sm text-muted-foreground">没有节点设置流量额度。</p>
          ) : (
            quota.map(({ node: n, used, pct }) => {
              const over = pct >= 1
              return (
                // 定宽 + 不换行：百分比在最后、右对齐，永远与数字列同一行（原来会被挤到第二行）。
                <div key={n.id} className="flex items-center gap-3 py-0.5 text-sm">
                  <span className="min-w-0 flex-1 truncate font-medium">{n.name}</span>
                  <span className="w-20 shrink-0 truncate text-xs text-muted-foreground">{n.group || "未分组"}</span>
                  <span className="w-24 shrink-0 overflow-hidden rounded-full bg-muted">
                    <span
                      className={`block h-2 rounded-full ${over ? "bg-danger-fg" : "bg-primary"}`}
                      style={{ width: `${Math.min(100, pct * 100)}%` }}
                    />
                  </span>
                  <span className="tnum w-32 shrink-0 text-right text-xs text-muted-foreground">
                    {bytes(used)} / {bytes(n.traffic_limit)}
                  </span>
                  <span className={`tnum w-12 shrink-0 text-right text-xs ${over ? "font-semibold text-danger-fg" : "font-medium"}`}>
                    {Math.round(pct * 100)}%
                  </span>
                </div>
              )
            })
          )}
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

        </>
      ) : (
        <CostBlock nodes={allNodes} group={group} />
      )}
        {/* ⭐ **"我已续费"那张卡** ✓（用户提的做法 ✓）：入口是总览页顶部
            「30 天内到期」那张 KPI 卡 ✓（点它弹出来 ✓）。一行一台 ✓，
            行尾「我已续费」⇒ **变成"确认续期？"** ✓（这就是二次确认 ✓——
            而且每行本来就把**新日期**写出来了 ✓✓：看到的，就是将要写进去的 ✓）。
            ⚠️ 续期走 `renewedExpiry`（纯函数 ✓ 带十个测试 ✓）：未到期从原日期接 ✓、
            已到期从今天起 ✓；**认不出周期或一次性 ⇒ 不能续** ✓（按钮置灰 ✓）。 */}
        <Dialog
          open={renewOpen}
          onOpenChange={(v) => {
            if (!v) {
              setRenewOpen(false)
              setRenewAsk(null)
            }
          }}
        >
          <DialogContent className="flex max-h-[calc(100dvh-4rem)] flex-col overflow-hidden sm:max-w-lg">
            <DialogHeader>
              <DialogTitle className="text-sm">临近到期的节点（{expiringNodes.length} 台）</DialogTitle>
            </DialogHeader>
            <div className="min-h-0 flex-1 divide-y divide-border overflow-y-auto overscroll-contain pr-1">
              {expiringNodes.map((n) => {
                const next = renewedExpiry(n.expires_at, n.billing_cycle, todayLocal)
                const asking = renewAsk === n.id
                return (
                  <div key={n.id} className="flex flex-wrap items-center gap-x-3 gap-y-2 py-3">
                    <span className="font-medium">{n.name}</span>
                    <span className="text-xs text-muted-foreground">{n.group || "未分组"}</span>
                    <Expiry date={n.expires_at} />
                    <span className="ml-auto text-xs text-muted-foreground">
                      {next ? `续一期 \u21d2 ${next}` : "未填计费周期"}
                    </span>
                    <Button
                      size="sm"
                      variant={asking ? "default" : "outline"}
                      disabled={!next || renewing}
                      onClick={() => {
                        if (!next) return
                        if (!asking) {
                          setRenewAsk(n.id)
                          return
                        }
                        setRenewing(true)
                        api(`/nodes/${n.id}`, {
                          method: "PUT",
                          body: JSON.stringify(changes(n, { expires_at: next })),
                        })
                          .then(() => {
                            setRenewAsk(null)
                            refresh()
                          })
                          .catch(() => {})
                          .finally(() => setRenewing(false))
                      }}
                    >
                      {asking ? "确认续期？" : "我已续费"}
                    </Button>
                  </div>
                )
              })}
            </div>
          </DialogContent>
        </Dialog>

        {/* ⭐ **点阵卡** ✓（一行一个服务 ✓ 一格 = 一次 12 小时 ✓）。
            ⚠️ 它必须待在三元表达式**之外** ✗✗ —— 我第一版把它贴在"某天到期明细"那个弹窗旁边 ✓，
            而那个弹窗属于「资源/成本」那一支 ✓✗ ⇒ 在「IP 质量」下**根本没被渲染** ✓✗。
            ⚠️ 三态都在这里 ✓，失败那一态有"重试" ✓。 */}
        <Dialog
          open={probeNode !== null}
          onOpenChange={(v) => {
            if (!v) {
              setProbeNode(null)
              setProbeRows(null)
              setProbeErr(false)
            }
          }}
        >
          {/* 宽度用 `min(96vw, 1100px)`（用户要求再放大一些）。
              120 格 × 最小 3px + 119 道缝 ≈ 600px，加服务名 112px 与徽章 96px ≈ 860px。 */}
          {/* ⚠️⚠️ 宽度用**内联 style** ✓✓ —— 本仓 DialogContent 自带 `w-full` ✓，
              而两个同属性的工具类谁生效看的是**样式表顺序** ✓✗（不是 className 里的顺序 ✓），
              所以我先前加的 `w-[…]` 一直被压住 ✓✗。内联 style 不参与那场优先级游戏 ✓。
              ⚠️ 组件本身是 `left-[50%] translate-x-[-50%]` ✓ ⇒ 居中不用管 ✓，只管宽度 ✓。 */}
          <DialogContent
            className="max-h-[85vh] overflow-y-auto"
            // ⚠️ 宽度按**点阵的真实需求**算 ✓：120 格 × 9px = 1080 ✓ + 服务名 112 ✓
            // + 状态徽章 80 ✓ + 内边距与列间距 ⇒ 约 1350px ✓ ⇒ 所以上限提到 1320 ✓
            //（再窄就会把点挤出容器 ✓✗ —— 而点的宽度是固定的 ✓，挤不掉 ✓）。
            // ⚠️ 宽度按**实测需求**算 ✓（不拍脑袋 ✓）：120 格 × 9px = 1080 ✓
            // + 119 道 1px 缝 = 119 ✓ ⇒ 点阵本身 **1199px** ✓
            // + 服务名 112 ✓ + 状态徽章 80 ✓ + 内边距 ≈ **1423px** ⇒ 上限取 1440 ✓✓
            // ⚠️ 宽度按**现在的点数**重算 ✓（一格一天 ⇒ 30 格 ✓）：
            // 30 × 9px = 270 ✓ + 29 道 1px 缝 = 29 ✓ ⇒ 点阵 **299px** ✓
            // + 服务名 112 ✓ + 徽章 80 ✓ + 内边距 ≈ **520px** ⇒ 上限取 640 ✓✓
            // ⚠️ 不能沿用 1440 ✗：列会被拉宽 ✓✗ ⇒ 点与点的**视觉间距又变大** ✓（那正是刚改掉的东西 ✓）。
            style={{ width: "min(96vw, 960px)", maxWidth: "none" }}
          >
            <DialogHeader>
              <DialogTitle className="text-sm">{probeNode?.name ?? ""} 的探测历史</DialogTitle>
            </DialogHeader>
            {probeErr ? (
              <div className="space-y-2 text-xs">
                <p className="text-destructive">没能取到探测历史。</p>
                <p className="text-muted-foreground">
                  这通常是网络或 hub 刚重启造成的——点下面重试一次即可。
                </p>
                <Button size="sm" variant="outline" onClick={() => probeNode && loadProbe(probeNode)}>
                  重试
                </Button>
              </div>
            ) : probeRows === null ? (
              <p className="text-xs text-muted-foreground">读取中…</p>
            ) : probeRows.length === 0 ? (
              <p className="text-xs text-muted-foreground">
                还没有记录。历史从 hub 升级到 1.11.16 之后才开始累积，早先只保留了最后一次结果。
              </p>
            ) : (
              <ProbeMatrix rows={probeRows} />
            )}
          </DialogContent>
        </Dialog>
    </div>
  )
}
/// ⭐ **探测点阵** ✓✓（用户提的做法 ✓）：一行一个服务 ✓ · 一格 = 一次 6 小时 ✓ ——
/// 点会汇成一张阵 ✓，于是"**从什么时候开始不对劲**"一眼可见 ✓✓。
///
/// ⚠️⚠️ **列的位置由时间算，不是"有几个点就画几个"** ✗✗ —— 这是整个组件最要紧的地方 ✓：
/// 夹具里就有"整整一天没有记录"的情形 ✓，若按顺序画 ✓，缺的那一天会让**后面的每个点都左移** ✓✗
/// ⇒ 点阵会**谎报时间** ✓✗（正是"错值看起来完全正常"那一类 ✓）。
/// ⇒ 第 k 列固定是 `start + k×6h` ✓✓。
///
/// ⚠️ **空格与"测了不通"必须长得不一样** ✓✓：没数据 ⇒ 一个很淡的**空槽** ✓
///（不是灰点 ✓ 也不是红点 ✓）—— 这与"失败不是结论"是同一条 ✓，只是这次它**看得见** ✓。
/// ⚠️ **整行没有数据**（这个服务从没测过 ✓）⇒ 单独写一句 ✓，而不是画 120 个空槽 ✓✗。
const PROBE_SERVICES = ["netflix", "tiktok", "reddit", "chatgpt", "amazonpv", "youtube"] as const
const PROBE_LABELS: Record<string, string> = {
  netflix: "Netflix",
  tiktok: "TikTok",
  reddit: "Reddit",
  chatgpt: "ChatGPT",
  amazonpv: "Prime Video",
  youtube: "YouTube",
}
// ⚠️ **一格 = 一天** ✓（用户要求"用更少的点表达相同的天数" ✓）——
// 30 天 = **30 格** ✓（原先是每 6 小时一格 ⇒ 120 格 ⇒ 卡片被撑得很宽 ✓✗）。
// ⚠️ **一格 = 12 小时** ✓（用户要求"点数增加一倍，刚好填满空缺" ✓）——
// 30 天 = **60 格** ✓；60 × 9px + 59 道 1px 缝 = 599px ✓ ⇒ 加上服务名与徽章 ≈ 820px ✓
// ⇒ 正好落在 960px 的卡片里 ✓✓（宽度不用再动 ✓）。
const PROBE_STEP = 43_200
// ⚠️ **14 天**（用户要求"不需要显示这么多天" ✓）—— 14 × 4 = **56 格** ✓。
// ⚠️ 而 hub 那边的**保留仍是 30 天** ✓（数据留着 ✓，只是这里少画一些 ✓ —— 想看更多时改这一个数即可 ✓）。
/// 状态的"严重程度" ✓（**数字越小越严重** ✓）—— 同一格里合成时用它 ✓（见下面那段说明 ✓）。
function probeRank(status?: string): number {
  switch (status) {
    case "blocked":
    case "cn":
      return 0
    case "idc":
    case "web":
    case "app":
    case "noprem":
      return 1
    case "unknown":
      return 2
    case "unlocked":
      return 3
    default:
      return 4
  }
}

const PROBE_SLOTS = 60

/// 一格的观感 ✓（四种色沿用本仓 `riskTone` 那一套 ✓，不发明 ✗）。
/// ⚠️ 只给"值得看的"上色 ✗：正常解锁是**常态** ✓ ⇒ 淡绿 ✓；有问题的才抢眼 ✓✓。
function probeTone(status?: string): string {
  switch (status) {
    case "unlocked":
      return "bg-ok/60"
    case "blocked":
      return "bg-destructive"
    case "idc":
    case "web":
    case "app":
    case "noprem":
      return "bg-warn"
    case "cn":
      return "bg-destructive/70"
    case "unknown":
      return "bg-muted-foreground/40"
    default:
      return ""
  }
}

function probeWord(status: string): string {
  switch (status) {
    case "unlocked":
      return "解锁"
    case "blocked":
      return "未解锁"
    case "idc":
      return "解锁（机房）"
    case "web":
      return "仅网页可用"
    case "app":
      return "仅 App 可用"
    case "cn":
      return "中国区"
    case "noprem":
      return "可用·无会员"
    case "unknown":
      return "没读明白"
    default:
      return status
  }
}

/// ⭐ **那一格的摘要** ✓✓（用户第 ③ 条 ✓）：**点名有问题的** ✓，正常的只计数 ✓。
///
/// ⚠️ 取舍写在前面 ✗："六项全正常"是常态 ✓ ⇒ 逐项列出来只会让那一格变高 ✓✗；
/// 而运维真正要的是"**哪台不对劲**" ✓ ⇒ 所以坏消息点名、好消息计数 ✓✓
///（与那一列"坏消息在前"的排法同一条 ✓）。
/// ⚠️ 纯函数 ✓：输入就是 hub 给的 `probe` 对象 ✓ ⇒ 不依赖任何组件状态 ✓，逻辑一眼可验 ✓。
/// ⚠️ 顺序固定按 `PROBE_SERVICES` ✓（不按对象键序 ✓✗ —— 那个顺序在 JS 里是"看起来稳定但没说死"的 ✓）。
function probeSummary(probe?: Record<string, { status?: string; region?: string }> | null): string | null {
  if (!probe) return null
  const bad: string[] = []
  let ok = 0
  for (const sv of PROBE_SERVICES) {
    const r = probe[sv]
    if (!r) continue
    if (r.status === "unlocked") ok += 1
    else bad.push(PROBE_LABELS[sv] ?? sv)
  }
  if (bad.length === 0) return ok > 0 ? `${ok} 项正常` : null
  const rest = ok > 0 ? ` · 其余 ${ok} 项正常` : ""
  // ⚠️ 坏消息太多时也不铺满 ✗：最多点两个名 ✓ + 一个计数 ✓（那一格只有一行 ✓）。
  const named = bad.slice(0, 2).join(" · ")
  const more = bad.length > 2 ? ` 等 ${bad.length} 项` : ""
  return `${named}${more}${rest}`
}

function ProbeMatrix({ rows }: { rows: ProbeHistoryRow[] }) {
  const latest = rows.reduce((m, r) => Math.max(m, r.at), 0)
  const end = Math.ceil(latest / PROBE_STEP) * PROBE_STEP
  const start = end - (PROBE_SLOTS - 1) * PROBE_STEP
  // ⚠️ 列号由**时间**算 ✓✓（不是数组下标 ✗ —— 见上面那段说明 ✓）
  const slotOf = (at: number) => Math.round((at - start) / PROBE_STEP)
  const byService = new Map<string, Map<number, ProbeHistoryRow>>()
  // ⚠️ **另外记一份"这一格测到了几次"** ✓（用户要求 ✓）——
  // 独立于上面那个 map ✓：那个存"最坏的那一次"（决定颜色 ✓），这个只用来写悬停 ✓。
  // 分开的好处 ✓：改这个不影响颜色规则 ✓（一处负责一件事 ✓）。
  const counts = new Map<string, Map<number, number>>()
  for (const r of rows) {
    const k = slotOf(r.at)
    if (k < 0 || k >= PROBE_SLOTS) continue
    if (!byService.has(r.service)) byService.set(r.service, new Map())
    const slot = byService.get(r.service)!
    const prev = slot.get(k)
    // ⚠️⚠️ **同一天里"坏消息优先"** ✗✗ —— 一刻度变成一天之后 ✓，一天会有四次探测 ✓，
    // 它们要合成一格 ✓ ⇒ 规则必须是"**只要有一次不好，这格就不好**" ✓✓：
    // 反过来（后写的覆盖前写的 ✓、或"只要好就算好" ✗）会把**一天的故障藏起来** ✓✗，
    // 而那正是这个界面最该让人看见的东西 ✓。
    if (!prev || probeRank(r.status) < probeRank(prev.status)) slot.set(k, r)
    // 计数 ✓：与颜色**无关** ✓ —— 一格里有几次就记几次 ✓
    if (!counts.has(r.service)) counts.set(r.service, new Map())
    const c = counts.get(r.service)!
    c.set(k, (c.get(k) ?? 0) + 1)
  }
  // ⚠️ **有问题的排在上面** ✓（运维先看坏的 ✓）
  // ⚠️ 顺手把"最新一格"也带出来 ✓（徽章要用 ✓）—— 在数据里算一次 ✓，
  // 好过在 JSX 里现算两遍 ✓（那种写法早晚会两处不一致 ✓）。
  const order = PROBE_SERVICES.map((sv) => {
    const m = byService.get(sv)
    const lastK = m && m.size ? Math.max(...m.keys()) : -1
    const st = lastK >= 0 ? m!.get(lastK)!.status : undefined
    // 名次：有问题的最前 ✓ · 正常的中间 ✓ · 从没测过的最后 ✓（它是"没有"，不该抢眼 ✓）
    const rank = m === undefined ? 2 : st === "unlocked" ? 1 : 0
    const last = lastK >= 0 ? m!.get(lastK) : undefined
    return { sv, m, rank, last }
  }).sort((a, b) => a.rank - b.rank || a.sv.localeCompare(b.sv))
  const fmt = (at: number) => new Date(at * 1000).toLocaleString()
  return (
    <div className="space-y-2 text-xs">
      <div className="flex items-center gap-2 text-[11px] text-muted-foreground/70">
        <span className="w-28 shrink-0" />
        {/* ⚠️ **相对时间** ✓（同事第 ④ 条 ✓）：绝对时间又长又死板 ✓✗ ——
            精确到秒的那一刻留给悬停 ✓（每个点本来就有 title ✓✓）。 */}
        <span>30 天前</span>
        <span className="mx-auto">15 天前</span>
        <span>今天</span>
      </div>
      {order.map(({ sv, m, last }) => (
        <div key={sv} className="flex items-center gap-2">
          {/* ⚠️ 名字后带**最新地域** ✓（用户第 ② 条 ✓："YouTube(UK)" ✓）——
                数据里本来就有 `last.region` ✓，只是先前没显示 ✓。
                ⚠️ 用 `last`（最新一格 ✓）而不是历史里随便一个 ✓ —— 与右端徽章同源 ✓✓。 */}
          <span className="w-28 shrink-0 truncate font-medium" title={last?.region ? `${PROBE_LABELS[sv] ?? sv}(${last.region})` : undefined}>
            {PROBE_LABELS[sv] ?? sv}
            {last?.region ? <span className="text-muted-foreground">({last.region})</span> : null}
          </span>
          {m === undefined ? (
            <div
              className="grid min-w-0 flex-1 gap-px"
              style={{ gridTemplateColumns: `repeat(${PROBE_SLOTS}, minmax(3px, 1fr))` }}
              title="这个服务还没有任何记录"
            >
              {Array.from({ length: PROBE_SLOTS }, (_, k) => (
                <span
                  key={k}
                  className="mx-auto h-[15px] w-[9px] rounded-[3px] border border-muted-foreground/35"
                />
              ))}
            </div>
          ) : (
            <div
              className="grid min-w-0 flex-1 gap-px"
              // 这里是点阵"消失"的根因：原来是 minmax(0, 1fr) —— `0` 表示"可以压到 0 宽"，
              // 缝（120 格 × 2px）再加上行尾 w-24 的徽章（96px）一挤，每一列就变成 0 像素，
              // 于是中间看起来什么都没有（而文字与徽章都正常）。给一个最小宽度：宁可横向滚动。
              style={{ gridTemplateColumns: `repeat(${PROBE_SLOTS}, minmax(3px, 1fr))` }}
            >
              {Array.from({ length: PROBE_SLOTS }, (_, k) => {
                const r = m.get(k)
                const at = start + k * PROBE_STEP
                return r ? (
                  <span
                    key={k}
                    title={`${fmt(at)} · ${probeWord(r.status)}${r.region ? ` · ${r.region}` : ""} · ${counts.get(sv)?.get(k) ?? 1} / 2 次`}
                    // ⚠️ **宽高比 3:4 的小长条** ✓（用户第 ① 条 ✓）—— 此前是 h-3.5 + rounded-full ✓，
                    // 而 56 格时每列宽约 15px ✓ ⇒ rounded-full 在 ~15×14 上就是个**椭圆** ✓✗。
                    // ⇒ 固定 9×12 ✓ + rounded-[3px] ✓（四角一点弧度 ✓）+ **居中** ✓
                    //（不居中的话它会被拉回满列宽 ✓✗，又变成椭圆 ✓）。
                    className={`mx-auto h-[15px] w-[9px] rounded-[3px] ${probeTone(r.status)}`}
                  />
                ) : (
                  <span
                    key={k}
                    title={`${fmt(at)} · 没有记录`}
                    className="mx-auto h-[15px] w-[9px] rounded-[3px] border border-muted-foreground/35"
                  />
                )
              })}
            </div>
          )}
          {/* ⭐ **最新状态** ✓✓（前端同事的第 ③ 条 ✓ —— 也是最值钱的一条 ✗）：
              看历史图时，第一眼最想确认的其实是"**现在通不通**" ✓；
              此前六行全绿的时候，这一点根本看不出来 ✓✗。 */}
          {m === undefined ? (
            <span className="w-20 shrink-0" />
          ) : last ? (
            <Badge variant="outline" className={`w-20 shrink-0 justify-center border-transparent ${probeTone(last.status)} font-normal`}>
              {probeWord(last.status)}
            </Badge>
          ) : null}
        </div>
      ))}
      {/* ⚠️ **图例 + 一句收起** ✓（前端同事的第 ⑤ 条 ✓）：
          原来那段像免责声明 ✓✗（"一格 = 6 小时 / 没测到与测了不通是两回事 / 从哪天开始记" ✓）
          ⇒ 图例放在外面 ✓（一眼看懂 ✓），细节收进 `Help` 的悬停层 ✓✓（本仓现成的组件 ✓）。 */}
      <div className="flex flex-wrap items-center gap-x-4 gap-y-1 border-t border-border pt-2 text-[11px] text-muted-foreground">
        <span className="flex items-center gap-1.5">
          <span className="size-2.5 rounded-full bg-ok/45" />
          解锁
        </span>
        <span className="flex items-center gap-1.5">
          <span className="size-2.5 rounded-full bg-warn" />
          受限（机房 / 仅一边可用 / 买不到会员）
        </span>
        <span className="flex items-center gap-1.5">
          <span className="size-2.5 rounded-full bg-destructive" />
          不通
        </span>
        <span className="flex items-center gap-1.5">
          <span className="size-2.5 rounded-full bg-muted/60" />
          没有记录
        </span>
        <Help>
          一格 = 一次 6 小时，共 30 天。很淡的空槽表示那一次
          <span className="font-medium">没有记录</span>
          ——它与"测了不通"是两回事，不要当成结论。历史从 hub 开始记录那天起累积，
          更早的结果只保留了最后一次；把鼠标停在某一格上可以看到那一格的时间与结论。
        </Help>
      </div>
    </div>
  )
}


// ───────────────────────── 成本 ─────────────────────────

/** 计费周期 → 月数。认不出的一律按 1 个月（宁可少算一项，也不要静默漏掉整台机器）。 */
/** 计费周期的**天数**，用于"剩余价值 = 价格 ÷ 周期天数 × 剩余天数"（维护者给的口径）。
 *  与 `cycleMonths` 分开：那个用来摊平成"每月"，这个用来按天折算剩余。 */
function cycleDays(cycle: string): number {
  switch ((cycle || "").toLowerCase()) {
    case "quarterly": case "quarter": case "3m": return 91
    case "semiannual": case "semi-annual": case "halfyear": case "half-yearly": case "6m": return 182
    case "yearly": case "annual": case "annually": case "year": case "12m": return 365
    default: return 30
  }
}

function cycleMonths(cycle: string): number {
  switch ((cycle || "").toLowerCase()) {
    case "quarterly": case "quarter": case "3m": return 3
    case "semiannual": case "semi-annual": case "halfyear": case "half-yearly": case "6m": return 6
    case "yearly": case "annual": case "annually": case "year": case "12m": return 12
    default: return 1
  }
}

const cny = (v: number) => (v >= 10000 ? `¥${(v / 10000).toFixed(2)} 万` : `¥${v.toFixed(v < 100 ? 2 : 0)}`)

/** 一行成本。**两种缺失分得很清**：没填价格 → `none`；有价格但没汇率 → `norate`。 */
type CostRow = { n: Node; monthly: number | null; why: "none" | "norate" | null }

function costRow(n: Node, perUnit: Record<string, number>): CostRow {
  if (!(n.price > 0)) return { n, monthly: null, why: "none" }
  const rate = perUnit[n.currency]
  if (!rate || rate <= 0) return { n, monthly: null, why: "norate" }
  return { n, monthly: (n.price / cycleMonths(n.billing_cycle)) * rate, why: null }
}

/** 到期状态。用**本仓的语义 token**（`warn-fg` / `danger-fg`），不写死调色板 —— 深色模式才跟着走。 */
function ExpiryBadge({ date }: { date: string | null }) {
  const d = daysUntil(date)
  if (d === null) return <span className="text-xs text-muted-foreground">∞</span>
  const tone = d < 0 ? "bg-danger-fg/10 text-danger-fg" : d <= 7 ? "bg-warn-fg/10 text-warn-fg" : "bg-muted text-muted-foreground"
  const label = d < 0 ? `已过期 ${-d} 天` : d === 0 ? "今天到期" : d <= 7 ? `${d} 天后到期` : (date ?? "")
  return <span className={`rounded-full px-2 py-0.5 text-[11px] font-medium ${tone}`}>{label}</span>
}

/**
 * 成本块：KPI + 未来 12 个月 + 每台服务器。
 *
 * 分区、间距与组件选用照前端同事的方案（shadcn/ui 规范）；颜色一律走本仓 token ——
 * 他给的 `bg-amber-500/10` 之类会写死调色板，深色模式下会破，这里换成 `warn-fg`。
 *
 * 两条来自实际使用的约束：
 * - 表格默认只列前 12 行、其余收进对话框（100 台时把整页拉长是不能接受的）；
 * - 月份的逐台明细是**常驻 callout**，不是浮在柱子上的卡片（不遮挡、不会在最左/最右顶出边界）。
 */
function CostBlock({ nodes, group }: { nodes: Node[]; group: string }) {
  const [fx, setFx] = useState<{ per_unit: Record<string, number>; fetched_at: number; manual: boolean; available: boolean } | null>(null)
  const [sort, setSort] = useState<"monthly" | "expiry">("monthly")
  const [allOpen, setAllOpen] = useState(false)
  const [pick, setPick] = useState<string | null>(null)
  const [only, setOnly] = useState<"priced" | "noprice" | "norate" | "all">("priced")
  const [q, setQ] = useState("")

  useEffect(() => {
    let alive = true
    api<{ per_unit: Record<string, number>; fetched_at: number; manual: boolean; available: boolean }>("/fx")
      .then((d) => alive && setFx(d))
      .catch(() => alive && setFx(null))
    return () => { alive = false }
  }, [])

  const shown = group ? nodes.filter((n) => n.group === group) : nodes
  const perUnit = fx?.per_unit ?? {}
  const rows = shown.map((n) => costRow(n, perUnit))
  const priced = rows.filter((r) => r.monthly !== null) as (CostRow & { monthly: number })[]
  const unpriced = rows.length - priced.length
  const norate = rows.filter((r) => r.why === "norate").length
  const monthly = priced.reduce((a, r) => a + r.monthly, 0)

  const now = new Date()
  const months = Array.from({ length: 12 }, (_, i) => {
    const d = new Date(now.getFullYear(), now.getMonth() + i, 1)
    return { key: `${d.getFullYear()}-${d.getMonth()}`, label: `${d.getMonth() + 1} 月`, total: 0, rows: [] as typeof priced }
  })
  const firstKey = months[0].key
  for (const r of priced) {
    const exp = r.n.expires_at ? new Date(r.n.expires_at) : null
    const key = exp && !isNaN(exp.getTime()) ? `${exp.getFullYear()}-${exp.getMonth()}` : firstKey
    const hit = months.find((m) => m.key === key) ?? months[0]
    hit.total += r.monthly
    hit.rows.push(r)
  }
  const peak = Math.max(1, ...months.map((m) => m.total))
  const picked = months.find((m) => m.key === pick) ?? null

  const filtered = rows.filter((r) => {
    if (q && !`${r.n.name} ${r.n.group ?? ""}`.toLowerCase().includes(q.toLowerCase())) return false
    if (only === "priced") return r.monthly !== null
    if (only === "noprice") return r.why === "none"
    if (only === "norate") return r.why === "norate"
    return true
  })
  const bySort = [...filtered].sort((a, b) =>
    sort === "monthly" ? (b.monthly ?? -1) - (a.monthly ?? -1) : (daysUntil(a.n.expires_at) ?? 1e9) - (daysUntil(b.n.expires_at) ?? 1e9),
  )
  const top = bySort.slice(0, 12)

  // 分组占比：**金额降序取前 3**，其余合并成「其他」—— 色数因此固定（3 主色 + 1 中性），
  // 图例不随分组数增长。这是 AWS/GCP 账单看板的标准做法（维护者的前端同事提的），
  // 但它属于**成分图**，所以只用在占比条上；12 个月那根柱图回答的是"哪个月花钱"，不堆分组。
  const byGroupAll = new Map<string, number>()
  for (const r of priced) {
    const g = r.n.group || "未分组"
    byGroupAll.set(g, (byGroupAll.get(g) ?? 0) + r.monthly)
  }
  const ranked3 = [...byGroupAll.entries()].sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]))
  const topByGroup = ranked3.slice(0, 3)
  const restSum = ranked3.slice(3).reduce((a, [, v]) => a + v, 0)
  const byGroup = new Map<string, number>(topByGroup)
  if (restSum > 0) byGroup.set("其他", restSum)
  // **只用三主色 + 一个中性**：中性专给「其他」（表示"这不是某一个具体分组"）。
  // 中性用 token（`bg-muted-foreground`），不写死 `#94a3b8` —— 深色模式才跟着主题走。
  // **同色三阶**：`bg-primary` / 60% / 30% —— 不借语义色（`bg-info-fg` 这类 token 本仓
  // 并没有定义，Tailwind 对不存在的工具类**不报错、什么都不生成**，于是那一段会变成透明 —— 我踩过）。
  // "同一色调由深到浅"也正好表达"份额由大到小"，比三种互不相关的颜色更贴合占比图。
  const tones = ["bg-primary", "bg-primary/60", "bg-primary/30"]

  const table = (list: CostRow[]) => (
    <Table>
      <TableHeader>
        <TableRow className="border-border/60 hover:bg-transparent">
          <TableHead className="w-[26%] text-xs font-medium">节点</TableHead>
          <TableHead className="w-[12%] text-xs font-medium">分组</TableHead>
          <TableHead className="w-[22%] text-xs font-medium">价格</TableHead>
          <TableHead className="w-[13%] text-right text-xs font-medium">
            <button type="button" className="hover:text-foreground" onClick={() => setSort("monthly")}>每月{sort === "monthly" ? " ↓" : ""}</button>
          </TableHead>
          <TableHead className="w-[13%] text-right text-xs font-medium">每年</TableHead>
          <TableHead className="w-[14%] text-right text-xs font-medium">
            <button type="button" className="hover:text-foreground" onClick={() => setSort("expiry")}>到期{sort === "expiry" ? " ↑" : ""}</button>
          </TableHead>
        </TableRow>
      </TableHeader>
      <TableBody>
        {list.map((r) => (
          <TableRow key={r.n.id} className="border-border/40">
            <TableCell className="py-2 font-medium">{r.n.name}</TableCell>
            <TableCell className="py-2">
              {r.n.group ? <Badge variant="secondary" className="font-normal">{r.n.group}</Badge> : <span className="text-muted-foreground">—</span>}
            </TableCell>
            <TableCell className="py-2 font-mono text-xs text-muted-foreground">
              {r.n.price > 0 ? `${r.n.price} ${r.n.currency} / ${r.n.billing_cycle}` : <Badge variant="outline" className="border-transparent bg-muted font-normal text-muted-foreground">未设置</Badge>}
            </TableCell>
            <TableCell className="py-2 text-right font-mono tabular-nums font-medium">
              {r.monthly === null ? <span className="text-muted-foreground">—</span> : cny(r.monthly)}
            </TableCell>
            <TableCell className="py-2 text-right font-mono tabular-nums text-muted-foreground">
              {r.monthly === null ? "—" : cny(r.monthly * 12)}
            </TableCell>
            <TableCell className="py-2 text-right"><ExpiryBadge date={r.n.expires_at} /></TableCell>
          </TableRow>
        ))}
        {list.length === 0 && (
          <TableRow><TableCell colSpan={6} className="py-6 text-center text-xs text-muted-foreground">没有符合条件的节点。</TableCell></TableRow>
        )}
      </TableBody>
    </Table>
  )

  return (
    // 三张卡，各回答一个问题（与"资源"那一屏同构）：总览 / 哪个月花钱 / 每台多少钱。
    // 整块仍占满一整行（它落在页面末尾那个两列网格里）。
    <div className="space-y-4 lg:col-span-2">
      <Card>
      <CardHeader className="gap-0 border-b border-border/60 pt-0.5 pb-2!">
        <CardTitle className="text-sm">成本概览</CardTitle>
        <CardAction>
          <span className="text-xs text-muted-foreground">
            {fx === null ? "汇率读取中…"
              : !fx.available ? "未有汇率（不会按 1:1 估算）"
              : fx.manual ? `手动汇率 · 1 USD = ${perUnit.USD?.toFixed(4)} CNY`
              : `汇率 1 USD = ${perUnit.USD?.toFixed(4)} CNY · ${fx.fetched_at ? new Date(fx.fetched_at * 1000).toLocaleDateString("zh-CN") : "—"}`}
          </span>
        </CardAction>
      </CardHeader>
      <CardContent className="space-y-5 pt-4">
        {/* ① 三个指标各占一个浅色嵌板。第三格是"待补全"，用 Badge 标出来而不是让汉字挂在数字下面。 */}
        <div className="grid gap-3 sm:grid-cols-3">
          <div className="rounded-lg border border-border/60 bg-muted/40 p-3">
            <div className="text-xs text-muted-foreground">每月成本{group ? `（${group}）` : ""}</div>
            <div className="tnum mt-2 text-3xl leading-none font-semibold tracking-tight">{cny(monthly)}</div>
            <div className="mt-0.5 text-xs text-muted-foreground">按当前价格摊平</div>
          </div>
          <div className="rounded-lg border border-border/60 bg-muted/40 p-3">
            <div className="text-xs text-muted-foreground">折合每年</div>
            <div className="tnum mt-2 text-3xl leading-none font-semibold tracking-tight">{cny(monthly * 12)}</div>
            <div className="mt-0.5 text-xs text-muted-foreground">同上 × 12</div>
          </div>
          {/* 第三格与另两格**同骨架**（标签 / 主数字 / 一行小字）：`98 台` 里"台"按资源卡的写法弱化，
              否则两行小字会把它的视觉中心压得与邻格错位。 */}
          <div className="rounded-lg border border-border/60 bg-muted/40 p-3">
            <div className="flex items-center justify-between gap-2">
              <span className="text-xs text-muted-foreground">待补全信息</span>
              <Badge variant="outline" className="shrink-0 font-normal">{unpriced > 0 ? "需跟进" : "齐了"}</Badge>
            </div>
            <div className="tnum mt-2 text-3xl leading-none font-semibold tracking-tight">
              {unpriced}
              <span className="ml-1 align-baseline text-xs font-normal text-muted-foreground">台</span>
            </div>
            <div className="mt-0.5 text-xs text-muted-foreground">未填价格{norate > 0 ? ` / 缺汇率 ${norate}` : ""}，未计入合计</div>
          </div>
        </div>

        {/* 与上下两块各留出更多空间：这一段与 KPI、与柱图挤在一起时会像同一块内容。 */}
        {byGroup.size > 1 && (
          <div className="space-y-1.5 pt-3 pb-2">
            <div className="text-xs text-muted-foreground">当月分组间开销比例</div>
            <div className="flex h-2 overflow-hidden rounded-full bg-muted">
              {[...byGroup.entries()].map(([g, v], i) => (
                <div
                  key={g}
                  className={g === "其他" ? "bg-muted-foreground/40" : tones[i % tones.length]}
                  style={{ width: `${(v / monthly) * 100}%` }}
                  title={g === "其他" ? `其他：${cny(v)}（${ranked3.length - 3} 个分组）` : `${g}：${cny(v)}`}
                />
              ))}
            </div>
            <div className="flex flex-wrap gap-x-3 gap-y-0.5 text-xs text-muted-foreground">
              {[...byGroup.entries()].map(([g, v], i) => (
                <span key={g} className="flex items-center gap-1" title={g === "其他" ? ranked3.slice(3).map(([n2, v2]) => `${n2} ${cny(v2)}`).join(" · ") : undefined}>
                  <span aria-hidden className={`size-2 rounded-[2px] ${g === "其他" ? "bg-muted-foreground/40" : tones[i % tones.length]}`} />
                  {g} <span className="font-mono tabular-nums text-foreground">{cny(v)}</span>
                </span>
              ))}
            </div>
          </div>
        )}

        </CardContent>
      </Card>
      <Card>
        <CardHeader className="gap-0 border-b border-border/60 pt-0.5 pb-2!">
          <CardTitle className="text-sm">
            未来 12 个月摊平预测{" "}
            <span className="text-xs font-normal text-muted-foreground">
              （按当前价格；到期的机器落在到期那个月）
            </span>
          </CardTitle>
        </CardHeader>
        <CardContent className="p-0">
        <div className="overflow-hidden">
          <div className="flex items-end gap-1 px-3 pt-3">
            {months.map((m) => (
              <button
                key={m.key}
                type="button"
                onMouseEnter={() => setPick(m.key)}
                onFocus={() => setPick(m.key)}
                onClick={() => setPick(m.key === pick ? null : m.key)}
                className="flex flex-1 flex-col items-center rounded-sm focus:outline-none focus-visible:ring-2 focus-visible:ring-primary"
                aria-label={`${m.label}：${cny(m.total)}`}
              >
                <div className="relative flex h-[120px] w-full items-end justify-center">
                  {/* 背景槽：本月的"容量"参考线，空月份也能看出这里有个槽位 */}
                  <div aria-hidden className="absolute inset-x-0 bottom-0 mx-auto h-full w-full max-w-[28px] rounded-t-sm bg-muted/60" />
                  <div
                    className={`relative w-full max-w-[28px] rounded-t-sm transition-colors ${pick === m.key ? "bg-primary" : "bg-primary/60 hover:bg-primary/80"}`}
                    style={{ height: `${Math.max(m.total > 0 ? 2 : 0, (m.total / peak) * 100)}%` }}
                  />
                </div>
                <div className={`mt-1.5 text-[11px] ${pick === m.key ? "text-foreground" : "text-muted-foreground"}`}>{m.label}</div>
              </button>
            ))}
          </div>
          <div className="mt-3 flex items-start gap-2 border-t border-border/60 bg-muted/20 px-3 py-2 text-xs text-muted-foreground">
            <CircleAlert className="mt-0.5 size-3.5 shrink-0" />
            {picked ? (
              <div className="min-w-0 flex-1">
                <div className="flex items-baseline justify-between gap-3">
                  <span className="text-foreground">{picked.label}</span>
                  <span className="font-mono tabular-nums text-foreground">预计本月 {cny(picked.total)}</span>
                </div>
                <div className="mt-1 flex flex-wrap gap-x-4 gap-y-0.5">
                  {picked.rows.length === 0 && <span>该月份暂无摊销成本。</span>}
                  {picked.rows.map((r) => (
                    <span key={r.n.id}>{r.n.name} <span className="font-mono tabular-nums text-foreground">{cny(r.monthly)}</span></span>
                  ))}
                </div>
              </div>
            ) : (
              <span>把鼠标移到某个月（或点它）看这个月的逐台明细。</span>
            )}
          </div>
        </div>

      </CardContent>
      </Card>
      <Card>
        <CardHeader className="gap-0 border-b border-border/60 pt-0.5 pb-2!">
          <CardTitle className="text-sm">服务器成本总览</CardTitle>
        </CardHeader>
        <CardContent className="space-y-3 pt-4">
        {/* ③ 工具条：等高胶囊 + 带放大镜的搜索框。 */}
        <div className="flex flex-wrap items-center gap-2">
          {/* 分段控件**复用本页已有的 `TRACK` / `pill`**：过滤条与页面上其它分段控件同一套语汇，
              不另起一套（那会与上面的资源卡颜色风格分家）。 */}
          <div className={`${TRACK} flex-wrap`}>
            {([
              ["priced", `有价格 ${priced.length}`],
              ["noprice", `未填价格 ${rows.filter((r) => r.why === "none").length}`],
              ["norate", `缺汇率 ${norate}`],
              ["all", `全部 ${rows.length}`],
            ] as const).map(([k, label]) => (
              <button key={k} type="button" onClick={() => setOnly(k)} className={`h-7 rounded-full px-2.5 text-xs ${pill(only === k)}`}>
                {label}
              </button>
            ))}
          </div>
          <span className="ml-auto text-xs text-muted-foreground">
            {filtered.length === rows.length ? `共 ${rows.length} 台` : `筛出 ${filtered.length} / ${rows.length} 台`}
          </span>
          <div className="relative">
            <Search className="pointer-events-none absolute left-2 top-1/2 size-3.5 -translate-y-1/2 text-muted-foreground" />
            <Input
              value={q}
              onChange={(e) => setQ(e.target.value)}
              placeholder="搜索节点或分组"
              className="h-8 w-44 pl-7 text-xs"
            />
          </div>
        </div>

        {/* ④ 明细：标准 Table。 */}
        <div className="rounded-lg border border-border/60">
          <div className="px-3">{table(top)}</div>
          {filtered.length > top.length && (
            <button
              type="button"
              onClick={() => setAllOpen(true)}
              className="w-full border-t border-border/60 py-2 text-xs text-muted-foreground transition-colors hover:bg-muted/40 hover:text-foreground"
            >
              还有 {filtered.length - top.length} 台 —— 点开看全部
            </button>
          )}
        </div>

        <Dialog open={allOpen} onOpenChange={setAllOpen}>
          <DialogContent className="max-h-[80vh] max-w-3xl overflow-y-auto">
            <DialogHeader>
              <DialogTitle className="text-sm">节点成本（{filtered.length} 台）</DialogTitle>
            </DialogHeader>
            <div className="rounded-lg border border-border/60 px-3">{table(bySort)}</div>
          </DialogContent>
        </Dialog>
      </CardContent>
      </Card>
    </div>
  )
}
