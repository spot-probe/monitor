//! 在线风险库（第三方）—— **只做解析** ✓，请求与额度在调用方。
//!
//! 与 `geo` 那条路**性质不同** ✗，三件事必须先说清：
//!
//! 1. **要 key** ✓（运维自己申请 ✓，存设置里 ✓，与 Telegram/Webhook 的凭据同一处理 ✓）；
//! 2. **有额度** ✓ ⇒ 必须"**每个 IP 每天只查一次 + 结果落库**" ✓✓ ——
//!    否则 100 台节点 × 每小时一次 × 几家 = 一天上万次 ✓，免费额度当天见底 ✓，
//!    而症状是**所有字段变空、没有任何报错** ✗；
//! 3. **会把节点 IP 发给第三方** ✗ ⇒ 必须有**开关** ✓（默认关 ✓）· 面板写明"发给哪几家" ✓ ·
//!    能一键全关 ✓。
//!
//! **绝不把多源合成一个分数** ✗✗ —— 维护者给的参考自己就是证据 ✓：
//! 同一台机器，ipapi 说「较高风险」✓、IP2Location 说「低风险」✓、IPQS 说「可疑」✓，
//! 地区上 [HK] 与 [CN] **并存** ✓。**分歧本身就是最有信息量的信号** ✓✓，
//! 平均掉它等于把唯一有用的东西删了 ✓。

// ⚠️ **整块还没接线** ✗：请求、额度缓存与开关是下一步的事 ✓，所以现在只有测试在调用 ✓
// ⇒ clippy 会报一串 dead_code ✗。这是**欠账**，不是设计 ✓ —— 接上后这行 allow 必须删掉 ✓
//（`geo.rs` / `report.rs` 顶部那行都是这么加的、也都是这么删的 ✓）。
#![allow(dead_code)]

/// 一次在线查询的结论 ✓ —— **逐源并列** ✓，不合成 ✓。
#[derive(Default, Clone, PartialEq, Debug, serde::Serialize)]
pub struct Risk {
    /// 来源名 ✓（面板上逐源显示 ✓："谁说的"和"说了什么"一样重要 ✓）。
    pub source: String,
    /// 0–100 的风险分 ✓（各家口径不同 ✓，所以**只在同一家内部可比** ✗）。
    pub score: Option<f64>,
    /// 家自带的档位文字 ✓（"低风险 / 较高风险 / 可疑" ✓ —— 比数字更可读 ✓）。
    pub label: Option<String>,
    /// 举报/命中次数 ✓（AbuseIPDB 的 `totalReports` ✓）。
    pub reports: Option<i64>,
    /// 家认为的用途 ✓（"Data Center/Web Hosting" ✓ —— 参考里"机房"那一列的来源 ✓）。
    pub usage: Option<String>,
    /// 是否 Tor 出口 ✓（各家都给这一项 ✓）。
    pub tor: Option<bool>,
}

/// 解析 **AbuseIPDB** 的 `check` 响应 ✓。
///
/// 字段名按官方文档 ✓（`data.abuseConfidenceScore` ✓ `data.totalReports` ✓
/// `data.usageType` ✓ `data.isTor` ✓）—— ⚠️ 但**我没有对着真实响应验过** ✗，
/// 第一次接通时应当拿一条真响应核一遍 ✓（这正是纯函数的好处：改字段名只改这里 ✓，
/// 而解析器有测试兜着 ✓）。
///
/// 任何字段缺失都返回 `None` ✓ —— 不编 0 ✗（"没举报过"与"拿不到数据"是两件事 ✓，
/// 而 `0` 会被读成前者 ✓）。
pub fn parse_abuseipdb(v: &serde_json::Value) -> Option<Risk> {
    let d = v.get("data")?;
    // **两个 `?`**：第一个是"响应里没有这个字段" ✓，第二个是"它不是数字" ✓ ——
    // 两者都是"拿不到数据" ✓（而不是"风险为 0" ✗），所以都在这里返回 `None` ✓。
    let score = d.get("abuseConfidenceScore")?.as_f64()?;
    Some(Risk {
        source: "AbuseIPDB".into(),
        score: Some(score),
        // 档位文字自己给 ✓：AbuseIPDB 只给分 ✓，而面板上"低风险/较高风险"比 42 好读 ✓。
        label: Some(
            match score {
                s if s < 25.0 => "低风险",
                s if s < 75.0 => "较高风险",
                _ => "高风险",
            }
            .into(),
        ),
        reports: d.get("totalReports").and_then(|x| x.as_i64()),
        usage: d.get("usageType").and_then(|x| x.as_str()).map(str::to_string),
        tor: d.get("isTor").and_then(|x| x.as_bool()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一条**形状与官方一致**的响应 ✓（值是我编的 ✓ —— 这里测的是映射 ✓，不是服务 ✓）。
    #[test]
    fn abuseipdb_fields_map_and_missing_ones_stay_none() {
        let v = serde_json::json!({
            "data": {
                "ipAddress": "203.0.113.9",
                "abuseConfidenceScore": 42,
                "totalReports": 7,
                "usageType": "Data Center/Web Hosting/Transit",
                "isTor": false,
                "countryCode": "HK"
            }
        });
        let r = parse_abuseipdb(&v).expect("有分数就该解析出来");
        assert_eq!(r.source, "AbuseIPDB");
        assert_eq!(r.score, Some(42.0));
        assert_eq!(r.label.as_deref(), Some("较高风险"), "42 落在 25–75 之间 ✓");
        assert_eq!(r.reports, Some(7));
        assert_eq!(r.usage.as_deref(), Some("Data Center/Web Hosting/Transit"));
        assert_eq!(r.tor, Some(false));
    }

    /// **档位文字**只在同一家内部可比 ✓ —— 阈值是我们给的 ✓，不是服务给的 ✓。
    #[test]
    fn the_label_is_ours_not_theirs() {
        let mk = |s: f64| serde_json::json!({ "data": { "abuseConfidenceScore": s } });
        assert_eq!(parse_abuseipdb(&mk(0.0)).unwrap().label.as_deref(), Some("低风险"));
        assert_eq!(parse_abuseipdb(&mk(24.9)).unwrap().label.as_deref(), Some("低风险"));
        assert_eq!(parse_abuseipdb(&mk(25.0)).unwrap().label.as_deref(), Some("较高风险"));
        assert_eq!(parse_abuseipdb(&mk(74.9)).unwrap().label.as_deref(), Some("较高风险"));
        assert_eq!(parse_abuseipdb(&mk(75.0)).unwrap().label.as_deref(), Some("高风险"));
        assert_eq!(parse_abuseipdb(&mk(100.0)).unwrap().label.as_deref(), Some("高风险"));
    }

    /// 缺字段 / 出错页 / 限流回执 ⇒ `None` ✓ —— **不编 0** ✗：
    /// `0` 会被读成"没举报过" ✓，而真相是"拿不到数据" ✓，两者在运维判断上差别很大 ✓。
    #[test]
    fn anything_short_of_a_real_answer_is_none() {
        assert_eq!(parse_abuseipdb(&serde_json::json!({})), None);
        assert_eq!(parse_abuseipdb(&serde_json::json!({ "data": {} })), None, "没有分数 ⇒ 拿不到");
        assert_eq!(parse_abuseipdb(&serde_json::json!({ "errors": [{ "detail": "rate limit" }] })), None);
        // 分数字段类型不对（半截 JSON / 换了口径）也要安全 ✓
        assert_eq!(parse_abuseipdb(&serde_json::json!({ "data": { "abuseConfidenceScore": "42" } })), None);
    }
}
