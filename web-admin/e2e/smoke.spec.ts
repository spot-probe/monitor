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

test("安全页：删除一条会话后，确认框必须关闭", async ({ page, baseURL }) => {
  // 先自己造一条可删的会话（另一个 cookie jar 就是另一台「设备」）。
  // 这样用例在全新的库上也能跑，不会因为「没有别的会话」而失败。
  const other = await pwRequest.newContext({ baseURL })
  const login = await other.post("/api/auth/login", { data: { password: process.env.PW_PASSWORD } })
  expect(login.ok()).toBeTruthy()

  await page.goto("/admin/security")

  // 当前设备那一行**没有**删除按钮（右上角的退出登录负责它），所以这个选择器天然只匹配可删的行。
  const trash = page.getByRole("button", { name: "退出该设备" })
  // 会话列表是异步取的，而**裸 count() 不会重试**（toBeVisible/toHaveCount 会）。
  // 先等第一颗出现，再数 —— 否则会在列表还没渲染时数到 0。
  await expect(trash.first()).toBeVisible()
  expect(await trash.count(), "应当至少有一条可删的会话").toBeGreaterThan(0)
  await trash.first().click()

  const dialog = page.getByRole("dialog")
  await expect(dialog).toBeVisible()
  await dialog.getByRole("button", { name: "退出该设备" }).click()

  // ← 这里就是那个 bug：删除成功后 `doomed` 没被清掉，弹窗一直开着。
  await expect(dialog).toBeHidden()
  await expect(page.getByText("已删除会话")).toBeVisible()

  await other.dispose()
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
