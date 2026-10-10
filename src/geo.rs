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

// ⚠️ **整块还没接线** ✗：下载、来源清单与 `App.data_dir` 是**下一块**的事 ✓，
// 所以现在只有测试在调用这些函数 ⇒ clippy 会报一串 dead_code ✗。
// 这是**欠账**，不是设计 ✓ —— 下一块接上后，这行 allow 必须删掉 ✓
//（`report.rs` 顶部那行就是这么加的、也是这么删的 ✓✓）。
#![allow(dead_code)]

use std::path::Path;

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
