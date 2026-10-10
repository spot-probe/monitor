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
        // 英文优先 ✓；退回中文时**两个键都试** ✗：MaxMind 用 `zh` ✓，
        // 而 DB-IP 的样例里是 **`zh-CN`** ✓（对着它的格式页核实过 ✓）——
        // 只写 `zh` 的话，用 DB-IP 时中文名会永远取不到 ✓，而那是静默的 ✗。
        city: s_at(&["city", "names", "en"])
            .or_else(|| s_at(&["city", "names", "zh"]))
            .or_else(|| s_at(&["city", "names", "zh-CN"])),
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
// 城市库的候选：优先 MaxMind，没有就退回 DB-IP ✓✓。
//
// 两家的字段名互相兼容 ✓（DB-IP 官方写明"尽量贴近既有工具的 schema" ✓，
// 且 country.iso_code / city.names / location.latitude 等都已对着它的格式页样例核实 ✓），
// 所以解析那一份代码两边通用 ✓。
// 差别只有一处 ✗：DB-IP 没有 location.time_zone ⇒ 用 DB-IP 时"时区"是空的 ✓
//（空比错好 ✓，面板上也如实显示为未知 ✓）。
const CITY_FILES: [&str; 2] = ["GeoLite2-City.mmdb", "dbip-city-lite.mmdb"];
/// ASN 库的候选 ✓（同理 ✓）。
const ASN_FILES: [&str; 2] = ["GeoLite2-ASN.mmdb", "dbip-asn-lite.mmdb"];

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
    // 按候选顺序问一遍 ✓，用**第一个存在**的那份 ✓（都不在 ⇒ 未知 ✓，不是错误 ✓）。
    let first = |names: &[&str]| names.iter().find_map(|n| read(n));
    merge(first(&CITY_FILES), first(&ASN_FILES))
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
    /// 厂商发出来的**包装** ✓ —— 免费库几乎都发压缩包 ✗（MaxMind 是 `.tar.gz` ✓、DB-IP 是 `.mmdb.gz` ✓），
    /// 只有 Tor 列表是裸文本 ✓。所以"只收裸 .mmdb"那条老规矩行不通 ✓（`unpack` 负责解开 ✓）。
    pub pack: Pack,
    /// **多久刷一次** ✓✓ —— 必须按来源定 ✗，不能一刀切：
    /// MaxMind 的 EULA 限**每天 30 次下载** ✗（官方页写明 ✓），而 Tor 列表随时小改、没有额度 ✓。
    pub every: std::time::Duration,
}

/// 现在纳入的库 ✓ —— **只放我确认过形态的** ✗。
///
/// ⚠️ 每个库的 `every` 不是随手定的 ✓：MaxMind 官方写明**每天最多 30 次下载** ✗，
/// 所以那两个是**每天一次** ✓（离限额很远 ✓）；而 Tor 列表没有额度限制 ✓，所以每小时 ✓。
/// 把这条约束放在这里 ✓，比放在某个循环里更不容易被改坏 ✓。
/// MaxMind 是 mmdb ✓、Tor 出口是纯文本 ✓；
/// IP2Proxy LITE / DB-IP 的发布形态（mmdb ✗ BIN ✗ CSV ✗）**我没有核实** ✓，
/// 所以先不写进来 ✓ —— 核实之后各加一行即可 ✓（它们的形态字段已经准备好 ✓）。
pub const SOURCES: [Source; 5] = [
    Source {
        name: "GeoLite2-City.mmdb",
        pack: Pack::TarGz,
        shape: Shape::Mmdb,
        key: "geo_city_url",
        min_bytes: 1 << 20,
        every: std::time::Duration::from_secs(24 * 3600),
    },
    Source {
        name: "GeoLite2-ASN.mmdb",
        pack: Pack::TarGz,
        shape: Shape::Mmdb,
        key: "geo_asn_url",
        min_bytes: 1 << 20,
        every: std::time::Duration::from_secs(24 * 3600),
    },
    Source {
        name: "dbip-city-lite.mmdb",
        shape: Shape::Mmdb,
        key: "geo_city_url",
        min_bytes: 1 << 20,
        pack: Pack::Gzip,
        every: std::time::Duration::from_secs(24 * 3600),
    },
    Source {
        name: "dbip-asn-lite.mmdb",
        shape: Shape::Mmdb,
        key: "geo_asn_url",
        min_bytes: 1 << 19,
        pack: Pack::Gzip,
        every: std::time::Duration::from_secs(24 * 3600),
    },
    Source {
        name: "tor-exit.txt",
        pack: Pack::Raw,
        shape: Shape::Text,
        key: "geo_tor_url",
        min_bytes: 1 << 10,
        every: std::time::Duration::from_secs(3600),
    },
];

/// 把配置里的 URL 展开成**这一次真正要取**的地址 ✓ —— 只替换日期占位符 ✓。
///
/// 支持 `{YYYY}` ✓ · `{YYYY-MM}` ✓ · `{YYYY-MM-DD}` ✓（都按给定的日期 ✓）。
///
/// **为什么需要它** ✗：几家免费库的直链是**带日期**的 ✓（DB-IP 那种
/// `dbip-city-lite-2026-10.mmdb.gz` ✓）—— 把某个具体月份写进设置，
/// **下个月就 404** ✓，而症状是"IP 质量莫名变空" ✗，
/// 与"一次抖动把库写坏"是同一类：代价在别处、动静在这里 ✓。
/// 让运维写 `{YYYY-MM}` ✓，每月就自动对上 ✓。
///
/// **纯函数** ✓（日期由调用方给 ✓）⇒ 它的边界能用测试钉死 ✓，不必等到下个月 ✓。
pub fn expand(url: &str, now: chrono::NaiveDate) -> String {
    url.replace("{YYYY-MM-DD}", &now.format("%Y-%m-%d").to_string())
        .replace("{YYYY-MM}", &now.format("%Y-%m").to_string())
        .replace("{YYYY}", &now.format("%Y").to_string())
}

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

/// 厂商文件的**包装形态** ✓。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pack {
    /// 裸文件 ✓（Tor 列表 ✓）。
    Raw,
    /// `.gz` ✓（DB-IP 的 `.mmdb.gz` ✓）。
    Gzip,
    /// `.tar.gz` ✓（MaxMind 的下载包 ✓ —— 里面还带一层目录 ✓，所以要挑出那个 `.mmdb` ✓）。
    TarGz,
}

/// 解开包装 ✓，返回**里面那个真正的库文件** ✓。
pub fn unpack(pack: Pack, name: &str, bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    match pack {
        Pack::Raw => Ok(bytes.to_vec()),
        Pack::Gzip => gunzip(bytes),
        Pack::TarGz => {
            let tar = gunzip(bytes)?;
            let mut archive = tar::Archive::new(std::io::Cursor::new(tar));
            for entry in archive.entries()? {
                let mut entry = entry?;
                // 按**后缀**挑 ✓：MaxMind 包里那句真库叫 `GeoLite2-City.mmdb` ✓，
                // 而目录名带日期 ✓（`GeoLite2-City_20260307/` ✓）⇒ 不能按整条路径比 ✓。
                let path = entry.path()?.to_string_lossy().into_owned();
                if path.ends_with(".mmdb") {
                    let mut out = Vec::with_capacity(entry.size() as usize);
                    std::io::copy(&mut entry, &mut out)?;
                    return Ok(out);
                }
            }
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{name}: the archive holds no .mmdb file"),
            ))
        }
    }
}

/// 解开一层 gzip ✓。
fn gunzip(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(bytes).read_to_end(&mut out)?;
    Ok(out)
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
    // 日期占位符在这里展开 ✓（`{YYYY-MM}` 等 ✓）—— 见 `expand` 的说明 ✓：
    // 带月份的直链写死了就会在某个月的第一天开始 404 ✓，而症状是"IP 质量莫名变空" ✗。
    let url = expand(url, chrono::Utc::now().date_naive());
    let got = app.http.get(&url).send().await;
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
    // 厂商发出来的多半是**压缩包** ✗（MaxMind 是 .tar.gz ✓、DB-IP 是 .mmdb.gz ✓）
    // ⇒ 先解开再校验 ✓（校验器只认裸 .mmdb ✓）。
    // ⚠️ **顺序要紧** ✗：解压失败 ⇒ 直接返回 ⇒ **旧文件一动不动** ✓
    //（与"下载失败不覆盖"是同一道防线 ✓，只是发生在更前面一步 ✓）。
    let bytes = match unpack(source.pack, source.name, &bytes) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("geo: {}: cannot unpack the download: {e:#}", source.name);
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

/// 启动后不久拉一轮 ✓，然后**每个库按自己的间隔**刷 ✓。
///
/// ⚠️ **不能一刀切** ✗（我原来是『每小时把所有库都拉一遍』✗）：
/// MaxMind 官方限**每天 30 次下载** ✓ —— 每小时一次就是每天 24 次城市 + 24 次 ASN = **48 次** ✗✗，
/// 超限之后的表现是『库更新不了、IP 质量慢慢变旧』✓，而**没有任何报错指向这里** ✗。
/// 所以间隔写在每个 [`Source`] 上 ✓（见那里关于 30 次的注释 ✓）。
///
/// 循环本身**每 10 分钟**醒一次 ✓，只为看谁到点了 ✓ —— 比按最长间隔睡更稳 ✓：
/// 将来加一个『每小时』的新库，也不用改这里 ✓。
pub async fn watch(app: crate::Shared) {
    let mut last: std::collections::HashMap<&'static str, std::time::Instant> =
        std::collections::HashMap::new();
    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    loop {
        for source in SOURCES.iter() {
            // 第一次必然到点 ✓（表是空的 ✓）⇒ 启动后 30 秒那一轮会把该拉的都拉了 ✓。
            let due = last.get(source.name).is_none_or(|t| t.elapsed() >= source.every);
            if due {
                last.insert(source.name, std::time::Instant::now());
                let _ = fetch(&app, source).await;
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(600)).await;
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

    /// **解压这条路要真的被执行过** ✓✓ —— 往返测一次，而不是只测"坏包会被拒" ✗。
    /// 这一条很重要 ✗：`lookup` 调 mmdb 那几行是"薄到不用测"的 ✓，
    /// 于是"厂商给的包能不能解开"就成了最容易在真机上第一次才发现的事 ✓。
    #[test]
    fn archives_round_trip_to_the_database_inside() {
        use std::io::Write;
        let inner = fake_mmdb();

        // ① `.gz` ✓（DB-IP 那种 ✓）
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(&inner).unwrap();
        let gz = enc.finish().unwrap();
        assert!(looks_like_mmdb(&unpack(Pack::Gzip, "x.mmdb", &gz).unwrap()), "解出来要是那份库 ✓");

        // ② `.tar.gz` ✓（MaxMind 那种 ✓ —— 而且**带一层目录** ✓，所以要按后缀挑 ✓）
        let mut tar = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(inner.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "GeoLite2-City_20260307/GeoLite2-City.mmdb", inner.as_slice()).unwrap();
        let tarball = tar.into_inner().unwrap();
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(&tarball).unwrap();
        let tgz = enc.finish().unwrap();
        assert!(
            looks_like_mmdb(&unpack(Pack::TarGz, "x.mmdb", &tgz).unwrap()),
            "带日期目录的 tar.gz 也要挑出那份库 ✓"
        );

        // ③ 裸文件原样通过 ✓
        assert_eq!(unpack(Pack::Raw, "x.txt", b"1.2.3.4\n").unwrap(), b"1.2.3.4\n");
        // ④ 坏包/不是压缩包 ⇒ 报错 ✓（调用方据此**保留旧文件** ✓）
        assert!(unpack(Pack::Gzip, "x.mmdb", b"not gzip at all").is_err());
        assert!(unpack(Pack::TarGz, "x.mmdb", &gz).is_err(), "只有 gz、没有 tar ⇒ 拒绝");
        // ⑤ tar.gz 里**没有 .mmdb** ⇒ 明说 ✓（别把一整包文档当成数据库 ✓）
        let mut tar = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(3);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "README.md", b"hi\n".as_slice()).unwrap();
        let plain = tar.into_inner().unwrap();
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(&plain).unwrap();
        let no_db = enc.finish().unwrap();
        assert!(unpack(Pack::TarGz, "x.mmdb", &no_db).is_err(), "包里没有 .mmdb ⇒ 拒绝");
    }

    /// URL 里的日期占位符 ✓✓ —— 这条测试的意义是"**不必等到下个月**"就能验 ✓：
    /// 写死月份的地址会在某个月的第一天开始 404 ✓，而那种错在事发前完全看不出来 ✓。
    #[test]
    fn date_placeholders_are_filled_in_from_the_given_day() {
        let d = chrono::NaiveDate::from_ymd_opt(2026, 3, 7).unwrap();
        assert_eq!(
            expand("https://download.db-ip.com/free/dbip-city-lite-{YYYY-MM}.mmdb.gz", d),
            "https://download.db-ip.com/free/dbip-city-lite-2026-03.mmdb.gz",
            "**月份要补零** ✓（3 月必须写成 03 ✓ —— 少一位就是一个 404 ✓）"
        );
        assert_eq!(expand("https://x/{YYYY}/y.zip", d), "https://x/2026/y.zip");
        assert_eq!(expand("https://x/{YYYY-MM-DD}/z", d), "https://x/2026-03-07/z");
        // 没有占位符 ⇒ **原样返回** ✓（绝大多数地址是这样的 ✓）
        assert_eq!(expand("https://example.com/City.mmdb", d), "https://example.com/City.mmdb");
        // 两个占位符同时出现也要都对 ✓
        assert_eq!(expand("https://x/{YYYY}/{YYYY-MM}", d), "https://x/2026/2026-03");
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
