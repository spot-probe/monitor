//! 只读的 Telegram 命令（`getUpdates` 长轮询）+ 一个写操作：`/upgrade`（带确认按钮）。
//!
//! 为什么不走 webhook：自建 hub 常常只有出网、没有入网，而 `getUpdates` 只要求能出网 ——
//! 它本来就要出网去取发布物与主题。代价是一条常驻的长连接，以及**重启后要先把积压的更新
//! 丢掉**：一条两小时前的命令不该在重启时被执行（见 `drained`）。
//!
//! ⚠️ 只认**配置里的那个会话**：消息里的 `chat.id` 与设置不同就当作没看见。bot token 写在
//! 设置里，而设置页只对管理员开放；这里不再引入第二套权限模型。
//!
//! ⚠️ 回复一律用 **HTML**（`parse_mode`）而不是 Markdown：节点名、系统名里出现的 `_` `*`
//! 在 Markdown 里会把整条消息弄坏，而 HTML 只需要转义三个字符（见 [`esc`]）。

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

const HELP: &str = "<b>可用命令</b>
/status —— 集群概览
/nodes —— 节点列表（在线、版本、CPU/内存）
/node &lt;名字&gt; —— 单台详情
/traffic —— 今日与本月流量
/probe —— 各节点最近一次流媒体探测
/upgrade &lt;名字&gt; —— 远程升级（会先弹确认按钮）";

/// Telegram API 的根。生产里没有理由改它 —— 存在的意义是**能用本地假服务器把轮询、
/// 会话过滤与回复跑一遍**：这三条只能在进程外验证，而单元测试碰不到 HTTP。
fn api_base() -> String {
    std::env::var("MONITOR_TELEGRAM_API").unwrap_or_else(|_| "https://api.telegram.org".to_string())
}

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
        .map_err(strip)?;
    let response = response.error_for_status().map_err(strip)?;
    let body: Value = response.json().await.map_err(strip)?;
    Ok(body.get("result").and_then(Value::as_array).cloned().unwrap_or_default())
}

fn strip(e: reqwest::Error) -> anyhow::Error {
    anyhow!("{}", anyhow::Error::from(e.without_url()))
}

async fn handle(app: &Shared, token: &str, chat: &str, update: &Value) {
    // 按钮回调走另一条路：`callback_query` 里带着 `data` 与原始消息。
    if let Some(query) = update.get("callback_query") {
        return callback(app, token, chat, query).await;
    }
    let Some(message) = update.get("message") else { return };
    // 只认配置里的会话：别的群或私聊发来的命令当没看见。
    if chat_of(message).as_deref() != Some(chat) {
        return;
    }
    let Some(text) = message.get("text").and_then(Value::as_str) else { return };
    let Some((cmd, arg)) = command(text) else { return };
    let snapshot = frame(app);
    match cmd {
        "start" | "help" => reply(app, token, chat, HELP.to_string()).await,
        "status" => reply(app, token, chat, status_text(&snapshot)).await,
        "nodes" => reply(app, token, chat, nodes_text(&snapshot)).await,
        "node" => reply(app, token, chat, node_text(&snapshot, arg)).await,
        "traffic" => reply(app, token, chat, traffic_text(&snapshot)).await,
        "probe" => reply(app, token, chat, probe_text(&snapshot)).await,
        "upgrade" => ask_upgrade(app, token, chat, &snapshot, arg).await,
        // 不认识的命令**不回**：两个 bot 在同一个群里时，互相回"不认识"会打起来。
        _ => {}
    }
}

async fn reply(app: &App, token: &str, chat: &str, text: String) {
    if let Err(e) = post_message(app, token, chat, &text, None).await {
        debug!("telegram sendMessage: {e:#}");
    }
}

/// 找到节点、检查它能不能升，然后**只发一条带确认按钮的消息** —— 不直接动手。
async fn ask_upgrade(app: &Shared, token: &str, chat: &str, snapshot: &Value, wanted: &str) {
    if wanted.is_empty() {
        return reply(app, token, chat, "用法：/upgrade &lt;名字&gt;（名字的一部分也行）".into()).await;
    }
    let Some(node) = find_node(snapshot, wanted) else {
        return reply(app, token, chat, format!("没找到名字里含「{}」的节点。", esc(wanted))).await;
    };
    let name = node["name"].as_str().unwrap_or("?");
    let id = node["id"].as_i64().unwrap_or(0);
    if let Some(why) = upgrade_blocker(&node) {
        return reply(app, token, chat, format!("<b>{}</b>：{why}。", esc(name))).await;
    }
    let latest = snapshot.get("agent_latest").and_then(Value::as_str).unwrap_or("");
    let text = format!(
        "把 <b>{}</b> 升级到 <code>{}</code>？\n\n它会下载约 1.8 MB、验签、替换二进制并重启；\n\
         180 秒内连不上 hub 会自动回滚。",
        esc(name),
        esc(latest)
    );
    let keyboard = json!({ "inline_keyboard": [[
        { "text": "确认升级", "callback_data": format!("up:{id}") },
        { "text": "取消", "callback_data": "no" },
    ]] });
    if let Err(e) = post_message(app, token, chat, &text, Some(keyboard)).await {
        debug!("telegram sendMessage: {e:#}");
    }
}

async fn callback(app: &Shared, token: &str, chat: &str, query: &Value) {
    // 回调也要验会话：按钮是从某条消息里点出来的，而那条消息可能被转发到别处。
    let from = query.pointer("/message/chat/id").and_then(Value::as_i64).map(|v| v.to_string());
    if from.as_deref() != Some(chat) {
        return;
    }
    let Some(data) = query.get("data").and_then(Value::as_str) else { return };
    let id = query.get("id").and_then(Value::as_str).unwrap_or("");
    if data == "no" {
        let _ = answer(app, token, id, "已取消").await;
        return;
    }
    let Some(raw) = data.strip_prefix("up:") else { return };
    let Ok(node) = raw.parse::<i64>() else { return };
    // 先应答，按钮上的转圈才会停（Telegram 要求几秒内应答）。
    let _ = answer(app, token, id, "开始升级…").await;
    match crate::api::push_upgrade(app, node).await {
        Ok((version, bytes)) => {
            reply(
                app,
                token,
                chat,
                format!(
                    "已推给节点 <b>#{node}</b>：<code>{}</code>（{bytes} 字节），等它回报校验结果。\n\
                     /status 能看到它是否已经回来。",
                    esc(&version)
                ),
            )
            .await;
        }
        Err((status, message)) => {
            reply(app, token, chat, format!("升级没有发出去（{}）：{}", status.as_u16(), esc(&message)))
                .await;
        }
    }
}

async fn answer(app: &App, token: &str, callback_id: &str, text: &str) -> Result<()> {
    let url = format!("{}/bot{token}/answerCallbackQuery", api_base());
    app.http
        .post(&url)
        .json(&json!({ "callback_query_id": callback_id, "text": text }))
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(strip)?
        .error_for_status()
        .map_err(strip)?;
    Ok(())
}

async fn post_message(app: &App, token: &str, chat: &str, text: &str, keyboard: Option<Value>) -> Result<()> {
    let url = format!("{}/bot{token}/sendMessage", api_base());
    let mut body = json!({ "chat_id": chat, "text": text, "parse_mode": "HTML" });
    if let Some(markup) = keyboard {
        body["reply_markup"] = markup;
    }
    app.http
        .post(&url)
        .json(&body)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(strip)?
        .error_for_status()
        .map_err(strip)?;
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

/// 能不能升：先看在线（下发要一条活着的连接），再看那台机器开没开关，最后看它是不是真的落后。
///
/// 抽成纯函数是因为这三种拒绝**各有各的话**，而它们只能在真机上分别触发 —— 拿快照里的
/// 几个字段就能把它们钉住。
fn upgrade_blocker(node: &Value) -> Option<&'static str> {
    if node["online"] != true {
        return Some("它不在线 —— 下发是在一条活着的连接上推的");
    }
    if node["allow_remote_upgrade"] != true {
        return Some("那台机器没开「允许远程升级」，只能 SSH 重跑安装命令");
    }
    if node["agent_old"] != true {
        return Some("它已经是最新的了");
    }
    None
}

/// HTML 里只有三个字符需要转义；节点名与系统名来自那台机器，必须过这一道。
fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// 面板用的那份管理快照 —— 与面板同一个数据源，避免第二套查询各写一遍。
fn frame(app: &App) -> Value {
    serde_json::from_str(crate::api::admin_frame(app).as_str()).unwrap_or(Value::Null)
}

fn chat_of(message: &Value) -> Option<String> {
    message.pointer("/chat/id").and_then(Value::as_i64).map(|v| v.to_string())
}

fn nodes_of(frame: &Value) -> Vec<Value> {
    frame.get("nodes").and_then(Value::as_array).cloned().unwrap_or_default()
}

fn find_node(frame: &Value, wanted: &str) -> Option<Value> {
    let needle = wanted.to_lowercase();
    nodes_of(frame).into_iter().find(|n| n["name"].as_str().unwrap_or("").to_lowercase().contains(&needle))
}

fn status_text(frame: &Value) -> String {
    let nodes = nodes_of(frame);
    let total = nodes.len();
    let online = nodes.iter().filter(|n| n["online"] == true).count();
    let outdated = nodes.iter().filter(|n| n["agent_old"] == true).count();
    let mut out = format!("集群 <b>{total}</b> 台 · 在线 <b>{online}</b> · 离线 <b>{}</b>", total - online);
    if outdated > 0 {
        match frame.get("agent_latest").and_then(Value::as_str) {
            Some(latest) if !latest.is_empty() => {
                out.push_str(&format!("\n待升级 <b>{outdated}</b> 台 · 最新 <code>{}</code>", esc(latest)))
            }
            _ => out.push_str(&format!("\n待升级 <b>{outdated}</b> 台")),
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
            format!("{:.0}% / {:.0}%", cpu.unwrap_or(0.0), used / total * 100.0)
        }
        _ => "—".into(),
    };
    format!(
        "{} <b>{}</b> · <code>{}</code> · {usage}",
        if online { "●" } else { "○" },
        esc(name),
        esc(if version.is_empty() { "?" } else { version })
    )
}

fn node_text(frame: &Value, wanted: &str) -> String {
    if wanted.is_empty() {
        return "用法：/node &lt;名字&gt;（名字的一部分也行）".into();
    }
    let Some(node) = find_node(frame, wanted) else {
        return format!("没找到名字里含「{}」的节点。", esc(wanted));
    };
    let name = node["name"].as_str().unwrap_or("?");
    let online = node["online"] == true;
    let mut out = format!("<b>{}</b> · {}\n", esc(name), if online { "在线" } else { "离线" });
    if let Some(v) = node["agent_version"].as_str().filter(|v| !v.is_empty()) {
        out.push_str(&format!("agent <code>{}</code>", esc(v)));
        if let Some(os) = node["os"].as_str().filter(|v| !v.is_empty()) {
            out.push_str(&format!(" · {}", esc(os)));
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
    let (day_down, day_up) = (num(&node, "day_rx"), num(&node, "day_tx"));
    let (month_down, month_up) = (num(&node, "month_rx"), num(&node, "month_tx"));
    out.push_str(&format!(
        "今日 <code>↓{} ↑{}</code> · 本月 <code>↓{} ↑{}</code>\n",
        bytes(day_down),
        bytes(day_up),
        bytes(month_down),
        bytes(month_up)
    ));
    if let Some(at) = node["expires_at"].as_str().filter(|v| !v.is_empty()) {
        match node.get("expires_in").and_then(Value::as_f64) {
            Some(days) => out.push_str(&format!("到期 <code>{}</code>（{days:.0} 天）", esc(at))),
            None => out.push_str(&format!("到期 <code>{}</code>", esc(at))),
        }
    }
    out.trim_end().to_string()
}

fn traffic_text(frame: &Value) -> String {
    let nodes = nodes_of(frame);
    let sum = |key: &str| nodes.iter().filter_map(|n| n.get(key).and_then(Value::as_f64)).sum::<f64>();
    let mut out = format!(
        "今日 <code>↓{} ↑{}</code>\n本月 <code>↓{} ↑{}</code>",
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
        .map(|(name, total)| format!("{} <code>{}</code>", esc(name), bytes(*total)))
        .collect();
    if !lines.is_empty() {
        out.push_str("\n\n本月前三：");
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
        let name = esc(node["name"].as_str().unwrap_or("?"));
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
            let region = if region.is_empty() { String::new() } else { format!("/{}", esc(region)) };
            parts.push(format!("{service}:{label}{region}"));
        }
        if !parts.is_empty() {
            lines.push(format!("<b>{name}</b>  {}", parts.join(" · ")));
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
                { "id": 7, "name": "hk-1", "online": true, "agent_version": "1.1.17",
                  "agent_old": false, "allow_remote_upgrade": true, "os": "Debian 12",
                  "expires_at": "2027-03-14", "expires_in": 147,
                  "day_rx": 1288490188.0, "day_tx": 314572800.0,
                  "month_rx": 32212254720.0, "month_tx": 8589934592.0,
                  "metrics": { "cpu": 2.0, "mem_used": 512.0, "mem_total": 1024.0,
                               "disk_used": 409.0, "disk_total": 1024.0 },
                  "probe": { "netflix": { "status": "unlocked", "region": "US" },
                             "chatgpt": { "status": "web" } } },
                { "id": 8, "name": "la-2", "online": false, "agent_version": "1.1.15",
                  "agent_old": true, "allow_remote_upgrade": false,
                  "day_rx": 0.0, "day_tx": 0.0, "month_rx": 0.0, "month_tx": 0.0,
                  "metrics": null }
            ]
        })
    }

    #[test]
    fn commands_are_read_with_or_without_an_at_suffix() {
        assert_eq!(command("/status"), Some(("status", "")));
        assert_eq!(command("/node hk-1"), Some(("node", "hk-1")));
        assert_eq!(command("  /nodes@MyBot  "), Some(("nodes", "")));
        assert_eq!(command("/node 名字 带空格"), Some(("node", "名字 带空格")));
        assert_eq!(command("/upgrade hk-1"), Some(("upgrade", "hk-1")));
        assert_eq!(command("hello"), None, "不是命令就不处理");
    }

    #[test]
    fn the_overview_counts_what_is_actually_there() {
        let text = status_text(&frame());
        assert!(text.contains("集群 <b>2</b> 台 · 在线 <b>1</b> · 离线 <b>1</b>"), "{text}");
        assert!(text.contains("待升级 <b>1</b> 台 · 最新 <code>1.1.17</code>"), "{text}");
    }

    #[test]
    fn the_list_shows_a_dot_a_bold_name_and_a_version() {
        let text = nodes_text(&frame());
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert!(lines[0].starts_with("● <b>hk-1</b>"), "{}", lines[0]);
        assert!(lines[0].contains("<code>1.1.17</code>"), "{}", lines[0]);
        assert!(lines[0].contains("2% / 50%"), "CPU 与内存百分比：{}", lines[0]);
        assert!(lines[1].starts_with("○ <b>la-2</b>"), "离线用空心点：{}", lines[1]);
        assert!(lines[1].contains('—'), "没有指标就写破折号：{}", lines[1]);
    }

    #[test]
    fn a_node_is_found_by_part_of_its_name() {
        let text = node_text(&frame(), "HK");
        assert!(text.contains("<b>hk-1</b> · 在线"), "{text}");
        assert!(text.contains("agent <code>1.1.17</code> · Debian 12"), "{text}");
        assert!(text.contains("CPU 2% · 内存 50% · 磁盘 40%"), "{text}");
        assert!(text.contains("今日 <code>↓1.20 GB ↑300 MB</code>"), "{text}");
        assert!(text.contains("到期 <code>2027-03-14</code>（147 天）"), "{text}");
        assert!(node_text(&frame(), "没有这台").contains("没找到"));
        assert!(node_text(&frame(), "").contains("用法"));
    }

    #[test]
    fn traffic_sums_both_directions_and_names_the_top() {
        let text = traffic_text(&frame());
        assert!(text.contains("今日 <code>↓1.20 GB ↑300 MB</code>"), "{text}");
        assert!(text.contains("本月 <code>↓30.0 GB ↑8.00 GB</code>"), "{text}");
        assert!(text.contains("本月前三：hk-1 <code>38.0 GB</code>"), "{text}");
    }

    #[test]
    fn probes_are_translated_into_words() {
        let text = probe_text(&frame());
        assert!(text.contains("<b>hk-1</b>  "), "{text}");
        assert!(text.contains("netflix:解锁/US"), "{text}");
        assert!(text.contains("chatgpt:仅网页"), "{text}");
        assert!(!text.contains("la-2"), "没报过的节点不该出现：{text}");
    }

    /// 节点名与系统名来自那台机器：进 HTML 之前必须转义，否则一条消息会被 Telegram 整条
    /// 拒绝，而拒绝原因看起来像"发送失败"，非常难查。
    #[test]
    fn markup_in_a_name_cannot_break_the_message() {
        let frame = json!({ "nodes": [{ "name": "a<b>&c", "online": true, "agent_version": "1" }] });
        let text = nodes_text(&frame);
        assert!(text.contains("a&lt;b&gt;&amp;c"), "{text}");
        assert!(!text.contains("<b>&"), "原始尖括号不能漏出去：{text}");
        assert_eq!(esc("<x>&"), "&lt;x&gt;&amp;");
    }

    #[test]
    fn every_way_an_upgrade_is_refused_says_why() {
        let ready = json!({ "online": true, "allow_remote_upgrade": true, "agent_old": true });
        assert_eq!(upgrade_blocker(&ready), None, "三样齐了才允许");
        let offline = json!({ "online": false, "allow_remote_upgrade": true, "agent_old": true });
        assert!(upgrade_blocker(&offline).unwrap().contains("不在线"));
        let closed = json!({ "online": true, "allow_remote_upgrade": false, "agent_old": true });
        assert!(upgrade_blocker(&closed).unwrap().contains("没开"));
        let current = json!({ "online": true, "allow_remote_upgrade": true, "agent_old": false });
        assert!(upgrade_blocker(&current).unwrap().contains("最新"));
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
