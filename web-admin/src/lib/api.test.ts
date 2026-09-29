/// <reference types="node" />
import assert from "node:assert/strict"
import { addresses, behind, changes, configFields, configForm, configOverrides, configSections, configValues, fits, GIB, provisioningSite, trafficCorrection } from "./api.ts"

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
// NAT: the public address the connection arrived from leads the private interface.
assert.deepEqual(addresses({ ip: "203.0.113.7", ipv4: "10.10.2.250", ipv6: "2001:db8::1" }), ["203.0.113.7", "10.10.2.250", "2001:db8::1"])
assert.deepEqual(addresses({ ip: "203.0.113.7", ipv4: "100.64.0.9" }), ["203.0.113.7", "100.64.0.9"])
// Hub on the same network, or on the same machine: the connection says nothing more.
assert.deepEqual(addresses({ ip: "192.168.1.2", ipv4: "192.168.1.5" }), ["192.168.1.5"])
assert.deepEqual(addresses({ ip: "127.0.0.1", ipv4: "172.16.0.5" }), ["172.16.0.5"])
// A public interface, or a connection over IPv6, stays as reported.
assert.deepEqual(addresses({ ip: "198.51.100.1", ipv4: "203.0.113.7" }), ["203.0.113.7"])
assert.deepEqual(addresses({ ip: "2001:db8::2", ipv4: "10.0.0.2", ipv6: "2001:db8::2" }), ["10.0.0.2", "2001:db8::2"])
assert.deepEqual(addresses({ ip: "203.0.113.7" }), ["203.0.113.7"])
// Without a recorded connection the list holds only what the agent reported.
assert.deepEqual(addresses({ ipv4: "10.0.0.2" }), ["10.0.0.2"])
assert.deepEqual(addresses({}), [])

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

console.log("partial edits, traffic corrections, provisioning and address checks and theme-config checks passed")
