import { useEffect, useState } from "react"

export type Metrics = {
  uptime: number
  cpu: number
  load: [number, number, number]
  mem_total: number
  mem_used: number
  swap_total: number
  swap_used: number
  disk_total: number
  disk_used: number
  net_rx: number
  net_tx: number
  total_rx: number
  total_tx: number
  month_rx: number
  month_tx: number
  tcp: number
  udp: number
  procs: number
}

export type Node = {
  id: number
  name: string
  sort: number
  /** The bucket the public page groups by; empty means ungrouped. */
  group: string
  public: boolean
  online: boolean
  last_seen: number
  metrics: Metrics | null
  os: string
  kernel: string
  arch: string
  virt: string
  cpu_name: string
  cpu_cores: number
  mem_total: number
  swap_total: number
  disk_total: number
  agent_version: string
  /** Panel only, decided by the hub: the latest agent release is newer than what
   *  this node reported. Never set for a node that has not connected. */
  agent_old?: boolean
  price: number
  currency: string
  billing_cycle: string
  expires_at: string | null
  traffic_limit: number
  traffic_mode: string
  traffic_reset_day: number
  total_rx: number
  total_tx: number
  month_rx: number
  month_tx: number
  month_start: string
  /** Panel only. */
  hostname?: string
  /** ISO 3166-1 alpha-2, derived from the address the agent connects from. */
  country: string
  ip?: string
  ipv4?: string
  ipv6?: string
  remark?: string
  /** Panel only. Empty for nodes created before the hub retained a copy. */
  token?: string
  /** Panel only. Whether going offline and returning are announced. */
  notify?: boolean
}

export type PingTask = { id: number; name: string; target: string; interval: number; nodes: number[] }

/** Form snapshots must never overwrite fields the user did not edit. */
export function changes<T extends object>(initial: T, values: Partial<T>): Partial<T> {
  return Object.fromEntries(Object.entries(values).filter(([key, value]) => value !== initial[key as keyof T])) as Partial<T>
}

/** One field of the settings form a theme declares under `config` in its `theme.json`. */
export type ConfigField = {
  key: string
  type: "string" | "text" | "number" | "boolean" | "select"
  label?: string
  help?: string
  default: unknown
  options?: { value: string; label?: string }[]
  min?: number
  max?: number
}

/** A heading between fields. It holds no value. */
export type ConfigTitle = { type: "title"; label: string }

const CONFIG_TYPES = ["string", "text", "number", "boolean", "select"]

/** Whether `value` is one the field can hold, the same test a theme applies to what it reads. */
export function fits(field: ConfigField, value: unknown): boolean {
  switch (field.type) {
    case "boolean":
      return typeof value === "boolean"
    case "number":
      return typeof value === "number" && Number.isFinite(value)
        && (field.min === undefined || value >= field.min) && (field.max === undefined || value <= field.max)
    case "select":
      return !!field.options?.some((option) => option.value === value)
    default:
      return typeof value === "string"
  }
}

/**
 * The entries of a theme's form the panel can draw, headings included, in the
 * manifest's order. The manifest is the theme author's, so a malformed entry is
 * left out rather than failing the form: a heading without a label or without a
 * field under it, a duplicate key, an unknown type, a label or help that is not
 * text (rendering one would throw and blank the panel), a select whose options
 * are not all non-empty strings (the dropdown cannot hold an empty value), or a
 * default the field could not hold.
 */
export function configForm(config: unknown): (ConfigField | ConfigTitle)[] {
  if (!Array.isArray(config)) return []
  const seen = new Set<string>()
  const text = (value: unknown) => value === undefined || typeof value === "string"
  const drawable = config.filter((field): field is ConfigField | ConfigTitle => {
    if (typeof field !== "object" || field === null) return false
    if (field.type === "title") return typeof field.label === "string" && field.label !== ""
    const { key, type, label, help, options, min, max } = field
    const ok = typeof key === "string" && key !== "" && !seen.has(key) && CONFIG_TYPES.includes(type)
      && text(label) && text(help)
      && (type !== "select" || (Array.isArray(options)
        && options.every((o) => typeof o?.value === "string" && o.value !== "" && text(o.label))))
      && [min, max].every((bound) => bound === undefined || typeof bound === "number")
      && fits(field, field.default)
    if (ok) seen.add(key)
    return ok
  })
  return drawable.filter((entry, i) => {
    const next = drawable[i + 1]
    return entry.type !== "title" || (next !== undefined && next.type !== "title")
  })
}

/** The entries of the form that hold a value. */
export function configFields(config: unknown): ConfigField[] {
  return configForm(config).filter((entry): entry is ConfigField => entry.type !== "title")
}

/**
 * The form split at its headings. Fields ahead of the first heading form a
 * section of their own; `configForm` has already dropped every heading with no
 * field under it, so every section has something to show.
 */
export function configSections(form: (ConfigField | ConfigTitle)[]): { label: string; fields: ConfigField[] }[] {
  const sections: { label: string; fields: ConfigField[] }[] = []
  for (const entry of form) {
    if (entry.type === "title") sections.push({ label: entry.label, fields: [] })
    else {
      if (!sections.length) sections.push({ label: "通用", fields: [] })
      sections[sections.length - 1].fields.push(entry)
    }
  }
  return sections
}

/** The value each field shows: the saved one while the field can still hold it, else the default. */
export function configValues(fields: ConfigField[], saved: Record<string, unknown>): Record<string, unknown> {
  return Object.fromEntries(fields.map((f) => [f.key, fits(f, saved[f.key]) ? saved[f.key] : f.default]))
}

/**
 * What the panel stores: only the fields that differ from their defaults, so a
 * default the theme changes later reaches every site that never altered it.
 * Keys in `saved` the current form does not declare are kept -- a field a newer
 * version dropped returns with a downgrade; an empty `saved` clears them, which
 * is the only way from the panel to drop a value a theme has since removed.
 */
export function configOverrides(
  fields: ConfigField[],
  saved: Record<string, unknown>,
  values: Record<string, unknown>,
): Record<string, unknown> {
  const next = { ...saved }
  for (const field of fields) {
    if (values[field.key] === field.default) delete next[field.key]
    else next[field.key] = values[field.key]
  }
  return next
}

/** `1.2.3` as numbers, or null for anything else. */
const versionParts = (v: string) => (/^\d+(\.\d+)*$/.test(v) ? v.split(".").map(Number) : null)

/**
 * Whether `current` names an earlier release than `latest`; missing components
 * count as 0. An empty side is never behind: a node that has not reported
 * carries no version, and an unreachable GitHub leaves no latest. A build ahead
 * of the release -- one compiled locally -- is not behind either. Versions that
 * are not `1.2.3` can only be compared for equality.
 *
 * The node list is not filtered with this: the hub decides that per node and
 * sends `agent_old`, so one rule governs both the marker in the node table and
 * the list on the update page. This comparison is for the hub's own version,
 * which only the panel can make.
 */
export function behind(current: string, latest: string): boolean {
  if (!current || !latest) return false
  const a = versionParts(current)
  const b = versionParts(latest)
  if (!a || !b) return current !== latest
  for (let i = 0; i < Math.max(a.length, b.length); i++) {
    if ((a[i] ?? 0) !== (b[i] ?? 0)) return (a[i] ?? 0) < (b[i] ?? 0)
  }
  return false
}

export const GIB = 1024 ** 3

/**
 * The traffic fields as a `TrafficPatch`: GB entered by hand, bytes on the wire,
 * and only the counters actually given a value.
 *
 * An emptied field means the counter is left unchanged rather than set to zero.
 * The patch is entirely `Option` and `set_traffic` COALESCEs, so omitting the key
 * expresses that; sending 0 would clear a lifetime total, the one figure that may
 * never decrease and that nothing can recompute. Zeroing deliberately remains one
 * keystroke away.
 */
export function trafficCorrection(
  pristine: Record<string, string>,
  typed: Record<string, string>,
): Record<string, number> {
  return Object.fromEntries(
    Object.entries(changes(pristine, typed))
      .filter(([, value]) => String(value).trim() !== "")
      .map(([key, value]) => [key, Math.round(Number(value) * GIB)]),
  )
}

/** Private, carrier-grade NAT, loopback or link-local: unreachable from outside the machine's own network. */
function isLocalV4(ip: string): boolean {
  const [a, b] = ip.split(".").map(Number)
  return a === 10 || a === 127 || (a === 172 && b >= 16 && b < 32) || (a === 192 && b === 168) ||
    (a === 100 && b >= 64 && b < 128) || (a === 169 && b === 254)
}

/**
 * The addresses shown for a node. The agent reports its interfaces; `ip` is
 * where its connection arrived from, which the hub canonicalizes to dotted form
 * for IPv4. Behind NAT the interface holds only a private IPv4 while the
 * connection arrives from the public one, so that address leads. `ip` alone is
 * also the fallback for an agent too old to report its interfaces.
 */
export function addresses(node: Pick<Node, "ip" | "ipv4" | "ipv6">): string[] {
  const reported = [node.ipv4, node.ipv6].filter(Boolean) as string[]
  const { ip } = node
  if (!ip) return reported
  if (node.ipv4 && isLocalV4(node.ipv4) && ip.includes(".") && !isLocalV4(ip)) return [ip, ...reported]
  return reported.length ? reported : [ip]
}

/** Installation commands require a TLS origin with a domain, never an IP. */
export function provisioningSite(site: string): string {
  try {
    const u = new URL(site)
    return u.protocol === "https:" && !u.hostname.startsWith("[") && !/^\d+\.\d+\.\d+\.\d+$/.test(u.hostname)
      && u.hostname !== "localhost" && !u.hostname.endsWith(".localhost") && !u.username && !u.password
      && u.pathname === "/" && !u.search && !u.hash ? u.origin : ""
  } catch {
    return ""
  }
}

export class ApiError extends Error {
  status: number
  constructor(status: number, message: string) {
    super(message)
    this.status = status
  }
}

export async function api<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(`/api${path}`, {
    ...init,
    headers: init?.body ? { "content-type": "application/json", ...init?.headers } : init?.headers,
  })
  if (!res.ok) throw new ApiError(res.status, (await res.text()) || res.statusText)
  return res.status === 204 ? (undefined as T) : res.json()
}

/**
 * 4 MiB: the only size a reverse proxy must pass, whatever the file behind it
 * weighs. The hub accepts up to 8 MiB per request, so this can change without
 * touching the server or negotiating first.
 */
const CHUNK = 4 * 1024 * 1024

/**
 * Uploads a file one chunk at a time. There is no upload id: the hub tracks an
 * upload by the length of what it has already written, so a chunk states only
 * where it begins. The last one carries the result.
 */
export async function upload<T>(
  path: string,
  file: File,
  onProgress?: (sent: number) => void,
  signal?: AbortSignal,
): Promise<T> {
  if (file.size === 0) throw new ApiError(400, "文件是空的")
  let last: Response | null = null
  for (let offset = 0; offset < file.size; offset += CHUNK) {
    // A chunk boundary is a genuine stopping point: the hub applies nothing until
    // the last piece lands, and `offset = 0` truncates whatever an abandoned
    // attempt left behind, so aborting here leaves the state unchanged.
    if (signal?.aborted) throw new DOMException("aborted", "AbortError")
    const res = await fetch(`/api${path}?offset=${offset}&total=${file.size}`, {
      method: "POST",
      headers: { "content-type": "application/octet-stream" },
      body: file.slice(offset, offset + CHUNK),
      signal,
    })
    if (!res.ok) {
      // A 413 never reached the hub: the proxy in front answered, and only its
      // own logs record it. The message names the setting responsible.
      throw new ApiError(
        res.status,
        res.status === 413
          ? "反向代理拒收了 4 MiB 的分片，把 nginx 的 client_max_body_size 调到 8m"
          : (await res.text()) || res.statusText,
      )
    }
    last = res
    onProgress?.(Math.min(offset + CHUNK, file.size))
  }
  return last!.json()
}

/**
 * Live node list. Uses the WebSocket the hub pushes every two seconds, falling
 * back to polling if it cannot be established.
 */
export function useNodes() {
  const [nodes, setNodes] = useState<Node[] | null>(null)
  // null until a frame reports it. The panel treats an explicit false as the
  // session no longer being an admin one, so an unanswered first fetch must not
  // read as that; see App.tsx.
  const [admin, setAdmin] = useState<boolean | null>(null)
  // What `agent_old` was decided against, for the tooltip. Replaced by whatever
  // the hub last read, so a hub that cannot reach GitHub sends null and the
  // panel simply has nothing to say.
  const [agentLatest, setAgentLatest] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [reload, setReload] = useState(0)

  useEffect(() => {
    let socket: WebSocket | null = null
    let poll: ReturnType<typeof setInterval> | null = null
    let retry: ReturnType<typeof setTimeout> | null = null
    let closed = false

    const fetchOnce = () =>
      api<{ nodes: Node[]; admin: boolean; agent_latest?: string | null }>("/nodes")
        .then((d) => {
          setNodes(d.nodes)
          setAdmin(d.admin)
          setAgentLatest(d.agent_latest ?? null)
          setError(null)
        })
        .catch((e: Error) => {
          setError(e.message)
          // With the public page switched off, a revoked session receives a 401
          // here and on the stream, so the frame that would report admin=false
          // never arrives and the panel would retain the list it already had.
          if (e instanceof ApiError && e.status === 401) setAdmin(false)
        })

    fetchOnce()

    const url = `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/api/ws`
    // A hub restart closes every stream. Without reconnecting, a page that
    // outlives a deploy would remain on the fallback poll for the rest of its
    // life, refreshing at a fifth of the live rate with no indication.
    const connect = () => {
      try {
        socket = new WebSocket(url)
      } catch {
        poll ??= setInterval(fetchOnce, 5000)
        return
      }
      socket.onmessage = (event) => {
        const frame = JSON.parse(event.data)
        setNodes(frame.nodes)
        setAdmin(frame.admin)
        setAgentLatest(frame.agent_latest ?? null)
        setError(null)
        // The stream has returned; the poll was only covering for it.
        if (poll) {
          clearInterval(poll)
          poll = null
        }
      }
      socket.onerror = () => socket?.close()
      socket.onclose = () => {
        if (closed) return
        poll ??= setInterval(fetchOnce, 5000)
        retry = setTimeout(connect, 5000)
      }
    }
    connect()

    return () => {
      closed = true
      socket?.close()
      if (poll) clearInterval(poll)
      if (retry) clearTimeout(retry)
    }
  }, [reload])

  return { nodes, admin, agentLatest, error, refresh: () => setReload((n) => n + 1) }
}
