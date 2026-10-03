import { test as setup, expect } from "@playwright/test"

const PASSWORD = process.env.PW_PASSWORD

/**
 * 登录一次并把会话存下来，后面所有用例复用。
 *
 * 用 `page.request` 而不是 `request`：前者与页面**共用同一个 cookie jar**，登录拿到的会话才会
 * 落到待会儿保存的 storageState 里。
 */
setup("登录并保存会话", async ({ page }) => {
  expect(PASSWORD, "需要 PW_PASSWORD（预览的登录口令）").toBeTruthy()
  const res = await page.request.post("/api/auth/login", { data: { password: PASSWORD } })
  expect(res.ok(), `登录失败：HTTP ${res.status()}`).toBeTruthy()

  // 确认真的进了面板，而不是停在登录页 —— 否则后面所有用例都会以「找不到元素」失败，看不出根因。
  await page.goto("/admin/nodes")
  await expect(page.locator("nav").first()).toBeVisible()

  await page.context().storageState({ path: "e2e/.auth/state.json" })
})
