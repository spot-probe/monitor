/// <reference types="node" />
// 面向用户的文案里不能出现 Markdown 的加粗语法（`**…**`）。
//
// 这个坑很小但很刺眼：它只在**悬停时**才出现，所以平时看页面永远发现不了 ——
// 维护者就是这样在 tooltip 里看到四个星号的。强调应当用 `<span className="font-medium">`。
//
// 检查范围：`<Help>…</Help>` 的内容，以及 `title=` / `aria-label=` 这两个属性的值
// （它们都会原样显示给用户）。源码注释里的 `**` 不受影响，那是这个仓库的书写习惯。
import assert from "node:assert/strict"
import { readdirSync, readFileSync } from "node:fs"
import { join } from "node:path"

const dir = new URL("../components/", import.meta.url)
const files = readdirSync(dir).filter((f) => f.endsWith(".tsx"))

const offenders: string[] = []

for (const f of files) {
  const src = readFileSync(join(dir.pathname, f), "utf8")
  // ① Help 的内容
  for (const m of src.matchAll(/<Help>([\s\S]*?)<\/Help>/g)) {
    if (m[1].includes("**")) offenders.push(`${f} · <Help> 里出现 **`)
  }
  // ② title / aria-label 的值（单行或跨行都算，取引号之间的内容）
  for (const m of src.matchAll(/\b(?:title|aria-label)=(?:"([^"]*)"|\{`([^`]*)`\})/g)) {
    const value = m[1] ?? m[2] ?? ""
    if (value.includes("**")) offenders.push(`${f} · ${m[0].slice(0, 40)}… 里出现 **`)
  }
}

assert.deepEqual(offenders, [], `面向用户的文案里不该出现 **：\n${offenders.join("\n")}`)
console.log("copy: no markdown bold in user-visible text")
