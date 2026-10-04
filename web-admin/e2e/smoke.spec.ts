import { expect, request as pwRequest, test } from "@playwright/test"

/**
 * 每条用例都对应一件我们**真的发过**的东西，而不是泛泛的冒烟。
 * 注释里写清「它在防什么」，免得以后有人觉得可以删。
 */

test("侧栏分成三组，组名与顺序固定", async ({ page }) => {
  await page.goto("/admin/nodes")
  const nav = page.locator("nav").first()

  // 防的是：分组被合并回两组、或名字被改回去（维护者为此专门定过名）。
  const groups = ["资源管理", "站点配置", "系统运维"]
  const ys: number[] = []
  for (const g of groups) {
    const label = nav.getByText(g, { exact: true })
    await expect(label).toBeVisible()
    ys.push((await label.boundingBox())!.y)
  }
  // 顺序也要对：光有这三个名字，排乱了也算过。
  expect(ys[0]).toBeLessThan(ys[1])
  expect(ys[1]).toBeLessThan(ys[2])

  // 「设置」属于站点配置、「通知」属于系统运维 —— 维护者在 A/B 两个方案里选了 A。
  const navText = (await nav.innerText()).replace(/\s+/g, " ")
  expect(navText.indexOf("设置")).toBeLessThan(navText.indexOf("系统运维"))
})

test("设置页：三个输入框的宽度按内容分档（320 / 160 / 672）", async ({ page }) => {
  await page.goto("/admin/settings")
  const inputs = page.locator('[data-slot="input"]')
  await expect(inputs).toHaveCount(3)

  // 防的是两类错：① 又改回「三个统一宽度」；② 限宽只加在容器上而控件没补 w-full，
  // 于是输入框缩成内容的宽度（这个只有真实布局才看得出来，jsdom 抓不到）。
  const want = [320, 160, 672]
  for (let i = 0; i < want.length; i++) {
    const box = await inputs.nth(i).boundingBox()
    expect(box, `第 ${i + 1} 个输入框应当可见`).toBeTruthy()
    expect(Math.round(box!.width), `第 ${i + 1} 个输入框宽度`).toBe(want[i])
  }
})

test("节点页：没有落后节点时不出现「待升级」按钮", async ({ page }) => {
  // 预览里 100 台节点的 agent_version 都是空串，而空串是**不予评判**的（不是 0.0.0），
  // 所以这里断言的是 C8 的一条规则：按钮只在真有待升级节点时出现。
  await page.goto("/admin/nodes")
  await expect(page.getByRole("button", { name: /待升级/ })).toHaveCount(0)
})

test("总览：默认落地页、KPI、续费日历", async ({ page }) => {
  // 只断言「结构」，不断言数值：CI 用全新库，KPI 全是 0、日历上一个到期日也没有。
  //
  // 这条用例已经抓到过一个真 bug：我加「近期事项」时把计算块插在了 `todayKey` **之前**，
  // 于是 TDZ 崩溃（`Cannot access 'S' before initialization`），整个页面白屏 ——
  // 而构建、lint、类型检查全绿。写它的时候我先误判成选择器问题，试了四种写法；
  // 真正的原因是页面根本没渲染，最后靠 pageerror 抓到。
  await page.goto("/admin")
  // 先等应用外壳：`toHaveURL` 立刻成立，不等应用启动。
  await expect(page.locator("nav").first()).toBeVisible({ timeout: 20_000 })
  await expect(page).toHaveURL(/\/admin\/overview$/)   // /admin 必须被规范化到总览
  await expect(page.getByText("节点总数")).toBeVisible()
  await expect(page.getByText("续费日历")).toBeVisible()

  // 日历的结构：日期格都带 aria-label="YYYY-MM-DD"，一个月 28–31 个。
  const days = page.getByRole("button", { name: /^\d{4}-\d{2}-\d{2}/ })
  const n = await days.count()
  expect(n, "日历应有 28–31 个日期格，实际 " + n).toBeGreaterThanOrEqual(28)
  expect(n).toBeLessThanOrEqual(31)
  await page.getByRole("button", { name: "下月" }).click()
  await expect(page.getByText(/\d+ 月 · 续费日历/)).toBeVisible()
})
