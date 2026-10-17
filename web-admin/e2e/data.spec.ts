import { expect, test } from "@playwright/test"

/**
 * 这些节点由 `seed.ts` 造出来（auth 之后、smoke 之前跑）。它们的意义是让面板渲染一次
 * **非零**数据：此前 CI 与本地都只跑过全新库，所有数值都是 0。
 *
 * 断言用正则而不是精确相等：格式化函数（`money` / `bytes`）在别处有自己的用例，
 * 这里要守住的是"数值真的从 hub 走到了页面上"。
 */
test("种下的节点渲染出非零的价格、流量额度与到期日", async ({ page }) => {
  await page.goto("/admin/nodes")
  const row = page.locator("tr", { hasText: "e2e-seed-a" })
  await expect(row).toBeVisible()

  // 价格：¥128 必须出现在这一行（不是"免费"、也不是空）。
  await expect(row).toContainText(/¥128/)
  // 流量：额度那一侧是 200 GB（已用是 0，因为还没有 agent 上报）。
  await expect(row).toContainText(/200 GB/)
  // 到期日：有日期的节点必须显示日期。
  await expect(row).toContainText(/2027-01-15/)
})

test("免费且无到期日的节点显示成「免费」而不是 0", async ({ page }) => {
  await page.goto("/admin/nodes")
  const row = page.locator("tr", { hasText: "e2e-seed-c" })
  await expect(row).toBeVisible()
  await expect(row).toContainText(/免费/)
  // 无限额度在流量列是 ∞，不是 "0 B / 0 B"。
  await expect(row).toContainText(/∞/)
})

test("节点表不再是空的：至少三行（就是种子那三台）", async ({ page }) => {
  await page.goto("/admin/nodes")
  await expect(page.locator("table")).toBeVisible()
  // ⚠️ 用"至少三行"而不是精确行数：本地预览里有上百台，CI 里是种子那三台。
  // 这条守的是"表格真的渲染了数据"，而不是"KPI 某个数字"—— 后者要给 KPI 卡片写一个
  // 依赖 DOM 结构的定位器，第一次尝试抓到的只是一个只含标签的包裹层。
  const rows = page.locator("tbody tr")
  await expect.poll(async () => rows.count(), { timeout: 10_000 }).toBeGreaterThanOrEqual(3)
})
