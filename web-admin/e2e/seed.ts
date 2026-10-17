import { expect, test as seed } from "@playwright/test"

/**
 * 造几台**有数据**的节点。
 *
 * 为什么需要它：CI 一直用全新库跑 e2e，于是所有用例都只看过"0 台节点、0 流量、没有到期日"
 * 这一种页面 —— 排序、按额度标红、`—` 与 0 的区别、真实数值的格式化，全都测不到。
 *
 * 幂等：先读 `/api/nodes`，同名就跳过。本地预览里已经有别的节点，各跑各的。
 */
const NODES = [
  {
    name: "e2e-seed-a",
    group: "e2e",
    traffic_limit: 200 * 1024 ** 3,
    price: 128,
    currency: "CNY",
    billing_cycle: "monthly",
    expires_at: "2027-01-15",
  },
  {
    name: "e2e-seed-b",
    group: "e2e",
    traffic_limit: 500 * 1024 ** 3,
    price: 20,
    currency: "USD",
    billing_cycle: "yearly",
    expires_at: "2026-12-01",
  },
  {
    // 免费、无限流量、没有到期日 —— 三种"空"必须与"0"分得开。
    name: "e2e-seed-c",
    group: "e2e",
    traffic_limit: 0,
    price: 0,
    currency: "CNY",
    billing_cycle: "monthly",
    expires_at: null as string | null,
  },
]

seed("造几台有数据的节点", async ({ request }) => {
  const listed = await request.get("/api/nodes")
  expect(listed.ok(), `读节点列表失败 HTTP ${listed.status()}`).toBeTruthy()
  const body = (await listed.json()) as { nodes?: { name: string }[] }
  const have = new Set((body.nodes ?? []).map((n) => n.name))

  for (const node of NODES) {
    if (have.has(node.name)) continue
    const created = await request.post("/api/nodes", { data: node })
    expect(
      created.ok(),
      `造节点 ${node.name} 失败 HTTP ${created.status()}：${await created.text()}`,
    ).toBeTruthy()
  }
})
