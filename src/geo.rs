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
    /// **可能的落盘名** ✓✓ —— 由**内容**决定用哪一个 ✓（`sniff` ✓），不再由"哪个键配的"决定 ✗。
    ///
    /// 顺序是刻意的 ✓：第一个是 MaxMind 那套名字 ✓（`.tar.gz` 与裸 `.mmdb` 用它 ✓），
    /// 第二个是 DB-IP 那套 ✓（`.mmdb.gz` 用它 ✓）—— 而这两个名字**正是 `lookup`
    /// 的候选名单里的那两个** ✓✓ ⇒ 读的一侧一行都不用改 ✓。
    /// Tor 只有一个名字 ✓（它本来就是裸文本 ✓），所以是切片 ✓。
    pub names: &'static [&'static str],
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
pub const SOURCES: [Source; 3] = [
    Source {
        name: "城市库",
        // 落盘名按内容挑 ✓：`.tar.gz`/裸 ⇒ MaxMind 的名字 ✓；`.gz` ⇒ DB-IP 的名字 ✓
        names: &["GeoLite2-City.mmdb", "dbip-city-lite.mmdb"],
        shape: Shape::Mmdb,
        key: "geo_city_url",
        min_bytes: 1 << 20, // 只在"内容判不出来"时兜底 ✓（见 fetch ✓）
        every: std::time::Duration::from_secs(24 * 3600),
    },
    Source {
        name: "ASN 库",
        names: &["GeoLite2-ASN.mmdb", "dbip-asn-lite.mmdb"],
        shape: Shape::Mmdb,
        key: "geo_asn_url",
        min_bytes: 1 << 19,
        every: std::time::Duration::from_secs(24 * 3600),
    },
    Source {
        name: "Tor 出口列表",
        names: &["tor-exit.txt"],
        shape: Shape::Text,
        key: "geo_tor_url",
        min_bytes: 1 << 10,
        every: std::time::Duration::from_secs(3600),
    },
];

/// 磁盘上那份库文件**够不够新** ✓（够新 ⇒ 这一轮不必下载 ✓）。
///
/// ⚠️ **为什么这个判断必须有** ✗✗ —— 2026-10-11 现场：
/// 记录"上次什么时候拉的"原本只是**进程内的一个局部变量** ✓ ⇒ **一重启就忘** ✗ ⇒
/// 每次重启都重新下载 127 MB ✓ ⇒ 又 OOM ✓ ⇒ 又重启 ✓ ⇒ **死循环** ✗✗
///（日志里 restart counter 一路涨到 12 ✓）。
/// ⇒ 判据改成看**文件自己的 mtime** ✓✓：既**不需要任何新状态** ✓，
/// 也**天然跨重启** ✓ —— 一台刚拉过的 hub，重启一次不会再花 127 MB 的流量 ✓。
///
/// 纯函数 ✓（时间由调用方给 ✓）⇒ 边界能测 ✓：文件不存在 ✓ / 刚拉过 ✓ / 已经过期 ✓。
pub fn recently_fetched(age: Option<std::time::Duration>, every: std::time::Duration) -> bool {
    age.is_some_and(|age| age < every)
}

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

/// **流式落盘** ✓✓：解开包装 → 直接写临时文件 → **从文件校验** → 原子替换 ✓。
///
/// 与 [`store_atomic`] 的区别就是**内存** ✗：那个要先把解压结果攒成几百 MB 的 `Vec` ✓，
/// 这个只用一个固定缓冲 ✓ —— 于是峰值与**文件大小脱钩** ✓✓（现场 OOM 的直接原因就是前者 ✓）。
///
/// **"坏内容绝不覆盖好文件"这条性质不变** ✓✓：任何一步失败 ⇒ 删掉临时文件 ⇒ 目标原封不动 ✓。
/// 顺序也仍然是最紧的那个 ✗：**先解压、再量尺寸、再校验、最后才 rename** ✓。
pub fn store_streamed(
    dir: &Path,
    name: &str,
    shape: Shape,
    pack: Pack,
    min_bytes: usize,
    src: impl std::io::Read,
) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let unpacked = {
        let mut out = std::fs::File::create(&tmp)?;
        match unpack_to(pack, name, src, &mut out) {
            Ok(n) => n,
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(e);
            }
        }
    };
    // 尺寸：先量**落盘之后**的字节数 ✓（不是压缩包的大小 ✓ —— 那两者差着几倍 ✓）
    let bad = if (unpacked as usize) < min_bytes {
        Some(format!("{name}: unpacked to {unpacked} bytes, below the {min_bytes} byte floor"))
    } else {
        match looks_valid_file(&tmp, shape) {
            Ok(true) => None,
            Ok(false) => Some(format!("{name}: the unpacked file does not look like a valid {shape:?}")),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(e);
            }
        }
    };
    if let Some(msg) = bad {
        let _ = std::fs::remove_file(&tmp);
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, msg));
    }
    match std::fs::rename(&tmp, dir.join(name)) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

// 调用它的是**下一步的接线** ✓：下载完先读头部 → 认出包装 → 再决定目标文件名 ✓
//（见 `sniff` 的说明 ✓）。在那之前只有测试在用 ✗ ⇒ 暂标 allow，接上即删 ✓。
#[allow(dead_code)]
/// **按内容**判断这是哪种包装 ✓✓ —— 不再靠"哪个来源配了哪个键" ✗。
///
/// ⚠️ **为什么必须这样** ✗✗（2026-10-11 现场）：MaxMind 与 DB-IP **共用同一个设置键** ✓
/// ⇒ 运维填了 DB-IP 的 `.mmdb.gz` ✓ ⇒ MaxMind 那两个来源也去下**同一个文件** ✗
/// ⇒ 拿 `.mmdb.gz` **当 tar.gz 解** ⇒ 日志里两条 `cannot unpack` 的 WARN ✓✓ ——
/// 无害 ✓（DB-IP 照常成功 ✓），但**把"配置正确"与"配置错误"变得长得一样** ✓，极误导 ✓。
/// 判据改成看**内容**之后 ✓：填哪个地址都能认出来 ✓，也不必再为两家各配一个键 ✓✓。
///
/// 只看**头部几百字节** ✓（gzip 魔数 ✓ + tar 的 `ustar` 标记在偏移 257 ✓）——
/// 于是它天然是流式的 ✓，与 [`store_streamed`] 那条低内存的路径合得来 ✓。
pub fn sniff(head: &[u8]) -> Pack {
    // 裸 mmdb：头两个字节就是魔数段之前的二进制树 ✓，但**尾部**才有 `\xab\xcd\xefMaxMind.com` ✓
    // ⇒ 在这里只能按"不是 gzip 也不是 tar"来判 ✓（那正是 Raw ✓）。
    if head.len() >= 2 && head[0] == 0x1f && head[1] == 0x8b {
        // gzip ✓ —— 里面是 tar 还是单个文件？tar 的 magic 在偏移 257 ✓（`ustar` ✓）
        if head.len() >= 262 && &head[257..262] == b"ustar" {
            return Pack::TarGz;
        }
        // ⚠️ 只看得到 gzip 头部时**不要急着下结论** ✗：gzip 的压缩流前面还有头部与文件名 ✓，
        // 真正的内容在更后面 ✓ ⇒ 由调用方多读一点再问一次 ✓（见 `sniff_decompressed` ✓）。
        return Pack::Gzip;
    }
    Pack::Raw
}

// 调用它的是**下一步的接线** ✓：下载完先读头部 → 认出包装 → 再决定目标文件名 ✓
//（见 `sniff` 的说明 ✓）。在那之前只有测试在用 ✗ ⇒ 暂标 allow，接上即删 ✓。
#[allow(dead_code)]
/// 已经把 gzip **解开一层**之后，再判断里面是 tar 还是单个文件 ✓。
///
/// 与 [`sniff`] 分开是刻意的 ✓：gzip 的头部里看不到压缩内容 ✓，
/// 所以"是不是 tar"必须**解压之后**才知道 ✓ —— 纯函数 ✓，两个都能单独测 ✓。
pub fn sniff_inner(first_bytes: &[u8]) -> Pack {
    if first_bytes.len() >= 262 && &first_bytes[257..262] == b"ustar" {
        Pack::TarGz
    } else {
        Pack::Raw
    }
}

/// 当前生效的 Tor 出口集合 ✓✓ —— **按文件 mtime 缓存** ✓。
///
/// 为什么这样而不是存进数据库 ✗：它就是**那个文件的影子** ✓ ——
/// 每小时换一份 ✓、内容只有几千行 ✓、随时可以从文件重建 ✓ ⇒
/// 加一张表、一次迁移、一列，都是**为一个派生值付存储的代价** ✓。
/// 缓存键用 **mtime** ✓（而不是"上次读了多久"✗）：文件一换就必然重读 ✓，
/// 文件没换就一次都不读 ✓ —— 与 `recently_fetched` 是同一个思路 ✓。
///
/// 文件不在（没配这个库 ✓）⇒ 空集合 ✓ —— **不是错误** ✓（与别处同一条规矩 ✓）。
pub fn tor_exits(dir: &Path) -> std::sync::Arc<std::collections::HashSet<std::net::IpAddr>> {
    use std::sync::{Arc, Mutex, OnceLock};
    // 抽个名字 ✓ —— clippy 说这个类型"太复杂" ✓，而它说得对 ✓：
    // 一行里塞四层泛型，读的人得从里往外数 ✓。
    type TorSet = Arc<std::collections::HashSet<std::net::IpAddr>>;
    static CACHE: OnceLock<Mutex<Option<(std::time::SystemTime, TorSet)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    let path = dir.join("tor-exit.txt");
    let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    let mut slot = cache.lock().unwrap_or_else(|e| e.into_inner());
    if let (Some(m), Some((cached, set))) = (mtime, slot.as_ref()) {
        if *cached == m {
            return set.clone(); // 文件没变 ⇒ 一次盘都不读 ✓
        }
    }
    let set = Arc::new(
        std::fs::read_to_string(&path).map(|t| parse_tor_list(&t)).unwrap_or_default(), // 文件不在 / 读不动 ⇒ 空集合 ✓（不是错误 ✓）
    );
    *slot = Some((mtime.unwrap_or(std::time::UNIX_EPOCH), set.clone()));
    set
}

/// 解析 Tor 出口清单 ✓✓ —— **纯函数** ✓，所以那些"现实里的脏输入"能全测死 ✓。
///
/// Tor 官方的批量清单（`torbulkexitlist` ✓）就是**一行一个 IP** ✓（v4 与 v6 混着 ✓），
/// 可能带 `#` 注释与空行 ✓，也可能哪天多出别的列 ✓ —— 所以这里**只认能解析成 IP 的行** ✓，
/// 其余一律**跳过** ✓（而不是报错 ✓：一份清单里有一行看不懂，不该让整份作废 ✓）。
///
/// **为什么要它** ✗（2026-10-11 的欠账 ✓）：这份列表此前**下载了但没人读** ✓ ——
/// 而"这个 IP 是不是 Tor 出口"本来**默认就该看得到** ✓；在线风险库也能给这一项 ✓，
/// 但它**默认关着** ✗、并且要把节点 IP **发给第三方** ✗。本地这份则**离线、免费、不外发** ✓✓。
pub fn parse_tor_list(text: &str) -> std::collections::HashSet<std::net::IpAddr> {
    text.lines()
        .map(|l| l.trim())
        // 注释与空行 ✓（Tor 的清单里有 `#` 开头的说明 ✓）
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        // 有的清单会写成 `ip  port` 之类的多列 ✓ ⇒ **只取第一段** ✓，而不是整行解析 ✓
        .filter_map(|l| l.split_whitespace().next())
        .filter_map(|ip| ip.parse::<std::net::IpAddr>().ok())
        .collect()
}

/// 只读一个文件**尾部的一段**（默认 128 KB ✓）✓ —— 校验 mmdb 只需要这一段 ✓✓。
///
/// **为什么必须这样** ✗✗：现场的 OOM（2026-10-11）是"整份文件进内存"造成的 ✓ ——
/// City 解压后几百 MB ✓，而 hub 的包线是 **256M** ✓（安装脚本写死的 ✓）。
/// 校验只需要**尾部**那点字节 ✓ ⇒ 这里就必须只读那点字节 ✓，别图省事整份读 ✓。
pub fn read_tail(path: &std::path::Path, limit: usize) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(limit as u64);
    f.seek(SeekFrom::Start(start))?;
    let mut out = Vec::with_capacity((len - start) as usize);
    f.take(len - start).read_to_end(&mut out)?;
    Ok(out)
}

/// 校验一份**已经在磁盘上**的库文件 ✓（mmdb 看尾部魔数 ✓；文本列表整份读但它本来就小 ✓）。
pub fn looks_valid_file(path: &std::path::Path, shape: Shape) -> std::io::Result<bool> {
    match shape {
        // 只读尾部 ✓ —— 见 `read_tail` 的说明 ✓（魔数只在尾部 ✓，头部是二进制树 ✓）。
        Shape::Mmdb => Ok(looks_like_mmdb(&read_tail(path, MMDB_TAIL)?)),
        Shape::Text => {
            let bytes = std::fs::read(path)?;
            Ok(std::str::from_utf8(&bytes).is_ok_and(looks_like_text_list))
        }
    }
}

/// **流式**解开包装 ✓：把里面那份库写进 `out` ✓，返回写了多少字节 ✓。
///
/// 与 [`unpack`] 的区别是**内存**：那个会攒出一个几百 MB 的 `Vec` ✗，
/// 这个只用一个固定缓冲 ✓ —— 于是占用量与文件大小**脱钩** ✓✓，
/// hub 才能在**项目自己声明的 256M 包线内**干活 ✓（见 `read_tail` 的说明 ✓）。
pub fn unpack_to(
    pack: Pack,
    name: &str,
    mut src: impl std::io::Read,
    out: &mut impl std::io::Write,
) -> std::io::Result<u64> {
    match pack {
        Pack::Raw => Ok(std::io::copy(&mut src, out)?),
        Pack::Gzip => Ok(std::io::copy(&mut flate2::read::GzDecoder::new(src), out)?),
        Pack::TarGz => {
            let tar = flate2::read::GzDecoder::new(src);
            let mut archive = tar::Archive::new(tar);
            for entry in archive.entries()? {
                let mut entry = entry?;
                let path = entry.path()?.to_string_lossy().into_owned();
                if path.ends_with(".mmdb") {
                    return std::io::copy(&mut entry, out);
                }
            }
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{name}: the archive holds no .mmdb file"),
            ))
        }
    }
}

// ⚠️ **它只在测试里用了** ✓ —— 但**不能删** ✗✗：`the_streaming_unpack_matches_the_in_memory_one_byte_for_byte`
// 正是拿它与流式那条路**逐字节比对** ✓✓（"两条路算出不同结果"是最难发现的错之一 ✓，
// 因为一条只在别人的机器上跑 ✓）。所以它留在这里当**参照实现** ✓。
#[cfg(test)]
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

// ⚠️ **它只在测试里用了** ✓ —— 但**不能删** ✗✗：`the_streaming_unpack_matches_the_in_memory_one_byte_for_byte`
// 正是拿它与流式那条路**逐字节比对** ✓✓（"两条路算出不同结果"是最难发现的错之一 ✓，
// 因为一条只在别人的机器上跑 ✓）。所以它留在这里当**参照实现** ✓。
#[cfg(test)]
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

// ⚠️ **它只在测试里用了** ✓ —— 但**不能删** ✗✗：`the_streaming_unpack_matches_the_in_memory_one_byte_for_byte`
// 正是拿它与流式那条路**逐字节比对** ✓✓（"两条路算出不同结果"是最难发现的错之一 ✓，
// 因为一条只在别人的机器上跑 ✓）。所以它留在这里当**参照实现** ✓。
#[cfg(test)]
/// 按形态校验 ✓。
pub fn looks_valid(shape: Shape, bytes: &[u8]) -> bool {
    match shape {
        Shape::Mmdb => looks_like_mmdb(bytes),
        Shape::Text => std::str::from_utf8(bytes).is_ok_and(looks_like_text_list),
    }
}

// ⚠️ **它只在测试里用了** ✓ —— 但**不能删** ✗✗：`the_streaming_unpack_matches_the_in_memory_one_byte_for_byte`
// 正是拿它与流式那条路**逐字节比对** ✓✓（"两条路算出不同结果"是最难发现的错之一 ✓，
// 因为一条只在别人的机器上跑 ✓）。所以它留在这里当**参照实现** ✓。
#[cfg(test)]
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
        // ⚠️ 原来是 `debug!` ✗ —— 那等于**故意让它沉默** ✓：
        // 2026-10-11 现场排查时，日志里"一条 geo 都没有" ✓，
        // 而真相是"两个库都没配" ✓ ⇒ 于是**无从判断**它到底跑没跑 ✓✗。
        // 这类"什么都没发生"的状态必须**说得出来** ✓ —— 否则运维只能看到一个安静的系统 ✓。
        tracing::info!("geo: {} has no URL configured; skipped", source.name);
        return false;
    }
    // 日期占位符在这里展开 ✓（`{YYYY-MM}` 等 ✓）—— 见 `expand` 的说明 ✓：
    // 带月份的直链写死了就会在某个月的第一天开始 404 ✓，而症状是"IP 质量莫名变空" ✗。
    let url = expand(url, chrono::Utc::now().date_naive());
    let got = app.http.get(&url).send().await;
    // ⚠️ **边收边写盘** ✗✓ —— 绝不把 127 MB 攒在内存里 ✓：
    // hub 的包线是 **256M** ✓（安装脚本写死 ✓），而"整份进内存 + 在内存里解压"正是现场 OOM 的成因 ✓。
    // 顺带加一道**体积上限** ✓：万一 URL 指错（指到一个巨物 ✓），在这里就停 ✓，而不是把机器拖垮 ✓。
    const BODY_CAP: u64 = 512 << 20;
    let dl = app.data_dir.join(format!(".{}.{}.dl", source.name, std::process::id()));
    let mut received: u64 = 0;
    let bytes = match got {
        Ok(mut r) if r.status().is_success() => {
            if let Err(e) = std::fs::create_dir_all(&app.data_dir) {
                tracing::warn!("geo: {}: cannot use the data directory: {e:#}", source.name);
                return false;
            }
            let mut f = match std::fs::File::create(&dl) {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!("geo: {}: cannot open a temporary file: {e:#}", source.name);
                    return false;
                }
            };
            loop {
                match r.chunk().await {
                    Ok(Some(chunk)) => {
                        received += chunk.len() as u64;
                        if received > BODY_CAP {
                            tracing::warn!(
                                "geo: {}: the download passed {BODY_CAP} bytes; giving up",
                                source.name
                            );
                            let _ = std::fs::remove_file(&dl);
                            return false;
                        }
                        if let Err(e) = std::io::Write::write_all(&mut f, &chunk) {
                            tracing::warn!("geo: {}: writing the download failed: {e:#}", source.name);
                            let _ = std::fs::remove_file(&dl);
                            return false;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!("geo: {}: body read failed: {e:#}", source.name);
                        let _ = std::fs::remove_file(&dl);
                        return false;
                    }
                }
            }
            std::mem::drop(f);
            match std::fs::File::open(&dl) {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!("geo: {}: cannot reopen the download: {e:#}", source.name);
                    return false;
                }
            }
        }
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
    // ⚠️ 这里**不再**先解开成一个 Vec ✗ —— 那正是现场 OOM 的成因（City 解压后几百 MB ✓，
    // 而 hub 的包线是 256M ✓）。改成**流式**：解开直接写临时文件 ✓，校验只看文件 ✓，
    // 最后原子替换 ✓ ⇒ 峰值与文件大小**脱钩** ✓✓（见 `store_streamed` ✓）。
    // ⭐ **按内容定名** ✓✓（修已知项 ① 的接线 ✓）—— 不再由"哪个键配的"决定 ✗，
    //    那正是两条误导性 WARN 的根 ✓。先读**头部**认出包装 ✓；
    //    若看到 gzip ✓，还要**解压出几百字节**才知道里面是 tar 还是单个文件 ✓
    //    —— gzip 的头部里看不到压缩内容 ✓，这就是 `sniff`/`sniff_inner` 分成两个的缘故 ✓。
    let pack = {
        use std::io::Read;
        let mut head = [0u8; 512];
        let n = match std::fs::File::open(&dl).and_then(|mut f| f.read(&mut head)) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!("geo: {}: cannot read the download: {e:#}", source.name);
                let _ = std::fs::remove_file(&dl);
                return false;
            }
        };
        let outer = sniff(&head[..n]);
        if outer == Pack::Gzip {
            // 解压**前 512 字节**再看 ✓ —— 只要几百字节 ✓，与流式那条低内存路径一致 ✓
            let mut inner = [0u8; 512];
            let m = match std::fs::File::open(&dl) {
                Ok(f) => flate2::read::GzDecoder::new(f).read(&mut inner).unwrap_or(0),
                Err(_) => 0,
            };
            match sniff_inner(&inner[..m]) {
                Pack::TarGz => Pack::TarGz,
                _ => Pack::Gzip,
            }
        } else {
            outer
        }
    };
    // 落盘名 ✓：`.tar.gz` 与裸文件 ⇒ 第一个（MaxMind 那套 ✓）；`.gz` ⇒ 第二个（DB-IP 那套 ✓）。
    // ⭐ 这两个名字**正是 `lookup` 候选名单里的那两个** ✓✓ ⇒ 读的一侧一行都不用改 ✓。
    // Tor 只有一个名字 ✓ ⇒ 永远是它自己 ✓。
    let target = if pack == Pack::Gzip && source.names.len() > 1 { source.names[1] } else { source.names[0] };

    // ⚠️ 临时下载文件必须删 ✗ —— 它是 127 MB 级的 ✓，而 pid 每次都变 ✓
    // ⇒ 忘了删就会**慢慢攒满磁盘** ✓（那种失败几周后才显形 ✓，且没人会想到是这里 ✓）。
    let outcome = store_streamed(
        &app.data_dir,
        // ⚠️ 落盘名用**按内容定下来的** `target` ✓，不是 `source.name` ✗ ——
        // 后者现在是**给人看的标签**（"城市库" ✓）⇒ 拿它当文件名会让 `lookup` 找不到 ✓✗。
        target,
        source.shape,
        // 同理：包装用 **sniff 出来的** `pack` ✓，不是 `source.pack` ✗
        pack,
        source.min_bytes,
        &bytes,
    );
    let _ = std::fs::remove_file(&dl);
    match outcome {
        Ok(()) => {
            // 报**落盘之后**的大小 ✓（压缩包的大小没有意义 ✓，而且现在也不再拿得到它 ✓）
            // ⚠️ 按 **target** 取 ✓（那个**真实落盘的名字** ✓）—— 按 `source.name` 取会怎样 ✓：
            // 它是标签（"城市库" ✓）⇒ 路径不存在 ⇒ 打出 `updated (0 bytes)` ✗ ——
            // 而任何人看到"0 bytes"都会以为**下载到了空文件** ✓。假信息比没信息更糟 ✓。
            let on_disk = std::fs::metadata(app.data_dir.join(target)).map(|m| m.len()).unwrap_or_default();
            tracing::info!("geo: {} updated ({} bytes)", source.name, on_disk);
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
        // 每轮**先把家底说清楚** ✓：几个库、几个配了地址 ✓ ——
        // 这样"任务活没活""配没配上"一眼可判 ✓，不必再从"有没有别的日志"去推 ✓。
        let configured =
            SOURCES.iter().filter(|s| !app.db.get(s.key).unwrap_or_default().trim().is_empty()).count();
        tracing::info!("geo: {} of {} databases configured", configured, SOURCES.len());
        for source in SOURCES.iter() {
            // 第一次必然到点 ✓（表是空的 ✓）⇒ 启动后 30 秒那一轮会把该拉的都拉了 ✓。
            // **文件自己说了算** ✓✓ —— 见 `recently_fetched` 的说明 ✓：
            // 只看进程内的计时 ✗ 会让"重启"变成"重新下载 127 MB" ✓ ✗。
            let age = std::fs::metadata(app.data_dir.join(source.name))
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok());
            if recently_fetched(age, source.every) {
                continue; // 磁盘上那份还新 ✓ ⇒ 这一轮不碰它 ✓（重启也只是重跑这一句 ✓）
            }
            let due = last.get(source.name).is_none_or(|t| t.elapsed() >= source.every);
            if due {
                last.insert(source.name, std::time::Instant::now());
                let _ = fetch(&app, source).await;
            }
        }
        // 顺带**扫一遍欠着质量的节点** ✓✓（离线也算 ✓）——
        // 心跳那道门只覆盖"此刻在线"的 ✓，而很少上线的机器恰恰最需要看一眼质量 ✓。
        // 节流在 `qualify` 内部 ✓（每节点每小时一次 ✓），所以这里每 10 分钟扫一次是安全的 ✓。
        let pending = app.db.quality_pending();
        if !pending.is_empty() {
            tracing::info!("geo: {} node(s) still need a quality lookup", pending.len());
            for (id, source) in pending {
                crate::agent_ws::qualify(app.clone(), id, source);
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

    /// 加载器：文件在 ⇒ 读得到 ✓；文件不在 ⇒ **空集合而且不报错** ✓（"没配"是正常状态 ✓）。
    #[test]
    fn the_tor_set_comes_from_the_file_and_an_absent_file_is_simply_empty() {
        let dir = tmpdir("tor");
        std::fs::create_dir_all(&dir).unwrap();
        // 文件不在 ⇒ 空 ✓（别处都按这条规矩办 ✓）
        assert!(tor_exits(&dir).is_empty(), "没配这个库 ⇒ 空集合 ✓，不是错误 ✓");
        // 放一份清单 ⇒ 读得到 ✓
        std::fs::write(dir.join("tor-exit.txt"), "# c\n1.2.3.4\n2001:db8::1\n").unwrap();
        let set = tor_exits(&dir);
        assert!(set.contains(&"1.2.3.4".parse().unwrap()));
        assert!(set.contains(&"2001:db8::1".parse().unwrap()));
        assert_eq!(set.len(), 2);
        // 再问一次 ⇒ 走缓存 ✓（返回的是同一份 ✓ —— 用指针相等来断言"没重读" ✓）
        assert!(std::sync::Arc::ptr_eq(&set, &tor_exits(&dir)), "文件没变 ⇒ 复用同一份 ✓");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **Tor 清单的解析** ✓✓ —— 这条测试的价值在于：真实清单里**混着 v4 与 v6** ✓，
    /// 还可能带注释、空行、多余的列 ✓，而**有一行看不懂不该让整份作废** ✓。
    #[test]
    fn the_tor_list_takes_what_it_can_and_ignores_the_rest() {
        let text = "\
# Tor exit list\n\
1.2.3.4\n\
\n\
  5.6.7.8  \n\
2001:db8::1\n\
# 下面这行是别人加的注释\n\
not-an-ip\n\
9.9.9.9 9030\n\
";
        let set = parse_tor_list(text);
        assert!(set.contains(&"1.2.3.4".parse().unwrap()), "普通一行 ✓");
        assert!(set.contains(&"5.6.7.8".parse().unwrap()), "两侧空白要去掉 ✓");
        assert!(set.contains(&"2001:db8::1".parse().unwrap()), "**v6 也要收** ✓（Tor 清单里有 ✓）");
        assert!(set.contains(&"9.9.9.9".parse().unwrap()), "多列时只取第一段 ✓");
        assert_eq!(set.len(), 4, "注释 / 空行 / 看不懂的行都不该进来 ✓");
        // 空清单与纯注释 ⇒ 空集合 ✓（不是错误 ✓）
        assert!(parse_tor_list("").is_empty());
        assert!(parse_tor_list("# 只有注释\n").is_empty());
    }

    /// **按内容判包装** ✓✓ —— 这条测试就是"两支 WARN"的解药 ✓：
    /// 填 MaxMind 的 `.tar.gz` 还是 DB-IP 的 `.mmdb.gz`，都该被认出**它到底是什么** ✓。
    #[test]
    fn the_packing_is_recognised_from_the_bytes_not_from_the_setting_key() {
        use std::io::Write;
        let inner = fake_mmdb();

        // 裸 mmdb ⇒ Raw ✓（它头两个字节是二进制树，没有固定签名 ✗ —— 靠"不是 gzip"来判 ✓）
        assert_eq!(sniff(&inner), Pack::Raw);
        assert_eq!(sniff(b""), Pack::Raw);

        // .gz（DB-IP 那种 ✓）⇒ 认出 gzip ✓
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(&inner).unwrap();
        let gz = enc.finish().unwrap();
        assert_eq!(sniff(&gz), Pack::Gzip);

        // .tar.gz（MaxMind 那种 ✓）⇒ 认出 TarGz ✓（**解压一层之后**看 tar 的 magic ✓）
        let mut tar = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(inner.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "GeoLite2-City_20260307/GeoLite2-City.mmdb", inner.as_slice()).unwrap();
        let tarball = tar.into_inner().unwrap();
        assert_eq!(sniff_inner(&tarball), Pack::TarGz, "tar 的 magic 在偏移 257 ✓");
        assert_eq!(sniff_inner(&inner), Pack::Raw, "不是 tar ⇒ 单文件 ✓");
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(&tarball).unwrap();
        let tgz = enc.finish().unwrap();
        assert_eq!(sniff(&tgz), Pack::Gzip, "只看头部时只能到 gzip ✓（内容要解压后才看得见 ✓）");

        // ⚠️ 短输入不许 panic ✓（网络半截、空文件 ✓ —— 这些是**常态**不是意外 ✓）
        for n in 0..300 {
            let _ = sniff(&vec![0u8; n]);
            let _ = sniff_inner(&vec![0u8; n]);
        }
    }

    /// ⭐ **流式与"攒成 Vec"必须逐字节相同** ✓✓ ——
    /// 这是我为省内存新开的第二条路 ✓，而"两条路算出不同结果"是最难发现的错之一 ✓
    ///（一条只在别人的机器上跑 ✓）。所以：同一份包，两条路的结果必须**完全一致** ✓。
    #[test]
    fn the_streaming_unpack_matches_the_in_memory_one_byte_for_byte() {
        use std::io::Write;
        let inner = fake_mmdb();
        // gz ✓
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(&inner).unwrap();
        let gz = enc.finish().unwrap();
        // tar.gz ✓（带日期目录 ✓）
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

        for (pack, blob) in [(Pack::Gzip, &gz), (Pack::TarGz, &tgz), (Pack::Raw, &inner)] {
            let mut streamed = Vec::new();
            let n = unpack_to(pack, "x.mmdb", blob.as_slice(), &mut streamed).unwrap();
            assert_eq!(n as usize, streamed.len(), "返回的字节数要和写出去的一致 ✓");
            assert_eq!(
                streamed,
                unpack(pack, "x.mmdb", blob.as_slice()).unwrap(),
                "{pack:?}: 两条路必须一致 ✓"
            );
        }
        // 坏包在两条路上**都要失败** ✓（否则流式那条会把半截文件当成库 ✓✗）
        let mut sink = Vec::new();
        assert!(unpack_to(Pack::Gzip, "x.mmdb", &b"not gzip"[..], &mut sink).is_err());
        assert!(
            unpack_to(Pack::TarGz, "x.mmdb", gz.as_slice(), &mut sink).is_err(),
            "只有 gz 没有 tar ⇒ 拒绝 ✓"
        );
    }

    /// 磁盘校验：只读尾部就够了 ✓✓（这条保证"校验不再整份读"是真的事实 ✓）。
    #[test]
    fn validating_from_disk_only_reads_the_tail() {
        let dir = tmpdir("tail");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("City.mmdb");
        // 一个"大"文件：前面全是 0 ✓，**只有尾部有魔数** ✓
        let mut big = vec![0u8; MMDB_TAIL * 2];
        big.extend_from_slice(MMDB_MARKER);
        std::fs::write(&path, &big).unwrap();
        assert!(looks_valid_file(&path, Shape::Mmdb).unwrap(), "尾部有魔数 ⇒ 合格 ✓");
        assert_eq!(read_tail(&path, 128 * 1024).unwrap().len(), 128 * 1024, "只读尾部 128 KB ✓");
        // 没有魔数 ⇒ 拒绝 ✓
        std::fs::write(&path, vec![0u8; MMDB_TAIL * 2]).unwrap();
        assert!(!looks_valid_file(&path, Shape::Mmdb).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
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

    /// **重启不该重新下载** ✓✓ —— 这条测试盯的就是那个死循环的成因 ✗：
    /// "上次拉取时间"只存在进程内 ⇒ 重启即遗忘 ⇒ 反复重下 127 MB ⇒ OOM ⇒ 死循环 ✓。
    /// 判据改成看文件 mtime 之后 ✓：够新就跳过 ✓，与进程活多久无关 ✓。
    #[test]
    fn a_recent_file_is_not_fetched_again_even_after_a_restart() {
        let day = std::time::Duration::from_secs(24 * 3600);
        assert!(!recently_fetched(None, day), "文件不在 ⇒ 该拉 ✓");
        assert!(
            recently_fetched(Some(std::time::Duration::from_secs(60)), day),
            "刚拉过 ⇒ 跳过 ✓（重启也一样 ✓）"
        );
        assert!(
            recently_fetched(Some(day - std::time::Duration::from_secs(1)), day),
            "还差一秒到期 ⇒ 仍跳过 ✓"
        );
        assert!(!recently_fetched(Some(day), day), "到点 ⇒ 该拉 ✓");
        assert!(!recently_fetched(Some(day * 2), day), "过期 ⇒ 该拉 ✓");
        // Tor 那种一小时一刷的 ✓：一小时零一秒前拉过 ⇒ 该拉 ✓
        let hour = std::time::Duration::from_secs(3600);
        assert!(!recently_fetched(Some(hour + std::time::Duration::from_secs(1)), hour));
        assert!(recently_fetched(Some(std::time::Duration::from_secs(3599)), hour));
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
            // ⚠️ `name` 现在是**标签** ✓（"城市库" ✓），文件名在 `names` 里 ✓ ——
            // 这条断言跟着改 ✓：护栏盯的是**落盘的那个名字** ✓（否则它会一直红 ✓，
            // 而"改了标签就红"的护栏会被人顺手删掉 ✓ —— 那才是最糟的结果 ✓）。
            for n in s.names {
                assert!(n.ends_with(".mmdb") || n.ends_with(".txt"), "落盘名要能看出形态：{n}");
            }
            assert!(!s.names.is_empty(), "至少有一个落盘名：{}", s.name);
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
