/// <reference types="node" />
// The billing form's cycle decision. It is checked here rather than through the
// dialog because that is where the bug was: a stored length this panel cannot
// read used to be rewritten into one the form invented, so saving a price turned
// `weekly` into `yearly`. These are the pure functions the form now calls.
import assert from "node:assert/strict"
import { cycleFields, cycleOk, cyclePatch, cycleValue, money } from "./format.ts"

// A named length and a length without a name are both readable, and a whole
// number of years is shown in years.
assert.deepEqual(cycleFields("monthly"), { unit: "months", count: "1", readable: true })
assert.deepEqual(cycleFields("yearly"), { unit: "years", count: "1", readable: true })
assert.deepEqual(cycleFields("18m"), { unit: "months", count: "18", readable: true })
assert.deepEqual(cycleFields("60m"), { unit: "years", count: "5", readable: true })
assert.deepEqual(cycleFields("once"), { unit: "once", count: "1", readable: true })

// A length this panel cannot read: neutral controls, and the stored value goes
// back untouched unless the admin actually moves them.
const dirty = cycleFields("weekly")
assert.deepEqual(dirty, { unit: "months", count: "1", readable: false })
assert.equal(cyclePatch("weekly", dirty, { unit: "months", count: "1" }), "weekly")
assert.equal(cyclePatch("weekly", dirty, { unit: "months", count: "5" }), "5m")
assert.equal(cyclePatch("", cycleFields(""), { unit: "years", count: "2" }), "24m")

// An untouched pair is passed through in its stored spelling, so a named length
// stays named; changing only the spelling is not a change.
assert.equal(cyclePatch("yearly", cycleFields("yearly"), { unit: "years", count: "1" }), "yearly")
assert.equal(cyclePatch("12m", cycleFields("12m"), { unit: "months", count: "12" }), "12m")
assert.equal(cyclePatch("60m", cycleFields("60m"), { unit: "years", count: "3" }), "36m")

// What the hub will store: a whole number of months or years, from one month to
// the hundred years it caps at, or one-off.
assert.equal(cycleOk("months", "0"), false)
assert.equal(cycleOk("months", ""), false)
assert.equal(cycleOk("months", "1.5"), false)
assert.equal(cycleOk("years", "1.5"), true, "a year and a half is eighteen whole months")
assert.equal(cycleOk("years", "100"), true)
assert.equal(cycleOk("years", "101"), false)
assert.equal(cycleOk("once", ""), true)
assert.equal(cycleValue("years", "5"), "60m")
assert.equal(cycleValue("once", "9"), "once")

// A price puts its currency in front of the number whichever code it is: the
// code used to trail the amount for everything outside a five-entry table, so
// `100.00 HKD` sat in the same column as `$100.00` (upstream issue #76). The five
// keep the symbol they always had; the rest take Intl's.
assert.equal(money(1234.5, "USD"), "$1,234.50")
assert.equal(money(1234.5, "CNY"), "¥1,234.50")
assert.equal(money(1234.5, "EUR"), "€1,234.50")
assert.equal(money(1234.5, "GBP"), "£1,234.50")
assert.equal(money(1000, "JPY"), "¥1,000", "Intl knows yen has no minor unit")
assert.equal(money(1234.5, "HKD"), "HK$1,234.50")
assert.equal(money(1234.5, "TWD"), "NT$1,234.50")
// Intl separates a currency from its amount with U+00A0, and so does the
// fallback below: a plain space would let a price wrap between the two.
assert.equal(money(1234.5, "SGD"), "SGD\u00A01,234.50", "Intl's own prefix for a code it has no symbol for")
assert.equal(money(1234.5, "XYZ"), "XYZ\u00A01,234.50")
assert.equal(money(1234.5, "USDT"), "USDT\u00A01,234.50", "four letters: from before the hub checked")
assert.equal(money(1234.5, ""), "1,234.50")

console.log("billing cycle fields, patch and bounds passed")
