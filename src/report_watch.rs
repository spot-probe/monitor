//! 流量报告的定时投递。
//!
//! 一条"到点该发哪些周期"的判断（**先看今天是不是它的上报日** ✓）+ 组文本 +
//! **交给现有 notify 队列** ✓ ——
//! 报告不是新通道 ✓，它就是一条 `Note` ✓（退避、重试、渠道、日志全部现成 ✓）。

use chrono::{Duration, NaiveDate, Timelike, Utc};

use crate::db::Period;
use crate::notify::{self, Note};
use crate::{App, Shared};

/// 设置键：面板与调度读同一套 ✓（面板那一节是最后一块 ✓，先用现有 settings 接口设值也能跑 ✓）。
pub const K_PERIODS: &str = "report_periods";
pub const K_TIME: &str = "report_time";
pub const K_TZ: &str = "report_tz";
/// 最近一次已入队的 `"<日期> <周期>"`，**每个周期一个键** ✓。
/// ⚠️ 原来只有一个 `report_last_sent` ✗：同一天里发完周报再发季报，槽位就被季报覆盖了，
/// 30 秒后下一拍发现"周报今天还没发过" ⇒ **再发一遍**（多周期同时开启时必然发生）。
pub fn sent_key(p: Period) -> String {
    format!("report_last_{}", key(p))
}

/// 到点之后还能补多久（分钟）。重启或短暂停机不会因此丢掉一整周的报告；
/// 过了这个窗口就等下一个周期 —— 否则"下午才打开开关"会立刻收到一份日报，看着像又一个 bug。
const GRACE_MINUTES: i64 = 120;

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

/// **今天是不是这个周期的上报日** ✗✓。这是原来缺的那一块：界面写的是「周报 每周一 /
/// 月报 每月 1 号」，而调度只判断"今天这个周期发过没有" —— 每天日期都不同，于是**天天发**。
fn due(p: Period, today: NaiveDate) -> bool {
    use chrono::{Datelike, Weekday};
    match p {
        Period::Day => true,
        Period::Week => today.weekday() == Weekday::Mon,
        Period::Month => today.day() == 1,
        Period::Quarter => today.day() == 1 && matches!(today.month(), 1 | 4 | 7 | 10),
        Period::Half => today.day() == 1 && matches!(today.month(), 1 | 7),
        Period::Year => today.day() == 1 && today.month() == 1,
    }
}

/// `"09:00"` → 540。认不出来返回 `None`（设置被手改坏时不发，也不 panic ✓）。
fn slot_minutes(at: &str) -> Option<i64> {
    let (h, m) = at.trim().split_once(':')?;
    let h: i64 = h.trim().parse().ok()?;
    let m: i64 = m.trim().parse().ok()?;
    (h <= 23 && m < 60).then_some(h * 60 + m)
}

/// 现在这一分钟是否落在"到点之后、补发窗口之内"。
fn in_window(now_min: i64, slot: i64) -> bool {
    now_min >= slot && now_min < slot + GRACE_MINUTES
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
    let Some(slot) = slot_minutes(&want) else { return Ok(()) };
    // ⚠️ 原来是**精确等于那一分钟** ✗（`%H:%M` == 设置值）：hub 在那 60 秒里重启一次，
    // 这一整个周期的报告就没了。换成"到点之后两小时内都算"，代价是下午才打开开关不会立刻发。
    let now_min = i64::from(now.hour()) * 60 + i64::from(now.minute());
    if !in_window(now_min, slot) {
        return Ok(());
    }
    let today: NaiveDate = now.date_naive();
    let periods = parse_periods(&app.db.get(K_PERIODS).unwrap_or_default());
    if periods.is_empty() {
        return Ok(()); // 一个周期都没勾 ⇒ 什么都不发 ✓（不是"默认全发" ✗）
    }
    for p in periods {
        // **今天不是它的上报日就跳过** —— 这一行是"周报每天发"的修复。
        if !due(p, today) {
            continue;
        }
        let stamp = format!("{} {}", today, key(p));
        if app.db.get(&sent_key(p)).unwrap_or_default() == stamp {
            continue; // 这个周期今天已经发过 ✓（重启也不会重发 ✓）
        }
        let (from, to, prev_from, prev_to) = p.last_full(today);
        let rows = report_rows(app, &from, &to)?;
        let prev = report_rows(app, &prev_from, &prev_to)?
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
            // 报告正文自带 HTML（加粗的标题 + `<pre>` 表格），见 `report::body`。
            Note { event: "traffic", node: String::new(), title, message, html: true, ..Default::default() },
        );
        app.db.set(&sent_key(p), &stamp)?;
    }
    Ok(())
}

/// 取一个区间的逐节点用量 ✓，并补齐报告需要的两样：**月度额度** ✓ 与**本月已用** ✓
///（超限提醒按月算 ✓ —— 见 `report::body` 的说明 ✓）。
fn report_rows(app: &App, from: &str, to: &str) -> anyhow::Result<Vec<crate::report::Row>> {
    // 额度由 `Db::traffic_limits` 提供 ✓ —— 走 `Db` 的用途明确的方法 ✓，
    // 而不是在这里直接拿连接 ✗（`conn` 是私有的 ✓，而且那样会把"表长什么样"漏到报告这一层 ✓）。
    let limits: std::collections::HashMap<i64, i64> = app.db.traffic_limits();
    // ⚠️ 读不到就**不发这份报告**：一份把"本月已用"写成 0 的账单比不发更糟 —— 收件人会据此
    // 以为额度没用，而实际上可能是数据读不出来。
    let month_used: std::collections::HashMap<i64, i64> =
        app.db.all_traffic()?.into_iter().map(|(id, t)| (id, t.month_rx + t.month_tx)).collect();
    Ok(app
        .db
        .traffic_sums(from, to)
        .into_iter()
        .map(|(id, name, rx, tx)| crate::report::Row {
            name,
            rx,
            tx,
            limit: limits.get(&id).copied().unwrap_or(0),
            month_used: month_used.get(&id).copied().unwrap_or(0),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ⚠️ **这一条是线上报的 bug 的回归测试** ✗✓：维护者的原话是"周报、季报、半年报好像是
    /// 每天都会通知一次"。根因是调度只判断"今天这个周期发过没有"，而每天的日期都不同 ⇒
    /// 每个周期天天发。`due()` 之前**根本不存在**。
    #[test]
    fn weekly_is_due_only_on_monday() {
        let mon = NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
        let tue = NaiveDate::from_ymd_opt(2026, 10, 20).unwrap();
        assert!(due(Period::Week, mon));
        assert!(!due(Period::Week, tue));
    }

    #[test]
    fn monthly_quarterly_half_and_year_land_on_their_own_first_day() {
        let d = |y, m, day| NaiveDate::from_ymd_opt(y, m, day).unwrap();
        // 10 月 1 日：月报 + 季报（Q4 起），不是半年报、不是年报
        assert!(due(Period::Month, d(2026, 10, 1)));
        assert!(due(Period::Quarter, d(2026, 10, 1)));
        assert!(!due(Period::Half, d(2026, 10, 1)));
        assert!(!due(Period::Year, d(2026, 10, 1)));
        // 7 月 1 日：半年报 + 季报，月报也到；不是年报
        assert!(due(Period::Half, d(2026, 7, 1)));
        assert!(due(Period::Quarter, d(2026, 7, 1)));
        assert!(!due(Period::Year, d(2026, 7, 1)));
        // 1 月 1 日：四个都到
        for p in [Period::Month, Period::Quarter, Period::Half, Period::Year] {
            assert!(due(p, d(2026, 1, 1)), "{p:?} 应在 1 月 1 日发");
        }
        // 2 月 1 日：只有月报
        assert!(due(Period::Month, d(2026, 2, 1)));
        assert!(!due(Period::Quarter, d(2026, 2, 1)));
        // 不是 1 号就一个都不发（日报除外）
        let mid = d(2026, 10, 15);
        for p in [Period::Week, Period::Month, Period::Quarter, Period::Half, Period::Year] {
            assert!(!due(p, mid), "{p:?} 不该在月中发");
        }
        assert!(due(Period::Day, mid));
    }

    #[test]
    fn a_missed_minute_is_recovered_inside_the_window_only() {
        assert_eq!(slot_minutes("09:00"), Some(540));
        assert_eq!(slot_minutes("14:00"), Some(840));
        assert_eq!(slot_minutes("oops"), None);
        // 正好那一分钟 ✓
        assert!(in_window(540, 540));
        // 停机 90 分钟后回来 ✓（原来会整周期丢掉）
        assert!(in_window(540 + 90, 540));
        // 窗口之外不发 —— 否则"下午才打开开关"会立刻收到一份日报
        assert!(!in_window(540 + GRACE_MINUTES, 540));
        // 还没到点 ✓
        assert!(!in_window(539, 540));
    }

    /// 每个周期各记各的：同一天发了周报再发季报，不会把周报的"已发"覆盖掉。
    #[test]
    fn every_period_has_its_own_dedupe_slot() {
        let keys: Vec<String> =
            [Period::Day, Period::Week, Period::Month, Period::Quarter, Period::Half, Period::Year]
                .into_iter()
                .map(sent_key)
                .collect();
        assert_eq!(keys.len(), 6);
        assert_eq!(keys.iter().collect::<std::collections::HashSet<_>>().len(), 6);
        assert!(keys.contains(&"report_last_week".to_string()));
    }

    /// ⚠️ 这个模块此前是**全仓唯一零测试的模块** ✗，而 `valid_periods` / `valid_time` /
    /// `valid_tz` 是报告设置**唯一**的写入校验（`api.rs` 的 `save_settings` 直接调它们）。
    /// 写错的表现是"永远不发报告且无人知道"——正是那个文件注释里写明要挡住的那类静默失败。
    #[test]
    fn valid_time_accepts_only_hh_mm_within_a_day() {
        for ok in ["00:00", "09:05", "23:59", " 9:5 "] {
            assert!(valid_time(ok), "{ok:?} 应当合法");
        }
        for bad in ["24:00", "09:60", "9", "0900", "", "09:05:00", "abc", "-1:00"] {
            assert!(!valid_time(bad), "{bad:?} 应当被拒绝");
        }
    }

    /// ⚠️ **校验与换算的语义不同** ✗：`valid_tz` 是"坏值就拒绝"，而 `offset_minutes` 是
    /// "坏值就退默认 +08:00"。两者用途不同，不能共用——这条用例把两边的边界都钉住。
    #[test]
    fn valid_tz_rejects_what_offset_minutes_would_silently_default() {
        // 合法：空串（用默认）、±HH:MM、±14:00 上限
        for ok in ["", "+08:00", "-05:30", "+00:00", "+14:00", "-14:00"] {
            assert!(valid_tz(ok), "{ok:?} 应当合法");
        }
        // 非法：没符号、超过 ±14、分钟越界、形状不对
        for bad in ["08:00", "+15:00", "+14:01", "+08:60", "+8", "UTC+8", "+08"] {
            assert!(!valid_tz(bad), "{bad:?} 应当被拒绝");
        }
        // 同一个坏值：校验拒绝，而换算悄悄用默认值 —— 这是刻意的不对称
        assert!(!valid_tz("+99:00"));
        assert_eq!(offset_minutes("+99:00"), 8 * 60, "坏值退默认 +08:00");
        assert_eq!(offset_minutes("+08:00"), 480);
        assert_eq!(offset_minutes("-05:30"), -330);
        assert_eq!(offset_minutes(""), 8 * 60);
    }

    /// 空串是合法的：那表示"一个周期都不发"，与"配置写坏了"必须分得开。
    ///
    /// ⚠️ 顺带钉住一个**宽容**的地方 ✓：`"day,"` 与 `"day,,week"` 都是**合法**的 ——
    /// 空段匹配 `""`，而 `parse_periods` 会跳过空段，所以多余的逗号等价于没写。
    /// 我第一版把 `"day,"` 当成应当被拒的输入 ✗，用例红了才发现是**断言写错**、不是代码错。
    /// 合法集合来自 `valid_periods` 里的那个 `matches!`：六个周期名 + 空串。
    #[test]
    fn valid_periods_accepts_the_known_names_and_the_empty_list() {
        for ok in [
            "",
            "day",
            "week",
            "month",
            "quarter",
            "half",
            "year",
            "day,week",
            " week , month ",
            "day,",
            "day,,week",
        ] {
            assert!(valid_periods(ok), "{ok:?} 应当合法");
        }
        for bad in ["hourly", "DAY", "Year", "day;week", "days", " day week "] {
            assert!(!valid_periods(bad), "{bad:?} 应当被拒绝");
        }
    }
}
