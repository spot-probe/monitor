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

use crate::text::{blank_left, blank_to, clip, width};
use crate::{App, Shared};

/// 配置缺失或请求失败时的退避。
const IDLE: Duration = Duration::from_secs(5);
/// `getUpdates` 的长轮询秒数。Telegram 允许到 50；取 25 是为了不撞上常见的 30 秒中间盒超时。
const LONG_POLL: u64 = 25;
/// 一条命令最多回多少台节点 —— 再多就该去面板看了（Telegram 单条也有 4096 字上限）。
const MAX_LINES: usize = 25;

/// 一条命令 + 一句解释。**对齐靠 `<pre>`**：Telegram 里只有 `<pre>`/`<code>` 是等宽字体，
/// 而普通文字是比例字体 —— 在比例字体里补空格永远对不齐。
const COMMANDS: [(&str, &str); 12] = [
    ("/nodes", "集群概览 + 节点列表（在线、版本、CPU/内存）"),
    ("/node <名字>", "单台详情，也可以 /node 3"),
    ("/traffic", "今日与本月流量"),
    ("/probe", "各节点最近一次流媒体探测"),
    ("/upgrade <名字>", "远程升级（会先弹确认按钮）"),
    ("/mute <名字> [2h]", "静音（默认 1 小时）"),
    ("/muted", "现在静音了哪些"),
    ("/report [周期]", "立刻出一份报告（默认日报）"),
    ("/whoami", "这个机器人连的是谁"),
    ("/expiry [天]", "多久内到期（默认 30 天）"),
    ("/cost", "每月/每年成本，最贵三台"),
    ("/uptime", "hub 运行了多久"),
];

/// Telegram API 的根（与通知共用一个，见 `notify::telegram_api`）。
fn api_base() -> String {
    crate::notify::telegram_api()
}

/// 轮询循环。设置里没有 token 或 chat 时什么都不做（连日志都不打，免得每 5 秒刷屏）。
/// 向左下角那个「菜单」按钮注册命令（Telegram 的 `setMyCommands`）。
/// ⚠️ 不注册的话，客户端只认得 `/start` —— 别的命令得靠用户自己记。
/// 幂等，失败只记日志：注册不上不影响**收**命令。
async fn register_commands(app: &App, token: &str) {
    let commands: Vec<Value> = COMMANDS
        .iter()
        .map(|(cmd, about)| {
            let name = cmd.trim_start_matches('/').split_whitespace().next().unwrap_or("");
            json!({ "command": name, "description": about })
        })
        .collect();
    let url = format!("{}/bot{token}/setMyCommands", api_base());
    let sent = app
        .http
        .post(&url)
        .json(&json!({ "commands": commands }))
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status);
    if let Err(e) = sent {
        debug!("setMyCommands: {:#}", strip(e));
    }
}

pub async fn watch(app: Shared) {
    let mut offset: i64 = 0;
    // 第一次成功轮询只推进 offset、不执行命令：hub 重启不该重放积压的旧消息。
    let mut drained = false;
    let mut registered = false;
    loop {
        let (Some(token), Some(chat)) =
            (setting(&app, "notify_telegram_token"), setting(&app, "notify_telegram_chat"))
        else {
            tokio::time::sleep(IDLE).await;
            continue;
        };
        // 第一次拿到 token 就注册一次命令菜单（幂等，失败不影响收命令）。
        if !registered {
            register_commands(&app, &token).await;
            registered = true;
        }
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
        // 文字之外再挂一排按钮：常用命令不必记、也不必打字。
        "start" => menu(app, token, chat, None).await,
        // `/help` 只打文字：在群里 `/start` 会留一条带按钮的消息，而多数时候只想扫一眼命令表。
        "help" => {
            let site = setting(app, "site_name")
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "Monitor".to_string());
            reply(app, token, chat, help_text(&site)).await
        }
        "node" => reply(app, token, chat, node_text(&snapshot, arg)).await,
        "upgrade" => ask_upgrade(app, token, chat, &snapshot, arg).await,
        "mute" | "unmute" => mute_cmd(app, token, chat, &snapshot, arg, cmd == "mute").await,
        "report" => report_cmd(app, token, chat, arg).await,
        "expiry" => reply(app, token, chat, expiry_text(&snapshot, arg)).await,
        "cost" => reply(app, token, chat, cost_text(app, &snapshot)).await,
        "uptime" => reply(app, token, chat, uptime_text(app)).await,
        "whoami" => reply(app, token, chat, whoami_text(app, chat, &snapshot)).await,
        "muted" => reply(app, token, chat, muted_text(&snapshot)).await,
        // 不认识的命令**不回**：两个 bot 在同一个群里时，互相回"不认识"会打起来。
        other => {
            if let Some(text) = render(other, &snapshot) {
                // ⚠️ **打字命令一律新回复** ✗✓：改上一条会让回复出现在几分钟前的旧消息位置
                // （用户刚发的那条在底部），看起来就是"这条命令没反应"。
                // 原地刷新只用于菜单按钮 —— 见 `menu_result`。
                reply(app, token, chat, text).await;
            }
        }
    }
}

async fn reply(app: &App, token: &str, chat: &str, text: String) {
    reply_with(app, token, chat, text, None).await
}

/// 带按钮的回复（按钮就是 `reply_markup.inline_keyboard`）。
async fn reply_with(app: &App, token: &str, chat: &str, text: String, keyboard: Option<Value>) {
    if let Err(e) = post_message(app, token, chat, &text, keyboard).await {
        debug!("telegram sendMessage: {e:#}");
    }
}

/// 一台节点的月成本（人民币）。缺价格 / 缺汇率 ⇒ `None`（不是 0 ✗ —— "不知道"与"免费"不同）。
fn monthly_cny(node: &Value, rates: &std::collections::HashMap<String, f64>) -> Option<f64> {
    let price = node["price"].as_f64().filter(|p| *p > 0.0)?;
    let months = crate::cycle_months(node["billing_cycle"].as_str().unwrap_or("monthly"))? as f64;
    let per = crate::fx::cny_per_unit(rates, node["currency"].as_str().unwrap_or("CNY"))?;
    Some(price / months * per)
}

fn yuan(v: f64) -> String {
    format!("¥{:.2}", v)
}

/// `/cost`：每月/每年合计 + 最贵三台。与面板的"成本"页同一套口径（按周期摊平）。
fn cost_text(app: &App, frame: &Value) -> String {
    let nodes = nodes_of(frame);
    let Some((rates, _, manual)) = crate::fx::rates(app) else {
        return "还没拿到汇率 —— 去面板的「成本」页看一次，或配一个手动汇率。".into();
    };
    let mut priced: Vec<(&str, f64)> = Vec::new();
    let mut missing = 0;
    for node in &nodes {
        match monthly_cny(node, &rates) {
            Some(v) => priced.push((node["name"].as_str().unwrap_or("?"), v)),
            // 没填价格、或币种不在汇率表里 —— 两种都**不计入合计**，但要数出来。
            None => missing += 1,
        }
    }
    let total: f64 = priced.iter().map(|(_, v)| v).sum();
    priced.sort_by(|a, b| b.1.total_cmp(&a.1));
    let mut out = format!("<b>每月 {}</b> · 折合每年 {}\n<pre>", yuan(total), yuan(total * 12.0));
    if let Some(rate) = crate::fx::cny_per_unit(&rates, "USD") {
        out.push_str(&format!("汇率 1 USD = {rate:.4} CNY{}\n", if manual { "（手动）" } else { "" }));
    }
    if missing > 0 {
        out.push_str(&format!("未填价格或缺汇率 {missing} 台（未计入合计）\n"));
    }
    if !priced.is_empty() {
        out.push('\n');
        let nw = priced.iter().map(|(n, _)| width(n)).max().unwrap_or(0).clamp(8, 16);
        for (i, (name, v)) in priced.iter().take(3).enumerate() {
            out.push_str(&format!(
                "{:>2} {}  {}\n",
                i + 1,
                blank_to(&esc(&clip(name, nw)), nw),
                blank_left(&yuan(*v), 9)
            ));
        }
    } else {
        out.push_str("\n还没有一台填了价格。\n");
    }
    format!("{}</pre>", out.trim_end())
}

/// `/expiry [天数]`：多久内到期（默认 30 天），按天数升序。
fn expiry_text(frame: &Value, arg: &str) -> String {
    let days: i64 = arg.trim().parse().ok().filter(|d| *d > 0 && *d <= 365).unwrap_or(30);
    let mut due: Vec<(&str, &str, i64)> = Vec::new();
    let all = nodes_of(frame);
    for node in all.iter() {
        let Some(at) = node["expires_at"].as_str().filter(|s| !s.is_empty()) else { continue };
        let Some(left) = node["expires_in"].as_f64() else { continue };
        let left = left as i64;
        if left <= days {
            due.push((node["name"].as_str().unwrap_or("?"), at, left));
        }
    }
    if due.is_empty() {
        return format!("{days} 天内没有到期的节点。");
    }
    due.sort_by_key(|(_, _, left)| *left);
    let nw = due.iter().map(|(n, _, _)| width(n)).max().unwrap_or(0).clamp(8, 16);
    let mut pre = String::new();
    for (i, (name, at, left)) in due.iter().enumerate() {
        pre.push_str(&format!(
            "{:>2} {}  {}  {:>4} 天后\n",
            i + 1,
            blank_to(&esc(&clip(name, nw)), nw),
            at,
            left
        ));
    }
    format!("<b>{} 天内到期 {} 台</b>\n<pre>{}</pre>", days, due.len(), pre.trim_end())
}

/// 人类读的时长：`3 天 7 小时` / `42 分钟` / `刚刚`。
fn span(secs: i64) -> String {
    let (d, h, m) = (secs / 86_400, secs % 86_400 / 3_600, secs % 3_600 / 60);
    match (d, h, m) {
        (0, 0, 0) => "刚刚".into(),
        (0, 0, m) => format!("{m} 分钟"),
        (0, h, _) => format!("{h} 小时"),
        (d, h, _) => format!("{d} 天 {h} 小时"),
    }
}

/// `/uptime`：hub 运行了多久 —— "机器人还活着吗"的最短答案。
fn uptime_text(app: &App) -> String {
    let secs = (chrono::Utc::now().timestamp() - app.started_at).max(0);
    format!("已运行 {} · hub <code>{}</code>", span(secs), env!("CARGO_PKG_VERSION"))
}

/// `/report [day|week|month|quarter|half|year]` —— 立刻出一份报告。
/// 与定时报告**同一套取数与排版**（`report_watch::report_rows` + `report::body`），
/// 只是不走通知队列：它是"我现在就要看"，不该推给所有渠道、也不该占掉当天的自动报告。
async fn report_cmd(app: &Shared, token: &str, chat: &str, arg: &str) {
    use crate::db::Period;
    let period = match arg.trim().to_lowercase().as_str() {
        "" | "day" | "日" | "日报" => Period::Day,
        "week" | "周" | "周报" => Period::Week,
        "month" | "月" | "月报" => Period::Month,
        "quarter" | "季" | "季报" => Period::Quarter,
        "half" | "半年" | "半年报" => Period::Half,
        "year" | "年" | "年报" => Period::Year,
        other => {
            return reply(
                app,
                token,
                chat,
                format!(
                    "不认识的周期「{}」。用法：<code>/report day|week|month|quarter|half|year</code>",
                    esc(other)
                ),
            )
            .await
        }
    };
    let today = chrono::Local::now().date_naive();
    let (from, to, prev_from, prev_to) = period.last_full(today);
    let rows = match crate::report_watch::report_rows(app, &from, &to) {
        Ok(rows) => rows,
        Err(e) => return reply(app, token, chat, format!("取不到数据：{}", esc(&e.to_string()))).await,
    };
    let prev: Vec<(String, i64, i64)> = crate::report_watch::report_rows(app, &prev_from, &prev_to)
        .unwrap_or_default()
        .into_iter()
        .map(|r| (r.name, r.rx, r.tx))
        .collect();
    let offset = crate::report_watch::offset_minutes(&setting(app, "report_tz").unwrap_or_default());
    let title = crate::report::title(period, &from, &to, &crate::report_watch::label(offset));
    let mut message = crate::report::body(&rows, &prev);
    if let Some(note) = crate::report::coverage_note(&from, &to, app.db.earliest_traffic_day().as_deref()) {
        message.push_str(&format!("\n\n{}", esc(&note)));
    }
    reply(app, token, chat, format!("{title}\n{message}")).await;
}

/// `/whoami`：机器人认的是哪个会话、连的是哪台 hub —— 群里不回话时第一个要确认的东西。
fn whoami_text(app: &App, chat: &str, frame: &Value) -> String {
    let site =
        setting(app, "site_name").filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "Monitor".to_string());
    let now = chrono::Utc::now().timestamp();
    let muted = nodes_of(frame).iter().filter(|n| n["muted_until"].as_i64().unwrap_or(0) > now).count();
    whoami_body(
        &site,
        env!("CARGO_PKG_VERSION"),
        chat,
        &setting(app, "report_periods").unwrap_or_default(),
        muted,
    )
}

/// 纯排版部分（单测不搭数据库）。
fn whoami_body(site: &str, version: &str, chat: &str, periods: &str, muted: usize) -> String {
    let periods = if periods.trim().is_empty() { "未开".to_string() } else { esc(periods) };
    format!(
        "<b>{}</b> · hub <code>{}</code>\n会话 <code>{}</code>\n报告 <code>{}</code> · 静音 {} 台",
        esc(site),
        esc(version),
        esc(chat),
        periods,
        muted
    )
}

/// `/mute <编号|名字> [2h]` / `/unmute <编号|名字>`。
async fn mute_cmd(app: &Shared, token: &str, chat: &str, snapshot: &Value, arg: &str, on: bool) {
    // 名字里可能有空格（"mkcloud 深港"），所以**从右边**试一次时长；不像时长就整段当名字。
    let (target, span) = match arg.rsplit_once(char::is_whitespace) {
        Some((t, s)) if parse_span(s).is_some() => (t.trim(), parse_span(s).unwrap_or(3600)),
        _ => (arg.trim(), 3600),
    };
    if target.is_empty() {
        return reply(app, token, chat, "用法：/mute &lt;编号或名字&gt; [10m|2h|1d]（默认 1 小时）".into())
            .await;
    }
    let Some(node) = find_node(snapshot, target) else {
        return reply(app, token, chat, format!("没找到「{}」。发 /nodes 看编号。", esc(target))).await;
    };
    let name = node["name"].as_str().unwrap_or("?").to_string();
    let id = node["id"].as_i64().unwrap_or(0);
    let until = if on { chrono::Utc::now().timestamp() + span } else { 0 };
    match app.db.set_muted(id, until) {
        Ok(()) if on => {
            reply(app, token, chat, format!("已静音 <b>{}</b> 到 {}。", esc(&name), clock(until))).await
        }
        Ok(()) => reply(app, token, chat, format!("已取消 <b>{}</b> 的静音。", esc(&name))).await,
        Err(e) => reply(app, token, chat, format!("没能改静音：{}", esc(&e.to_string()))).await,
    }
}

/// 找到节点、检查它能不能升，然后**只发一条带确认按钮的消息** —— 不直接动手。
async fn ask_upgrade(app: &Shared, token: &str, chat: &str, snapshot: &Value, wanted: &str) {
    if wanted.trim().is_empty() {
        // 不带参数 = "谁该升级"（维护者要的那条入口）；带参数才是"升这一台"。
        return reply(app, token, chat, upgrade_list(snapshot)).await;
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
    // 菜单按钮：`cmd:<命令>` —— 与文字命令走同一个 `render`，不会两套。
    if data == "cmd:menu" {
        let _ = answer(app, token, id, "").await;
        menu(app, token, chat, Some(query)).await;
        return;
    }
    if let Some(cmd) = data.strip_prefix("cmd:") {
        let snapshot = frame(app);
        match render(cmd, &snapshot) {
            Some(text) => {
                let _ = answer(app, token, id, "").await;
                menu_result(app, token, chat, query, text, back_keyboard()).await;
            }
            None => {
                let _ = answer(app, token, id, "这个按钮没有对应的命令").await;
            }
        }
        return;
    }
    // 告警下面那颗「静音 1 小时」：`mute:<分钟>:<节点 id>`。
    if let Some(rest) = data.strip_prefix("mute:") {
        if let Some((mins, node)) = rest.split_once(':') {
            if let (Ok(mins), Ok(node_id)) = (mins.parse::<i64>(), node.parse::<i64>()) {
                let until = chrono::Utc::now().timestamp() + mins * 60;
                // ⚠️ `answer` 要的是**回调 id**（上面那个 `id`），不是节点 id —— 这里别把名字覆盖掉。
                let _ = answer(app, token, id, "已静音").await;
                match app.db.set_muted(node_id, until) {
                    Ok(()) => {
                        let name = nodes_of(&frame(app))
                            .into_iter()
                            .find(|n| n["id"].as_i64() == Some(node_id))
                            .and_then(|n| n["name"].as_str().map(str::to_string))
                            .unwrap_or_else(|| format!("#{node_id}"));
                        reply(
                            app,
                            token,
                            chat,
                            format!("已静音 <b>{}</b> 到 {}。", esc(&name), clock(until)),
                        )
                        .await;
                    }
                    Err(e) => reply(app, token, chat, format!("没能改静音：{}", esc(&e.to_string()))).await,
                }
                return;
            }
        }
    }
    // 「详情」：`node:<id>` —— 把 `/node` 的输出直接发过来。
    if let Some(rest) = data.strip_prefix("node:") {
        if let Ok(node_id) = rest.parse::<i64>() {
            let snapshot = frame(app);
            if let Some(name) = nodes_of(&snapshot)
                .into_iter()
                .find(|n| n["id"].as_i64() == Some(node_id))
                .and_then(|n| n["name"].as_str().map(str::to_string))
            {
                let _ = answer(app, token, id, "").await;
                let text = node_text(&snapshot, &name);
                reply(app, token, chat, text).await;
            }
        }
        return;
    }
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

/// 把超长消息**按行**截到 4000 字符以内，并补齐可能被截断的 `<pre>`。
/// 整条超过 4096 时 Telegram 是**整条拒收**，命令看起来像"没反应" —— 那比截断糟得多。
fn fit(text: &str) -> String {
    const MAX: usize = 4_000;
    if text.chars().count() <= MAX {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for line in text.lines() {
        let w = line.chars().count() + 1;
        // 给 `</pre>` 与那句截断提示留出预算，否则补完尾巴又超了。
        if used + w > MAX - 40 {
            break;
        }
        out.push_str(line);
        out.push('\n');
        used += w;
    }
    if out.matches("<pre>").count() > out.matches("</pre>").count() {
        out.push_str("</pre>\n");
    }
    out.push_str("…（太长了，已截断）");
    out
}

async fn post_message(
    app: &App,
    token: &str,
    chat: &str,
    text: &str,
    keyboard: Option<Value>,
) -> Result<i32> {
    let url = format!("{}/bot{token}/sendMessage", api_base());
    let mut body = json!({ "chat_id": chat, "text": fit(text), "parse_mode": "HTML" });
    if let Some(markup) = keyboard {
        body["reply_markup"] = markup;
    }
    let value: Value = app
        .http
        .post(&url)
        .json(&body)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(strip)?
        .error_for_status()
        .map_err(strip)?
        .json()
        .await
        .map_err(strip)?;
    Ok(value.pointer("/result/message_id").and_then(Value::as_i64).unwrap_or(0) as i32)
}

/// 改一条已发出的消息。`message is not modified` 当成功 —— 内容一样时 Telegram 会这么回。
async fn edit_message(
    app: &App,
    token: &str,
    chat: &str,
    id: i32,
    text: &str,
    keyboard: Option<Value>,
) -> Result<bool> {
    let url = format!("{}/bot{token}/editMessageText", api_base());
    let mut body = json!({
        "chat_id": chat, "message_id": id, "text": fit(text), "parse_mode": "HTML"
    });
    if let Some(markup) = keyboard {
        body["reply_markup"] = markup;
    }
    let response =
        app.http.post(&url).json(&body).timeout(Duration::from_secs(15)).send().await.map_err(strip)?;
    if response.status().is_success() {
        return Ok(true);
    }
    let _ = response.text().await;
    // 改不动（含"内容一字未变"）都返回 false —— "屏幕上什么都没变"无法与"命令没被处理"区分。
    Ok(false)
}

/// 菜单按钮点出来的结果：**编辑被点的那条消息**，并挂一颗「返回菜单」。
/// 一次点击只占一条消息的位置，不会在群里堆一串列表；点「返回菜单」再改回来。
async fn menu_result(app: &Shared, token: &str, chat: &str, query: &Value, text: String, keyboard: Value) {
    let id = query.pointer("/message/message_id").and_then(Value::as_i64).unwrap_or(0) as i32;
    if id != 0 && matches!(edit_message(app, token, chat, id, &text, Some(keyboard.clone())).await, Ok(true))
    {
        return;
    }
    if let Err(e) = post_message(app, token, chat, &text, Some(keyboard)).await {
        debug!("telegram sendMessage: {e:#}");
    }
}

/// 结果页那排按钮：只有一颗「返回菜单」。
fn back_keyboard() -> Value {
    json!({ "inline_keyboard": [[
        { "text": "← 返回菜单", "callback_data": "cmd:menu" }
    ]] })
}

/// `/start` 那条：品牌 + 命令表 + 菜单按钮。`/菜单` 回来时也用它。
async fn menu(app: &Shared, token: &str, chat: &str, query: Option<&Value>) {
    let site =
        setting(app, "site_name").filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "Monitor".to_string());
    let text = help_text(&site);
    match query {
        // 回来时把**菜单那排按钮**装回去，而不是又一个「返回菜单」。
        Some(q) => menu_result(app, token, chat, q, text, menu_keyboard()).await,
        None => reply_with(app, token, chat, text, Some(menu_keyboard())).await,
    }
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

/// `/start`：品牌 + 一张等宽对齐的命令表。
fn help_text(site: &str) -> String {
    let w = COMMANDS.iter().map(|(c, _)| width(c)).max().unwrap_or(0);
    let mut pre = String::new();
    for (cmd, about) in COMMANDS {
        pre.push_str(&blank_to(cmd, w));
        pre.push_str("  ");
        pre.push_str(about);
        pre.push('\n');
    }
    format!("<b>{}</b> TGBot · 运维命令\n<pre>{}</pre>", esc(site), esc(pre.trim_end()))
}

/// `/nodes`：**编号 + 对齐的三列**（名字 / 版本 / CPU·内存）。
/// 编号是给 `/node 3` 用的 —— 名字里有中文和连字符，手打容易错。
fn nodes_text(frame: &Value) -> String {
    let nodes = nodes_of(frame);
    if nodes.is_empty() {
        return "还没有节点。发 /start 看命令。".into();
    }
    let total = nodes.len();
    let online = nodes.iter().filter(|n| n["online"] == true).count();
    let shown: Vec<&Value> = nodes.iter().take(MAX_LINES).collect();
    let iw = shown.len().to_string().len().max(2);
    // 名字列**限宽**：手机上整行超过约 40 列会折行，折行之后对齐就没了。
    let nw = shown.iter().map(|n| width(n["name"].as_str().unwrap_or("?"))).max().unwrap_or(0).clamp(8, 16);
    let mut pre = String::new();
    // 列名。Telegram 没有表格，只有等宽块 —— 没有表头就只能靠猜（维护者的原话）。
    pre.push_str(&format!(
        "{:>iw$}   {}  {} {}\n",
        "#",
        blank_to("节点", nw),
        blank_to("版本", 6),
        "CPU / 内存",
        iw = iw
    ));
    for (i, node) in shown.iter().enumerate() {
        let online = node["online"] == true;
        let name = node["name"].as_str().unwrap_or("?");
        let version = node["agent_version"].as_str().unwrap_or("");
        let metrics = node.get("metrics").filter(|m| !m.is_null());
        let cpu = metrics.and_then(|m| m.get("cpu")).and_then(Value::as_f64);
        let mem = metrics.and_then(|m| {
            let used = m.get("mem_used")?.as_f64()?;
            let total = m.get("mem_total")?.as_f64()?;
            (total > 0.0).then(|| used / total * 100.0)
        });
        let usage = match (cpu, mem) {
            // 11 列 = "  0% /  15%"。离线那行的「—」右对齐到同一列，看起来才像一张表。
            (Some(c), Some(m)) => format!("{c:>3.0}% / {m:>3.0}%"),
            _ => blank_left("—", 11),
        };
        pre.push_str(&format!("{:>iw$} ", i + 1, iw = iw));
        pre.push_str(if online { "● " } else { "○ " });
        pre.push_str(&blank_to(&esc(&clip(name, nw)), nw));
        pre.push_str("  ");
        pre.push_str(&blank_to(&esc(if version.is_empty() { "?" } else { version }), 6));
        pre.push(' ');
        pre.push_str(&usage);
        pre.push('\n');
    }
    // 表头就是「集群概览」：数量 + 待升级 + 最新版本。
    // ⚠️ 原来 `/status` 是单独一条命令，而它说的三件事里有**两件**这个表头已经说了 ——
    // 两处维护同一句话，早晚会说成两样。现在合成一处，`/status` 留作别名。
    let outdated = nodes.iter().filter(|n| n["agent_old"] == true).count();
    let latest = frame.get("agent_latest").and_then(Value::as_str).unwrap_or("");
    let mut out = format!(
        "<b>集群 {total} 台</b> · 在线 <b>{online}</b> · 离线 <b>{}</b> · {}",
        total - online,
        fetched()
    );
    if outdated > 0 {
        out.push_str(&format!("\n待升级 <b>{outdated}</b> 台 · 发 <code>/upgrade</code> 看是哪几台"));
        if !latest.is_empty() {
            out.push_str(&format!(" · 最新 <code>{}</code>", esc(latest)));
        }
    }
    out.push_str(&format!("\n<pre>{}</pre>", pre.trim_end()));
    if total > MAX_LINES {
        out.push_str(&format!("\n…还有 {} 台，去面板看全部。", total - MAX_LINES));
    }
    // 分组名**列出来**：不然读者不知道该拿什么去筛（维护者的原话："从哪里知道有哪些组"）。
    let mut groups: Vec<(&str, usize)> = Vec::new();
    for node in &nodes {
        let Some(g) = node["group"].as_str().filter(|g| !g.is_empty()) else { continue };
        match groups.iter_mut().find(|(name, _)| *name == g) {
            Some((_, n)) => *n += 1,
            None => groups.push((g, 1)),
        }
    }
    groups.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    if !groups.is_empty() {
        let list: Vec<String> = groups.iter().map(|(g, n)| format!("{} {}", esc(g), n)).collect();
        out.push_str(&format!("\n分组：{}", list.join(" · ")));
    }
    out.push_str("\n用 <code>/node</code> 加编号看单台，例如 <code>/node 1</code>。");
    out
}

/// 找节点：**先认编号**（`/nodes` 里那个），再认名字的一部分。
fn find_node(frame: &Value, wanted: &str) -> Option<Value> {
    let nodes = nodes_of(frame);
    let t = wanted.trim();
    if let Ok(n) = t.parse::<usize>() {
        if n >= 1 {
            if let Some(node) = nodes.get(n - 1) {
                return Some(node.clone());
            }
        }
    }
    let needle = t.to_lowercase();
    nodes.into_iter().find(|n| n["name"].as_str().unwrap_or("").to_lowercase().contains(&needle))
}

/// `/node`：标题一行 + **标签对齐的等宽块**。
/// 原来是一长串 `·` 分隔的行，读者要在句子里找数字；改成"标签一列、值一列"之后，
/// 同一台机器的 CPU / 内存 / 磁盘 / 流量 / 到期可以竖着扫。
fn node_text(frame: &Value, wanted: &str) -> String {
    if wanted.trim().is_empty() {
        return "用法：/node &lt;名字或编号&gt;（编号就是 /nodes 里的序号）".into();
    }
    let Some(node) = find_node(frame, wanted) else {
        return format!("没找到「{}」。发 /nodes 看编号。", esc(wanted.trim()));
    };
    let name = node["name"].as_str().unwrap_or("?");
    let online = node["online"] == true;
    let mut rows: Vec<(&str, String)> = Vec::new();
    if let Some(v) = node["agent_version"].as_str().filter(|v| !v.is_empty()) {
        rows.push(("agent", v.to_string()));
    }
    if let Some(os) = node["os"].as_str().filter(|v| !v.is_empty()) {
        rows.push(("系统", clip(os, 30)));
    }
    if let Some(m) = node.get("metrics").filter(|m| !m.is_null()) {
        let pct = |key: &str, total: &str| {
            let used = m.get(key).and_then(Value::as_f64)?;
            let total = m.get(total).and_then(Value::as_f64)?;
            (total > 0.0).then(|| format!("{:.0}%", used / total * 100.0))
        };
        if let Some(cpu) = m.get("cpu").and_then(Value::as_f64) {
            rows.push(("CPU", format!("{cpu:.0}%")));
        }
        if let Some(v) = pct("mem_used", "mem_total") {
            rows.push(("内存", v));
        }
        if let Some(v) = pct("disk_used", "disk_total") {
            rows.push(("磁盘", v));
        }
    }
    rows.push(("今日", format!("↓{} ↑{}", bytes(num(&node, "day_rx")), bytes(num(&node, "day_tx")))));
    rows.push(("本月", format!("↓{} ↑{}", bytes(num(&node, "month_rx")), bytes(num(&node, "month_tx")))));
    if let Some(at) = node["expires_at"].as_str().filter(|v| !v.is_empty()) {
        rows.push((
            "到期",
            match node.get("expires_in").and_then(Value::as_f64) {
                Some(days) => format!("{}（{days:.0} 天）", at),
                None => at.to_string(),
            },
        ));
    }
    let w = rows.iter().map(|(k, _)| width(k)).max().unwrap_or(0);
    let mut pre = String::new();
    for (label, value) in &rows {
        pre.push_str(&blank_to(label, w));
        pre.push_str("  ");
        pre.push_str(&esc(value));
        pre.push('\n');
    }
    format!("<b>{}</b> · {}\n<pre>{}</pre>", esc(name), if online { "在线" } else { "离线" }, pre.trim_end())
}

/// `2h` / `30m` / `1d` / 裸数字（分钟）→ 秒。上限 30 天：静音到"永久"等于把告警关掉，
/// 那件事应该去面板里做，而不是在聊天窗口里手滑。
fn parse_span(s: &str) -> Option<i64> {
    let s = s.trim();
    let (num, mul) = match s.strip_suffix(['m', 'h', 'd']) {
        Some(_) => {
            let unit = s.chars().last()?;
            let mul = match unit {
                'm' => 60,
                'h' => 3600,
                _ => 86400,
            };
            (&s[..s.len() - 1], mul)
        }
        None => (s, 60),
    };
    let n: i64 = num.parse().ok()?;
    (n > 0).then(|| (n * mul).min(30 * 86_400))
}

/// 快照类回复的"取数时间"。既是信息（这份数据多新），也保证同一个按钮再点一次时
/// 编辑**真的会有变化** —— 否则 Telegram 回 `message is not modified`。
fn fetched() -> String {
    format!("{} 的数据", chrono::Local::now().format("%H:%M"))
}

fn clock(at: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_opt(at, 0)
        .single()
        .map(|t| t.format("%m-%d %H:%M").to_string())
        .unwrap_or_else(|| at.to_string())
}

/// 现在的静音名单。
fn muted_text(frame: &Value) -> String {
    let now = chrono::Utc::now().timestamp();
    let all = nodes_of(frame);
    let muted: Vec<&Value> = all.iter().filter(|n| n["muted_until"].as_i64().unwrap_or(0) > now).collect();
    if muted.is_empty() {
        return "现在没有静音的节点。静音：<code>/mute 3 2h</code>".into();
    }
    let nw = muted.iter().map(|n| width(n["name"].as_str().unwrap_or("?"))).max().unwrap_or(0).clamp(8, 16);
    let mut pre = String::new();
    for (i, node) in muted.iter().enumerate() {
        let until = node["muted_until"].as_i64().unwrap_or(0);
        pre.push_str(&format!("{:>2} ", i + 1));
        pre.push_str(&blank_to(&esc(&clip(node["name"].as_str().unwrap_or("?"), nw)), nw));
        pre.push_str("  到 ");
        pre.push_str(&clock(until));
        pre.push('\n');
    }
    format!("<b>静音中 {} 台</b> · {}\n<pre>{}</pre>", muted.len(), fetched(), pre.trim_end())
}

/// 一台待升级节点的处置方式，**短标签**（表格里放不下 `upgrade_blocker` 的长句）。
fn upgrade_tag(node: &Value) -> &'static str {
    if node["online"] != true {
        "离线"
    } else if node["allow_remote_upgrade"] != true {
        "仅手动"
    } else {
        "可升级"
    }
}

/// `/upgrade` 不带参数：**哪些节点的 agent 需要更新**。
/// 原来只有 `/upgrade <名字>`，而"谁该升"只能自己拿 `/nodes` 的版本号去比 —— 这条补上缺口。
/// 顺序按"能动手的排前面"：可升级 → 仅手动 → 离线。
fn upgrade_list(frame: &Value) -> String {
    let nodes = nodes_of(frame);
    let latest = frame.get("agent_latest").and_then(Value::as_str).unwrap_or("");
    let mut stale: Vec<&Value> = nodes.iter().filter(|n| n["agent_old"] == true).collect();
    if stale.is_empty() {
        // ⚠️ `agent_latest` 为空 = **hub 还没查过**（更新页没点过"检查"）。
        // 这时说"都是最新的"是句无法验证的话 —— 与"沉默的省略 = 假信息"是同一类错。
        return if latest.is_empty() {
            "没有待升级的节点。\n不过 hub 还没拿到最新版本号 —— 去面板的「更新」页点一次检查。".into()
        } else {
            format!("所有节点的 agent 都是最新的（<code>{}</code>）。", esc(latest))
        };
    }
    stale.sort_by_key(|n| match upgrade_tag(n) {
        "可升级" => 0,
        "仅手动" => 1,
        _ => 2,
    });
    let iw = stale.len().to_string().len().max(2);
    let nw = stale.iter().map(|n| width(n["name"].as_str().unwrap_or("?"))).max().unwrap_or(0).clamp(8, 16);
    let mut pre = String::new();
    for (i, node) in stale.iter().enumerate() {
        let name = node["name"].as_str().unwrap_or("?");
        let version = node["agent_version"].as_str().filter(|v| !v.is_empty()).unwrap_or("?");
        pre.push_str(&format!("{:>iw$} ", i + 1, iw = iw));
        pre.push_str(&blank_to(&esc(&clip(name, nw)), nw));
        pre.push_str("  ");
        pre.push_str(&blank_to(&esc(version), 6));
        pre.push(' ');
        pre.push_str(upgrade_tag(node));
        pre.push('\n');
    }
    let mut out = format!(
        "<b>待升级 {} 台</b>{} · {}",
        stale.len(),
        if latest.is_empty() { String::new() } else { format!(" · 最新 <code>{}</code>", esc(latest)) },
        fetched()
    );
    out.push_str(&format!("\n<pre>{}</pre>", pre.trim_end()));
    out.push_str(
        "\n发 <code>/upgrade 编号</code> 升级（会先弹确认）。「仅手动」要去那台机器上重跑安装命令。",
    );
    out
}

/// 命令 → 文本。文字命令和按钮回调**共用这一张表**，免得两边各写一遍、慢慢长歪。
fn render(cmd: &str, frame: &Value) -> Option<String> {
    match cmd {
        // `/status` 保留为**别名**：老习惯还在，但不单独维护一份内容。
        "status" | "nodes" => Some(nodes_text(frame)),
        "traffic" => Some(traffic_text(frame)),
        "probe" => Some(probe_text(frame)),
        "upgrade" => Some(upgrade_list(frame)),
        _ => None,
    }
}

/// `/start` 底部那排按钮。`callback_data` 用 `cmd:<命令>` —— 与文字命令走同一个 `render`。
fn menu_keyboard() -> Value {
    let button = |label: &str, cmd: &str| json!({ "text": label, "callback_data": format!("cmd:{cmd}") });
    json!({ "inline_keyboard": [
        [button("节点", "nodes"), button("流量", "traffic")],
        [button("流媒体", "probe"), button("待升级", "upgrade")],
    ] })
}

/// `/traffic`：**一张对齐的两列表**（今日 / 本月 × 下行 / 上行），后面跟本月前三。
/// 原来是一行 `今日 ↓x ↑y`，四个数字挤在一起，看不出哪个是哪个量级。
fn traffic_text(frame: &Value) -> String {
    let nodes = nodes_of(frame);
    let sum = |key: &str| nodes.iter().filter_map(|n| n.get(key).and_then(Value::as_f64)).sum::<f64>();
    let rows = [
        ("今日", bytes(sum("day_rx")), bytes(sum("day_tx"))),
        ("本月", bytes(sum("month_rx")), bytes(sum("month_tx"))),
    ];
    let dw = rows.iter().map(|(_, d, _)| width(d)).max().unwrap_or(0);
    let uw = rows.iter().map(|(_, _, u)| width(u)).max().unwrap_or(0);
    let lw = 4; // 「今日」「本月」都是 4 列
    let mut pre = String::new();
    pre.push_str(&blank_to("", lw));
    pre.push_str("  ");
    pre.push_str(&blank_left("↓ 下行", dw));
    pre.push_str("  ");
    pre.push_str(&blank_left("↑ 上行", uw));
    pre.push('\n');
    for (label, down, up) in &rows {
        pre.push_str(&blank_to(label, lw));
        pre.push_str("  ");
        pre.push_str(&blank_left(down, dw));
        pre.push_str("  ");
        pre.push_str(&blank_left(up, uw));
        pre.push('\n');
    }
    // ⚠️ 两段数据并进**一个** `<pre>`：Telegram 给每个代码块都画一条「复制」栏，
    // 两个块就是两条 —— 并起来只留一条，中间用一个空行分隔。
    let mut top: Vec<(&str, f64)> = nodes
        .iter()
        .map(|n| (n["name"].as_str().unwrap_or("?"), num(n, "month_rx") + num(n, "month_tx")))
        .collect();
    top.sort_by(|a, b| b.1.total_cmp(&a.1));
    let top: Vec<(&str, f64)> = top.into_iter().filter(|(_, total)| *total > 0.0).take(3).collect();
    if !top.is_empty() {
        pre.push('\n');
        pre.push_str("本月用量前三\n");
        let nw = top.iter().map(|(name, _)| width(name)).max().unwrap_or(0).clamp(8, 16);
        let vw = top.iter().map(|(_, total)| width(&bytes(*total))).max().unwrap_or(0);
        for (i, (name, total)) in top.iter().enumerate() {
            pre.push_str(&format!("{:>2} ", i + 1));
            pre.push_str(&blank_to(&esc(&clip(name, nw)), nw));
            pre.push_str("  ");
            pre.push_str(&blank_left(&bytes(*total), vw));
            pre.push('\n');
        }
    }
    format!("<b>流量</b> · {} 台 · {}\n<pre>{}</pre>", nodes.len(), fetched(), pre.trim_end())
}

/// 服务的**正式写法** —— 上报里是 `netflix` 这种键名，直接摆出来是"程序里的名字"。
fn service_name(key: &str) -> &str {
    match key {
        "netflix" => "Netflix",
        "youtube" => "YouTube",
        "chatgpt" => "ChatGPT",
        "tiktok" => "TikTok",
        "disney" => "Disney+",
        "prime" => "Prime Video",
        other => other,
    }
}

/// 固定顺序：同一台机器在两次 /probe 之间，服务顺序不该跟着 JSON 键序变。
fn service_rank(key: &str) -> u8 {
    match key {
        "netflix" => 0,
        "youtube" => 1,
        "chatgpt" => 2,
        "tiktok" => 3,
        _ => 9,
    }
}

/// `/probe`：编号 + 名字 + 服务结论。服务集合各台不同，所以**名字列对齐**、结论跟在后面，
/// 不强行做成矩阵（那会有一半是空格）。
fn probe_text(frame: &Value) -> String {
    let nodes = nodes_of(frame);
    let mut rows: Vec<(usize, String, String)> = Vec::new();
    for (i, node) in nodes.iter().enumerate() {
        let Some(probe) = node.get("probe").filter(|p| !p.is_null()) else { continue };
        let Some(services) = probe.as_object() else { continue };
        let mut parts: Vec<(u8, String)> = Vec::new();
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
            let region = if region.is_empty() { String::new() } else { format!("/{region}") };
            parts.push((service_rank(service), format!("{} {label}{region}", service_name(service))));
        }
        if !parts.is_empty() {
            parts.sort_by_key(|(rank, _)| *rank);
            let about = parts.into_iter().map(|(_, s)| s).collect::<Vec<_>>().join(" · ");
            rows.push((i + 1, node["name"].as_str().unwrap_or("?").to_string(), about));
        }
        if rows.len() >= MAX_LINES {
            break;
        }
    }
    if rows.is_empty() {
        return "还没有节点报过流媒体探测结果。".into();
    }
    let iw = rows.iter().map(|(i, _, _)| i.to_string().len()).max().unwrap_or(2).max(2);
    let nw = rows.iter().map(|(_, name, _)| width(name)).max().unwrap_or(0).clamp(8, 16);
    let mut pre = String::new();
    for (i, name, about) in &rows {
        pre.push_str(&format!("{:>iw$} ", i, iw = iw));
        pre.push_str(&blank_to(&esc(&clip(name, nw)), nw));
        pre.push_str("  ");
        pre.push_str(&esc(about));
        pre.push('\n');
    }
    format!("<b>流媒体探测</b> · 最近一次 · {}\n<pre>{}</pre>", fetched(), pre.trim_end())
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

    /// `/status` 已并进 `/nodes` 的表头（两处维护同一句话早晚会说成两样）。
    /// 这条测试同时盯两件事：数量对，且 `/status` 仍能拿到同样的内容。
    #[test]
    fn the_header_is_the_fleet_overview_and_status_is_an_alias() {
        let text = nodes_text(&frame());
        assert!(text.contains("<b>集群 2 台</b> · 在线 <b>1</b> · 离线 <b>1</b>"), "{text}");
        // 指个路：光说"待升级 N 台"，读者还得自己翻列表比版本
        assert!(
            text.contains(
                "待升级 <b>1</b> 台 · 发 <code>/upgrade</code> 看是哪几台 · 最新 <code>1.1.17</code>"
            ),
            "{text}"
        );
        assert_eq!(render("status", &frame()), Some(text), "/status 应当等于 /nodes");
    }

    #[test]
    fn the_list_is_numbered_aligned_and_carries_cpu_and_memory() {
        let text = nodes_text(&frame());
        // 编号 + 圆点 + 名字 + 版本 + 用量，一列都不许糊在一起
        assert!(text.contains("<b>集群 2 台</b> · 在线 <b>1</b> · 离线 <b>1</b>"), "{text}");
        assert!(text.contains("<pre>"), "整块要用等宽字体才对得齐：{text}");
        assert!(text.contains(" 1 ● hk-1"), "第 1 台：{text}");
        assert!(text.contains(" 2 ○ la-2"), "离线用空心点：{text}");
        assert!(text.contains("1.1.17"), "{text}");
        assert!(text.contains("  2% /  50%"), "CPU/内存右对齐：{text}");
        assert!(text.contains('—'), "没有指标就写破折号：{text}");
        assert!(text.contains("/node"), "页脚要告诉读者怎么按编号看单台：{text}");
    }

    /// 编号能直接喂给 `/node`（维护者要的"用序号代替名字"）。
    #[test]
    fn a_node_can_be_found_by_its_number() {
        assert!(node_text(&frame(), "1").contains("<b>hk-1</b>"));
        assert!(node_text(&frame(), " 2 ").contains("<b>la-2</b>"));
        // 超出范围就退回按名字找，而不是 panic
        assert!(node_text(&frame(), "9").contains("没找到"));
    }

    /// 中文 / 全角在等宽字体里占两格 —— 只按字符数补空格会让中文名错位。
    #[test]
    fn wide_characters_take_two_columns() {
        assert_eq!(width("hk-1"), 4);
        assert_eq!(width("阿里云"), 6);
        assert_eq!(width("（x）"), 5);
        assert_eq!(blank_to("云", 4), "云  ");
        assert_eq!(clip("腾讯云硅谷-PPANEL", 8), "腾讯云…");
    }

    /// `/start`：品牌名 + 等宽对齐的命令表（品牌取站点名）。
    #[test]
    fn the_help_is_branded_and_its_columns_line_up() {
        let text = help_text("SRVS");
        assert!(text.contains("<b>SRVS</b> TGBot · 运维命令"), "{text}");
        let body = text.split("<pre>").nth(1).unwrap().split("</pre>").next().unwrap();
        let w = COMMANDS.iter().map(|(c, _)| width(c)).max().unwrap();
        for (cmd, _) in COMMANDS {
            // 帮助文本整体过了 HTML 转义（`/node <名字>` → `/node &lt;名字&gt;`），断言也要转一次。
            let want = esc(&blank_to(cmd, w));
            assert!(body.lines().any(|l| l.starts_with(&want)), "「{cmd}」没补齐：{body}");
        }
    }

    /// `/upgrade` 不带参数 = "谁该升级"：只列落后的那台，并标出处置方式。
    #[test]
    fn upgrade_without_a_name_lists_who_is_behind() {
        let text = upgrade_list(&frame());
        assert!(text.contains("<b>待升级 1 台</b>"), "{text}");
        assert!(text.contains("la-2"), "落后的那台要点名：{text}");
        assert!(!text.contains("hk-1"), "已经最新的一台不该出现：{text}");
        assert!(text.contains("1.1.15"), "要显示它现在是什么版本：{text}");
        // 这台离线且没开远程升级 —— 先报"离线"（最要紧的那条）
        assert!(text.contains("离线"), "{text}");
        assert!(text.contains("/upgrade 编号"), "要告诉读者怎么升级：{text}");
    }

    /// 没有待升级节点、但 hub 也没查过最新版本时，**不能**声称"都是最新的"。
    #[test]
    fn without_a_known_latest_it_does_not_claim_everyone_is_current() {
        let f = json!({ "nodes": [ { "id": 1, "name": "a", "online": true, "agent_old": false } ] });
        let text = upgrade_list(&f);
        assert!(!text.contains("都是最新的"), "没拿到最新版本就不该下这个结论：{text}");
        assert!(text.contains("还没拿到最新版本号"), "{text}");
    }

    /// 超长消息要**截断**而不是让 Telegram 整条拒收；被截在 `<pre>` 里时要把标签补上。
    #[test]
    fn an_overlong_message_is_cut_at_a_line_and_keeps_its_tags_balanced() {
        let long = format!("<pre>{}</pre>", "行\n".repeat(3000));
        let fitted = fit(&long);
        assert!(fitted.chars().count() <= 4000, "长度 {}", fitted.chars().count());
        assert_eq!(fitted.matches("<pre>").count(), fitted.matches("</pre>").count());
        assert!(fitted.ends_with("已截断）"), "{fitted}");
        assert_eq!(fit("短"), "短", "没超长就原样返回");
    }

    /// 时长写法：`2h` / `30m` / `1d` / 裸数字（分钟）；上限 30 天。
    #[test]
    fn mute_spans_are_read_in_minutes_hours_and_days() {
        assert_eq!(parse_span("30m"), Some(1800));
        assert_eq!(parse_span("2h"), Some(7200));
        assert_eq!(parse_span("1d"), Some(86_400));
        assert_eq!(parse_span("90"), Some(5400), "裸数字 = 分钟");
        assert_eq!(parse_span("0h"), None, "0 不是时长");
        assert_eq!(parse_span("abc"), None);
        assert_eq!(parse_span("999d"), Some(30 * 86_400), "封顶 30 天");
    }

    /// `/muted` 只列静音中的，且带到期时刻。
    #[test]
    fn the_mute_list_only_shows_nodes_still_muted() {
        let now = chrono::Utc::now().timestamp();
        let f = json!({ "nodes": [
            { "id": 1, "name": "静音中的", "muted_until": now + 3600 },
            { "id": 2, "name": "已过期", "muted_until": now - 60 },
            { "id": 3, "name": "没静音", "muted_until": 0 }
        ]});
        let text = muted_text(&f);
        assert!(text.contains("静音中的"), "{text}");
        assert!(!text.contains("已过期") && !text.contains("没静音"), "只列还在静音的：{text}");
        assert!(muted_text(&json!({ "nodes": [] })).contains("没有静音"));
    }

    /// 静音与详情按钮：只挂在**单台**的离线/恢复通知上。
    #[test]
    fn alert_buttons_only_ride_on_single_node_offline_alerts() {
        use crate::notify::Note;
        let one = Note { event: "offline", ids: vec![7], ..Default::default() };
        let kb = crate::notify::alert_buttons(&one).expect("单台离线应当有按钮");
        let rows = kb["inline_keyboard"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0]["callback_data"], "mute:60:7");
        assert_eq!(rows[0][1]["callback_data"], "node:7");

        let many = Note { event: "offline", ids: vec![1, 2, 3], ..Default::default() };
        assert!(crate::notify::alert_buttons(&many).is_none(), "多台合并的那条不挂按钮");
        let other = Note { event: "traffic", ids: vec![7], ..Default::default() };
        assert!(crate::notify::alert_buttons(&other).is_none(), "流量告警不挂");
    }

    /// 时长的三种量级。
    #[test]
    fn spans_read_as_days_hours_or_minutes() {
        assert_eq!(span(0), "刚刚");
        assert_eq!(span(90), "1 分钟");
        assert_eq!(span(3 * 3600), "3 小时");
        assert_eq!(span(3 * 86_400 + 7 * 3600), "3 天 7 小时");
    }

    /// 月成本：按周期摊平再折成人民币；缺价格或缺汇率是 `None`，不是 0。
    #[test]
    fn monthly_cost_flattens_the_cycle_and_converts_the_currency() {
        let mut rates = std::collections::HashMap::new();
        rates.insert("CNY".to_string(), 6.68_f64);
        rates.insert("EUR".to_string(), 0.92_f64);
        let usd = json!({ "price": 12.5, "currency": "USD", "billing_cycle": "monthly" });
        assert!((monthly_cny(&usd, &rates).unwrap() - 12.5 * 6.68).abs() < 0.001);
        let yearly = json!({ "price": 340.0, "currency": "CNY", "billing_cycle": "yearly" });
        assert!((monthly_cny(&yearly, &rates).unwrap() - 340.0 / 12.0).abs() < 0.001);
        let eur = json!({ "price": 4.5, "currency": "EUR", "billing_cycle": "monthly" });
        assert!((monthly_cny(&eur, &rates).unwrap() - 4.5 * 6.68 / 0.92).abs() < 0.001);
        // 没填价格 / 币种不在汇率表里 ⇒ 不知道，不是 0
        assert!(monthly_cny(&json!({ "price": 0.0 }), &rates).is_none());
        assert!(monthly_cny(&json!({ "price": 5.0, "currency": "XYZ" }), &rates).is_none());
    }

    /// 到期列表：默认 30 天，按天数升序，超出窗口的不列。
    #[test]
    fn expiry_lists_only_what_falls_inside_the_window() {
        let f = json!({ "nodes": [
            { "name": "far", "expires_at": "2027-08-19", "expires_in": 298.0 },
            { "name": "soon", "expires_at": "2026-10-30", "expires_in": 3.0 },
            { "name": "mid", "expires_at": "2026-11-17", "expires_in": 21.0 }
        ]});
        let text = expiry_text(&f, "");
        assert!(text.contains("30 天内到期 2 台"), "{text}");
        assert!(text.contains("soon") && text.contains("mid"), "{text}");
        assert!(!text.contains("far"), "298 天的别列出来：{text}");
        // 天数升序：soon 在 mid 前面
        assert!(text.find("soon") < text.find("mid"), "{text}");
        assert!(expiry_text(&f, "365").contains("3 台"), "窗口可以放宽");
    }

    /// `/whoami`：站点名、hub 版本、会话 id、静音数 —— 排障时先看这个。
    #[test]
    fn whoami_names_the_site_the_version_and_the_chat() {
        let text = whoami_body("SRVS", env!("CARGO_PKG_VERSION"), "424242", "day,week", 1);
        assert!(text.contains("SRVS"), "{text}");
        assert!(text.contains(env!("CARGO_PKG_VERSION")), "{text}");
        assert!(text.contains("424242"), "{text}");
        assert!(text.contains("静音 1 台"), "{text}");
        assert!(whoami_body("X", "1.0", "1", "", 0).contains("未开"), "没开报告要说清楚");
    }

    /// 菜单按钮：每个 `callback_data` 都必须能被 `render` 接住 —— 否则是个死按钮。
    #[test]
    fn every_menu_button_maps_to_a_real_command() {
        let kb = menu_keyboard();
        let rows = kb["inline_keyboard"].as_array().expect("inline_keyboard");
        let mut seen = 0;
        for row in rows {
            for button in row.as_array().expect("row") {
                let data = button["callback_data"].as_str().expect("callback_data");
                let cmd = data.strip_prefix("cmd:").expect("cmd: 前缀");
                assert!(render(cmd, &frame()).is_some(), "按钮 {cmd} 没有对应的 render");
                seen += 1;
            }
        }
        assert_eq!(seen, 4, "菜单按钮数量");
    }

    #[test]
    fn a_node_is_found_by_part_of_its_name() {
        let text = node_text(&frame(), "HK");
        assert!(text.contains("<b>hk-1</b> · 在线"), "{text}");
        assert!(text.contains("<pre>"), "标签与值要在等宽块里：{text}");
        assert!(text.contains("agent  1.1.17"), "{text}");
        assert!(text.contains("系统   Debian 12"), "{text}");
        assert!(text.contains("CPU    2%"), "{text}");
        assert!(text.contains("内存   50%"), "{text}");
        assert!(text.contains("磁盘   40%"), "{text}");
        assert!(text.contains("今日   ↓1.20 GB ↑300 MB"), "{text}");
        assert!(text.contains("到期   2027-03-14（147 天）"), "{text}");
        assert!(node_text(&frame(), "没有这台").contains("没找到"));
        assert!(node_text(&frame(), "").contains("用法"));
    }

    #[test]
    fn traffic_is_a_two_column_table_with_the_top_three_under_it() {
        let text = traffic_text(&frame());
        assert!(text.contains("<pre>"), "两列数字要在等宽块里：{text}");
        assert!(text.contains("今日"), "{text}");
        assert!(text.contains("1.20 GB") && text.contains("300 MB"), "{text}");
        assert!(text.contains("本月"), "{text}");
        assert!(text.contains("30.0 GB") && text.contains("8.00 GB"), "{text}");
        assert!(text.contains("本月用量前三"), "{text}");
        assert!(text.contains("hk-1") && text.contains("38.0 GB"), "{text}");
        // 数字**右对齐**：两行同一个数量级时，"GB" 落在同一列
        let rows: Vec<&str> = text.lines().filter(|l| l.contains("GB")).collect();
        assert!(rows.len() >= 2, "{text}");
    }

    #[test]
    fn probes_are_translated_into_words() {
        let text = probe_text(&frame());
        // 服务名用**正式写法**（上报里是 netflix 这样的键名），且顺序固定
        assert!(text.contains("Netflix 解锁/US"), "{text}");
        assert!(text.contains("ChatGPT 仅网页"), "{text}");
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
