import { defineConfig } from "@playwright/test"

/**
 * 跑在**真实**的预览上（面板 + hub + rust-embed 一起），而不是 jsdom。
 *
 * 这是有意的：这个仓库已经有一层纯函数测试（`npm test`，node 脚本），它抓不到我们今天犯的两类错 ——
 * 一类是「设了状态却没清」（确认框不关），一类是**真实布局**（输入框被 flex 压短）。
 * 后者在 jsdom 里根本不存在：jsdom 没有布局引擎，宽高永远是 0。
 *
 * 地址与口令都从环境变量取（口令不进仓库）：
 *   PW_BASE_URL   默认 http://127.0.0.1:28190
 *   PW_PASSWORD   本地预览的登录口令；CI 里放 secret
 */
const baseURL = process.env.PW_BASE_URL ?? "http://127.0.0.1:28190"

export default defineConfig({
  testDir: "./e2e",
  // 会话表是有状态的（删一条少一条），所以串行跑，一个 worker。
  fullyParallel: false,
  workers: 1,
  reporter: [["list"]],
  use: {
    baseURL,
    // 桌面视口是前提：侧栏的组名在 md 以下不显示（窄屏时它们是「挤在按钮之间的词」）。
    viewport: { width: 1280, height: 900 },
    trace: "retain-on-failure",
  },
  projects: [
    { name: "auth", testMatch: /auth\.setup\.ts/ },
    {
      name: "smoke",
      dependencies: ["auth"],
      testMatch: /.*\.spec\.ts/,
      use: { storageState: "e2e/.auth/state.json" },
    },
  ],
})
