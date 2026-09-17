# monitor

## 特性

- 实时监控：秒级实时数据展示
- 轻量高效：Rust 语言构建，低资源占用，极简高效
- 自托管：完全掌控数据隐私，部署简单
- 通知：节点掉线、流量、到期与登录，推送到 Telegram 或自定义 Webhook

## 组成

| 仓库 | 说明 |
|---|---|
| [monitor](https://github.com/spot-probe/monitor) | hub：后台、API、公开页宿主 |
| [agent](https://github.com/spot-probe/agent) | Linux agent |
| [monitor-theme-default](https://github.com/spot-probe/monitor-theme-default) | 内置默认主题 |

> fork 自 [monitor-probe](https://github.com/monitor-probe)，在其上独立演进：公开页改为浅色语义
> 配色、节点可分组、后台沿用同一套调色板。安装脚本与 hub 取的都是本组织的 release，
> 不是上游的。

```
agent (Linux)  ──WebSocket / JSON-RPC 2.0──▶  hub (axum + SQLite)  ──▶  后台 + 状态页
```
