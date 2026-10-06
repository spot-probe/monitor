//! 折算成人民币的汇率。
//!
//! 机器价格分币种存在 `node.currency` 里，而要比较、求和就必须折到**一个**币种。
//! 这里只做一件事：每天取一次「1 USD 等于各币种多少」，落进 `setting`；页面只读缓存。
//!
//! **四条硬规矩**（都是为了让"算错"不可能悄悄发生）：
//!
//! 1. **绝不回退成 1:1** —— 取不到就用**上一次的值**，页面上标明它有多旧。若悄悄按
//!    1:1 算，`$50` 会被当成 `¥50`，总额少算七倍而页面看起来完全正常。
//! 2. 失败**不写任何东西** —— 旧值与旧时间戳原样留着，于是"多久没更新"是可见的。
//! 3. **可以手动覆盖** —— 内网机器完全没有出网时，这一条是唯一出路；手动值优先。
//! 4. 一次取**全部**币种：`latest?from=USD` 本来就返回所有，所以
//!    `X → CNY = (CNY/USD) ÷ (X/USD)`，加一种货币不需要改代码。
//!
//! 一天一次够了：汇率对这种用途只在数量级上重要，而 `theme::watch` / `agent_release::watch`
//! 已经是同样的节奏。**不走 `proxied()`**：那个是给 GitHub 用的代理，汇率不该挤进去。

use std::collections::HashMap;
use std::time::Duration;

use axum::http::header;
use serde::Deserialize;
use tracing::{info, warn};

use crate::{App, Shared};

/// 上次取回的整张汇率表（以 USD 为基准），JSON 文本。
const RATES: &str = "fx_rates";
/// 取回时间（Unix 秒），用来在页面上说"这是什么时候的汇率"。
const FETCHED_AT: &str = "fx_fetched_at";
/// 手工指定的「1 USD = ? CNY」；非空且为正时**完全取代**自动值。
const MANUAL: &str = "fx_cny_per_usd_manual";

/// 启动后稍等再查，避开启动时那一堆事；之后每天一次。
const FIRST: Duration = Duration::from_secs(10);
const INTERVAL: Duration = Duration::from_secs(24 * 3600);

#[derive(Deserialize)]
struct Latest {
    rates: HashMap<String, f64>,
}

pub async fn watch(app: Shared) {
    tokio::time::sleep(FIRST).await;
    loop {
        refresh(&app).await;
        tokio::time::sleep(INTERVAL).await;
    }
}

/// 取一次并落库。失败只记日志：**不写任何键**，旧值因此原样保留。
pub async fn refresh(app: &App) {
    let url = "https://api.frankfurter.app/latest?from=USD";
    let read = app
        .http
        .get(url)
        .header(header::USER_AGENT, "monitor-hub")
        .send()
        .await
        .and_then(|r| r.error_for_status());
    match read {
        Ok(response) => match response.json::<Latest>().await {
            Ok(latest) if latest.rates.get("CNY").is_some_and(|v| *v > 0.0) => {
                match serde_json::to_string(&latest.rates) {
                    Ok(text) => {
                        if let Err(e) = app.db.set(RATES, &text) {
                            warn!("汇率存不进 setting：{e:#}");
                            return;
                        }
                        let _ = app.db.set(FETCHED_AT, &chrono::Utc::now().timestamp().to_string());
                        info!("汇率已更新（{} 种货币）", latest.rates.len());
                    }
                    Err(e) => warn!("汇率序列化失败：{e:#}"),
                }
            }
            Ok(_) => warn!("汇率响应里没有可用的 CNY"),
            Err(e) => warn!("读不出汇率 JSON（保留上一次的值）：{e:#}"),
        },
        Err(e) => warn!("取汇率失败（保留上一次的值）：{e:#}"),
    }
}

/// 当前用的汇率：`(每次读取都重新解释的手动值 → 否则缓存)`。
///
/// 返回 `(表, 取回时间戳, 是否手动)`。表以 USD 为基准；`None` 表示**一次都没取到过**
/// —— 调用方必须把这个情况显示出来，而不是当成 1:1。
pub fn rates(app: &App) -> Option<(HashMap<String, f64>, i64, bool)> {
    if let Some(v) = app.db.get(MANUAL).and_then(|v| v.trim().parse::<f64>().ok()).filter(|v| *v > 0.0) {
        let mut m = HashMap::new();
        m.insert("USD".to_string(), 1.0);
        m.insert("CNY".to_string(), v);
        return Some((m, 0, true));
    }
    let table: HashMap<String, f64> = serde_json::from_str(&app.db.get(RATES)?).ok()?;
    let at: i64 = app.db.get(FETCHED_AT).and_then(|v| v.parse().ok()).unwrap_or(0);
    Some((table, at, false))
}

/// 从 USD 基准的表里算出「1 个 `currency` 值多少 CNY」。
///
/// 纯函数，便于测试：`cny_per_unit(t, "USD") == t["CNY"]`，
/// 而 `cny_per_unit(t, "EUR") == t["CNY"] / t["EUR"]`。
pub fn cny_per_unit(table: &HashMap<String, f64>, currency: &str) -> Option<f64> {
    let per_usd = table.get("CNY").copied().filter(|v| *v > 0.0)?;
    // `from=USD` 的响应**不含 USD 自己**（基准币被省略），所以要给它隐式的 1.0 ——
    // 否则以美元计价的机器一台都折不出来，而 USD 恰恰是最常见的那个币种。
    let usd_per_unit =
        if currency == "USD" { 1.0 } else { table.get(currency).copied().filter(|v| *v > 0.0)? };
    let v = per_usd / usd_per_unit;
    v.is_finite().then_some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> HashMap<String, f64> {
        // 与真实响应同形：都是「1 USD = ? 该币种」
        // 故意**不放 USD**：真实的 `from=USD` 响应就是这样（基准币被省略）。
        [("CNY", 7.2), ("EUR", 0.9), ("JPY", 150.0)].into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    }

    #[test]
    fn converting_any_currency_to_cny() {
        let t = table();
        assert_eq!(cny_per_unit(&t, "USD"), Some(7.2));
        assert!((cny_per_unit(&t, "EUR").unwrap() - 8.0).abs() < 1e-9);
        assert!((cny_per_unit(&t, "JPY").unwrap() - 0.048).abs() < 1e-9);
        assert_eq!(cny_per_unit(&t, "CNY"), Some(1.0));
        assert_eq!(cny_per_unit(&t, "GBP"), None, "表里没有的币种必须给 None，绝不能当 1:1");
    }
}
