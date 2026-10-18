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

test("节点页：落后节点与「待升级」按钮同进同退", async ({ page }) => {
  // ⚠️ 原来断言的是"**永远不**出现"，而它的对象是整页、整页取决于库里有什么：
  // CI 是全新库（种子节点没有 agent_version，"不予评判"），本地预览里却可能真有落后节点
  // —— 于是这条会在预览上**合法地**红，此前只能在注释里认了。
  //
  // 现在读一次 `/api/nodes` 拿事实，两种数据下都对得上：有落后节点就必须有按钮，
  // 没有就必须没有。它测的还是同一件事（按钮别乱出现），只是不再依赖环境。
  const res = await page.request.get("/api/nodes")
  const { nodes } = (await res.json()) as { nodes: { agent_old?: boolean }[] }
  const behind = nodes.some((n) => n.agent_old)
  await page.goto("/admin/nodes")
  const button = page.getByRole("button", { name: /待升级/ })
  if (behind) await expect(button).toBeVisible()
  else await expect(button).toHaveCount(0)
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

  // 日历的结构：数**日期格里显示的数字**（span.tnum），一个月 28–31 个。
  // **不要数按钮** —— 只有「当天有到期节点」的格子才是按钮；全新库上一个都没有，
  // 按按钮数会得到 0。日期格本身与「能不能点」是两件事。
  const grid = page.locator("div.grid.grid-cols-7").first()
  const n = await grid.locator("span.tnum").count()
  expect(n, "日历应有 28–31 个日期格，实际 " + n).toBeGreaterThanOrEqual(28)
  expect(n).toBeLessThanOrEqual(31)
  // 月份导航：两条切换 + 中间那个「回到本月」的月份标签。
  await expect(page.getByRole("button", { name: "上月" })).toBeVisible()
  await expect(page.getByRole("button", { name: "下月" })).toBeVisible()
  await expect(page.getByTitle("回到本月")).toBeVisible()
  await page.getByRole("button", { name: "下月" }).click()
})

test("实时通道真的连上了 WebSocket，而不是悄悄退回 5 秒轮询", async ({ page }) => {
  // 防的是：`/api/ws` 坏掉时面板会**静默**退回 5 秒轮询（`lib/api.ts` 的兜底），页面照常显示、
  // 其它用例全绿 —— 于是「每 2 秒推送」这条能力实际上无人验证。
  //
  // 必须在 `goto` **之前**挂监听：连接是在应用启动时就建立的，晚一步就等不到那个事件。
  const opened = page.waitForEvent("websocket")
  await page.goto("/admin/nodes")
  const ws = await opened
  expect(new URL(ws.url()).pathname, "面板应当连到 /api/ws").toBe("/api/ws")

  // 光"握手成功"不够：一个连上就断、或者一直不推帧的实现也能过上面那条。
  // 这里等第一帧真的收到 —— 面板靠它刷新节点状态。
  const frame = await ws.waitForEvent("framereceived")
  expect(frame.payload.length, "第一帧不应当是空的").toBeGreaterThan(0)
})

test("节点页不再有实时资源列", async ({ page }) => {
  // 这一列被**有意删掉**（维护者：CPU/内存是瞬时值，列表里没有决策价值，还挤窄了价格与到期）。
  // 断言它不在，比删掉用例强：哪天有人"顺手加回来"，这里会红。
  await page.goto("/admin/nodes")
  await expect(page.getByRole("columnheader", { name: "名称" })).toBeVisible()
  await expect(page.getByRole("columnheader", { name: "资源" })).toHaveCount(0)
})
