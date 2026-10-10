//! 流量报告的定时投递。
//!
//! 一条"到点该发哪些周期"的判断 + 组文本 + **交给现有 notify 队列** ✓ ——
//! 报告不是新通道 ✓，它就是一条 `Note` ✓（退避、重试、渠道、日志全部现成 ✓）。

use chrono::{Duration, NaiveDate, Utc};

use crate::db::Period;
use crate::notify::{self, Note};
use crate::{App, Shared};

/// 设置键：面板与调度读同一套 ✓（面板那一节是最后一块 ✓，先用现有 settings 接口设值也能跑 ✓）。
pub const K_PERIODS: &str = "report_periods";
pub const K_TIME: &str = "report_time";
pub const K_TZ: &str = "report_tz";
/// 最近一次已入队的 `"<日期> <周期>"` ✓ —— **重启也不重发** ✓。
pub const K_LAST: &str = "report_last_sent";

/// 30 秒看一次表 ✓：报告只要**分钟级**精度 ✓，而"是否已发"由 [`K_LAST`] 记住 ✓
///（不靠"恰好那一秒只跑一次"这种脆弱假设 ✗）。
const TICK: std::time::Duration = std::time::Duration::from_secs(30);

/// `+08:00` / `-05:30` → 分钟数 ✓。认不出来就用 **+08:00**（维护者定的默认 ✓）。
fn offset_minutes(tz: &str) -> i32 {
    let t = tz.trim();
    let (sign, rest) = match t.strip_prefix('-') {
        Some(r) => (-1, r),
        None => (1, t.strip_prefix('+').unwrap_or(t)),
    };
    match rest.split_once(':') {
        Some((h, m)) => match (h.trim().parse::<i32>(), m.trim().parse::<i32>()) {
            (Ok(h), Ok(m)) if (0..=23).contains(&h) && (0..=59).contains(&m) => sign * (h * 60 + m),
            _ => 8 * 60,
        },
        None => 8 * 60,
    }
}

/// 严格校验：`HH:MM` ✓（**与 `offset_minutes` 不同** ✗ —— 那个是"坏值就退默认" ✓，
/// 而校验要的是"坏值就拒绝" ✓。两者用途不同，不能共用一个函数 ✓）。
pub fn valid_time(v: &str) -> bool {
    match v.split_once(':') {
        Some((h, m)) => {
            matches!((h.trim().parse::<u32>(), m.trim().parse::<u32>()), (Ok(h), Ok(m)) if h < 24 && m < 60)
        }
        None => false,
    }
}

/// 严格校验：`±HH:MM` 且在 ±14:00 内 ✓（空串表示"用默认 +08:00" ✓，所以也算合法 ✓）。
pub fn valid_tz(v: &str) -> bool {
    if v.trim().is_empty() {
        return true;
    }
    let t = v.trim();
    if !(t.starts_with('+') || t.starts_with('-')) {
        return false;
    }
    match t[1..].split_once(':') {
        Some((h, m)) => match (h.trim().parse::<i32>(), m.trim().parse::<i32>()) {
            (Ok(h), Ok(m)) => h <= 14 && m < 60 && (h < 14 || m == 0),
            _ => false,
        },
        None => false,
    }
}

/// 严格校验：逗号分隔，元素只能是那四个（**空串合法** ✓ = 一个周期都不发 ✓）。
pub fn valid_periods(v: &str) -> bool {
    v.split(',').all(|p| matches!(p.trim(), "" | "day" | "week" | "month" | "quarter" | "half" | "year"))
}

fn label(minutes: i32) -> String {
    let (s, m) = if minutes < 0 { ('-', -minutes) } else { ('+', minutes) };
    format!("UTC{s}{:02}:{:02}", m / 60, m % 60)
}

fn parse_periods(s: &str) -> Vec<Period> {
    let mut out = Vec::new();
    for part in s.split(',') {
        let p = match part.trim() {
            "day" => Some(Period::Day),
            "week" => Some(Period::Week),
            "month" => Some(Period::Month),
            "quarter" => Some(Period::Quarter),
            "half" => Some(Period::Half),
            "year" => Some(Period::Year),
            _ => None,
        };
        if let Some(p) = p {
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out
}

fn key(p: Period) -> &'static str {
    match p {
        Period::Day => "day",
        Period::Week => "week",
        Period::Month => "month",
        Period::Quarter => "quarter",
        Period::Half => "half",
        Period::Year => "year",
    }
}

/// 到点就发 ✓。**每 30 秒检查一次**，是否已发由 `report_last_sent` 决定 ✓。
pub async fn watch(app: Shared) {
    loop {
        if let Err(e) = tick(&app) {
            tracing::warn!("traffic report tick failed: {e:#}");
        }
        tokio::time::sleep(TICK).await;
    }
}

fn tick(app: &App) -> anyhow::Result<()> {
    let offset = offset_minutes(&app.db.get(K_TZ).unwrap_or_default());
    let now = Utc::now() + Duration::minutes(offset as i64);
    let want = app.db.get(K_TIME).unwrap_or_else(|| "09:00".to_string());
    if now.format("%H:%M").to_string() != want.trim() {
        return Ok(());
    }
    let today: NaiveDate = now.date_naive();
    let periods = parse_periods(&app.db.get(K_PERIODS).unwrap_or_default());
    if periods.is_empty() {
        return Ok(()); // 一个周期都没勾 ⇒ 什么都不发 ✓（不是"默认全发" ✗）
    }
    let sent = app.db.get(K_LAST).unwrap_or_default();
    for p in periods {
        let stamp = format!("{} {}", today, key(p));
        if sent == stamp {
            continue; // 这个周期今天已经发过 ✓（重启也不会重发 ✓）
        }
        let (from, to, prev_from, prev_to) = p.last_full(today);
        let rows = report_rows(app, &from, &to);
        let prev = report_rows(app, &prev_from, &prev_to)
            .into_iter()
            .map(|r| (r.name, r.rx, r.tx))
            .collect::<Vec<_>>();
        let title = crate::report::title(p, &from, &to, &label(offset));
        // **覆盖说明拼在末尾** ✓（脚注的位置 ✓）—— 只在"确实不全"时才有内容 ✓（见它的说明 ✓）。
        let message = match crate::report::coverage_note(&from, &to, app.db.earliest_traffic_day().as_deref())
        {
            Some(note) => format!("{}\n\n{note}", crate::report::body(&rows, &prev)),
            None => crate::report::body(&rows, &prev),
        };
        notify::send(
            app,
            Note { event: "traffic", node: String::new(), title, message, ..Default::default() },
        );
        app.db.set(K_LAST, &stamp)?;
    }
    Ok(())
}

/// 取一个区间的逐节点用量 ✓，并补齐报告需要的两样：**月度额度** ✓ 与**本月已用** ✓
///（超限提醒按月算 ✓ —— 见 `report::body` 的说明 ✓）。
fn report_rows(app: &App, from: &str, to: &str) -> Vec<crate::report::Row> {
    // 额度由 `Db::traffic_limits` 提供 ✓ —— 走 `Db` 的用途明确的方法 ✓，
    // 而不是在这里直接拿连接 ✗（`conn` 是私有的 ✓，而且那样会把"表长什么样"漏到报告这一层 ✓）。
    let limits: std::collections::HashMap<i64, i64> = app.db.traffic_limits();
    let month_used: std::collections::HashMap<i64, i64> =
        app.db.all_traffic().into_iter().map(|(id, t)| (id, t.month_rx + t.month_tx)).collect();
    app.db
        .traffic_sums(from, to)
        .into_iter()
        .map(|(id, name, rx, tx)| crate::report::Row {
            name,
            rx,
            tx,
            limit: limits.get(&id).copied().unwrap_or(0),
            month_used: month_used.get(&id).copied().unwrap_or(0),
        })
        .collect()
}
