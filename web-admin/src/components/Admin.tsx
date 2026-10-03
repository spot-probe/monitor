import { useCallback, useEffect, useRef, useState } from "react"
import { flushSync } from "react-dom"
import { ArrowUpCircle, Bell, CalendarClock, ChevronRight, CircleAlert, CircleCheck, CircleQuestionMark, Copy, Database, Download, GripVertical, Palette, Pencil, Plus, Radio, RefreshCw, Send, Server, Settings, Shield, SlidersHorizontal, TestTube2, Trash2, Upload, Webhook } from "lucide-react"
import { Gauge, Timer } from "lucide-react"
import { toast } from "sonner"

import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card } from "@/components/ui/card"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Skeleton } from "@/components/ui/skeleton"
import { PageSkeleton } from "@/components/ui/page-skeleton"
import { EmptyState } from "@/components/ui/empty-state"
import { ICMP_AGENT_FLOOR, versionBelow } from "@/lib/format"
import { RetryState } from "@/components/ui/retry-state"
import { Switch } from "@/components/ui/switch"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip"
import { addresses, api, behind, changes, configFields, configForm, configOverrides, configSections, configValues, fits, GIB, matchingGroups, provisioningSite, shortAddress, trafficCorrection, upload, type ConfigField, type Node, type PingError, type PingTask, type Source } from "@/lib/api"
import { bytes, cycleFields, cycleOk, cyclePatch, FOREVER, money, monthUsage, uptime, type CycleUnit } from "@/lib/format"

// Counters the panel can correct after migration or an accounting error.
const TRAFFIC_FIELDS = [
  ["total_rx", "累计下行"],
  ["total_tx", "累计上行"],
  ["month_rx", "本月下行"],
  ["month_tx", "本月上行"],
] as const
const TRAFFIC_MODES: Record<string, string> = {
  sum: "上下行相加",
  max: "取较大值",
  up: "仅上行",
  down: "仅下行",
}

// Reordering uses the browser's view transitions, so displaced rows slide.
// Browsers without support jump instead.
function animate(update: () => void) {
  if (document.startViewTransition) document.startViewTransition(() => flushSync(update))
  else update()
}

function copy(text: string) {
  // The async clipboard API exists only in a secure context, and a hub reached
  // over plain http on a LAN address is not one -- which is a supported way to
  // run this (the panel says so when it refuses to add a node). A copy button
  // that silently does nothing there is worse than the old API, so this falls
  // back to it.
  const legacy = () => {
    const box = document.createElement("textarea")
    box.value = text
    box.setAttribute("readonly", "")
    // Parked off-screen rather than hidden: a hidden element cannot be selected,
    // and selecting is what `execCommand` copies.
    box.style.position = "fixed"
    box.style.top = "-1000px"
    document.body.appendChild(box)
    box.select()
    let ok = false
    try {
      ok = document.execCommand("copy")
    } catch {
      ok = false
    }
    box.remove()
    if (ok) toast.success("已复制")
    else toast.error("复制失败")
  }
  if (navigator.clipboard?.writeText) {
    navigator.clipboard.writeText(text).then(() => toast.success("已复制"), legacy)
  } else {
    legacy()
  }
}

const SOURCES: Record<Source, string> = {
  manual: "手动填写",
  interface: "网卡地址",
  exit: "hub 看到的出口，不在节点网卡上（NAT 或代理）",
  connection: "hub 看到的连接地址",
}

// The address a node is reached by, one per family, each click-to-copy: pasting
// one into an ssh command is why they are shown. Where each came from is in the
// tooltip, keeping the column to addresses alone.
function Addresses({ node }: { node: Node }) {
  const list = addresses(node)
  if (!list.length) return <span className="text-sm text-muted-foreground">—</span>
  return (
    <div className="flex flex-col items-start gap-y-0.5">
      {list.map(({ address, source }) => {
        // A full IPv6 runs to 39 characters and would set the width of this
        // column; the tooltip and the clipboard still carry the whole thing.
        const shown = shortAddress(address)
        return (
          <button
            key={address}
            type="button"
            onClick={() => copy(address)}
            title={shown === address ? `${SOURCES[source]}。点击复制` : `${SOURCES[source]}：${address}。点击复制`}
            className="tnum group inline-flex items-center gap-1 text-sm hover:text-foreground"
          >
            {shown}
            <Copy className="size-3 shrink-0 opacity-0 transition-opacity group-hover:opacity-100" />
          </button>
        )
      })}
    </div>
  )
}

/**
 * The explanation a label keeps out of the way until it is asked for.
 *
 * A tap shows no tooltip on its own, so a click opens it as well; and both the
 * trigger's own handlers and the label's would close it again -- the button sits
 * inside a `<label>` that focuses the control it wraps -- so both are prevented.
 */
function Help({ children, width = "max-w-64" }: { children: React.ReactNode; width?: string }) {
  const [open, setOpen] = useState(false)
  return (
    <Tooltip open={open} onOpenChange={setOpen}>
      <TooltipTrigger asChild>
        <button
          type="button"
          aria-label="说明"
          className="text-muted-foreground hover:text-foreground"
          onPointerDown={(e) => e.preventDefault()}
          onClick={(e) => {
            e.preventDefault()
            setOpen(true)
          }}
        >
          <CircleQuestionMark className="size-3.5" />
        </button>
      </TooltipTrigger>
      {/* text-wrap rather than the content's own text-balance, which breaks
          multi-line Chinese halfway across the box. */}
      <TooltipContent collisionPadding={16} className={`${width} space-y-1 text-left text-wrap`}>
        {children}
      </TooltipContent>
    </Tooltip>
  )
}

function Field({ label, hint, help, suffix, className = "", row = false, icon, children }: { label: string; hint?: string; help?: React.ReactNode; suffix?: string; className?: string; row?: boolean; icon?: React.ReactNode; children: React.ReactNode }) {
  // row：一项压成一行 —— 标签与控件在左，说明在右。竖排时三行说明把卡片撑得很高，
  // 而它们本来就短；横排后一眼能扫完。单位仍是输入框内部的 suffix，与竖排同一套写法。
  // row：一项一行，且三列真的对齐（标签 / 控件 / 说明）——每行各自 flex 时列会参差。
  // 关联改用 htmlFor + useId：标签不再是控件的父节点，但屏幕阅读器与点击仍然对得上。
  // 外层不套小卡片：背景与圆角叠在一起会让整块发闷，分组交给分割线。
  // row：一项一行。控件仍然**放在 Label 里面**（关联靠这个，不靠 id：两者只做兄弟时
  // 读屏念不出输入框的名字，这一条 27 个输入框共用，不能破）。列之所以能跨行对齐，是因为
  // 每行用的是同一个固定模板 grid，不是各自撑开的 flex。外层也不套小卡片。
  if (row) {
  	return (
  		<Label className="grid items-center gap-x-4 gap-y-1 md:grid-cols-[11rem_7rem_minmax(0,1fr)]">
  			<span className="flex items-center gap-2 text-sm font-medium">
  				{icon}
  				{label}
  				{help ? <Help>{help}</Help> : null}
  			</span>
  			{suffix ? (
  				<span className="relative">
  					{children}
  					<span className="pointer-events-none absolute inset-y-0 right-3 flex items-center text-xs text-muted-foreground">{suffix}</span>
  				</span>
  			) : (
  				children
  			)}
  			{hint ? <span className="text-xs text-muted-foreground">{hint}</span> : null}
  		</Label>
  	)
  }

  return (
    <div className={`space-y-2 ${className}`}>
      {/* The control goes inside the label, which is what associates the two. As
          siblings they were merely adjacent: a screen reader announced the input with
          no name, and clicking the label did not focus it. 27 inputs share this. */}
      <Label className="flex flex-col items-start gap-2 text-sm font-medium">
        {/* The question mark sits inside the label as well, which is why it has to
            cancel the label's own activation: see `Help`. */}
        <span className="flex items-center gap-1.5">
          {label}
          {help ? <Help>{help}</Help> : null}
        </span>
      {/* The unit sits inside the control rather than in the label: "离线宽限期（分钟）"
          made the label do two jobs, and the reader had to parse past the parenthesis to
          find the field's name. */}
      {suffix ? (
        <div className="relative">
          {children}
          <span className="pointer-events-none absolute inset-y-0 right-3 flex items-center text-xs text-muted-foreground">{suffix}</span>
        </div>
      ) : (
        children
      )}
      </Label>
      {hint && <p className="text-xs leading-relaxed text-muted-foreground">{hint}</p>}
    </div>
  )
}

/** A titled block of the page. Without one, two unrelated groups of settings read as a
 *  single long form. */
function Section({ title, hint, action, children }: { title: string; hint?: string; action?: React.ReactNode; children: React.ReactNode }) {
  return (
    <section className="space-y-3">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0 flex-1">
          <h2 className="text-base font-semibold tracking-tight">{title}</h2>
          {hint && <p className="mt-1 text-xs leading-relaxed text-muted-foreground">{hint}</p>}
        </div>
        {action}
      </div>
      {children}
    </section>
  )
}


function ConfirmDialog({ title, description, confirmLabel, busy = false, tone = "danger", onClose, onConfirm, children }: {
  title: string
  description: string
  confirmLabel: string
  busy?: boolean
  /** `danger` (the default) paints the confirm button red. Not every confirmation
   *  is a deletion: reclaiming space is maintenance, and a red button there teaches
   *  the reader that red does not mean anything in particular. */
  tone?: "danger" | "default"
  onClose: () => void
  onConfirm: () => void
  children?: React.ReactNode
}) {
  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>{title}</DialogTitle>
          <DialogDescription className="leading-relaxed">{description}</DialogDescription>
        </DialogHeader>
        {children}
        <DialogFooter className="border-t pt-4">
          <Button variant="ghost" onClick={onClose}>取消</Button>
          <Button variant={tone === "danger" ? "destructive" : "default"} onClick={onConfirm} disabled={busy}>{confirmLabel}</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

function CreateNode({ onClose, onSaved }: {
  onClose: () => void
  onSaved: () => void
}) {
  const [name, setName] = useState("")
  const [saving, setSaving] = useState(false)

  async function save(e: React.FormEvent) {
    e.preventDefault()
    if (!name.trim()) return toast.error("请填写节点名称")
    setSaving(true)
    try {
      await api("/nodes", {
        method: "POST",
        body: JSON.stringify({ name: name.trim() }),
      })
      toast.success("节点已添加")
      onClose()
      onSaved()
    } catch (e) {
      toast.error((e as Error).message)
    } finally {
      setSaving(false)
    }
  }

  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>添加节点</DialogTitle>
        </DialogHeader>
        <form className="space-y-4" onSubmit={save}>
          <Field label="名称">
            <Input autoFocus value={name} onChange={(e) => setName(e.target.value)} placeholder="香港 · 甲商家" />
          </Field>
          <DialogFooter className="border-t pt-4">
            <Button type="button" variant="ghost" onClick={onClose}>取消</Button>
            <Button type="submit" disabled={saving}>添加</Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}

/**
 * The group field with the names already in use listed under it.
 *
 * Only drawn when the hub was started with `--group-dropdown`: the field is also
 * how a new group is made, so the list is an opt-in help for the case where there
 * are many names to remember. Typing works exactly as it did -- this is the same
 * input, with a list beside it.
 *
 * The list is drawn in place rather than in a popover: a popover takes focus when
 * it opens, and a field that cannot be typed into is worse than no list at all.
 */
function GroupPicker({ value, onChange, groups }: { value: string; onChange: (v: string) => void; groups: string[] }) {
  const [open, setOpen] = useState(false)
  // Which entry the arrow keys are on, or none: Enter takes it only after the
  // admin has chosen one, so it keeps saving the form as before otherwise.
  const [active, setActive] = useState<number | null>(null)
  const picks = matchingGroups(groups, value)
  const list = open && picks.length > 0
  const move = (step: number) =>
    setActive((a) => (a === null ? (step > 0 ? 0 : picks.length - 1) : (a + step + picks.length) % picks.length))
  return (
    <div className="relative">
      <Input
        maxLength={13}
        value={value}
        placeholder="建站"
        onChange={(e) => {
          onChange(e.target.value)
          setOpen(true)
          setActive(null)
        }}
        onFocus={() => setOpen(true)}
        // A blur fires before the click below lands, so the list would be gone by
        // the time it arrives; one tick is enough for the click to be delivered.
        onBlur={() => setTimeout(() => setOpen(false), 150)}
        onKeyDown={(e) => {
          if (!list) return
          if (e.key === "ArrowDown" || e.key === "ArrowUp") {
            e.preventDefault()
            move(e.key === "ArrowDown" ? 1 : -1)
          } else if (e.key === "Enter" && active !== null) {
            e.preventDefault()
            onChange(picks[active])
            setOpen(false)
          } else if (e.key === "Escape") {
            setOpen(false)
          }
        }}
      />
      {list && (
        <div
          role="listbox"
          className="absolute z-50 mt-1 max-h-56 w-full overflow-y-auto rounded-md border bg-popover p-1 shadow-md"
        >
          {picks.map((g, i) => (
            <button
              key={g}
              type="button"
              role="option"
              aria-selected={g === value}
              className={`flex w-full items-center rounded-sm px-2 py-1.5 text-left text-sm ${
                i === active ? "bg-accent text-accent-foreground" : "hover:bg-accent hover:text-accent-foreground"
              }`}
              // Keeps the field focused, so the click lands on a list that is
              // still open.
              onMouseDown={(e) => e.preventDefault()}
              onClick={() => {
                onChange(g)
                setOpen(false)
              }}
            >
              {g}
            </button>
          ))}
        </div>
      )}
    </div>
  )
}

function NodeForm({ node, groups, groupDropdown, onClose, onSaved }: {
  node: Node
  /** The group names already in use, for the opt-in list below. */
  groups: string[]
  groupDropdown: boolean
  onClose: () => void
  onSaved: () => void
}) {
  const [form, setForm] = useState(node)
  const [limitGib, setLimitGib] = useState(String(node.traffic_limit / GIB || ""))
  const [saving, setSaving] = useState(false)
  const gib = (bytes: number) => String(Number((bytes / GIB).toFixed(3)))
  const [traffic, setTraffic] = useState(() =>
    Object.fromEntries(TRAFFIC_FIELDS.map(([k]) => [k, gib(node[k])])) as Record<string, string>,
  )
  // Compared as entered rather than as bytes: rounding to GB would read as an
  // edit and zero a node that has transferred a few MB.
  const pristine = useRef(traffic)
  const set = <K extends keyof Node>(k: K, v: Node[K]) => setForm((f) => ({ ...f, [k]: v }))
  // What each address box falls back to when left empty.
  const automatic = (v6: boolean) =>
    addresses({ ...node, ipv4_pin: "", ipv6_pin: "" }).find((a) => a.address.includes(":") === v6)?.address ?? "无"

  async function save() {
    if (!form.name.trim()) return toast.error("请填写节点名称")
    const patch = changes(node, {
      name: form.name.trim(),
      group: form.group.trim(),
      public: form.public,
      remark: form.remark,
      traffic_mode: form.traffic_mode,
      traffic_limit: Math.round(Number(limitGib) * GIB),
      traffic_reset_day: Math.min(31, Math.max(1, Math.round(Number(form.traffic_reset_day) || 1))),
      notify: !!form.notify,
      ipv4_pin: (form.ipv4_pin ?? "").trim(),
      ipv6_pin: (form.ipv6_pin ?? "").trim(),
      country_pin: (form.country_pin ?? "").trim().toUpperCase(),
    })
    const correction = trafficCorrection(pristine.current, traffic)
    if ([patch.traffic_limit, ...Object.values(correction)].some((v) => v !== undefined && (!Number.isSafeInteger(v) || v < 0))) {
      return toast.error("流量必须是有效的非负数，且不能超出精确计数范围")
    }
    setSaving(true)
    try {
      // The correction belongs to the new reset period, so its day is saved
      // first.
      if (Object.keys(patch).length) {
        await api(`/nodes/${node.id}`, { method: "PUT", body: JSON.stringify(patch) })
      }
      if (Object.keys(correction).length) {
        await api(`/nodes/${node.id}/traffic`, {
          method: "PUT",
          body: JSON.stringify(correction),
        })
      }
      toast.success("节点设置已保存")
      onClose()
      onSaved()
    } catch (e) {
      toast.error((e as Error).message)
    } finally {
      setSaving(false)
    }
  }

  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent onOpenAutoFocus={(e) => e.preventDefault()} className="sm:max-w-xl">
        <DialogHeader>
          <DialogTitle>{node.name}</DialogTitle>
        </DialogHeader>
        <div className="space-y-5">
          <Field label="名称">
            <Input value={form.name} onChange={(e) => set("name", e.target.value)} />
          </Field>
          {/* Filed beside the name rather than with the billing fields: this is how
              the node is grouped, not what it costs. The public page derives its tabs
              from the values in use, so there is nothing to pick from -- only to type,
              unless the hub was started with --group-dropdown and the names are worth
              offering. Empty means the node appears under every tab. */}
          <Field label="分组" hint="公开页按它分页签，例如「建站」「入口集群」。最多 13 字。留空则只在「全部节点」下出现">
            {groupDropdown ? (
              <GroupPicker value={form.group} onChange={(v) => set("group", v)} groups={groups} />
            ) : (
              <Input maxLength={13} value={form.group} onChange={(e) => set("group", e.target.value)} placeholder="建站" />
            )}
          </Field>
          <div className="grid gap-4 sm:grid-cols-2">
            <Field label="每月流量额度 (GB)" hint="留空或 0 不限">
              <Input type="number" value={limitGib} onChange={(e) => setLimitGib(e.target.value)} placeholder="1024" />
            </Field>
            <Field label="流量计算方式">
              <Select value={form.traffic_mode} onValueChange={(v) => set("traffic_mode", v)}>
                <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
                <SelectContent>
                  {Object.entries(TRAFFIC_MODES).map(([k, v]) => (
                    <SelectItem key={k} value={k}>{v}</SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </Field>
          </div>
          <div className="grid gap-4 sm:grid-cols-2">
            <Field label="每月重置日" hint="1–31。本月流量按新周期重算，总流量不变">
              <Input type="number" min={1} max={31} value={form.traffic_reset_day} onChange={(e) => set("traffic_reset_day", Number(e.target.value))} />
            </Field>
            <Field label="备注" hint="仅管理员可见；填了会显示在该节点的详情页（公开页只有你登录后才看得见）">
              <Input value={form.remark ?? ""} onChange={(e) => set("remark", e.target.value)} placeholder="商家、用途" />
            </Field>
          </div>
          <details className="rounded-lg border bg-muted/30 px-3 py-2.5">
            <summary className="cursor-pointer text-sm font-medium">流量校正</summary>
            <p className="mt-2 text-xs leading-relaxed text-muted-foreground">
              按 GB 填入需要校正的值，未修改的计数器继续正常累计。
            </p>
            <div className="mt-3 grid gap-4 sm:grid-cols-2">
              {TRAFFIC_FIELDS.map(([key, label]) => (
                <Field key={key} label={`${label} (GB)`}>
                  <Input
                    type="number"
                    step="0.001"
                    value={traffic[key]}
                    onChange={(e) => setTraffic((t) => ({ ...t, [key]: e.target.value }))}
                  />
                </Field>
              ))}
            </div>
          </details>
          <label className="flex cursor-pointer items-center justify-between gap-4 rounded-lg border bg-muted/30 px-3 py-2.5 text-sm">
            <span>
              <span className="block font-medium">公开显示</span>
              <span className="mt-0.5 block text-xs text-muted-foreground">关闭后只在管理后台可见</span>
            </span>
            <Switch checked={form.public} onCheckedChange={(v) => set("public", v)} />
          </label>
          <label className="flex cursor-pointer items-center justify-between gap-4 rounded-lg border bg-muted/30 px-3 py-2.5 text-sm">
            <span>
              <span className="block font-medium">离线通知</span>
              <span className="mt-0.5 block text-xs text-muted-foreground">掉线超过宽限期推送一条，恢复在线时再推一条</span>
            </span>
            <Switch checked={!!form.notify} onCheckedChange={(v) => set("notify", v)} />
          </label>
          {/* Held apart from the fields above: these replace what the agent and the
              hub worked out for themselves, and every one of them is empty by
              default. The placeholder carries the automatic value, so a box left
              alone shows what is in use. */}
          <div className="space-y-3 border-t pt-5">
            <h3 className="text-sm font-medium">地址与地区</h3>
            <div className="grid gap-4 sm:grid-cols-[1fr_1.4fr_6rem]">
              <Field label="IPv4">
                <Input value={form.ipv4_pin ?? ""} onChange={(e) => set("ipv4_pin", e.target.value)} placeholder={`自动：${automatic(false)}`} />
              </Field>
              <Field label="IPv6">
                <Input value={form.ipv6_pin ?? ""} onChange={(e) => set("ipv6_pin", e.target.value)} placeholder={`自动：${automatic(true)}`} />
              </Field>
              <Field label="国家/地区">
                <Input
                  value={form.country_pin ?? ""}
                  maxLength={2}
                  onChange={(e) => set("country_pin", e.target.value.toUpperCase())}
                  placeholder={`自动：${node.country_auto || "无"}`}
                />
              </Field>
            </div>
            <p className="text-xs leading-relaxed text-muted-foreground">
              留空为自动。国家/地区填两位代码，如 CN；手填的值会一直显示，IP 变了要自己改。
            </p>
          </div>
        </div>
        <DialogFooter>
          <Button variant="ghost" onClick={onClose}>取消</Button>
          <Button onClick={save} disabled={saving}>保存</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

const CURRENCY_NAMES = new Intl.DisplayNames(["zh-CN"], { type: "currency" })

// The name confirms a code the hub can only check the shape of. DisplayNames
// echoes back a code outside ISO 4217 and throws on anything but three letters,
// which the hub refuses with its own message.
function currencyHint(code: string) {
  try {
    const name = CURRENCY_NAMES.of(code)
    return name === code ? "未知代码，照原样显示" : name
  } catch {
    return undefined
  }
}

function BillingForm({ node, onClose, onSaved }: {
  node: Node
  onClose: () => void
  onSaved: () => void
}) {
  const [form, setForm] = useState(node)
  // Text rather than a number: a numeric state cannot represent an empty field,
  // so clearing it would snap back to 0 mid-entry. Empty means free.
  const [price, setPrice] = useState(node.price > 0 ? String(node.price) : "")
  // Whole years are entered in years, the way a five-year plan is sold. A stored
  // length this panel cannot read -- an older hub accepted any string, so a
  // database may hold `weekly` or `2y` -- gets controls of its own, and the value
  // is passed back untouched unless the admin actually moves them: turning it
  // into `yearly` behind their back would be a silent edit of their billing.
  const stored = cycleFields(node.billing_cycle)
  const [unit, setUnit] = useState(stored.unit)
  const [count, setCount] = useState(stored.count)
  const [saving, setSaving] = useState(false)
  const set = <K extends keyof Node>(k: K, v: Node[K]) => setForm((f) => ({ ...f, [k]: v }))

  async function save() {
    // The cycle is sent only when one of its two controls was touched -- an
    // untouched pair means "leave the stored length alone", which is also what
    // keeps a stored length this panel cannot read from being rewritten.
    const touched = unit !== stored.unit || count !== stored.count
    if (touched && !cycleOk(unit, count)) {
      return toast.error("付款周期要在 1 个月到 100 年之间，填整数")
    }
    setSaving(true)
    try {
      await api(`/nodes/${node.id}`, {
        method: "PUT",
        body: JSON.stringify(changes(node, {
          price: Math.max(0, Number(price) || 0),
          currency: form.currency,
          billing_cycle: cyclePatch(node.billing_cycle, stored, { unit, count }),
          expires_at: form.expires_at || null,
        })),
      })
      toast.success("续费设置已保存")
      onClose()
      onSaved()
    } catch (e) {
      toast.error((e as Error).message)
    } finally {
      setSaving(false)
    }
  }

  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent onOpenAutoFocus={(e) => e.preventDefault()} className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>{node.name}</DialogTitle>
        </DialogHeader>
        <div className="space-y-5">
          <div className="grid gap-4 sm:grid-cols-2">
            <Field label="价格" hint="留空或 0 为免费">
              <Input
                type="number"
                min="0"
                step="0.01"
                value={price}
                onChange={(e) => setPrice(e.target.value)}
                placeholder="免费"
              />
            </Field>
            <Field
              label="货币"
              hint={currencyHint(form.currency.toUpperCase())}
              help={
                <>
                  <p>填三个字母的货币代码，大小写都行。</p>
                  <p>
                    例如：
                    {["美元 USD", "人民币 CNY", "港币 HKD", "新台币 TWD", "欧元 EUR", "日元 JPY"].map((c, i) => (
                      <span key={c}>
                        {i > 0 && "、"}
                        <span className="whitespace-nowrap">{c}</span>
                      </span>
                    ))}
                  </p>
                </>
              }
            >
              {/* Uppercased by CSS: rewriting the value mid-composition would
                  break an input method, and the hub stores it uppercased. */}
              <Input
                className="uppercase"
                maxLength={3}
                autoCapitalize="characters"
                autoCorrect="off"
                spellCheck={false}
                value={form.currency}
                onChange={(e) => set("currency", e.target.value)}
                placeholder="USD"
              />
            </Field>
          </div>
          <div className="grid gap-4 sm:grid-cols-2">
            <Field
              label="付款周期"
              hint={
                stored.readable
                  ? "1 个月到 100 年，或一次性"
                  : `现在存的是「${node.billing_cycle || "空"}」，不动这里就保持原样`
              }
            >
              <div className="flex items-center gap-2">
                <Input
                  type="number"
                  min={1}
                  // Whole months only: a fractional one would be stored as
                  // `1.5m`, which the hub refuses as "要是整月".
                  step={1}
                  aria-label="周期长度"
                  className="flex-1"
                  value={unit === "once" ? "" : count}
                  disabled={unit === "once"}
                  onChange={(e) => setCount(e.target.value)}
                />
                {/* The three options below are exactly `CycleUnit`. */}
                <Select value={unit} onValueChange={(v) => setUnit(v as CycleUnit)}>
                  <SelectTrigger className="w-28 shrink-0"><SelectValue /></SelectTrigger>
                  <SelectContent>
                    <SelectItem value="months">个月</SelectItem>
                    <SelectItem value="years">年</SelectItem>
                    <SelectItem value="once">一次性</SelectItem>
                  </SelectContent>
                </Select>
              </div>
            </Field>
            <Field label="到期时间">
              <Input type="date" value={form.expires_at ?? ""} onChange={(e) => set("expires_at", e.target.value)} />
            </Field>
          </div>
        </div>
        <DialogFooter>
          <Button variant="ghost" onClick={onClose}>取消</Button>
          <Button onClick={save} disabled={saving}>保存</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

// Built here rather than fetched: the node list already carries the token, so
// viewing an install command is a read rather than an action. Reissuing one to
// display it would take the running agent offline.
function installCommand(site: string, token: string, seconds: number) {
  site = provisioningSite(site)
  if (!site) return ""
  const args = [`--server ${site}`, `--token ${token}`, `--interval ${seconds}`]
  return `curl -fsSL ${site}/install.sh | sh -s -- ${args.join(" ")}`
}

// One command for a batch of machines. The key belongs to the hub, is valid only
// within the window it opened, and each machine exchanges it for a token of its
// own, so unlike an install command this text is no one's credential and can be
// used directly in a loop.
function registerCommand(site: string, key: string) {
  site = provisioningSite(site)
  if (!site) return ""
  const args = [`--server ${site}`, `--register ${key}`]
  return `curl -fsSL ${site}/install.sh | sh -s -- ${args.join(" ")}`
}

// Also the same for every node, and carrying no credential: install.sh reads the
// token and the hub address from the machine's own env file, so one command can
// be sent to a whole fleet.
function upgradeCommand(site: string) {
  site = provisioningSite(site)
  if (!site) return ""
  return `curl -fsSL ${site}/install.sh | sh -s -- --upgrade`
}

// Carries no token, so it is the same for every node and remains valid after the
// node is deleted.
function uninstallCommand(site: string) {
  site = provisioningSite(site)
  if (!site) return ""
  return `curl -fsSL ${site}/install.sh | sh -s -- --uninstall`
}

export type Versions = { hub: string; hub_latest: string; agent_latest: string; notice: boolean }

/**
 * What is running and what is published. Read once per panel load: the hub holds
 * the answer for six hours, so this costs a GitHub lookup a few times a day at
 * most, and nothing at all while nobody opens the panel.
 *
 * A hub that cannot reach github.com answers with empty latest fields, which
 * render as no update rather than as an error.
 *
 * A failed attempt, or one that came back with no latest at all, is retried
 * twice. Reading it exactly once used to leave the panel showing 查不到版本 --
 * and the navigation without its dot -- for the rest of the page's life if that
 * one request landed in the wrong second, as the hub's own first check does when
 * a browser reaches a hub that has just started. Once the hub has an answer it
 * serves it from a six-hour cache, so a retry costs one request.
 *
 * It lives here and is passed down from `App` rather than read in both places:
 * the navigation's dot and the page are the same answer, and reading it twice
 * would make the panel ask twice.
 */
const VERSION_RETRY_MS = [1_500, 4_000]

export function useVersions() {
  const [versions, setVersions] = useState<Versions | null>(null)
  const load = useCallback(() => api<Versions>("/version").then(setVersions).catch(() => {}), [])
  useEffect(() => {
    let stop = false
    let timer: ReturnType<typeof setTimeout> | undefined
    const retry = (tries: number) => {
      if (stop || tries >= VERSION_RETRY_MS.length) return
      timer = setTimeout(() => attempt(tries + 1), VERSION_RETRY_MS[tries])
    }
    const attempt = (tries: number) => {
      api<Versions>("/version")
        .then((v) => {
          if (!stop) setVersions(v)
          // Empty latest is worth one more look when the hub's own check had not
          // landed yet -- a hub that has just started asks again on this call,
          // so the next attempt can find the answer. A hub that cannot reach
          // GitHub at all answers the same way every time: it caches the empty
          // result (ten minutes for the hub release, six hours for the agent),
          // so those retries are spent, not wasted -- two requests, bounded.
          if (!v.hub_latest && !v.agent_latest) retry(tries)
        })
        .catch(() => retry(tries))
    }
    attempt(0)
    return () => {
      stop = true
      clearTimeout(timer)
    }
  }, [])
  return { versions, reload: load }
}

/** Whether anything is published that this hub or its agents are not running. */
export function updatesAvailable(versions: Versions | null, nodes: Node[]): boolean {
  if (!versions?.notice) return false
  return behind(versions.hub, versions.hub_latest) || nodes.some((n) => n.agent_old)
}

// The window lives on the hub; this reads it back and counts down, which is also
// what makes an expired one disappear from the panel without interaction.
function useRegisterWindow() {
  const [key, setKey] = useState("")
  const [until, setUntil] = useState(0)
  const [now, setNow] = useState(() => Math.floor(Date.now() / 1000))

  useEffect(() => {
    api<Settings>("/settings")
      .then((s) => { setKey(String(s.register_key ?? "")); setUntil(Number(s.register_until ?? 0)) })
      .catch(() => {})
    const timer = setInterval(() => setNow(Math.floor(Date.now() / 1000)), 1000)
    return () => clearInterval(timer)
  }, [])

  return {
    key,
    left: key === "" ? 0 : Math.max(0, until - now),
    async open(minutes: number) {
      try {
        const w = await api<{ register_key: string; register_until: string }>("/register-window", {
          method: "POST",
          body: JSON.stringify({ minutes }),
        })
        setKey(w.register_key)
        setUntil(Number(w.register_until))
      } catch (e) {
        toast.error((e as Error).message)
      }
    },
    async close() {
      try {
        await api("/register-window", { method: "DELETE" })
        setKey("")
        setUntil(0)
        toast.success("注册窗口已关闭")
      } catch (e) {
        toast.error((e as Error).message)
      }
    },
  }
}

function RegisterDialog({ site, reg, onClose }: {
  site: string
  reg: ReturnType<typeof useRegisterWindow>
  onClose: () => void
}) {
  const [minutes, setMinutes] = useState(60)
  const command = reg.left > 0 ? registerCommand(site, reg.key) : ""
  const clock = `${Math.floor(reg.left / 60)}:${String(reg.left % 60).padStart(2, "0")}`
  const span = minutes === 60 ? "一小时" : `${minutes} 分钟`

  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent onOpenAutoFocus={(e) => e.preventDefault()} className="sm:max-w-xl">
        <DialogHeader>
          <DialogTitle>批量添加</DialogTitle>
        </DialogHeader>
        <div className="space-y-4">
          <p className="text-sm text-muted-foreground">
            开一个{span}的注册窗口。期间这条命令在任意机器上跑一次，那台机器就会自己出现在
            列表里，名字取自它的 hostname。命令里没有任何一台机器的凭证，可以直接进循环。
          </p>
          {command ? (
            <div className="space-y-2">
              <Label className="text-sm font-medium">安装命令</Label>
              <pre className="h-24 overflow-auto whitespace-pre-wrap break-all rounded-lg border bg-muted/40 p-3 text-xs leading-relaxed select-all">
                {command}
              </pre>
              <div className="flex items-center justify-between gap-4 rounded-lg border bg-muted/30 px-3 py-2.5 text-sm">
                <span>
                  <span className="block font-medium">窗口 {clock} 后自动关闭</span>
                  <span className="mt-0.5 block text-xs text-muted-foreground">
                    到点自动失效，装完了也可以现在就关
                  </span>
                </span>
                <Button variant="outline" size="sm" onClick={reg.close}>立即关闭</Button>
              </div>
            </div>
          ) : (
            // 时长预设而不是自由输入：常见选择一眼可选，也不必校验越界（hub 同样封顶，
            // 那里才是真正说了算的地方）。上限就是一小时。
            <div className="space-y-2">
              <Label className="text-sm font-medium">窗口时长</Label>
              <div className="flex flex-wrap gap-2">
                {[5, 10, 15, 30, 60].map((m) => (
                  <Button
                    key={m}
                    type="button"
                    size="sm"
                    variant={m === minutes ? "default" : "outline"}
                    onClick={() => setMinutes(m)}
                  >
                    {m === 60 ? "1 小时" : `${m} 分钟`}
                  </Button>
                ))}
              </div>
            </div>
          )}
        </div>
        {/* 两种状态各有一套动作：没开窗时主动作是「开启窗口」，开好了主动作是「复制」。
            原先「开启窗口」孤零零待在正文里，而页脚只有 关闭 / 复制，于是三个按钮分居两处。 */}
        <DialogFooter>
          <Button variant="ghost" onClick={onClose}>关闭</Button>
          {command ? (
            <Button onClick={() => copy(command)}>
              <Copy className="size-4" /> 复制
            </Button>
          ) : (
            <Button onClick={() => reg.open(minutes)}>开启窗口</Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

function InstallDialog({ node, site, onClose, onRotated }: {
  node: Node
  site: string
  onClose: () => void
  onRotated: () => void
}) {
  const [token, setToken] = useState(node.token ?? "")
  const [interval, setInterval] = useState("1")
  const [rotating, setRotating] = useState(false)
  const [confirmRotate, setConfirmRotate] = useState(false)

  const seconds = Math.min(3600, Math.max(1, Math.round(Number(interval) || 1)))
  const command = token ? installCommand(site, token, seconds) : ""

  async function rotate() {
    setRotating(true)
    try {
      const fresh = await api<{ token: string }>(`/nodes/${node.id}/token`, { method: "POST" })
      setToken(fresh.token)
      setConfirmRotate(false)
      toast.success("凭证已换发，需用新命令重装")
      onRotated()
    } catch (e) {
      toast.error((e as Error).message)
    } finally {
      setRotating(false)
    }
  }

  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent onOpenAutoFocus={(e) => e.preventDefault()} className="sm:max-w-xl">
        <DialogHeader>
          <DialogTitle>{node.name}</DialogTitle>
          {/* The one place a single node's agent version is shown, and what an
              issue report asks for. Empty until the node has reported once. */}
          {node.agent_version && <DialogDescription>当前 agent v{node.agent_version}</DialogDescription>}
        </DialogHeader>
        <div className="space-y-5">
          <Field label="上报间隔（秒）" hint="1–3600，默认 1 秒">
            <Input type="number" min={1} max={3600} value={interval} onChange={(e) => setInterval(e.target.value)} />
          </Field>
          <div className="space-y-2">
            <Label className="text-sm font-medium">安装命令</Label>
            <pre className="h-28 overflow-auto whitespace-pre-wrap break-all rounded-lg border bg-muted/40 p-3 text-xs leading-relaxed select-all">
              {/* A node added before the hub kept tokens has nothing to show
                  until one is reissued. */}
              {command || "旧版本创建的凭证不可读取，换发后显示"}
            </pre>
          </div>
          <div className="flex items-center justify-between gap-4 rounded-lg border bg-muted/30 px-3 py-2.5 text-sm">
            <span>
              <span className="block font-medium">换发凭证</span>
              <span className="mt-0.5 block text-xs text-muted-foreground">
                旧凭证立即作废，agent 掉线，需用新命令重装
              </span>
            </span>
            <Button variant="outline" size="sm" disabled={rotating} onClick={() => setConfirmRotate(true)}>
              换发
            </Button>
          </div>
        </div>
        <DialogFooter>
          <Button variant="ghost" onClick={onClose}>关闭</Button>
          <Button onClick={() => copy(command)} disabled={!command}>
            <Copy className="size-4" /> 复制
          </Button>
        </DialogFooter>
      </DialogContent>
      {confirmRotate && (
        <ConfirmDialog
          title={`给「${node.name}」换发凭证？`}
          description="旧凭证立即作废，agent 掉线，必须用新命令重装。仅在凭证可能泄露时使用。"
          confirmLabel="换发凭证"
          busy={rotating}
          onClose={() => setConfirmRotate(false)}
          onConfirm={rotate}
        />
      )}
    </Dialog>
  )
}

/// Whole days until a date, counted the way a person counts them: the day itself is 0,
/// yesterday is -1. Parsed as local midnight, matching how the panel's own date input
/// writes the value.
function daysUntil(date: string | null): number | null {
  if (!date) return null
  const at = new Date(`${date}T00:00:00`)
  return Number.isNaN(at.getTime()) ? null : Math.floor((at.getTime() - Date.now()) / 86_400_000)
}

/// Three states, and the point is telling them apart at a glance in a long table: an
/// ordinary date is secondary text, under a month turns amber, past it turns red. The
/// colours are the theme's darker twins -- warn-fg is 5.02:1 and danger-fg 4.83:1 --
/// because the amber and red fills do not carry as words on white.
function Expiry({ date }: { date: string | null }) {
  const days = daysUntil(date)
  if (days === null) return <span className="text-muted-foreground">{FOREVER}</span>
  if (days < 0) {
    return (
      <span className="inline-flex items-center gap-1.5">
        <span className="tnum text-danger-fg">{date}</span>
        <span className="rounded bg-destructive/10 px-1.5 py-0.5 text-[10px] leading-none text-danger-fg">
          已过期 {-days} 天
        </span>
      </span>
    )
  }
  if (days <= 30) {
    return (
      <span className="inline-flex items-center gap-1.5">
        <span className="tnum text-warn-fg">{date}</span>
        <span className="rounded bg-warn/15 px-1.5 py-0.5 text-[10px] leading-none text-warn-fg">
          {days === 0 ? "今天到期" : `${days} 天后到期`}
        </span>
      </span>
    )
  }
  return <span className="tnum text-muted-foreground">{date}</span>
}

/**
 * Ordering by drag or by ↑/↓, shared by the two tables that have one: the node
 * list and the probe list. Both send the complete order on drop, because the hub
 * refuses a list that does not name every row exactly once.
 *
 * `ids` is the order the hub returned. Rows are shown in the operator's own
 * arrangement, with anything the hub has that it does not mention appended, so a
 * tab left open across an insert cannot hide a row.
 */
function useDragOrder(ids: number[], url: string, refresh: () => void) {
  const [manual, setManual] = useState<number[]>([])
  const [dragging, setDragging] = useState<number | null>(null)
  const before = useRef<number[]>([])
  const order = [...manual.filter((id) => ids.includes(id)), ...ids.filter((id) => !manual.includes(id))]

  // Rows are displaced while the pointer is down; the order is saved on drop.
  function move(from: number, to: number) {
    if (from < 0 || to < 0 || to >= order.length || from === to) return
    const next = [...order]
    next.splice(to, 0, ...next.splice(from, 1))
    animate(() => setManual(next))
    return next
  }

  // Dropped outside the table or cancelled with Escape: the order is restored.
  function cancel() {
    setDragging(null)
    const rollback = before.current
    if (rollback.length) animate(() => setManual(rollback))
  }

  function save(next: number[]) {
    setDragging(null)
    const rollback = before.current
    if (!rollback.length || next.join() === rollback.join()) return
    before.current = next
    api(url, { method: "PUT", body: JSON.stringify({ ids: next }) }).then(refresh, (e: Error) => {
      setManual(rollback)
      toast.error(e.message)
    })
  }

  return { order, dragging, setDragging, before, move, cancel, save }
}

/** The grip. `disabled` while the list is filtered: a drop sends every row's id,
 *  and a filtered list only offers its own rows to drop onto. */
function DragHandle({ onStart, onEnd, onKey, disabled, title, label }: {
  onStart: (e: React.DragEvent) => void
  onEnd: (e: React.DragEvent) => void
  onKey: (e: React.KeyboardEvent) => void
  disabled: boolean
  title: string
  label: string
}) {
  return (
    <button
      type="button"
      draggable={!disabled}
      disabled={disabled}
      className="cursor-grab touch-none rounded p-1 text-muted-foreground hover:bg-muted hover:text-foreground active:cursor-grabbing disabled:cursor-default disabled:opacity-40 disabled:hover:bg-transparent"
      title={title}
      aria-label={label}
      onDragStart={onStart}
      onDragEnd={onEnd}
      onKeyDown={onKey}
    >
      <GripVertical className="size-4" />
    </button>
  )
}

function Nodes({ nodes, refresh, site, canProvision, provisionNote, groupDropdown, agentLatest }: {
  nodes: Node[]
  refresh: () => void
  site: string
  canProvision: boolean
  provisionNote: string
  /** `--group-dropdown`: offer the groups in use under the group field. */
  groupDropdown: boolean
  /** The newest agent release the hub has read, or null when it has not. */
  agentLatest: string | null
}) {
  const [creating, setCreating] = useState(false)
  const [editing, setEditing] = useState<Node | null>(null)
  const [billing, setBilling] = useState<Node | null>(null)
  const [installing, setInstalling] = useState<Node | null>(null)
  const [registering, setRegistering] = useState(false)
  const reg = useRegisterWindow()
  const [deleting, setDeleting] = useState<Node | null>(null)
  const [removing, setRemoving] = useState(false)
  const [query, setQuery] = useState("")
	// 只看 agent 需要升级的节点。与「未分组」那条筛选项同样的取舍：没有可筛的东西时
	// 不出现，否则工具栏上会多一个按下去什么也不改变的按钮。
	const [onlyOutdated, setOnlyOutdated] = useState(false)
  const [group, setGroup] = useState("all")
  const drag = useDragOrder(nodes.map((node) => node.id), "/nodes/order", refresh)
  const byId = new Map(nodes.map((node) => [node.id, node]))
  const order = drag.order.map((id) => byId.get(id)).filter((node): node is Node => Boolean(node))
  // Name and address, the two things a row is looked up by. `order` itself stays
  // whole, because the order sent on drop is the order of every node.
	const outdatedCount = order.filter((n) => n.agent_old).length
  const needle = query.trim().toLowerCase()
  // The group names are whatever the nodes actually use -- there is no separate list
  // to keep in step, and a value stops offering itself as soon as no node carries it.
  const groups = [...new Set(order.map((n) => n.group).filter(Boolean))].sort((a, b) => a.localeCompare(b, "zh"))
  // Worth offering only while some node still lacks one: it is the "what have I not
  // filed yet" filter, and it would be noise once every node has a group.
  const hasUngrouped = order.some((n) => !n.group)
  const visible = order
    .filter((n) => group === "all" || (group === "" ? !n.group : n.group === group))
    .filter((n) =>
      !needle || [n.name, n.ip, n.ipv4, n.ipv6, n.ipv4_pin, n.ipv6_pin].some((v) => v?.toLowerCase().includes(needle)))
    .filter((n) => !onlyOutdated || n.agent_old)
  // Offered on the same terms as the install command: only where this panel is
  // the https domain entry an install command can name.
  const uninstall = canProvision ? uninstallCommand(site) : ""

  async function remove() {
    if (!deleting) return
    setRemoving(true)
    try {
      await api(`/nodes/${deleting.id}`, { method: "DELETE" })
      toast.success("已删除")
      setDeleting(null)
      refresh()
    } catch (e) {
      toast.error((e as Error).message)
    } finally {
      setRemoving(false)
    }
  }

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center justify-end gap-2">
        {groups.length > 0 && (
          <Select value={group} onValueChange={setGroup}>
            <SelectTrigger className="w-full sm:w-40" aria-label="按分组筛选">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="all">全部分组</SelectItem>
              {groups.map((g) => (
                <SelectItem key={g} value={g}>{g}</SelectItem>
              ))}
              {hasUngrouped && <SelectItem value="">未分组</SelectItem>}
            </SelectContent>
          </Select>
        )}
        <Input
          className="mr-auto w-full sm:w-64"
          placeholder="搜索名称或地址"
          aria-label="搜索节点"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
        />
          {outdatedCount > 0 && (
          	<Button
          		variant={onlyOutdated ? "default" : "outline"}
          		onClick={() => setOnlyOutdated((v) => !v)}
          		title="只看 agent 需要升级的节点"
          	>
          		{onlyOutdated ? "只看待升级（点掉）" : `待升级 ${outdatedCount}`}
          	</Button>
          )}
        {/* An open window is visible from the list itself, so nobody has to
            remember they left one open. */}
        <Button variant="outline" disabled={!canProvision} onClick={() => setRegistering(true)}>
          <Server /> 批量添加{reg.left > 0 && ` · ${Math.ceil(reg.left / 60)} 分`}
        </Button>
        <Button disabled={!canProvision} onClick={() => setCreating(true)}>
          <Plus /> 添加节点
        </Button>
      </div>

      {/* Both buttons above are disabled without a word otherwise. The note names
          the half that refused rather than listing every way it could. */}
      {!canProvision && provisionNote && (
        <p className="text-xs leading-relaxed text-muted-foreground">{provisionNote}</p>
      )}

      <Card className="overflow-x-auto p-0">
        <Table className="min-w-[880px]">
          <TableHeader>
            {/* Percentages, or the address column swallows every spare pixel
                and pushes status across the table. The version column was taken
                out of the others rather than added on top: the table already
                fills the page, and a version is five characters wide. */}
            <TableRow>
              <TableHead className="w-[18%]">名称</TableHead>
              <TableHead className="w-[20%]">IP</TableHead>
              <TableHead className="w-[11%]">状态</TableHead>
              <TableHead className="w-[9%]">版本</TableHead>
              <TableHead className="text-right w-[15%]">流量</TableHead>
              <TableHead className="text-right w-[10%]">价格</TableHead>
              <TableHead className="text-right w-[11%]">到期</TableHead>
              <TableHead className="text-right">操作</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {visible.length === 0 ? (
            	<TableRow>
            		<TableCell colSpan={8} className="py-12 text-center text-sm text-muted-foreground">
            			{nodes.length === 0
            				? "还没有节点。用右上角的「添加节点」或「批量添加」，让机器自己登记。"
            				: "没有匹配的节点，换个搜索词或分组试试。"}
            		</TableCell>
            	</TableRow>
            ) : visible.map((n, index) => (
              <TableRow
                key={n.id}
                style={{ viewTransitionName: `node-${n.id}` }}
                data-dragging={drag.dragging === n.id || undefined}
                className="transition-opacity data-[dragging]:opacity-40"
                onDragOver={(e) => { e.preventDefault(); e.dataTransfer.dropEffect = "move" }}
                onDragEnter={() =>
                  drag.dragging !== null &&
                  drag.move(drag.order.findIndex((id) => id === drag.dragging), index)
                }
                onDrop={(e) => { e.preventDefault(); drag.save(drag.order) }}
              >
                <TableCell>
                  <div className="flex items-center gap-2">
                    <DragHandle
                      // A drop sends the order of every node, and a filtered list
                      // offers only its own rows to drop onto, so the index below
                      // is the full one exactly while nothing is filtered out.
                      disabled={!!needle}
                      title={needle ? "清空搜索后可拖动排序" : "拖动排序"}
                      label={`拖动 ${n.name} 排序`}
                      onStart={(e) => {
                        drag.before.current = drag.order
                        drag.setDragging(n.id)
                        e.dataTransfer.effectAllowed = "move"
                        // Firefox refuses to start a drag without a payload.
                        e.dataTransfer.setData("text/plain", String(n.id))
                      }}
                      onEnd={(e) =>
                        e.dataTransfer.dropEffect === "none" ? drag.cancel() : drag.save(drag.order)
                      }
                      onKey={(e) => {
                        const delta = e.key === "ArrowUp" ? -1 : e.key === "ArrowDown" ? 1 : 0
                        if (!delta) return
                        e.preventDefault()
                        drag.before.current = drag.order
                        const ids = drag.move(index, index + delta)
                        if (ids) drag.save(ids)
                      }}
                    />
                    <div className="min-w-0 max-w-[200px] truncate font-medium" title={n.name}>
                      {n.name}
                    </div>
                    {n.group && (
                      <Badge variant="outline" className="shrink-0 border-transparent bg-tag font-normal text-tag-foreground">
                        {n.group}
                      </Badge>
                    )}
                    {n.country && (
                      <Badge
                        variant="outline"
                        title={n.country_pin ? "手动指定" : undefined}
                        className="shrink-0 font-normal text-muted-foreground"
                      >
                        {n.country}
                      </Badge>
                    )}
                  </div>
                </TableCell>
                {/* Addresses live only here, never on the public page. */}
                <TableCell>
                  <Addresses node={n} />
                </TableCell>
                <TableCell>
                  {/* A dot and a word in a light pill, not a filled badge: the filled
                      one read as a button and competed with the node name beside it. */}
                  <span className="inline-flex items-center gap-1.5 rounded-full bg-muted px-2 py-0.5 text-xs">
                    <span
                      className={n.online ? "size-1.5 rounded-full bg-ok" : "size-1.5 rounded-full bg-muted-foreground/60"}
                      aria-hidden
                    />
                    <span className={n.online ? "text-ok-fg" : "text-muted-foreground"}>{n.online ? "在线" : "离线"}</span>
                  </span>
                  {!n.public && <Badge variant="outline" className="ml-1 font-normal">不公开</Badge>}
                  {/* Under the badge, not inside it: the column is a tenth of
                      the table and the three do not share one line. */}
                  {!n.online && n.last_seen > 0 && Date.now() / 1000 - n.last_seen >= 60 && (
                    <div className="tnum mt-1 text-xs text-muted-foreground">
                      {uptime(Date.now() / 1000 - n.last_seen)}
                    </div>
                  )}
                </TableCell>
                {/* What the node last reported it was running. Empty for one that
                    has never connected: the hub stores it at the handshake, so
                    there is nothing to show rather than nothing to ask.

                    `agent_old` is the hub's answer, not this file's: it compares
                    the reported version against the newest release it has read
                    (a daily check). A dot rather than a word, because the row is
                    a number and the page is scanned -- the title carries what to
                    do about it. Nothing is drawn when the hub has not read a
                    release, which is also what an offline hub gets. */}
                <TableCell className="tnum text-sm">
                  {n.agent_version || <span className="text-muted-foreground">—</span>}
                  {n.agent_old && (
                    <span
                      className="ml-1.5 inline-block size-1.5 shrink-0 rounded-full bg-warn align-middle"
                      title={`有新版 agent${agentLatest ? ` ${agentLatest}` : ""}：在该节点上重跑一次安装命令即可升级`}
                      aria-label={`agent 有新版本${agentLatest ? ` ${agentLatest}` : ""}`}
                      role="img"
                    />
                  )}
                </TableCell>
                {/* Counted by the node's own billing rule, as on the public
                    page. */}
                <TableCell className="tnum text-right text-sm">
                  {bytes(monthUsage(n))}
                  <span className="text-muted-foreground">
                    {" / "}{n.traffic_limit > 0 ? bytes(n.traffic_limit) : FOREVER}
                  </span>
                </TableCell>
                <TableCell className="tnum text-right text-sm">
                  {n.price > 0 ? money(n.price, n.currency) : "免费"}
                </TableCell>
                <TableCell className="tnum text-right text-sm">
                  <Expiry date={n.expires_at} />
                </TableCell>
                <TableCell className="text-right whitespace-nowrap">
                  <div className="flex items-center justify-end gap-1">
                  <Button variant="ghost" size="icon" disabled={!canProvision} onClick={() => setInstalling(n)} title="安装 Agent" aria-label="安装 Agent">
                    <Download />
                  </Button>
                  <Button variant="ghost" size="icon" onClick={() => setEditing(n)} title="编辑节点" aria-label="编辑节点">
                    <Pencil />
                  </Button>
                  <Button variant="ghost" size="icon" onClick={() => setBilling(n)} title="续费设置" aria-label="续费设置">
                    <CalendarClock />
                  </Button>
                  <Button variant="ghost" size="icon" onClick={() => setDeleting(n)} title="删除节点" aria-label="删除节点">
                    <Trash2 className="text-destructive" />
                  </Button>
                  </div>
                </TableCell>
              </TableRow>
            ))}
            {nodes.length === 0 && (
              <TableRow>
                <TableCell colSpan={8} className="py-10 text-center text-sm text-muted-foreground">
                  还没有节点，右上角添加
                </TableCell>
              </TableRow>
            )}
            {needle && nodes.length > 0 && !visible.length && (
              <TableRow>
                <TableCell colSpan={8} className="py-10 text-center text-sm text-muted-foreground">
                  没有匹配的节点
                </TableCell>
              </TableRow>
            )}
          </TableBody>
        </Table>
      </Card>

      {creating && (
        <CreateNode
          onClose={() => setCreating(false)}
          onSaved={refresh}
        />
      )}
      {editing && (
        <NodeForm
          node={editing}
          groups={groups}
          groupDropdown={groupDropdown}
          onClose={() => setEditing(null)}
          onSaved={refresh}
        />
      )}
      {billing && (
        <BillingForm node={billing} onClose={() => setBilling(null)} onSaved={refresh} />
      )}
      {registering && <RegisterDialog site={site} reg={reg} onClose={() => { setRegistering(false); refresh() }} />}

      {installing && (
        <InstallDialog
          node={installing}
          site={site}
          onClose={() => setInstalling(null)}
          onRotated={refresh}
        />
      )}
      {deleting && (
        <ConfirmDialog
          title={`删除节点「${deleting.name}」？`}
          description="历史指标、流量记录和凭证一并删除，不可恢复。"
          confirmLabel="删除节点"
          busy={removing}
          onClose={() => setDeleting(null)}
          onConfirm={remove}
        >
          {/* Deleting the node leaves the agent running on the machine, retrying
              with a token the hub no longer accepts. */}
          {uninstall && (
            <div className="space-y-2">
              <div className="flex items-center justify-between gap-2">
                <Label className="text-sm font-medium">卸载 agent</Label>
                <Button variant="ghost" size="sm" onClick={() => copy(uninstall)}>
                  <Copy className="size-4" /> 复制
                </Button>
              </div>
              <pre className="overflow-auto whitespace-pre-wrap break-all rounded-lg border bg-muted/40 p-3 text-xs leading-relaxed select-all">
                {uninstall}
              </pre>
              <p className="text-xs text-muted-foreground">
                在这台机器上以 root 执行，停止 agent，删除二进制、env 文件和服务文件。
              </p>
            </div>
          )}
        </ConfirmDialog>
      )}
    </div>
  )
}

/// A probe's recent latency as a bare polyline. No axes, no library: the column
/// beside it already carries the number, and this only has to show the shape.
function Sparkline({ values, className = "" }: { values: number[]; className?: string }) {
  if (values.length < 2) return <span className="text-xs text-muted-foreground">—</span>
  const w = 64
  const h = 16
  const lo = Math.min(...values)
  const hi = Math.max(...values)
  const span = hi - lo || 1
  const points = values
    .map((v, i) => `${(i / (values.length - 1)) * w},${h - ((v - lo) / span) * (h - 2) - 1}`)
    .join(" ")
  return (
    <svg width={w} height={h} viewBox={`0 0 ${w} ${h}`} className={className} aria-hidden focusable="false">
      <polyline points={points} fill="none" stroke="currentColor" strokeWidth="1.5" vectorEffect="non-scaling-stroke" />
    </svg>
  )
}

function Ping({ nodes }: { nodes: Node[] }) {
  const [tasks, setTasks] = useState<PingTask[]>([])
  // The list starts empty, so an empty-state check on `tasks.length` alone fires while the
  // first fetch is still in flight and tells the operator there are no probes. Themes and
  // Sessions already guard this with a null; here a flag is enough and touches less.
  const [loaded, setLoaded] = useState(false)
	// 首次请求失败时的原因。`load()` 原来把错误整个吞掉（`.catch(() => {})`），于是页面
	// 显示「没有监控」——和「没能取到数据」长得一模一样，而这两件事需要完全不同的动作。
	const [error, setError] = useState("")
  // Measured results, keyed by task id. The hub serves probe history per node --
  // `ping_record`'s key order is built for exactly that query -- so the page asks
  // each node that runs a probe and folds the answers together here.
  const [stats, setStats] = useState<Record<number, { last: number | null; loss: number; series: number[] }>>({})
  // Why a probe produced nothing, when the agent said why: without this the row shows
  // only 100% loss, which reads as a broken link rather than as a probe that cannot run.
  const [probeErrors, setProbeErrors] = useState<PingError[]>([])
	// 与 errors 分开：errors 是 agent **报回来**的原因，needs 是 hub **自己就知道**的事实
	// （节点 agent 太旧、这种探测根本跑不了）。前者是「探测在失败」，后者是「探测还没开始」。
	const [probeNeeds, setProbeNeeds] = useState<PingError[]>([])
  const [editing, setEditing] = useState<Partial<PingTask> | null>(null)
  const [deleting, setDeleting] = useState<PingTask | null>(null)
  const [saving, setSaving] = useState(false)
  const [removing, setRemoving] = useState(false)
  // The probe list is ordered the same way the node list is, and the order is the
  // chart's: the public page draws its lines, colours and legend in this order.
  const drag = useDragOrder(tasks.map((t) => t.id), "/ping-tasks/order", load)
  const listed = new Map(tasks.map((t) => [t.id, t]))
  const ordered = drag.order.map((id) => listed.get(id)).filter((t): t is PingTask => Boolean(t))

  function load() {
    return api<{ tasks: PingTask[]; errors?: PingError[]; needs?: PingError[] }>("/ping-tasks")
      .then((d) => {
        setTasks(d.tasks)
        setProbeErrors(d.errors ?? [])
		setProbeNeeds(d.needs ?? [])
		setError("")
      })
		.catch((e) => setError((e as Error).message))
      .finally(() => setLoaded(true))
  }

  // Fetched once the task list is known, and again whenever it changes. Samples from
  // several nodes are averaged per timestamp: each reports on its own clock, and a
  // task's latency is one line however many machines measure it.
  useEffect(() => {
    const ids = [...new Set(tasks.flatMap((t) => t.nodes))]
    // No probes: nothing to clear. Stale entries are unreachable, since only the rows
    // in `tasks` read them, and setting state synchronously here would cost a render.
    if (ids.length === 0) return
    let alive = true
    type PingPoint = { task_id: number; ts: number; latency: number | null }
    Promise.all(
      ids.map((id) =>
        api<{ ping: PingPoint[]; loss?: Record<string, number> }>(`/nodes/${id}/metrics?series=ping`)
          .catch(() => null)
          .then((d) => [id, d] as const),
      ),
    ).then((answers) => {
      if (!alive) return
      const buckets = new Map<number, Map<number, number[]>>()
      const loss = new Map<number, number[]>()
      for (const [, d] of answers) {
        if (!d) continue
        for (const p of d.ping ?? []) {
          if (p.latency === null || p.latency === undefined) continue
          const perTask = buckets.get(p.task_id) ?? new Map<number, number[]>()
          perTask.set(p.ts, [...(perTask.get(p.ts) ?? []), p.latency])
          buckets.set(p.task_id, perTask)
        }
        for (const [id, pct] of Object.entries(d.loss ?? {})) {
          loss.set(Number(id), [...(loss.get(Number(id)) ?? []), pct])
        }
      }
      const next: typeof stats = {}
      for (const [taskId, perTs] of buckets) {
        const series = [...perTs.entries()]
          .sort((a, b) => a[0] - b[0])
          .map(([, samples]) => Math.round(samples.reduce((a, b) => a + b, 0) / samples.length))
        next[taskId] = {
          last: series.length ? series[series.length - 1] : null,
          // The worst node, not the average: a probe losing packets on one machine is
          // the thing worth seeing in a list.
          loss: Math.round(Math.max(0, ...(loss.get(taskId) ?? [0]))),
          series,
        }
      }
      setStats(next)
    })
    return () => {
      alive = false
    }
  }, [tasks])
  useEffect(() => { load() }, [])

  async function save() {
    if (!editing) return
    if (!editing.name?.trim() || !editing.target?.trim()) return toast.error("请填写名称和目标")
    setSaving(true)
    try {
      await api("/ping-tasks", { method: "POST", body: JSON.stringify(editing) })
      toast.success("已保存，正在下发")
      setEditing(null)
      load()
    } catch (e) {
      toast.error((e as Error).message)
    } finally {
      setSaving(false)
    }
  }

  async function remove() {
    if (!deleting) return
    setRemoving(true)
    try {
      await api(`/ping-tasks/${deleting.id}`, { method: "DELETE" })
      toast.success("监控已删除")
      setDeleting(null)
      load()
    } catch (e) {
      toast.error((e as Error).message)
    } finally {
      setRemoving(false)
    }
  }

  const toggle = (id: number) =>
    setEditing((t) => {
      if (!t) return t
      const nodes = t.nodes ?? []
      return { ...t, nodes: nodes.includes(id) ? nodes.filter((n) => n !== id) : [...nodes, id] }
    })
  // Three helpers rather than one "select all" checkbox: with a fleet of any size,
  // adding a probe to twenty machines was twenty clicks.
  const setAll = (on: boolean) =>
    setEditing((t) => (t ? { ...t, nodes: on ? nodes.map((n) => n.id) : [] } : t))
  const invert = () =>
    setEditing((t) => (t ? { ...t, nodes: nodes.filter((n) => !(t.nodes ?? []).includes(n.id)).map((n) => n.id) } : t))
  // Distinct nodes any probe runs on: coverage is the question this page answers.
  const covered = new Set(tasks.flatMap((t) => t.nodes)).size

  // 首次请求未回：给一个和这张表同形的骨架，而不是先画一张空表再填（切换菜单时的顿挫感
  // 就来自后者）。失败则停在那条原因上，并给一个重试入口 —— toast 会飘走，页面不会。
  if (error) {
  	return (
  		<RetryState
  			message={error}
  			onRetry={() => { setError(""); setLoaded(false); load() }}
  		/>
  	)
  }
  if (!loaded) return <PageSkeleton shape="list" rows={3} />

  // 一个监控都没有时，空表格只会让人以为坏了。这里说明它是空的、以及去哪儿加。
  if (tasks.length === 0) {
    return (
      <EmptyState
        title="还没有监控"
        hint="右上角「添加监控」可以加一条：选 TCP 或 ICMP，勾上要跑它的节点，延迟与丢包就有了。"
      />
    )
  }

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-start justify-between gap-3">
        {/* The left half of this row was empty: the button was pinned right and the row
            said nothing about what the page does. */}
        <div className="min-w-0 flex-1">
          <p className="text-xs leading-relaxed text-muted-foreground">
            每个节点独立探测目标并上报耗时：默认是 TCP 握手，也可以选 ICMP 回显。公开页据此画出延迟与丢包。勾选运行节点，即可让一台机器同时探多个目标。
          </p>
          {tasks.length > 0 && (
            <p className="mt-1.5 text-xs text-muted-foreground">
              共 <span className="font-medium text-foreground">{tasks.length}</span> 个监控 · 覆盖{" "}
              <span className="font-medium text-foreground">{covered}</span> / {nodes.length} 个节点
            </p>
          )}
        </div>
        <Button onClick={() => setEditing({ name: "", target: "", interval: 60, nodes: [], kind: "tcp" })}>
          <Plus /> 添加监控
        </Button>
      </div>

      <Card className="overflow-x-auto p-0">
        <Table className="min-w-[760px]">
          <TableHeader>
            <TableRow>
              <TableHead className="w-[18%]">名称</TableHead>
              <TableHead className="w-[24%]">目标</TableHead>
              <TableHead className="w-[9%]">间隔</TableHead>
              <TableHead className="w-[8%]">节点</TableHead>
              <TableHead className="w-[22%]">延迟</TableHead>
              <TableHead className="w-[8%]">丢包</TableHead>
              <TableHead className="text-right">操作</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {ordered.map((t, index) => (
              <TableRow
                key={t.id}
                data-dragging={drag.dragging === t.id || undefined}
                className="transition-opacity data-[dragging]:opacity-40"
                onDragOver={(e) => { e.preventDefault(); e.dataTransfer.dropEffect = "move" }}
                onDragEnter={() =>
                  drag.dragging !== null &&
                  drag.move(drag.order.findIndex((id) => id === drag.dragging), index)
                }
                onDrop={(e) => { e.preventDefault(); drag.save(drag.order) }}
              >
                <TableCell>
                  <div className="flex items-center gap-2">
                    <DragHandle
                      disabled={false}
                      title="拖动排序"
                      label={`拖动 ${t.name} 排序`}
                      onStart={(e) => {
                        drag.before.current = drag.order
                        drag.setDragging(t.id)
                        e.dataTransfer.effectAllowed = "move"
                        // Firefox refuses to start a drag without a payload.
                        e.dataTransfer.setData("text/plain", String(t.id))
                      }}
                      onEnd={(e) =>
                        e.dataTransfer.dropEffect === "none" ? drag.cancel() : drag.save(drag.order)
                      }
                      onKey={(e) => {
                        const delta = e.key === "ArrowUp" ? -1 : e.key === "ArrowDown" ? 1 : 0
                        if (!delta) return
                        e.preventDefault()
                        drag.before.current = drag.order
                        const ids = drag.move(index, index + delta)
                        if (ids) drag.save(ids)
                      }}
                    />
                    <span className="font-medium">{t.name}</span>
                  </div>
                  {probeNeeds
                    .filter((n) => n.task_id === t.id)
                    .map((n) => (
                      <p key={`need-${n.node_id}`} className={`mt-0.5 text-xs break-all line-clamp-2 ${WARN}`}>
                        需升级：{n.reason}
                        {probeNeeds.filter((x) => x.task_id === t.id).length > 1 ? `（节点 ${n.node_id}）` : ""}
                      </p>
                    ))}
                  {probeErrors
                    .filter((e) => e.task_id === t.id)
                    .map((e) => (
                      <p key={e.node_id} className={`mt-0.5 text-xs break-all line-clamp-2 ${WARN}`}>
                        {e.reason}
                        {probeErrors.filter((x) => x.task_id === t.id).length > 1 ? `（节点 ${e.node_id}）` : ""}
                      </p>
                    ))}
                </TableCell>
                <TableCell className="tnum text-sm">
                  {t.target}
                  {/* Only the echo gets a label. TCP is the default and the common case,
                      so a badge on every row would be noise; what a reader needs to see is
                      the probe that is **not** a handshake -- it is also the one that can
                      fail for a reason the row has to explain. */}
                  {/* Both kinds are labelled now. With two probe types in one list the
                      *absence* of a label reads as "unknown" rather than as "the default",
                      which is what the operator asked after seeing ICMP next to nothing. */}
                  <span className="ml-2 rounded border px-1.5 py-0.5 text-xs text-muted-foreground">
                    {t.kind === "icmp" ? "ICMP" : "TCP"}
                  </span>
                </TableCell>
                <TableCell className="tnum text-sm">{t.interval}s</TableCell>
                <TableCell className="text-sm text-muted-foreground">{t.nodes.length} 个</TableCell>
                <TableCell>
                  {/* Empty until the first fetches land, and never a zero: a task
                      nobody has reported on yet is unknown, not instant. */}
                  {stats[t.id]?.last == null ? (
                    <span className="text-sm text-muted-foreground">—</span>
                  ) : (
                    <div className="flex items-center gap-2">
                      <span className="tnum text-sm">{stats[t.id].last} ms</span>
                      <Sparkline values={stats[t.id].series} className="text-primary" />
                    </div>
                  )}
                </TableCell>
                <TableCell className="text-sm">
                  {stats[t.id]?.last == null ? (
                      <span className="text-sm text-muted-foreground">—</span>
                    ) : (stats[t.id]?.loss ?? 0) > 0 ? (
                    <span className={(stats[t.id]?.loss ?? 0) >= 5 ? "text-danger-fg" : "text-warn-fg"}>
                      {stats[t.id]?.loss}%
                    </span>
                  ) : (
                    <span className="text-muted-foreground">0%</span>
                  )}
                </TableCell>
                <TableCell className="text-right whitespace-nowrap">
                  <Button variant="ghost" size="icon" onClick={() => setEditing(t)} title="编辑监控" aria-label="编辑监控"><Pencil /></Button>
                  <Button variant="ghost" size="icon" onClick={() => setDeleting(t)} title="删除监控" aria-label="删除监控">
                    <Trash2 className="text-destructive" />
                  </Button>
                </TableCell>
              </TableRow>
            ))}
            {loaded && tasks.length === 0 && (
              <TableRow>
                <TableCell colSpan={7} className="py-10 text-center text-sm text-muted-foreground">
                  <p>还没有延迟监控。每个节点独立 TCP 连接目标端口并上报耗时。</p>
                  {/* An empty state that only describes itself leaves the reader to hunt
                      for the control; the toolbar's button is repeated here. */}
                  <Button
                    size="sm"
                    variant="outline"
                    className="mt-3"
                    onClick={() => setEditing({ name: "", target: "", interval: 60, nodes: [], kind: "tcp" })}
                  >
                    <Plus /> 添加监控
                  </Button>
                </TableCell>
              </TableRow>
            )}
          </TableBody>
        </Table>
      </Card>

      {editing && (
        <Dialog open onOpenChange={(open) => !open && setEditing(null)}>
          <DialogContent onOpenAutoFocus={(e) => e.preventDefault()} className="sm:max-w-xl">
            <DialogHeader>
              <DialogTitle>{editing.id ? "编辑监控" : "添加监控"}</DialogTitle>
            </DialogHeader>
            <div className="space-y-5">
              <div className="grid gap-4 sm:grid-cols-2">
                <Field label="名称">
                  {/* A new monitor starts empty, so the cursor belongs here;
                      editing an existing one starts with nothing selected. */}
                  <Input autoFocus={!editing.id} value={editing.name ?? ""} onChange={(e) => setEditing({ ...editing, name: e.target.value })} placeholder="Cloudflare" />
                </Field>
                <Field label="间隔（秒）" hint="5–3600">
                  {/* `|| 60`, as the three other number boxes on this page do:
                      an emptied `type="number"` reads back as "", and Number("")
                      is 0 -- which the hub used to clamp into a 5-second probe on
                      every assigned node. It refuses that now, so this keeps a
                      cleared box from being a round trip to an error. */}
                  <Input type="number" min="5" max="3600" value={editing.interval ?? 60} onChange={(e) => setEditing({ ...editing, interval: Number(e.target.value) || 60 })} />
                </Field>
              </div>
              <Field
                label="探测方式"
                hint={
                  editing.kind === "icmp"
                    ? "发送 ICMP 回显请求。需要 agent 有相应权限（见文档），没有权限时这里会显示原因"
                    : "与目标端口建立 TCP 连接，不需要额外权限"
                }
              >
                {/* Two buttons rather than a select: there are exactly two, and the
                    chosen one has to be visible without opening anything. */}
                <div className="flex gap-2">
                  {([["tcp", "TCP ping"], ["icmp", "ICMP ping"]] as const).map(([value, label]) => (
                    <Button
                      key={value}
                      type="button"
                      variant={editing.kind === value ? "default" : "outline"}
                      onClick={() => setEditing({ ...editing, kind: value })}
                    >
                      {label}
                    </Button>
                  ))}
                </div>
              </Field>
              <Field
                label="目标地址"
                hint={editing.kind === "icmp" ? "主机名或 IP，不要端口" : "host:port"}
              >
                <Input
                  value={editing.target ?? ""}
                  onChange={(e) => setEditing({ ...editing, target: e.target.value })}
                  placeholder={editing.kind === "icmp" ? "1.1.1.1" : "1.1.1.1:443"}
                />
              </Field>
              {/* 保存前就说清：ICMP 需要 agent 1.1.1+，更早的 agent 不会拒绝这种任务，而是把它当 TCP
                  握手跑 —— 一个读数都没有、也不说明原因（真机上就是这么表现的）。hub 在下发后也会
                  把它算进 `needs`，两处用同一条版本规则。 */}
              {editing.kind === "icmp" &&
                (() => {
                  const stale = nodes.filter(
                    (n) => (editing.nodes ?? []).includes(n.id) && versionBelow(n.agent_version, ICMP_AGENT_FLOOR),
                  )
                  if (stale.length === 0) return null
                  return (
                    <p className={`text-xs ${WARN}`}>
                      选中的 {stale.map((n) => n.name).join("、")} 的 agent 太旧（ICMP 需要 {ICMP_AGENT_FLOOR}{" "}
                      或更新），它们不会发 ICMP —— 升级这些节点上的 agent 后才会开始探测。
                    </p>
                  )
                })()}
              <div className="space-y-2 border-t pt-4">
                <div className="flex flex-wrap items-center justify-between gap-2">
                  <Label className="text-sm font-medium">运行节点</Label>
                  <div className="flex items-center gap-1">
                    <span className="mr-1 text-xs text-muted-foreground">
                      已选 <span className="font-medium text-foreground">{editing.nodes?.length ?? 0}</span> / {nodes.length}
                    </span>
                    {/* 清空 rather than 取消: the dialog's own 取消 sits just below and
                        closes the dialog, so the same word would mean two things. The rest
                        state carries a background so they read as buttons before you hover
                        them -- bare ghost buttons look like stray words. */}
                    {([
                      ["全选", () => setAll(true), nodes.length === 0 || (editing.nodes?.length ?? 0) === nodes.length],
                      ["清空", () => setAll(false), (editing.nodes?.length ?? 0) === 0],
                      ["反选", invert, nodes.length === 0],
                    ] as const).map(([label, onClick, disabled]) => (
                      <Button
                        key={label}
                        type="button"
                        size="sm"
                        variant="ghost"
                        className="h-6 rounded-md bg-muted/60 px-2 text-xs font-normal hover:bg-muted"
                        onClick={onClick}
                        disabled={disabled}
                      >
                        {label}
                      </Button>
                    ))}
                  </div>
                </div>
                {/* Cards in a grid rather than bare checkboxes: the country is what makes a
                    name recognisable in a long list, and once most of the fleet is selected
                    the checked state has to be visible at a glance rather than hunted for. */}
                <div className="grid max-h-72 gap-1.5 overflow-y-auto rounded-lg border bg-muted/20 p-2 sm:grid-cols-2">
                  {nodes.map((n) => {
                    const on = editing.nodes?.includes(n.id) ?? false
                    return (
                      <label
                        key={n.id}
                        className={`flex cursor-pointer items-center gap-2 rounded-md border px-2.5 py-2 text-sm transition-colors ${
                          on ? "border-primary bg-accent text-foreground" : "border-border bg-background hover:bg-muted/60"
                        }`}
                      >
                        <input type="checkbox" checked={on} onChange={() => toggle(n.id)} className="size-3.5 shrink-0 accent-primary" />
                        <span className="min-w-0 flex-1 truncate">{n.name}</span>
                        {n.country && (
                          <span className="shrink-0 rounded bg-tag px-1 py-0.5 text-[10px] leading-none text-tag-foreground">{n.country}</span>
                        )}
                      </label>
                    )
                  })}
                  {nodes.length === 0 && <p className="p-2 text-xs text-muted-foreground">先添加节点</p>}
                </div>
              </div>
            </div>
            <DialogFooter>
              <Button variant="ghost" onClick={() => setEditing(null)}>取消</Button>
              <Button onClick={save} disabled={saving}>保存</Button>
            </DialogFooter>
          </DialogContent>
        </Dialog>
      )}
      {deleting && (
        <ConfirmDialog
          title={`删除监控「${deleting.name}」？`}
          description="该监控及其历史延迟记录一并删除，不可恢复。"
          confirmLabel="删除监控"
          busy={removing}
          onClose={() => setDeleting(null)}
          onConfirm={remove}
        />
      )}
    </div>
  )
}

type Theme = {
  name: string
  short: string
  description: string
  version: string
  author: string
  url: string
  // 主题自己声明的「至少需要哪一级主题 API」。hub 比它低的主题会被列出来但不能用，
  // 也不会被服务（公开页回落到内置主题），所以面板要能说明原因。
  api: number
  usable: boolean
  selected: boolean
  // 内置主题在二进制里，没有目录可删。装上一份同名的会顶替它，那一份就是普通
  // 主题，删掉之后内置的重新顶上。
  builtin: boolean
  // theme.json 里声明的设置表单，原样转过来，由 configFields 挑出能画的字段。
  config?: unknown
}

// hub 每天读一次每个主题自己的仓库，结果随主题列表一起回来。这里只把「有新版」
// 画成角标：真正写入的仍然只有 ⟳，检查本身不装任何东西。
type Updates = {
  checked_at: number
  themes: Record<string, { latest?: string; newer: boolean; error?: string }>
}

// 主题在 theme.json 里声明的设置。hub 只存与默认值不同的项，其余由主题用自己的默认值
// 补上，所以主题作者日后改了某个默认值，没动过这一项的站点会跟着变。
//
// 字段不多时是一列；多了改成宽对话框：按分组标题分节，左侧切换，右侧两列，否则几
// 十项排成一条细长的列表。有改动的节在导航上带一个点。
function ThemeSettings({ theme, saved, onClose }: {
  theme: Theme
  saved: Record<string, unknown>
  onClose: () => void
}) {
  const form = configForm(theme.config)
  const fields = configFields(theme.config)
  const sections = configSections(form)
  const large = fields.length > 6
  const paged = large && sections.length > 1
  const [current, setCurrent] = useState(0)
  const [values, setValues] = useState(() => configValues(fields, saved))
  // 保存时叠在什么之上。表单不认识的 key 会保留；只有「恢复默认」会把它们一起清掉
  // ——那是从面板里丢掉一个已被新版主题移除的值的唯一办法。
  const [base, setBase] = useState(saved)
  const [saving, setSaving] = useState(false)
  const set = (key: string, value: unknown) => setValues((old) => ({ ...old, [key]: value }))
  const label = (field: ConfigField) => field.label || field.key
  // 数字框在编辑期间保留自己那段文本：空框表示「没有数字」，而 Number("") 会读成 0。
  const typed = (f: ConfigField) =>
    f.type !== "number" ? values[f.key] : values[f.key] === "" ? NaN : Number(values[f.key])
  const differs = (f: ConfigField) => typed(f) !== f.default

  async function save(e: React.FormEvent) {
    e.preventDefault()
    const invalid = fields.find((f) => f.type === "number" && !fits(f, typed(f)))
    if (invalid) {
      // 浏览器只在屏幕上那些框上做 required/min/max 检查，而切走的分节已经不在
      // 渲染里了 —— 所以这里自己查，并把出问题的那一节切回来。
      setCurrent(Math.max(0, sections.findIndex((section) => section.fields.includes(invalid))))
      const range =
        invalid.min !== undefined && invalid.max !== undefined ? `${invalid.min}–${invalid.max} 之间的`
          : invalid.min !== undefined ? `不小于 ${invalid.min} 的`
            : invalid.max !== undefined ? `不大于 ${invalid.max} 的` : ""
      return toast.error(`「${label(invalid)}」要填${range}数字`)
    }
    setSaving(true)
    try {
      await api(`/themes/${theme.short}/config`, {
        method: "PUT",
        body: JSON.stringify(configOverrides(fields, base, Object.fromEntries(fields.map((f) => [f.key, typed(f)])))),
      })
      toast.success("主题设置已保存，公开页刷新后生效")
      onClose()
    } catch (e) {
      toast.error((e as Error).message)
    } finally {
      setSaving(false)
    }
  }

  // 每一项都是同一形状的一行：带框，名字在左，控件在右。开关原先就是这个形状，
  // 其余类型走 `Field`（名字压在控件上方、没有框）——同一个分节里两种形态并存，
  // 看起来像两套东西。多行的 `text` 是唯一例外：它塞不进右侧那一列，所以保留框、
  // 上下排列。
  //
  // `items-start`：一行里只要有一项带了说明文字就会变高，标题要对齐在同一水平线
  // 上；短的那一行本来高度就一样，所以只影响高的那个。
  const ROW = "h-full gap-4 rounded-lg border bg-muted/30 px-3 py-2.5 text-sm"

  const named = (field: ConfigField) => (
    <span className="min-w-0">
      <span className="block font-medium">{label(field)}</span>
      {field.help && <span className="mt-0.5 block text-xs text-muted-foreground">{field.help}</span>}
    </span>
  )

  const input = (field: ConfigField) => {
    if (field.type === "text") {
      return (
        <div key={field.key} className={`flex flex-col items-stretch ${ROW}`}>
          {named(field)}
          <textarea
            rows={4}
            className={`${TEXT_BOX} text-sm`}
            value={values[field.key] as string}
            onChange={(e) => set(field.key, e.target.value)}
          />
        </div>
      )
    }
    if (field.type === "boolean") {
      // 与「公开显示」「离线通知」同一形状：开关自己可点，文字只是说明。
      return (
        <label key={field.key} className={`flex cursor-pointer items-start justify-between ${ROW}`}>
          {named(field)}
          <Switch checked={values[field.key] as boolean} onCheckedChange={(v) => set(field.key, v)} />
        </label>
      )
    }
    return (
      <div key={field.key} className={`flex items-start justify-between ${ROW}`}>
        {named(field)}
        <div className="w-40 shrink-0">
          {field.type === "select" ? (
            <Select value={values[field.key] as string} onValueChange={(v) => set(field.key, v)}>
              <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
              <SelectContent>
                {field.options!.map((o) => (
                  <SelectItem key={o.value} value={o.value}>{o.label || o.value}</SelectItem>
                ))}
              </SelectContent>
            </Select>
          ) : field.type === "number" ? (
            <Input
              type="number"
              required
              step="any"
              min={field.min}
              max={field.max}
              value={String(values[field.key])}
              onChange={(e) => set(field.key, e.target.value)}
            />
          ) : (
            <Input value={values[field.key] as string} onChange={(e) => set(field.key, e.target.value)} />
          )}
        </div>
      </div>
    )
  }

  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent
        onOpenAutoFocus={(e) => e.preventDefault()}
        // `max-h`, not `h`: it was a fixed height for the 49-field form this
        // layout was written against, and with a handful of fields that is a
        // dialog two-thirds empty below the last one.
        className={large ? "flex max-h-[min(46rem,calc(100dvh-2rem))] flex-col overflow-hidden sm:max-w-4xl" : "sm:max-w-lg"}
      >
        <DialogHeader>
          <DialogTitle>{theme.name} 设置</DialogTitle>
        </DialogHeader>
        <form className="flex min-h-0 flex-1 flex-col gap-4" onSubmit={save}>
          <div className="flex min-h-0 flex-1 flex-col gap-4 sm:flex-row">
            {paged && (
              <nav className="-mx-1 flex shrink-0 gap-1 overflow-x-auto px-1 pb-1 sm:mx-0 sm:w-48 sm:flex-col sm:overflow-y-auto sm:px-0">
                {sections.map((section, index) => (
                  <button
                    key={index}
                    type="button"
                    aria-current={index === current}
                    onClick={() => setCurrent(index)}
                    className={`flex shrink-0 items-center gap-2 rounded-md px-3 py-1.5 text-left text-sm transition-colors ${
                      index === current ? "bg-muted font-medium" : "text-muted-foreground hover:bg-muted/60 hover:text-foreground"
                    }`}
                  >
                    <span className="whitespace-nowrap sm:whitespace-normal">{section.label}</span>
                    {section.fields.some(differs) && (
                      <span className="ml-auto size-1.5 shrink-0 rounded-full bg-primary" title="有改动" />
                    )}
                  </button>
                ))}
              </nav>
            )}
            <div className={`min-h-0 flex-1 ${large ? "overflow-y-auto pr-1" : ""}`}>
              <div className={`grid gap-4 ${large ? "sm:grid-cols-2" : ""}`}>
                {paged
                  ? sections[current].fields.map(input)
                  : form.map((entry, index) =>
                      entry.type === "title" ? (
                        <h3 key={`title-${index}`} className={`pt-2 text-sm font-semibold first:pt-0 ${large ? "sm:col-span-2" : ""}`}>
                          {entry.label}
                        </h3>
                      ) : (
                        input(entry)
                      ),
                    )}
              </div>
            </div>
          </div>
          {/* One row on a phone as well: stacked, the three buttons would take a
              third of the height the fields have. */}
          <DialogFooter className="flex-row items-center border-t pt-4">
            <Button
              type="button"
              variant="outline"
              className="mr-auto"
              onClick={() => {
                setValues(Object.fromEntries(fields.map((f) => [f.key, f.default])))
                setBase({})
              }}
            >
              {paged ? "全部恢复默认" : "恢复默认"}
            </Button>
            <Button type="button" variant="ghost" onClick={onClose}>取消</Button>
            <Button type="submit" disabled={saving}>保存</Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}

function Themes() {
  const [themes, setThemes] = useState<Theme[] | null>(null)
  const [updates, setUpdates] = useState<Updates | null>(null)
  const [busy, setBusy] = useState("")
  const [doomed, setDoomed] = useState<Theme | null>(null)
  const [zoomed, setZoomed] = useState<Theme | null>(null)
	// 哪些预览图已经到了、哪些主题根本没有预览图。两者都是为了让卡片**从第一帧就占住**
	// 预览图的位置：原先的写法是「图加载完才显示」（为的是不闪一个空边框盒子），代价就是
	// 图到的那一刻卡片长高，把读者正在看的东西顶走。骨架能同时满足这两件事。
	const [previewLoaded, setPreviewLoaded] = useState<Set<string>>(new Set())
	const [previewMissing, setPreviewMissing] = useState<Set<string>>(new Set())
  const [configuring, setConfiguring] = useState<{ theme: Theme; saved: Record<string, unknown> } | null>(null)
  const [repo, setRepo] = useState("")
  const picker = useRef<HTMLInputElement>(null)

  const load = () =>
    api<{ themes: Theme[]; updates: Updates }>("/themes")
      .then((data) => {
        setThemes(data.themes)
        setUpdates(data.updates)
      })
      .catch(() => setThemes([]))
  useEffect(() => { load() }, [])

  async function select(short: string) {
    try {
      await api("/settings", { method: "PUT", body: JSON.stringify({ theme: short }) })
      setThemes((old) => old?.map((theme) => ({ ...theme, selected: theme.short === short })) ?? old)
      toast.success("主题已切换")
    } catch (e) {
      toast.error((e as Error).message)
    }
  }

  // Both ways in answer with the installed manifest.
  async function install(how: "upload" | "github", installing: () => Promise<{ theme: Theme }>) {
    setBusy(how)
    try {
      const { theme } = await installing()
      // The hub reads a theme from disk on every request, so it is already live;
      // reloading the list only brings this page up to date.
      toast.success(`已安装 ${theme.name} ${theme.version}`)
      if (how === "github") setRepo("")
      load()
    } catch (e) {
      toast.error((e as Error).message)
    } finally {
      setBusy("")
    }
  }

  // Only a theme whose manifest names a GitHub repository has a source to update
  // from; the hub refuses anything else, and this merely hides the button.
  const updatable = (theme: Theme) => theme.url.startsWith("https://github.com/")

  async function update(theme: Theme) {
    setBusy(`update:${theme.short}`)
    try {
      const { updated, version } = await api<{ updated: boolean; version: string }>(
        `/themes/${theme.short}/update`,
        { method: "POST" },
      )
      toast.success(updated ? `${theme.name} 已更新到 ${version}` : `${theme.name} 已是最新版本 ${version}`)
      if (updated) load()
    } catch (e) {
      toast.error((e as Error).message)
    } finally {
      setBusy("")
    }
  }

  // 打开时先读，而不是让对话框自己去读：否则表单会先闪一下默认值。
  async function configure(theme: Theme) {
    try {
      setConfiguring({ theme, saved: await api(`/themes/${theme.short}/config`) })
    } catch (e) {
      toast.error((e as Error).message)
    }
  }

  async function remove(theme: Theme) {
    setBusy("delete")
    try {
      await api(`/themes/${theme.short}`, { method: "DELETE" })
      toast.success(`已删除 ${theme.name}`)
      load()
    } catch (e) {
      toast.error((e as Error).message)
    } finally {
      setBusy("")
      setDoomed(null)
    }
  }

  if (!themes) {
  	// 统一到共享原语：形状按该页实际长相给（这里分别是卡片网格与表格）。
  	return <PageSkeleton shape="cards" rows={4} />
  }
  // 空列表也要说话：只有一个标题加一片空白，读起来像「加载失败」，而不是「还没有」。
  if (themes.length === 0) {
  	return <EmptyState title="还没有装主题" hint="公开页现在用的是 hub 内置的默认主题；从 GitHub 装一份，或上传一个主题包，就会出现在这里。" />
  }
  return (
    <div className="space-y-4">
      <Card className="gap-4 p-5">
        <div>
          <div className="flex items-center gap-1.5">
            <h3 className="text-sm font-medium">安装主题</h3>
            <Help width="max-w-[min(28rem,calc(100vw-2rem))]">
              <p>
                填主题的 GitHub 仓库地址，例如 <span className="whitespace-nowrap">https://github.com/作者/仓库</span>
              </p>
              <p>仓库首页、Releases 页的地址都可以，总是安装最新的 release。</p>
              <p>也可以上传 release 里的 theme.tar.gz，不要选 Source code。</p>
              <p>两种方式都是同名主题整体替换。</p>
              <p>
                主题的 url 指向 GitHub 仓库时，卡片上的 <RefreshCw className="inline size-3" /> 检查更新，
                版本没变就不下载。
              </p>
            </Help>
          </div>
          {/* Stays in view: it is the one line about what installing permits. */}
          <p className="mt-1 text-xs text-muted-foreground">
            主题代码在访客浏览器中执行，请只安装可信来源。
          </p>
        </div>
        <form
          className="flex flex-wrap gap-2"
          onSubmit={(e) => {
            e.preventDefault()
            install("github", () =>
              api<{ theme: Theme }>("/theme-install", { method: "POST", body: JSON.stringify({ url: repo.trim() }) }),
            )
          }}
        >
          {/* text rather than url: the browser's own validation would answer in
              its own language before the hub's message could. */}
          <Input
            value={repo}
            onChange={(e) => setRepo(e.target.value)}
            inputMode="url"
            autoCapitalize="off"
            autoCorrect="off"
            spellCheck={false}
            placeholder="主题 GitHub 仓库地址"
            aria-label="主题 GitHub 仓库地址"
            className="h-8 flex-1 basis-60"
          />
          <Button size="sm" type="submit" disabled={!!busy || !repo.trim()}>
            <Download /> {busy === "github" ? "安装中…" : "从 GitHub 安装"}
          </Button>
          <Button size="sm" type="button" variant="outline" disabled={!!busy} onClick={() => picker.current?.click()}>
            <Upload /> {busy === "upload" ? "安装中…" : "上传主题包"}
          </Button>
          <input
            ref={picker}
            type="file"
            accept=".gz,.tgz,application/gzip"
            className="hidden"
            onChange={(e) => {
              const file = e.target.files?.[0]
              e.target.value = ""
              if (file) install("upload", () => upload<{ theme: Theme }>("/themes", file))
            }}
          />
        </form>
      </Card>

      {/* items-start：有预览图和没有的卡片不该为了等高而留白 */}
      <div className="grid items-start gap-3 sm:grid-cols-2">
        {themes.map((theme) => (
          <Card key={theme.short} className="gap-4 p-5">
            {/* 主题包里可选的 preview.png，所以后端不用告诉前端有没有这张图：
                没有就是 404，图一直不显示。hidden 挂在 <a> 上而不是 <img> 上——
                隐藏的是整个链接，否则卡片里留着一个高度为 0 却照样吃 gap-4 的空
                链接。必须从 hidden 开始：带边框的 aspect-video 空盒子会在响应回
                来之前就画出来，闪一下再消失。
                缩略图被压到卡片那点宽度，比例不是 16:9 的还会被 object-cover
                裁掉边，所以图本身要能点开看原尺寸——就地开一个对话框，不跳走。 */}
            <button
            	type="button"
            	title="查看完整预览图"
            	className="group relative block w-full cursor-zoom-in"
            	onClick={() => setZoomed(theme)}
            >
            	{previewMissing.has(theme.short) ? (
            		<span className="flex aspect-video w-full items-center justify-center rounded-md border border-dashed text-xs text-muted-foreground">
            			这个主题没有预览图
            		</span>
            	) : (
            		<>
            			{!previewLoaded.has(theme.short) && <Skeleton className="aspect-video w-full rounded-md" />}
            			<img
            				src={`/api/themes/${theme.short}/preview`}
            				alt={`${theme.name} 预览图`}
            				onLoad={() => setPreviewLoaded((s) => new Set(s).add(theme.short))}
            				onError={() => setPreviewMissing((s) => new Set(s).add(theme.short))}
            				className={`${previewLoaded.has(theme.short) ? "" : "hidden"} aspect-video w-full rounded-md border object-cover object-top`}
            			/>
            		</>
            	)}
            </button>
            <div className="flex items-start gap-3">
              <div className="min-w-0 flex-1">
                <div className="flex flex-wrap items-center gap-2">
                  <h3 className="font-medium">{theme.name}</h3>
                  {theme.selected && <Badge>当前</Badge>}
                  {theme.builtin && <Badge variant="secondary" className="font-normal">内置</Badge>}
                  {/* 有新版就画出来，这是这个页面唯一会主动提示的东西：hub 每天读一次
                      仓库，装不装由人决定。 */}
                  {updates?.themes[theme.short]?.newer && (
                    <Badge className="tnum font-normal">
                      {updates.themes[theme.short].latest} 可用
                    </Badge>
                  )}
                  {/* 主题声明的 API 等级比 hub 高：它不会被服务（公开页用内置主题），
                      所以既不能选也不该看起来正常。升级 hub 之后自动生效。 */}
                  {!theme.usable && (
                    <Badge variant="outline" className="font-normal text-muted-foreground">
                      需要更新的 hub
                    </Badge>
                  )}
                </div>
                <p className="mt-1 text-sm text-muted-foreground">{theme.description}</p>
              </div>
              <div className="flex shrink-0 items-center gap-1">
                <Button
                  size="sm"
                  variant={theme.selected ? "secondary" : "default"}
                  disabled={theme.selected || !theme.usable}
                  title={theme.usable ? undefined : "这个主题需要更新的 hub；升级后它会自动生效"}
                  onClick={() => select(theme.short)}
                >
                  {theme.selected ? "使用中" : "使用"}
                </Button>
                {configFields(theme.config).length > 0 && (
                  <Button size="icon" variant="ghost" title="主题设置" onClick={() => configure(theme)}>
                    <SlidersHorizontal />
                  </Button>
                )}
                {updatable(theme) && (
                  <Button
                    size="icon"
                    variant="ghost"
                    title={
                      updates?.themes[theme.short]?.newer
                        ? `从 GitHub 更新到 ${updates.themes[theme.short].latest}`
                        : "从 GitHub 更新"
                    }
                    disabled={!!busy}
                    onClick={() => update(theme)}
                  >
                    <RefreshCw className={busy === `update:${theme.short}` ? "animate-spin" : ""} />
                  </Button>
                )}
                {/* The built-in theme is served from the binary and has no
                    directory to delete -- it is also the fallback everything
                    else lands on. */}
                {!theme.builtin && (
                  <Button
                    size="icon"
                    variant="ghost"
                    title="删除主题"
                    aria-label="删除主题"
                    disabled={!!busy}
                    onClick={() => setDoomed(theme)}
                  >
                    <Trash2 />
                  </Button>
                )}
              </div>
            </div>
            <p className="text-xs text-muted-foreground">
              {theme.author} · {theme.version}
              {theme.url && <> · <a className="hover:underline" href={theme.url} target="_blank" rel="noreferrer">源码</a></>}
            </p>
          </Card>
        ))}
      </div>

      {/* 原图，不是卡片上那张裁过的：宽度给到 4xl，高度让 80vh 兜住，
          object-contain 保证整张都在框里而不是被切一刀。 */}
      {zoomed && (
        <Dialog open onOpenChange={(open) => !open && setZoomed(null)}>
          <DialogContent className="sm:max-w-4xl">
            <DialogHeader>
              <DialogTitle>{zoomed.name} 预览图</DialogTitle>
              <DialogDescription>{zoomed.author} · {zoomed.version}</DialogDescription>
            </DialogHeader>
            <img
              src={`/api/themes/${zoomed.short}/preview`}
              alt={`${zoomed.name} 预览图`}
              className="max-h-[80vh] w-full rounded-md border object-contain"
            />
          </DialogContent>
        </Dialog>
      )}

      {configuring && <ThemeSettings {...configuring} onClose={() => setConfiguring(null)} />}

      {doomed && (
        <ConfirmDialog
          title={`删除 ${doomed.name}？`}
          description={
            doomed.short === "default"
              ? "装上的这份会从磁盘上删掉，公开页回到 hub 内置的那份默认主题。"
              : doomed.selected
                ? "这是当前使用的主题，删除后公开页会回到内置的默认主题。"
                : "主题目录会从磁盘上删掉，重新上传主题包可以装回来。"
          }
          confirmLabel="删除"
          busy={!!busy}
          onClose={() => setDoomed(null)}
          onConfirm={() => remove(doomed)}
        />
      )}
    </div>
  )
}

type Settings = Record<string, string | boolean>

// Two pages write settings, and each loads only what it displays.
function useSettings() {
  const [s, setS] = useState<Settings | null>(null)
  const [error, setError] = useState("")
  // 失败要能被页面看见：原来 `.catch(() => {})` 把原因吞掉，`s` 永远是 null，
  // 于是设置页与安全页会一直停在骨架上，看起来像「一直在加载」。
  const load = useCallback(() => {
  	api<Settings>("/settings")
  		.then((v) => { setS(v); setError("") })
  		.catch((e: Error) => setError(e.message))
  }, [])
  useEffect(() => { load() }, [load])
  return {
    error,
    s,
    // Discards unsaved edits by re-reading the hub's copy. Per-card saving means the
    // only way back is to ask for the stored values again.
    // 清掉失败原因再重取：页面只该说「重试」，不该碰到 setError。
    retry: () => { setError(""); load() },
    reload: load,
    set: (k: string, v: string) => setS((old) => ({ ...(old ?? {}), [k]: v })),
    save: async (patch: Record<string, string>) => {
      try {
        await api("/settings", { method: "PUT", body: JSON.stringify(patch) })
        toast.success("已保存")
        // Only the saved keys and the `*_set` flags are taken from the hub: a
        // credential comes back as a flag, so the typed value must not linger,
        // while another card's unsaved edits on the same page must survive.
        const fresh = await api<Settings>("/settings")
        setS((old) => {
          const next = { ...old }
          for (const key of Object.keys(patch)) next[key] = fresh[key]
          for (const [key, value] of Object.entries(fresh)) if (key.endsWith("_set")) next[key] = value
          return next
        })
      } catch (e) {
        toast.error((e as Error).message)
      }
    },
  }
}

function SettingsTab() {
  const { s, set, save, error, retry } = useSettings()
  if (!s) {
  	// 加载中与失败要分开：失败时停在原因上并给一个重试入口。
  	return error ? (
  		<RetryState message={error} onRetry={retry} />
  	) : (
  		<PageSkeleton shape="form" rows={3} />
  	)
  }

  return (
    <div className="space-y-4">
      <Card className="gap-4 p-5">
        <div className="grid gap-4 sm:grid-cols-2">
          <Field label="站点名称">
            <Input value={String(s.site_name ?? "")} onChange={(e) => set("site_name", e.target.value)} placeholder="Monitor" />
          </Field>
          <Field
            label="历史数据保留天数"
            hint="超过明细窗口（7 天）的历史会先汇总成小时存着，再按这个天数清理，所以天数越大占用的空间增长很慢；累计流量不受影响。最长 365 天，调小会让更早的历史被清掉。"
          >
            <Input
              type="number"
              min={1}
              max={365}
              value={String(s.retention_days ?? "")}
              onChange={(e) => set("retention_days", e.target.value)}
              placeholder="90"
            />
          </Field>
          <Field
            label="GitHub 代理"
            hint="留空直连。仅在 hub 自己拉不到 GitHub Release 时填。这个地址返回的字节会被安装到每一台节点上，只填信得过的镜像"
          >
            <Input
              value={String(s.github_proxy ?? "")}
              onChange={(e) => set("github_proxy", e.target.value)}
              placeholder="https://ghfast.top"
            />
          </Field>
        </div>
        {/* 不是 <label>：点文字不该切换开关，只有开关自己可点。
            aria-labelledby 保住读屏软件那边的关联。 */}
        <div className="flex items-center gap-2 text-sm">
          <Switch
            aria-labelledby="public-page-label"
            checked={s.public_page !== "off"}
            onCheckedChange={(v) => set("public_page", v ? "on" : "off")}
          />
          <span id="public-page-label">开放公开状态页，关闭后所有页面需登录</span>
        </div>
        <div>
          <Button
            size="sm"
            onClick={() =>
              save({
                site_name: String(s.site_name ?? ""),
                // `||` rather than `??`: the hub returns "" for an unset key
                // rather than null, and "" is the one value this key's write path
                // refuses.
                retention_days: String(s.retention_days || "90"),
                github_proxy: String(s.github_proxy ?? ""),
                public_page: s.public_page === "off" ? "off" : "on",
              })
            }
          >
            保存站点设置
          </Button>
        </div>
      </Card>
    </div>
  )
}

const TEXT_BOX =
  "w-full min-w-0 rounded-md border border-input bg-transparent px-3 py-2 shadow-xs outline-none placeholder:text-muted-foreground focus-visible:border-ring focus-visible:ring-[3px] focus-visible:ring-ring/50 dark:bg-input/30"
const TEXTAREA = `${TEXT_BOX} font-mono text-xs`

// One offline alert, filled in the way the hub fills a template: in a single pass,
// JSON-escaped for the webhook body. Previews only; nothing here is sent.
const SAMPLE_NOTE: Record<string, string> = {
  event: "offline",
  node: "香港 · 甲商家",
  title: "🔴 香港 · 甲商家 离线",
  message: "最后上报 09-15 20:13 +08:00",
  time: "09-15 20:16 +08:00",
}

const PLACEHOLDERS = "{{title}} {{message}} {{node}} {{event}} {{site}} {{time}}"

function TemplatePreview({ template, site, json = false }: { template: string; site: string; json?: boolean }) {
  if (!template.trim()) return <p className="text-xs text-muted-foreground">留空保存即恢复默认模板</p>
  const values = { ...SAMPLE_NOTE, site }
  let out = template.replace(/\{\{(event|node|title|message|site|time)\}\}/g, (_, key: keyof typeof values) =>
    json ? JSON.stringify(values[key]).slice(1, -1) : values[key],
  )
  if (json) {
    try {
      out = JSON.stringify(JSON.parse(out), null, 2)
    } catch {
      return (
        <p className="rounded-md bg-destructive/10 px-3 py-2 text-xs text-danger-fg">
          代入后不是合法 JSON，保存会被拒绝。占位符要写在引号里，例如 "text": "{"{{title}}"}"
        </p>
      )
    }
  }
  return (
    <div className="space-y-1">
      <div className="text-xs text-muted-foreground">预览（以一条离线通知为例）</div>
      <pre className="overflow-x-auto rounded-md bg-muted/50 px-3 py-2 font-mono text-xs whitespace-pre-wrap break-all">{out}</pre>
    </div>
  )
}

// A channel's form, collapsed until needed. The summary carries whether the
// channel is configured, so the closed card still answers the common question.
function ChannelCard({ title, icon, configured, children }: { title: string; icon: React.ReactNode; configured: boolean; children: React.ReactNode }) {
  return (
    <Card className="p-5">
      <details className="group">
        <summary className="flex cursor-pointer list-none items-center gap-3 rounded-md outline-none focus-visible:ring-[3px] focus-visible:ring-ring/50 [&::-webkit-details-marker]:hidden">
          {/* A mark per channel: two rows of bare text were indistinguishable at a
              glance, which is the one thing a channel list has to be. */}
          <span className="grid size-9 shrink-0 place-items-center rounded-lg bg-tag text-tag-foreground" aria-hidden>
            {icon}
          </span>
          <span className="min-w-0 flex-1">
            <span className="block text-sm font-medium">{title}</span>
            {/* Status as an icon plus tinted text, not as a bordered pill: the pill read
                as a button, and it was the only thing on the row that looked pressable. */}
            <span className={configured ? "mt-0.5 flex items-center gap-1 text-xs text-ok-fg" : "mt-0.5 flex items-center gap-1 text-xs text-muted-foreground"}>
              {configured ? <CircleCheck className="size-3.5" /> : <CircleAlert className="size-3.5" />}
              {configured ? "已配置" : "未配置"}
            </span>
          </span>
          <span className="shrink-0 text-xs text-muted-foreground group-open:hidden">配置</span>
          <ChevronRight className="size-4 shrink-0 text-muted-foreground transition-transform group-open:rotate-90" />
        </summary>
        <div className="mt-4 space-y-4">{children}</div>
      </details>
    </Card>
  )
}

// Offline alerts are opt-in per node, so turning them on for a fleet needs one
// place rather than one dialog per node.
function OfflineNodes({ nodes, refresh }: { nodes: Node[]; refresh: () => void }) {
  const [busy, setBusy] = useState(false)

  async function apply(targets: Node[], on: boolean) {
    setBusy(true)
    try {
      // Awaited in turn, the requests would cost one round trip per node, and
      // the two-second stream would render each one as it lands.
      await Promise.all(
        targets
          .filter((n) => !!n.notify !== on)
          .map((n) => api(`/nodes/${n.id}`, { method: "PUT", body: JSON.stringify({ notify: on }) })),
      )
    } catch (e) {
      toast.error((e as Error).message)
    } finally {
      refresh()
      setBusy(false)
    }
  }

  const enabled = nodes.filter((n) => n.notify).length
  return (
    <Card className="gap-4 p-5">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0 flex-1">
          <h3 className="text-sm font-medium">离线通知</h3>
          <p className="mt-1 text-xs text-muted-foreground">按节点打开，默认关。已打开 {enabled} / {nodes.length} 台</p>
        </div>
        {/* A matched pair inside one border: the same kind of action differing only in
            direction, which a strong/ghost pairing overstated. */}
        <div className="flex shrink-0 overflow-hidden rounded-md border">
          <Button className="rounded-none" size="sm" variant="ghost" disabled={busy || enabled === nodes.length} onClick={() => apply(nodes, true)}>全部打开</Button>
          <span className="w-px bg-border" aria-hidden />
          <Button className="rounded-none" size="sm" variant="ghost" disabled={busy || enabled === 0} onClick={() => apply(nodes, false)}>全部关闭</Button>
        </div>
      </div>
      {nodes.length > 0 && (
        // One column, with each switch beside the name it belongs to. Two columns put
        // a row's name and its switch half a page apart, so every row had to be read
        // twice -- once to find it, once to find its control.
        <div className="max-h-64 divide-y overflow-y-auto rounded-lg border">
          {nodes.map((node) => (
            <label key={node.id} className="flex cursor-pointer items-center gap-3 px-3 py-2 text-sm hover:bg-muted/50">
              <span
                className={node.online ? "size-1.5 shrink-0 rounded-full bg-ok" : "size-1.5 shrink-0 rounded-full bg-muted-foreground/40"}
                title={node.online ? "在线" : "离线"}
              />
              <span className="min-w-0 flex-1 truncate">{node.name}</span>
              {node.country && (
                <Badge variant="outline" className="shrink-0 border-transparent bg-tag font-normal text-tag-foreground">
                  {node.country}
                </Badge>
              )}
              <Switch checked={!!node.notify} disabled={busy} onCheckedChange={(v) => apply([node], v)} />
            </label>
          ))}
        </div>
      )}
    </Card>
  )
}

function Notify({ nodes, refresh }: { nodes: Node[]; refresh: () => void }) {
  const { s, set, save, reload } = useSettings()
  const [testing, setTesting] = useState(false)
  if (!s) {
  	// 首次请求未回时不再是「什么都不画」：留一个与该页同形的骨架，
  	// 否则切过来先是空白，再突然长出内容 —— 这就是切换菜单的顿挫感。
  	return <PageSkeleton shape="cards" rows={3} />
  }
  const text = (k: string) => String(s[k] ?? "")
  // A credential is sent only when something was typed: the field starts empty
  // because the hub never returns the stored value.
  const typed = (...keys: string[]) =>
    Object.fromEntries(keys.filter((k) => typeof s[k] === "string" && s[k] !== "").map((k) => [k, text(k)]))
  const secretHint = (k: string) => (s[`${k}_set`] ? "已设置，留空不变" : "未设置")

  async function test() {
    setTesting(true)
    try {
      const { sent } = await api<{ sent: string[] }>("/notify/test", { method: "POST" })
      toast.success(`测试通知已发送：${sent.join("、")}`)
    } catch (e) {
      toast.error((e as Error).message)
    } finally {
      setTesting(false)
    }
  }

  return (
    // Wider gaps between sections than within them: the two groups answer different
    // questions, and a single rhythm made the page read as one long form.
    <div className="space-y-6">
      <Section
        title="推送渠道"
        hint="Telegram 和 Webhook 配了哪个就发哪个，也可以同时用。"
        action={
          <Button size="sm" variant="secondary" disabled={testing} onClick={test}>
            <TestTube2 /> {testing ? "发送中…" : "发送测试"}
          </Button>
        }
      >

      <ChannelCard title="Telegram" icon={<Send className="size-4" />} configured={!!s.notify_telegram_token_set && text("notify_telegram_chat") !== ""}>
        <div className="grid gap-4 sm:grid-cols-2">
          <Field label="Bot Token" hint={secretHint("notify_telegram_token")}>
            <Input
              type="password"
              autoComplete="off"
              placeholder={s.notify_telegram_token_set ? "••••••••" : "123456:ABC-DEF…"}
              value={text("notify_telegram_token")}
              onChange={(e) => set("notify_telegram_token", e.target.value)}
            />
          </Field>
          <Field label="Chat ID" hint="数字 ID，群组是负数；公开频道可填 @频道名">
            <Input value={text("notify_telegram_chat")} onChange={(e) => set("notify_telegram_chat", e.target.value)} placeholder="-1001234567890" />
          </Field>
        </div>
        <Field label="消息模板" hint={`纯文本。占位符 ${PLACEHOLDERS}`}>
          <textarea rows={3} className={TEXTAREA} value={text("notify_telegram_text")} onChange={(e) => set("notify_telegram_text", e.target.value)} />
        </Field>
        <TemplatePreview template={text("notify_telegram_text")} site={text("site_name") || "Monitor"} />
        <div className="flex justify-end gap-2 border-t pt-4">
          {s.notify_telegram_token_set && (
            <Button size="sm" variant="ghost" onClick={() => save({ notify_telegram_token: "", notify_telegram_chat: "" })}>
              清除
            </Button>
          )}
          <Button
            size="sm"
            onClick={() =>
              save({
                notify_telegram_chat: text("notify_telegram_chat"),
                notify_telegram_text: text("notify_telegram_text"),
                ...typed("notify_telegram_token"),
              })
            }
          >
            保存 Telegram
          </Button>
        </div>
      </ChannelCard>

      <ChannelCard title="Webhook" icon={<Webhook className="size-4" />} configured={!!s.notify_webhook_url_set}>
        <Field label="URL" hint={secretHint("notify_webhook_url")}>
          <Input
            type="password"
            autoComplete="off"
            placeholder={s.notify_webhook_url_set ? "••••••••" : "https://…"}
            value={text("notify_webhook_url")}
            onChange={(e) => set("notify_webhook_url", e.target.value)}
          />
        </Field>
        <Field label="请求头" hint={`可选，一行一个。${s.notify_webhook_headers_set ? "已设置，留空不变" : ""}`}>
          <textarea
            rows={2}
            className={TEXTAREA}
            placeholder={s.notify_webhook_headers_set ? "••••••••" : "Authorization: Bearer xxx"}
            value={text("notify_webhook_headers")}
            onChange={(e) => set("notify_webhook_headers", e.target.value)}
          />
        </Field>
        <Field label="请求体" hint={`以 POST 发送，Content-Type 为 application/json。占位符 ${PLACEHOLDERS}，须写在引号内`}>
          <textarea rows={4} className={TEXTAREA} value={text("notify_webhook_body")} onChange={(e) => set("notify_webhook_body", e.target.value)} />
        </Field>
        <TemplatePreview template={text("notify_webhook_body")} site={text("site_name") || "Monitor"} json />
        <div className="flex justify-end gap-2 border-t pt-4">
          {s.notify_webhook_headers_set && (
            <Button size="sm" variant="ghost" onClick={() => save({ notify_webhook_headers: "" })}>
              清除请求头
            </Button>
          )}
          {s.notify_webhook_url_set && (
            <Button size="sm" variant="ghost" onClick={() => save({ notify_webhook_url: "", notify_webhook_headers: "" })}>
              清除
            </Button>
          )}
          <Button
            size="sm"
            onClick={() => save({ notify_webhook_body: text("notify_webhook_body"), ...typed("notify_webhook_url", "notify_webhook_headers") })}
          >
            保存 Webhook
          </Button>
        </div>
      </ChannelCard>
      </Section>

      <Section title="触发规则" hint="离线通知在下面按节点打开；流量和到期提醒对填了额度、到期日的节点生效。">
        <OfflineNodes nodes={nodes} refresh={refresh} />
      </Section>

      <Section
      	title="事件设置"
      	hint="离线、流量、到期三种规则的阈值，以及两类提醒开关；改完点右下角保存。"
      >

      <Card className="gap-6 p-6">

      	{/* 数值类规则单独成组，且**一项一行**：三列平铺时说明长短不一，左侧与下方都是参差的空白；
      	    压成一行、说明移到右侧后高度立刻降下来，单位仍留在输入框内部（Field 的 suffix）。 */}
      	<section className="space-y-3">
      		<h4 className="flex items-center gap-2 text-sm font-medium">
      			<SlidersHorizontal className="size-4 text-muted-foreground" /> 基础规则配置
      		</h4>
      		<Field row icon={<Timer className="size-4 text-muted-foreground" />} label="离线宽限期" suffix="分钟" hint="断开超过这么久才算离线，1–30">
      			<Input type="number" min={1} max={30} className="pr-14" value={text("notify_grace")} onChange={(e) => set("notify_grace", e.target.value)} />
      		</Field>
      		<Field row icon={<Gauge className="size-4 text-muted-foreground" />} label="流量提醒" suffix="%" hint="本期用量达到该比例和 100% 时各提醒一次，0 关闭">
      			<Input type="number" min={0} max={100} className="pr-9" value={text("notify_traffic")} onChange={(e) => set("notify_traffic", e.target.value)} />
      		</Field>
      		<Field row icon={<CalendarClock className="size-4 text-muted-foreground" />} label="到期提醒" suffix="天" hint="每天 9 点汇总这么多天内到期的节点，自动续期时也提醒，0 关闭">
      			<Input type="number" min={0} max={365} className="pr-9" value={text("notify_expiry")} onChange={(e) => set("notify_expiry", e.target.value)} />
      		</Field>
      	</section>

      	{/* 开关与上面的数值不是一回事：那些是「什么时候发」，这些是「要不要发」。分成两块，
      	    每一行整行可点、右端对齐，而不是两个飘在卡片里的控件。 */}
<section className="border-t pt-5">
      		<h4 className="flex items-center gap-2 text-sm font-medium">
      			<Bell className="size-4 text-muted-foreground" /> 消息提醒开关
      		</h4>
      		<div className="divide-y">
      			<div className="flex items-center justify-between gap-4 rounded-lg py-3.5 transition-colors hover:bg-muted/40">
      				<span className="min-w-0">
      					<span id="notify-login-label" className="block text-sm font-medium">登录后台时提醒</span>
      					<span className="mt-0.5 block text-xs text-muted-foreground">有人用应急密码或 GitHub 登录后台时发一条</span>
      				</span>
      				<Switch className="shrink-0" aria-labelledby="notify-login-label" checked={s.notify_login !== "off"} onCheckedChange={(v) => set("notify_login", v ? "on" : "off")} />
      			</div>
      			<div className="flex items-center justify-between gap-4 rounded-lg py-3.5 transition-colors hover:bg-muted/40">
      				<span className="min-w-0">
      					<span id="notify-update-label" className="block text-sm font-medium">有新版本时提醒</span>
      					<span className="mt-0.5 block text-xs text-muted-foreground">hub 或 agent 有新版本时发一条，每个新版本只说一次</span>
      				</span>
      				<Switch className="shrink-0" aria-labelledby="notify-update-label" checked={s.notify_update !== "off"} onCheckedChange={(v) => set("notify_update", v ? "on" : "off")} />
      			</div>
      		</div>
      	</section>

      	{/* 分割线之上是读的部分，之下是唯一会提交的动作。 */}
      	<div className="flex justify-end gap-2 border-t pt-4">
      		<Button size="sm" variant="ghost" onClick={reload} title="放弃未保存的修改">重置</Button>
      		<Button
      			size="sm"
      			onClick={() =>
      				save({
      					notify_grace: text("notify_grace"),
      					notify_traffic: text("notify_traffic"),
      					notify_expiry: text("notify_expiry"),
      					notify_login: s.notify_login === "off" ? "off" : "on",
      					notify_update: s.notify_update === "off" ? "off" : "on",
      				})
      			}
      		>
      			保存事件设置
      		</Button>
      	</div>
      </Card>
      </Section>
    </div>
  )
}

// The two ways into this panel, on their own page: the GitHub identity it trusts
// and the password that works when GitHub does not.
type Session = { id: string; current: boolean; created_at: number; login: string; ip: string; user_agent: string; last_seen: number }

/// A coarse device label from a user agent, which is all the panel needs and all it can
/// honestly claim: the string is whatever the client chose to send, so it is shown as a
/// guess and the raw value stays in the title attribute.
function device(ua: string): string {
  if (!ua) return "—"
  const os = /iPhone|iPad/.test(ua) ? "iOS"
    : /Android/.test(ua) ? "Android"
    : /Mac OS X/.test(ua) ? "macOS"
    : /Windows/.test(ua) ? "Windows"
    : /Linux/.test(ua) ? "Linux"
    : ""
  const browser = /Edg\//.test(ua) ? "Edge"
    : /OPR\//.test(ua) ? "Opera"
    : /Firefox\//.test(ua) ? "Firefox"
    : /Chrome\//.test(ua) ? "Chrome"
    : /Safari\//.test(ua) ? "Safari"
    : "未知浏览器"
  return os ? `${browser} · ${os}` : browser
}

/// How long ago, in the largest unit that still reads as a number a person uses.
function since(ts: number): string {
  if (!ts) return "—"
  const secs = Math.max(0, Math.floor(Date.now() / 1000) - ts)
  if (secs < 60) return "刚刚"
  if (secs < 3600) return `${Math.floor(secs / 60)} 分钟前`
  if (secs < 86_400) return `${Math.floor(secs / 3600)} 小时前`
  return `${Math.floor(secs / 86_400)} 天前`
}

function Sessions() {
  const [rows, setRows] = useState<Session[] | null>(null)
  const [doomed, setDoomed] = useState<Session | null>(null)
  const [busy, setBusy] = useState("")

  const [error, setError] = useState("")
  const load = () => api<Session[]>("/sessions").then((r) => { setRows(r); setError("") }).catch((e: Error) => { setError(e.message); toast.error(e.message) })
  useEffect(() => { load() }, [])

  async function remove(id: string) {
    setBusy(id)
    try {
      await api(`/sessions/${id}`, { method: "DELETE" })
      toast.success("已删除会话")
      load()
    } catch (e) {
      toast.error((e as Error).message)
    } finally {
      setBusy("")
      setDoomed(null)
    }
  }

  if (!rows) {
  	// 失败与加载中要分开：原来只 toast，`rows` 永远是 null，于是骨架一直转下去。
  	return error ? (
  		<RetryState message={error} onRetry={() => { setError(""); load() }} />
  	) : (
  		<PageSkeleton shape="list" rows={5} />
  	)
  }
  // 空列表也要说话：只有一个标题加一片空白，读起来像「加载失败」，而不是「还没有」。
  if (rows.length === 0) {
  	return <EmptyState title="还没有登录会话" hint="用 GitHub 或应急密码登录之后，这里会列出每一个已登录的浏览器，可以逐个撤销。" />
  }
  return (
    <Card className="gap-4 p-5">
      <div>
        <h3 className="text-sm font-medium">登录会话</h3>
        <p className="mt-1 text-xs text-muted-foreground">
          每次登录一条，14 天后过期。删除后该设备下一次请求就被登出。
        </p>
      </div>
      {/* The list grew without limit: a dozen logins pushed 应急密码 and GitHub 单点登录
          off the first screen. Capped and scrolled instead. The rows say everything the
          backend knows -- the table stores a token hash and an expiry, nothing else -- so
          this is not a place where more columns can come from. */}
      {/* A table, not a row of loose text: side by side is what makes five facts
          comparable down a list. Still capped and scrolled -- the card must not grow
          with the number of sessions. */}
      <div className="max-h-72 overflow-y-auto">
        <Table className="min-w-[620px]">
          <TableHeader>
            <TableRow>
              <TableHead>登录时间</TableHead>
              <TableHead>方式</TableHead>
              <TableHead>来源 IP</TableHead>
              <TableHead>设备</TableHead>
              <TableHead>最后活动</TableHead>
              <TableHead className="text-right">操作</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {rows.map((s) => (
              <TableRow key={s.id}>
                <TableCell className="tnum text-sm whitespace-nowrap">
                  {new Date(s.created_at * 1000).toLocaleString()}
                  {s.current && (
                    <Badge variant="secondary" className="ml-2">
                      当前设备
                    </Badge>
                  )}
                </TableCell>
                <TableCell className="text-sm">
                  <Badge variant="outline" className="font-normal">
                    {s.login ? `GitHub · ${s.login}` : "应急密码"}
                  </Badge>
                </TableCell>
                {/* Empty for sessions issued before the hub recorded any of this, and for
                    the two paths that reissue one without a peer address. */}
                <TableCell className="tnum text-sm text-muted-foreground">{s.ip || "—"}</TableCell>
                <TableCell className="max-w-[12rem] truncate text-sm text-muted-foreground" title={s.user_agent || undefined}>
                  {device(s.user_agent)}
                </TableCell>
                <TableCell className="text-sm text-muted-foreground">{since(s.last_seen)}</TableCell>
                <TableCell className="text-right">
                  {/* 当前会话没有删除按钮：右上角的退出登录做的就是这件事，而在这里删
                      只会让已经渲染好的面板以为自己还登着。 */}
                  {!s.current && (
                    <Button size="icon" variant="ghost" disabled={!!busy} onClick={() => setDoomed(s)} title="退出该设备" aria-label="退出该设备">
                      <Trash2 />
                    </Button>
                  )}
                </TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      </div>
      {doomed && (
        <ConfirmDialog
          title="退出该设备？"
          description="该设备下一次请求就会被登出。它在 14 天内本来也会自然过期。"
          confirmLabel="退出该设备"
          busy={!!busy}
          onClose={() => setDoomed(null)}
          onConfirm={() => remove(doomed.id)}
        />
      )}
    </Card>
  )
}

function Security({ site }: { site: string }) {
  const { s, set, save, error, retry } = useSettings()
  const [password, setPassword] = useState("")
  if (!s) {
  	// 加载中与失败要分开：失败时停在原因上并给一个重试入口。
  	return error ? (
  		<RetryState message={error} onRetry={retry} />
  	) : (
  		<PageSkeleton shape="form" rows={3} />
  	)
  }
  const callback = `${site}/api/auth/github/callback`

  return (
    <div className="space-y-4">
      <Sessions />

      <Card className="gap-4 p-5">
        <div>
          <h3 className="text-sm font-medium">GitHub 单点登录</h3>
          <p className="mt-1 text-xs text-muted-foreground">
            OAuth App 回调地址 <code className="rounded bg-muted px-1">{callback}</code>
          </p>
        </div>
        <div className="grid gap-4 sm:grid-cols-2">
          <Field label="Client ID">
            <Input value={String(s.github_client_id ?? "")} onChange={(e) => set("github_client_id", e.target.value)} />
          </Field>
          <Field label="Client Secret" hint={s.github_secret_set ? "已设置，留空不变" : "未设置"}>
            <Input type="password" placeholder={s.github_secret_set ? "••••••••" : ""} onChange={(e) => set("github_client_secret", e.target.value)} />
          </Field>
        </div>
        {String(s.github_client_id ?? "") !== "" && String(s.github_allowed_users ?? "").trim() === "" && (
          <p className="rounded-md bg-destructive/10 px-3 py-2 text-sm text-danger-fg">
            白名单为空，GitHub 登录拒绝所有人。填入用户名并保存后生效。
          </p>
        )}
        <Field label="允许登录的 GitHub 用户名" hint="逗号分隔。留空 = 拒绝所有人，不是放行所有人">
          <Input value={String(s.github_allowed_users ?? "")} onChange={(e) => set("github_allowed_users", e.target.value)} placeholder="GitHub 用户名" />
        </Field>
        <div>
          <Button
            size="sm"
            onClick={() => {
              const patch: Record<string, string> = {
                github_client_id: String(s.github_client_id ?? ""),
                github_allowed_users: String(s.github_allowed_users ?? ""),
              }
              if (typeof s.github_client_secret === "string" && s.github_client_secret) {
                patch.github_client_secret = s.github_client_secret
              }
              save(patch)
            }}
          >
            保存 GitHub 设置
          </Button>
        </div>
      </Card>

      <Card className="gap-4 p-5">
        <div>
          <h3 className="text-sm font-medium">应急密码</h3>
          <p className="mt-1 text-xs text-muted-foreground">
            GitHub 不可用时的备用入口。修改后其它设备登录立即失效，当前设备不受影响。
          </p>
        </div>
        <Field label="新密码" hint="至少 12 位">
          <Input type="password" value={password} onChange={(e) => setPassword(e.target.value)} autoComplete="new-password" />
        </Field>
        <div>
          <Button
            size="sm"
            disabled={password.length < 12}
            onClick={() => save({ admin_password: password }).then(() => setPassword(""))}
          >
            修改密码
          </Button>
        </div>
      </Card>
    </div>
  )
}

type DbInfo = {
  path: string
  size: number
  wal: number
  free: number
  /** Timestamp of the earliest history row, null on a database with none. */
  oldest: number | null
  retention: number
  rows: Record<string, number>
}

// The only two tables whose row count indicates anything about size. Every other
// holds one row per node or per key.
const DB_ROWS: [string, string][] = [
  ["metric", "历史明细"],
  ["ping_record", "延迟记录"],
]

function Data() {
  const [info, setInfo] = useState<DbInfo | null>(null)
  const [busy, setBusy] = useState("")
  const [confirm, setConfirm] = useState<"vacuum" | null>(null)
  const [pending, setPending] = useState<File | null>(null)
  const [sent, setSent] = useState(0)
  // Closing the dialog must stop the upload rather than merely hide it: restore
  // is the one irreversible action here, and it takes minutes on a large
  // backup.
  const abort = useRef<AbortController | null>(null)
  const picker = useRef<HTMLInputElement>(null)

  const [error, setError] = useState("")
  const load = () => api<DbInfo>("/db").then((r) => { setInfo(r); setError("") }).catch((e: Error) => { setError(e.message); toast.error(e.message) })
  useEffect(() => { load() }, [])

  async function vacuum() {
    setBusy("vacuum")
    try {
      const { pruned, freed } = await api<{ pruned: number; freed: number }>("/db/vacuum", { method: "POST" })
      toast.success(`已清理 ${pruned} 行，回收 ${bytes(freed)}`)
      load()
    } catch (e) {
      toast.error((e as Error).message)
    } finally {
      setBusy("")
      setConfirm(null)
    }
  }

  async function restore(file: File) {
    setBusy("restore")
    setSent(0)
    abort.current = new AbortController()
    try {
      await upload("/db/restore", file, setSent, abort.current.signal)
      toast.success("已恢复，正在重新加载")
      // Every node, setting and session on the page came from the database just
      // replaced.
      setTimeout(() => location.reload(), 800)
    } catch (e) {
      // Aborting partway is not a failure: the hub replaces nothing until the
      // last chunk, so the original database remains.
      const aborted = (e as Error).name === "AbortError"
      if (aborted) toast.info("已取消，数据库没有改动")
      else toast.error((e as Error).message)
      setBusy("")
    }
    setPending(null)
  }

  if (!info) {
  	// 失败与加载中要分开：原来只 toast，`rows` 永远是 null，于是骨架一直转下去。
  	return error ? (
  		<RetryState message={error} onRetry={() => { setError(""); load() }} />
  	) : (
  		<PageSkeleton shape="list" rows={5} />
  	)
  }
  const stat = (label: string, value: string) => (
    <div key={label}>
      <div className="text-xs text-muted-foreground">{label}</div>
      <div className="tnum mt-0.5 text-sm">{value}</div>
    </div>
  )

  return (
    <div className="space-y-4">
      <Card className="gap-4 p-5">
        <h3 className="text-sm font-medium">数据库</h3>
        <div className="grid grid-cols-2 gap-4 sm:grid-cols-4">
          {stat("文件大小", bytes(info.size))}
          {stat("预写日志", bytes(info.wal))}
          {stat("可回收空间", bytes(info.free))}
          {stat("保留天数", `${info.retention} 天`)}
          {/* 和保留天数并排：跨度小于保留期是还没攒够，大于保留期就是每小时
              那次 prune 没在跑。 */}
          {stat("历史跨度", info.oldest ? `${Math.floor((Date.now() / 1000 - info.oldest) / 86400)} 天` : "—")}
          {DB_ROWS.map(([key, label]) => stat(label, (info.rows[key] ?? 0).toLocaleString()))}
        </div>
        <p className="truncate text-xs text-muted-foreground" title={info.path}>
          <code>{info.path}</code>
        </p>
      </Card>

      <Card className="gap-4 p-5">
        <div>
          <h3 className="text-sm font-medium">回收空间</h3>
          <p className="mt-1 text-xs leading-relaxed text-muted-foreground">
            按保留天数清掉过期明细，再重建数据库文件把空出来的页还给磁盘（SQLite 的 VACUUM）。
            重建期间需要与数据库等量的空闲磁盘，过程中面板和上报会短暂变慢。
          </p>
        </div>
        <div>
          <Button size="sm" variant="secondary" disabled={!!busy} onClick={() => setConfirm("vacuum")}>
            {busy === "vacuum" ? "回收中…" : "立即回收"}
          </Button>
        </div>
      </Card>

      <Card className="gap-4 p-5">
        <div>
          <h3 className="text-sm font-medium">备份</h3>
          <p className="mt-1 text-xs leading-relaxed text-muted-foreground">
            导出的是整个数据库，含节点凭证与登录密码哈希，请当作密钥保管。恢复会用备份文件整体覆盖当前数据，
            当前节点、设置、历史全部作废，所有设备需要重新登录。
            <br />
            请用这里导出的文件恢复：直接复制 <code>monitor.db</code> 会丢掉预写日志里还没落盘的那部分。
          </p>
        </div>
        <div className="flex flex-wrap gap-2">
          {/* The browser's own download: the file is streamed straight from
              the response, never held in the page. */}
          <Button size="sm" asChild>
            <a href="/api/db/backup" download>
              <Download /> 导出备份
            </a>
          </Button>
          <Button size="sm" variant="secondary" disabled={!!busy} onClick={() => picker.current?.click()}>
            <Upload /> 导入备份
          </Button>
          <input
            ref={picker}
            type="file"
            accept=".db,application/octet-stream"
            className="hidden"
            onChange={(e) => {
              setPending(e.target.files?.[0] ?? null)
              e.target.value = ""
            }}
          />
        </div>
      </Card>

      {confirm === "vacuum" && (
        <ConfirmDialog
          title="回收空间？"
          description="超出保留天数的历史明细会被删除，然后重建数据库文件。累计流量不受影响。"
          confirmLabel="开始回收"
          tone="default"
          busy={!!busy}
          onClose={() => setConfirm(null)}
          onConfirm={vacuum}
        />
      )}
      {pending && (
        <ConfirmDialog
          title="用备份覆盖当前数据？"
          description={`将用 ${pending.name}（${bytes(pending.size)}）整体替换当前数据库。当前的节点、设置和历史全部丢失，且无法撤销。`}
          confirmLabel={busy === "restore" ? `已上传 ${bytes(sent)} / ${bytes(pending.size)}` : "确认恢复"}
          busy={!!busy}
          onClose={() => { abort.current?.abort(); setPending(null) }}
          onConfirm={() => restore(pending)}
        />
      )}
    </div>
  )
}

// Each area is its own route rather than a tab, so a page can be linked to and a
// reload returns to the same section. Grouped the way the work divides: the machines
// and what they report, then the settings that apply to all of them. Seven flat items
// gave no hint that 通知 and 节点 are different kinds of thing.
// Our own repositories, not upstream's: the release a version names is published
// here.
const releaseUrl = (repo: string, version: string) => `https://github.com/spot-probe/${repo}/releases/tag/v${version}`
// The page for this card's own subject. It used to point at 接入节点, which
// explains the first install and leaves the reader to find the upgrade page --
// the one page that answers "how do I send this to every machine" is /install/batch.
const BATCH_DOCS = "https://spot-probe-docs.hualala.workers.dev/install/batch"

/** `v1.2.0 → v1.3.0` when something is published, the running version alone otherwise. */
function VersionPair({ current, latest }: { current: string; latest: string }) {
  if (!behind(current, latest)) {
    return <span className="text-xs text-muted-foreground">{latest ? `已是最新 v${current}` : `当前 v${current}`}</span>
  }
  return (
    <span className="text-xs text-muted-foreground">
      v{current} <span className="px-0.5">→</span>
      <span className="ml-0.5 font-medium text-foreground">v{latest}</span>
    </span>
  )
}

/** Outdated nodes grouped by the version they run, oldest first. */
function byVersion(nodes: Node[]): [string, Node[]][] {
  const groups = new Map<string, Node[]>()
  for (const n of nodes) groups.set(n.agent_version, [...(groups.get(n.agent_version) ?? []), n])
  return [...groups].sort(([a], [b]) => a.localeCompare(b, undefined, { numeric: true }))
}

/**
 * What is published for the hub and for the agents.
 *
 * Its own route rather than a banner on the node list; the dot in the navigation
 * is what says there is something here.
 *
 * Which nodes are behind is the hub's own verdict (`agent_old`), not a comparison
 * made again here: the chart in the node table and this list then cannot disagree.
 *
 * The hub card names the release alone -- how to take it depends on how the hub
 * was installed, script or container, which the hub cannot tell.
 */
function Update({ versions, reload, nodes, site, canProvision, provisionNote, agentLatest }: {
  versions: Versions | null
  reload: () => void
  nodes: Node[]
  site: string
  canProvision: boolean
  provisionNote: string
  agentLatest: string | null
}) {
  const [saving, setSaving] = useState(false)
  if (!versions) {
  	// 首次请求未回时不再是「什么都不画」：留一个与该页同形的骨架，
  	// 否则切过来先是空白，再突然长出内容 —— 这就是切换菜单的顿挫感。
  	return <PageSkeleton shape="list" rows={3} />
  }
  const outdated = nodes.filter((n) => n.agent_old)
  const offline = outdated.filter((n) => !n.online).length
  // The same refusal the node page carries for its install command, and for the
  // same reason: a command naming a plaintext origin is one the agent refuses.
  const upgrade = canProvision ? upgradeCommand(site) : ""
  const unreachable = !versions.hub_latest && !versions.agent_latest
  // The command does not depend on the lookup, so it is offered whenever it was
  // ever needed -- including on a hub that reads agents through the GitHub proxy,
  // which is exactly the hub that cannot read tags.
  const needsAgents = outdated.length > 0 || !agentLatest

  // Applied on the spot: one switch, and the navigation changes with it.
  async function setNotice(on: boolean) {
    setSaving(true)
    try {
      await api("/settings", { method: "PUT", body: JSON.stringify({ update_notice: on ? "on" : "off" }) })
      reload()
    } catch (e) {
      toast.error((e as Error).message)
    } finally {
      setSaving(false)
    }
  }

  return (
    <div className="space-y-4">
      {unreachable && (
        <Card className="p-5">
          <p className="text-sm text-muted-foreground">
            查不到最新版本：这台 hub 连不上 api.github.com。面板里配的 GitHub 代理只用于下载，不作用于这一项。
          </p>
        </Card>
      )}

      <Card className="gap-4 p-5">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <h3 className="text-sm font-medium">hub</h3>
          <div className="flex items-center gap-3">
            <VersionPair current={versions.hub} latest={versions.hub_latest} />
            {behind(versions.hub, versions.hub_latest) && (
              <Button size="sm" variant="ghost" asChild>
                <a href={releaseUrl("monitor", versions.hub_latest)} target="_blank" rel="noreferrer">
                  发布说明
                </a>
              </Button>
            )}
          </div>
        </div>
      </Card>

      <Card className="gap-4 p-5">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <h3 className="text-sm font-medium">agent</h3>
          <span className="text-xs text-muted-foreground">
            {agentLatest
              ? outdated.length
                ? <>最新 v{agentLatest} · <span className="font-medium text-foreground">{outdated.length} 台待升级</span></>
                : `全部已是最新 v${agentLatest}`
              : "查不到版本"}
          </span>
        </div>
        {needsAgents && (
          <>
            {/* The order is not a preference, it is a failure mode we hit for real: an
                agent older than a probe kind does not refuse the task, it runs it as a
                TCP handshake, which reports nothing and explains nothing (a flat 100%
                loss while the target answered in 8 ms). Upgrading the hub first does not
                help -- the agent is what has to understand the new task. */}
            <p className="rounded-lg border border-warn/30 bg-warn/5 px-3 py-2.5 text-xs leading-relaxed">
              <span className="font-medium">先升 agent，再升 hub。</span>
              比 hub 旧的 agent 不认识新的探测方式时<span className="font-medium">不会报错</span>，而是把任务按 TCP 跑 ——
              结果是一个读数都没有。升 hub 不会修好这一点，因为要看懂新任务的是 agent。
            </p>
            <p className="text-xs leading-relaxed text-muted-foreground">
              以 root 在每台机器上执行一次。命令不含凭证、沿用机器上已有的设置，不会新建节点，也不会消耗注册窗口。
            </p>
            {upgrade ? (
              <>
                <pre className="overflow-auto whitespace-pre-wrap break-all rounded-lg border bg-muted/40 p-3 text-xs leading-relaxed select-all">
                  {upgrade}
                </pre>
                <div className="flex flex-wrap gap-2">
                  <Button size="sm" variant="secondary" onClick={() => copy(upgrade)}>
                    <Copy className="size-4" /> 复制命令
                  </Button>
                  {agentLatest && (
                    <Button size="sm" variant="ghost" asChild>
                      <a href={releaseUrl("agent", agentLatest)} target="_blank" rel="noreferrer">
                        发布说明
                      </a>
                    </Button>
                  )}
                  <Button size="sm" variant="ghost" asChild>
                    <a href={BATCH_DOCS} target="_blank" rel="noreferrer">
                      批量升级的做法
                    </a>
                  </Button>
                </div>
              </>
            ) : (
              // Why the commands are not there: the hub or this browser refused,
              // and the two are fixed by different people.
              <p className="text-xs leading-relaxed text-muted-foreground">
                {provisionNote || "请通过 HTTPS 域名访问面板后生成升级命令。"}
              </p>
            )}
            {/* Collapsed until asked for, grouped by version, and bounded in height
                once open, so any number of nodes stays one line on the page. */}
            {outdated.length > 0 && (
              <details className="group border-t pt-3">
                <summary className="flex cursor-pointer list-none items-center justify-between gap-3 rounded-md text-xs text-muted-foreground outline-none focus-visible:ring-[3px] focus-visible:ring-ring/50 [&::-webkit-details-marker]:hidden">
                  <span className="flex items-center gap-1.5">
                    <ChevronRight className="size-4 transition-transform group-open:rotate-90" />
                    待升级的节点
                  </span>
                  {offline > 0 && <span>其中 {offline} 台离线</span>}
                </summary>
                <div className="mt-3 max-h-60 space-y-3 overflow-auto">
                  {byVersion(outdated).map(([version, group]) => (
                    <div key={version} className="space-y-1.5">
                      <div className="text-xs text-muted-foreground">v{version} · {group.length} 台</div>
                      <div className="flex flex-wrap gap-1.5">
                        {group.map((n) => (
                          <Badge
                            key={n.id}
                            variant="secondary"
                            className={`font-normal ${n.online ? "" : "opacity-50"}`}
                            title={n.online ? undefined : "离线"}
                          >
                            {n.name}
                          </Badge>
                        ))}
                      </div>
                    </div>
                  ))}
                </div>
              </details>
            )}
          </>
        )}
      </Card>

      <label className="flex cursor-pointer items-center justify-between gap-4 rounded-lg border bg-muted/30 px-3 py-2.5 text-sm">
        <span>
          <span className="block font-medium">更新提醒</span>
          <span className="mt-0.5 block text-xs text-muted-foreground">
            有新版本时在导航的「更新」旁显示小圆点。关闭只是不再显示圆点，这一页照常检查
          </span>
        </span>
        <Switch checked={versions.notice} disabled={saving} onCheckedChange={setNotice} />
      </label>
    </div>
  )
}

export const ADMIN_SECTIONS = [
  {
    group: "资源管理",
    items: [
      { path: "/admin/nodes", label: "节点", icon: Server },
      { path: "/admin/ping", label: "延迟", icon: Radio },
      { path: "/admin/data", label: "数据", icon: Database },
    ],
  },
  {
    group: "系统设置",
    items: [
      { path: "/admin/notify", label: "通知", icon: Bell },
      { path: "/admin/themes", label: "主题", icon: Palette },
      { path: "/admin/security", label: "安全", icon: Shield },
      { path: "/admin/settings", label: "设置", icon: Settings },
      { path: "/admin/update", label: "更新", icon: ArrowUpCircle },
    ],
  },
]

/** Flattened, for the header: the page's own label and the group it belongs to. */
export const ADMIN_ITEMS = ADMIN_SECTIONS.flatMap((section) => section.items)

const WARN = 'text-destructive'

export function Admin({
  path,
  nodes,
  refresh,
  site,
  canProvision,
  provisionNote,
  groupDropdown,
  agentLatest,
  versions,
  reloadVersions,
}: {
  path: string
  nodes: Node[]
  refresh: () => void
  site: string
  canProvision: boolean
  provisionNote: string
  /** `--group-dropdown`: offer the groups in use as a list under that field. */
  groupDropdown: boolean
  agentLatest: string | null
  /** Read once in `App`, which also marks the navigation with it. */
  versions: Versions | null
  reloadVersions: () => void
}) {
  return (
    <div className="min-w-0">
        {path === "/admin/ping" ? (
          <Ping nodes={nodes} />
        ) : path === "/admin/notify" ? (
          <Notify nodes={nodes} refresh={refresh} />
        ) : path === "/admin/data" ? (
          <Data />
        ) : path === "/admin/themes" ? (
          <Themes />
        ) : path === "/admin/security" ? (
          <Security site={site} />
        ) : path === "/admin/settings" ? (
          <SettingsTab />
        ) : path === "/admin/update" ? (
          <Update
            versions={versions}
            reload={reloadVersions}
            nodes={nodes}
            site={site}
            canProvision={canProvision}
            provisionNote={provisionNote}
            agentLatest={agentLatest}
          />
        ) : (
          <Nodes nodes={nodes} refresh={refresh} site={site} canProvision={canProvision} provisionNote={provisionNote} groupDropdown={groupDropdown} agentLatest={agentLatest} />
        )}
      </div>
  )
}
