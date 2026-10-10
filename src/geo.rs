//! 外部 IP 数据库（MaxMind / IP2Proxy / DB-IP / Tor 列表）的**下载与留存**。
//!
//! 这一块只负责"把文件安全地放到 `data_dir` 里" ✓，**不含任何查询** ✓（查询在块 2 ✓）。
//! 全部在 hub 本地 ✓ —— agent 零改动 ✓，节点 IP 也不外发 ✓。
//!
//! ## 为什么"落盘"要单独写、还要写测试
//!
//! 最自然的写法是"下载完直接覆盖那个文件" ✗ —— 而一次网络抖动、一次磁盘写满，
//! 就会把**好文件写坏** ✓，症状是"IP 质量整列变空" ✓，而**没有任何报错**指向这里 ✗。
//! 所以：**先写临时文件 → 校验 → 原子替换** ✓✓ —— 校验不过就**原封不动** ✓。
//! 这条是这一块的命门 ✓，所以它有自己的单测（不碰网络 ✓）。

use crate::App;
use std::path::Path;

/// 一次查询的**结论** ✓ —— 参考里第 ① 节要的那些字段 ✓。
///
/// 全部 `Option` ✓：库没配、IP 查不到、字段缺失，都是 `None` ✓ ——
/// **不编造默认值** ✗（"未知"与"美国"是两件事 ✓，而 `""` 会被读成后者 ✓）。
// 见 `lookup` 上方那段说明 ✓（同一次欠账 ✓）。
#[allow(dead_code)]
#[derive(Default, Clone, PartialEq, Debug, serde::Serialize)]
pub struct Quality {
    pub country: Option<String>,
    pub city: Option<String>,
    pub subdivision: Option<String>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub time_zone: Option<String>,
    pub asn: Option<u32>,
    pub org: Option<String>,
}

/// 把 City 与 ASN 两份查询结果**合成一个结论** ✓ —— **纯函数** ✓，所以它有自己的单测 ✓。
///
/// 分层是刻意的 ✓：真正调用 mmdb 的那几行保持极薄 ✓（没有便宜的样例库可造 ✗），
/// 而"怎么解读结果"（哪些字段要 ✓、缺了怎么办 ✓、两份怎么合 ✓）留在这里被测死 ✓✓。
pub fn merge(city: Option<Quality>, asn: Option<Quality>) -> Quality {
    let mut out = city.unwrap_or_default();
    if let Some(a) = asn {
        // ASN 只有那两样 ✓：只补**空**的字段 ✓ —— 不覆盖 City 已有的事实 ✓。
        if out.asn.is_none() {
            out.asn = a.asn;
        }
        if out.org.is_none() {
            out.org = a.org;
        }
    }
    out
}

/// 从 mmdb 查出来的**一个 JSON 记录**里取出我们要的字段 ✓ —— **纯函数** ✓，所以能手写 JSON 测死 ✓✓。
///
/// 为什么让它吃 JSON 而不是 `geoip2::City` ✗：那样就**必须有一个真库**才能测 ✓，
/// 而 mmdb 的样例造不出来 ✗（格式带二叉树 ✓）⇒ 测试只能跳过 ✓。
/// 吃 JSON 之后，"取哪些字段、缺了怎么办"全都能用**手写的小 JSON** 钉住 ✓✓，
/// 而真正调 mmdb 的那两行保持**薄到不用测** ✓。
///
/// 字段名按 MaxMind 的 GeoLite2 记录 ✓（City 与 ASN 两库同构：有哪个取哪个 ✓）。
pub fn quality_from_json(v: &serde_json::Value) -> Quality {
    let s_at = |path: &[&str]| -> Option<String> {
        let mut cur = v;
        for k in path {
            cur = cur.get(k)?;
        }
        cur.as_str().map(str::to_string)
    };
    let f_at = |path: &[&str]| -> Option<f64> {
        let mut cur = v;
        for k in path {
            cur = cur.get(k)?;
        }
        cur.as_f64()
    };
    Quality {
        country: s_at(&["country", "iso_code"]),
        // 城市名优先英文 ✓（面板与报告都是中文界面 ✓，但英文名比"某些库只给本地名"更通用 ✓）。
        city: s_at(&["city", "names", "en"]).or_else(|| s_at(&["city", "names", "zh"])),
        subdivision: v
            .get("subdivisions")
            .and_then(|a| a.get(0))
            .and_then(|d| d.get("iso_code"))
            .and_then(|c| c.as_str())
            .map(str::to_string),
        latitude: f_at(&["location", "latitude"]),
        longitude: f_at(&["location", "longitude"]),
        time_zone: s_at(&["location", "time_zone"]),
        // ASN 库的两样 ✓（同一个函数吃两种记录 ✓ —— 它们字段名不同、互不冲突 ✓）。
        asn: v.get("autonomous_system_number").and_then(|x| x.as_u64()).map(|x| x as u32),
        org: s_at(&["autonomous_system_organization"]),
    }
}

/// 查一个 IP ✓：City 与 ASN 两份库各查一次，再用 [`merge`] 合起来 ✓。
///
/// **三种情况刻意分开** ✓（这是这一层的全部判断 ✓）：
/// - 库文件不在 ⇒ **没配** ⇒ 返回空结论 ✓、**不报错** ✗（与下载那一层同一条原则 ✓）；
/// - 文件在、但打不开或读不动 ⇒ **要日志** ✓（库不兼容 / 被截断 ✓ —— 那是要人处理的 ✓）；
/// - 这个 IP 不在库里 ⇒ 空字段 ✓（正常 ✓ —— MaxMind 也确实有未收录的段 ✓）。
// 调用它的是**下一步**：把结论落到节点上并暴露给面板与主题（块 3 ✓）。
// 在那之前它只有测试在用 ✗ ⇒ 暂标 allow，紧跟着这条说明，接上即删 ✓
//（`Quality` / `merge` 的 allow 在下一轮就能删 ✓ —— 它们已经被 `lookup` 用起来了 ✓）。
pub fn lookup(dir: &Path, ip: std::net::IpAddr) -> Quality {
    let read = |name: &str| -> Option<Quality> {
        let path = dir.join(name);
        if !path.exists() {
            return None; // 没配 ⇒ 不算错 ✓
        }
        let reader = match maxminddb::Reader::open_readfile(&path) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("geo: {name}: cannot open the database: {e:#}");
                return None;
            }
        };
        // v0.32 的 API（**读源码确认过** ✓，不是猜的 ✗）：`lookup(ip)` 不吃泛型 ✓，
        // 返回 `LookupResult` ✓，再 `decode::<T>()` 得到 `Result<Option<T>>` ——
        // **`Ok(None)` 正好表示"这个 IP 不在库里"** ✓（MaxMind 确实有未收录的段 ✓），
        // 那与"库坏了/读不动"是两件事 ✓：前者是空结论 ✓，后者要日志 ✓。
        match reader.lookup(ip) {
            Ok(found) => match found.decode::<serde_json::Value>() {
                Ok(Some(v)) => Some(quality_from_json(&v)),
                Ok(None) => Some(Quality::default()),
                Err(e) => {
                    tracing::warn!("geo: {name}: decode failed: {e:#}");
                    None
                }
            },
            Err(e) => {
                tracing::warn!("geo: {name}: lookup failed: {e:#}");
                None
            }
        }
    };
    merge(read(SOURCES[0].name), read(SOURCES[1].name))
}

/// 库清单里的一个库 ✓。
///
/// **URL 不在代码里** ✗✓：镜像地址由设置给（默认空 ✓）—— 与 `github_proxy` 完全同形 ✓：
/// 运维指一个自己信得过的镜像 ✓，而代码里**不编任何地址** ✗
/// （编一个就是又一个"悄悄下不到、字段全空"的来源 ✓）。
pub struct Source {
    /// 落盘的名字 ✓（也是查询那一层要打开的文件名 ✓）。
    pub name: &'static str,
    pub shape: Shape,
    /// 哪个设置键给它的 URL ✓（空 = 没配 ⇒ 跳过这个库 ✓，不是报错 ✗）。
    pub key: &'static str,
    /// 小于这个字节数一定是坏的 ✓（一个 200 字节的"城市库"不可能是真的 ✓）。
    pub min_bytes: usize,
}

/// 现在纳入的库 ✓ —— **只放我确认过形态的** ✗：
/// MaxMind 是 mmdb ✓、Tor 出口是纯文本 ✓；
/// IP2Proxy LITE / DB-IP 的发布形态（mmdb ✗ BIN ✗ CSV ✗）**我没有核实** ✓，
/// 所以先不写进来 ✓ —— 核实之后各加一行即可 ✓（它们的形态字段已经准备好 ✓）。
pub const SOURCES: [Source; 3] = [
    Source { name: "GeoLite2-City.mmdb", shape: Shape::Mmdb, key: "geo_city_url", min_bytes: 1 << 20 },
    Source { name: "GeoLite2-ASN.mmdb", shape: Shape::Mmdb, key: "geo_asn_url", min_bytes: 1 << 20 },
    Source { name: "tor-exit.txt", shape: Shape::Text, key: "geo_tor_url", min_bytes: 1 << 10 },
];

/// 库文件放哪 ✓：**显式参数优先** ✓，否则与数据库同目录 ✓。
///
/// 与 DB 同目录是刻意的 ✓：数据文件跟着数据走 ✓（一眼能找到 ✓，也不碰系统目录 ✓）——
/// 与 `main.rs` 里给 DB 设权限那句用的是**同一条推导** ✓（那里已经在用 `parent()` ✓）。
pub fn data_dir(db_path: &str, explicit: Option<&str>) -> std::path::PathBuf {
    if let Some(d) = explicit.filter(|d| !d.trim().is_empty()) {
        return std::path::PathBuf::from(d);
    }
    std::path::Path::new(db_path)
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .map(|d| d.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

/// 库文件的形态决定怎么校验 ✓。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Shape {
    /// MaxMind 格式（`.mmdb` ✓，IP2Proxy / DB-IP 也有 mmdb 版本 ✓）。
    Mmdb,
    /// 纯文本列表（如 Tor 出口 ✓）。
    Text,
}

/// MaxMind 数据库的**元数据魔数** ✓ —— mmdb 文件在**尾部**用这四个字节标记元数据段的开始 ✓。
/// 不检查头部而检查尾部，是这种格式的特性 ✓（头部是二进制搜索树，没有固定签名字节 ✗）。
const MMDB_MARKER: &[u8] = b"\xab\xcd\xefMaxMind.com";

/// 元数据段总在文件最后这一段里 ✓（官方实现也是只在尾部找它 ✓）。
const MMDB_TAIL: usize = 128 * 1024;

/// 文件看起来**像不像**一个完整的 mmdb ✓。
///
/// 只做"像不像"这一级的判断 ✗：真正的解析留给块 2 的读取器 ✓ ——
/// 在这里做深度解析，等于把"下载"和"解析"绑死 ✓，而它们失败的处置完全不同 ✓
/// （一个该重试下载 ✓，一个该报告库不兼容 ✓）。
pub fn looks_like_mmdb(bytes: &[u8]) -> bool {
    let tail = &bytes[bytes.len().saturating_sub(MMDB_TAIL)..];
    tail.windows(MMDB_MARKER.len()).any(|w| w == MMDB_MARKER)
}

/// 文本列表看起来**像不像**一份能用的清单 ✓：非空 ✓、至少有一行**不是注释** ✓。
///
/// 不看具体内容 ✗（Tor 列表的格式可能变 ✓）—— 只挡住"空文件 / 一整页 HTML 错误页" ✓，
/// 而那两种恰恰是下载失败时最常见的产物 ✓✓。
pub fn looks_like_text_list(text: &str) -> bool {
    !text.trim().is_empty()
        && text.lines().any(|l| {
            !l.trim().is_empty() && !l.trim_start().starts_with('#') && !l.trim_start().starts_with('<')
        })
}

/// 按形态校验 ✓。
pub fn looks_valid(shape: Shape, bytes: &[u8]) -> bool {
    match shape {
        Shape::Mmdb => looks_like_mmdb(bytes),
        Shape::Text => std::str::from_utf8(bytes).is_ok_and(looks_like_text_list),
    }
}

/// **先写临时文件 → 校验 → 原子替换** ✓✓。
///
/// 校验不过、或任何一步 IO 失败：**目标文件原封不动** ✓，临时文件被清掉 ✓。
/// 这就是"一次抖动不能把好库写坏"的落实处 ✓。
pub fn store_atomic(dir: &Path, name: &str, shape: Shape, bytes: &[u8]) -> std::io::Result<()> {
    if !looks_valid(shape, bytes) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{name}: downloaded content does not look like a valid {shape:?} file"),
        ));
    }
    std::fs::create_dir_all(dir)?;
    // 临时名带上 pid ✓：同一进程里两个刷新任务同时跑也不会互相踩 ✓。
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    match std::fs::rename(&tmp, dir.join(name)) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// 取一个库并落盘 ✓。**返回是否真的更新了** ✓。
///
/// 三处刻意的行为 ✓：
/// - **URL 空 = 没配 ⇒ 跳过** ✓（不是错误 ✗ —— "没配"是正常状态 ✓，与"配了但下不来"要分开 ✓）；
/// - **失败只记日志、保留旧文件** ✓：`store_atomic` 已经保证"坏内容不落盘" ✓，
///   这里再保证"网络失败不动它" ✓ —— 两者合起来就是"一次抖动不能把好库写坏" ✓✓；
/// - **不因为一个库失败就跳过其余的** ✓（它们是独立的 ✓）。
pub async fn fetch(app: &App, source: &Source) -> bool {
    let url = app.db.get(source.key).unwrap_or_default();
    let url = url.trim();
    if url.is_empty() {
        tracing::debug!("geo: {} has no URL configured; skipped", source.name);
        return false;
    }
    let got = app.http.get(url).send().await;
    let bytes = match got {
        Ok(r) if r.status().is_success() => match r.bytes().await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("geo: {}: body read failed: {e:#}", source.name);
                return false;
            }
        },
        Ok(r) => {
            tracing::warn!("geo: {}: HTTP {} from the configured mirror", source.name, r.status());
            return false;
        }
        Err(e) => {
            tracing::warn!("geo: {}: request failed: {e:#}", source.name);
            return false;
        }
    };
    if bytes.len() < source.min_bytes {
        // 明确说出"太小" ✓：镜像返回一个 200 的错误页时，这是唯一看得见的线索 ✓。
        tracing::warn!(
            "geo: {}: got {} bytes, below the {} byte floor; keeping the previous file",
            source.name,
            bytes.len(),
            source.min_bytes
        );
        return false;
    }
    match store_atomic(&app.data_dir, source.name, source.shape, &bytes) {
        Ok(()) => {
            tracing::info!("geo: {} updated ({} bytes)", source.name, bytes.len());
            true
        }
        Err(e) => {
            // `store_atomic` 已经把坏内容挡在门外 ✓，旧文件没动 ✓ —— 这里只需要说出来 ✓。
            tracing::warn!("geo: {}: rejected and left untouched: {e:#}", source.name);
            false
        }
    }
}

/// 拉一轮**全部**库 ✓。
pub async fn refresh(app: &App) -> usize {
    let mut updated = 0;
    for source in SOURCES.iter() {
        if fetch(app, source).await {
            updated += 1;
        }
    }
    updated
}

/// 启动时拉一次，然后**低频**刷新 ✓。
///
/// 间隔按"库本身更新很慢"来定 ✓（MaxMind 每周 ✓、Tor 列表随时小改 ✓）——
/// 每小时一次足够 ✓，而且**没有任何额度** ✓（这些是静态文件 ✓）。
pub async fn watch(app: crate::Shared) {
    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    loop {
        refresh(&app).await;
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("monitor-geo-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// 一个最小的"像 mmdb"的字节串：尾部带元数据魔数 ✓。
    fn fake_mmdb() -> Vec<u8> {
        let mut v = vec![0u8; 1024];
        v.extend_from_slice(MMDB_MARKER);
        v.extend_from_slice(b"\x00\x00");
        v
    }

    /// **用手写 JSON 测字段映射** ✓✓ —— 这一层能测死，正是因为 `lookup` 把 mmdb 的返回值
    /// 当 JSON 吃 ✓（否则就必须有一个真库 ✗，而 mmdb 样例造不出来 ✓）。
    #[test]
    fn quality_reads_the_fields_maxmind_actually_ships() {
        // 一份 City 记录 ✓（字段名取自 GeoLite2 的真实结构 ✓）。
        let city = serde_json::json!({
            "country": { "iso_code": "HK" },
            "city": { "names": { "en": "Hong Kong", "zh": "香港" } },
            "subdivisions": [{ "iso_code": "HK" }],
            "location": { "latitude": 22.3193, "longitude": 114.1694, "time_zone": "Asia/Hong_Kong" }
        });
        let q = quality_from_json(&city);
        assert_eq!(q.country.as_deref(), Some("HK"));
        assert_eq!(q.city.as_deref(), Some("Hong Kong"), "英文名优先 ✓");
        assert_eq!(q.subdivision.as_deref(), Some("HK"));
        assert_eq!(q.latitude, Some(22.3193));
        assert_eq!(q.time_zone.as_deref(), Some("Asia/Hong_Kong"));
        assert_eq!(q.asn, None, "City 记录里没有 ASN ✓");

        // 一份 ASN 记录 ✓（同一个函数吃它 ✓）。
        let asn = serde_json::json!({
            "autonomous_system_number": 906,
            "autonomous_system_organization": "DMIT Cloud Services"
        });
        let a = quality_from_json(&asn);
        assert_eq!(a.asn, Some(906));
        assert_eq!(a.org.as_deref(), Some("DMIT Cloud Services"));
        assert_eq!(a.country, None);

        // 缺字段时**不能编** ✓ —— 只有中文名时退回中文 ✓，什么都没有就是 None ✓。
        let zh_only = serde_json::json!({ "city": { "names": { "zh": "香港" } } });
        assert_eq!(quality_from_json(&zh_only).city.as_deref(), Some("香港"));
        assert_eq!(quality_from_json(&serde_json::json!({})), Quality::default());
        // 类型不对也要安全 ✓（库里理论上不会，但坏库/半截库会 ✓）。
        let weird = serde_json::json!({ "country": { "iso_code": 42 }, "autonomous_system_number": "906" });
        assert_eq!(quality_from_json(&weird), Quality::default(), "类型不对 ⇒ 当没有 ✓，不 panic ✗");
    }

    /// 两份结果合成：**只补空字段** ✓ —— ASN 库不该覆盖 City 库里已有的事实 ✓；
    /// 两份都缺时全是 `None` ✓（**不编默认值** ✗ —— `""` 会被读成"未知的那个国家" ✓）。
    #[test]
    fn merging_takes_the_asn_only_where_the_city_left_a_hole() {
        let city = Quality {
            country: Some("HK".into()),
            city: Some("Hong Kong".into()),
            org: Some("City 库知道的组织".into()),
            ..Default::default()
        };
        let asn = Quality { asn: Some(906), org: Some("DMIT".into()), ..Default::default() };
        let q = merge(Some(city), Some(asn));
        assert_eq!(q.country.as_deref(), Some("HK"));
        assert_eq!(q.asn, Some(906), "ASN 补进来 ✓");
        assert_eq!(q.org.as_deref(), Some("City 库知道的组织"), "**不覆盖**已有的 ✓");
        // 只有 ASN 库可用时 ✓（城市库没配 ✓）
        let only_asn = merge(None, Some(Quality { asn: Some(906), ..Default::default() }));
        assert_eq!(only_asn.asn, Some(906));
        assert_eq!(only_asn.country, None, "没有就是 None ✓ —— 不编 ✗");
        // 两个都没有 ⇒ 全空 ✓
        assert_eq!(merge(None, None), Quality::default());
    }

    /// 数据目录：显式优先 ✓；否则与 DB 同目录 ✓；DB 就在当前目录时退回 `.` ✓。
    #[test]
    fn the_data_dir_follows_the_database_unless_told_otherwise() {
        assert_eq!(data_dir("/var/lib/monitor/hub.db", None), std::path::PathBuf::from("/var/lib/monitor"));
        assert_eq!(data_dir("hub.db", None), std::path::PathBuf::from("."));
        assert_eq!(data_dir("/x/hub.db", Some("/mnt/geo")), std::path::PathBuf::from("/mnt/geo"));
        // 空的显式值当作"没给" ✓ —— 命令行传了个空字符串不该把库丢到别处 ✓。
        assert_eq!(data_dir("/x/hub.db", Some("  ")), std::path::PathBuf::from("/x"));
    }

    /// 库清单里**没有硬编码的地址** ✓✓ —— 这是刻意的 ✗：镜像由设置给（与 `github_proxy` 同形 ✓）。
    /// 这条测试是"别哪天有人图省事塞一个 URL 进来"的护栏 ✓。
    #[test]
    fn no_source_carries_a_hardcoded_url() {
        for s in SOURCES {
            assert!(s.key.starts_with("geo_"), "URL 必须来自设置：{}", s.name);
            assert!(s.name.ends_with(".mmdb") || s.name.ends_with(".txt"), "落盘名要能看出形态：{}", s.name);
            assert!(s.min_bytes >= 1024, "最小尺寸太小挡不住坏下载：{}", s.name);
        }
    }

    /// **下载失败/内容损坏 ⇒ 旧文件原封不动** ✓✓ —— 这一块的命门 ✓。
    #[test]
    fn a_bad_download_never_touches_the_existing_file() {
        let dir = tmpdir("keep");
        // 先放一份"好库" ✓。
        store_atomic(&dir, "City.mmdb", Shape::Mmdb, &fake_mmdb()).unwrap();
        let good = std::fs::read(dir.join("City.mmdb")).unwrap();
        assert_eq!(good, fake_mmdb());

        // 三种坏内容：空的 ✓ · 截断的 ✓ · 一整页 HTML（下载失败最常见的样子 ✓）。
        for bad in [b"".to_vec(), vec![0u8; 16], b"<html>502 Bad Gateway</html>".to_vec()] {
            let e = store_atomic(&dir, "City.mmdb", Shape::Mmdb, &bad);
            assert!(e.is_err(), "坏内容必须被拒绝 ✗");
            assert_eq!(
                std::fs::read(dir.join("City.mmdb")).unwrap(),
                good,
                "**旧文件必须原封不动** —— 这是这一块存在的理由"
            );
        }
        // 临时文件也不该留下 ✓。
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "临时文件要清掉：{leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 文本列表：空文件与 HTML 错误页都要挡住 ✓ —— 它们是下载失败最常见的产物 ✓。
    #[test]
    fn text_lists_reject_empty_and_html() {
        assert!(!looks_like_text_list(""));
        assert!(!looks_like_text_list("   \n\n"));
        assert!(!looks_like_text_list("<html><body>502</body></html>"));
        assert!(!looks_like_text_list("# only comments\n\n"));
        assert!(looks_like_text_list("# comment\n1.2.3.4\n"));
    }

    /// mmdb 的判据是**尾部**的元数据魔数 ✓（头部没有固定签名 ✗）。
    #[test]
    fn mmdb_is_recognised_by_its_tail_marker() {
        assert!(looks_like_mmdb(&fake_mmdb()));
        assert!(!looks_like_mmdb(&[]));
        assert!(!looks_like_mmdb(&vec![0u8; 4096]));
        // 魔数出现在**很远的前面**不算 ✓（真正的 mmdb 里它只在尾部 ✓）——
        // 否则一个碰巧含这串字节的普通文件会被当成库 ✓。
        let mut far = MMDB_MARKER.to_vec();
        far.extend(std::iter::repeat_n(0u8, MMDB_TAIL + 16));
        assert!(!looks_like_mmdb(&far), "远离尾部的不算");
    }
}
