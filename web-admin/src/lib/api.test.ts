/// <reference types="node" />
import assert from "node:assert/strict"
import { addresses, behind, changes, configFields, configForm, configOverrides, configSections, configValues, fits, GIB, isPublic, matchingGroups, provisioningSite, trafficCorrection } from "./api.ts"

assert.deepEqual(changes({ public: true, price: 5 }, { price: 20 }), { price: 20 })
assert.deepEqual(changes({ total_rx: "100", month_tx: "2" }, { total_rx: "100", month_tx: "3" }), { month_tx: "3" })
assert.deepEqual(changes({ expires_at: "2030-01-01" as string | null }, { expires_at: null }), { expires_at: null })
assert.equal(provisioningSite("https://monitor.example.com:8443/"), "https://monitor.example.com:8443")
for (const site of ["http://monitor.example.com", "https://127.0.0.1", "https://[::1]", "https://2130706433", "https://0x7f000001", "https://localhost", "https://user@monitor.example.com", "https://monitor.example.com/path"]) {
  assert.equal(provisioningSite(site), "", site)
}
// An emptied traffic field means the counter is not to be corrected. Sent as 0
// it would clear a lifetime total, which must never decrease.
const shown = { total_rx: "1.5", total_tx: "2", month_rx: "0.25", month_tx: "1" }
assert.deepEqual(trafficCorrection(shown, { ...shown, total_rx: "" }), {})
assert.deepEqual(trafficCorrection(shown, { ...shown, total_rx: "   " }), {})
assert.deepEqual(trafficCorrection(shown, { ...shown, total_rx: "0" }), { total_rx: 0 })
assert.deepEqual(trafficCorrection(shown, { ...shown, total_tx: "3" }), { total_tx: 3 * GIB })
assert.deepEqual(trafficCorrection(shown, shown), {})
// One address per family, marked with where it came from.
const rows = (node: Parameters<typeof addresses>[0]) => addresses(node).map((a) => `${a.address} ${a.source}`)
// A public interface is the machine; a different exit in front of it is a proxy and stays out.
assert.deepEqual(rows({ ip: "2001:db8::2", ipv4: "203.0.113.7", ipv6: "2001:db8::2" }), ["203.0.113.7 interface", "2001:db8::2 interface"])
assert.deepEqual(rows({ ip: "198.51.100.1", ipv4: "203.0.113.7" }), ["203.0.113.7 interface"])
// NAT: the exit replaces the private interface address, which nobody outside can use.
assert.deepEqual(rows({ ip: "203.0.113.7", ipv4: "10.10.2.250" }), ["203.0.113.7 exit"])
assert.deepEqual(rows({ ip: "203.0.113.7", ipv4: "100.64.0.9" }), ["203.0.113.7 exit"])
// An LXC guest behind NAT with a public /128, reached over v4 by a current agent...
assert.deepEqual(rows({ ip: "203.0.113.7", ipv4: "10.10.1.5", ipv6: "2401:b60:1c::5" }), ["203.0.113.7 exit", "2401:b60:1c::5 interface"])
// ...and over v6 by an older one reporting the ULA ahead of it.
assert.deepEqual(rows({ ip: "2401:b60:1c::5", ipv4: "10.10.1.5", ipv6: "fd42:43af::1" }), ["2401:b60:1c::5 exit"])
// Behind a transparent proxy the exit is the proxy's; the home line can only be set by hand.
const home = { ip: "198.51.100.77", ipv4: "192.168.1.5", ipv6: "2409:8a1e::5" }
assert.deepEqual(rows(home), ["198.51.100.77 exit", "2409:8a1e::5 interface"])
assert.deepEqual(rows({ ...home, ipv4_pin: "203.0.113.50" }), ["203.0.113.50 manual", "2409:8a1e::5 interface"])
// A pin wins over a public interface too, and may name a private address for use on the LAN.
assert.deepEqual(rows({ ipv4: "203.0.113.7", ipv6: "2001:db8::5", ipv6_pin: "2001:db8::9" }), ["203.0.113.7 interface", "2001:db8::9 manual"])
assert.deepEqual(rows({ ip: "203.0.113.7", ipv4: "10.0.0.2", ipv4_pin: "10.0.0.2" }), ["10.0.0.2 manual"])
// No interface in the exit's family: a translator (NAT64, WARP) that does not lead to the machine.
assert.deepEqual(rows({ ip: "104.28.1.1", ipv6: "2001:db8::5" }), ["2001:db8::5 interface"])
// Nothing public anywhere: hub and node share a network, and the private addresses are all there is.
assert.deepEqual(rows({ ip: "192.168.1.2", ipv4: "192.168.1.5" }), ["192.168.1.5 interface"])
assert.deepEqual(rows({ ip: "fd00::2", ipv4: "10.0.0.2", ipv6: "fd00::5" }), ["10.0.0.2 interface", "fd00::5 interface"])
assert.deepEqual(rows({ ip: "198.18.0.1", ipv4: "192.168.1.5" }), ["192.168.1.5 interface"], "a TUN proxy's fake-IP range is not public")
// With no interface reported the connection is all there is, and nothing says it is not the machine's own.
assert.deepEqual(rows({ ip: "203.0.113.7" }), ["203.0.113.7 connection"])
assert.deepEqual(rows({ ipv4: "10.0.0.2" }), ["10.0.0.2 interface"])
assert.deepEqual(rows({}), [])
for (const ip of ["10.0.0.1", "172.31.0.1", "192.168.0.1", "100.64.0.1", "127.0.0.1", "169.254.0.1", "0.0.0.1", "192.0.0.4", "198.19.0.1", "224.0.0.1", "fd42::1", "fe80::1", "::1"]) {
  assert.ok(!isPublic(ip), ip)
}
for (const ip of ["1.1.1.1", "100.128.0.1", "172.32.0.1", "192.0.1.1", "198.20.0.1", "2401:b60:1c::5", "3fff::1"]) {
  assert.ok(isPublic(ip), ip)
}

// A theme's form: malformed fields drop out one by one, a saved value the field
// can no longer hold shows the default, and only changes from a default are stored.
const entries = [
  { type: "title", label: "外观" },
  { key: "notice", type: "text", default: "" },
  { key: "layout", type: "select", default: "grid", options: [{ value: "grid" }, { value: "table" }] },
  { key: "refresh", type: "number", default: 5, min: 1, max: 60 },
  { key: "dark", type: "boolean", default: false },
  { key: "notice", type: "string", default: "duplicate" },
  { key: "odd", type: "color", default: "#000" },
  { key: "bare", type: "select", default: "a" },
  { key: "blank", type: "select", default: "", options: [{ value: "" }, { value: "a" }] },
  { key: "wrong", type: "boolean", default: "yes" },
  { key: "range", type: "number", default: 0, min: 1 },
  { key: "i18n", type: "string", default: "", label: { zh: "公告", en: "Notice" } },
  { key: "hinted", type: "boolean", default: true, help: 1 },
  { key: "labelled", type: "select", default: "a", options: [{ value: "a", label: { zh: "甲" } }] },
  "not a field",
  { type: "title" },
  { type: "title", label: "空" },
  { type: "title", label: "末尾" },
]
assert.deepEqual(configForm(entries).map((f) => (f.type === "title" ? `# ${f.label}` : f.key)), ["# 外观", "notice", "layout", "refresh", "dark"])
const form = configFields(entries)
assert.deepEqual(form.map((f) => f.key), ["notice", "layout", "refresh", "dark"])
assert.deepEqual(configFields({ notice: "x" }), [])
const savedConfig = { layout: "cards", refresh: 10, legacy: 1 }
const initial = configValues(form, savedConfig)
assert.deepEqual(initial, { notice: "", layout: "grid", refresh: 10, dark: false })
assert.equal(fits(form[2], 61), false)
assert.deepEqual(configOverrides(form, savedConfig, { ...initial, refresh: 5, dark: true }), { legacy: 1, dark: true })
// 恢复默认 builds on nothing, so undeclared keys go with the overrides.
assert.deepEqual(configOverrides(form, {}, Object.fromEntries(form.map((f) => [f.key, f.default]))), {})

// Headings split the form; leading fields get a section, empty headings none.
assert.deepEqual(
  configSections(configForm([{ key: "a", type: "string", default: "" }, { type: "title", label: "空" }, { type: "title", label: "外观" }, { key: "b", type: "boolean", default: true }]))
    .map((s) => [s.label, s.fields.map((f) => f.key)]),
  [["通用", ["a"]], ["外观", ["b"]]],
)

// A version is behind only when it is older: a locally built one ahead of the
// release is not, and neither is an unknown one.
assert.equal(behind("1.8.0", "1.9.0"), true)
assert.equal(behind("1.9.0", "1.8.0"), false)
assert.equal(behind("1.9.0", "1.9.0"), false)
assert.equal(behind("1.10.0", "1.9.0"), false, "numeric, not lexicographic")
assert.equal(behind("1.8", "1.8.1"), true, "a missing component counts as 0")
assert.equal(behind("", "1.9.0"), false, "nothing reported is not behind")
assert.equal(behind("1.8.0", ""), false, "an unreachable GitHub leaves no latest")
assert.equal(behind("dev-abc", "1.9.0"), true, "not 1.2.3 can only be compared for equality")

// The opt-in group list (`--group-dropdown`). A field holding a known name is
// offering to change it, so it lists every group; anything else is a new name
// being typed, and filters.
const NAMES = ["建站", "入口集群", "落地"]
assert.deepEqual(matchingGroups(NAMES, ""), NAMES)
assert.deepEqual(matchingGroups(NAMES, "建站"), NAMES, "a chosen name still lists the alternatives")
assert.deepEqual(matchingGroups(NAMES, "  建站  "), NAMES, "the field's own spaces are not part of the name")
assert.deepEqual(matchingGroups(NAMES, "落"), ["落地"])
assert.deepEqual(matchingGroups(NAMES, "站"), ["建站"])
assert.deepEqual(matchingGroups(NAMES, "不存在的"), [])
assert.deepEqual(matchingGroups(["Edge", "edge-2"], "eDgE"), ["Edge", "edge-2"], "case-insensitive")
assert.deepEqual(matchingGroups(["Edge", "edge-2"], "edge-2"), ["Edge", "edge-2"])
assert.deepEqual(matchingGroups([], "x"), [])

console.log("partial edits, traffic corrections, provisioning and address checks and theme-config checks passed")
