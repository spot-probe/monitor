/// <reference types="node" />
// 成本口径的纯算术。抽出来的直接原因：这里发生过两次真实错误，而组件内没法测。
//   ① 一次性买断被当成月付 —— 月开销凭空多出整笔价格；
//   ② 同一次买断的"剩余价值"用 30 天当分母 —— 剩 90 天时算出原价的三倍。
// 下面每条都钉住一个具体数字，而不只是"函数被调用过"。
import assert from "node:assert/strict"
import { cycleDays, cycleMonths, isOneOff, monthlyCost, residualValue } from "./cost.ts"

// ── 一次性买断：两个口径都必须为 0，且判定要能容忍空格与大小写 ──
assert.equal(isOneOff("once"), true)
assert.equal(isOneOff(" ONCE "), true)
assert.equal(isOneOff("monthly"), false)
assert.equal(isOneOff(""), false)

// ① 月开销：¥500 的一次性买断，每月贡献 0（此前是 ¥500）
assert.equal(monthlyCost(500, "once", 1), 0)
// ② 剩余价值：同一台剩 90 天，也是 0（此前 500 / 30 * 90 = ¥1500，原价三倍）
assert.equal(residualValue(500, "once", 90, 1), 0)

// ── 正常周期：月摊与剩余价值都按各自的周期算 ──
assert.equal(monthlyCost(600, "yearly", 1), 50)
assert.equal(monthlyCost(600, "12m", 1), 50)
assert.equal(monthlyCost(300, "quarterly", 1), 100)
assert.equal(cycleMonths("3m"), 3)
// 注释里举的那个例子：600/年、剩 180 天 → ¥295.89
assert.equal(Math.round(residualValue(600, "yearly", 180, 1) * 100) / 100, 295.89)
assert.equal(cycleDays("yearly"), 365)

// ── 汇率只做乘法，不改变口径 ──
assert.equal(monthlyCost(600, "yearly", 7.2), 360)
assert.equal(residualValue(600, "once", 90, 7.2), 0)

// ── "认不出"仍走兜底：宁可少算一项，也不要静默漏掉整台机器 ──
// 注意它与 `once` 的**区别**：认不出的按 1 个月摊，而 `once` 是 0。
assert.equal(cycleMonths("weekly"), 1)
assert.equal(cycleDays("weekly"), 30)
assert.equal(monthlyCost(100, "weekly", 1), 100)
assert.equal(Math.round(residualValue(100, "weekly", 15, 1) * 100) / 100, 50)

console.log("cost: ok")
