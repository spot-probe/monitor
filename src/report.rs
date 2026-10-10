//! 流量报告的正文。
//!
//! **这里是纯函数** ✓：输入是已经取好的数字（快照求和 ✓、额度 ✓、上期 ✓），输出是一段文本 ✓ ——
//! 查库与调度在 `api.rs` / 定时循环里 ✓。这样"报告长什么样"能被单测钉住 ✓，
//! 而不必为了验一句话去搭一个数据库 ✓。

use crate::db::Period;

/// 一行：一台节点在这个周期里的用量，以及它的额度与"本月已用"（用于超限提醒）。
pub struct Row {
    pub name: String,
    pub rx: i64,
    pub tx: i64,
    /// 月度额度（0 = 不限 ✓，与 `node.traffic_limit` 同一约定 ✓）。
    pub limit: i64,
    /// 本月已用（含这个周期 ✓）—— 超限判断只看它 ✓，因为额度是**按月**算的 ✓。
    pub month_used: i64,
}

fn human(n: i64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < U.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

/// 与上期相比的百分比。上期为 0 时**不写百分比** ✗ —— "从 0 涨到 5GB"的百分比是无穷大 ✓，
/// 写出来只会是噪音 ✓（改写成"新增"✓，由调用方在 `prev == 0` 时显示 ✓）。
fn delta(cur: i64, prev: i64) -> Option<f64> {
    if prev <= 0 {
        return None;
    }
    Some((cur - prev) as f64 * 100.0 / prev as f64)
}

/// 明细最多列这么多台 ✓ —— **Telegram 单条上限 4096 字符** ✗：100 台节点约 4KB，
/// 会被**直接拒掉** ✓，而发送路径不截断 ✓ ⇒ 报告丢失、且没人知道 ✓。
/// Top 20 加头部约 1.6KB ✓，稳稳在限内 ✓。要全量明细的正确做法是**附一个链接/文件** ✗，
/// 不是把上千行硬塞进一条消息 ✓。
const TOP_N: usize = 20;

/// 报告标题：`【流量日报】2026-03-14 · UTC+8` ✓（周期名与日期一眼可见 ✓）。
pub fn title(period: Period, from: &str, to: &str, tz: &str) -> String {
    let name = match period {
        Period::Day => "日报",
        Period::Week => "周报",
        Period::Month => "月报",
        Period::Quarter => "季报",
        Period::Half => "半年报",
        Period::Year => "年报",
    };
    // 区间含首不含尾 ✓，所以"覆盖到哪天"要把 `to` 退一天 ✓，否则读起来像多算了一天 ✗。
    format!("【流量{name}】{from} ~ {to} · {tz}")
}

/// **覆盖说明**：快照从哪天开始，这一段里实际覆盖了多少天 ✓。
///
/// **半年报与年报必须带它** ✗：快照是"启用那天"才开始有的 ✓ ⇒ 一年后第一份年报，
/// 区间里可能只有几个月的行 ✓ —— 而我们是"区间里有什么就加什么" ✓
/// ⇒ **总量会偏小，且报告里看不出来** ✗✗。把这句写在报告里，是唯一能让读者自己判断的办法 ✓。
///
/// 覆盖完整时**返回 `None`** ✓（日报/周报通常如此 ✓）—— 不给每次报告都加一行废话 ✓，
/// 只在"确实不全"时说话 ✓。
pub fn coverage_note(from: &str, to: &str, earliest: Option<&str>) -> Option<String> {
    let e = earliest?;
    // 字典序即日期序 ✓（`YYYY-MM-DD` ✓）—— 所以这里不需要解析日期 ✓。
    if e <= from {
        return None;
    }
    let days = chrono::NaiveDate::parse_from_str(from, "%Y-%m-%d")
        .ok()
        .zip(chrono::NaiveDate::parse_from_str(to, "%Y-%m-%d").ok())
        .zip(chrono::NaiveDate::parse_from_str(e, "%Y-%m-%d").ok())
        // 日期**相减**取天数 ✓ —— `Range<NaiveDate>` 不能迭代 ✗（chrono 的日期不是 `Step` ✓）。
        .map(|((f, t), e)| (t - e.max(f)).num_days().max(0))
        .unwrap_or(0);
    Some(format!("本期覆盖 {days} 天（快照自 {e} 起 —— 更早的用量没有留存，总量偏小）"))
}

/// 正文 ✓。`rows` 与 `prev` 由调用方按总量降序传进来 ✓（顺序在这里不再排 ✓ —— 排一次就够 ✓）。
pub fn body(rows: &[Row], prev: &[(String, i64, i64)]) -> String {
    let sum = |v: &[(String, i64, i64)]| v.iter().fold((0i64, 0i64), |a, r| (a.0 + r.1, a.1 + r.2));
    let (rx, tx) = sum(&rows.iter().map(|r| (r.name.clone(), r.rx, r.tx)).collect::<Vec<_>>());
    let (prx, ptx) = sum(prev);
    let mut out = String::new();
    out.push_str(&format!("合计  上行 {} · 下行 {} · 总计 {}\n", human(rx), human(tx), human(rx + tx)));
    match delta(rx + tx, prx + ptx) {
        Some(d) => out.push_str(&format!(
            "较上期  {}  {}{:.1}%\n",
            human(prx + ptx),
            if d >= 0.0 { "↑" } else { "↓" },
            d.abs()
        )),
        None if !prev.is_empty() => {
            out.push_str(&format!("较上期  {}（上期为 0，不显示百分比）\n", human(prx + ptx)))
        }
        None => out.push_str("较上期  没有可比的上期数据\n"),
    }
    out.push_str("\n逐节点（按总量）\n");
    if rows.is_empty() {
        out.push_str("  这个周期没有任何节点的上报。\n");
        return out;
    }
    let prev_of = |name: &str| prev.iter().find(|p| p.0 == name).map(|p| p.1 + p.2);
    // 明细只列前 `TOP_N` 台 ✓（`rows` 已按总量降序 ✓，所以这就是 Top N ✓）。
    // 注意**上面那段合计仍然按全部 rows 算** ✓ —— 明细被截断不该让总量变小 ✗。
    let shown = rows.len().min(TOP_N);
    for r in &rows[..shown] {
        let total = r.rx + r.tx;
        let cmp = match prev_of(&r.name) {
            Some(p) => match delta(total, p) {
                Some(d) => format!("  {}{:.1}%", if d >= 0.0 { "↑" } else { "↓" }, d.abs()),
                None => "  新增".to_string(),
            },
            None => String::new(),
        };
        // 超限提醒：额度是**按月**的 ✓，所以看 `month_used` 而不是这个周期的量 ✓。
        let warn = if r.limit > 0 {
            let pct = r.month_used as f64 * 100.0 / r.limit as f64;
            if pct >= 100.0 {
                format!("  ⚠ 本月已超出额度（{:.0}%）", pct)
            } else if pct >= 90.0 {
                format!("  ⚠ 本月已用 {:.0}% 额度", pct)
            } else {
                format!("  本月已用 {:.0}%", pct)
            }
        } else {
            String::new()
        };
        out.push_str(&format!("  {:<16} {:>9}{}{}\n", r.name, human(total), cmp, warn));
    }
    if rows.len() > shown {
        // 被截掉的那些**必须报出数量与合计** ✓ —— 否则报告看起来像"整个周期只有这 20 台在跑" ✗，
        // 而那种误解比少几行明细严重得多 ✓。
        let rest: i64 = rows[shown..].iter().map(|r| r.rx + r.tx).sum();
        out.push_str(&format!("  …还有 {} 台 · 合计 {}\n", rows.len() - shown, human(rest)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, rx: i64, tx: i64, limit: i64, month_used: i64) -> Row {
        Row { name: name.into(), rx, tx, limit, month_used }
    }

    /// 总量、上期对比、超限提醒三件都在 ✓ —— 且**上期为 0 时不写百分比** ✗（那是无穷大 ✓）。
    #[test]
    fn a_report_carries_totals_comparison_and_over_limit_marks() {
        let gb = 1024i64 * 1024 * 1024;
        let rows = vec![
            row("ucloud_usa", 2 * gb, 8 * gb, 100 * gb, 50 * gb), // 50% → 只写百分比 ✓
            row("backwaves_hk", 4 * gb, 4 * gb, 10 * gb, 95 * gb / 10), // 95% → ⚠ 将超限 ✓
            row("tiny", 1, 1, 0, 0),                              // 不限额度 → 不写额度 ✓
        ];
        let prev = vec![("ucloud_usa".to_string(), gb, 4 * gb), ("backwaves_hk".to_string(), 2 * gb, 2 * gb)];
        let t = body(&rows, &prev);
        assert!(t.contains("合计  上行 6.0 GB · 下行 12.0 GB · 总计 18.0 GB"), "总量：{}", t);
        assert!(t.contains("较上期"), "要有上期对比：{}", t);
        assert!(t.contains("⚠ 本月已用 95% 额度"), "将超限要标出来：{}", t);
        assert!(t.contains("ucloud_usa") && t.contains("50%"), "普通额度也报：{}", t);
        assert!(
            !t.contains("tiny") || !t.contains("tiny                    1 B  本月"),
            "不限额度不写额度行：{}",
            t
        );
    }

    /// **Top 20**：不足 20 台全列 ✓；超过时只列 20 台 + 一行「还有 N 台 · 合计 X」✓，
    /// 而**合计那一行仍按全部台数算** ✓（明细被截断不该让总量变小 ✗）。
    #[test]
    fn the_detail_stops_at_twenty_but_the_total_does_not() {
        let gb = 1024i64 * 1024 * 1024;
        let mk = |n: usize| -> Vec<Row> {
            (0..n)
                .map(|i| row(&format!("node{i:03}"), gb, gb, 0, 0)) // 每台 2GB ✓
                .collect()
        };
        // 不足 20 台：全部列出 ✓，**没有**那行汇总 ✓。
        let few = body(&mk(5), &[]);
        assert!(few.contains("node004"), "全部列出：{}", few);
        assert!(!few.contains("还有"), "不足 20 台不该有汇总行：{}", few);
        // 30 台：列 20 台 ✓、汇总行写"还有 10 台"✓，合计按 30 台 = 60GB ✓。
        let many = body(&mk(30), &[]);
        assert!(many.contains("node019"), "第 20 台要在：{}", many);
        assert!(!many.contains("node020"), "第 21 台不该在：{}", many);
        assert!(many.contains("…还有 10 台 · 合计 20.0 GB"), "汇总行：{}", many);
        assert!(many.contains("合计  上行 30.0 GB · 下行 30.0 GB · 总计 60.0 GB"), "总量按全部算：{}", many);
    }

    /// 覆盖说明：**完整时不说话** ✓、不全时说清从哪天起 ✓ —— 这一句就是"别让第一份年报在说谎" ✓。
    #[test]
    fn the_coverage_note_only_speaks_when_something_is_missing() {
        // 快照比区间起点更早 ⇒ 完整 ⇒ **不写** ✓。
        assert_eq!(coverage_note("2026-03-01", "2026-03-02", Some("2020-01-01")), None);
        assert_eq!(
            coverage_note("2026-03-01", "2026-03-02", Some("2026-03-01")),
            None,
            "正好从起点开始也算完整"
        );
        // 还没有任何快照 ⇒ 也不写（那种情况本来就整份都没有内容 ✓）。
        assert_eq!(coverage_note("2026-03-01", "2026-03-02", None), None);
        // 快照晚于起点 ⇒ **必须说** ✓，天数按"从快照那天到区间终点" ✓。
        let n = coverage_note("2026-01-01", "2026-04-01", Some("2026-02-01")).unwrap();
        assert!(n.contains("本期覆盖 59 天"), "{n}");
        assert!(n.contains("快照自 2026-02-01 起"), "{n}");
        assert!(n.contains("总量偏小"), "要说清后果，而不是只报个数字：{n}");
    }

    /// 上期为 0（或没有上期）时**不写百分比** ✓，但要说明原因 ✓ —— 不能悄悄省略 ✗。
    #[test]
    fn no_previous_period_is_said_not_guessed() {
        let rows = vec![row("a", 100, 100, 0, 0)];
        let t = body(&rows, &[]);
        assert!(t.contains("没有可比的上期数据"), "{}", t);
        assert!(!t.contains('%'), "不该出现百分比：{}", t);
        let t2 = body(&rows, &[("a".to_string(), 0, 0)]);
        assert!(t2.contains("不显示百分比"), "上期为 0 要说清：{}", t2);
    }

    /// 超限是**按月**算的 ✓：额度看 `month_used`，而不是这个周期的量 ✓
    /// （否则一个月报会在月初把"还有 90% 额度"的机器标成超限 ✗）。
    #[test]
    fn the_limit_warning_follows_the_month_not_the_period() {
        let gb = 1024i64 * 1024 * 1024;
        // 周期内只用了 1GB，但本月已用 120GB / 100GB ⇒ 超限 ✓。
        let rows = vec![row("heavy", gb / 2, gb / 2, 100 * gb, 120 * gb)];
        let t = body(&rows, &[]);
        assert!(t.contains("⚠ 本月已超出额度（120%）"), "{}", t);
    }
}
