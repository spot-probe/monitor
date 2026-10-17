//! 只读的 Telegram 命令：**长轮询** `getUpdates`，不要求 hub 有公网地址。
//!
//! 为什么不走 webhook：自建 hub 常常只有出网、没有入网，而 `getUpdates` 只要求能出网 ——
//! 它本来就要出网去取发布物与主题。代价是一条常驻的长连接，以及**重启后要先把积压的更新
//! 丢掉**：一条两小时前的命令不该在重启时被执行（见 `drained`）。
//!
//! 第一批只做**只读**命令：`/status` `/nodes` `/node` `/traffic` `/probe` `/help`。
//! 写操作（静音、重新探测、升级）留到下一批 —— 它们需要二次确认，而那是另一套交互。
//!
//! ⚠️ 只认**配置里的那个会话**：消息里的 `chat.id` 与设置不同就当作没看见。bot token 写在
//! 设置里，而设置页只对管理员开放；这里不再引入第二套权限模型。

use std::time::Duration;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use tracing::debug;

use crate::{App, Shared};

/// 配置缺失或请求失败时的退避。
const IDLE: Duration = Duration::from_secs(5);
/// `getUpdates` 的长轮询秒数。Telegram 允许到 50；取 25 是为了不撞上常见的 30 秒中间盒超时。
const LONG_POLL: u64 = 25;
/// 一条命令最多回多少台节点 —— 再多就该去面板看了（Telegram 单条也有 4096 字上限）。
const MAX_LINES: usize = 25;

/// Telegram API 的根。生产里没有理由改它 —— 存在的意义是**能用本地假服务器把轮询、
/// 会话过滤与回复跑一遍**：这三条只能在进程外验证，而单元测试碰不到 HTTP。
fn api_base() -> String {
    std::env::var("MONITOR_TELEGRAM_API").unwrap_or_else(|_| "https://api.telegram.org".to_string())
}

const HELP: &str = "可用命令：
/status —— 集群概览
/nodes —— 节点列表（在线、版本、CPU/内存）
/node <名字> —— 单台详情
/traffic —— 今日与本月流量
/probe —— 各节点最近一次流媒体探测";

/// 轮询循环。设置里没有 token 或 chat 时什么都不做（连日志都不打，免得每 5 秒刷屏）。
pub async fn watch(app: Shared) {
    let mut offset: i64 = 0;
    // 第一次成功轮询只推进 offset、不执行命令：hub 重启不该重放积压的旧消息。
    let mut drained = false;
    loop {
        let (Some(token), Some(chat)) =
            (setting(&app, "notify_telegram_token"), setting(&app, "notify_telegram_chat"))
        else {
            tokio::time::sleep(IDLE).await;
            continue;
        };
        let timeout = if drained { LONG_POLL } else { 0 };
        match updates(&app, &token, offset, timeout).await {
            Ok(list) => {
                for update in list {
                    if let Some(id) = update.get("update_id").and_then(Value::as_i64) {
                        offset = offset.max(id + 1);
                    }
                    if drained {
                        handle(&app, &token, &chat, &update).await;
                    }
                }
                drained = true;
            }
            Err(e) => {
                // 不带 URL：Telegram 的地址里有 bot token。
                debug!("telegram getUpdates: {e:#}");
                tokio::time::sleep(IDLE).await;
            }
        }
    }
}

fn setting(app: &App, key: &str) -> Option<String> {
    app.db.get(key).filter(|v| !v.is_empty())
}

async fn updates(app: &App, token: &str, offset: i64, timeout: u64) -> Result<Vec<Value>> {
    let url = format!("{}/bot{token}/getUpdates", api_base());
    let response = app
        .http
        .get(&url)
        .query(&[("offset", offset.to_string()), ("timeout", timeout.to_string())])
        .timeout(Duration::from_secs(timeout + 20))
        // ⚠️ `without_url()`：`reqwest::Error` 的 Display 带着完整地址，那里面有 bot token，
        // 而这条错误会进日志。notify.rs 里同一条规矩。
        .send()
        .await
        .map_err(|e| anyhow!("{}", anyhow::Error::from(e.without_url())))?;
    let response =
        response.error_for_status().map_err(|e| anyhow!("{}", anyhow::Error::from(e.without_url())))?;
    let body: Value =
        response.json().await.map_err(|e| anyhow!("{}", anyhow::Error::from(e.without_url())))?;
    Ok(body.get("result").and_then(Value::as_array).cloned().unwrap_or_default())
}

async fn handle(app: &App, token: &str, chat: &str, update: &Value) {
    let Some(message) = update.get("message") else { return };
    // 只认配置里的会话：别的群或私聊发来的命令当没看见。
    let from = message.pointer("/chat/id").and_then(Value::as_i64).map(|v| v.to_string());
    if from.as_deref() != Some(chat) {
        return;
    }
    let Some(text) = message.get("text").and_then(Value::as_str) else { return };
    let Some((cmd, arg)) = command(text) else { return };
    let frame = frame(app);
    let reply = match cmd {
        "start" | "help" => HELP.to_string(),
        "status" => status_text(&frame),
        "nodes" => nodes_text(&frame),
        "node" => node_text(&frame, arg),
        "traffic" => traffic_text(&frame),
        "probe" => probe_text(&frame),
        // 不认识的命令**不回**：两个 bot 在同一个群里时，互相回"不认识"会打起来。
        _ => return,
    };
    if let Err(e) = send(app, token, chat, &reply).await {
        debug!("telegram sendMessage: {e:#}");
    }
}

async fn send(app: &App, token: &str, chat: &str, text: &str) -> Result<()> {
    let url = format!("{}/bot{token}/sendMessage", api_base());
    let response = app
        .http
        .post(&url)
        // 纯文本：节点名里出现标点不会让整条消息被拒（与 notify.rs 同一个理由）。
        .json(&json!({ "chat_id": chat, "text": text }))
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| anyhow!("{}", anyhow::Error::from(e.without_url())))?;
    response.error_for_status().map_err(|e| anyhow!("{}", anyhow::Error::from(e.without_url())))?;
    Ok(())
}

/// `/nodes`、`/node hk-1`、`/nodes@MyBot`。参数里允许有空格。
fn command(text: &str) -> Option<(&str, &str)> {
    let rest = text.trim().strip_prefix('/')?;
    let (cmd, arg) = match rest.split_once(char::is_whitespace) {
        Some((cmd, arg)) => (cmd, arg.trim()),
        None => (rest, ""),
    };
    // 群里多个 bot 时 Telegram 会带上 `@名字`。
    Some((cmd.split('@').next().unwrap_or(cmd), arg))
}

/// 面板用的那份管理快照 —— 与面板同一个数据源，避免第二套查询各写一遍。
fn frame(app: &App) -> Value {
    serde_json::from_str(crate::api::admin_frame(app).as_str()).unwrap_or(Value::Null)
}

fn nodes_of(frame: &Value) -> Vec<Value> {
    frame.get("nodes").and_then(Value::as_array).cloned().unwrap_or_default()
}

fn status_text(frame: &Value) -> String {
    let nodes = nodes_of(frame);
    let total = nodes.len();
    let online = nodes.iter().filter(|n| n["online"] == true).count();
    let outdated = nodes.iter().filter(|n| n["agent_old"] == true).count();
    let mut out = format!("集群 {total} 台 · 在线 {online} · 离线 {}", total - online);
    if outdated > 0 {
        match frame.get("agent_latest").and_then(Value::as_str) {
            Some(latest) if !latest.is_empty() => {
                out.push_str(&format!("\n待升级 {outdated} 台（最新 agent {latest}）"))
            }
            _ => out.push_str(&format!("\n待升级 {outdated} 台")),
        }
    }
    out
}

fn nodes_text(frame: &Value) -> String {
    let nodes = nodes_of(frame);
    if nodes.is_empty() {
        return "还没有节点。".into();
    }
    let mut out = String::new();
    for node in nodes.iter().take(MAX_LINES) {
        out.push_str(&node_line(node));
        out.push('\n');
    }
    if nodes.len() > MAX_LINES {
        out.push_str(&format!("…还有 {} 台，去面板看全部。\n", nodes.len() - MAX_LINES));
    }
    out.trim_end().to_string()
}

fn node_line(node: &Value) -> String {
    let online = node["online"] == true;
    let name = node["name"].as_str().unwrap_or("?");
    let version = node["agent_version"].as_str().unwrap_or("");
    let cpu = node.pointer("/metrics/cpu").and_then(Value::as_f64);
    let mem_used = node.pointer("/metrics/mem_used").and_then(Value::as_f64);
    let mem_total = node.pointer("/metrics/mem_total").and_then(Value::as_f64);
    let usage = match (mem_used, mem_total) {
        (Some(used), Some(total)) if total > 0.0 => {
            // 不补齐宽度：名字长度本来就不定，补了也对不齐，反而让同一行里多出空格。
            format!("{:.0}% / {:.0}%", cpu.unwrap_or(0.0), used / total * 100.0)
        }
        _ => "—".into(),
    };
    format!(
        "{} {name} · {} · {usage}",
        if online { "●" } else { "○" },
        if version.is_empty() { "?" } else { version }
    )
}

fn node_text(frame: &Value, wanted: &str) -> String {
    if wanted.is_empty() {
        return "用法：/node <名字>（名字的一部分也行）".into();
    }
    let needle = wanted.to_lowercase();
    let nodes = nodes_of(frame);
    let Some(node) = nodes.iter().find(|n| n["name"].as_str().unwrap_or("").to_lowercase().contains(&needle))
    else {
        return format!("没找到名字里含「{wanted}」的节点。");
    };
    let name = node["name"].as_str().unwrap_or("?");
    let online = node["online"] == true;
    let mut out = format!("{name} · {}\n", if online { "在线" } else { "离线" });
    if let Some(v) = node["agent_version"].as_str().filter(|v| !v.is_empty()) {
        out.push_str(&format!("agent {v}"));
        if let Some(os) = node["os"].as_str().filter(|v| !v.is_empty()) {
            out.push_str(&format!(" · {os}"));
        }
        out.push('\n');
    }
    if let Some(m) = node.get("metrics").filter(|m| !m.is_null()) {
        let pct = |key: &str, total: &str| {
            let used = m.get(key).and_then(Value::as_f64)?;
            let total = m.get(total).and_then(Value::as_f64)?;
            (total > 0.0).then(|| format!("{:.0}%", used / total * 100.0))
        };
        let mut parts = Vec::new();
        if let Some(cpu) = m.get("cpu").and_then(Value::as_f64) {
            parts.push(format!("CPU {cpu:.0}%"));
        }
        if let Some(v) = pct("mem_used", "mem_total") {
            parts.push(format!("内存 {v}"));
        }
        if let Some(v) = pct("disk_used", "disk_total") {
            parts.push(format!("磁盘 {v}"));
        }
        if !parts.is_empty() {
            out.push_str(&parts.join(" · "));
            out.push('\n');
        }
    }
    let (day_down, day_up) = (num(node, "day_rx"), num(node, "day_tx"));
    let (month_down, month_up) = (num(node, "month_rx"), num(node, "month_tx"));
    out.push_str(&format!(
        "今日 ↓{} ↑{} · 本月 ↓{} ↑{}\n",
        bytes(day_down),
        bytes(day_up),
        bytes(month_down),
        bytes(month_up)
    ));
    if let Some(at) = node["expires_at"].as_str().filter(|v| !v.is_empty()) {
        match node.get("expires_in").and_then(Value::as_f64) {
            Some(days) => out.push_str(&format!("到期 {at}（{days:.0} 天）")),
            None => out.push_str(&format!("到期 {at}")),
        }
    }
    out.trim_end().to_string()
}

fn traffic_text(frame: &Value) -> String {
    let nodes = nodes_of(frame);
    let sum = |key: &str| nodes.iter().filter_map(|n| n.get(key).and_then(Value::as_f64)).sum::<f64>();
    let mut out = format!(
        "今日 ↓{} ↑{}\n本月 ↓{} ↑{}",
        bytes(sum("day_rx")),
        bytes(sum("day_tx")),
        bytes(sum("month_rx")),
        bytes(sum("month_tx"))
    );
    let mut top: Vec<(&str, f64)> = nodes
        .iter()
        .map(|n| (n["name"].as_str().unwrap_or("?"), num(n, "month_rx") + num(n, "month_tx")))
        .collect();
    top.sort_by(|a, b| b.1.total_cmp(&a.1));
    let lines: Vec<String> = top
        .iter()
        .filter(|(_, total)| *total > 0.0)
        .take(3)
        .map(|(name, total)| format!("{name} {}", bytes(*total)))
        .collect();
    if !lines.is_empty() {
        out.push_str("\n本月前三：");
        out.push_str(&lines.join(" · "));
    }
    out
}

fn probe_text(frame: &Value) -> String {
    let nodes = nodes_of(frame);
    let mut lines = Vec::new();
    for node in nodes.iter().take(MAX_LINES) {
        let Some(probe) = node.get("probe").filter(|p| !p.is_null()) else { continue };
        let Some(services) = probe.as_object() else { continue };
        let name = node["name"].as_str().unwrap_or("?");
        let mut parts = Vec::new();
        for (service, v) in services {
            let status = v.get("status").and_then(Value::as_str).unwrap_or("");
            let region = v.get("region").and_then(Value::as_str).unwrap_or("");
            let label = match status {
                "unlocked" => "解锁",
                "blocked" => "未解锁",
                "idc" => "IDC",
                "web" => "仅网页",
                "app" => "仅App",
                "cn" => "中国区",
                "noprem" => "无会员",
                "org" => "组织",
                _ => continue,
            };
            parts.push(if region.is_empty() {
                format!("{service}:{label}")
            } else {
                format!("{service}:{label}/{region}")
            });
        }
        if !parts.is_empty() {
            lines.push(format!("{name}  {}", parts.join(" · ")));
        }
    }
    if lines.is_empty() {
        "还没有节点报过流媒体探测结果。".into()
    } else {
        lines.join("\n")
    }
}

fn num(node: &Value, key: &str) -> f64 {
    node.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

/// 1024 进制、三位有效数字 —— 与面板同一套读法，免得同一个数在两处长得不一样。
fn bytes(n: f64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if !n.is_finite() || n <= 0.0 {
        return "0 B".into();
    }
    let mut value = n;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value:.0} B")
    } else if value < 10.0 {
        format!("{value:.2} {}", UNITS[unit])
    } else if value < 100.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame() -> Value {
        json!({
            "agent_latest": "1.1.17",
            "nodes": [
                { "name": "hk-1", "online": true, "agent_version": "1.1.17",
                  "agent_old": false, "os": "Debian 12", "expires_at": "2027-03-14", "expires_in": 147,
                  "day_rx": 1288490188.0, "day_tx": 314572800.0,
                  "month_rx": 32212254720.0, "month_tx": 8589934592.0,
                  "metrics": { "cpu": 2.0, "mem_used": 512.0, "mem_total": 1024.0,
                               "disk_used": 409.0, "disk_total": 1024.0 },
                  "probe": { "netflix": { "status": "unlocked", "region": "US" },
                             "chatgpt": { "status": "web" } } },
                { "name": "la-2", "online": false, "agent_version": "1.1.15",
                  "agent_old": true, "day_rx": 0.0, "day_tx": 0.0,
                  "month_rx": 0.0, "month_tx": 0.0, "metrics": null }
            ]
        })
    }

    #[test]
    fn commands_are_read_with_or_without_an_at_suffix() {
        assert_eq!(command("/status"), Some(("status", "")));
        assert_eq!(command("/node hk-1"), Some(("node", "hk-1")));
        assert_eq!(command("  /nodes@MyBot  "), Some(("nodes", "")));
        assert_eq!(command("/node 名字 带空格"), Some(("node", "名字 带空格")));
        assert_eq!(command("hello"), None, "不是命令就不处理");
    }

    #[test]
    fn the_overview_counts_what_is_actually_there() {
        let text = status_text(&frame());
        assert!(text.contains("集群 2 台 · 在线 1 · 离线 1"), "{text}");
        assert!(text.contains("待升级 1 台（最新 agent 1.1.17）"), "{text}");
    }

    #[test]
    fn the_list_shows_a_dot_and_a_version_per_node() {
        let text = nodes_text(&frame());
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert!(lines[0].starts_with("● hk-1"), "{}", lines[0]);
        assert!(lines[0].contains("1.1.17"), "{}", lines[0]);
        assert!(lines[0].contains("2% / 50%"), "CPU 与内存百分比：{}", lines[0]);
        assert!(lines[1].starts_with("○ la-2"), "离线用空心点：{}", lines[1]);
        assert!(lines[1].contains('—'), "没有指标就写破折号：{}", lines[1]);
    }

    #[test]
    fn a_node_is_found_by_part_of_its_name() {
        let text = node_text(&frame(), "HK");
        assert!(text.contains("hk-1 · 在线"), "{text}");
        assert!(text.contains("agent 1.1.17 · Debian 12"), "{text}");
        assert!(text.contains("CPU 2% · 内存 50% · 磁盘 40%"), "{text}");
        assert!(text.contains("今日 ↓1.20 GB ↑300 MB"), "{text}");
        assert!(text.contains("到期 2027-03-14（147 天）"), "{text}");
        assert!(node_text(&frame(), "没有这台").contains("没找到"));
        assert!(node_text(&frame(), "").contains("用法"));
    }

    #[test]
    fn traffic_sums_both_directions_and_names_the_top() {
        let text = traffic_text(&frame());
        assert!(text.contains("今日 ↓1.20 GB ↑300 MB"), "{text}");
        assert!(text.contains("本月 ↓30.0 GB ↑8.00 GB"), "{text}");
        assert!(text.contains("本月前三：hk-1 38.0 GB"), "{text}");
    }

    #[test]
    fn probes_are_translated_into_words() {
        let text = probe_text(&frame());
        assert!(text.contains("hk-1  "), "{text}");
        assert!(text.contains("netflix:解锁/US"), "{text}");
        assert!(text.contains("chatgpt:仅网页"), "{text}");
        assert!(!text.contains("la-2"), "没报过的节点不该出现：{text}");
    }

    #[test]
    fn bytes_use_the_same_ladder_as_the_panel() {
        assert_eq!(bytes(0.0), "0 B");
        assert_eq!(bytes(1023.0), "1023 B");
        assert_eq!(bytes(1024.0), "1.00 KB");
        assert_eq!(bytes(10.0 * 1024.0), "10.0 KB");
        assert_eq!(bytes(100.0 * 1024.0), "100 KB");
        assert_eq!(bytes(-5.0), "0 B");
    }
}
