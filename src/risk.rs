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

/// 这家来源今天**要不要去查** ✓ —— **纯函数** ✓，所以"开关关着就不发请求"能不靠网络测死 ✓。
///
/// 两个条件缺一不可 ✗：开关**恰好**是 `"on"` ✓（缺失 / 空串 / 被改坏的值一律算关 ✓✓ ——
/// 于是"默认关"不依赖某个默认值一定被写上 ✓）、且 key 非空 ✓（空 = 没配这家 ✓，
/// 与 `geo_*_url` 的空值同一处理 ✓：跳过，而不是报错 ✓）。
pub fn active(enabled: &str, key: &str) -> bool {
    enabled == "on" && !key.trim().is_empty()
}

/// AbuseIPDB 的查询地址 ✓（`maxAgeInDays=90` = 官方默认口径 ✓）。
pub fn abuseipdb_url(ip: &str) -> String {
    format!("https://api.abuseipdb.com/api/v2/check?ipAddress={ip}&maxAgeInDays=90")
}

/// 查一个 IP 的**在线**风险结论 ✓（AbuseIPDB ✓）。
///
/// **三处"不发请求"都是刻意的** ✗：
/// - 开关关着 ✓✓ ⇒ 不把节点 IP 发给任何第三方 ✓（这是隐私的**默认状态** ✓）；
/// - 没配 key ✓ ⇒ 跳过这家 ✓（正常状态 ✓，不是错误 ✓）；
/// - **今天已经查过** ✓✓ ⇒ 直接用缓存 ✓ —— 这就是"免费额度不会当天见底"的执行者 ✓。
///
/// 顺序是「**先落库、再解析**」✓✓：解析失败（改了字段名 / 服务换了口径 ✓）
/// 也不该把**那一次额度**丢掉 ✓（额度比一次解析贵得多 ✓）。
pub async fn check(app: &crate::App, ip: &str, day: &str) -> Option<Risk> {
    let enabled = app.db.get("risk_enabled").unwrap_or_default();
    let key = app.db.get("risk_abuseipdb_key").unwrap_or_default();
    if !active(&enabled, &key) {
        return None;
    }
    // 今天的缓存命中 ⇒ **不再请求** ✓（额度就省在这里 ✓）
    if let Some(body) = app.db.risk_cached(ip, "abuseipdb", day) {
        return serde_json::from_str::<serde_json::Value>(&body).ok().as_ref().and_then(parse_abuseipdb);
    }
    let sent = app
        .http
        .get(abuseipdb_url(ip))
        .header("Key", key.trim())
        .header("Accept", "application/json")
        .send()
        .await;
    let body = match sent {
        Ok(r) => match r.text().await {
            Ok(t) => t,
            Err(e) => {
                return {
                    tracing::warn!("risk: abuseipdb body read failed: {e:#}");
                    None
                }
            }
        },
        Err(e) => {
            return {
                tracing::warn!("risk: abuseipdb request failed: {e:#}");
                None
            }
        }
    };
    // **先落库** ✓✓（哪怕这次解析不出来，额度也没白花 ✓；而且不必为此重查 ✓）
    if let Err(e) = app.db.save_risk(ip, "abuseipdb", day, &body) {
        tracing::warn!("risk: caching the abuseipdb answer failed: {e:#}");
    }
    serde_json::from_str::<serde_json::Value>(&body).ok().as_ref().and_then(parse_abuseipdb)
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

    /// **开关与 key 的判据** ✓✓ —— 这一条是"默认不把节点 IP 发给第三方"的保证 ✓，
    /// 而且它**不依赖某个默认值一定被写上** ✗：缺失 / 空串 / 被改坏的值一律算关 ✓。
    #[test]
    fn nothing_leaves_the_hub_unless_it_is_switched_on_and_keyed() {
        assert!(!active("off", "k"), "关着就不发");
        assert!(!active("", "k"), "缺失也算关（**不依赖默认值被写上** ✓）");
        assert!(!active("yes", "k"), "只有恰好 on 才算开（写坏的值得按关处理 ✓✓）");
        assert!(!active("ON", "k"), "大小写不同也算关（宁可漏查，不可误发 ✓）");
        assert!(!active("on", ""), "没配 key ⇒ 跳过这家，不是报错 ✓");
        assert!(!active("on", "   "), "空白 key 等于没配 ✓");
        assert!(active("on", "k"), "开了且有 key ⇒ 才去查 ✓");
    }

    /// 查询地址：带 IP 与官方默认窗口 ✓（`maxAgeInDays=90` ✓）。
    #[test]
    fn the_request_url_carries_the_ip_and_the_window() {
        let u = abuseipdb_url("203.0.113.9");
        assert!(u.starts_with("https://api.abuseipdb.com/api/v2/check?"), "{u}");
        assert!(u.contains("ipAddress=203.0.113.9"), "{u}");
        assert!(u.contains("maxAgeInDays=90"), "{u}");
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
