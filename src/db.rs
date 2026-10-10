//! SQLite storage. A single writer connection behind a mutex: at a handful of
//! nodes reporting every few seconds, every statement here is sub-millisecond.
// ponytail: single global connection; move to a read pool if the dashboard ever
// blocks behind ingest.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use anyhow::{Context, Result};
use chrono::{Datelike, Local, NaiveDate, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use tracing::info;

pub struct Db {
    conn: Mutex<Connection>,
    /// Why the last probe of a node's task produced no sample, and when that was said,
    /// keyed by `(node, task)`.
    ///
    /// In memory rather than in a column: this is the **current** explanation, not
    /// history -- a row would exist only to be overwritten, and the pattern is one
    /// write per probe round. It is what keeps a probe that cannot run (no permission,
    /// no route) from reaching the panel as an unexplained 100% loss.
    ping_errors: Mutex<std::collections::HashMap<(i64, i64), (String, i64)>>,
}

/// How long a reason stays worth showing. A probe that failed once and has been quiet
/// since is not news; one that is failing every round keeps its reason fresh.
const PING_ERROR_TTL: i64 = 600;

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;
-- 8 MiB of page cache. The whole working set of a few hundred nodes fits, so
-- the read paths stop going back to the filesystem.
PRAGMA cache_size = -8192;
-- Without these the WAL grows to whatever the busiest minute needed and never
-- gives the space back: a hub is a long-running process on a small VPS.
PRAGMA wal_autocheckpoint = 256;
PRAGMA journal_size_limit = 1048576;

CREATE TABLE IF NOT EXISTS setting (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS node (
  id            INTEGER PRIMARY KEY,
  name          TEXT    NOT NULL,
  -- The agent's credential, in the clear: the panel shows a node's install
  -- command whenever it is asked, so it has to be able to read it back.
  token         TEXT    NOT NULL UNIQUE,
  sort          INTEGER NOT NULL DEFAULT 0,
  public        INTEGER NOT NULL DEFAULT 1,
  price         REAL    NOT NULL DEFAULT 0,
  currency      TEXT    NOT NULL DEFAULT 'USD',
  billing_cycle TEXT    NOT NULL DEFAULT 'monthly',
  expires_at    TEXT,
  remark        TEXT    NOT NULL DEFAULT '',
  traffic_limit INTEGER NOT NULL DEFAULT 0,
  traffic_mode  TEXT    NOT NULL DEFAULT 'sum',
  traffic_reset_day INTEGER NOT NULL DEFAULT 1,
  hostname TEXT NOT NULL DEFAULT '', os TEXT NOT NULL DEFAULT '',
  kernel   TEXT NOT NULL DEFAULT '', arch TEXT NOT NULL DEFAULT '',
  virt     TEXT NOT NULL DEFAULT '', cpu_name TEXT NOT NULL DEFAULT '',
  cpu_cores INTEGER NOT NULL DEFAULT 0, mem_total INTEGER NOT NULL DEFAULT 0,
  swap_total INTEGER NOT NULL DEFAULT 0, disk_total INTEGER NOT NULL DEFAULT 0,
  agent_version TEXT NOT NULL DEFAULT '', ip TEXT NOT NULL DEFAULT '',
  ipv4 TEXT NOT NULL DEFAULT '', ipv6 TEXT NOT NULL DEFAULT '',
  -- ISO 3166-1 alpha-2, looked up from `country_ip` once per address. Empty
  -- until the lookup answers, and empty is what a node whose country nobody
  -- could tell stays: the public page just leaves the badge off.
  country TEXT NOT NULL DEFAULT '',
  -- The address `country` belongs to: a public interface address the agent
  -- reported, else `ip`. Empty when neither is public.
  country_ip TEXT NOT NULL DEFAULT '',
  -- The last answered pair `country_ip` / `country` before the current one;
  -- an address never answered does not displace it. A hello taken before
  -- every interface is up picks the other family, and the next one returns;
  -- the address returned to takes its answer back from here instead of
  -- waiting out the hourly lookup limit the detour spent.
  -- One pair suffices: a machine's sources are its v4, or the exit in front of
  -- it, and its v6.
  country_prev_ip TEXT NOT NULL DEFAULT '',
  country_prev TEXT NOT NULL DEFAULT '',
  -- Set in the panel. When not empty it is the country shown, in place of the
  -- looked-up one, which goes on updating underneath.
  country_pin TEXT NOT NULL DEFAULT '',
  -- Set in the panel, each replacing the address shown for its family. Empty
  -- means automatic. Panel only, like the reported addresses.
  ipv4_pin TEXT NOT NULL DEFAULT '', ipv6_pin TEXT NOT NULL DEFAULT '',
  -- Survives the disconnection it describes, unlike the in-memory live entry:
  -- an offline node's page is exactly where "since when" is worth reading.
  last_seen INTEGER NOT NULL DEFAULT 0,
  -- Opt-in, as the operator decides which machines are worth an alert.
  notify INTEGER NOT NULL DEFAULT 0,
  -- `last_seen` as of the offline alert, zero while none is outstanding. Stored
  -- rather than held in memory so that a hub restart neither repeats the alert
  -- nor loses the recovery that pairs with it.
  down_since INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL,
  -- Free-form bucket the public page groups by ("建站", "入口集群", ...). A plain
  -- string rather than a table: the set is whatever the operator types, and the
  -- page's tabs are derived from the values in use. Quoted because GROUP is a
  -- keyword -- every reference to this column needs the quotes.
  "group" TEXT NOT NULL DEFAULT '',
  -- **只在管理后台可见**的备注。`remark` 是半公开的（管理员登录后看公开页也会显示），
  -- 这一列则**任何**公开响应里都不出现 —— 见 `api.rs` 里 `if full` 那段与那条隐藏列表测试。
  -- 放在最后：`ALTER TABLE ADD COLUMN` 只能追加，`SCHEMA` 的顺序要和迁移后的一致。
  private_remark TEXT NOT NULL DEFAULT '',
  -- agent 是否允许被 hub 远程升级（它在 hello 里如实上报；老 agent 不上报 → 0 ✓）。
  -- **只能由这台机器自己打开**：hub 不给写这个列的能力，升级必须由本机重跑安装命令。
  allow_remote_upgrade INTEGER NOT NULL DEFAULT 0,
  -- 节点质量（IP 结论 ✓）：由 hub 用**本地**库查出 ✓，不是 agent 上报的 ✗ ——
  -- agent 依旧零改动 ✓，也不把节点 IP 发给第三方 ✓（除非以后显式打开在线库 ✓）。
  -- 八个列**全部可空** ✓：查不到就是空 ✓ —— "未知"与"美国"是两件事 ✗（`''` 会被读成后者 ✓）。
  q_country     TEXT,
  q_city        TEXT,
  q_subdivision TEXT,
  q_latitude    REAL,
  q_longitude   REAL,
  q_time_zone   TEXT,
  q_asn         INTEGER,
  q_org         TEXT
);

-- Monotonic byte counters that survive both agent reboots and hub restarts.
CREATE TABLE IF NOT EXISTS traffic (
  node_id  INTEGER PRIMARY KEY REFERENCES node(id) ON DELETE CASCADE,
  boot_id  TEXT    NOT NULL DEFAULT '',
  last_rx  INTEGER NOT NULL DEFAULT 0,
  last_tx  INTEGER NOT NULL DEFAULT 0,
  total_rx INTEGER NOT NULL DEFAULT 0,
  total_tx INTEGER NOT NULL DEFAULT 0,
  month_rx INTEGER NOT NULL DEFAULT 0,
  month_tx INTEGER NOT NULL DEFAULT 0,
  month_start TEXT NOT NULL DEFAULT '',
  day_rx INTEGER NOT NULL DEFAULT 0,
  day_tx INTEGER NOT NULL DEFAULT 0,
  day_start TEXT NOT NULL DEFAULT ''
);

  -- ⚠️ **这张表不参与保留期清理** ✗（housekeeping 只动 `metric` / `ping_*` ✓）——
  -- 半年报与年报依赖它跨年留存 ✓（年报要 13 个月以上的行 ✓）：哪天有人顺手把它也"按保留期清一下"，
  -- 年报就会被**静悄悄地掏空** ✓（总量偏小、且只有那行「本期覆盖 N 天」能提示 ✓）。
  -- 体量可忽略：100 台 × 365 天 ≈ 3.6 万行/年 ✓。
  --
  -- 按天的流量快照：`traffic` 只存"今天 / 本月 / 累计"三个**当前**窗口，
  -- 而日报要的是"昨天"、周报要 7 天、季报要 3 个月 —— 那些边界一过就取不到了。
  -- 每天在**日切那一刻**写一行（与 `day_start` 同一次判断，不另起一套）。
  -- 于是周/月/季报全都变成**这张表的区间求和**，只有一套逻辑。
  CREATE TABLE IF NOT EXISTS traffic_day (
    node_id INTEGER NOT NULL REFERENCES node(id) ON DELETE CASCADE,
    date    TEXT    NOT NULL,
    rx      INTEGER NOT NULL DEFAULT 0,
    tx      INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (node_id, date)
  );

CREATE TABLE IF NOT EXISTS metric (
  node_id INTEGER NOT NULL REFERENCES node(id) ON DELETE CASCADE,
  ts      INTEGER NOT NULL,
  cpu REAL NOT NULL, load1 REAL,
  mem_used INTEGER NOT NULL, swap_used INTEGER NOT NULL, disk_used INTEGER NOT NULL,
  net_rx INTEGER NOT NULL, net_tx INTEGER NOT NULL,
  tcp INTEGER NOT NULL, udp INTEGER NOT NULL, procs INTEGER NOT NULL,
  -- Last, where `migrate_to_9` has to put them: `ALTER TABLE ADD COLUMN` appends,
  -- so a column declared anywhere else would leave an upgraded database with a
  -- different column order from a fresh one. The highest rate the agent measured
  -- across one report interval within this minute, never below the mean beside
  -- it: the mean is what integrates to the traffic totals, so a 15-second burst
  -- in an otherwise idle minute stores as its average and loses the shape that
  -- made it worth looking at. 0 on rows written before it existed, which the
  -- history query reads as "no peak" rather than rewriting history.
  net_rx_max INTEGER NOT NULL DEFAULT 0, net_tx_max INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (node_id, ts)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS ping_task (
  id       INTEGER PRIMARY KEY,
  name     TEXT    NOT NULL,
  target   TEXT    NOT NULL,
  interval INTEGER NOT NULL DEFAULT 60,
  -- Last, where `migrate_to_10` has to put it: a column declared anywhere else
  -- would leave an upgraded database with a different column order from a fresh
  -- one. The panel's order; every probe ties at 0 on an upgraded database, so
  -- `ORDER BY sort, id` keeps the order it listed them in.
  sort     INTEGER NOT NULL DEFAULT 0,
  kind     TEXT    NOT NULL DEFAULT 'tcp'
);
-- `kind` is `tcp` (a handshake, what every task was before the column existed) or
-- `icmp` (an echo request), and it is declared last for the same reason `sort` is:
-- see `migrate_to_10`. The note lives out here rather than beside the column because
-- **a `--` comment inside a CREATE TABLE breaks `ALTER TABLE ... DROP COLUMN`** --
-- SQLite rewrites the statement and the comment swallows what follows, which is an
-- "incomplete input" error. Tests that reduce a database to an older version drop
-- columns, so a comment in there is a trap for them, not a note for a reader.

CREATE TABLE IF NOT EXISTS ping_node (
  task_id INTEGER NOT NULL REFERENCES ping_task(id) ON DELETE CASCADE,
  node_id INTEGER NOT NULL REFERENCES node(id) ON DELETE CASCADE,
  PRIMARY KEY (task_id, node_id)
);

-- Key order follows the only query there is: one node, one time window,
-- every probe. With task_id ahead of ts SQLite can seek to the node and no
-- further, then scans every record it ever kept -- see the migration in open().
CREATE TABLE IF NOT EXISTS ping_record (
  node_id INTEGER NOT NULL, task_id INTEGER NOT NULL,
  ts INTEGER NOT NULL, latency INTEGER NOT NULL,
  PRIMARY KEY (node_id, ts, task_id)
) WITHOUT ROWID;

-- The two summary layers, for history older than `DETAIL_DAYS`. An hour is
-- written once it has ended and stayed quiet for a further hour (an agent may
-- report up to an hour late, and a probe's answer lands with the next frame),
-- and the minute rows it was built from are deleted only afterwards.
--
-- `minutes` is how many minute rows went into the average, so a bucket that
-- merges several hours weights each by its own count; a bucket built from a
-- half-reported hour must not count as much as a full one.
--
-- `swap_used` and `load1` are carried here, unlike the rest of the minute
-- table's columns, because this hub's charts draw them: a tier without them
-- would drop two series from every window wider than the detail window. `load1`
-- is nullable in `metric` too -- an agent reporting no load is not an agent
-- reporting zero -- and `AVG` over an hour of nulls stays null.
CREATE TABLE IF NOT EXISTS metric_hour (
  node_id INTEGER NOT NULL REFERENCES node(id) ON DELETE CASCADE,
  ts      INTEGER NOT NULL,
  minutes INTEGER NOT NULL,
  cpu REAL NOT NULL,
  mem_used INTEGER NOT NULL, swap_used INTEGER NOT NULL, disk_used INTEGER NOT NULL,
  net_rx INTEGER NOT NULL, net_tx INTEGER NOT NULL,
  load1 REAL,
  net_rx_max INTEGER NOT NULL, net_tx_max INTEGER NOT NULL,
  PRIMARY KEY (node_id, ts)
) WITHOUT ROWID;

-- `latency` is the median of the hour's answers, NULL when none arrived; `lo`
-- and `hi` bound them. `answered` weights that median when a chart point covers
-- several hours.
CREATE TABLE IF NOT EXISTS ping_hour (
  node_id INTEGER NOT NULL, task_id INTEGER NOT NULL, ts INTEGER NOT NULL,
  answered INTEGER NOT NULL, lost INTEGER NOT NULL,
  latency INTEGER, lo INTEGER, hi INTEGER,
  PRIMARY KEY (node_id, ts, task_id)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS session (
  token_hash TEXT    PRIMARY KEY,
  expires_at INTEGER NOT NULL,
  github_login TEXT  NOT NULL DEFAULT '',
  created_at INTEGER NOT NULL DEFAULT 0,
  ip         TEXT    NOT NULL DEFAULT '',
  user_agent TEXT    NOT NULL DEFAULT '',
  last_seen  INTEGER NOT NULL DEFAULT 0
);
"#;

/// Schema revision this build expects, stamped into `PRAGMA user_version`.
/// Increment it and add a `migrate_to_N` when the schema changes under a
/// database already in service. Every migration must be:
///
/// - Additive: a new column carries a default, and no column an earlier build
///   reads is renamed or dropped. install-hub.sh rolls a hub that fails to start
///   back to the previous binary, which then runs on the migrated file.
/// - Safe to run twice: an earlier build stamps its own, lower version into a
///   newer file, and the next upgrade runs the migration again.
///
/// A new column goes into `SCHEMA` as well, for fresh files, but an index on it
/// cannot: `open` runs `SCHEMA` before migrating, and on an older file the
/// column is not there yet.
const SCHEMA_VERSION: i64 = 17;

/// Adds a column older databases lack. A duplicate column indicates the
/// migration has already run; every other error must propagate.
fn add_column(conn: &Connection, table: &str, column: &str) -> Result<()> {
    match conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {column}"), []) {
        Ok(_) => Ok(()),
        Err(e) if e.to_string().contains("duplicate column name") => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// True when `table`'s stored DDL contains `needle`, which is how a migration
/// determines the shape of the database it inherited.
fn schema_mentions(conn: &Connection, table: &str, needle: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE name=?1 AND sql LIKE ?2",
        params![table, format!("%{needle}%")],
        |r| r.get::<_, i64>(0),
    )? > 0)
}

/// One table's column names. `table` is always a [`TABLES`] entry rather than
/// caller-supplied, which is why it can be formatted into the pragma.
fn columns_of(conn: &Connection, table: &str) -> Result<HashSet<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt.query_map([], |r| r.get::<_, String>(1))?;
    Ok(names.collect::<Result<_, _>>()?)
}

/// Everything accumulated before a version was recorded. Runs once, on a
/// database predating the stamp.
fn migrate_to_1(conn: &Connection) -> Result<()> {
    for column in [
        "day_rx INTEGER NOT NULL DEFAULT 0",
        "day_tx INTEGER NOT NULL DEFAULT 0",
        "day_start TEXT NOT NULL DEFAULT ''",
    ] {
        add_column(conn, "traffic", column)?;
    }
    for column in [
        "ipv4 TEXT NOT NULL DEFAULT ''",
        "ipv6 TEXT NOT NULL DEFAULT ''",
        "last_seen INTEGER NOT NULL DEFAULT 0",
    ] {
        add_column(conn, "node", column)?;
    }
    // The column held a sha256 of the token and now holds the token itself.
    // Databases predating the change retain digests no agent can present, so
    // those nodes require a new token issued from the panel.
    if schema_mentions(conn, "node", "token_hash")? {
        conn.execute("ALTER TABLE node RENAME COLUMN token_hash TO token", [])?;
        info!("renamed node.token_hash to node.token; existing nodes need a fresh token");
    }
    // Reordering a key requires rebuilding the table; CREATE TABLE IF NOT EXISTS
    // leaves an existing one untouched. The old order placed task_id between the
    // node and the timestamp, so the chart query scanned a node's entire history
    // to answer for one hour of it: 42 ms against 0.8 ms at a month of
    // retention.
    if schema_mentions(conn, "ping_record", "(node_id, task_id, ts)")? {
        conn.execute_batch(
            "CREATE TABLE ping_record_rekeyed (
               node_id INTEGER NOT NULL, task_id INTEGER NOT NULL,
               ts INTEGER NOT NULL, latency INTEGER NOT NULL,
               PRIMARY KEY (node_id, ts, task_id)
             ) WITHOUT ROWID;
             INSERT INTO ping_record_rekeyed SELECT * FROM ping_record;
             DROP TABLE ping_record;
             ALTER TABLE ping_record_rekeyed RENAME TO ping_record;",
        )?;
        info!("rebuilt ping_record on a key the latency chart can seek");
    }
    Ok(())
}

/// `metric.load1` was written on every history row and read by nothing: the card
/// draws the live `load` array from the report, and no chart draws load from
/// history. Dropping it recovers 21% of what the five unread columns cost, and
/// it is the only one the hub can lose without also losing a figure the UI
/// displays.
///
/// The column is `NOT NULL` with no default, so this migration is mandatory:
/// without it every metric insert this build makes violates the constraint.
fn migrate_to_2(conn: &Connection) -> Result<()> {
    if schema_mentions(conn, "metric", "load1")? {
        conn.execute("ALTER TABLE metric DROP COLUMN load1", [])?;
        info!("dropped metric.load1; nothing read it");
    }
    Ok(())
}

fn migrate_to_3(conn: &Connection) -> Result<()> {
    add_column(conn, "node", "country TEXT NOT NULL DEFAULT ''")
}

fn migrate_to_4(conn: &Connection) -> Result<()> {
    add_column(conn, "node", "notify INTEGER NOT NULL DEFAULT 0")?;
    add_column(conn, "node", "down_since INTEGER NOT NULL DEFAULT 0")
}

fn migrate_to_5(conn: &Connection) -> Result<()> {
    add_column(conn, "node", r#""group" TEXT NOT NULL DEFAULT ''"#)
}

/// Which login a session came from. Empty means the emergency password, which has no
/// name to record -- and which is why the panel can say "应急密码" without a second
/// column for the method.
fn migrate_to_6(conn: &Connection) -> Result<()> {
    add_column(conn, "session", "github_login TEXT NOT NULL DEFAULT ''")
}

/// What a login leaves behind: when it happened, from where, on what, and when it was
/// last used.
///
/// `created_at` is stored rather than derived. It used to be computed as
/// `expires_at - SESSION_DAYS`, which meant that changing the session lifetime silently
/// rewrote the displayed sign-in time of every existing row.
///
/// The three text columns default to empty for rows that predate this, which is exactly
/// how the panel reads "unknown": they are sessions from before the hub recorded it, and
/// they expire within `SESSION_DAYS` either way.
fn migrate_to_7(conn: &Connection) -> Result<()> {
    add_column(conn, "session", "created_at INTEGER NOT NULL DEFAULT 0")?;
    add_column(conn, "session", "ip TEXT NOT NULL DEFAULT ''")?;
    add_column(conn, "session", "user_agent TEXT NOT NULL DEFAULT ''")?;
    add_column(conn, "session", "last_seen INTEGER NOT NULL DEFAULT 0")
}

/// Puts `metric.load1`, the 1-minute load average, back. `migrate_to_2` dropped
/// it because nothing read it; the CPU panel now draws the load curve beside the
/// CPU curve, so history has to carry it again.
///
/// Nullable, unlike the column `migrate_to_2` removed: only a minute whose
/// reports carried a load has one to average, and a row that stored zero for a
/// minute with no sample would draw an idle machine. Rows written before this
/// migration keep NULL, which the history query returns as JSON null.
fn migrate_to_8(conn: &Connection) -> Result<()> {
    add_column(conn, "metric", "load1 REAL")
}

/// The per-minute peak of each transfer rate, beside the minute's mean.
///
/// Rows written before this hold 0. `Db::metrics` reads that as "no peak" and
/// falls back to the row's own mean, so history taken before the column existed
/// still draws -- without rewriting every row of it here.
fn migrate_to_9(conn: &Connection) -> Result<()> {
    add_column(conn, "metric", "net_rx_max INTEGER NOT NULL DEFAULT 0")?;
    add_column(conn, "metric", "net_tx_max INTEGER NOT NULL DEFAULT 0")
}

/// The probe order the panel arranges. Every existing probe ties at 0, so
/// `ORDER BY sort, id` keeps the id order an upgraded database listed them in.
fn migrate_to_10(conn: &Connection) -> Result<()> {
    add_column(conn, "ping_task", "sort INTEGER NOT NULL DEFAULT 0")
}

/// Where a node's country comes from, and what the panel may put in its place.
///
/// `country_ip` records the address the stored `country` was looked up from,
/// which is no longer the connection's (`agent_ws::country_source` picks a
/// public interface address first). Every country stored until now was looked
/// up from `ip`, so backfilling that keeps the badge of a node whose lookup
/// address is still `ip`, and has a node whose public interface address now
/// takes precedence asked about again at its next hello. The backfill reaches
/// only rows that have a country: an empty one is owed a lookup either way, and
/// leaving `country_ip` empty for it keeps `country_owed` from answering for an
/// address nobody asked about.
///
/// `country_prev_ip` / `country_prev` are the pair the current one displaced;
/// see `save_facts`. The three columns set by hand start empty: automatic.
///
/// All six in one step because they are one change in behaviour -- look the
/// country up by the machine's own address, let the panel override it, and give
/// an address its answer back when a node returns to it -- and scattering them
/// across three migrations would leave the same rule in three places.
/// The summary tables, for databases already in service. The statements are the
/// same as the ones in `SCHEMA`; `an_upgraded_release_matches_a_fresh_database`
/// compares the columns of the two paths, so changing one side alone fails there
/// rather than in production.
fn migrate_to_12(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS metric_hour (
  node_id INTEGER NOT NULL REFERENCES node(id) ON DELETE CASCADE,
  ts      INTEGER NOT NULL,
  minutes INTEGER NOT NULL,
  cpu REAL NOT NULL,
  mem_used INTEGER NOT NULL, swap_used INTEGER NOT NULL, disk_used INTEGER NOT NULL,
  net_rx INTEGER NOT NULL, net_tx INTEGER NOT NULL,
  load1 REAL,
  net_rx_max INTEGER NOT NULL, net_tx_max INTEGER NOT NULL,
  PRIMARY KEY (node_id, ts)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS ping_hour (
  node_id INTEGER NOT NULL, task_id INTEGER NOT NULL, ts INTEGER NOT NULL,
  answered INTEGER NOT NULL, lost INTEGER NOT NULL,
  latency INTEGER, lo INTEGER, hi INTEGER,
  PRIMARY KEY (node_id, ts, task_id)
) WITHOUT ROWID;",
    )?;
    Ok(())
}

fn migrate_to_11(conn: &Connection) -> Result<()> {
    add_column(conn, "node", "country_ip TEXT NOT NULL DEFAULT ''")?;
    add_column(conn, "node", "country_prev_ip TEXT NOT NULL DEFAULT ''")?;
    add_column(conn, "node", "country_prev TEXT NOT NULL DEFAULT ''")?;
    add_column(conn, "node", "country_pin TEXT NOT NULL DEFAULT ''")?;
    add_column(conn, "node", "ipv4_pin TEXT NOT NULL DEFAULT ''")?;
    add_column(conn, "node", "ipv6_pin TEXT NOT NULL DEFAULT ''")?;
    conn.execute("UPDATE node SET country_ip = ip WHERE country != ''", [])?;
    Ok(())
}

/// Every probe was a TCP handshake before this column existed; the default says so, so
/// an upgraded hub behaves exactly as it did until someone asks for an echo.
///
/// Twice-safe like every other step, which for `ADD COLUMN` means asking first: the
/// migration tests build a file with the current schema and then reduce it, so a step
/// that assumed the column was absent would refuse to run there -- and a step that
/// merely swallowed the error would never be shown to do anything at all.
fn migrate_to_15(conn: &Connection) -> Result<()> {
    add_column(conn, "node", "allow_remote_upgrade INTEGER NOT NULL DEFAULT 0")
}

fn migrate_to_14(conn: &Connection) -> Result<()> {
    add_column(conn, "node", "private_remark TEXT NOT NULL DEFAULT ''")
}

fn migrate_to_13(conn: &Connection) -> Result<()> {
    let has_kind: i64 =
        conn.query_row("SELECT COUNT(*) FROM pragma_table_info('ping_task') WHERE name = 'kind'", [], |r| {
            r.get(0)
        })?;
    if has_kind == 0 {
        conn.execute_batch("ALTER TABLE ping_task ADD COLUMN kind TEXT NOT NULL DEFAULT 'tcp';")?;
    }
    Ok(())
}

/// Brings a database already in service up to `SCHEMA_VERSION` and stamps it.
/// `from` is its current version, so a fresh file passes `SCHEMA_VERSION` and
/// receives only the stamp.
///
/// One transaction covers every step and the stamp. SQLite rolls back schema
/// changes and `user_version` alike, so a failure part-way -- a full disk, a
/// killed process -- leaves the file at the version it started from rather than
/// between two. The steps below therefore open no transaction of their own.
///
/// Restoring a backup also arrives here: the copy carries its own version and
/// requires the same migrations a restart would have run.
fn migrate(conn: &Connection, from: i64) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    if from < 1 {
        migrate_to_1(&tx)?;
    }
    if from < 2 {
        migrate_to_2(&tx)?;
    }
    if from < 3 {
        migrate_to_3(&tx)?;
    }
    if from < 4 {
        migrate_to_4(&tx)?;
    }
    if from < 5 {
        migrate_to_5(&tx)?;
    }
    if from < 6 {
        migrate_to_6(&tx)?;
    }
    if from < 7 {
        migrate_to_7(&tx)?;
    }
    if from < 8 {
        migrate_to_8(&tx)?;
    }
    if from < 9 {
        migrate_to_9(&tx)?;
    }
    if from < 10 {
        migrate_to_10(&tx)?;
    }
    if from < 11 {
        migrate_to_11(&tx)?;
    }
    if from < 12 {
        migrate_to_12(&tx)?;
    }
    if from < 13 {
        migrate_to_13(&tx)?;
    }
    // 各自一条守卫：migrate_to_14 一度被塞进上面那个 from < 13 块里 ——
    // 于是 from == 13 的库（每一个升级上来的生产库）跳过它，却仍被末尾那行盖章到 14，
    // 从此永久缺列。全新库的 SCHEMA 已带该列，add_column 会当作「重复列」忽略，
    // 所以别的测试全绿也发现不了 —— 见 migrating_from_13_adds_the_private_remark_column。
    if from < 14 {
        migrate_to_14(&tx)?;
    }
    if from < 15 {
        migrate_to_15(&tx)?;
    }
    // 16：按天的流量快照表。**从上一版升上来的库**必须在这里补建 ✓ ——
    // 全新库由上面的 SCHEMA 带上 ✓，所以只有"升级路径"的测试碰得到这一块 ✓
    // （1.9.12 的教训：步骤必须在自己的 `if from < N` 里，且盖章在块外 ✓）。
    if from < 16 {
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS traffic_day (
               node_id INTEGER NOT NULL REFERENCES node(id) ON DELETE CASCADE,
               date    TEXT    NOT NULL,
               rx      INTEGER NOT NULL DEFAULT 0,
               tx      INTEGER NOT NULL DEFAULT 0,
               PRIMARY KEY (node_id, date)
             );",
        )?;
    }

    // 17：节点质量（IP 结论）. 八个可空列 ✓ —— 查不到就是空 ✓。
    // 全新库由上面的 SCHEMA 带上 ✓ ⇒ **只有升级路径的测试碰得到这一块** ✓（1.9.12 的教训 ✓）。
    if from < 17 {
        for (col, ty) in [
            ("q_country", "TEXT"),
            ("q_city", "TEXT"),
            ("q_subdivision", "TEXT"),
            ("q_latitude", "REAL"),
            ("q_longitude", "REAL"),
            ("q_time_zone", "TEXT"),
            ("q_asn", "INTEGER"),
            ("q_org", "TEXT"),
        ] {
            // `add_column` 是 3 个参数 ✓ —— 列名与类型在**同一个字符串**里 ✓（见它自己的用法 ✓）。
            add_column(&tx, "node", &format!("{col} {ty}"))?;
        }
    }

    tx.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
    tx.commit()?;
    Ok(())
}

/// Every table a backup must carry before this build will restore it.
const TABLES: [&str; 8] =
    ["setting", "node", "traffic", "metric", "ping_task", "ping_node", "ping_record", "session"];

/// The summary layers: history older than the detail window is kept here, one row
/// per hour rather than per minute. Deliberately not in `TABLES` -- that list is
/// what a backup must already contain, and one taken before these existed does
/// not, so listing them would refuse every older backup.
///
/// Test-visible only, for now: the rollup and the retention pass are what use it
/// in production.
/// The two summary tables. They are **not** in [`TABLES`]: a backup taken before
/// they existed restores, and `check_backup` only requires what a hub of any
/// version would have. `stats` reports their sizes separately.
const HOUR_TABLES: [&str; 2] = ["metric_hour", "ping_hour"];

/// One node's stored configuration and last known facts.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Node {
    #[serde(default)]
    pub id: i64,
    pub name: String,
    #[serde(default = "yes")]
    pub public: bool,
    #[serde(default)]
    pub sort: i64,
    /// Which bucket the public page files this node under. Empty means ungrouped;
    /// the page then shows the node under every tab.
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub price: f64,
    #[serde(default = "usd")]
    pub currency: String,
    #[serde(default = "monthly")]
    pub billing_cycle: String,
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub remark: String,
    /// 只在管理后台可见；**任何**公开响应里都不出现。
    #[serde(default)]
    pub private_remark: String,
    /// agent 是否允许被远程升级。**只能由那台机器自己打开**，hub 只读不写。
    #[serde(default)]
    pub allow_remote_upgrade: bool,
    /// Monthly allowance in bytes; 0 means unmetered.
    #[serde(default)]
    pub traffic_limit: i64,
    /// How the allowance is counted: sum, max, up or down.
    #[serde(default = "sum")]
    pub traffic_mode: String,
    #[serde(default = "one")]
    pub traffic_reset_day: u32,
    #[serde(default)]
    pub hostname: String,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub kernel: String,
    #[serde(default)]
    pub arch: String,
    #[serde(default)]
    pub virt: String,
    #[serde(default)]
    pub cpu_name: String,
    #[serde(default)]
    pub cpu_cores: i64,
    #[serde(default)]
    pub mem_total: i64,
    #[serde(default)]
    pub swap_total: i64,
    #[serde(default)]
    pub disk_total: i64,
    #[serde(default)]
    pub agent_version: String,
    #[serde(default)]
    pub ip: String,
    /// Reported by the agent from its own interfaces, unlike `ip`, which is
    /// merely the address the agent's connection originated from.
    #[serde(default)]
    pub ipv4: String,
    #[serde(default)]
    pub ipv6: String,
    /// ISO 3166-1 alpha-2, uppercase, or empty when unknown; see
    /// `agent_ws::country_source` for the address it is looked up from. Public:
    /// it appears on the status page beside the node's name.
    #[serde(default)]
    pub country: String,
    /// Set in the panel: two uppercase letters, or empty for the looked-up
    /// `country`. What the status page shows is this when present.
    #[serde(default)]
    pub country_pin: String,
    /// Set in the panel, in canonical form, for what neither agent nor hub can
    /// know: the home line behind a transparent proxy, or which of several public
    /// addresses to show. Each replaces the address shown for its family; empty
    /// is automatic. Panel only, like `ip`.
    #[serde(default)]
    pub ipv4_pin: String,
    #[serde(default)]
    pub ipv6_pin: String,
    /// Unix seconds of the node's last report, written once a minute alongside
    /// the metric row. Zero for a node that has never reported.
    #[serde(default)]
    pub last_seen: i64,
    /// Whether going offline and coming back are announced. See `notify`.
    #[serde(default)]
    pub notify: bool,
    #[serde(default)]
    pub down_since: i64,
    /// When the node was added to the hub, in seconds. This is the start of the
    /// node's own history: a node cannot have been reporting before it existed,
    /// so uptime divides by the part of the window since this moment rather than
    /// by the whole window. Without the clamp a node added yesterday reads as
    /// three percent available over thirty days.
    #[serde(default)]
    pub created_at: i64,
    /// What the agent authenticates with. Readable so the panel can display an
    /// install command on demand; it never leaves the admin view.
    #[serde(default)]
    pub token: String,
}

fn yes() -> bool {
    true
}

/// Omitted settings stay unchanged. An explicit null clears the expiry date.
#[derive(Deserialize, Default)]
pub struct NodePatch {
    pub name: Option<String>,
    pub sort: Option<i64>,
    pub public: Option<bool>,
    pub price: Option<f64>,
    pub currency: Option<String>,
    pub billing_cycle: Option<String>,
    #[serde(default, deserialize_with = "expiry_patch")]
    pub expires_at: Option<Option<String>>,
    pub remark: Option<String>,
    pub private_remark: Option<String>,
    pub traffic_limit: Option<i64>,
    pub traffic_mode: Option<String>,
    pub traffic_reset_day: Option<u32>,
    pub notify: Option<bool>,
    pub group: Option<String>,
    pub country_pin: Option<String>,
    pub ipv4_pin: Option<String>,
    pub ipv6_pin: Option<String>,
}

fn expiry_patch<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(d).map(Some)
}

#[derive(Deserialize, Default)]
pub struct TrafficPatch {
    pub total_rx: Option<i64>,
    pub total_tx: Option<i64>,
    pub month_rx: Option<i64>,
    pub month_tx: Option<i64>,
}
fn usd() -> String {
    "USD".into()
}
fn monthly() -> String {
    "monthly".into()
}
fn sum() -> String {
    "sum".into()
}
fn one() -> u32 {
    1
}

#[derive(Serialize, Debug, Clone, Default)]
pub struct Traffic {
    pub total_rx: i64,
    pub total_tx: i64,
    pub month_rx: i64,
    pub month_tx: i64,
    pub month_start: String,
    pub day_rx: i64,
    pub day_tx: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PingTask {
    #[serde(default)]
    pub id: i64,
    pub name: String,
    pub target: String,
    #[serde(default)]
    pub interval: i64,
    #[serde(default)]
    pub nodes: Vec<i64>,
    /// `"icmp"` for an echo request. Absent means `"tcp"`, which is what every task
    /// was before this field existed -- so an old hub, an old backup and an old panel
    /// all keep working, and a task that says nothing is a handshake.
    ///
    /// `None` rather than a defaulted `String` on purpose: the column does not exist
    /// in the schema yet, so the read path cannot supply it, and every construction
    /// site would otherwise have to name a value it does not have an opinion about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

impl PingTask {
    /// What this task probes: `"tcp"` unless it says otherwise. Empty is treated as
    /// absent, because the panel sends an empty string for a field it does not fill.
    pub fn probe_kind(&self) -> &str {
        match self.kind.as_deref() {
            Some(kind) if !kind.is_empty() => kind,
            _ => "tcp",
        }
    }
}

/// Points SQLite's temporary files -- the copy `VACUUM` rebuilds the database
/// into, and any sort too large for memory -- at the directory holding the
/// database.
///
/// SQLite otherwise tries $SQLITE_TMPDIR, $TMPDIR, /var/tmp, /usr/tmp, /tmp and
/// the working directory. The Docker image is built from scratch and has none of
/// them writable, so reclaiming space failed there with "unable to determine a
/// suitable directory for temporary files". Under the systemd unit /tmp is
/// private, and where the distribution mounts it as tmpfs -- Debian 13 does by
/// default -- the copy is charged to the unit's `MemoryMax`: vacuuming a 559 MiB
/// database at 256M without swap was OOM-killed. Beside the database, the copy
/// lands on the disk that has room for the database. SQLite unlinks each file
/// as it opens it, so none remain there.
///
/// Process-wide and not thread-safe, so it is called once, before any
/// connection is opened. A bare file name keeps SQLite's own search, which ends
/// at the working directory holding it.
pub fn temp_files_beside(database: &str) -> Result<()> {
    let Some(dir) = std::path::Path::new(database).parent().filter(|d| !d.as_os_str().is_empty()) else {
        return Ok(());
    };
    let dir = dir.to_string_lossy().replace('\'', "''");
    Connection::open_in_memory()?.execute_batch(&format!("PRAGMA temp_store_directory = '{dir}'"))?;
    Ok(())
}

/// Restricts the database to its owner.
///
/// It is the credential store: node tokens in the clear, the GitHub client
/// secret, the password hash. SQLite creates it under the umask, which at a
/// default 022 is world-readable, and the WAL and shm files hold the same rows.
///
/// Best effort: a filesystem without Unix modes still works.
fn restrict(path: &str) {
    for file in [path.to_owned(), format!("{path}-wal"), format!("{path}-shm")] {
        own_only(&file);
    }
}

/// One file, owner-only. Also applied to the backup copy `VACUUM INTO` writes,
/// which is the entire credential store in one portable file, created under the
/// umask like any other.
fn own_only(file: &str) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600));
    }
}

/// The `main` database's path as SQLite reports it, empty for `:memory:`.
/// Queried rather than cached so there is a single answer to which file is
/// open.
fn main_file(conn: &Connection) -> String {
    conn.query_row("PRAGMA database_list", [], |r| r.get(2)).unwrap_or_default()
}

fn bytes_of(file: &str) -> i64 {
    std::fs::metadata(file).map(|m| m.len() as i64).unwrap_or(0)
}

/// Bytes the database occupies. The WAL is included: committed rows remain there
/// until a checkpoint folds them into the main file, so the two together are what
/// an operator sees on disk.
fn on_disk(file: &str) -> i64 {
    bytes_of(file) + bytes_of(&format!("{file}-wal"))
}

/// The rows behind the latency chart: one node's probe results over a window,
/// bucketed and in time order. Everything the chart draws is folded out of them
/// in [`close_bucket`].
///
/// The key is `(node_id, ts, task_id)`, so this is a seek and the rows emerge
/// sorted without a sorter, which is what allows the fold to hold one bucket at
/// a time. Asking SQLite for the summary instead cost three sorts of the whole
/// window -- two window passes and a GROUP BY -- against this single scan: on a
/// week of four probes, 284 ms against 54 ms, all of it holding the connection
/// the agents write through.
///
/// A constant because the query plan is asserted against it in
/// `rekeying_ping_record_keeps_the_rows_and_lets_the_chart_query_seek`.
/// 一个节点的两条产出：按 rank 排好的行，以及窗口级 loss（不取整、只给丢过的探测）。
pub type NodeSeries = (Vec<serde_json::Value>, serde_json::Value);

/// 一次按任务取回的结果：节点 id → 该节点的序列。[`Db::ping_series_by_task`] 的返回类型。
pub type ByTaskSeries = std::collections::BTreeMap<i64, NodeSeries>;

/// 折叠过程中的累计：节点 → （行，探测 → (丢了多少, 一共多少)）。
type NodeTotals = std::collections::BTreeMap<i64, (Vec<serde_json::Value>, HashMap<i64, (i64, i64)>)>;

const PING_ROWS: &str = "SELECT ts/?3, task_id, latency FROM ping_record
     WHERE node_id=?1 AND ts>=?2
           AND task_id IN (SELECT task_id FROM ping_node WHERE node_id=?1)
     ORDER BY ts";

/// The setting naming the first hour not yet folded into the summary tables.
/// Persisted rather than derived from `MAX(ts)` in the summary: it is a cursor,
/// and it has to keep pointing at the same hour even when that hour folded
/// nothing (no node reported, or the hub was down).
const ROLLED: &str = "rolled_hour";

/// How long an hour stays unfoldable after it ends. An agent may report up to an
/// hour late and a probe's answer lands with the frame after it, so an hour is
/// only complete once the next one has passed.
const LATE: i64 = 3_600;

/// Days of minute rows kept. Older history lives in `metric_hour` / `ping_hour`
/// and is read from there; see `roll_up` and `prune`.
pub const DETAIL_DAYS: i64 = 7;

/// What a hub keeps when the operator has not chosen. A quarter is a couple of
/// hundred megabytes of hourly rows at a hundred nodes, against the gigabytes the
/// same span of minute rows would take, and it is a range the themes already offer.
pub const DEFAULT_RETENTION_DAYS: i64 = 90;

/// The widest window an operator may set. A year of hourly rows is 8760 per series,
/// which one request still answers; beyond it the answer grows without bound and
/// the themes' own ranges stop at a year. **Reducing this truncates**: an operator
/// who had set more loses the history past the new ceiling as the hourly prune
/// reaches it, so the hub says so at startup.
pub const MAX_RETENTION_DAYS: i64 = 365;

/// Whether SQLite refused because another connection holds the write lock, which
/// a catch-up treats as "wait and try the same hour again" rather than an error:
/// see `roll_up`.
fn is_locked(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<rusqlite::Error>(),
        Some(rusqlite::Error::SqliteFailure(err, _))
            if err.code == rusqlite::ErrorCode::DatabaseBusy || err.code == rusqlite::ErrorCode::DatabaseLocked
    )
}

/// Nodes per transaction and per yield while folding an hour's probes. One node
/// at a time cost more in commits and in sleeping than in work -- 549 ms per
/// hour against ~100 ms with blocks -- while the whole hour at once held the lock
/// the agents write through long enough to refuse a report. Ten keeps a writer's
/// wait to one block's statements, which is tens of milliseconds.
const FOLD_NODES: usize = 10;

/// Releases the lock the agents write through between two statements of a long
/// pass. Measured: a pass that held it throughout showed up as a node going
/// offline for 12 s.
fn let_waiters_in() {
    std::thread::sleep(std::time::Duration::from_millis(1));
}

/// The first hour not yet folded into the summary tables, `None` before the
/// first rollup while every row is still a minute row.
fn rolled(conn: &Connection) -> Result<Option<i64>> {
    let value: Option<String> =
        conn.query_row("SELECT value FROM setting WHERE key=?1", [ROLLED], |r| r.get(0)).optional()?;
    Ok(value.and_then(|v| v.parse().ok()))
}

/// The earliest `ts` across `tables`, sought one node at a time: every key in
/// these tables begins with `node_id`, so `MIN(ts)` over a whole table scans it
/// (15.9 s at 90 days of 100 nodes) where this seeks (1.8 ms).
fn oldest(conn: &Connection, tables: &[&str]) -> Result<Option<i64>> {
    let per_node: Vec<String> = tables
        .iter()
        .map(|t| format!("SELECT (SELECT MIN(ts) FROM {t} WHERE node_id=n.id) AS ts FROM node n"))
        .collect();
    let sql = format!("SELECT MIN(ts) FROM ({})", per_node.join(" UNION ALL "));
    Ok(conn.query_row(&sql, [], |r| r.get(0))?)
}

/// One probe's answer, or one hour's answers summarised.
// Consumed by the hourly read path, which a window wider than the detail
// window reaches; nothing constructs these until that lands.
#[allow(dead_code)]
struct Sample {
    /// How many stored results the median stands for. One for a minute row, the
    /// hour's count for an hourly one -- which is what makes a chart point that
    /// covers several hours weigh each of them by what it actually holds.
    answered: i64,
    lost: i64,
    median: Option<i64>,
    lo: Option<i64>,
    hi: Option<i64>,
}

// Consumed by the hourly read path, which a window wider than the detail
// window reaches; nothing constructs these until that lands.
#[allow(dead_code)]
impl Sample {
    /// One stored result. A timeout is stored as -1: counted as lost, and kept
    /// out of the median.
    fn result(latency: i64) -> Self {
        let answer = (latency >= 0).then_some(latency);
        Self {
            answered: i64::from(answer.is_some()),
            lost: i64::from(answer.is_none()),
            median: answer,
            lo: answer,
            hi: answer,
        }
    }
}

/// What one probe's rows in one bucket add up to.
#[derive(Default)]
// Consumed by the hourly read path, which a window wider than the detail
// window reaches; nothing constructs these until that lands.
#[allow(dead_code)]
struct Tally {
    /// Each sample's median and the number of answers it stands for.
    medians: Vec<(i64, i64)>,
    lost: i64,
    lo: Option<i64>,
    hi: Option<i64>,
}

// Consumed by the hourly read path, which a window wider than the detail
// window reaches; nothing constructs these until that lands.
#[allow(dead_code)]
impl Tally {
    fn add(&mut self, s: Sample) {
        if let Some(median) = s.median {
            self.medians.push((median, s.answered));
        }
        self.lost += s.lost;
        self.lo = self.lo.into_iter().chain(s.lo).min();
        self.hi = self.hi.into_iter().chain(s.hi).max();
    }

    fn answered(&self) -> i64 {
        self.medians.iter().map(|m| m.1).sum()
    }

    /// The median of the answers, each sample's median counted once per answer
    /// it stands for. Over single results that is the ordinary median, the
    /// middle two averaged; over hourly rows it is exact while a bucket is one
    /// hour and an approximation once a bucket spans several, because an hour's
    /// median stands in for the answers themselves.
    fn median(&mut self) -> Option<i64> {
        self.medians.sort_unstable();
        let total = self.answered();
        let at = |rank: i64| {
            let mut seen = 0;
            self.medians.iter().find(|m| {
                seen += m.1;
                seen >= rank
            })
        };
        Some((at((total + 1) / 2)?.0 + at(total / 2 + 1)?.0) / 2)
    }
}

/// 「需要处理的节点」一条榜的行：探测名 · 节点名 · 数值 · 样本数。
pub type AtRiskRow = (String, String, f64, i64);

/// 三条榜：延迟最差 · 丢包最多 · 离线最久（后者是 `(节点名, last_seen)`）。
pub type AtRisk = (Vec<AtRiskRow>, Vec<AtRiskRow>, Vec<(String, i64)>);

/// 热力图的一个时间桶：每条的计数 + 丢包数。
///
/// `counts` 的长度是 `edges.len() - 1`（由调用方保证）；`lost` 单列而不是混进
/// `counts` —— 丢包没有延迟、落不进任何延迟区间，而它恰恰是最该看见的那一维。
/// 报告周期：日报 / 周报 / 月报 / 季报。
///
/// **四个周期共用同一个区间算法** ✓ —— 它们不是四套逻辑，而是"同一张按天快照表"的
/// 四种长度 ✓（这正是先做 `traffic_day` 换来的性质 ✓）。
// 整条报告链**还没接线** ✗（`report` 模块只有测试在调用 ✓）—— 见 src/report.rs 顶部那段说明 ✓：
// 下一步的调度器会构造这些变体 ✓，届时连同 report.rs 那行模块级 allow 一起删掉 ✓。
#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Period {
    Day,
    Week,
    Month,
    Quarter,
    /// 自然半年：1–6 月 / 7–12 月 ✓（**不是财年** ✗ —— 本仓其余地方也都是自然年 / ISO 周 ✓）。
    Half,
    Year,
}

impl Period {
    /// 该周期的**上一个完整周期**：返回 `(start, end, prev_start, prev_end)`，
    /// 四个日期都是 `YYYY-MM-DD`，区间**含首不含尾**（`start <= date < end` ✓）。
    ///
    /// **刻意取"已结束的那一个"，而不是"至今"** ✗：报告是按时发的（默认 09:00 ✓），
    /// 若取"至今"，今天这一截会被算进去 ✓ ⇒ 同一份日报在 09:00 与 17:00 是两个数 ✗，
    /// 而"与上周期对比"的基准也会跟着浮动 ✓。固定成"昨天整天"才可比 ✓、也才可复现 ✓。
    ///
    /// 边界一律按 **ISO**（周一起算 ✓、季度 1/4/7/10 月起算 ✓）—— 与东八区的习惯一致 ✓。
    // 调度器（下一步）会调用它 ✓；现在 `title()` 只用到 `Period` 本身 ✗ ⇒ 暂标 allow，
    // 紧跟着这条说明，接上调度后删掉 ✓（与之前的处理一致 ✓）。
    #[allow(dead_code)]
    pub fn last_full(self, today: chrono::NaiveDate) -> (String, String, String, String) {
        use chrono::{Datelike, Duration, NaiveDate};
        let (start, end) = match self {
            Period::Day => (today - Duration::days(1), today),
            Period::Week => {
                // 本周一（`weekday().num_days_from_monday()`：周一 = 0 ✓）
                let this_mon = today - Duration::days(today.weekday().num_days_from_monday() as i64);
                (this_mon - Duration::days(7), this_mon)
            }
            Period::Month => {
                let this_first = NaiveDate::from_ymd_opt(today.year(), today.month(), 1).unwrap();
                let prev_first = if today.month() == 1 {
                    NaiveDate::from_ymd_opt(today.year() - 1, 12, 1).unwrap()
                } else {
                    NaiveDate::from_ymd_opt(today.year(), today.month() - 1, 1).unwrap()
                };
                (prev_first, this_first)
            }
            Period::Half => {
                // 当前半年从 1 月或 7 月起 ✓ —— 于是"上一个完整半年"就是**另一半** ✓
                // （上半年里看到的是去年下半年 ✓，跨年 ✓）。
                let m = if today.month() <= 6 { 1 } else { 7 };
                let this_h = NaiveDate::from_ymd_opt(today.year(), m, 1).unwrap();
                let prev_h = if m == 1 {
                    NaiveDate::from_ymd_opt(today.year() - 1, 7, 1).unwrap()
                } else {
                    NaiveDate::from_ymd_opt(today.year(), 1, 1).unwrap()
                };
                (prev_h, this_h)
            }
            Period::Year => {
                let this_y = NaiveDate::from_ymd_opt(today.year(), 1, 1).unwrap();
                (NaiveDate::from_ymd_opt(today.year() - 1, 1, 1).unwrap(), this_y)
            }
            Period::Quarter => {
                let this_q_first_month = ((today.month() - 1) / 3) * 3 + 1; // 1 / 4 / 7 / 10
                let this_q = NaiveDate::from_ymd_opt(today.year(), this_q_first_month, 1).unwrap();
                let prev_q = if this_q_first_month == 1 {
                    NaiveDate::from_ymd_opt(today.year() - 1, 10, 1).unwrap()
                } else {
                    NaiveDate::from_ymd_opt(today.year(), this_q_first_month - 3, 1).unwrap()
                };
                (prev_q, this_q)
            }
        };
        // **上一周期按日历算，不按"等长后退"** ✗ —— 这一条是测试抓出来的 ✓：
        // 月份长度不等，2024 年 2 月（闰月 29 天 ✓）若后退等长天数会落到 **2024-01-03** ✗，
        // 而"上一个月"显然是 1 月整月 ✓。季度同理 ✓（跨年时更明显 ✓）。
        let prev = match self {
            Period::Day => start - Duration::days(1),
            Period::Week => start - Duration::days(7),
            Period::Month => {
                if start.month() == 1 {
                    NaiveDate::from_ymd_opt(start.year() - 1, 12, 1).unwrap()
                } else {
                    NaiveDate::from_ymd_opt(start.year(), start.month() - 1, 1).unwrap()
                }
            }
            Period::Quarter => {
                if start.month() == 1 {
                    NaiveDate::from_ymd_opt(start.year() - 1, 10, 1).unwrap()
                } else {
                    NaiveDate::from_ymd_opt(start.year(), start.month() - 3, 1).unwrap()
                }
            }
            Period::Half => {
                if start.month() == 1 {
                    NaiveDate::from_ymd_opt(start.year() - 1, 7, 1).unwrap()
                } else {
                    NaiveDate::from_ymd_opt(start.year(), 1, 1).unwrap()
                }
            }
            Period::Year => NaiveDate::from_ymd_opt(start.year() - 1, 1, 1).unwrap(),
        };
        (start.to_string(), end.to_string(), prev.to_string(), start.to_string())
    }
}

pub struct HeatBucket {
    pub ts: i64,
    pub counts: Vec<i64>,
    pub lost: i64,
}

/// 一个探测任务在窗口内的**延迟分布矩阵**：时间桶 × 延迟区间 → 频次。
///
/// 为什么需要它：折线图只能画一个统计量（P50/P90…），回答不了"这段时间**堆在哪个区间**"；
/// 而这个矩阵能 —— 而且它和丢包条一样是**分格**的，所以"长窗口 + 短事件"不会被压成几个像素。
pub struct Heatmap {
    pub edges: Vec<i64>,
    pub buckets: Vec<HeatBucket>,
}

impl Db {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        // Queried before CREATE TABLE runs: a file with no tables receives the
        // current schema directly rather than the history of how it was reached.
        let fresh = conn
            .query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='table'", [], |r| r.get::<_, i64>(0))?
            == 0;
        conn.execute_batch(SCHEMA)?;
        restrict(path);

        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        migrate(&conn, if fresh { SCHEMA_VERSION } else { version })?;
        Ok(Self { conn: Mutex::new(conn), ping_errors: Default::default() })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Records why a probe produced no sample, or forgets the reason once one arrives:
    /// a task that starts working must stop explaining itself.
    pub fn note_ping_error(&self, node_id: i64, task_id: i64, error: Option<&str>) {
        let mut errors = self.ping_errors.lock().unwrap_or_else(|e| e.into_inner());
        match error {
            Some(text) => {
                errors.insert((node_id, task_id), (text.to_owned(), Utc::now().timestamp()));
            }
            None => {
                errors.remove(&(node_id, task_id));
            }
        }
    }

    /// The reasons still worth showing, by `(node, task)`. Anything older than
    /// [`PING_ERROR_TTL`] is dropped on the way out rather than swept on a timer: it is
    /// read far less often than it is written.
    pub fn ping_errors(&self) -> std::collections::HashMap<(i64, i64), String> {
        let mut errors = self.ping_errors.lock().unwrap_or_else(|e| e.into_inner());
        let cutoff = Utc::now().timestamp() - PING_ERROR_TTL;
        errors.retain(|_, (_, at)| *at >= cutoff);
        errors.iter().map(|(k, (text, _))| (*k, text.clone())).collect()
    }

    // ---- settings ----

    pub fn get(&self, key: &str) -> Option<String> {
        self.lookup(key).ok().flatten()
    }

    /// As [`Db::get`], with a failed read kept apart from an absent key, for a
    /// caller that would otherwise act on "nothing saved".
    pub fn lookup(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row("SELECT value FROM setting WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn set(&self, key: &str, value: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO setting (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // ---- nodes ----

    pub fn nodes(&self) -> Result<Vec<Node>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT * FROM node ORDER BY sort, id")?;
        let rows = stmt.query_map([], |r| Ok(row_to_node(r)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Node id to the agent version it reported, for the one caller that judges whether
    /// a node can run a probe kind at all. `nodes()` would answer too and is much
    /// heavier; this is a single column.
    pub fn agent_versions(&self) -> Result<std::collections::HashMap<i64, String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT id, agent_version FROM node")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn node(&self, id: i64) -> Result<Option<Node>> {
        Ok(self
            .conn()
            .query_row("SELECT * FROM node WHERE id = ?1", [id], |r| Ok(row_to_node(r)))
            .optional()?)
    }

    /// Creates a node and returns its id.
    ///
    /// Both rows or neither: `accumulate` reads the `traffic` row on every
    /// report, so a node lacking one cannot report.
    pub fn create_node(&self, n: &Node, token: &str) -> Result<i64> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            // A new node belongs at the end. The caller sends sort 0, which would
            // tie with whatever the last reorder placed first.
            "INSERT INTO node (name, token, sort, public, price, currency, billing_cycle,
                               expires_at, remark, traffic_limit, traffic_mode, traffic_reset_day, created_at, \"group\",
                               private_remark)
             VALUES (?1,?2,(SELECT COALESCE(MAX(sort),-1)+1 FROM node),?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            params![
                n.name,
                token,
                n.public,
                n.price,
                n.currency,
                n.billing_cycle,
                n.expires_at,
                n.remark,
                n.traffic_limit,
                n.traffic_mode,
                n.traffic_reset_day,
                Utc::now().timestamp(),
                n.group,
                n.private_remark
            ],
        )?;
        let id = tx.last_insert_rowid();
        tx.execute("INSERT INTO traffic (node_id) VALUES (?1)", [id])?;
        tx.commit()?;
        Ok(id)
    }

    /// How many nodes were created at or after `ts`. Bounds what one registration
    /// window can add; see `api::REGISTER_LIMIT`.
    pub fn nodes_created_since(&self, ts: i64) -> Result<i64> {
        Ok(self.conn().query_row("SELECT COUNT(*) FROM node WHERE created_at >= ?1", [ts], |r| r.get(0))?)
    }

    /// Backdates a node, for tests that need one older than the window it is
    /// measured over. `create_node` always stamps the moment it runs, and a node
    /// created during a test is by definition younger than every window -- which
    /// is the one case uptime does not divide over, so the clamp needs a node
    /// whose birthday is in the past to be exercised at all.
    #[cfg(test)]
    pub fn set_created_at(&self, id: i64, ts: i64) -> Result<()> {
        self.conn().execute("UPDATE node SET created_at=?2 WHERE id=?1", params![id, ts])?;
        Ok(())
    }

    /// Records that the node reported. Written on the same cadence as the metric
    /// row, so it costs one update per minute rather than one per report.
    pub fn touch_seen(&self, id: i64, ts: i64) -> Result<()> {
        self.conn().execute("UPDATE node SET last_seen=?2 WHERE id=?1", params![id, ts])?;
        Ok(())
    }

    /// False when no node has this id.
    pub fn update_node(&self, id: i64, n: &NodePatch) -> Result<bool> {
        let found = self.conn().execute(
            "UPDATE node SET name=COALESCE(?2,name), sort=COALESCE(?3,sort), public=COALESCE(?4,public),
                             price=COALESCE(?5,price), currency=COALESCE(?6,currency),
                             billing_cycle=COALESCE(?7,billing_cycle),
                             expires_at=CASE WHEN ?8 THEN ?9 ELSE expires_at END,
                             remark=COALESCE(?10,remark), traffic_limit=COALESCE(?11,traffic_limit),
                             traffic_mode=COALESCE(?12,traffic_mode),
                             traffic_reset_day=COALESCE(?13,traffic_reset_day),
                             notify=COALESCE(?14,notify), \"group\"=COALESCE(?15,\"group\"),
                             country_pin=COALESCE(?16,country_pin),
                             ipv4_pin=COALESCE(?17,ipv4_pin), ipv6_pin=COALESCE(?18,ipv6_pin),
                             private_remark=COALESCE(?19,private_remark)
             WHERE id=?1",
            params![
                id,
                n.name,
                n.sort,
                n.public,
                n.price,
                n.currency,
                n.billing_cycle,
                n.expires_at.is_some(),
                n.expires_at.as_ref().and_then(|v| v.as_deref()),
                n.remark,
                n.traffic_limit,
                n.traffic_mode,
                n.traffic_reset_day,
                n.notify,
                n.group,
                n.country_pin,
                n.ipv4_pin,
                n.ipv6_pin,
                n.private_remark
            ],
        )?;
        Ok(found > 0)
    }

    pub fn set_expiry(&self, id: i64, date: &str) -> Result<()> {
        self.conn().execute("UPDATE node SET expires_at=?2 WHERE id=?1", params![id, date])?;
        Ok(())
    }

    pub fn set_down_since(&self, id: i64, ts: i64) -> Result<()> {
        self.conn().execute("UPDATE node SET down_since=?2 WHERE id=?1", params![id, ts])?;
        Ok(())
    }

    pub fn reorder_nodes(&self, ids: &[i64]) -> Result<()> {
        self.reorder("node", ids)
    }

    pub fn reorder_ping_tasks(&self, ids: &[i64]) -> Result<()> {
        self.reorder("ping_task", ids)
    }

    /// Renumbers `sort` from a list that must name every row exactly once, so a
    /// tab that missed an insert or a delete cannot renumber around it.
    ///
    /// The count is read inside the transaction, not before it: re-reading the
    /// list here would only race the write it guards.
    fn reorder(&self, table: &str, ids: &[i64]) -> Result<()> {
        let unique: HashSet<_> = ids.iter().collect();
        if unique.len() != ids.len() {
            anyhow::bail!("排序里有重复的条目");
        }
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let count: i64 = tx.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?;
        if count as usize != ids.len() {
            anyhow::bail!("列表已在别处改动，刷新后再排序");
        }
        let sql = format!("UPDATE {table} SET sort=?2 WHERE id=?1");
        for (sort, id) in ids.iter().enumerate() {
            if tx.execute(&sql, params![id, sort as i64])? != 1 {
                anyhow::bail!("列表已在别处改动，刷新后再排序");
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// False when no node has this id.
    pub fn delete_node(&self, id: i64) -> Result<bool> {
        let conn = self.conn();
        // `ping_record` carries no foreign key -- it is WITHOUT ROWID and keyed
        // for the chart query -- so it is cleared explicitly. SQLite reassigns a
        // deleted node's id to the next node created, which would otherwise
        // inherit the removed machine's latency chart.
        conn.execute("DELETE FROM ping_record WHERE node_id = ?1", [id])?;
        Ok(conn.execute("DELETE FROM node WHERE id = ?1", [id])? > 0)
    }

    /// Replaces a node's token, which immediately locks out the old one. False
    /// when no node has this id.
    pub fn reset_token(&self, id: i64, token: &str) -> Result<bool> {
        Ok(self.conn().execute("UPDATE node SET token=?2 WHERE id=?1", params![id, token])? > 0)
    }

    pub fn node_by_token(&self, token: &str) -> Result<Option<i64>> {
        Ok(self.conn().query_row("SELECT id FROM node WHERE token = ?1", [token], |r| r.get(0)).optional()?)
    }

    /// Stores the slow-changing facts an agent sends on connect, and reports
    /// whether the node still requires a country lookup for `source`, the address
    /// `agent_ws::country_source` chose. An empty `source` has no country and is
    /// never owed one.
    ///
    /// A new source invalidates the previous country, so the two move together in
    /// one statement: `SET` reads the row as it was, so the comparison is against
    /// the stored address rather than the one being written. The pair replaced
    /// moves to `country_prev_ip` / `country_prev` if it had an answer, and a
    /// source equal to that address takes its answer back without a lookup.
    pub fn save_facts(&self, id: i64, f: &serde_json::Value, ip: &str, source: &str) -> Result<bool> {
        // The same rule `api::agent_register` applies to the name it receives:
        // these values come from an unvouched machine, control characters break
        // the panel's rows, and the length must be bounded. Six of them -- os,
        // kernel, arch, virt, cpu_name, agent_version -- go straight into the
        // anonymous public frame, which is rebuilt and pushed to every viewer
        // every two seconds, so without a ceiling one node would determine that
        // frame's size. 128 rather than 64: a real PRETTY_NAME runs to about 60
        // characters and a CPU model to about 50.
        let s = |k: &str| {
            f.get(k)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .chars()
                .filter(|c| !c.is_control())
                .take(128)
                .collect::<String>()
        };
        let n = |k: &str| f.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
        let conn = self.conn();
        conn.execute(
            "UPDATE node SET hostname=?2, os=?3, kernel=?4, arch=?5, virt=?6, cpu_name=?7,
                             cpu_cores=?8, mem_total=?9, swap_total=?10, disk_total=?11,
                             agent_version=?12, ip=?13, ipv4=?14, ipv6=?15, country_ip=?16,
                             country=CASE WHEN country_ip=?16 THEN country
                                          WHEN country_prev_ip=?16 THEN country_prev ELSE '' END,
                             country_prev_ip=CASE WHEN country_ip=?16 OR country='' THEN country_prev_ip
                                                  ELSE country_ip END,
                             country_prev=CASE WHEN country_ip=?16 OR country='' THEN country_prev
                                               ELSE country END,
                             -- 追加在最后：已有 16 个编号一个都不动（同 1.9.12 那次的做法）。
                             -- 老 agent 不发这个字段 → false → 落库 0 = 「仅手动升级」✓
                             allow_remote_upgrade=?17
             WHERE id=?1",
            params![
                id,
                s("hostname"),
                s("os"),
                s("kernel"),
                s("arch"),
                s("virt"),
                s("cpu_name"),
                n("cpu_cores"),
                n("mem_total"),
                n("swap_total"),
                n("disk_total"),
                s("agent_version"),
                ip,
                s("ipv4"),
                s("ipv6"),
                source,
                i64::from(f.get("allow_remote_upgrade").and_then(|v| v.as_bool()).unwrap_or(false))
            ],
        )?;
        let blank: bool = conn.query_row("SELECT country = '' FROM node WHERE id=?1", [id], |r| r.get(0))?;
        Ok(blank && !source.is_empty())
    }

    /// Whether the node still lacks a country for `source`: false once a lookup
    /// has landed, or once the node has moved to another address.
    pub fn country_owed(&self, id: i64, source: &str) -> Result<bool> {
        let owed = self
            .conn()
            .query_row(
                "SELECT country = '' FROM node WHERE id=?1 AND country_ip=?2",
                params![id, source],
                |r| r.get(0),
            )
            .optional()?;
        Ok(owed.unwrap_or(false))
    }

    /// Records the country a lookup returned, unless the node moved to another
    /// address while the lookup was outstanding. This is the same rule
    /// `save_facts` encodes in its `CASE`: the country belongs to the address it
    /// was asked about, so a late answer for an address the node has left is not
    /// an answer about the node. Kept apart from the panel's own writes:
    /// `update_node` never touches this column.
    pub fn set_country(&self, id: i64, cc: &str, source: &str) -> Result<()> {
        self.conn()
            .execute("UPDATE node SET country=?2 WHERE id=?1 AND country_ip=?3", params![id, cc, source])?;
        Ok(())
    }

    // ---- traffic ----

    /// Every node's counters in one query, because the node list renders a row per
    /// node and a query per node would queue the agents' writes behind it.
    ///
    /// The period counters are gated on the period they were written for. They
    /// restart lazily in `accumulate`, on the node's next report, so a node
    /// offline since before a boundary still holds the previous period's bytes on
    /// disk. This is the only reader, so the rule lives in one place.
    /// 全队按天的聚合，给「总览」页的趋势图用。返回
    /// `[day_ts, rx_bytes, tx_bytes, rx_peak, tx_peak, cpu_pct, mem_pct, disk_pct]`。
    ///
    /// **每种指标的算法不同，这是这里唯一需要小心的地方：**
    /// - `cpu` 是**按节点加权平均**（权重是该小时有数据的分钟数 `minutes`）；
    /// - 内存与硬盘是 `Σ已用 / Σ总量` —— 真正的全队占用率，而不是各节点百分比的算术平均
    ///   （一台 2G 和一台 64G 的机器，按百分比平均会给出一个没有意义的数）；
    /// - 带宽是**按节点求和**（全队吞吐量 = 各节点速率之和），峰值取当天各小时的**全队**速率最大值。
    ///
    /// `metric_hour.net_rx` 是**字节/秒**（见 `agent_ws` 里 `(total_rx - rx0) / elapsed`），所以一天的
    /// 字节数 = `Σ(速率 × 60 × minutes)`。这是积分近似：假设采样间隔内速率不变，也是这类图表的通行做法。
    /// 最后四个值依次是：**覆盖秒数**、**最热节点的 cpu / 内存 / 硬盘**（给「均值 + 分布带」用）。
    /// **最后一个值是「有数据覆盖的秒数」**：平均速率要除以它，不能除以 86400，否则没有样本的小时
    /// 会以 0 参与平均，把均值压低 —— 那是错的。
    /// 分组用的是 **UTC 日**；面板负责按本地时区显示日期（跨日边界会有小时级的偏移，这一点在那边的注释里写）。
    pub fn overview_daily(&self, days: i64, group: Option<&str>) -> Vec<[f64; 12]> {
        let since = Utc::now().timestamp() - days * 86_400;
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                // 窗口函数一次算出「每个小时的全队速率」，替换掉原先的逐行相关子查询：
                // metric_hour 有两百多万行，逐行子查询是 O(行数) 次索引查找，这是页面加载慢的正因。
                //
                // 最后一个输出列是「有数据覆盖的秒数」：带宽的**平均速率**要除以它，不能除以整天
                // 86400 —— 否则没有样本的小时会以 0 参与平均，把均值压低，那是错的。
                // **说明写在 Rust 注释里，不写进 SQL 字符串**：上一次把 `--` 注释写进字符串，
                // 解析报的是 `no such column: ts`，偏移正好落在注释那几行上。
                "WITH hourly AS (
                     SELECT m.ts, m.minutes, m.cpu, m.mem_used, m.disk_used,
                            m.net_rx, m.net_tx,
                            n.mem_total, n.disk_total,
                            -- 同一小时里有几台在报：用来把「各节点分钟数之和」还原成**墙上时钟**
                            -- 的覆盖秒数。折线画的是全队速率（各节点之和），分母必须与之同口径。
                            COUNT(*) OVER (PARTITION BY m.ts) AS node_count,
                            SUM(m.net_rx) OVER (PARTITION BY m.ts) AS fleet_rx,
                            SUM(m.net_tx) OVER (PARTITION BY m.ts) AS fleet_tx
                       FROM metric_hour m JOIN node n ON n.id = m.node_id
                      WHERE m.ts >= ?1 AND (?2 IS NULL OR n.[group] = ?2)
                 ),
                 -- 按天给每个节点-小时排名，用来取 **P95**（而不是简单的 MAX）。MAX 会被偶发的一小时
                 -- 毛刺顶起来，P95 描述的是「最热的那一小撮机器」——更能代表集群里真实的紧绷程度。
                 ranked AS (
                     SELECT (ts / 86400) * 86400 AS day,
                            cpu,
                            mem_used * 100.0 / NULLIF(mem_total, 0) AS mem,
                            disk_used * 100.0 / NULLIF(disk_total, 0) AS disk,
                            ROW_NUMBER() OVER (PARTITION BY (ts / 86400) ORDER BY cpu) AS rn_cpu,
                            ROW_NUMBER() OVER (PARTITION BY (ts / 86400) ORDER BY mem_used * 100.0 / NULLIF(mem_total, 0)) AS rn_mem,
                            ROW_NUMBER() OVER (PARTITION BY (ts / 86400) ORDER BY disk_used * 100.0 / NULLIF(disk_total, 0)) AS rn_disk,
                            COUNT(*) OVER (PARTITION BY (ts / 86400)) AS n,
                            -- 其余列原样带下来，外层聚合仍按墙上时钟口径算。
                            minutes, net_rx, net_tx, fleet_rx, fleet_tx, node_count,
                            mem_used, mem_total, disk_used, disk_total
                       FROM hourly
                 )
                 SELECT day,
                        CAST(SUM(net_rx * 60 * minutes) AS INTEGER),
                        CAST(SUM(net_tx * 60 * minutes) AS INTEGER),
                        CAST(MAX(fleet_rx) AS INTEGER), CAST(MAX(fleet_tx) AS INTEGER),
                        SUM(cpu * minutes) * 1.0 / NULLIF(SUM(minutes), 0),
                        SUM(mem_used * minutes) * 100.0 / NULLIF(SUM(mem_total * minutes), 0),
                        SUM(disk_used * minutes) * 100.0 / NULLIF(SUM(disk_total * minutes), 0),
                        SUM(minutes * 60.0 / node_count),
                        -- 「最热的那一台」：cpu 本身就是百分比；内存与硬盘要用**逐节点**的
                        -- 已用/总量，不能拿全队已用去除全队总量 —— 那会把单台的打爆平均掉。
                        -- P95：取「排名进入前 5%」的那些值里的最大者。
                        MAX(CASE WHEN rn_cpu >= n * 0.95 THEN cpu END),
                        MAX(CASE WHEN rn_mem >= n * 0.95 THEN mem END),
                        MAX(CASE WHEN rn_disk >= n * 0.95 THEN disk END)
                   FROM ranked
                  GROUP BY day ORDER BY day",
            )
            .unwrap();
        let rows = stmt
            .query_map(rusqlite::params![since, group], |r| {
                Ok([
                    r.get::<_, i64>(0)? as f64,
                    r.get::<_, i64>(1)? as f64,
                    r.get::<_, i64>(2)? as f64,
                    r.get::<_, i64>(3)? as f64,
                    r.get::<_, i64>(4)? as f64,
                    r.get::<_, Option<f64>>(5)?.unwrap_or(0.0),
                    r.get::<_, Option<f64>>(6)?.unwrap_or(0.0),
                    r.get::<_, Option<f64>>(7)?.unwrap_or(0.0),
                    r.get::<_, Option<f64>>(8)?.unwrap_or(0.0),
                    r.get::<_, Option<f64>>(9)?.unwrap_or(0.0),
                    r.get::<_, Option<f64>>(10)?.unwrap_or(0.0),
                    r.get::<_, Option<f64>>(11)?.unwrap_or(0.0),
                ])
            })
            .unwrap();
        rows.filter_map(|r| r.ok()).collect()
    }

    /// 「需要处理的节点」三条榜。**一次算完**，卡片切榜时不必再发请求。
    ///
    /// - 延迟：`ping_hour.latency` 是**逐小时的中位**，所以这里按 `answered` 加权求平均 ——
    ///   那是**均值**，不是中位。**标签就写「平均」**，不冒充 p50。
    /// - 丢包：`Σlost / Σ(answered+lost)`，就是这段时间的丢包率。
    /// - 离线：只按 `last_seen` 从旧到新取（且 `last_seen > 0`，排除"从未上报"）。
    ///   **在线与否的判定不在这里重写** —— 面板已经有 `online` 标志，阈值只该有一处。
    pub fn at_risk(&self, days: i64, limit: i64, group: Option<&str>) -> AtRisk {
        let conn = self.conn();
        let since = Utc::now().timestamp() - days * 86_400;
        let latency = {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT t.name, n.name,
                            SUM(h.latency * h.answered) * 1.0 / NULLIF(SUM(h.answered), 0),
                            SUM(h.answered)
                       FROM ping_hour h
                       JOIN ping_task t ON t.id = h.task_id
                       JOIN node n ON n.id = h.node_id
                      WHERE h.ts >= ?1 AND h.answered > 0 AND (?3 IS NULL OR n.[group] = ?3)
                      GROUP BY h.task_id, h.node_id
                      ORDER BY 3 DESC
                      LIMIT ?2",
                )
                .unwrap();
            stmt.query_map(params![since, limit, group], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, f64>(2)?, r.get::<_, i64>(3)?))
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .collect::<Vec<AtRiskRow>>()
        };
        let loss = {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT t.name, n.name,
                            SUM(h.lost) * 1.0 / NULLIF(SUM(h.answered + h.lost), 0),
                            SUM(h.answered + h.lost)
                       FROM ping_hour h
                       JOIN ping_task t ON t.id = h.task_id
                       JOIN node n ON n.id = h.node_id
                      WHERE h.ts >= ?1 AND (h.answered + h.lost) > 0 AND (?3 IS NULL OR n.[group] = ?3)
                      GROUP BY h.task_id, h.node_id
                      HAVING SUM(h.lost) > 0
                      ORDER BY 3 DESC
                      LIMIT ?2",
                )
                .unwrap();
            stmt.query_map(params![since, limit, group], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, f64>(2)?, r.get::<_, i64>(3)?))
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .collect::<Vec<AtRiskRow>>()
        };
        let down = {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT name, last_seen FROM node
                      WHERE last_seen > 0 AND (?2 IS NULL OR [group] = ?2)
                      ORDER BY last_seen ASC
                      LIMIT ?1",
                )
                .unwrap();
            stmt.query_map(params![limit, group], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect::<Vec<_>>()
        };
        (latency, loss, down)
    }

    /// 一次拿到**一个探测任务**在窗口内的全部节点序列 —— 面板因此只发一个请求，
    /// 而不是"每台一个"（100 台 = 100 个请求；100 ms RTT 下实测 2 326 ms 对 698 ms，
    /// 且随节点数线性增长）。
    ///
    /// **与 [`Db::ping_window`] 逐点一致** —— 那一条是 `api.rs` 真正调用的
    /// （`/nodes/{id}/metrics?series=ping`），所以参照必须是它：
    /// 同一段**两层拼接**的 SQL（`ping_hour` 到水位线 + `ping_record` 之后，分钟行改造成
    /// 同一个 `Sample` 形状）、同一个 [`Tally`] 折叠、同一个 [`close_tallies`] 收尾、
    /// 同一套 rank 排序与**未取整**的窗口 loss。区别只有一个：这里按节点分组。
    ///
    /// 先前两版都把参照认错了（先是小时级 `ping_records_hourly`，再是分钟级 `ping_records`），
    /// 结果"与 X 一致"是对着一个**不是线上用的**函数成立的。这一版按 `ping_window` 的 SQL 抄。
    /// 一个探测任务在窗口内的**延迟分布矩阵**：时间桶 × 延迟区间 → 频次。
    ///
    /// **只查明细（`ping_record`）** ✓ —— 小时汇总里没有频次分布（只有 `answered/lost/latency/lo/hi`），
    /// 所以热力图的长窗口受**水位线**限制：水位线之前的数据只能靠小时行，而小时行给不出区间计数。
    /// 于是这里显式只取明细 ⇒ 窗口上限就是明细的保留期（7 天）；更长的窗口要等汇总带上频次。
    ///
    /// 区间边界用 [`Db::band_edges`]（由**这个窗口内**的分位数算 ✓）—— 不写死毫秒：
    /// 延迟的"正常"取决于目标。
    pub fn ping_heatmap(&self, task_id: i64, since: i64, step: i64, group: Option<&str>) -> Result<Heatmap> {
        let conn = self.conn();
        let rolled = rolled(&conn)?.unwrap_or(0);
        let mut stmt = conn.prepare_cached(
            "SELECT (ts/?4)*?4 AS bucket, latency FROM ping_record
             WHERE task_id=?1 AND ts>=?2 AND ts>=?3
             AND (?5 IS NULL OR node_id IN (SELECT id FROM node WHERE \"group\"=?5))
             ORDER BY bucket",
        )?;
        let mut rows = stmt.query(params![task_id, since, rolled, step, group])?;
        let mut samples: Vec<(i64, i64)> = Vec::new();
        while let Some(row) = rows.next()? {
            samples.push((row.get(0)?, row.get(1)?));
        }
        drop(rows);
        drop(stmt);
        // 边界只由**有效样本**算（丢包没有延迟，写进来会把分布拉低）。
        let mut ok: Vec<i64> = samples.iter().map(|(_, l)| *l).filter(|l| *l >= 0).collect();
        ok.sort_unstable();
        let edges = Db::band_edges(&ok);
        let mut buckets: Vec<HeatBucket> = Vec::new();
        let mut i = 0;
        while i < samples.len() {
            let ts = samples[i].0;
            let mut counts = vec![0i64; edges.len() - 1];
            let mut lost = 0i64;
            while i < samples.len() && samples[i].0 == ts {
                let lat = samples[i].1;
                if lat < 0 {
                    lost += 1;
                } else {
                    counts[Db::band_of(&edges, lat)] += 1;
                }
                i += 1;
            }
            buckets.push(HeatBucket { ts, counts, lost });
        }
        Ok(Heatmap { edges, buckets })
    }

    pub fn ping_series_by_task(
        &self,
        task_id: i64,
        since: i64,
        step: i64,
        group: Option<&str>,
    ) -> Result<ByTaskSeries> {
        let conn = self.conn();
        let rolled = rolled(&conn)?.unwrap_or(0);
        let mut stmt = conn.prepare_cached(
            "SELECT (ts/?5)*?5 AS bucket, node_id, task_id, latency, answered, lost, lo, hi FROM (
               SELECT ts, node_id, task_id, latency, answered, lost, lo, hi FROM ping_hour
                WHERE task_id=?1 AND ts>=?2 AND ts<?3
                  AND (?6 IS NULL OR node_id IN (SELECT id FROM node WHERE \"group\"=?6))
               UNION ALL
               SELECT ts, node_id, task_id,
                      CASE WHEN latency >= 0 THEN latency END,
                      CASE WHEN latency >= 0 THEN 1 ELSE 0 END,
                      CASE WHEN latency < 0 THEN 1 ELSE 0 END,
                      CASE WHEN latency >= 0 THEN latency END,
                      CASE WHEN latency >= 0 THEN latency END
                 FROM ping_record WHERE task_id=?1 AND ts>=?2 AND ts>=?3
                  AND (?6 IS NULL OR node_id IN (SELECT id FROM node WHERE \"group\"=?6))
             ) ORDER BY node_id, (ts/?5)*?5",
        )?;
        let mut rows = stmt.query(params![task_id, since, rolled, rolled, step, group])?;
        let mut out: NodeTotals = Default::default();
        let mut node = -1i64;
        let mut bucket = -1i64;
        let mut open: std::collections::BTreeMap<i64, Tally> = Default::default();
        let flush = |out: &mut NodeTotals,
                     node: i64,
                     bucket: i64,
                     open: &mut std::collections::BTreeMap<i64, Tally>| {
            if node < 0 || open.is_empty() {
                return;
            }
            let mut v: Vec<serde_json::Value> = Vec::new();
            close_tallies(&mut v, std::mem::take(open), bucket);
            out.entry(node).or_default().0.extend(v);
        };
        while let Some(row) = rows.next()? {
            let b = row.get::<_, i64>(0)?;
            let n = row.get::<_, i64>(1)?;
            if n != node || b != bucket {
                flush(&mut out, node, bucket, &mut open);
                node = n;
                bucket = b;
            }
            let task = row.get::<_, i64>(2)?;
            let sample = Sample {
                median: row.get::<_, Option<i64>>(3)?,
                answered: row.get::<_, i64>(4)?,
                lost: row.get::<_, i64>(5)?,
                lo: row.get::<_, Option<i64>>(6)?,
                hi: row.get::<_, Option<i64>>(7)?,
            };
            let seen = out.entry(n).or_default().1.entry(task).or_insert((0, 0));
            seen.0 += sample.lost;
            seen.1 += sample.answered + sample.lost;
            open.entry(task).or_default().add(sample);
        }
        flush(&mut out, node, bucket, &mut open);
        drop(rows);
        drop(stmt);
        let rank: HashMap<i64, usize> = conn
            .prepare_cached("SELECT id FROM ping_task ORDER BY sort, id")?
            .query_map([], |r| r.get(0))?
            .enumerate()
            .map(|(i, id)| id.map(|id| (id, i)))
            .collect::<Result<_, _>>()?;
        drop(conn);
        Ok(out
            .into_iter()
            .map(|(n, (mut v, totals))| {
                v.sort_by_cached_key(|row| {
                    row["task_id"].as_i64().and_then(|id| rank.get(&id).copied()).unwrap_or(usize::MAX)
                });
                let loss: serde_json::Map<String, serde_json::Value> = totals
                    .into_iter()
                    .filter(|(_, (lost, _))| *lost > 0)
                    .map(|(task, (lost, samples))| {
                        (task.to_string(), serde_json::json!(100.0 * lost as f64 / samples as f64))
                    })
                    .collect();
                (n, (v, serde_json::Value::Object(loss)))
            })
            .collect())
    }

    /// 快照里**最早的一天** ✓（`None` = 还没有任何快照 ✓）。
    ///
    /// 报告的"本期覆盖 N 天"要它 ✓（见 `report::coverage_note` ✓）——
    /// 半年报与年报必须带这句 ✗：快照是从启用那天才有的 ✓，否则第一份年报会静悄悄地偏小 ✓。
    pub fn earliest_traffic_day(&self) -> Option<String> {
        let conn = self.conn();
        conn.query_row("SELECT MIN(date) FROM traffic_day", [], |r| r.get::<_, Option<String>>(0))
            .ok()
            .flatten()
            .filter(|d| !d.is_empty())
    }

    /// 把一次 IP 查询的结论落到节点上 ✓（八个可空列 ✓）。
    ///
    /// **查不到的字段写 NULL，不写空串** ✓✓ —— 这一条是刻意的 ✗：
    /// `''` 会被读成"这个国家是空的" ✓，而 `NULL` 才是"未知" ✓；
    /// 面板与主题据此决定要不要显示那一格 ✓ —— 两者混起来就没法区分了 ✓。
    /// （`params!` 里传 `Option` 就自然得到 NULL ✓，不必手写 `CASE` ✓。）
    // 调用它的是**下一步**：节点连上来时按"欠一次查询"那套触发（`agent_ws.rs` 的 `country_owed` ✓）。
    // 在那之前只有测试在用 ✗ ⇒ 暂标 allow，接上即删 ✓。
    #[allow(dead_code)]
    pub fn save_quality(&self, node_id: i64, q: &crate::geo::Quality) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE node SET q_country=?2, q_city=?3, q_subdivision=?4, q_latitude=?5,
                             q_longitude=?6, q_time_zone=?7, q_asn=?8, q_org=?9
             WHERE id=?1",
            params![
                node_id,
                q.country,
                q.city,
                q.subdivision,
                q.latitude,
                q.longitude,
                q.time_zone,
                q.asn,
                q.org
            ],
        )?;
        Ok(())
    }

    /// 每台节点的**月度额度**（0 = 不限 ✓，与 `node.traffic_limit` 同一约定 ✓）。
    ///
    /// 报告的"超限 / 将超限"要它 ✓ —— 额度是**月度**的 ✓，所以判断看的是"本月已用"
    /// 而不是这个周期的量 ✓（见 `report::body` 的说明 ✓）。
    pub fn traffic_limits(&self) -> std::collections::HashMap<i64, i64> {
        let conn = self.conn();
        let Ok(mut stmt) = conn.prepare_cached("SELECT id, traffic_limit FROM node") else {
            return Default::default();
        };
        stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    }

    /// 从按天快照表求和：某个日期区间内**每台节点**的上下行合计，按总量从多到少 ✓。
    ///
    /// 区间**含首不含尾**（`from <= date < to` ✓）—— 与 [`Period::last_full`] 返回的四个日期
    /// 同一约定 ✓。周报 / 月报 / 季报因此是**同一句 SQL** ✓，只是区间不同 ✓。
    ///
    /// 返回 `(node_id, name, rx, tx)` ✓ —— 名字一起带出来 ✓，因为报告的"逐节点明细"就是它 ✓
    /// （调用方不必再查一次 ✓，也就不会出现两处各写一次、早晚分叉 ✗）。
    pub fn traffic_sums(&self, from: &str, to: &str) -> Vec<(i64, String, i64, i64)> {
        let conn = self.conn();
        let Ok(mut stmt) = conn.prepare_cached(
            "SELECT d.node_id, n.name, SUM(d.rx), SUM(d.tx)
             FROM traffic_day d JOIN node n ON n.id = d.node_id
             WHERE d.date >= ?1 AND d.date < ?2
             GROUP BY d.node_id
             ORDER BY SUM(d.rx) + SUM(d.tx) DESC, d.node_id",
        ) else {
            return Vec::new();
        };
        stmt.query_map(params![from, to], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                r.get::<_, Option<i64>>(3)?.unwrap_or(0),
            ))
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
    }

    pub fn all_traffic(&self) -> HashMap<i64, Traffic> {
        let conn = self.conn();
        let Ok(mut stmt) = conn.prepare_cached(
            "SELECT t.node_id, t.total_rx, t.total_tx, t.month_rx, t.month_tx, t.month_start,
                    t.day_rx, t.day_tx, t.day_start, n.traffic_reset_day
                 FROM traffic t JOIN node n ON n.id = t.node_id",
        ) else {
            return HashMap::new();
        };
        let today = Local::now().date_naive();
        let day = today.to_string();
        let rows = stmt.query_map([], |r| {
            // Zero rather than absent: a theme drawing a meter requires a
            // number.
            let current = |stored: String, now: &str, rx: i64, tx: i64| {
                if stored == now {
                    (rx, tx)
                } else {
                    (0, 0)
                }
            };
            let period = period_start(today, r.get(9)?).to_string();
            let (month_rx, month_tx) = current(r.get(5)?, &period, r.get(3)?, r.get(4)?);
            let (day_rx, day_tx) = current(r.get(8)?, &day, r.get(6)?, r.get(7)?);
            Ok((
                r.get::<_, i64>(0)?,
                Traffic {
                    total_rx: r.get(1)?,
                    total_tx: r.get(2)?,
                    month_rx,
                    month_tx,
                    month_start: period,
                    day_rx,
                    day_tx,
                },
            ))
        });
        rows.map(|r| r.flatten().collect()).unwrap_or_default()
    }

    /// Folds one report's raw kernel counters into the node's running totals.
    ///
    /// A changed boot_id, or a counter that moved backwards, means the kernel
    /// restarted its counting; the total must not follow it downward. `None`
    /// denotes a report carrying no readable counters at all -- see below.
    ///
    /// The billing reset day is read here rather than passed in: it is one join
    /// from a row this already reads, and fetching it separately would cost every
    /// report a second acquisition of the single write connection.
    pub fn accumulate(&self, node_id: i64, boot_id: &str, counters: Option<(i64, i64)>) -> Result<Traffic> {
        let conn = self.conn();
        let (
            prev_boot,
            last_rx,
            last_tx,
            mut total_rx,
            mut total_tx,
            mut month_rx,
            mut month_tx,
            month_start,
            mut day_rx,
            mut day_tx,
            day_start,
            reset_day,
        ) = conn
            .prepare_cached(
                "SELECT t.boot_id, t.last_rx, t.last_tx, t.total_rx, t.total_tx, t.month_rx, t.month_tx,
                    t.month_start, t.day_rx, t.day_tx, t.day_start, n.traffic_reset_day
                 FROM traffic t JOIN node n ON n.id = t.node_id WHERE t.node_id=?1",
            )?
            .query_row([node_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, i64>(8)?,
                    r.get::<_, i64>(9)?,
                    r.get::<_, String>(10)?,
                    r.get::<_, u32>(11)?,
                ))
            })?;

        // Only bytes this hub observed a counter climb through are booked.
        // Without a baseline under this exact boot there is nothing to subtract
        // from, and a bare reading represents the machine's entire history.
        //
        // The baseline can be missing in three ways, all handled identically. A
        // first report has none. A reading that shrank under the same boot lost
        // one -- an interface included in the sum has disappeared -- so the
        // reading is the remainder of that history and booking it would count it
        // twice. A changed boot_id means the counters restarted or,
        // indistinguishably from here, that a second machine shares the token.
        // Realigning costs the seconds since the reboot; the alternative costs
        // hundreds of gigabytes against a total that only increases.
        //
        // A fourth case: no reading at all. The row is left exactly as it was,
        // since writing zero would realign the baseline to zero and book the next
        // report's lifetime counter as a single delta.
        let (d_rx, d_tx) = match counters {
            None => (0, 0),
            Some(_) if prev_boot.is_empty() || prev_boot != boot_id => {
                // Logged in either case: on a healthy node this is a reboot,
                // while one every few seconds indicates two machines sharing a
                // token.
                if !prev_boot.is_empty() {
                    info!("node {node_id} reports a new boot; re-aligning");
                }
                (0, 0)
            }
            Some((rx, tx)) => ((rx.saturating_sub(last_rx)).max(0), (tx.saturating_sub(last_tx)).max(0)),
        };
        // Saturating rather than a plain `+`: the release profile disables
        // overflow checks, so a total near i64::MAX would wrap to a large
        // negative -- a lifetime figure that has decreased. Two paths reach this
        // column: a node's own counters, which arrive from another repository's
        // binary, and `set_traffic`, through which the panel writes corrections.
        // Clamping here covers both rather than each caller separately.
        total_rx = total_rx.saturating_add(d_rx);
        total_tx = total_tx.saturating_add(d_tx);
        month_rx = month_rx.saturating_add(d_rx);
        month_tx = month_tx.saturating_add(d_tx);
        day_rx = day_rx.saturating_add(d_rx);
        day_tx = day_tx.saturating_add(d_tx);

        // Both boundaries are calendar dates -- the day a provider resets an
        // allowance, the day a person means by "today" -- so both follow the
        // hub's local timezone rather than UTC.
        let period = period_start(Local::now().date_naive(), reset_day).to_string();
        if month_start != period {
            // A new billing period restarts the month counter but not the total.
            month_rx = d_rx;
            month_tx = d_tx;
        }
        let today = Local::now().date_naive().to_string();
        if day_start != today {
            // ⚠️ **首次上报不算日切** ✗：新节点的这一行里 `day_start` 是**空串**（列默认值 ✓），
            // 于是它第一次上报也满足 `day_start != today` ✓ —— 但那不是"一天结束了"，
            // 而是"还**没有**过任何一天" ✓。不排除它就会写出一行 `date=''` 的空快照 ✗，
            // 而任何按日期求和的报表都得先把它剔掉 ✓。
            // （这条是单测 `a_rollover_snapshots_the_day_that_just_ended` 的第一条断言抓到的 ✓。）
            if !day_start.is_empty() {
                // **值直接由 SQL 从表里取** ✓（而不是用局部变量 ✗）：
                // 走到这里时，局部 `day_rx/day_tx` 可能**已经被重算成新一天的值** ✓
                // —— 单测的第二条断言就是这么抓到的（存进去的是 9500/4600，而要求是 8000/4000 ✗）。
                // 而那一刻表里**还是旧值** ✓（重置的 UPDATE 在这段之后 ✓），
                // 所以 `INSERT ... SELECT` 读到的必然"刚结束那天"的总量 ✓ ——
                // 它免疫于局部变量的顺序 ✓，这正是我想要的性质 ✓。
                conn.execute(
                    "INSERT OR REPLACE INTO traffic_day (node_id, date, rx, tx)
                     SELECT node_id, day_start, day_rx, day_tx FROM traffic WHERE node_id = ?1",
                    params![node_id],
                )?;
            }
            day_rx = d_rx;
            day_tx = d_tx;
        }

        if let Some((rx, tx)) = counters {
            conn.prepare_cached(
                "UPDATE traffic SET boot_id=?2, last_rx=?3, last_tx=?4, total_rx=?5, total_tx=?6,
                                month_rx=?7, month_tx=?8, month_start=?9, day_rx=?10, day_tx=?11,
                                day_start=?12 WHERE node_id=?1",
            )?
            .execute(params![
                node_id, boot_id, rx, tx, total_rx, total_tx, month_rx, month_tx, period, day_rx, day_tx,
                today
            ])?;
        }
        Ok(Traffic { total_rx, total_tx, month_rx, month_tx, month_start: period, day_rx, day_tx })
    }

    /// Allows the panel to correct a total, for example after moving a node to
    /// new hardware.
    ///
    /// The corrected month figures are stamped with the current period; otherwise
    /// they would belong to whichever period the row still held, `all_traffic`
    /// would read them back as zero, and the node's next report would restart the
    /// counter and discard the correction.
    ///
    /// False when no node has this id.
    pub fn set_traffic(&self, node_id: i64, p: &TrafficPatch) -> Result<bool> {
        let conn = self.conn();
        let Some(reset_day): Option<u32> = conn
            .query_row("SELECT traffic_reset_day FROM node WHERE id=?1", [node_id], |r| r.get(0))
            .optional()?
        else {
            return Ok(false);
        };
        let period = period_start(Local::now().date_naive(), reset_day).to_string();
        conn.execute(
            "UPDATE traffic SET total_rx=COALESCE(?2,total_rx), total_tx=COALESCE(?3,total_tx),
                 month_rx=COALESCE(?4,CASE WHEN month_start=?6 THEN month_rx ELSE 0 END),
                 month_tx=COALESCE(?5,CASE WHEN month_start=?6 THEN month_tx ELSE 0 END), month_start=?6
             WHERE node_id=?1",
            params![node_id, p.total_rx, p.total_tx, p.month_rx, p.month_tx, period],
        )?;
        Ok(true)
    }

    // ---- metrics ----

    pub fn insert_metric(&self, node_id: i64, ts: i64, m: &serde_json::Value) -> Result<()> {
        let f = |k: &str| m.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
        let n = |k: &str| m.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
        // Not read with `f`: an absent `load1` must reach SQLite as NULL rather
        // than the zero every other column defaults to, because the history
        // query averages it and a minute with no load sample would otherwise
        // drag the curve toward zero. Only `write_into` ever omits it, and only
        // for a minute no report gave a load.
        let load1 = m.get("load1").and_then(|v| v.as_f64());
        self.conn()
            .prepare_cached(
                "INSERT OR REPLACE INTO metric
               (node_id, ts, cpu, mem_used, swap_used, disk_used, net_rx, net_tx, tcp, udp, procs, load1,
                net_rx_max, net_tx_max)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            )?
            .execute(params![
                node_id,
                ts,
                f("cpu"),
                n("mem_used"),
                n("swap_used"),
                n("disk_used"),
                n("net_rx"),
                n("net_tx"),
                n("tcp"),
                n("udp"),
                n("procs"),
                load1,
                n("net_rx_max"),
                n("net_tx_max")
            ])?;
        Ok(())
    }

    /// History for one node, thinned to one sample every `step` seconds.
    ///
    /// Bucketed rather than filtered on a multiple of `step`: rows normally land
    /// on the minute, but nothing enforces it, and a filter would return nothing
    /// for a stamp falling between grid lines.
    ///
    /// Averaged over the bucket rather than sampled from it. Keeping one row per
    /// bucket would reintroduce the 1/60 sampling the write side already rejects:
    /// the seven-day window integrated to 53.69 GB against the 27.52 GB the
    /// minutes hold, while averaging gives 28.02 GB, matching the accumulator.
    ///
    /// `swap_used` and `load1` are returned alongside `mem_used`, all averaged
    /// over the bucket, since a memory chart draws the first two together and
    /// the CPU panel draws the load curve beside the CPU curve. `tcp`, `udp`
    /// and `procs` are stored but not returned, as nothing draws them from
    /// history. The columns are retained deliberately.
    ///
    /// `load1` is a real number, so unlike `mem_used`/`swap_used` it is left as
    /// `AVG` returns it; and `AVG` over a bucket whose rows are all NULL is
    /// NULL, which reaches the JSON as null rather than as the zero a chart
    /// would draw as an idle machine.
    ///
    /// The stamp is the bucket's start rather than a row inside it, so every
    /// series lands on one grid and the probe rows below can be shared.
    ///
    /// `net_rx_max` and `net_tx_max` are the bucket's **highest** rather than its
    /// mean, since a maximum of maxima loses nothing: a week's window peaks at
    /// the rate the busiest minute reached. Each row counts as at least its own
    /// mean -- rows predating the column hold 0, and the mean, timed by the hub's
    /// arrivals rather than the agent's own clock, can edge past the agent's
    /// rates by the network's jitter.
    /// The probe series as the hourly tier holds them, for a window that has already
    /// been folded: each hour row is one sample whose median stands for `answered`
    /// answers, and [`Tally`] weights it by exactly that.
    ///
    /// Read-only for now, like [`Db::metrics_hourly`]: the read path picks between
    /// this and [`Db::ping_records`] once it also stitches the minute rows past the
    /// watermark.
    #[allow(dead_code)]
    pub fn ping_records_hourly(
        &self,
        node_id: i64,
        since: i64,
        until: i64,
        step: i64,
    ) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(
            "SELECT (ts/?4)*?4, task_id, answered, lost, latency, lo, hi FROM ping_hour
              WHERE node_id=?1 AND ts>=?2 AND ts<?3 ORDER BY ts/?4",
        )?;
        let mut rows = stmt.query(params![node_id, since, until, step])?;
        let mut out: Vec<serde_json::Value> = Vec::new();
        let mut open: Option<(i64, std::collections::BTreeMap<i64, Tally>)> = None;
        while let Some(r) = rows.next()? {
            let bucket = r.get::<_, i64>(0)?;
            if open.as_ref().map(|(b, _)| *b) != Some(bucket) {
                if let Some((ts, tallies)) = open.take() {
                    close_tallies(&mut out, tallies, ts);
                }
                open = Some((bucket, Default::default()));
            }
            let task = r.get::<_, i64>(1)?;
            let sample = Sample {
                answered: r.get(2)?,
                lost: r.get(3)?,
                median: r.get(4)?,
                lo: r.get(5)?,
                hi: r.get(6)?,
            };
            open.as_mut().expect("just set").1.entry(task).or_default().add(sample);
        }
        if let Some((ts, tallies)) = open.take() {
            close_tallies(&mut out, tallies, ts);
        }
        Ok(out)
    }

    /// The resource series for a window that **reaches past the watermark**: the
    /// hourly tier up to it, the minute rows after it, and the two merged by bucket.
    ///
    /// Merged in SQL, not in Rust, and that is what settles the seam: the weight is
    /// explicit in each half (`minutes` for an hour row, 1 for a minute row), and a
    /// bucket straddling the watermark -- which happens whenever `step` is coarser
    /// than an hour, and always for the theme's six-hour and one-day ranges -- is
    /// one `GROUP BY` bucket rather than two values that have to be combined by
    /// hand. Without a watermark (`rolled` is `None`) the hour half selects nothing
    /// and this is exactly what the minute rows alone would give.
    pub fn metrics_window(&self, node_id: i64, since: i64, step: i64) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn();
        let rolled = rolled(&conn)?.unwrap_or(0);
        let mut stmt = conn.prepare_cached(
            "SELECT (b.ts/?4)*?4, SUM(b.cpu*b.w)*1.0/SUM(b.w),
                    CAST(SUM(b.mem_used*b.w)*1.0/SUM(b.w) AS INTEGER),
                    CAST(SUM(b.swap_used*b.w)*1.0/SUM(b.w) AS INTEGER),
                    CAST(SUM(b.disk_used*b.w)*1.0/SUM(b.w) AS INTEGER),
                    CAST(SUM(b.net_rx*b.w)*1.0/SUM(b.w) AS INTEGER),
                    CAST(SUM(b.net_tx*b.w)*1.0/SUM(b.w) AS INTEGER),
                    CASE WHEN SUM(CASE WHEN b.load1 IS NOT NULL THEN b.w END) > 0
                         THEN SUM(b.load1*b.w)*1.0/SUM(CASE WHEN b.load1 IS NOT NULL THEN b.w END) END,
                    MAX(b.net_rx_max), MAX(b.net_tx_max)
             FROM (
               SELECT ts, cpu, mem_used, swap_used, disk_used, net_rx, net_tx, load1,
                      net_rx_max, net_tx_max, minutes AS w
                 FROM metric_hour WHERE node_id=?1 AND ts>=?2 AND ts<?3
               UNION ALL
               SELECT ts, cpu, mem_used, swap_used, disk_used, net_rx, net_tx, load1,
                      MAX(net_rx, net_rx_max), MAX(net_tx, net_tx_max), 1
                 FROM metric WHERE node_id=?1 AND ts>=?2 AND ts>=?3
             ) b GROUP BY (b.ts/?4)*?4 ORDER BY (b.ts/?4)*?4",
        )?;
        let rows = stmt.query_map(params![node_id, since, rolled, step], |r| {
            Ok(serde_json::json!({
                "ts": r.get::<_, i64>(0)?, "cpu": r.get::<_, f64>(1)?,
                "mem_used": r.get::<_, i64>(2)?, "swap_used": r.get::<_, i64>(3)?,
                "disk_used": r.get::<_, i64>(4)?,
                "net_rx": r.get::<_, i64>(5)?, "net_tx": r.get::<_, i64>(6)?,
                "load1": r.get::<_, Option<f64>>(7)?,
                "net_rx_max": r.get::<_, i64>(8)?, "net_tx_max": r.get::<_, i64>(9)?,
            }))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The resource series as the hourly tier holds them, for a window that has
    /// already been folded.
    ///
    /// Averaged **weighted by `minutes`**, so a bucket covering hours of different
    /// lengths -- a hub that was down, an agent that reported late -- does not let a
    /// short hour count as much as a full one, and the answer matches what the same
    /// minutes would give one row at a time. `load1` divides by the minutes that
    /// carried a value and is NULL when none did, which is what `AVG(load1)` does
    /// over the minute rows by ignoring NULLs. Multiplied by `1.0` to divide as
    /// reals: integer division would truncate twice where `AVG` truncates once.
    ///
    /// #[allow] until the read path picks between this and [`Db::metrics`]: a window
    /// wider than the detail window reads this before the watermark and the minute
    /// rows after it.
    #[allow(dead_code)]
    pub fn metrics_hourly(
        &self,
        node_id: i64,
        since: i64,
        until: i64,
        step: i64,
    ) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(
            "SELECT (ts/?4)*?4, SUM(cpu*minutes)*1.0/SUM(minutes),
                    CAST(SUM(mem_used*minutes)*1.0/SUM(minutes) AS INTEGER),
                    CAST(SUM(swap_used*minutes)*1.0/SUM(minutes) AS INTEGER),
                    CAST(SUM(disk_used*minutes)*1.0/SUM(minutes) AS INTEGER),
                    CAST(SUM(net_rx*minutes)*1.0/SUM(minutes) AS INTEGER),
                    CAST(SUM(net_tx*minutes)*1.0/SUM(minutes) AS INTEGER),
                    CASE WHEN SUM(CASE WHEN load1 IS NOT NULL THEN minutes END) > 0
                         THEN SUM(load1*minutes)*1.0/SUM(CASE WHEN load1 IS NOT NULL THEN minutes END) END,
                    MAX(net_rx_max), MAX(net_tx_max)
             FROM metric_hour WHERE node_id=?1 AND ts>=?2 AND ts<?3
             GROUP BY ts/?4 ORDER BY ts/?4",
        )?;
        let rows = stmt.query_map(params![node_id, since, until, step], |r| {
            Ok(serde_json::json!({
                "ts": r.get::<_, i64>(0)?, "cpu": r.get::<_, f64>(1)?,
                "mem_used": r.get::<_, i64>(2)?, "swap_used": r.get::<_, i64>(3)?,
                "disk_used": r.get::<_, i64>(4)?,
                "net_rx": r.get::<_, i64>(5)?, "net_tx": r.get::<_, i64>(6)?,
                "load1": r.get::<_, Option<f64>>(7)?,
                "net_rx_max": r.get::<_, i64>(8)?, "net_tx_max": r.get::<_, i64>(9)?,
            }))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn metrics(&self, node_id: i64, since: i64, step: i64) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(
            "SELECT (MIN(ts)/?3)*?3, AVG(cpu), CAST(AVG(mem_used) AS INTEGER),
                    CAST(AVG(swap_used) AS INTEGER),
                    CAST(AVG(disk_used) AS INTEGER),
                    CAST(AVG(net_rx) AS INTEGER), CAST(AVG(net_tx) AS INTEGER),
                    AVG(load1),
                    MAX(MAX(net_rx, net_rx_max)), MAX(MAX(net_tx, net_tx_max))
             FROM metric WHERE node_id=?1 AND ts>=?2 GROUP BY ts/?3 ORDER BY ts/?3",
        )?;
        let rows = stmt.query_map(params![node_id, since, step], |r| {
            Ok(serde_json::json!({
                "ts": r.get::<_, i64>(0)?, "cpu": r.get::<_, f64>(1)?,
                "mem_used": r.get::<_, i64>(2)?, "swap_used": r.get::<_, i64>(3)?,
                "disk_used": r.get::<_, i64>(4)?,
                "net_rx": r.get::<_, i64>(5)?, "net_tx": r.get::<_, i64>(6)?,
                "load1": r.get::<_, Option<f64>>(7)?,
                "net_rx_max": r.get::<_, i64>(8)?, "net_tx_max": r.get::<_, i64>(9)?,
            }))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Every minute each node reported within each of the two uptime windows,
    /// counted in one pass.
    ///
    /// The absence is the signal: a row is written once per reported minute and
    /// never for a minute the node was silent, so the count of rows is the count
    /// of minutes the node was up. One query for both windows because the node
    /// list is what every anonymous visitor to the status page loads, and a
    /// second scan of the same rows would double what that page costs.
    ///
    /// `since7` must be the later of the two boundaries. A node silent across
    /// both windows simply has no row here, which the caller reads as zero.
    pub fn uptime_counts(&self, since7: i64, since30: i64, until: i64) -> Result<HashMap<i64, (i64, i64)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(
            "SELECT node_id, SUM(ts >= ?1), COUNT(*) FROM metric
             WHERE ts >= ?2 AND ts < ?3 GROUP BY node_id",
        )?;
        let rows = stmt.query_map(params![since7, since30, until], |r| {
            Ok((r.get::<_, i64>(0)?, (r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Every minute one node reported inside `[since, until)`, oldest first.
    ///
    /// The timestamps alone, read from the primary key, so this is an index-only
    /// seek into the node's own rows. `count(*) GROUP BY hour` would answer the
    /// availability bar more cheaply, but it cannot say *where inside an hour*
    /// the missing minutes were, and an outage is reported to the minute -- a
    /// twelve-minute gap leaves an hour that is 80% full, which a bare count
    /// cannot tell from a gap of twelve minutes somewhere else in it.
    pub fn reported_minutes(&self, node_id: i64, since: i64, until: i64) -> Result<Vec<i64>> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare_cached("SELECT ts FROM metric WHERE node_id=?1 AND ts>=?2 AND ts<?3 ORDER BY ts")?;
        let rows = stmt.query_map(params![node_id, since, until], |r| r.get::<_, i64>(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Folds every hour whose rows are complete into the summary tables and
    /// returns how many were folded.
    ///
    /// One hour per transaction, so a long catch-up -- the first run after an
    /// upgrade folds every hour still held as minute rows -- lets the agents'
    /// writes through between hours. Measured on 90 days of 100 nodes: 2159
    /// hours in 159 s, the slowest request during it 118 ms.
    ///
    /// Without a watermark it starts at the oldest minute row within
    /// `keep_days`; with no rows at all, at the current hour, which is recorded
    /// so that `prune` has a watermark to respect.
    pub fn roll_up(&self, now: i64, keep_days: i64) -> Result<usize> {
        let (mut hour, recorded) = {
            let conn = self.conn();
            match rolled(&conn)? {
                Some(hour) => (hour, true),
                None => {
                    let from = oldest(&conn, &["metric", "ping_record"])?.unwrap_or(now);
                    (from.max(now - keep_days * 86_400).div_euclid(3_600) * 3_600, false)
                }
            }
        };
        let mut folded = 0;
        while hour + 3_600 + LATE <= now {
            if let Err(e) = self.fold_hour(hour) {
                // A writer held the lock past `busy_timeout`. Measured: one such
                // conflict aborted a catch-up that had folded 7 of 2159 hours, and
                // the next pass starts over from the same hour -- so the whole pass
                // is lost work. Every row is written idempotently, so waiting and
                // retrying the same hour costs only time.
                if !is_locked(&e) {
                    return Err(e);
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
                continue;
            }
            // Between hours, not only between nodes: a catch-up folds thousands of
            // hours, and the agents' writes must get in throughout.
            let_waiters_in();
            hour += 3_600;
            folded += 1;
        }
        if folded == 0 && !recorded {
            // Nothing to fold yet, but the watermark has to exist before `prune`
            // may delete a minute row at all.
            self.set(ROLLED, &hour.to_string())?;
        }
        Ok(folded)
    }

    /// One hour of every node into `metric_hour` and `ping_hour`, and the
    /// watermark past it, in one transaction: a failure leaves the hour to be
    /// folded again rather than half folded, and `INSERT OR REPLACE` makes a
    /// second fold of the same hour a rewrite.
    fn fold_hour(&self, hour: i64) -> Result<()> {
        // Three kinds of write, and **none of them holds the lock the agents write
        // through for longer than one node's work**. It was one transaction per
        // hour, which on a large hour held it long enough to refuse an agent's
        // report: the probe below measures the lock, not the fold.
        //
        // Every row is written with `INSERT OR REPLACE`, so a failure anywhere
        // leaves the hour to be folded again and rewriting what did land; the
        // watermark written last is what makes the hour count as folded.
        // Both halves are folded a block of nodes at a time, for the same reason:
        // `ts` alone cannot seek -- the keys begin with `node_id` -- so a statement
        // covering **every** node scans the whole table once per hour. Measured:
        // 461 ms an hour with the metric fold written that way, against a few
        // milliseconds with an `IN` list per block.
        let nodes: Vec<i64> = {
            let conn = self.conn();
            let mut stmt = conn.prepare_cached("SELECT id FROM node")?;
            let ids: Vec<i64> = stmt.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
            ids
        };
        for block in nodes.chunks(FOLD_NODES) {
            let mut conn = self.conn();
            let tx = conn.transaction()?;
            // `IN (?, ...)`, not `BETWEEN`: an inequality on the leading primary key
            // column `node_id` leaves the `ts` range as a **filter over every row
            // those nodes ever wrote** -- 5.2M rows, 2157 ms an hour -- where an IN
            // list is one equality seek per id with the range inside it.
            //
            // The median is picked by rank in SQL rather than in Rust: pulling the
            // rows out cost 441 ms an hour, most of it moving 24 000 rows through the
            // driver, against a few statements this way. `LIMIT 2 OFFSET
            // (COUNT(*)-1)/2` takes the middle one when the count is odd and the
            // middle two when it is even, and `CAST(AVG(...) AS INTEGER)` truncates --
            // the answer `close_bucket` computes in Rust for the minute path, which is
            // what lets a window drawn from either layer agree.
            // `IN (?, ...)`, not `BETWEEN`: an inequality on the leading primary key
            // column `node_id` leaves the `ts` range as a **filter over every row
            // those nodes ever wrote** -- 5.2M rows, 2157 ms an hour -- where an IN
            // list is one equality seek per id with the range inside it.
            //
            // The median is picked by rank in SQL rather than in Rust: pulling the
            // rows out cost 441 ms an hour, most of it moving 24 000 rows through the
            // driver. `ROW_NUMBER()` gives each answer its rank, the two ranks
            // `(n+1)/2` and `n/2+1` are the middle one or the middle two, and
            // `CAST(AVG(...) AS INTEGER)` truncates -- the answer `close_bucket`
            // computes in Rust for the minute path, which is what lets a window drawn
            // from either layer agree.
            //
            // Not `ORDER BY ... LIMIT 2 OFFSET (SELECT COUNT(*) ...)`: a LIMIT
            // expression cannot reference the outer row, so the offset has to come
            // from a window function instead.
            // Every placeholder numbered explicitly: mixing `?` with `?N` makes the
            // numbering depend on document order, and the count SQLite then expects
            // does not match the values passed.
            let first: Vec<String> = (1..=block.len()).map(|i| format!("?{i}")).collect();
            let h = block.len() + 1;
            let metric_sql = format!(
                "INSERT OR REPLACE INTO metric_hour
                   (node_id, ts, minutes, cpu, mem_used, swap_used, disk_used, net_rx, net_tx, load1,
                    net_rx_max, net_tx_max)
                 SELECT node_id, ?{h}, COUNT(*), AVG(cpu), CAST(AVG(mem_used) AS INTEGER),
                        CAST(AVG(swap_used) AS INTEGER), CAST(AVG(disk_used) AS INTEGER),
                        CAST(AVG(net_rx) AS INTEGER), CAST(AVG(net_tx) AS INTEGER), AVG(load1),
                        MAX(MAX(net_rx, net_rx_max)), MAX(MAX(net_tx, net_tx_max))
                 FROM metric WHERE node_id IN ({}) AND ts>=?{h} AND ts<?{h}+3600
                 GROUP BY node_id",
                first.join(",")
            );
            let mut mvalues: Vec<rusqlite::types::Value> = block.iter().map(|id| (*id).into()).collect();
            mvalues.push(hour.into());
            tx.prepare_cached(&metric_sql)?.execute(rusqlite::params_from_iter(mvalues))?;
            let second: Vec<String> = (0..block.len()).map(|i| format!("?{}", h + 1 + i)).collect();
            let (marks, more) = (first.join(","), second.join(","));
            let sql = format!(
                "INSERT OR REPLACE INTO ping_hour
                   (node_id, task_id, ts, answered, lost, latency, lo, hi)
                 SELECT a.node_id, a.task_id, ?{h}, a.answered, a.lost, m.latency, a.lo, a.hi
                 FROM (
                   SELECT node_id, task_id,
                          SUM(CASE WHEN latency >= 0 THEN 1 ELSE 0 END) AS answered,
                          SUM(CASE WHEN latency <  0 THEN 1 ELSE 0 END) AS lost,
                          MIN(CASE WHEN latency >= 0 THEN latency END) AS lo,
                          MAX(CASE WHEN latency >= 0 THEN latency END) AS hi
                     FROM ping_record
                    WHERE node_id IN ({marks}) AND ts >= ?{h} AND ts < ?{h} + 3600
                    GROUP BY node_id, task_id
                 ) a
                 LEFT JOIN (
                   SELECT node_id, task_id, CAST(AVG(latency) AS INTEGER) AS latency
                     FROM (
                       SELECT node_id, task_id, latency,
                              ROW_NUMBER() OVER (PARTITION BY node_id, task_id ORDER BY latency) AS rn,
                              COUNT(*)     OVER (PARTITION BY node_id, task_id) AS n
                         FROM ping_record
                        WHERE node_id IN ({more}) AND ts >= ?{h} AND ts < ?{h} + 3600
                          AND latency >= 0
                     )
                    WHERE rn IN ((n + 1) / 2, n / 2 + 1)
                    GROUP BY node_id, task_id
                 ) m ON m.node_id = a.node_id AND m.task_id = a.task_id"
            );
            let mut values: Vec<rusqlite::types::Value> = Vec::new();
            values.extend(block.iter().map(|id| (*id).into()));
            values.push(hour.into());
            values.extend(block.iter().map(|id| (*id).into()));
            tx.prepare_cached(&sql)?.execute(rusqlite::params_from_iter(values))?;
            tx.commit()?;
            drop(conn);
            let_waiters_in();
        }
        self.set(ROLLED, &(hour + 3_600).to_string())?;
        Ok(())
    }

    /// Drops history beyond the retention window: minute rows past
    /// [`DETAIL_DAYS`] (or the window itself, when shorter) and hourly rows past
    /// `keep_days`. Traffic totals live in their own table precisely so history
    /// can be pruned freely.
    ///
    /// **A minute row outlives its window until its hour has been folded**, which
    /// is what keeps a rollup that has fallen behind costing disk rather than
    /// history: the cutoff is the earlier of the two.
    ///
    /// Per node and at most a day at a time, with the lock the agents write
    /// through released in between. `ts < ?` alone scans the whole table: 25 s at
    /// 90 days of 100 nodes against 86 ms by seek, and the first pass after an
    /// upgrade deletes everything past the detail window -- 37.8 s in one
    /// statement, 0.9 s per node this way, with no agent report refused.
    pub fn prune(&self, keep_days: i64) -> Result<usize> {
        let now = Utc::now().timestamp();
        let (nodes, rolled): (Vec<i64>, Option<i64>) = {
            let conn = self.conn();
            let nodes = conn
                .prepare_cached("SELECT id FROM node")?
                .query_map([], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            (nodes, rolled(&conn)?)
        };
        // No watermark yet means nothing has been folded, so no minute row may go.
        let minutes = (now - keep_days.min(DETAIL_DAYS) * 86_400).min(rolled.unwrap_or(i64::MIN));
        let hours = now - keep_days * 86_400;
        let mut pruned = 0;
        for id in nodes {
            for (table, before) in
                [("metric", minutes), ("ping_record", minutes), ("metric_hour", hours), ("ping_hour", hours)]
            {
                loop {
                    let conn = self.conn();
                    let first: Option<i64> = conn
                        .prepare_cached(&format!("SELECT MIN(ts) FROM {table} WHERE node_id=?1"))?
                        .query_row([id], |r| r.get(0))?;
                    let Some(first) = first.filter(|&ts| ts < before) else { break };
                    pruned += conn
                        .prepare_cached(&format!("DELETE FROM {table} WHERE node_id=?1 AND ts<?2"))?
                        .execute(params![id, before.min(first + 86_400)])?;
                    // Released before the pause, or the pause is itself a lock.
                    drop(conn);
                    let_waiters_in();
                }
            }
        }
        Ok(pruned)
    }

    // ---- ping ----

    pub fn ping_tasks(&self) -> Result<Vec<PingTask>> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT id, name, target, interval, kind FROM ping_task ORDER BY sort, id")?;
        let tasks: Vec<PingTask> = stmt
            .query_map([], |r| {
                Ok(PingTask {
                    kind: r.get("kind")?,
                    id: r.get(0)?,
                    name: r.get(1)?,
                    target: r.get(2)?,
                    interval: r.get(3)?,
                    nodes: Vec::new(),
                })
            })?
            .collect::<Result<_, _>>()?;
        drop(stmt);
        let mut stmt = conn.prepare("SELECT node_id FROM ping_node WHERE task_id=?1")?;
        tasks
            .into_iter()
            .map(|mut t| {
                t.nodes = stmt.query_map([t.id], |r| r.get(0))?.collect::<Result<_, _>>()?;
                Ok(t)
            })
            .collect()
    }

    /// The maximum number of probes one node may be assigned.
    ///
    /// The agent enforces the same limit: `MAX_PING_TASKS` in that repository
    /// caps the list it will run, since a compromised or buggy hub could
    /// otherwise ask a node for hundreds of outbound connects per second. That
    /// cap is a defence and remains, but on its own it truncates silently,
    /// leaving one line in the node's journal while the hub continues pushing
    /// probes that never run and drawing charts that stay empty.
    ///
    /// The hub knows the total, so the hub issues the refusal. The two must stay
    /// in step; the agent's copy is the backstop rather than the message.
    const MAX_PROBES_PER_NODE: i64 = 64;

    /// The assignments are replaced wholesale, so they run in one transaction:
    /// failing between the delete and the inserts would unassign every node from
    /// a probe the panel still lists them under.
    pub fn save_ping_task(&self, t: &PingTask) -> Result<i64> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let id = if t.id > 0 {
            tx.execute(
                "UPDATE ping_task SET name=?2, target=?3, interval=?4, kind=?5 WHERE id=?1",
                // `probe_kind()` rather than the field: an absent or empty kind is
                // written as `tcp`, so the column always says what the task does.
                params![t.id, t.name, t.target, t.interval, t.probe_kind()],
            )?;
            t.id
        } else {
            tx.execute(
                // At the end, as in `create_node`.
                "INSERT INTO ping_task (name, target, interval, sort, kind)
                 VALUES (?1,?2,?3,(SELECT COALESCE(MAX(sort),-1)+1 FROM ping_task),?4)",
                params![t.name, t.target, t.interval, t.probe_kind()],
            )?;
            tx.last_insert_rowid()
        };
        tx.execute("DELETE FROM ping_node WHERE task_id=?1", [id])?;
        for node in &t.nodes {
            // The foreign key is the check; naming the node turns SQLite's
            // "FOREIGN KEY constraint failed" into something the panel can show.
            tx.execute("INSERT INTO ping_node (task_id, node_id) VALUES (?1,?2)", params![id, node])
                .with_context(|| format!("节点 {node} 不存在"))?;
        }
        // Queried from the table after the rows are in rather than counted from
        // the request: an update replaces this task's own assignments, so
        // arithmetic on the way in would have to subtract them again. The
        // transaction makes this atomic with the write, and bailing here rolls it
        // back.
        let crowded: Option<i64> = tx
            .query_row(
                "SELECT node_id FROM ping_node GROUP BY node_id HAVING COUNT(*) > ?1 LIMIT 1",
                [Self::MAX_PROBES_PER_NODE],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(node) = crowded {
            anyhow::bail!(
                "节点 {node} 会被分配超过 {} 个探测任务，agent 最多只跑这么多，多出来的会被静默丢掉",
                Self::MAX_PROBES_PER_NODE
            );
        }
        tx.commit()?;
        Ok(id)
    }

    /// Deletes a probe and the results filed under it.
    ///
    /// `ping_record` carries no foreign key -- it is WITHOUT ROWID and keyed for
    /// the chart query -- so it is cleared explicitly, as in `delete_node`.
    /// SQLite reassigns a deleted probe's id to the next one created, and the
    /// chart selects on `task_id IN (assignments for this node)`: without this
    /// the new probe would draw the removed one's latency under its own name,
    /// with its timeouts folded into the loss figure.
    ///
    /// The delete is a scan -- the key begins at `node_id` -- comparable in cost
    /// to `prune`, for an action taken manually a few times a year.
    pub fn delete_ping_task(&self, id: i64) -> Result<()> {
        let conn = self.conn();
        conn.execute("DELETE FROM ping_record WHERE task_id = ?1", [id])?;
        conn.execute("DELETE FROM ping_task WHERE id=?1", [id])?;
        Ok(())
    }

    /// The task list pushed to one agent.
    ///
    /// Ordered, because the agent keeps the first [`Self::MAX_PROBES_PER_NODE`]
    /// as its backstop against a hub requesting hundreds. Unordered, a list at
    /// that boundary could yield a different subset on each push, restarting half
    /// the timers each time; `save_ping_task` prevents reaching that boundary,
    /// and this makes the backstop deterministic should a database arrive there
    /// by another route.
    pub fn ping_tasks_for(&self, node_id: i64) -> Result<Vec<serde_json::Value>> {
        // **Derived from `ping_tasks` rather than its own SELECT.** Two hand-written
        // queries over the same table are how the agent's list silently lost `kind`
        // while the panel's kept it: an ICMP task was then probed as a TCP handshake to a
        // host with no port, which produces no sample and explains nothing -- a real hub
        // showed 100% loss with the target plainly answering in 8 ms. One source of truth
        // means a field added for the panel cannot go missing here.
        //
        // `name` is deliberately not sent: a probe name routinely carries a hostname or a
        // customer (see `ping_task_names`), and the agent has no use for it -- it reports
        // by task id.
        //
        // The order is the panel's `sort, id` rather than this function's old `id`. The
        // agent matches tasks by id and does not draw them, so the order is not its
        // concern; keeping one order is the point.
        Ok(self
            .ping_tasks()?
            .into_iter()
            .filter(|t| t.nodes.contains(&node_id))
            .map(|t| {
                serde_json::json!({
                    "id": t.id,
                    "target": t.target,
                    "interval": t.interval,
                    "kind": t.probe_kind(),
                })
            })
            .collect())
    }

    /// Probe names keyed by id, for labelling one node's latency chart. Names
    /// only: targets and node assignments remain behind `Admin`.
    ///
    /// Restricted to the probes assigned to that node, the only ones its chart
    /// has samples to label. A probe name is operator-supplied text that
    /// routinely carries a hostname or a customer, and the rest of the table
    /// belongs to nodes this caller may not be able to see.
    pub fn ping_task_names(&self, node_id: i64) -> Result<serde_json::Value> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, name FROM ping_task WHERE id IN (SELECT task_id FROM ping_node WHERE node_id=?1)",
        )?;
        let rows = stmt.query_map([node_id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        let mut names = serde_json::Map::new();
        for row in rows {
            let (id, name) = row?;
            names.insert(id.to_string(), serde_json::json!(name));
        }
        Ok(serde_json::Value::Object(names))
    }

    /// Files one probe result, and only under a probe this node is assigned. A
    /// result for anything else is dropped rather than treated as an error, since
    /// the agent can do nothing useful with the distinction.
    ///
    /// The assignment is tested inside the statement because that is the only
    /// place it is atomic with the write: `ping_record` carries no foreign key,
    /// being WITHOUT ROWID and keyed for the chart query. Two cases arrive
    /// without an assignment. A result already in flight when the panel deleted
    /// its probe, which would otherwise land after `delete_ping_task` swept the
    /// history and be inherited by whichever probe SQLite assigns the id to next.
    /// And a node token in the wrong hands: every other write an agent can cause
    /// is bounded -- one `metric` row per node per minute, one `traffic` row per
    /// node -- while `task_id` is chosen by the reporter, making this the one
    /// write whose row count would otherwise be unbounded.
    ///
    /// The chart's `task_id IN (assignments)` filter hides both afterwards, but
    /// does not prevent the write, its storage, or the id being reused.
    pub fn insert_ping(&self, node_id: i64, task_id: i64, ts: i64, latency: i64) -> Result<()> {
        self.conn().execute(
            "INSERT OR REPLACE INTO ping_record (node_id, task_id, ts, latency)
             SELECT ?1, ?2, ?3, ?4
             WHERE EXISTS (SELECT 1 FROM ping_node WHERE task_id = ?2 AND node_id = ?1)",
            params![node_id, task_id, ts, latency],
        )?;
        Ok(())
    }

    /// Probe results for one node, one sample per probe per `step` seconds: the
    /// bucket's median round trip, its range, and the proportion lost.
    ///
    /// These stamps fall wherever the probe finished rather than on a minute, so
    /// the thinning buckets them instead of matching a multiple, as in `metrics`
    /// above. This is the larger half of that response, since a probe reports far
    /// more often than once a minute.
    ///
    /// [`PING_ROWS`] returns rows in time order, so a bucket is complete the
    /// moment the next opens and only one is held at a time -- at most the probes
    /// assigned to the node times the results one bucket spans.
    ///
    /// Returns the buckets and, alongside them, the proportion of the whole
    /// window each probe lost. The latter cannot be recovered from the former:
    /// [`close_bucket`] divides within each bucket and keeps only the quotient,
    /// so averaging those percentages would weight a bucket holding one sample
    /// equally with one holding twelve. The buckets are necessarily unequal --
    /// the window's first and last are partial by construction, and a probe that
    /// starts, stops, loses its node or skips a round produces more. The
    /// denominators are available only here, in the pass that already reads every
    /// row. Probes that lost nothing are omitted, as `loss` is per bucket.
    pub fn ping_records(
        &self,
        node_id: i64,
        since: i64,
        step: i64,
    ) -> Result<(Vec<serde_json::Value>, serde_json::Value)> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(PING_ROWS)?;
        let mut rows = stmt.query(params![node_id, since, step])?;
        let mut out = Vec::new();
        // Per probe in the bucket being filled: what answered, and how many did
        // not.
        let mut open: Vec<(i64, Vec<i64>, i64)> = Vec::new();
        // Per probe across the whole window: how many were lost, out of how many.
        // Folded in the same pass rather than queried from SQLite a second time,
        // for the same reason the bucket fold itself is in Rust.
        let mut totals: HashMap<i64, (i64, i64)> = HashMap::new();
        let mut bucket = 0;
        while let Some(row) = rows.next()? {
            let (b, task, latency) = (row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?);
            if b != bucket {
                close_bucket(&mut out, &mut open, bucket * step);
                bucket = b;
            }
            let seen = totals.entry(task).or_insert((0, 0));
            seen.1 += 1;
            let probe = match open.iter().position(|(id, ..)| *id == task) {
                Some(at) => &mut open[at],
                None => {
                    open.push((task, Vec::new(), 0));
                    open.last_mut().expect("just pushed")
                }
            };
            // A timeout is stored as -1: excluded from the median and counted
            // instead.
            if latency < 0 {
                probe.2 += 1;
                seen.0 += 1;
            } else {
                probe.1.push(latency);
            }
        }
        close_bucket(&mut out, &mut open, bucket * step);
        // Probe by probe in the panel's order, each probe's rows still in time
        // order. Themes take their series, colours and legend from the order in
        // which probes first appear; bucket by bucket that would be whichever
        // probe happened to answer inside the window's partial first bucket.
        drop(rows);
        drop(stmt);
        let rank: HashMap<i64, usize> = conn
            .prepare_cached("SELECT id FROM ping_task ORDER BY sort, id")?
            .query_map([], |r| r.get(0))?
            .enumerate()
            .map(|(i, id)| id.map(|id| (id, i)))
            .collect::<Result<_, _>>()?;
        drop(conn);
        // A probe missing from the rank goes last rather than taking the first
        // colour; the assignment filter in `PING_ROWS` rules that out today.
        out.sort_by_cached_key(|row| {
            row["task_id"].as_i64().and_then(|id| rank.get(&id).copied()).unwrap_or(usize::MAX)
        });
        // Unrounded: the caller decides how to render it, and rounding here would
        // turn 0.14% into the 0% that denotes no loss at all.
        let loss: serde_json::Map<String, serde_json::Value> = totals
            .into_iter()
            .filter(|(_, (lost, _))| *lost > 0)
            .map(|(task, (lost, samples))| {
                (task.to_string(), serde_json::json!(100.0 * lost as f64 / samples as f64))
            })
            .collect();
        Ok((out, serde_json::Value::Object(loss)))
    }

    /// The probe series for a window that **reaches past the watermark**: the hourly
    /// tier up to it and the minute rows after, both fed to the same [`Tally`] so the
    /// median is one implementation either way.
    ///
    /// Separate from [`Db::ping_records`] rather than replacing it: that one is what
    /// the panel reads today, and this is switched over to it only once the two are
    /// known to agree. Same assignment filter, so a probe taken off the node stops
    /// appearing here too.
    /// 热力图的区间边界：**由窗口内的分位数算出来**，不写死毫秒。
    ///
    /// 为什么必须这样：延迟的「正常」取决于目标 —— 到香港 3 ms 与到洛杉矶 150 ms 的
    /// "慢"完全不是一回事，写死 `50/100/200` 只会让一张图在一个目标上挤成一条、
    /// 在另一个目标上散成一片。本仓在别处已经写过同一条判断（"延迟的「正常」取决于目标，
    /// 所以不写死毫秒阈值"），这里沿用。
    ///
    /// 返回 `n+1` 个边界（含首尾）：`[min, p10, p25, p50, p75, p90, max]` ⇒ 6 条带。
    /// 用**最近秩**取分位（不做插值）：运维读的是"这一段里有多少个点"，
    /// 边界取一个**真实出现过的值**比取一个插值出来的小数更好解释。
    pub fn band_edges(sorted: &[i64]) -> Vec<i64> {
        if sorted.is_empty() {
            // 没有样本时给一条退化的带子 —— 让调用方不必特判，画出来就是"空的"。
            return vec![0, 1];
        }
        let n = sorted.len();
        let mut edges = vec![sorted[0]];
        for q in [0.10f64, 0.25, 0.50, 0.75, 0.90] {
            let i = ((q * (n as f64 - 1.0)).round() as usize).min(n - 1);
            edges.push(sorted[i]);
        }
        edges.push(sorted[n - 1]);
        // 边界必须**严格递增**：分位数可能重复（样本少、或大量同值时），
        // 而重复的边界会造出宽度为 0 的带子 —— 那种带子永远为空，图上是噪音。
        edges.dedup();
        // 但 dedup 也可能把边界塌缩到**只剩一个值** ⇒ 一条带子都没有 ⇒ 图上是空的。
        // （大量同值时就会这样：全部样本都在同一个毫秒上 —— 单测 `…survive_a_single_value` 抓到过。）
        // 这时给一条**退化但可用**的带子，调用方不必特判。
        if edges.len() < 2 {
            edges.push(edges[0] + 1);
        }
        edges
    }

    /// 一个延迟值落在第几条带（0 起）。区间按 `[lo, hi)` 取，**最后一个带包含上界**
    /// —— 否则"恰好等于窗口最大值的那个样本"会掉出所有带子，凭空少一个点。
    ///
    /// 抽成纯函数是为了能不依赖数据库直接测边界：差一格是这类计数最典型的错，
    /// 而它在图上只表现为"某一格颜色略浅"，肉眼根本看不出来。
    pub fn band_of(edges: &[i64], latency: i64) -> usize {
        debug_assert!(edges.len() >= 2, "至少两个边界才成一条带子");
        let mut k = 0;
        while k + 1 < edges.len() && latency >= edges[k + 1] {
            k += 1;
        }
        k.min(edges.len().saturating_sub(2))
    }

    pub fn ping_window(
        &self,
        node_id: i64,
        since: i64,
        step: i64,
    ) -> Result<(Vec<serde_json::Value>, serde_json::Value)> {
        let conn = self.conn();
        let rolled = rolled(&conn)?.unwrap_or(0);
        let mut stmt = conn.prepare_cached(
            "SELECT (ts/?4)*?4 AS bucket, task_id, latency, answered, lost, lo, hi FROM (
               SELECT ts, task_id, latency, answered, lost, lo, hi FROM ping_hour
                WHERE node_id=?1 AND ts>=?2 AND ts<?3
                  AND task_id IN (SELECT task_id FROM ping_node WHERE node_id=?1)
               UNION ALL
               SELECT ts, task_id,
                      CASE WHEN latency >= 0 THEN latency END,
                      CASE WHEN latency >= 0 THEN 1 ELSE 0 END,
                      CASE WHEN latency < 0 THEN 1 ELSE 0 END,
                      CASE WHEN latency >= 0 THEN latency END,
                      CASE WHEN latency >= 0 THEN latency END
                 FROM ping_record WHERE node_id=?1 AND ts>=?2 AND ts>=?3
                  AND task_id IN (SELECT task_id FROM ping_node WHERE node_id=?1)
             ) ORDER BY (ts/?4)*?4",
        )?;
        let mut rows = stmt.query(params![node_id, since, rolled, step])?;
        let mut out = Vec::new();
        let mut totals: HashMap<i64, (i64, i64)> = HashMap::new();
        let mut open: Option<(i64, std::collections::BTreeMap<i64, Tally>)> = None;
        while let Some(row) = rows.next()? {
            let bucket = row.get::<_, i64>(0)?;
            if open.as_ref().map(|(b, _)| *b) != Some(bucket) {
                if let Some((ts, tallies)) = open.take() {
                    close_tallies(&mut out, tallies, ts);
                }
                // Column 0 is already a timestamp, scaled by `step` in the SQL;
                // multiplying again here made every sample its own bucket.
                open = Some((bucket, Default::default()));
            }
            let task = row.get::<_, i64>(1)?;
            let sample = Sample {
                median: row.get::<_, Option<i64>>(2)?,
                answered: row.get::<_, i64>(3)?,
                lost: row.get::<_, i64>(4)?,
                lo: row.get::<_, Option<i64>>(5)?,
                hi: row.get::<_, Option<i64>>(6)?,
            };
            let seen = totals.entry(task).or_insert((0, 0));
            seen.0 += sample.lost;
            seen.1 += sample.answered + sample.lost;
            open.as_mut().expect("just set").1.entry(task).or_default().add(sample);
        }
        if let Some((ts, tallies)) = open.take() {
            close_tallies(&mut out, tallies, ts);
        }
        drop(rows);
        drop(stmt);
        let rank: HashMap<i64, usize> = conn
            .prepare_cached("SELECT id FROM ping_task ORDER BY sort, id")?
            .query_map([], |r| r.get(0))?
            .enumerate()
            .map(|(i, id)| id.map(|id| (id, i)))
            .collect::<Result<_, _>>()?;
        drop(conn);
        out.sort_by_cached_key(|row| {
            row["task_id"].as_i64().and_then(|id| rank.get(&id).copied()).unwrap_or(usize::MAX)
        });
        let loss: serde_json::Map<String, serde_json::Value> = totals
            .into_iter()
            .filter(|(_, (lost, _))| *lost > 0)
            .map(|(task, (lost, samples))| {
                (task.to_string(), serde_json::json!(100.0 * lost as f64 / samples as f64))
            })
            .collect();
        Ok((out, serde_json::Value::Object(loss)))
    }

    // ---- the database file itself ----

    /// The file this connection is open on, empty for `:memory:`.
    pub fn file(&self) -> String {
        main_file(&self.conn())
    }

    /// The retention window used by both `prune` and the data page. Stored as
    /// text by the settings form, so a missing or unparsable value falls back to
    /// the default rather than erroring.
    pub fn retention_days(&self) -> i64 {
        self.get("retention_days")
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(DEFAULT_RETENTION_DAYS)
            .clamp(1, MAX_RETENTION_DAYS)
    }

    /// What the panel's data page reads: how much space the file occupies, how
    /// much of that is free pages awaiting a `VACUUM`, and how far back the
    /// history actually reaches.
    ///
    /// `oldest` against `retention` is the one pair here that can indicate a
    /// fault: history older than the window means `prune` has not been running.
    pub fn stats(&self) -> Result<serde_json::Value> {
        // Before acquiring the connection: `conn()` returns a guard on a plain
        // Mutex, and `retention_days` acquires the same one.
        let retention = self.retention_days();
        let conn = self.conn();
        let file = main_file(&conn);
        let page_size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
        let free_pages: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
        // Both are pruned at the same cutoff, so the earlier of the two marks where
        // history begins -- sought one node at a time, because `MIN(ts)` over a
        // whole table cannot use a key that begins with `node_id` (15.9 s at 90
        // days of 100 nodes, 1.8 ms this way).
        let oldest = oldest(&conn, &["metric", "ping_record"])?;
        let mut rows = serde_json::Map::new();
        for table in TABLES {
            let n: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?;
            rows.insert(table.to_owned(), serde_json::json!(n));
        }
        // The summary tables, counted apart from `TABLES` so that a backup taken
        // before they existed still passes `check_backup`. This is how an operator
        // sees what the tiering is holding: minute rows fall away at the detail
        // window while these grow with the retention.
        let mut summary = serde_json::Map::new();
        for table in HOUR_TABLES {
            let n: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?;
            summary.insert(table.to_owned(), serde_json::json!(n));
        }
        Ok(serde_json::json!({
            "path": file,
            "size": bytes_of(&file),
            "wal": bytes_of(&format!("{file}-wal")),
            "free": free_pages * page_size,
            "oldest": oldest,
            "retention": retention,
            "rows": rows,
            "summary": summary,
        }))
    }

    /// Writes a consistent copy of the live database to `dest`, which must not
    /// already exist.
    ///
    /// `VACUUM INTO` is SQLite's own mechanism for this: one statement, a single
    /// read transaction, and a compacted copy with free pages already dropped. It
    /// reads the whole file, so the caller runs it off the runtime -- every other
    /// statement here is sub-millisecond, this one is not.
    pub fn backup_into(&self, dest: &str) -> Result<()> {
        // A second connection to the same file. `VACUUM INTO` only reads, and WAL
        // allows it to read a consistent snapshot while the agents continue
        // writing through the first -- exporting is the one heavy operation here
        // that need not block them. A fresh connection inherits none of the
        // PRAGMAs in SCHEMA, so the busy timeout must be set again or a
        // checkpoint racing this read returns SQLITE_BUSY immediately.
        let reader = Connection::open(self.file())?;
        reader.busy_timeout(std::time::Duration::from_secs(5))?;
        reader.execute("VACUUM INTO ?1", [dest])?;
        // The copy is the credential store in one portable file: node tokens in
        // the clear, the GitHub secret, the password hash. SQLite creates it
        // under the umask, which at the usual 022 is world-readable.
        own_only(dest);
        Ok(())
    }

    /// Rebuilds the file, reclaiming the pages deleted history left behind.
    /// Returns the bytes recovered.
    ///
    /// SQLite's constraints on `VACUUM`, and why they hold here: it cannot run
    /// inside a transaction or with a live statement on the connection (there is
    /// one connection, and this call owns it); it requires roughly as much free
    /// disk as the database itself, and a failure rolls back leaving the original
    /// untouched; and it can renumber rowids, which nothing here keys on, since
    /// `metric` and `ping_record` are WITHOUT ROWID and every other table
    /// declares its own primary key.
    ///
    /// In WAL mode the rewrite lands in the WAL first, so without the checkpoint
    /// the file on disk grows rather than shrinking.
    pub fn vacuum(&self) -> Result<i64> {
        let conn = self.conn();
        let file = main_file(&conn);
        let before = on_disk(&file);
        conn.execute_batch("VACUUM")?;
        // Best effort: the space is already reclaimed within the database, and a
        // checkpoint that cannot run now does not constitute a failed vacuum.
        let _ = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
        Ok((before - on_disk(&file)).max(0))
    }

    /// What a file must satisfy before a single page of it is copied over the
    /// live database. Restore is the one operation here that destroys data, and
    /// the file behind it originates from a disk this hub knows nothing about.
    ///
    /// **Writes to `src`.** The migrations an older backup requires run here, on
    /// the upload, rather than after copying: everything that can fail does so
    /// while the live database is still untouched. The caller owns that file and
    /// deletes it in either case.
    pub fn check_backup(&self, src: &str) -> Result<()> {
        // Read-write rather than read-only: a plain copy of a running hub's
        // database is in WAL mode, and SQLite cannot open such a file read-only
        // without its -shm companion.
        let candidate = Connection::open(src)?;
        let health: String = candidate
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .map_err(|e| anyhow::anyhow!("not a readable SQLite database: {e}"))?;
        if health != "ok" {
            anyhow::bail!("the file is a damaged database: {health}");
        }
        // Pages are copied verbatim, so whatever schema the file carries becomes
        // the schema this hub runs its statements against. A view or trigger
        // where a table belongs would route every subsequent write through
        // externally supplied code.
        let plotted: i64 = candidate.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type IN ('view', 'trigger')",
            [],
            |r| r.get(0),
        )?;
        if plotted > 0 {
            anyhow::bail!("the file carries views or triggers, which a hub backup never does");
        }
        for table in TABLES {
            let found: i64 = candidate.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                [table],
                |r| r.get(0),
            )?;
            if found == 0 {
                anyhow::bail!("the file is not a hub backup: no {table} table");
            }
        }
        let version: i64 = candidate.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            anyhow::bail!(
                "the backup is from a newer hub (schema {version}, this one reads {SCHEMA_VERSION}); upgrade first"
            );
        }
        // The online backup API refuses a page size change while the destination
        // is in WAL mode; an explicit message is clearer than SQLITE_READONLY.
        let theirs: i64 = candidate.query_row("PRAGMA page_size", [], |r| r.get(0))?;
        let ours: i64 = self.conn().query_row("PRAGMA page_size", [], |r| r.get(0))?;
        if theirs != ours {
            anyhow::bail!("the backup uses a {theirs}-byte page, this database uses {ours}");
        }
        // Brought up to this build's schema here, on the upload. Run after the
        // copy instead, a failed migration would leave the hub on a database it
        // could not use while reporting a failure to the panel -- the one
        // arrangement in which the restore has failed and the original data is
        // also gone.
        migrate(&candidate, version)?;
        // The migration lands in a -wal beside a backup taken from a running hub.
        // Checkpointed here so the copy below reads a single file.
        let _ = candidate.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));

        // Table names are not a schema. Pages are copied verbatim, so the columns
        // the file carries become the ones this hub's statements run against, and
        // eight correctly named tables holding the wrong columns pass every gate
        // above while leaving the database unusable.
        //
        // Compared against a database this build creates for itself, so there is
        // no second column list to keep in step with `SCHEMA`. Names are compared
        // as sets rather than as stored DDL: a migrated old backup reaches the
        // same columns through `ALTER TABLE`, whose text never matches a fresh
        // `CREATE TABLE`. Extra columns are ignored.
        let reference = Connection::open_in_memory()?;
        reference.execute_batch(SCHEMA)?;
        migrate(&reference, SCHEMA_VERSION)?;
        for table in TABLES {
            let want = columns_of(&reference, table)?;
            let got = columns_of(&candidate, table)?;
            let mut missing: Vec<&str> = want.difference(&got).map(String::as_str).collect();
            if !missing.is_empty() {
                missing.sort_unstable();
                anyhow::bail!("the file's {table} table is missing {}", missing.join(", "));
            }
        }
        Ok(())
    }

    /// Copies a checked backup over the live database page by page through
    /// SQLite's online backup API: the destination retains its file, permissions
    /// and journal mode, and a partial failure rolls back rather than leaving
    /// half a database behind.
    ///
    /// Call [`Db::check_backup`] first, as it is what brings `src` to this
    /// build's schema; the copy is then the last step and nothing after it can
    /// fail. Like the other two, this reads and writes the whole file and belongs
    /// off the runtime.
    pub fn restore_from(&self, src: &str) -> Result<()> {
        let mut conn = self.conn();
        conn.restore(rusqlite::MAIN_DB, src, None::<fn(rusqlite::backup::Progress)>)?;
        Ok(())
    }

    // ---- sessions ----

    pub fn create_session(
        &self,
        token_hash: &str,
        expires_at: i64,
        github_login: &str,
        ip: &str,
        user_agent: &str,
    ) -> Result<()> {
        self.conn().execute(
            "INSERT OR REPLACE INTO session (token_hash, expires_at, github_login, created_at, ip, user_agent, last_seen)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?4)",
            params![token_hash, expires_at, github_login, Utc::now().timestamp(), ip, user_agent],
        )?;
        Ok(())
    }

    /// Records that a session was used, at most once a minute.
    ///
    /// The throttle is the `WHERE` clause rather than bookkeeping in Rust: every admin
    /// request passes through here, and one write per request per session is a lot of
    /// fsync for a column nothing acts on. A minute is finer than anyone reads it.
    pub fn touch_session(&self, token_hash: &str, now: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE session SET last_seen = ?2 WHERE token_hash = ?1 AND ?2 - last_seen > 60",
            params![token_hash, now],
        )?;
        Ok(())
    }

    /// The login behind one session, for the panel's own header. `None` when the hash
    /// is not a live session, so a stale cookie asks for nothing.
    pub fn session_login(&self, token_hash: &str) -> Option<String> {
        self.conn()
            .query_row(
                "SELECT github_login FROM session WHERE token_hash = ?1 AND expires_at > ?2",
                params![token_hash, Utc::now().timestamp()],
                |r| r.get(0),
            )
            .ok()
    }

    pub fn session_valid(&self, token_hash: &str) -> bool {
        self.conn()
            .query_row(
                "SELECT 1 FROM session WHERE token_hash=?1 AND expires_at > ?2",
                params![token_hash, Utc::now().timestamp()],
                |_| Ok(()),
            )
            .optional()
            .ok()
            .flatten()
            .is_some()
    }

    /// Live sessions, newest first. Expired rows are filtered here rather than
    /// left to `expire_sessions`, which sweeps only once an hour.
    /// One row per live session, newest first: hash, the two timestamps, and what the
    /// login said about itself.
    #[allow(clippy::type_complexity)]
    pub fn sessions(&self) -> Result<Vec<(String, i64, String, i64, String, String, i64)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT token_hash, expires_at, github_login, created_at, ip, user_agent, last_seen
             FROM session WHERE expires_at > ?1 ORDER BY expires_at DESC",
        )?;
        let rows = stmt
            .query_map([Utc::now().timestamp()], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    pub fn drop_session(&self, token_hash: &str) -> Result<()> {
        self.conn().execute("DELETE FROM session WHERE token_hash=?1", [token_hash])?;
        Ok(())
    }

    /// Replaces the admin password hash and signs every session out, both or
    /// neither: a reset that stored the hash and then failed would report failure
    /// while the old password no longer works.
    pub fn replace_password(&self, hash: &str) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO setting (key, value) VALUES ('admin_password_hash', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [hash],
        )?;
        tx.execute("DELETE FROM session", [])?;
        tx.commit()?;
        Ok(())
    }

    /// Invalidates every login. Used after a restore, which would otherwise
    /// revive every session the backup holds.
    pub fn drop_all_sessions(&self) -> Result<()> {
        self.conn().execute("DELETE FROM session", [])?;
        Ok(())
    }

    pub fn expire_sessions(&self) -> Result<()> {
        self.conn().execute("DELETE FROM session WHERE expires_at <= ?1", [Utc::now().timestamp()])?;
        Ok(())
    }
}

/// Turns one finished bucket into a row per probe, stamped with the bucket's
/// start so every series lands on the same grid.
///
/// Median rather than mean: one SYN retransmit is tens of milliseconds and would
/// drag a mean, and it is the reading that is wrong rather than the link.
///
/// `latency` is null when an entire bucket timed out. `loss` is the percentage
/// that did, included only when non-zero -- a healthy day is 2,880 rows, and
/// `"loss":0` on each would add 29 kB of nothing. Rounded up, so that the absence
/// of a `loss` key means no timeouts occurred: truncating would report a bucket
/// that lost 1 of 180 as clean.
/// One bucket of **hourly** samples, written the way [`close_bucket`] writes one of
/// minute answers: the same band rule, the same ceiling percentage, and the median
/// from the shared [`Tally`] -- so a bucket spanning several hours is measured by
/// the same code as a bucket of single answers, which is what keeps a window drawn
/// from either layer agreeing.
fn close_tallies(out: &mut Vec<serde_json::Value>, tallies: std::collections::BTreeMap<i64, Tally>, ts: i64) {
    for (task, mut t) in tallies {
        let median = t.median();
        let mut row = serde_json::json!({"task_id": task, "ts": ts, "latency": median});
        if let (Some(lo), Some(hi)) = (t.lo, t.hi) {
            if hi > lo {
                row["band"] = serde_json::json!([lo, hi]);
            }
        }
        if t.lost > 0 {
            let total = t.answered() + t.lost;
            row["loss"] = ((100 * t.lost + total - 1) / total).into();
        }
        out.push(row);
    }
}

fn close_bucket(out: &mut Vec<serde_json::Value>, open: &mut Vec<(i64, Vec<i64>, i64)>, ts: i64) {
    // Not ordered here: the caller sorts the whole window by the panel's order,
    // because themes take their series, colours and legend from the order in
    // which probes first appear, and bucket by bucket that would be whichever
    // probe happened to answer in the window's partial first bucket.
    for (task, mut answered, lost) in open.drain(..) {
        answered.sort_unstable();
        let middle = match answered.len() {
            0 => None,
            n if n % 2 == 1 => Some(answered[n / 2]),
            n => Some((answered[n / 2 - 1] + answered[n / 2]) / 2),
        };
        let mut row = serde_json::json!({"task_id": task, "ts": ts, "latency": middle});
        // Only when the bucket actually varied. At the hour and six-hour windows a
        // bucket holds one sample, and a band would be a zero-height ribbon under
        // every line.
        if let (Some(lo), Some(hi)) = (answered.first(), answered.last()) {
            if hi > lo {
                row["band"] = serde_json::json!([lo, hi]);
            }
        }
        if lost > 0 {
            let total = answered.len() as i64 + lost;
            row["loss"] = ((100 * lost + total - 1) / total).into();
        }
        out.push(row);
    }
}

fn row_to_node(r: &rusqlite::Row<'_>) -> Node {
    let s = |i: &str| r.get::<_, String>(i).unwrap_or_default();
    let n = |i: &str| r.get::<_, i64>(i).unwrap_or(0);
    Node {
        id: n("id"),
        name: s("name"),
        group: s("group"),
        public: r.get::<_, bool>("public").unwrap_or(true),
        sort: n("sort"),
        price: r.get::<_, f64>("price").unwrap_or(0.0),
        currency: s("currency"),
        billing_cycle: s("billing_cycle"),
        expires_at: r.get::<_, Option<String>>("expires_at").unwrap_or(None),
        remark: s("remark"),
        private_remark: s("private_remark"),
        allow_remote_upgrade: n("allow_remote_upgrade") != 0,
        traffic_limit: n("traffic_limit"),
        traffic_mode: s("traffic_mode"),
        traffic_reset_day: n("traffic_reset_day") as u32,
        hostname: s("hostname"),
        os: s("os"),
        kernel: s("kernel"),
        arch: s("arch"),
        virt: s("virt"),
        cpu_name: s("cpu_name"),
        cpu_cores: n("cpu_cores"),
        mem_total: n("mem_total"),
        swap_total: n("swap_total"),
        disk_total: n("disk_total"),
        agent_version: s("agent_version"),
        ip: s("ip"),
        ipv4: s("ipv4"),
        ipv6: s("ipv6"),
        country: s("country"),
        country_pin: s("country_pin"),
        ipv4_pin: s("ipv4_pin"),
        ipv6_pin: s("ipv6_pin"),
        last_seen: n("last_seen"),
        notify: n("notify") != 0,
        down_since: n("down_since"),
        created_at: n("created_at"),
        token: s("token"),
    }
}

/// Start of the billing period containing `today`, given a reset day of month.
/// A reset day past the end of a short month lands on that month's last day.
pub fn period_start(today: NaiveDate, reset_day: u32) -> NaiveDate {
    let day = reset_day.clamp(1, 31);
    let clamped = |y: i32, m: u32| {
        let last =
            NaiveDate::from_ymd_opt(if m == 12 { y + 1 } else { y }, if m == 12 { 1 } else { m + 1 }, 1)
                .unwrap()
                .pred_opt()
                .unwrap()
                .day();
        NaiveDate::from_ymd_opt(y, m, day.min(last)).unwrap()
    };
    let this = clamped(today.year(), today.month());
    if today >= this {
        this
    } else if today.month() == 1 {
        clamped(today.year() - 1, 12)
    } else {
        clamped(today.year(), today.month() - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        Db::open(":memory:").unwrap()
    }

    /// PRAGMA settings are per connection, so a value read through any other
    /// handle proves nothing about the one the hub writes through.
    #[test]
    fn the_tuning_pragmas_reach_the_connection_the_hub_uses() {
        let db = db();
        let conn = db.conn();
        let read = |p: &str| conn.query_row(&format!("PRAGMA {p}"), [], |r| r.get::<_, i64>(0)).unwrap();
        assert_eq!(read("cache_size"), -8192, "8 MiB of page cache");
        assert_eq!(read("wal_autocheckpoint"), 256);
        assert_eq!(read("journal_size_limit"), 1_048_576);
        assert_eq!(read("busy_timeout"), 5_000);
    }

    /// A real file, since these three tests exist to exercise what happens to
    /// one. Removed by the test that created it.
    struct Scratch(String);

    impl Scratch {
        fn new() -> Self {
            Self(
                std::env::temp_dir()
                    .join(format!("monitor-test-{}.db", rand::random::<u64>()))
                    .to_string_lossy()
                    .into_owned(),
            )
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm", ".copy"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", self.0));
            }
        }
    }

    /// Backup and restore are the two operations that can lose every row in the
    /// database, so this exercises the whole path: take a copy, modify the live
    /// database, restore the copy, and confirm the change is gone.
    #[test]
    fn a_backup_restores_the_database_it_was_taken_from() {
        let scratch = Scratch::new();
        let copy = format!("{}.copy", scratch.0);
        let db = Db::open(&scratch.0).unwrap();
        let kept =
            db.create_node(&Node { name: "backed-up".into(), ..Default::default() }, "token-kept").unwrap();
        db.backup_into(&copy).unwrap();

        // Everything after the copy must disappear on restore, including a node
        // that reclaimed the deleted one's id.
        db.delete_node(kept).unwrap();
        db.create_node(&Node { name: "after".into(), ..Default::default() }, "token-after").unwrap();

        db.check_backup(&copy).unwrap();
        db.restore_from(&copy).unwrap();
        let back = db.nodes().unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!((back[0].name.as_str(), back[0].token.as_str()), ("backed-up", "token-kept"));
        assert!(db.node_by_token("token-after").unwrap().is_none(), "the row made after the copy is gone");

        // The connection remains the hub's: it can write, it is on the schema this
        // build expects, and it retains the journal mode the hub opened with --
        // the copy `VACUUM INTO` wrote is not in WAL mode.
        node(&db, 1);
        let conn = db.conn();
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0)).unwrap(),
            SCHEMA_VERSION
        );
        assert_eq!(conn.query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0)).unwrap(), "wal");
        drop(conn);
        let _ = std::fs::remove_file(&copy);
    }

    /// The upload behind restore is an externally supplied file. Each case here
    /// is a way for it not to be a hub backup, and every one must be caught
    /// before a single page is copied over live data.
    #[test]
    fn restore_refuses_anything_that_is_not_a_backup_of_this_hub() {
        let scratch = Scratch::new();
        let db = Db::open(&scratch.0).unwrap();
        let bad = format!("{}.copy", scratch.0);

        std::fs::write(&bad, b"this is not a database at all").unwrap();
        assert!(db.check_backup(&bad).is_err(), "not SQLite");

        let _ = std::fs::remove_file(&bad);
        let empty = Connection::open(&bad).unwrap();
        empty.execute_batch("CREATE TABLE unrelated (a)").unwrap();
        assert!(db.check_backup(&bad).is_err(), "SQLite, but not this schema");

        // A file carrying its own code where a table belongs: the restore copies
        // pages, so that schema would become the one the hub runs every statement
        // against.
        empty.execute_batch(&SCHEMA.replace("PRAGMA journal_mode = WAL;", "")).unwrap();
        empty
            .execute_batch(
                "DROP TABLE session; CREATE VIEW session AS SELECT 1 AS token_hash, 2 AS expires_at",
            )
            .unwrap();
        assert!(db.check_backup(&bad).is_err(), "a view where a table belongs");

        // Eight tables with the right names and none of the right columns. Every
        // gate above passes: it is a healthy SQLite file, it carries no view or
        // trigger, all eight names are present, it stamps itself with this build's
        // version and uses the same page size. Restoring copies pages, so those
        // columns would become the ones the hub runs every statement against,
        // leaving the panel reporting a failed restore over a database already
        // replaced.
        let _ = std::fs::remove_file(&bad);
        let shaped = Connection::open(&bad).unwrap();
        for table in TABLES {
            shaped.execute_batch(&format!("CREATE TABLE {table} (x TEXT)")).unwrap();
        }
        shaped.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}")).unwrap();
        // Which table fails first follows the order of TABLES and is incidental;
        // naming the table and the columns is what matters.
        let refused = db.check_backup(&bad).unwrap_err().to_string();
        assert!(refused.contains("table is missing"), "{refused}");

        // From a hub carrying a schema this build has never seen.
        let _ = std::fs::remove_file(&bad);
        let newer = Connection::open(&bad).unwrap();
        newer.execute_batch(SCHEMA).unwrap();
        newer.execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION + 1)).unwrap();
        assert!(db.check_backup(&bad).is_err(), "from a newer hub");

        newer.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}")).unwrap();
        db.check_backup(&bad).unwrap();
    }

    /// `oldest` is what the data page compares against the retention window, so it
    /// must span both pruned tables rather than whichever happens to have rows.
    #[test]
    fn stats_report_the_earliest_history_row_and_the_window_it_is_kept_for() {
        let scratch = Scratch::new();
        let db = Db::open(&scratch.0).unwrap();
        let id = node(&db, 1);
        let now = Utc::now().timestamp();

        assert_eq!(db.stats().unwrap()["oldest"], serde_json::Value::Null, "no history, no start");
        assert_eq!(
            db.stats().unwrap()["retention"],
            DEFAULT_RETENTION_DAYS,
            "an unset window is the default"
        );

        db.insert_metric(id, now - 3 * 86_400, &serde_json::json!({"cpu": 1.0})).unwrap();
        assert_eq!(db.stats().unwrap()["oldest"], now - 3 * 86_400);

        // Older, and in the other table: the earlier of the two prevails. The probe
        // must be assigned, or the result is not this node's to file.
        let task = db
            .save_ping_task(&PingTask {
                kind: None,
                id: 0,
                name: "p".into(),
                target: "1.1.1.1:443".into(),
                interval: 60,
                nodes: vec![id],
            })
            .unwrap();
        db.insert_ping(id, task, now - 9 * 86_400, 12).unwrap();
        assert_eq!(db.stats().unwrap()["oldest"], now - 9 * 86_400);

        // Above the ceiling reads as the ceiling -- and the hourly prune then
        // deletes the history past it, which is the one change in this release that
        // can take away history an operator had been keeping. Hence the startup
        // warning in `main`.
        db.set("retention_days", "9999").unwrap();
        assert_eq!(
            db.stats().unwrap()["retention"],
            MAX_RETENTION_DAYS,
            "a stored window is clamped to the ceiling"
        );
        assert_eq!(MAX_RETENTION_DAYS, 365, "and the ceiling is a year");
    }

    /// Deleted rows leave free pages behind; only a rebuild returns them to the
    /// filesystem, and in WAL mode only after the checkpoint.
    #[test]
    fn vacuum_gives_the_deleted_pages_back_to_the_filesystem() {
        let scratch = Scratch::new();
        let db = Db::open(&scratch.0).unwrap();
        let id = node(&db, 1);
        let now = Utc::now().timestamp();
        let sample = serde_json::json!({"cpu": 1.0, "mem_used": 1, "swap_used": 1, "disk_used": 1,
            "net_rx": 1, "net_tx": 1, "tcp": 1, "udp": 1, "procs": 1});
        // Past the detail window, so folding and pruning have something to do with
        // them: a minute row may only be dropped once its hour is in the summary
        // tables, and neither pass touches anything younger than the window.
        for i in 1..=20_000 {
            db.insert_metric(id, now - 8 * 86_400 - i, &sample).unwrap();
        }
        let _ = db.conn().query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
        let fat = on_disk(&scratch.0);
        db.roll_up(now, 7).unwrap();
        db.prune(7).unwrap();

        let freed = db.vacuum().unwrap();
        assert!(freed > 0, "a vacuum after deleting 20 000 rows has to return space");
        assert!(on_disk(&scratch.0) < fat);
        assert_eq!(db.stats().unwrap()["rows"]["metric"], 0);
        assert_eq!(db.nodes().unwrap().len(), 1, "vacuum keeps the rows that are left");
    }

    /// Reclaiming space rebuilds the whole database into a temporary file, so it
    /// needs a directory the process can write. The image built from scratch has
    /// none of SQLite's own choices, and under the systemd unit `/tmp` is a
    /// tmpfs charged to `MemoryMax`; both are answered by putting the copy
    /// beside the database, on the disk that already holds it.
    #[test]
    fn temporary_files_go_beside_the_database() {
        let scratch = Scratch::new();
        temp_files_beside(&scratch.0).unwrap();
        let read = || -> String {
            Connection::open_in_memory()
                .unwrap()
                .query_row("PRAGMA temp_store_directory", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(read(), std::path::Path::new(&scratch.0).parent().unwrap().to_string_lossy());

        // A bare file name names no directory to move to, and SQLite's own search
        // already ends at the working directory that holds it: leave the setting
        // alone rather than point the process at nothing.
        temp_files_beside("monitor.db").unwrap();
        assert_eq!(read(), std::path::Path::new(&scratch.0).parent().unwrap().to_string_lossy());

        // A directory that is not there is refused by SQLite rather than stored
        // silently -- and the refusal leaves the previous setting alone, since a
        // setting that would not work is worse than the default. This is the
        // path `main` turns into a warning.
        assert!(temp_files_beside("/nonexistent-xyz/monitor.db").is_err());
        assert_eq!(read(), std::path::Path::new(&scratch.0).parent().unwrap().to_string_lossy());

        // The setting is process-wide, which is why `main` makes it before the
        // first connection. With the copy on the database's own disk, reclaiming
        // space still does its job.
        let db = Db::open(&scratch.0).unwrap();
        let id = node(&db, 1);
        let now = Utc::now().timestamp();
        let sample = serde_json::json!({"cpu": 1.0, "mem_used": 1, "swap_used": 1, "disk_used": 1,
            "net_rx": 1, "net_tx": 1, "tcp": 1, "udp": 1, "procs": 1});
        for i in 1..=5_000 {
            db.insert_metric(id, now - i, &sample).unwrap();
        }
        let _ = db.conn().query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
        db.prune(0).unwrap();
        assert!(db.vacuum().unwrap() > 0, "the copy landed in a directory it fits in");
    }

    fn node(db: &Db, reset_day: u32) -> i64 {
        let token = format!("token-{}", rand::random::<u32>());
        db.create_node(&Node { name: "n".into(), traffic_reset_day: reset_day, ..Default::default() }, &token)
            .unwrap()
    }

    /// The country is derived from the address, so it must be dropped the moment
    /// the address no longer matches -- and only then, or every reconnect would
    /// spend an outbound request repeating a settled lookup.
    #[test]
    fn a_country_outlives_a_reconnect_and_dies_with_the_address_it_came_from() {
        let db = db();
        let id = node(&db, 1);
        let facts = serde_json::json!({"hostname": "h"});
        let save = |ip: &str| db.save_facts(id, &facts, ip, ip).unwrap();
        let stored = || db.node(id).unwrap().unwrap().country;

        assert!(save("198.51.100.4"), "a node with no country is owed a lookup");
        db.set_country(id, "US", "198.51.100.4").unwrap();
        assert!(!save("198.51.100.4"), "the same address asks nothing a second time");
        assert_eq!(stored(), "US");
        assert!(save("203.0.113.9"), "a new address is a new question");
        assert_eq!(stored(), "", "and the old answer no longer shows");
        assert!(db.country_owed(id, "203.0.113.9").unwrap(), "owed until an answer lands");
        assert!(!db.country_owed(id, "198.51.100.4").unwrap(), "nothing is owed for an address left behind");

        // A lookup issued for the old address, arriving after the move.
        db.set_country(id, "US", "198.51.100.4").unwrap();
        assert_eq!(stored(), "", "an answer about an address the node has left is dropped");
        db.set_country(id, "JP", "203.0.113.9").unwrap();
        assert_eq!(stored(), "JP", "the answer about the address it is at now lands");
        assert!(!db.country_owed(id, "203.0.113.9").unwrap());

        // The source, not the connection address, is what the country belongs to:
        // a proxy exit changing under a node with a public interface address
        // leaves the badge alone.
        assert!(!db.save_facts(id, &facts, "198.51.100.77", "203.0.113.9").unwrap());
        assert_eq!(stored(), "JP");
        // Nothing public to look up: no country, and none owed.
        assert!(!db.save_facts(id, &facts, "192.168.1.2", "").unwrap());
        assert_eq!(stored(), "");
    }

    /// A reboot: the first hello carries only the v6, the next one the v4 again.
    /// The detour spends the node's hourly lookup, so the address returned to
    /// must be answered from the row.
    #[test]
    fn a_country_returns_with_the_address_it_came_from() {
        let db = db();
        let id = node(&db, 1);
        let facts = serde_json::json!({});
        let save = |source: &str| db.save_facts(id, &facts, "198.51.100.4", source).unwrap();
        let stored = || db.node(id).unwrap().unwrap().country;
        let (v4, v6) = ("198.51.100.4", "2001:db8::5");

        save(v4);
        db.set_country(id, "RU", v4).unwrap();
        assert!(save(v6), "an address never answered is asked about");
        db.set_country(id, "US", v6).unwrap();
        assert!(!save(v4), "the address before it is not asked about again");
        assert_eq!(stored(), "RU");
        assert!(!save(v6), "nor, after that, the one in between");
        assert_eq!(stored(), "US");

        // Addresses never answered pass through without displacing the last answer.
        assert!(save("203.0.113.9"));
        assert!(save("203.0.113.10"));
        assert!(!save(v6));
        assert_eq!(stored(), "US");
    }

    #[test]
    fn traffic_survives_a_reboot_instead_of_resetting() {
        let db = db();
        let id = node(&db, 1);

        // The first report only establishes the baseline.
        let t = db.accumulate(id, "boot-a", Some((5_000, 3_000))).unwrap();
        assert_eq!((t.total_rx, t.total_tx), (0, 0));

        let t = db.accumulate(id, "boot-a", Some((9_000, 6_000))).unwrap();
        assert_eq!((t.total_rx, t.total_tx), (4_000, 3_000));

        // Reboot: a new boot_id with counters restarting near zero. The total must
        // not fall back to the fresh value, and the 700 bytes moved before the
        // first report are not booked, nothing having measured them.
        let t = db.accumulate(id, "boot-b", Some((700, 400))).unwrap();
        assert_eq!((t.total_rx, t.total_tx), (4_000, 3_000), "a reboot must not reset the total");

        // Counting resumes from the new baseline.
        let t = db.accumulate(id, "boot-b", Some((1_700, 900))).unwrap();
        assert_eq!((t.total_rx, t.total_tx), (5_000, 3_500));
        assert_eq!((t.month_rx, t.month_tx), (5_000, 3_500));
    }

    /// One install command pasted onto a second machine: both agents answer for
    /// the same node and evict each other, so the hub sees two boot_ids
    /// alternating, each with its own lifetime counter. Booking those would add
    /// roughly 180 GB per swap to a total that only increases.
    #[test]
    fn two_machines_sharing_one_token_cannot_inflate_the_total() {
        let db = db();
        let id = node(&db, 1);
        let (a, b) = (100_000_000_000, 80_000_000_000); // two lifetime counters

        db.accumulate(id, "boot-a", Some((a, a))).unwrap();
        let t = db.accumulate(id, "boot-a", Some((a + 1_000, a + 1_000))).unwrap();
        assert_eq!(t.total_rx, 1_000, "the real machine's own traffic still counts");

        // Every swap presents a boot_id with no baseline, so every swap books
        // nothing.
        for round in 0..3 {
            db.accumulate(id, "boot-b", Some((b + round, b + round))).unwrap();
            db.accumulate(id, "boot-a", Some((a + 1_000 + round, a + 1_000 + round))).unwrap();
        }
        let t = db.all_traffic()[&id].clone();
        assert!(t.total_rx < 10_000, "six swaps booked {} bytes, not a lifetime counter", t.total_rx);
    }

    #[test]
    fn a_shrinking_reading_re_aligns_instead_of_re_counting_history() {
        let db = db();
        let id = node(&db, 1);
        db.accumulate(id, "boot-a", Some((10_000, 10_000))).unwrap();
        let t = db.accumulate(id, "boot-a", Some((12_000, 12_000))).unwrap();
        assert_eq!((t.total_rx, t.total_tx), (2_000, 2_000));

        // The same boot with a reduced reading: an interface included in the sum
        // has gone, so this is the remainder of the machine's history rather than
        // new bytes.
        let t = db.accumulate(id, "boot-a", Some((500, 500))).unwrap();
        assert_eq!((t.total_rx, t.total_tx), (2_000, 2_000));

        // Aligned to the smaller baseline, counting resumes from there.
        let t = db.accumulate(id, "boot-a", Some((900, 900))).unwrap();
        assert_eq!((t.total_rx, t.total_tx), (2_400, 2_400));

        // A new boot realigns identically, for the same reason: it has no baseline
        // either.
        let t = db.accumulate(id, "boot-b", Some((300, 300))).unwrap();
        assert_eq!((t.total_rx, t.total_tx), (2_400, 2_400));

        // One direction shrinking does not deprive the other of its increment.
        let t = db.accumulate(id, "boot-b", Some((100, 900))).unwrap();
        assert_eq!((t.total_rx, t.total_tx), (2_400, 3_000));
    }

    /// The two counters that restart on their own schedules, against a total that
    /// never does. Each derives from its own stored date, so a rollover must leave
    /// the other untouched.
    #[test]
    fn day_and_month_restart_independently_while_the_total_keeps_climbing() {
        let db = db();
        let id = node(&db, 1);
        db.accumulate(id, "boot-a", Some((0, 0))).unwrap();
        let t = db.accumulate(id, "boot-a", Some((8_000, 4_000))).unwrap();
        assert_eq!((t.day_rx, t.day_tx), (8_000, 4_000));
        assert_eq!((t.month_rx, t.month_tx), (8_000, 4_000));

        // Midnight passes, forced through the stored date the rollover reads.
        db.conn().execute("UPDATE traffic SET day_start='1999-01-01' WHERE node_id=?1", [id]).unwrap();
        let t = db.accumulate(id, "boot-a", Some((9_500, 4_600))).unwrap();
        assert_eq!((t.day_rx, t.day_tx), (1_500, 600), "a new day counts only this report's delta");
        assert_eq!(t.month_rx, 9_500, "the month is not a day");
        assert_eq!(t.total_rx, 9_500, "and the total is neither");

        // The billing period then rolls over, partway through that same day.
        db.conn().execute("UPDATE traffic SET month_start='1999-01-01' WHERE node_id=?1", [id]).unwrap();
        let t = db.accumulate(id, "boot-a", Some((10_000, 4_700))).unwrap();
        assert_eq!((t.month_rx, t.month_tx), (500, 100), "a new period counts only this report's delta");
        assert_eq!((t.day_rx, t.day_tx), (2_000, 700), "the day carries on across a billing rollover");
        assert_eq!(t.total_rx, 10_000, "lifetime total is untouched by either rollover");
    }

    /// **日切时先存快照、再重置** ✓ —— 顺序反了就会把"还没过完的新一天"当成昨天写进去 ✗，
    /// 而那种错在报表上只表现为"少一天/多一天"，几乎看不出来 ✓（与主键那一条同一个理由 ✓）。
    #[test]
    fn a_rollover_snapshots_the_day_that_just_ended() {
        let db = db();
        let id = node(&db, 1);
        db.accumulate(id, "boot-a", Some((0, 0))).unwrap();
        db.accumulate(id, "boot-a", Some((8_000, 4_000))).unwrap();
        // 还没跨日：**不该**有任何快照 —— 否则报表里会多出一个"今天"的行 ✗。
        let n: i64 = db.conn().query_row("SELECT COUNT(*) FROM traffic_day", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0, "没跨日就不该有快照");

        // 跨日：这次上报先把**刚结束那天**存下来，然后才重置这一天的计数。
        db.conn().execute("UPDATE traffic SET day_start='1999-01-01' WHERE node_id=?1", [id]).unwrap();
        db.accumulate(id, "boot-a", Some((9_500, 4_600))).unwrap();
        let (date, rx, tx): (String, i64, i64) = db
            .conn()
            .query_row("SELECT date, rx, tx FROM traffic_day WHERE node_id=?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!(date, "1999-01-01", "快照属于**刚结束**的那一天，不是今天");
        assert_eq!((rx, tx), (8_000, 4_000), "值必须是**重置前**的总量");

        // 再走一次同样的路径：**不能**写出第二行（重复快照 ⇒ 周报翻倍 ✗）。
        db.accumulate(id, "boot-a", Some((9_900, 4_800))).unwrap();
        let n: i64 = db.conn().query_row("SELECT COUNT(*) FROM traffic_day", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1, "同一天只该有一行");
    }

    /// The other half of the rollover: the counters restart on the node's next
    /// report, so a node silent since before a boundary still holds the previous
    /// period's bytes on disk. The read side must not return those.
    #[test]
    fn a_node_that_went_quiet_before_a_boundary_reads_as_zero_this_period() {
        let db = db();
        let id = node(&db, 1);
        db.accumulate(id, "boot-a", Some((0, 0))).unwrap();
        db.accumulate(id, "boot-a", Some((8_000, 4_000))).unwrap();
        assert_eq!(db.all_traffic()[&id].day_rx, 8_000, "still today, so it still counts");

        // Offline across both boundaries, with no report to restart either.
        db.conn()
            .execute(
                "UPDATE traffic SET day_start='1999-01-01', month_start='1999-01-01' WHERE node_id=?1",
                [id],
            )
            .unwrap();
        let t = db.all_traffic()[&id].clone();
        assert_eq!((t.day_rx, t.day_tx), (0, 0), "yesterday's bytes are not today's");
        assert_eq!((t.month_rx, t.month_tx), (0, 0), "last period's bytes are not this period's");
        assert_eq!(t.month_start, period_start(Local::now().date_naive(), 1).to_string());
        assert_eq!((t.total_rx, t.total_tx), (8_000, 4_000), "the lifetime total never resets");
    }

    #[test]
    fn period_start_handles_short_months_and_wraparound() {
        let d = |y, m, day| NaiveDate::from_ymd_opt(y, m, day).unwrap();
        // Reset on the 15th, today the 20th: the current month.
        assert_eq!(period_start(d(2026, 3, 20), 15), d(2026, 3, 15));
        // The reset day itself counts as the start of the new period.
        assert_eq!(period_start(d(2026, 3, 15), 15), d(2026, 3, 15));
        // Before the reset day the period began in the previous month.
        assert_eq!(period_start(d(2026, 3, 10), 15), d(2026, 2, 15));
        // January rolls back into the previous year.
        assert_eq!(period_start(d(2026, 1, 10), 15), d(2025, 12, 15));
        // Day 31 in February clamps to the 28th; 2028 is a leap year.
        assert_eq!(period_start(d(2026, 2, 28), 31), d(2026, 2, 28));
        assert_eq!(period_start(d(2028, 2, 29), 31), d(2028, 2, 29));
    }

    #[test]
    fn deleting_a_node_takes_its_data_with_it() {
        let db = db();
        let id = node(&db, 1);
        let probe = |nodes| PingTask {
            kind: None,
            id: 0,
            name: "cm".into(),
            target: "1.1.1.1:443".into(),
            interval: 60,
            nodes,
        };
        let task = db.save_ping_task(&probe(vec![id])).unwrap();
        db.accumulate(id, "b", Some((10, 10))).unwrap();
        db.insert_metric(id, 1, &serde_json::json!({"cpu": 1.0})).unwrap();
        db.insert_ping(id, task, 1, 42).unwrap();
        db.delete_node(id).unwrap();
        assert!(db.node(id).unwrap().is_none());
        assert_eq!(db.metrics(id, 0, 60).unwrap().len(), 0);
        assert!(!db.all_traffic().contains_key(&id));

        // `ping_record` has no foreign key to cascade through, and SQLite reassigns
        // the deleted id to the next node created: without the sweep in
        // `delete_node` the new machine would draw the old one's chart.
        let fresh = node(&db, 1);
        assert_eq!(fresh, id, "the id is reused, which is what makes this reachable");
        db.save_ping_task(&PingTask { kind: None, id: task, nodes: vec![fresh], ..probe(vec![]) }).unwrap();
        assert!(db.ping_records(fresh, 0, 60).unwrap().0.is_empty(), "and it starts with no history");
    }

    /// The mirror of the sweep above, on the other key of the same table. SQLite
    /// reuses a deleted probe's id as well, and the chart selects on a node's
    /// assignments, so the removed probe's samples would reappear under the new
    /// probe's name with its timeouts folded into the new loss figure.
    #[test]
    fn deleting_a_probe_takes_its_history_with_it() {
        let db = db();
        let id = node(&db, 1);
        let probe = |name: &str| PingTask {
            kind: None,
            id: 0,
            name: name.into(),
            target: "1.1.1.1:443".into(),
            interval: 60,
            nodes: vec![id],
        };
        let old = db.save_ping_task(&probe("tokyo")).unwrap();
        db.insert_ping(id, old, 1, 999).unwrap();
        db.delete_ping_task(old).unwrap();

        let fresh = db.save_ping_task(&probe("singapore")).unwrap();
        assert_eq!(fresh, old, "the id is reused, which is what makes this reachable");
        assert!(db.ping_records(id, 0, 60).unwrap().0.is_empty(), "and it starts with no history");
    }

    /// Counted directly from the table rather than read back through
    /// `ping_records`: that query filters on the node's assignments, so a row
    /// written under a probe it does not have is invisible to it. An assertion
    /// made through it therefore could not fail for the write this test exists to
    /// prevent.
    #[test]
    fn a_result_for_a_probe_this_node_does_not_have_is_not_stored() {
        let db = db();
        let mine = node(&db, 1);
        let other = node(&db, 1);
        let rows = || db.conn().query_row("SELECT COUNT(*) FROM ping_record", [], |r| r.get::<_, i64>(0));
        let task = db
            .save_ping_task(&PingTask {
                kind: None,
                id: 0,
                name: "p".into(),
                target: "1.1.1.1:443".into(),
                interval: 60,
                nodes: vec![mine],
            })
            .unwrap();

        db.insert_ping(mine, task, 1, 42).unwrap();
        assert_eq!(rows().unwrap(), 1, "the node the probe is assigned to files its own result");

        // A probe that exists but belongs to another node, and ids naming no probe
        // at all: what a node token can place on the wire.
        db.insert_ping(other, task, 1, 42).unwrap();
        for invented in [7, 999_999, i64::from(i32::MAX) + 1] {
            db.insert_ping(mine, invented, 1, 42).unwrap();
        }
        assert_eq!(rows().unwrap(), 1, "nothing else reaches the table");

        // Deleting the probe also ends its node's results, so one already in flight
        // cannot land after the sweep and be inherited by the next probe to take
        // the id.
        db.delete_ping_task(task).unwrap();
        db.insert_ping(mine, task, 2, 42).unwrap();
        assert_eq!(rows().unwrap(), 0, "a late result for a deleted probe is dropped");
    }

    /// The strings in a `hello` come from an unvouched machine, and six of them go
    /// straight into the frame pushed to the public page every two seconds, so
    /// their length cannot be the node's to choose. `api` enforces the same bound
    /// on the one string `agent_register` accepts.
    #[test]
    fn facts_from_an_unvouched_machine_cannot_choose_their_own_length() {
        let db = db();
        let id = node(&db, 1);
        db.save_facts(id, &serde_json::json!({"os": "A".repeat(10_000), "hostname": "x\u{7}y"}), "ip", "")
            .unwrap();
        let stored = db.node(id).unwrap().unwrap();
        assert_eq!(stored.os.chars().count(), 128);
        assert_eq!(stored.hostname, "xy", "control characters break the panel's rows");
    }

    /// A correction must survive the node's return. `all_traffic` gates the month
    /// figures on the period they were written for, and `accumulate` restarts the
    /// counter when the stored period is stale, so a correction left under the
    /// previous period would read as zero and then be discarded.
    #[test]
    fn a_month_correction_is_stamped_with_the_period_it_was_made_in() {
        let db = db();
        let id = node(&db, 1);
        db.accumulate(id, "boot-a", Some((0, 0))).unwrap();
        // A node silent since before its reset day still holds the old period.
        db.conn().execute("UPDATE traffic SET month_start='1999-01-01' WHERE node_id=?1", [id]).unwrap();

        db.set_traffic(
            id,
            &TrafficPatch {
                total_rx: Some(4_000),
                total_tx: Some(2_000),
                month_rx: Some(300),
                month_tx: Some(100),
            },
        )
        .unwrap();
        let t = db.all_traffic().remove(&id).unwrap();
        assert_eq!((t.month_rx, t.month_tx), (300, 100), "the correction reads back as this period's");

        let t = db.accumulate(id, "boot-a", Some((500, 50))).unwrap();
        assert_eq!((t.month_rx, t.month_tx), (800, 150), "and the next report adds to it");
        assert_eq!((t.total_rx, t.total_tx), (4_500, 2_050));
    }

    #[test]
    fn partial_edits_keep_other_settings_and_live_counters() {
        let db = db();
        let id = node(&db, 1);
        let patch = |v| serde_json::from_value::<NodePatch>(v).unwrap();
        db.update_node(
            id,
            &patch(serde_json::json!({"public":false,"remark":"private","expires_at":"2030-01-01"})),
        )
        .unwrap();
        db.update_node(id, &patch(serde_json::json!({"price":20}))).unwrap();
        let n = db.node(id).unwrap().unwrap();
        assert!(!n.public);
        assert_eq!(n.remark, "private");
        assert_eq!(n.expires_at.as_deref(), Some("2030-01-01"));
        db.update_node(id, &patch(serde_json::json!({"price":0,"expires_at":null}))).unwrap();
        let n = db.node(id).unwrap().unwrap();
        assert_eq!(n.price, 0.0);
        assert_eq!(n.expires_at, None);

        db.accumulate(id, "boot", Some((0, 0))).unwrap();
        db.accumulate(id, "boot", Some((120_000, 10_000))).unwrap();
        db.set_traffic(id, &TrafficPatch { month_tx: Some(3_000), ..Default::default() }).unwrap();
        let t = db.all_traffic().remove(&id).unwrap();
        assert_eq!((t.total_rx, t.total_tx, t.month_rx, t.month_tx), (120_000, 10_000, 120_000, 3_000));

        db.update_node(id, &patch(serde_json::json!({"traffic_reset_day":2}))).unwrap();
        db.set_traffic(id, &TrafficPatch { month_rx: Some(7_000), ..Default::default() }).unwrap();
        let t = db.all_traffic().remove(&id).unwrap();
        assert_eq!((t.month_rx, t.month_tx), (7_000, 0));
        // Correcting only a lifetime total cannot revive the previous month's
        // bytes.
        db.conn()
            .execute("UPDATE traffic SET month_start='1999-01-01',month_tx=999 WHERE node_id=?1", [id])
            .unwrap();
        db.set_traffic(id, &TrafficPatch { total_rx: Some(130_000), ..Default::default() }).unwrap();
        assert_eq!(db.all_traffic()[&id].month_tx, 0);
    }

    #[test]
    fn a_token_is_readable_and_rotation_retires_the_old_one() {
        let db = db();
        let id = db.create_node(&Node { name: "n".into(), ..Default::default() }, "first-token").unwrap();

        // Readable, so the panel can display the install command without issuing a
        // new token.
        assert_eq!(db.node(id).unwrap().unwrap().token, "first-token");
        assert_eq!(db.node_by_token("first-token").unwrap(), Some(id));

        db.reset_token(id, "second-token").unwrap();
        assert_eq!(db.node(id).unwrap().unwrap().token, "second-token");
        assert_eq!(db.node_by_token("second-token").unwrap(), Some(id));
        assert_eq!(db.node_by_token("first-token").unwrap(), None, "the old token stops working");
    }

    #[test]
    fn nodes_can_be_reordered_atomically() {
        let db = db();
        let (a, b, c) = (node(&db, 1), node(&db, 1), node(&db, 1));
        let order = || db.nodes().unwrap().iter().map(|n| n.id).collect::<Vec<_>>();
        db.reorder_nodes(&[c, a, b]).unwrap();
        assert_eq!(order(), vec![c, a, b]);

        // Every rejected input leaves the existing order intact. The partial list
        // matters most: a stale tab would otherwise renumber around a node it never
        // saw.
        assert!(db.reorder_nodes(&[a, a, c]).is_err(), "duplicates");
        assert!(db.reorder_nodes(&[a, b]).is_err(), "a node left out");
        assert!(db.reorder_nodes(&[a, b, 9999]).is_err(), "an id that is not a node");
        assert_eq!(order(), vec![c, a, b]);
        // A node added afterwards goes to the end rather than wherever sort 0
        // places it.
        let d = node(&db, 1);
        assert_eq!(db.nodes().unwrap().iter().map(|n| n.id).collect::<Vec<_>>(), vec![c, a, b, d]);
    }

    /// Themes draw probes in the order they first appear in the rows, so the rows
    /// follow the panel's order even when a later probe alone answered in the
    /// window's first bucket -- and a new probe starts at the end rather than on
    /// top of the order it was added to.
    #[test]
    fn a_probe_chart_follows_the_panel_order() {
        let db = db();
        let id = node(&db, 1);
        let probe = |name: &str| {
            db.save_ping_task(&PingTask {
                kind: None,
                id: 0,
                name: name.into(),
                target: "1.1.1.1:443".into(),
                interval: 60,
                nodes: vec![id],
            })
            .unwrap()
        };
        let (a, b) = (probe("a"), probe("b"));
        db.reorder_ping_tasks(&[b, a]).unwrap();
        let c = probe("c");
        let listed: Vec<_> = db.ping_tasks().unwrap().iter().map(|t| t.id).collect();
        assert_eq!(listed, vec![b, a, c], "a new probe starts at the end");

        for (task, ts, latency) in [(a, 0, 10), (a, 60, 10), (b, 60, 20), (c, 60, 30)] {
            db.insert_ping(id, task, ts, latency).unwrap();
        }
        let rows = db.ping_records(id, 0, 60).unwrap().0;
        let drawn: Vec<_> =
            rows.iter().map(|r| (r["task_id"].as_i64().unwrap(), r["ts"].as_i64().unwrap())).collect();
        assert_eq!(drawn, vec![(b, 60), (a, 0), (a, 60), (c, 60)]);
    }

    #[test]
    fn prune_drops_history_but_never_traffic_totals() {
        let db = db();
        let id = node(&db, 1);
        db.accumulate(id, "b", Some((100, 100))).unwrap();
        db.accumulate(id, "b", Some((900, 900))).unwrap();
        let now = Utc::now().timestamp();
        db.insert_metric(id, now - 2 * 86_400, &serde_json::json!({"cpu": 1.0})).unwrap();
        db.insert_metric(id, now, &serde_json::json!({"cpu": 2.0})).unwrap();

        // A minute row outlives its window until its hour is in the summary tables,
        // so with no rollup behind it nothing may be dropped at all.
        assert_eq!(db.prune(1).unwrap(), 0, "nothing folded, so nothing may go");
        assert_eq!(db.metrics(id, 0, 60).unwrap().len(), 2);

        // Folded, it is dropped, and the totals it fed are untouched.
        db.roll_up(now, 1).unwrap();
        db.prune(1).unwrap();
        assert_eq!(db.metrics(id, 0, 60).unwrap().len(), 1);
        assert_eq!(db.all_traffic()[&id].total_rx, 800);
    }

    /// Swap reaches the chart through the same bucket as memory, so it has to be
    /// the bucket's mean rather than a row from inside it: a chart that read one
    /// report per bucket would draw a step the node never took, and a node with
    /// no swap configured must read as zero rather than as a series the theme
    /// cannot draw.
    #[test]
    fn history_reports_the_mean_swap_of_a_bucket_and_zero_as_zero() {
        let db = db();
        let id = node(&db, 1);
        // Three reports in one minute, no one of which carries the mean.
        for (ts, swap_used) in [(60, 100), (61, 200), (62, 300)] {
            db.insert_metric(id, ts, &serde_json::json!({"swap_used": swap_used})).unwrap();
        }
        let row = &db.metrics(id, 0, 60).unwrap()[0];
        assert_eq!(row["mem_used"], 0, "the rest of the row is unaffected");
        assert_eq!(row["swap_used"], 200, "the bucket reports its mean, not a sample");

        let other = node(&db, 1);
        db.insert_metric(other, 60, &serde_json::json!({"swap_used": 0})).unwrap();
        assert_eq!(db.metrics(other, 0, 60).unwrap()[0]["swap_used"], 0, "zero, not null");
    }

    /// A bucket peaks where its busiest minute did, and a minute written before
    /// the peak column existed counts as its own mean rather than as a zero that
    /// would drag the window's peak down.
    #[test]
    fn history_peaks_where_its_busiest_minute_did() {
        let db = db();
        let id = node(&db, 1);
        // One bucket: a quiet minute, then one whose busiest second ran four
        // times its own average.
        db.insert_metric(id, 10, &serde_json::json!({"net_rx": 0, "net_tx": 3_000})).unwrap();
        db.insert_metric(
            id,
            70,
            &serde_json::json!({"net_rx": 1_000, "net_rx_max": 4_000, "net_tx": 1_000, "net_tx_max": 2_000}),
        )
        .unwrap();

        let row = &db.metrics(id, 0, 120).unwrap()[0];
        assert_eq!(row["net_rx"], 500, "the bucket is its mean, not one row of it");
        assert_eq!(row["net_rx_max"], 4_000, "the bucket peaks where its busiest minute did");
        assert_eq!(row["net_tx_max"], 3_000, "a row without a peak counts as its own mean, not as zero");
    }

    /// The CPU panel draws load from history, so the bucket has to report the
    /// mean of its own minute rather than the first or last row of it -- the same
    /// reason `cpu` is averaged. A bucket no report gave a load must read as
    /// null: the theme draws that as a gap, while a zero would draw the machine
    /// idle for a minute nobody measured.
    #[test]
    fn history_reports_the_mean_load_of_a_bucket_and_no_load_as_null() {
        let db = db();
        let id = node(&db, 1);
        // Three rows in one minute, no one of which is the mean.
        for (ts, load1) in [(60, 1.0), (61, 2.0), (62, 3.0)] {
            db.insert_metric(id, ts, &serde_json::json!({"cpu": 1.0, "load1": load1})).unwrap();
        }
        let row = &db.metrics(id, 0, 60).unwrap()[0];
        assert_eq!(row["load1"], 2.0, "the bucket reports its mean, not a sample");
        assert_eq!(row["cpu"], 1.0, "the rest of the row is unaffected");

        // A row with no `load1` -- what a minute no report gave a load leaves --
        // stores NULL, which averages over the bucket as null rather than zero.
        let other = node(&db, 1);
        db.insert_metric(other, 60, &serde_json::json!({"cpu": 1.0})).unwrap();
        let row = &db.metrics(other, 0, 60).unwrap()[0];
        assert_eq!(row["load1"], serde_json::Value::Null, "no sample is not a sample of zero");

        // Mixed: the mean covers only the rows that have one, so a silent minute
        // must not pull a measured one down.
        let mixed = node(&db, 1);
        db.insert_metric(mixed, 60, &serde_json::json!({"load1": 2.0})).unwrap();
        db.insert_metric(mixed, 61, &serde_json::json!({"cpu": 1.0})).unwrap();
        assert_eq!(db.metrics(mixed, 0, 60).unwrap()[0]["load1"], 2.0, "NULL is not a zero sample");
    }

    /// The rekeying in `open()`: rows must survive it, and the chart's query must
    /// emerge able to seek. A migration that leaves every row on the old key fails
    /// silently, and stays silent while the query it exists for scans a node's
    /// entire history.
    #[test]
    fn rekeying_ping_record_keeps_the_rows_and_lets_the_chart_query_seek() {
        let file = std::env::temp_dir().join(format!("monitor-rekey-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&file);
        let path = file.to_str().unwrap();

        // A database as an older hub left it.
        let old = Connection::open(path).unwrap();
        old.execute_batch(
            "CREATE TABLE ping_record (
               node_id INTEGER NOT NULL, task_id INTEGER NOT NULL,
               ts INTEGER NOT NULL, latency INTEGER NOT NULL,
               PRIMARY KEY (node_id, task_id, ts)
             ) WITHOUT ROWID;
             INSERT INTO ping_record VALUES (1,7,100,12),(1,8,100,34),(1,7,200,56),(2,7,100,78);",
        )
        .unwrap();
        drop(old);

        let db = Db::open(path).unwrap();
        let conn = db.conn();
        let rows: Vec<(i64, i64, i64, i64)> = conn
            .prepare("SELECT node_id, task_id, ts, latency FROM ping_record ORDER BY node_id, ts, task_id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(rows, vec![(1, 7, 100, 12), (1, 8, 100, 34), (1, 7, 200, 56), (2, 7, 100, 78)]);

        // Without the timestamp second in the key the plan stops at `node_id=?`
        // and scans everything beneath it, and the fold in `ping_records` requires
        // rows in time order, which only the seek provides without a sorter.
        let plan: String = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {PING_ROWS}"))
            .unwrap()
            .query_map(params![1, 0, 60], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .join(" | ");
        assert!(plan.contains("node_id=? AND ts>?"), "the window has to be a seek, not a scan: {plan}");
        assert!(!plan.contains("ORDER BY"), "the time order has to come off the key, not a sorter: {plan}");

        // Opening again must not rebuild a table that is already correct.
        drop(conn);
        drop(db);
        assert!(Db::open(path).is_ok());
        let _ = std::fs::remove_file(&file);
    }

    /// `metric.load1` across the version history: `migrate_to_2` drops the
    /// `NOT NULL` column a v1-era hub carried, and `migrate_to_8` puts it back as
    /// the nullable one this build writes. The column being `NOT NULL` with no
    /// default is why migration 2 is mandatory -- without it every insert this
    /// build made against that file violated the constraint -- and the ordering
    /// is why the re-add has to come after it rather than before.
    ///
    /// The old row survives; the load it carried does not, since dropping the
    /// column discarded it and the re-add has nothing to refill it with.
    #[test]
    fn dropping_load1_keeps_the_history_and_lets_new_rows_in() {
        let file = std::env::temp_dir().join(format!("monitor-load1-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&file);
        let path = file.to_str().unwrap();

        // A database as a hub predating this build left it: one metric row
        // carrying a load average, stamped with the schema version of the time.
        let old = Connection::open(path).unwrap();
        old.execute_batch(
            "CREATE TABLE metric (
               node_id INTEGER NOT NULL, ts INTEGER NOT NULL,
               cpu REAL NOT NULL, load1 REAL NOT NULL,
               mem_used INTEGER NOT NULL, swap_used INTEGER NOT NULL, disk_used INTEGER NOT NULL,
               net_rx INTEGER NOT NULL, net_tx INTEGER NOT NULL,
               tcp INTEGER NOT NULL, udp INTEGER NOT NULL, procs INTEGER NOT NULL,
               PRIMARY KEY (node_id, ts)
             ) WITHOUT ROWID;
             INSERT INTO metric VALUES (1,60,12.5,0.75,100,0,0,0,0,0,0,0);
             PRAGMA user_version = 1;",
        )
        .unwrap();
        drop(old);

        let db = Db::open(path).unwrap();
        // Migration 2 dropped it and migration 8 re-added it, nullable this time.
        assert!(schema_mentions(&db.conn(), "metric", "load1").unwrap(), "the column has to be back");
        // The row remains, along with everything else it carried -- but not the
        // load, which the drop discarded and the re-add did not restore.
        let kept = &db.metrics(1, 0, 60).unwrap()[0];
        assert_eq!((kept["ts"].as_i64(), kept["cpu"].as_f64()), (Some(60), Some(12.5)));
        assert_eq!(kept["load1"], serde_json::Value::Null, "the dropped value is gone, not refilled");
        // The shape this build inserts now fits the table. The loadless row is
        // what proves the re-added column is nullable: the `NOT NULL` one
        // migration 2 removed would have rejected it.
        db.insert_metric(1, 120, &serde_json::json!({"cpu": 2.0, "load1": 0.5})).unwrap();
        db.insert_metric(1, 180, &serde_json::json!({"cpu": 3.0})).unwrap();
        let rows = db.metrics(1, 0, 60).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1]["load1"], 0.5, "a new row carries the load it was given");
        assert_eq!(rows[2]["load1"], serde_json::Value::Null, "and one without a load stores NULL");

        // Opening again must not attempt to drop a column already replaced, nor
        // add one that is present.
        drop(db);
        assert!(Db::open(path).is_ok());
        let _ = std::fs::remove_file(&file);
    }

    /// A database on the immediately preceding schema, whose `metric` table has
    /// no `load1` at all. Migration 8 adds it, and every row already there reads
    /// back NULL rather than a zero the chart would draw.
    #[test]
    fn a_v7_database_gains_a_nullable_load1_with_its_old_rows_null() {
        let file = std::env::temp_dir().join(format!("monitor-v7-load1-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&file);
        let path = file.to_str().unwrap();

        // The metric table as a v7 hub created it: no load1.
        let old = Connection::open(path).unwrap();
        old.execute_batch(
            "CREATE TABLE metric (
               node_id INTEGER NOT NULL, ts INTEGER NOT NULL,
               cpu REAL NOT NULL,
               mem_used INTEGER NOT NULL, swap_used INTEGER NOT NULL, disk_used INTEGER NOT NULL,
               net_rx INTEGER NOT NULL, net_tx INTEGER NOT NULL,
               tcp INTEGER NOT NULL, udp INTEGER NOT NULL, procs INTEGER NOT NULL,
               PRIMARY KEY (node_id, ts)
             ) WITHOUT ROWID;
             INSERT INTO metric VALUES (1,60,12.5,100,0,0,0,0,0,0,0);
             PRAGMA user_version = 7;",
        )
        .unwrap();
        drop(old);

        let db = Db::open(path).unwrap();
        assert!(schema_mentions(&db.conn(), "metric", "load1").unwrap(), "migration 8 adds the column");
        // The row predates the column, so it reads back null rather than zero.
        let kept = &db.metrics(1, 0, 60).unwrap()[0];
        assert_eq!(kept["cpu"], 12.5, "the old row is untouched");
        assert_eq!(kept["load1"], serde_json::Value::Null, "a row written before the column has no load");
        // A row written after it carries a load, and one without a load still
        // fits, which a `NOT NULL` column would not allow.
        db.insert_metric(1, 120, &serde_json::json!({"cpu": 2.0, "load1": 1.25})).unwrap();
        db.insert_metric(1, 180, &serde_json::json!({"cpu": 3.0})).unwrap();
        assert_eq!(db.metrics(1, 120, 60).unwrap()[0]["load1"], 1.25);
        assert_eq!(db.metrics(1, 180, 60).unwrap()[0]["load1"], serde_json::Value::Null);

        // And the migration is idempotent across a reopen.
        drop(db);
        let db = Db::open(path).unwrap();
        assert!(schema_mentions(&db.conn(), "metric", "load1").unwrap());
        drop(db);
        let _ = std::fs::remove_file(&file);
    }

    /// The six columns this build adds, as a hub before it lacked them. Named
    /// once so both upgrade tests below agree on what the change is.
    const ADDED: [&str; 6] =
        ["country_ip", "country_prev_ip", "country_prev", "country_pin", "ipv4_pin", "ipv6_pin"];

    /// A file this build's schema is written in, with the six columns taken back
    /// out and the version wound back: a database the previous release left.
    fn a_v10_file(path: &str, rows: &str) {
        let db = Db::open(path).unwrap();
        for column in ADDED {
            db.conn().execute(&format!("ALTER TABLE node DROP COLUMN {column}"), []).unwrap();
        }
        // Opening the file above writes this build's schema, so the tables a later
        // migration adds are present and have to be taken back out: a v10 file had
        // no such table, and a fixture that keeps them passes a missing migration.
        for table in HOUR_TABLES {
            db.conn().execute(&format!("DROP TABLE {table}"), []).unwrap();
        }
        db.conn()
            .execute_batch(&format!(
                "INSERT INTO node (id, name, token, ip, country, created_at) {rows};
                                     PRAGMA user_version = 10;"
            ))
            .unwrap();
    }

    /// Every row of each table as one string, so two databases can be compared
    /// whole: a row count says nothing about the values a migration rewrote.
    fn dump(conn: &Connection, tables: &[&str]) -> Vec<(String, String)> {
        tables
            .iter()
            .map(|t| {
                let mut stmt = conn.prepare(&format!("SELECT * FROM {t} ORDER BY 1")).unwrap();
                let width = stmt.column_count();
                let rows: Vec<String> = stmt
                    .query_map([], |r| {
                        let cells: Vec<String> =
                            (0..width).map(|i| format!("{:?}", r.get_ref(i).unwrap())).collect();
                        Ok(cells.join("|"))
                    })
                    .unwrap()
                    .map(Result::unwrap)
                    .collect();
                ((*t).to_owned(), rows.join("\n"))
            })
            .collect()
    }

    /// One hour, folded: the counts the average is weighted by, the peaks, the
    /// median of the answers and the losses, all against figures computed here by
    /// hand rather than read back from the same code.
    #[test]
    fn folding_an_hour_sums_what_its_minute_rows_hold() {
        let db = db();
        let id = node(&db, 1);
        let probe = db
            .save_ping_task(&PingTask {
                kind: None,
                id: 0,
                name: "p".into(),
                target: "1.1.1.1:443".into(),
                interval: 60,
                nodes: vec![id],
            })
            .unwrap();
        // A fixed hour in the past, so nothing about the current minute matters.
        let hour = 472_224 * 3_600;
        for (i, cpu) in [1.0, 2.0, 3.0, 4.0].iter().enumerate() {
            db.insert_metric(
                id,
                hour + i as i64 * 60,
                &serde_json::json!({"cpu": cpu, "mem_used": 100, "swap_used": 10, "disk_used": 5,
                    "net_rx": 1_000, "net_tx": 500, "net_rx_max": 9_000, "load1": 0.5}),
            )
            .unwrap();
        }
        // Three answers and a timeout: -1 is stored for a probe that did not reply.
        for (i, latency) in [10, 20, -1, 40].iter().enumerate() {
            db.insert_ping(id, probe, hour + i as i64 * 60, *latency).unwrap();
        }

        db.roll_up(hour + 4 * 3_600, 30).unwrap();

        let conn = db.conn();
        let (minutes, cpu, swap, load, rx_max): (i64, f64, i64, Option<f64>, i64) = conn
            .query_row(
                "SELECT minutes, cpu, swap_used, load1, net_rx_max FROM metric_hour WHERE node_id=?1 AND ts=?2",
                params![id, hour],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(minutes, 4, "each minute row counts once");
        assert_eq!(cpu, 2.5, "the mean of 1, 2, 3 and 4");
        assert_eq!(swap, 10);
        assert_eq!(load, Some(0.5));
        assert_eq!(rx_max, 9_000, "the peak is the largest of the means and the peaks");
        let (answered, lost, median, lo, hi): (i64, i64, i64, i64, i64) = conn
            .query_row(
                "SELECT answered, lost, latency, lo, hi FROM ping_hour WHERE node_id=?1 AND task_id=?2 AND ts=?3",
                params![id, probe, hour],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!((answered, lost), (3, 1), "a timeout is a loss, not an answer");
        assert_eq!(median, 20, "the middle of 10, 20 and 40");
        assert_eq!((lo, hi), (10, 40));
    }

    /// A window reaching across the watermark draws what the minute rows alone would
    /// draw. Six hours are folded and the last three are not, so the seam falls in
    /// the middle of the window; `metrics` reads **every** minute row, folded or not,
    /// and is therefore the independent baseline.
    ///
    /// Two hours and a day per point are the cases that matter: a bucket then spans
    /// both halves, and it is the `GROUP BY` in `metrics_window` that has to merge
    /// them rather than let the same `ts` out twice.
    #[test]
    fn a_window_across_the_watermark_draws_what_the_minutes_would() {
        let db = db();
        let id = node(&db, 1);
        let start = 472_224 * 3_600;
        for h in 0..6i64 {
            for m in 0..(60 - h * 7) {
                db.insert_metric(
                    id,
                    start + h * 3_600 + m * 60,
                    &serde_json::json!({"cpu": h as f64 + m as f64 / 100.0, "mem_used": 1_000 + m,
                        "swap_used": 10, "disk_used": 5, "net_rx": 100 + m, "net_tx": 50,
                        "load1": 0.25, "net_rx_max": 900 + m, "net_tx_max": 400}),
                )
                .unwrap();
            }
        }
        // Three hours folded, three left as minute rows: the seam is mid-window.
        for h in 0..3i64 {
            let when = start + h * 3_600 + 7_200;
            db.roll_up(when, 30).unwrap();
            db.fold_hour(start + h * 3_600).unwrap();
        }
        assert_eq!(count(&db, "metric_hour"), 3, "one node, three hours folded");

        let until = start + 6 * 3_600;
        for step in [3_600i64, 7_200, 86_400] {
            let window = db.metrics_window(id, start, step).unwrap();
            let minutes = db.metrics(id, start, step).unwrap();
            assert_eq!(window.len(), minutes.len(), "step {step}: the same buckets");
            for (w, m) in window.iter().zip(&minutes) {
                assert_eq!(w["ts"], m["ts"], "step {step}");
                assert_eq!(w["net_rx_max"], m["net_rx_max"], "step {step}: the peak is exact");
                let (got, want) = (w["cpu"].as_f64().unwrap(), m["cpu"].as_f64().unwrap());
                assert!((got - want).abs() < 1e-9, "step {step}: cpu {got} vs {want}");
                for key in ["mem_used", "swap_used", "disk_used", "net_rx", "net_tx"] {
                    let d = (w[key].as_i64().unwrap() - m[key].as_i64().unwrap()).abs();
                    assert!(d <= 1, "step {step}: {key} differs by {d}");
                }
                let (got, want) = (w["load1"].as_f64().unwrap(), m["load1"].as_f64().unwrap());
                assert!((got - want).abs() < 1e-9, "step {step}: load1 {got} vs {want}");
            }
        }
        let _ = until;
    }

    /// The hourly tier's probe series answer what the minute rows do, hour per point
    /// and two hours per point. The median is the weighted one, so the second case is
    /// where that shows: `Tally` counts each hour's median once per answer it stands
    /// for, not once per hour.
    #[test]
    fn the_hourly_tier_draws_the_probes_the_minute_rows_would() {
        let db = db();
        let id = node(&db, 1);
        let probe = db
            .save_ping_task(&PingTask {
                kind: None,
                id: 0,
                name: "p".into(),
                target: "1.1.1.1:443".into(),
                interval: 60,
                nodes: vec![id],
            })
            .unwrap();
        let start = 472_224 * 3_600;
        // Six hours of different lengths, answers that differ per hour, and losses.
        for h in 0..6i64 {
            for m in 0..(60 - h * 7) {
                let latency = if m % 13 == 0 { -1 } else { 20 + h * 10 + m % 5 };
                db.insert_ping(id, probe, start + h * 3_600 + m * 60, latency).unwrap();
            }
        }
        let until = start + 6 * 3_600;
        db.roll_up(until + 7_200, 30).unwrap();

        let key = |r: &serde_json::Value| (r["ts"].as_i64().unwrap(), r["task_id"].as_i64().unwrap());
        let hourly = db.ping_records_hourly(id, start, until, 3_600).unwrap();
        let (minutes, _) = db.ping_records(id, start, 3_600).unwrap();
        let by: std::collections::HashMap<_, _> = minutes.iter().map(|m| (key(m), m.clone())).collect();
        assert_eq!(hourly.len(), 6, "one point per hour");
        for h in &hourly {
            assert_eq!(
                h,
                by.get(&key(h)).expect("the minute path has this point"),
                "an hour per point is exact"
            );
        }

        // Two hours a point: the band and the loss are exact, the median is the
        // weighted one and falls inside its own band.
        let hourly = db.ping_records_hourly(id, start, until, 7_200).unwrap();
        let (minutes, _) = db.ping_records(id, start, 7_200).unwrap();
        let by: std::collections::HashMap<_, _> = minutes.iter().map(|m| (key(m), m.clone())).collect();
        assert_eq!(hourly.len(), 3, "two hours a point over six hours");
        for h in &hourly {
            let m = by.get(&key(h)).expect("the minute path has this point");
            assert_eq!((&h["band"], &h["loss"]), (&m["band"], &m["loss"]), "exact either way");
            let median = h["latency"].as_i64().unwrap();
            let band = h["band"].as_array().map(|b| (b[0].as_i64().unwrap(), b[1].as_i64().unwrap()));
            assert!(band.is_none_or(|(lo, hi)| (lo..=hi).contains(&median)), "{h}");
        }

        // The rank rule itself, computed here from the `ping_hour` rows rather than
        // read back from the API: the median is the value at rank (answered+1)/2 by
        // weight, counted once per answer an hour stands for. An unweighted median --
        // one vote per hour, whatever it holds -- lands elsewhere as soon as the
        // hours hold different numbers of answers, which is what this pins. (It is
        // *not* the median over the underlying minutes: medians do not compose, which
        // is why a point spanning hours is an approximation and the band sits beside
        // it.)
        assert_eq!(hourly.len(), 3, "two hours a point over six hours");
        for (i, first) in [0i64, 2, 4].iter().enumerate() {
            let conn = db.conn();
            let mut stmt = conn
                .prepare(
                    "SELECT latency, answered FROM ping_hour
                      WHERE node_id=?1 AND task_id=?2 AND ts>=?3 AND ts<?4",
                )
                .unwrap();
            let mut samples: Vec<(i64, i64)> = stmt
                .query_map(params![id, probe, start + first * 3_600, start + (first + 2) * 3_600], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            samples.sort_unstable();
            let total: i64 = samples.iter().map(|s| s.1).sum();
            let at = |rank: i64| {
                let mut seen = 0;
                samples
                    .iter()
                    .find(|s| {
                        seen += s.1;
                        seen >= rank
                    })
                    .map(|s| s.0)
                    .unwrap()
            };
            let want = (at((total + 1) / 2) + at(total / 2 + 1)) / 2;
            assert_eq!(
                hourly[i]["latency"].as_i64().unwrap(),
                want,
                "bucket {i}: the median is the value at rank by weight"
            );
        }
    }

    /// The hourly tier's resource series answer what the minute rows they were
    /// folded from answer, hour per point. The fold's own arithmetic is covered by
    /// `folding_an_hour_sums_what_its_minute_rows_hold`; this is the **reading** of
    /// it, which is what a window wider than the detail window does.
    ///
    /// Six hours with different numbers of minutes, so a plain average of the
    /// hourly means would differ from the weighted one.
    #[test]
    fn the_hourly_tier_draws_what_the_minute_rows_would() {
        let db = db();
        let id = node(&db, 1);
        let start = 472_224 * 3_600;
        for h in 0..6i64 {
            for m in 0..(60 - h * 7) {
                db.insert_metric(
                    id,
                    start + h * 3_600 + m * 60,
                    &serde_json::json!({"cpu": h as f64 + m as f64 / 100.0, "mem_used": 1_000 + m,
                        "swap_used": 10, "disk_used": 5, "net_rx": 100 + m, "net_tx": 50,
                        "load1": 0.25, "net_rx_max": 900 + m, "net_tx_max": 400}),
                )
                .unwrap();
            }
        }
        let until = start + 6 * 3_600;
        db.roll_up(until + 7_200, 30).unwrap();

        let hourly = db.metrics_hourly(id, start, until, 3_600).unwrap();
        let minutes = db.metrics(id, start, 3_600).unwrap();
        assert_eq!(hourly.len(), 6, "one point per hour");
        assert_eq!(hourly.len(), minutes.len());
        for (h, m) in hourly.iter().zip(&minutes) {
            assert_eq!(h["ts"], m["ts"]);
            assert_eq!(h["net_rx_max"], m["net_rx_max"], "the peak is the peak either way");
            assert_eq!(h["net_tx_max"], m["net_tx_max"]);
            assert!(
                (h["cpu"].as_f64().unwrap() - m["cpu"].as_f64().unwrap()).abs() < 1e-9,
                "{} vs {}",
                h["cpu"],
                m["cpu"]
            );
            for key in ["mem_used", "swap_used", "disk_used", "net_rx", "net_tx"] {
                assert_eq!(h[key], m[key], "{key}: the same minutes, weighted the same");
            }
            assert_eq!(h["load1"], m["load1"]);
        }

        // And at two hours a point, which is the case that actually guards the
        // weighting: one point then covers hours of different lengths, so a plain
        // average of the hourly means differs from the mean of the minutes. (At one
        // hour a point it cannot: a bucket holds a single hour row, whose value is
        // already that hour's mean. Verified by mutation -- summing instead of
        // weighting left this test green until this case existed.)
        //
        // The integer columns are allowed one unit: the hour's own mean is truncated
        // when it is stored, so two hours can add up to a fraction more or less than
        // the mean over both.
        let hourly = db.metrics_hourly(id, start, until, 7_200).unwrap();
        let minutes = db.metrics(id, start, 7_200).unwrap();
        assert_eq!(hourly.len(), 3, "two hours a point over six hours");
        assert_eq!(hourly.len(), minutes.len());
        for (h, m) in hourly.iter().zip(&minutes) {
            assert_eq!(h["ts"], m["ts"]);
            assert_eq!(h["net_rx_max"], m["net_rx_max"], "the peak is exact");
            assert_eq!(h["net_tx_max"], m["net_tx_max"]);
            let (got, want) = (h["cpu"].as_f64().unwrap(), m["cpu"].as_f64().unwrap());
            assert!((got - want).abs() < 1e-9, "cpu {got} vs {want}");
            for key in ["mem_used", "swap_used", "disk_used", "net_rx", "net_tx"] {
                let d = (h[key].as_i64().unwrap() - m[key].as_i64().unwrap()).abs();
                assert!(d <= 1, "{key} differs by {d}");
            }
        }
    }

    /// The stitched probe reader agrees with the minute one on a database with
    /// nothing folded -- where it must degenerate to exactly that -- and still
    /// carries the fields the panel reads: latency, band and loss.
    #[test]
    fn the_stitched_probe_reader_matches_the_minute_one_when_nothing_is_folded() {
        let app_db = db();
        let id = node(&app_db, 1);
        let _ = app_db
            .save_ping_task(&PingTask {
                kind: None,
                id: 0,
                name: "p".into(),
                target: "1.1.1.1:443".into(),
                interval: 60,
                nodes: vec![id],
            })
            .unwrap();
        let task = app_db.conn().query_row("SELECT id FROM ping_task", [], |r| r.get(0)).unwrap();
        let base = 472_224 * 3_600;
        for (i, latency) in [30, -1, -1, -1, 12, 44].into_iter().enumerate() {
            app_db.insert_ping(id, task, base + i as i64 * 10, latency).unwrap();
        }

        let (stitched, window) = app_db.ping_window(id, base, 120).unwrap();
        let (minutes, minute_window) = app_db.ping_records(id, base, 120).unwrap();
        assert_eq!(stitched, minutes, "nothing is folded, so this is the minute path");
        assert_eq!(window, minute_window, "and the same window loss");
        // All six land in the one 120-second bucket: three answer (30, 12, 44) and
        // three do not, so the median is 30 and the loss is 50%.
        assert_eq!(stitched[0]["loss"], 50, "three of six did not answer");
        assert_eq!(stitched[0]["latency"], 30, "the median of 30, 12 and 44");
    }

    /// A task that says nothing about how it probes is a TCP probe: that is what every
    /// task was before the field existed, and what an old hub, an old panel and an old
    /// backup all send. Empty counts as absent, because the panel sends an empty string
    /// for a field it does not fill.
    #[test]
    fn a_task_without_a_kind_probes_tcp() {
        let bare: PingTask =
            serde_json::from_value(serde_json::json!({"name": "p", "target": "1.1.1.1:443"})).unwrap();
        assert_eq!(bare.probe_kind(), "tcp", "absent means a handshake");
        let empty: PingTask =
            serde_json::from_value(serde_json::json!({"name": "p", "target": "1.1.1.1", "kind": ""}))
                .unwrap();
        assert_eq!(empty.probe_kind(), "tcp", "an empty string is a field nobody filled in");
        let echo: PingTask =
            serde_json::from_value(serde_json::json!({"name": "p", "target": "1.1.1.1", "kind": "icmp"}))
                .unwrap();
        assert_eq!(echo.probe_kind(), "icmp");
        // And a task that says nothing serialises without the field at all: an
        // existing payload must not change by a byte because this exists.
        assert!(!serde_json::to_string(&bare).unwrap().contains("kind"));
    }

    /// A file that really lacks `kind` gains it, and the row that pre-dates it is a
    /// TCP probe afterwards.
    ///
    /// Deliberately drops the column first: the other fixtures are built with the
    /// current schema and only pretend to be older, so they cannot show that the step
    /// does anything -- deleting `migrate_to_13` would leave them green. This one
    /// fails without it.
    /// 升级路径的闸：一个「13 版」的库（缺 private_remark）迁移后必须有那一列、且戳到 14。
    ///
    /// 这条是踩出来的：migrate_to_14 曾被塞进 if from < 13 块里，而盖章在块外 ——
    /// 于是 from == 13（升级上来的生产库）跳过迁移却被打上 14，永久缺列。
    /// 全新库的 SCHEMA 本来就带那一列，add_column 会当作「重复列」忽略，别的测试发现不了。
    /// **升级路径的闸（1.9.12 的教训照搬）**：一个"14 版"的库（缺 allow_remote_upgrade）迁移后
    /// 必须有那一列、且戳到 15。全新库的 SCHEMA 本来就带它，`add_column` 会当「重复列」忽略，
    /// 所以只有这种"从上一版升上来"的测试才碰得到真正的升级路径。
    /// **升级路径的闸**（照 1.9.12 那条搬）：一个"15 版"的库（没有 traffic_day）迁移后
    /// 必须有这张表、且戳到 16 ✓。全新库的 SCHEMA 本来就带上它 ✗ —— 所以
    /// **只有这种"从上一版升上来"的测试才碰得到真正的升级路径** ✓。
    /// 周期 → 日期区间：四种周期的**边界**各钉一条 ✓（跨月、跨年、跨季、周一起算 ✓）。
    /// 这类错只表现为"报表少一天/多一天"，没人看得出来 ✓ —— 所以必须靠测试 ✓。
    #[test]
    fn periods_cover_the_last_full_one_and_its_predecessor() {
        use chrono::NaiveDate;
        let d = |y, m, day| NaiveDate::from_ymd_opt(y, m, day).unwrap();

        // 日报：昨天整天；上一周期 = 前天整天。
        assert_eq!(
            Period::Day.last_full(d(2026, 3, 15)),
            ("2026-03-14".into(), "2026-03-15".into(), "2026-03-13".into(), "2026-03-14".into())
        );
        // 周报：周一发的报告应当覆盖**上一个周一至周日**（周一起算 ✓）。
        assert_eq!(
            Period::Week.last_full(d(2026, 3, 16)), // 2026-03-16 是周一
            ("2026-03-09".into(), "2026-03-16".into(), "2026-03-02".into(), "2026-03-09".into())
        );
        // 月报：3 月 1 日发 → 覆盖 2 月整月（**闰年 2 月是 29 天** ✓，上一周期与它等长 ✓）。
        assert_eq!(
            Period::Month.last_full(d(2024, 3, 1)),
            ("2024-02-01".into(), "2024-03-01".into(), "2024-01-01".into(), "2024-02-01".into())
        );
        // 跨年：1 月 1 日发月报 → 上一个周期是去年 12 月 ✓。
        assert_eq!(
            Period::Month.last_full(d(2026, 1, 1)),
            ("2025-12-01".into(), "2026-01-01".into(), "2025-11-01".into(), "2025-12-01".into())
        );
        // 季报：4 月 1 日发 → 覆盖 Q1（1–3 月 ✓）；上一周期 = 去年 Q4 ✓（跨年 ✓）。
        assert_eq!(
            Period::Quarter.last_full(d(2026, 4, 1)),
            ("2026-01-01".into(), "2026-04-01".into(), "2025-10-01".into(), "2026-01-01".into())
        );
        // 半年：4 月里发 → 上一个完整半年是**去年下半年**（7/1 → 今年 1/1 ✓，跨年 ✓）；
        // 对比期 = 去年上半年 ✓。
        assert_eq!(
            Period::Half.last_full(d(2026, 4, 10)),
            ("2025-07-01".into(), "2026-01-01".into(), "2025-01-01".into(), "2025-07-01".into())
        );
        // 半年：10 月里发 → 上一个完整半年是**今年上半年**（1/1 → 7/1 ✓）；对比期 = 去年下半年 ✓。
        assert_eq!(
            Period::Half.last_full(d(2026, 10, 2)),
            ("2026-01-01".into(), "2026-07-01".into(), "2025-07-01".into(), "2026-01-01".into())
        );
        // 半年边界：正好 7 月 1 日发 → 上一个完整半年是今年上半年 ✓（不能算成"刚过去的那一天" ✗）。
        assert_eq!(
            Period::Half.last_full(d(2026, 7, 1)),
            ("2026-01-01".into(), "2026-07-01".into(), "2025-07-01".into(), "2026-01-01".into())
        );
        // 年报：3 月里发 → 上一个完整年是**去年全年** ✓；对比期 = 前年 ✓。
        assert_eq!(
            Period::Year.last_full(d(2026, 3, 5)),
            ("2025-01-01".into(), "2026-01-01".into(), "2024-01-01".into(), "2025-01-01".into())
        );
        // 年报边界：正好 1 月 1 日发 → 上一个完整年仍是去年 ✓（不是"今年" ✗）。
        assert_eq!(
            Period::Year.last_full(d(2026, 1, 1)),
            ("2025-01-01".into(), "2026-01-01".into(), "2024-01-01".into(), "2025-01-01".into())
        );
        // 闰年不影响年与半年的长度 ✓（区间是**日期** ✓ —— 天数由日历决定 ✓，2024 是闰年 ✓）。
        assert_eq!(
            Period::Year.last_full(d(2025, 6, 1)),
            ("2024-01-01".into(), "2025-01-01".into(), "2023-01-01".into(), "2024-01-01".into())
        );

        // 季中：5 月里发 → 仍然是"上一个完整季度"（Q1 ✓），不是"进行中的 Q2" ✗。
        assert_eq!(
            Period::Quarter.last_full(d(2026, 5, 20)),
            ("2026-01-01".into(), "2026-04-01".into(), "2025-10-01".into(), "2026-01-01".into())
        );
    }

    /// 快照求和：区间**含首不含尾** ✓ · 按总量排序 ✓ · 区间内没有快照的节点不出现 ✓。
    /// 边界那一格（`to` 那天）算不算，正是报表"多一天/少一天"的来源 ✓ —— 必须钉住 ✓。
    #[test]
    fn traffic_sums_cover_the_half_open_range_only() {
        let db = db();
        let (a, b) = (node(&db, 1), node(&db, 2));
        let name_of = |id: i64| -> String {
            db.conn().query_row("SELECT name FROM node WHERE id=?1", [id], |r| r.get(0)).unwrap()
        };
        let put = |id: i64, date: &str, rx: i64, tx: i64| {
            db.conn()
                .execute(
                    "INSERT OR REPLACE INTO traffic_day (node_id, date, rx, tx) VALUES (?1,?2,?3,?4)",
                    rusqlite::params![id, date, rx, tx],
                )
                .unwrap();
        };
        put(a, "2026-03-01", 1_000, 100);
        put(a, "2026-03-02", 2_000, 200);
        put(a, "2026-03-03", 4_000, 400); // `to` 那天：**不算** ✓
        put(b, "2026-03-02", 500, 50);
        put(b, "2026-02-28", 9_999, 999); // 区间之前：不算 ✓

        let rows = db.traffic_sums("2026-03-01", "2026-03-03");
        assert_eq!(rows.len(), 2, "区间内的两台都在");
        // 3 月 1–2 日：a = 3000 / 300 ✓（3 月 3 日被排除 ✓）；b = 500 / 50 ✓
        assert_eq!((rows[0].0, rows[0].2, rows[0].3), (a, 3_000, 300), "含首不含尾：`to` 那天不算");
        assert_eq!(rows[0].1, name_of(a), "名字一起带出来");
        // 按总量（rx+tx）从多到少：a(3300) 在 b(550) 前面 ✓
        assert_eq!((rows[1].0, rows[1].2, rows[1].3), (b, 500, 50));
        // 区间内没有任何快照的节点**不出现** ✓（而不是出现一行 0 —— 那会让"哪些机器在跑流量"
        // 这件事在报告里失真 ✓）。
        let had = db.traffic_sums("2020-01-01", "2020-01-02");
        assert!(had.is_empty(), "空区间应当是空的");
    }

    /// 落库：有值就写值 ✓、**没有就写 NULL 而不是空串** ✓✓ —— 后者会被读成"这个国家是空的" ✓。
    #[test]
    fn quality_lands_as_null_when_it_is_unknown() {
        let db = db();
        let id = node(&db, 1);
        // 先放一份"查到了"的 ✓
        db.save_quality(
            id,
            &crate::geo::Quality {
                country: Some("HK".into()),
                city: Some("Hong Kong".into()),
                latitude: Some(22.3193),
                asn: Some(906),
                org: Some("DMIT".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let row = |c: &str| -> Option<String> {
            // 一律按**文本**读 ✓（`CAST` 一下 ✓）—— `q_asn` 是 INTEGER ✓，
            // 而"它是不是 NULL"这件事与类型无关 ✓，测试只想问那个 ✓。
            db.conn()
                .query_row(&format!("SELECT CAST({c} AS TEXT) FROM node WHERE id=?1"), [id], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(row("q_country").as_deref(), Some("HK"));
        assert_eq!(row("q_city").as_deref(), Some("Hong Kong"));
        assert_eq!(row("q_org").as_deref(), Some("DMIT"));
        assert_eq!(row("q_asn").as_deref(), Some("906"));
        // 没查到的两列 **必须是 NULL** ✓（不是空串 ✗）
        assert_eq!(row("q_subdivision"), None, "查不到 ⇒ NULL");
        assert_eq!(row("q_time_zone"), None, "查不到 ⇒ NULL");

        // 再落一份"什么都没查到"的 ✓ ⇒ 八列**全部回到 NULL** ✓（覆盖旧值 ✓）
        db.save_quality(id, &crate::geo::Quality::default()).unwrap();
        for c in [
            "q_country",
            "q_city",
            "q_subdivision",
            "q_latitude",
            "q_longitude",
            "q_time_zone",
            "q_asn",
            "q_org",
        ] {
            assert_eq!(row(c), None, "{c} 应当回到 NULL");
        }
    }

    /// **升级路径的闸**：一个"16 版"的库（没有那八列）迁移后必须有它们、且戳到 17 ✓。
    /// 全新库的 SCHEMA 本来就带 ⇒ 只有这种"从上一版升上来"的测试碰得到真正的升级路径 ✓。
    #[test]
    fn migrating_from_16_adds_the_node_quality_columns() {
        let db = Db::open(":memory:").unwrap();
        let conn = db.conn();
        const COLS: [&str; 8] = [
            "q_country",
            "q_city",
            "q_subdivision",
            "q_latitude",
            "q_longitude",
            "q_time_zone",
            "q_asn",
            "q_org",
        ];
        for col in COLS {
            conn.execute(&format!("ALTER TABLE node DROP COLUMN {col}"), []).unwrap();
        }
        conn.execute("PRAGMA user_version = 16", []).unwrap();
        let cols = |c: &rusqlite::Connection| -> Vec<String> {
            c.prepare("PRAGMA table_info(node)")
                .unwrap()
                .query_map([], |r| r.get::<_, String>(1))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        assert!(!cols(&conn).contains(&"q_asn".to_string()), "前提：列已删掉");
        migrate(&conn, 16).unwrap();
        let after = cols(&conn);
        for col in COLS {
            assert!(after.contains(&col.to_string()), "迁移后必须有 {col}");
        }
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0)).unwrap(),
            SCHEMA_VERSION
        );
        // **必须可空** ✓：查不到就是 NULL ✓ —— 若被写成 NOT NULL，插入节点时就会失败 ✓，
        // 而那种错会在"新增一台节点"时才炸 ✗，与这里看起来毫无关系 ✓。
        let notnull: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('node') WHERE name LIKE 'q_%' AND \"notnull\"=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(notnull, 0, "八个质量列都必须可空");
    }

    #[test]
    fn migrating_from_15_adds_the_traffic_day_table() {
        let db = Db::open(":memory:").unwrap();
        let conn = db.conn();
        conn.execute("DROP TABLE traffic_day", []).unwrap();
        conn.execute("PRAGMA user_version = 15", []).unwrap();
        let has = |c: &rusqlite::Connection| -> bool {
            c.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name='traffic_day'")
                .unwrap()
                .exists([])
                .unwrap()
        };
        assert!(!has(&conn), "前提：表已删掉");
        migrate(&conn, 15).unwrap();
        assert!(has(&conn), "迁移后必须有 traffic_day");
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0)).unwrap(),
            SCHEMA_VERSION
        );
        // 表形也要对：漏了主键会让同一天写两次、周报翻倍。
        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(traffic_day)")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(cols, vec!["node_id", "date", "rx", "tx"], "列与顺序都要对");
    }

    #[test]
    fn migrating_from_14_adds_the_allow_remote_upgrade_column() {
        let db = Db::open(":memory:").unwrap();
        let conn = db.conn();
        conn.execute("ALTER TABLE node DROP COLUMN allow_remote_upgrade", []).unwrap();
        conn.execute("PRAGMA user_version = 14", []).unwrap();
        let cols = |c: &rusqlite::Connection| -> Vec<String> {
            c.prepare("PRAGMA table_info(node)")
                .unwrap()
                .query_map([], |r| r.get::<_, String>(1))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        assert!(!cols(&conn).contains(&"allow_remote_upgrade".to_string()), "前提：列已删掉");
        migrate(&conn, 14).unwrap();
        assert!(cols(&conn).contains(&"allow_remote_upgrade".to_string()), "迁移后必须有该列");
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0)).unwrap(),
            SCHEMA_VERSION
        );
        // 缺省必须是 0（"仅手动升级"）—— 老 agent 不上报这个字段，绝不能让它们**默认变成可远程升级**。
        let n: i64 =
            conn.query_row("SELECT allow_remote_upgrade FROM node LIMIT 1", [], |r| r.get(0)).unwrap_or(0);
        assert_eq!(n, 0, "缺省必须是关");
    }

    #[test]
    fn migrating_from_13_adds_the_private_remark_column() {
        let db = Db::open(":memory:").unwrap();
        let conn = db.conn();
        conn.execute("ALTER TABLE node DROP COLUMN private_remark", []).unwrap();
        conn.execute("PRAGMA user_version = 13", []).unwrap();
        let cols = |c: &rusqlite::Connection| -> Vec<String> {
            c.prepare("PRAGMA table_info(node)")
                .unwrap()
                .query_map([], |r| r.get::<_, String>(1))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        assert!(!cols(&conn).contains(&"private_remark".to_string()), "前提：列已删掉");
        migrate(&conn, 13).unwrap();
        assert!(cols(&conn).contains(&"private_remark".to_string()), "迁移后必须有该列");
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0)).unwrap(),
            SCHEMA_VERSION
        );
    }

    #[test]
    fn a_file_without_the_kind_column_gains_it_as_tcp() {
        let db = db();
        let id = node(&db, 1);
        db.save_ping_task(&PingTask {
            id: 0,
            name: "p".into(),
            target: "1.1.1.1:443".into(),
            interval: 60,
            nodes: vec![id],
            kind: None,
        })
        .unwrap();
        let conn = db.conn();
        conn.execute("ALTER TABLE ping_task DROP COLUMN kind", []).unwrap();
        migrate(&conn, 12).unwrap();
        let kind: String = conn.query_row("SELECT kind FROM ping_task LIMIT 1", [], |r| r.get(0)).unwrap();
        assert_eq!(kind, "tcp", "a task from before the column is a handshake");
        // Twice-safe: running the step again changes nothing.
        migrate(&conn, 12).unwrap();
        assert_eq!(
            conn.query_row::<i64, _, _>(
                "SELECT COUNT(*) FROM pragma_table_info('ping_task') WHERE name='kind'",
                [],
                |r| r.get(0)
            )
            .unwrap(),
            1,
            "still exactly one such column"
        );
    }

    /// A reason is remembered while it is fresh, forgotten the moment a sample arrives,
    /// and dropped once it has gone quiet: a task that was fixed elsewhere must stop
    /// explaining itself, or the panel keeps showing yesterday's cause.
    #[test]
    fn a_ping_error_is_remembered_then_forgotten() {
        let db = db();
        let id = node(&db, 1);
        assert!(db.ping_errors().is_empty(), "nothing has failed yet");
        db.note_ping_error(id, 7, Some("ICMP needs CAP_NET_RAW"));
        assert_eq!(db.ping_errors().get(&(id, 7)).map(String::as_str), Some("ICMP needs CAP_NET_RAW"));
        // A sample arriving is the end of the explanation.
        db.note_ping_error(id, 7, None);
        assert!(db.ping_errors().is_empty(), "a working probe stops explaining itself");
        // And one that has gone quiet is dropped, so a cause cannot outlive the facts.
        db.note_ping_error(id, 7, Some("stale"));
        db.ping_errors
            .lock()
            .unwrap()
            .insert((id, 7), ("stale".into(), Utc::now().timestamp() - PING_ERROR_TTL - 1));
        assert!(db.ping_errors().is_empty(), "a reason older than the window is not shown");
    }

    /// A fold counts exactly the minutes of its own hour: the window is
    /// `ts >= hour && ts < hour + 3600`, so the hour's first and last minute are in
    /// it and the next hour's first minute is not. Constructed here rather than
    /// inferred from a large fixture -- the question is where the window ends, not
    /// how much is in it -- and with two adjacent hours folded to show that a
    /// minute is counted once and never twice.
    #[test]
    fn a_fold_counts_the_minutes_of_its_own_hour_once() {
        let db = db();
        let id = node(&db, 1);
        let hour = 472_224 * 3_600;
        for (offset, cpu) in [(0, 1.0), (1, 2.0), (3_599, 3.0), (3_600, 99.0)] {
            db.insert_metric(id, hour + offset, &serde_json::json!({"cpu": cpu})).unwrap();
        }
        let folded = |ts: i64| -> Option<(i64, f64)> {
            db.conn()
                .query_row(
                    "SELECT minutes, cpu FROM metric_hour WHERE node_id=?1 AND ts=?2",
                    params![id, ts],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .unwrap()
        };

        db.fold_hour(hour).unwrap();
        assert_eq!(folded(hour), Some((3, 2.0)), "the hour's own three minutes, their mean");
        assert_eq!(folded(hour + 3_600), None, "its last minute is not the next hour's");

        db.fold_hour(hour + 3_600).unwrap();
        assert_eq!(folded(hour), Some((3, 2.0)), "a later hour does not disturb it");
        assert_eq!(folded(hour + 3_600), Some((1, 99.0)), "counted once, in its own hour");
    }

    /// An hour is not folded when it ends: an agent may report up to an hour late
    /// and a probe's answer arrives with the frame after it, so the hour is only
    /// complete once the next one has passed. Folding it early would drop whatever
    /// arrived late, permanently.
    #[test]
    fn an_hour_is_folded_only_once_the_next_one_has_passed() {
        let db = db();
        let id = node(&db, 1);
        let hour = 472_224 * 3_600;
        db.insert_metric(id, hour + 60, &serde_json::json!({"cpu": 1.0})).unwrap();

        // A minute into the next hour: the hour has ended, but not long enough ago.
        assert_eq!(db.roll_up(hour + 3_600 + 60, 30).unwrap(), 0);
        assert_eq!(count(&db, "metric_hour"), 0, "the hour may still be written to");

        // One more hour on, it is complete and folds.
        assert_eq!(db.roll_up(hour + 7_200 + 60, 30).unwrap(), 1);
        assert_eq!(count(&db, "metric_hour"), 1);
    }

    /// The fold is idempotent, and the watermark is what makes it so: folding an
    /// hour again rewrites one row rather than adding another, and a minute row
    /// that arrives after its hour was folded is **not** folded into it. That is
    /// the price of a bounded window, and the reason the margin is a whole hour.
    #[test]
    fn folding_an_hour_twice_rewrites_one_row() {
        let db = db();
        let id = node(&db, 1);
        let hour = 472_224 * 3_600;
        db.insert_metric(id, hour + 60, &serde_json::json!({"cpu": 1.0})).unwrap();
        db.fold_hour(hour).unwrap();
        db.fold_hour(hour).unwrap();
        assert_eq!(count(&db, "metric_hour"), 1, "a second fold of one hour is a rewrite");
        {
            let conn = db.conn();
            let rolled: Option<String> = conn
                .query_row("SELECT value FROM setting WHERE key=?1", [ROLLED], |r| r.get(0))
                .optional()
                .unwrap();
            assert_eq!(
                rolled.as_deref(),
                Some((hour + 3_600).to_string().as_str()),
                "rolled={rolled:?} hour={hour}"
            );
        }

        // A minute row landing even later belongs to an hour the cursor has left.
        // The rollup advances through the hours between (folding them, empty or
        // not) rather than going back to the one it has already passed.
        db.insert_metric(id, hour + 120, &serde_json::json!({"cpu": 9.0})).unwrap();
        db.roll_up(hour + 4 * 3_600, 30).unwrap();
        let conn = db.conn();
        let cpu: f64 = conn
            .query_row("SELECT cpu FROM metric_hour WHERE node_id=?1 AND ts=?2", params![id, hour], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(cpu, 1.0, "the hour is not rebuilt from rows that arrived after it");
    }

    /// Rows in one table, for the fold tests.
    fn count(db: &Db, table: &str) -> i64 {
        db.conn().query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)).unwrap()
    }

    /// A backup taken before the summary tables existed restores. `check_backup`
    /// runs the migrations on the upload, and it is the only place a table added
    /// by a migration is load-bearing: the startup path applies `SCHEMA` first,
    /// whose `CREATE TABLE IF NOT EXISTS` would create it anyway.
    ///
    /// Without that migration the file is refused as not a hub backup, which is
    /// what an older release's export would then get.
    #[test]
    fn a_backup_from_before_the_summary_tables_still_restores() {
        let scratch = Scratch::new();
        let copy = format!("{}.old", scratch.0);
        let db = Db::open(&scratch.0).unwrap();
        db.create_node(&Node { name: "old".into(), ..Default::default() }, "token-old").unwrap();
        db.backup_into(&copy).unwrap();

        // The file as the previous release left it: no summary tables, and its own
        // version stamp.
        {
            let old = Connection::open(&copy).unwrap();
            for table in HOUR_TABLES {
                old.execute(&format!("DROP TABLE {table}"), []).unwrap();
            }
            old.execute_batch("PRAGMA user_version = 11").unwrap();
        }

        db.check_backup(&copy).unwrap();
        db.restore_from(&copy).unwrap();
        // The uploaded file becomes the database this hub runs on, and
        // `restore_from` does not reapply `SCHEMA`: whatever the migration put in
        // the upload is what the hub will have. (A restart would repair it, which
        // is exactly why this is asserted here rather than left to the next one.)
        {
            let conn = db.conn();
            assert!(columns_of(&conn, "metric_hour").unwrap().contains("minutes"));
            assert!(columns_of(&conn, "ping_hour").unwrap().contains("answered"));
            let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
            assert_eq!(version, SCHEMA_VERSION);
        }
        assert_eq!(db.nodes().unwrap()[0].name, "old");
        let _ = std::fs::remove_file(&copy);
    }

    /// A database the previous release wrote, opened by this build, has to end up
    /// with the same tables and the same columns as one this build creates: a
    /// column `SCHEMA` gained without a migration, a migration that does not
    /// reach `SCHEMA`, or a migration that changes data on its second run all
    /// fail here.
    ///
    /// Started from the v10 fixture rather than from the release just before this
    /// one, so every migration this build has runs, not only the newest.
    #[test]
    fn an_upgraded_release_matches_a_fresh_database() {
        let file = std::env::temp_dir().join(format!("monitor-upgraded-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&file);
        let path = file.to_str().unwrap();
        a_v10_file(path, "VALUES (1,'known','t1','198.51.100.4','US',1)");
        Connection::open(path)
            .unwrap()
            .execute_batch(
                "INSERT INTO traffic (node_id, total_rx) VALUES (1, 5000);
                 INSERT INTO metric (node_id, ts, cpu, mem_used, swap_used, disk_used, net_rx, net_tx, tcp, udp, procs)
                   VALUES (1, 60, 12.5, 100, 0, 0, 0, 0, 0, 0, 0);
                 INSERT INTO ping_task (id, name, target) VALUES (1, 'cm', '1.1.1.1:443');
                 INSERT INTO ping_node VALUES (1, 1);
                 INSERT INTO ping_record VALUES (1, 1, 60, 42);",
            )
            .unwrap();

        let db = Db::open(path).unwrap();
        let fresh = Db::open(":memory:").unwrap();
        for table in TABLES {
            assert_eq!(
                columns_of(&db.conn(), table).unwrap(),
                columns_of(&fresh.conn(), table).unwrap(),
                "{table}"
            );
        }
        // The summary tables are not in TABLES -- that list is what a backup must
        // already contain, and an older backup does not -- so they are compared
        // here instead.
        for table in HOUR_TABLES {
            assert_eq!(
                columns_of(&db.conn(), table).unwrap(),
                columns_of(&fresh.conn(), table).unwrap(),
                "{table}"
            );
        }
        let all: Vec<&str> = TABLES.iter().chain(HOUR_TABLES.iter()).copied().collect();
        let upgraded = dump(&db.conn(), &all);
        assert_eq!(upgraded.len(), all.len(), "every table is readable: {upgraded:#?}");
        assert!(
            upgraded.iter().any(|(t, rows)| t == "metric" && rows.contains("Real(12.5)")),
            "the rows an older release wrote survive: {upgraded:#?}"
        );

        // An earlier build opening the file stamps its own, lower version, so the
        // next upgrade runs every migration again -- and must change nothing.
        db.conn().execute_batch("PRAGMA user_version = 10").unwrap();
        drop(db);
        let again = Db::open(path).unwrap();
        assert_eq!(dump(&again.conn(), &all), upgraded, "a second run changes nothing");
        let version: i64 = again.conn().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        drop(again);
        let _ = std::fs::remove_file(&file);
    }

    /// Every country stored until now was looked up from the connection address,
    /// so the upgrade records that address as the one the badge belongs to. A node
    /// with no country is owed a lookup either way, and is left without an address
    /// nobody asked about -- `country_owed` would otherwise answer for one.
    #[test]
    fn an_upgrade_records_the_address_each_stored_country_was_looked_up_from() {
        let file = std::env::temp_dir().join(format!("monitor-country-ip-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&file);
        let path = file.to_str().unwrap();
        a_v10_file(
            path,
            "VALUES (1,'known','t1','198.51.100.4','US',1), (2,'unknown','t2','203.0.113.9','',1)",
        );

        let db = Db::open(path).unwrap();
        let read = |db: &Db, id: i64| {
            db.conn()
                .query_row("SELECT country_ip, country FROM node WHERE id=?1", [id], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })
                .unwrap()
        };
        assert_eq!(read(&db, 1), ("198.51.100.4".into(), "US".into()), "the stored badge keeps its address");
        assert_eq!(read(&db, 2), (String::new(), String::new()), "nothing was asked about this one");
        assert!(!db.country_owed(1, "198.51.100.4").unwrap(), "and its lookup has landed");
        // The address this node connects from was never asked about, so the
        // upgrade must not leave the row looking as if it had been.
        assert!(!db.country_owed(2, "203.0.113.9").unwrap());

        // The rules every migration follows: an earlier build stamps its own,
        // lower version into the file, so the next upgrade runs this one again.
        db.conn().execute_batch("PRAGMA user_version = 10").unwrap();
        drop(db);
        let db = Db::open(path).unwrap();
        assert_eq!(read(&db, 1), ("198.51.100.4".into(), "US".into()), "a second run changes nothing");
        assert_eq!(read(&db, 2), (String::new(), String::new()));
        for column in ADDED {
            assert!(columns_of(&db.conn(), "node").unwrap().contains(column), "{column}");
        }
        drop(db);
        let _ = std::fs::remove_file(&file);
    }

    /// A failure part-way through an upgrade -- a full disk, a killed process --
    /// leaves the file at the version it started from rather than between two, so
    /// the build that rolls back to its predecessor runs on what it expects.
    #[test]
    fn a_failed_upgrade_leaves_the_file_as_it_was() {
        let file = std::env::temp_dir().join(format!("monitor-failed-upgrade-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&file);
        let path = file.to_str().unwrap();
        a_v10_file(path, "VALUES (1,'known','t1','198.51.100.4','US',1)");

        // The backfill is the statement this build adds; make it fail, after the
        // column additions before it have already run.
        let old = Connection::open(path).unwrap();
        old.execute_batch(
            "CREATE TRIGGER fail BEFORE UPDATE ON node BEGIN SELECT RAISE(ABORT, 'disk full'); END",
        )
        .unwrap();
        drop(old);

        assert!(Db::open(path).is_err(), "the upgrade cannot finish");
        let conn = Connection::open(path).unwrap();
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, 10, "the stamp rolled back with the steps");
        let columns = columns_of(&conn, "node").unwrap();
        for column in ADDED {
            assert!(!columns.contains(column), "{column} is not left half-added");
        }
        let _ = std::fs::remove_file(&file);
    }

    /// Removing a node from a probe must remove the probe from that node's chart.
    /// `ping_record` carries no foreign key to the assignment that produced it, so
    /// the rows outlive it until retention; the window query is what must stop
    /// drawing them, and immediately rather than at the next hourly sweep.
    #[test]
    fn a_probe_taken_off_a_node_stops_appearing_in_its_history() {
        let db = db();
        let id = node(&db, 1);
        let probe = |nodes: Vec<i64>, task| {
            db.save_ping_task(&PingTask {
                kind: None,
                id: task,
                name: "cm".into(),
                target: "1.1.1.1:443".into(),
                interval: 60,
                nodes,
            })
            .unwrap()
        };
        let task = probe(vec![id], 0);
        db.insert_ping(id, task, 100, 42).unwrap();
        assert_eq!(db.ping_records(id, 0, 60).unwrap().0.len(), 1, "an assigned probe draws");

        probe(vec![], task);
        assert!(db.ping_records(id, 0, 60).unwrap().0.is_empty(), "an unassigned one does not");

        // The rows remain: reassigning restores the history rather than starting
        // over.
        probe(vec![id], task);
        assert_eq!(db.ping_records(id, 0, 60).unwrap().0.len(), 1, "and it comes back with its history");

        // The names accompany those samples and follow the same filter: a probe
        // name is operator-supplied text that routinely carries a hostname or a
        // customer.
        assert_eq!(db.ping_task_names(id).unwrap()[&task.to_string()], "cm");
        let other = node(&db, 1);
        assert!(
            db.ping_task_names(other).unwrap().as_object().is_some_and(|m| m.is_empty()),
            "a node the probe was never assigned to must not learn its name"
        );
    }

    #[test]
    fn ping_tasks_round_trip_with_their_node_assignments() {
        let db = db();
        let (a, b) = (node(&db, 1), node(&db, 1));
        let id = db
            .save_ping_task(&PingTask {
                kind: None,
                id: 0,
                name: "cf".into(),
                target: "1.1.1.1:443".into(),
                interval: 60,
                nodes: vec![a, b],
            })
            .unwrap();
        assert_eq!(db.ping_tasks_for(a).unwrap().len(), 1);

        // Reassigning to one node must drop the other's copy.
        db.save_ping_task(&PingTask {
            kind: None,
            id,
            name: "cf".into(),
            target: "1.1.1.1:443".into(),
            interval: 30,
            nodes: vec![a],
        })
        .unwrap();
        assert_eq!(db.ping_tasks_for(b).unwrap().len(), 0);
        assert_eq!(db.ping_tasks().unwrap()[0].interval, 30);
    }

    /// The agent caps the probe list it will run and drops the remainder with
    /// nothing but a line in its own journal. The hub knows the total, so the hub
    /// issues the refusal; otherwise the panel lists probes that never ran and
    /// charts that stay empty, with the only record on the node.
    #[test]
    fn a_node_cannot_be_given_more_probes_than_the_agent_will_run() {
        let db = db();
        let id = node(&db, 1);
        let save = |task: i64, nodes: Vec<i64>| {
            db.save_ping_task(&PingTask {
                kind: None,
                id: task,
                name: "p".into(),
                target: "1.1.1.1:443".into(),
                interval: 60,
                nodes,
            })
        };
        for _ in 0..Db::MAX_PROBES_PER_NODE {
            save(0, vec![id]).unwrap();
        }
        assert_eq!(db.ping_tasks_for(id).unwrap().len() as i64, Db::MAX_PROBES_PER_NODE);

        let refused = save(0, vec![id]).expect_err("one past the cap must be refused");
        assert!(refused.to_string().contains("探测任务"), "{refused}");
        // Rolled back entirely: the probe must not survive its assignment being
        // rejected, or the panel accumulates one that never runs.
        assert_eq!(db.ping_tasks().unwrap().len() as i64, Db::MAX_PROBES_PER_NODE);
        assert_eq!(db.ping_tasks_for(id).unwrap().len() as i64, Db::MAX_PROBES_PER_NODE);

        // Editing an existing probe does not count as adding one.
        let first = db.ping_tasks().unwrap()[0].id;
        save(first, vec![id]).expect("an existing probe can still be edited at the cap");
    }

    /// The two windows come from one scan and are distinguished by `ts >=`, and
    /// both ends are half-open: a row stamped on a boundary belongs to the window
    /// that starts there and not to the one that ends there.
    #[test]
    fn uptime_counts_split_the_two_windows_and_bounds_both_ends() {
        let db = db();
        let id = node(&db, 1);
        let idle = node(&db, 1);
        let now = 1_800_000_000;
        let since7 = now - 7 * 86_400;
        let since30 = now - 30 * 86_400;

        // Ten minutes inside the week, five in the one before it, and eight days
        // back for the idle node -- inside the month and outside the week.
        for i in 1..=10 {
            db.insert_metric(id, now - i * 60, &serde_json::json!({"cpu": 1.0})).unwrap();
        }
        for i in 7 * 1_440 + 1..=7 * 1_440 + 5 {
            db.insert_metric(id, now - i * 60, &serde_json::json!({"cpu": 1.0})).unwrap();
        }
        db.insert_metric(idle, now - 8 * 86_400, &serde_json::json!({"cpu": 1.0})).unwrap();

        let counts = db.uptime_counts(since7, since30, now).unwrap();
        assert_eq!(counts[&id], (10, 15), "the second figure counts the whole month");
        assert_eq!(counts[&idle], (0, 1));
        assert!(!counts.contains_key(&9_999), "a node that never reported has no row at all");

        // A row stamped exactly at the end belongs to the window that begins
        // there; the minute in progress is not counted as expected.
        db.insert_metric(id, now, &serde_json::json!({"cpu": 1.0})).unwrap();
        assert_eq!(db.uptime_counts(since7, since30, now).unwrap()[&id], (10, 15));
        assert_eq!(db.uptime_counts(since7, since30, now + 60).unwrap()[&id], (11, 16));

        let minutes = db.reported_minutes(id, since7, now).unwrap();
        assert_eq!(minutes.len(), 10);
        assert!(minutes.windows(2).all(|w| w[0] < w[1]), "oldest first");
        assert!(minutes.iter().all(|t| *t >= since7 && *t < now));
        assert_eq!(db.reported_minutes(idle, since7, now).unwrap().len(), 0);
    }

    /// 全队聚合的三条规则：cpu 按节点加权平均、内存按 Σ已用/Σ总量、带宽按节点求和。
    /// 三台机器刻意给不同的 mem_total —— 若把内存写成「各节点百分比的算术平均」，这个用例就会红。
    #[test]
    fn overview_daily_averages_resources_but_sums_bandwidth() {
        let db = Db::open(":memory:").unwrap();
        let ts = (Utc::now().timestamp() / 3600) * 3600;
        let mut ids = Vec::new();
        for (name, mem_total, mem_used, rx) in
            [("a", 2_000i64, 1_000i64, 100i64), ("b", 8_000, 2_000, 200), ("c", 30_000, 3_000, 300)]
        {
            let n = db
                .create_node(
                    &Node { name: name.into(), mem_total, disk_total: 100_000, ..Default::default() },
                    &format!("tok-{name}"),
                )
                .unwrap();
            // `create_node` 只写节点自身的字段，容量来自 agent 上报，所以这里直接落库。
            db.conn()
                .execute(
                    "UPDATE node SET mem_total = ?1, disk_total = ?2 WHERE id = ?3",
                    rusqlite::params![mem_total, 100_000i64, n],
                )
                .unwrap();
            db.conn()
                .execute(
                    "INSERT INTO metric_hour (node_id, ts, minutes, cpu, mem_used, swap_used, disk_used,
                                              net_rx, net_tx, load1, net_rx_max, net_tx_max)
                     VALUES (?1, ?2, 60, 10.0, ?3, 0, 50_000, ?4, ?4, NULL, ?4, ?4)",
                    rusqlite::params![n, ts, mem_used, rx],
                )
                .unwrap();
            ids.push(n);
        }
        let rows = db.overview_daily(1, None);
        assert_eq!(rows.len(), 1, "只插了一个小时，应当只有一天");
        let r = rows[0];
        // 带宽：求和（100+200+300），再乘 60 分钟 × 60 秒
        assert_eq!(r[1], (100 + 200 + 300) as f64 * 3600.0);
        // 峰值：同一时刻三台之和
        assert_eq!(r[3], 600.0);
        // cpu：三台都是 10%，加权平均仍是 10
        assert!((r[5] - 10.0).abs() < 0.01, "cpu 加权平均 = {}", r[5]);
        // 内存：Σ已用 / Σ总量 = 6000 / 40000 = 15%（按百分比平均则是 (50+25+10)/3 = 28.3%）
        assert!((r[6] - 15.0).abs() < 0.01, "内存应为 Σ已用/Σ总量 = 15%，实际 {}", r[6]);
        assert!((r[7] - 50.0).abs() < 0.01, "硬盘 = Σ已用/Σ总量 = 50%，实际 {}", r[7]);
        // 三台都在同一个小时里、各给 60 分钟：**墙上时钟**的覆盖时间就是 3600 秒。
        // （若按各节点相加会得到 10800，那与折线「全队速率」不同口径，会让柱与线不可比。）
        assert_eq!(r[8], 3600.0, "覆盖秒数应当是墙上时钟的 3600 秒，实际 {}", r[8]);
        // 三条真不变量：**最热的那台 ≥ 全队平均**（三台都是 10% cpu、内存 50/25/10%，所以这里取等）。
        assert!(r[9] >= r[5], "最热 cpu {} 不该低于平均 {}", r[9], r[5]);
        assert!(r[10] >= r[6], "最热内存 {} 不该低于平均 {}", r[10], r[6]);
        assert!(r[11] >= r[7], "最热硬盘 {} 不该低于平均 {}", r[11], r[7]);
        assert_eq!(r[10], 50.0, "三台里内存占用率最高的是 a：1000/2000 = 50%");
        let _ = ids;
    }

    /// 「需要处理的节点」：最差的排第一；**没丢包的探测不该出现在丢包榜**；离线榜排除从未上报。
    #[test]
    fn at_risk_ranks_worst_first() {
        let db = Db::open(":memory:").unwrap();
        let now = Utc::now().timestamp();
        let hour = (now / 3600) * 3600;
        let mut ids = Vec::new();
        for name in ["slow", "fast", "never"] {
            let n = db
                .create_node(
                    &Node { name: name.into(), mem_total: 1000, disk_total: 1000, ..Default::default() },
                    name,
                )
                .unwrap();
            // create_node 不写 last_seen，直接落库；never 保持 0（= 从未上报）
            if name != "never" {
                db.conn()
                    .execute("UPDATE node SET last_seen=?1 WHERE id=?2", params![now - 3600, n])
                    .unwrap();
            }
            ids.push(n);
        }
        db.conn()
            .execute(
                "INSERT INTO ping_task (id,name,target,interval,sort,kind) VALUES (1,'t','x',60,0,'icmp')",
                [],
            )
            .unwrap();
        for n in &ids {
            db.conn().execute("INSERT INTO ping_node (task_id,node_id) VALUES (1,?1)", params![n]).unwrap();
        }
        // slow：慢且丢包；fast：快且不丢；never：也报一次但很快
        for (n, lat, answered, lost) in
            [(ids[0], 300.0, 90_i64, 10_i64), (ids[1], 20.0, 100, 0), (ids[2], 10.0, 100, 0)]
        {
            db.conn()
                .execute(
                    "INSERT INTO ping_hour (node_id,task_id,ts,answered,lost,latency,lo,hi) VALUES (?1,1,?2,?3,?4,?5,?5,?5)",
                    params![n, hour, answered, lost, lat],
                )
                .unwrap();
        }
        let (latency, loss, down) = db.at_risk(7, 5, None);
        assert_eq!(latency.len(), 3, "延迟榜应有 3 行");
        assert_eq!(latency[0].1, "slow", "最慢的必须排第一，实际 {:?}", latency[0]);
        assert!((latency[0].2 - 300.0).abs() < 1.0, "加权均值 {}", latency[0].2);
        assert_eq!(loss.len(), 1, "**只有丢过包的**才该进丢包榜");
        assert_eq!(loss[0].1, "slow");
        assert!((loss[0].2 - 0.1).abs() < 0.001, "丢包率 {}", loss[0].2);
        // 离线榜：never（last_seen=0）不该出现
        assert_eq!(down.len(), 2, "从来未上报的不是离线，应排除");
        assert!(down.iter().all(|(name, _)| name != "never"), "实际 {:?}", down);
    }

    /// 分组筛选也要作用于「需要处理的节点」—— 否则切换分组时，KPI/事项跟着变、这三条榜不变，
    /// 正是"一半按分组、一半不按"。
    #[test]
    fn at_risk_filters_by_group() {
        let db = Db::open(":memory:").unwrap();
        let now = Utc::now().timestamp();
        let hour = (now / 3600) * 3600;
        let mut ids = Vec::new();
        for (name, group) in [("slow", "east"), ("fast", "west")] {
            let n = db
                .create_node(
                    &Node { name: name.into(), mem_total: 1000, disk_total: 1000, ..Default::default() },
                    name,
                )
                .unwrap();
            db.conn()
                .execute(
                    "UPDATE node SET last_seen=?1, \"group\"=?2 WHERE id=?3",
                    params![now - 60, group, n],
                )
                .unwrap();
            ids.push(n);
        }
        db.conn()
            .execute(
                "INSERT INTO ping_task (id,name,target,interval,sort,kind) VALUES (1,'t','x',60,0,'icmp')",
                [],
            )
            .unwrap();
        for (n, lat) in [(ids[0], 300.0), (ids[1], 20.0)] {
            db.conn().execute("INSERT INTO ping_hour (node_id,task_id,ts,answered,lost,latency,lo,hi) VALUES (?1,1,?2,100,0,?3,?3,?3)", params![n, hour, lat]).unwrap();
        }
        let all = db.at_risk(7, 5, None);
        assert_eq!(all.0.len(), 2, "不分分组时两条都在");
        let east = db.at_risk(7, 5, Some("east"));
        assert_eq!(east.0.len(), 1, "只看 east 时应当只剩一条");
        assert_eq!(east.0[0].1, "slow");
        assert_eq!(east.2.len(), 1, "离线榜也要按分组过滤");
        assert_eq!(east.2[0].0, "slow");
        assert!(db.at_risk(7, 5, Some("nowhere")).0.is_empty());
    }

    /// **不变量**：一次按任务取回的结果，必须与**逐节点调用线上那条 `ping_window`** 的结果
    /// **逐点相等**（行与窗口 loss 都相等）。
    ///
    /// 面板从此只发一个请求，但用户看到的数**一个也不能变** —— 这条测试就是那个保证。
    /// 注意参照必须是**线上在用的**那条（`ping_records`，分钟级），不是 `ping_records_hourly`
    /// 那条还没接上的小时级路径 —— 一开始我正是拿后者当了参照，那样写等于拿死代码证明一致性。
    #[test]
    fn ping_series_by_task_matches_the_per_node_path() {
        let db = Db::open(":memory:").unwrap();
        let now = Utc::now().timestamp();
        let minute = (now / 60) * 60;
        // 任务先存在：`ping_node` 有指向 `ping_task` 的外键（级联），顺序错了会 ConstraintViolation。
        db.conn()
            .execute(
                "INSERT INTO ping_task (id,name,target,interval,sort,kind) VALUES (1,'t','x',60,0,'icmp')",
                [],
            )
            .unwrap();
        let mut ids = Vec::new();
        for (name, group) in [("a", "east"), ("b", "west")] {
            let n = db
                .create_node(
                    &Node { name: name.into(), mem_total: 1000, disk_total: 1000, ..Default::default() },
                    name,
                )
                .unwrap();
            db.conn().execute("UPDATE node SET \"group\"=?1 WHERE id=?2", params![group, n]).unwrap();
            db.conn().execute("INSERT INTO ping_node (task_id,node_id) VALUES (1,?1)", params![n]).unwrap();
            ids.push(n);
        }
        // 每台几分钟的样本，含一次超时（latency = -1）与一次正常值
        for (i, n) in ids.iter().enumerate() {
            for m in 0..6 {
                let lat = if m == 3 { -1 } else { 20 + i as i64 + m };
                db.conn()
                    .execute(
                        "INSERT INTO ping_record (node_id,task_id,ts,latency) VALUES (?1,1,?2,?3)",
                        params![n, minute - m * 60, lat],
                    )
                    .unwrap();
            }
        }
        let many = db.ping_series_by_task(1, minute - 3_600, 300, None).unwrap();
        assert_eq!(many.len(), 2, "两台节点都该有序列");
        for n in &ids {
            let (one_rows, one_loss) = db.ping_window(*n, minute - 3_600, 300).unwrap();
            let (rows, loss) = many.get(n).cloned().unwrap_or_default();
            assert_eq!(
                serde_json::to_string(&rows).unwrap(),
                serde_json::to_string(&one_rows).unwrap(),
                "节点 {} 的行必须与逐节点 ping_records 逐点相等",
                n
            );
            assert_eq!(
                serde_json::to_string(&loss).unwrap(),
                serde_json::to_string(&one_loss).unwrap(),
                "节点 {} 的窗口 loss 也必须相等（不取整）",
                n
            );
        }
        // 分组过滤：只取 east，应当只剩一台
        let east = db.ping_series_by_task(1, minute - 3_600, 300, Some("east")).unwrap();
        assert_eq!(east.len(), 1, "按分组应当只剩一台");
    }

    /// 分组筛选：取某一组时只算那一组；**各组合计必须等于全部**（分组最容易错的地方）。
    #[test]
    fn overview_daily_filters_by_group() {
        let db = Db::open(":memory:").unwrap();
        let ts = (Utc::now().timestamp() / 3600) * 3600;
        for (name, group, cpu) in [("a1", "east", 10.0), ("a2", "east", 20.0), ("b1", "west", 40.0)] {
            let n = db
                .create_node(
                    &Node { name: name.into(), mem_total: 1000, disk_total: 1000, ..Default::default() },
                    name,
                )
                .unwrap();
            // `create_node` 不写 `group`（与它不写 `mem_total` 是同一件事）→ 直接落库；
            // `group` 是 SQL 关键字，列名必须加引号。
            db.conn()
                .execute("UPDATE node SET \"group\" = ?1 WHERE id = ?2", rusqlite::params![group, n])
                .unwrap();
            db.conn()
                .execute(
                    "INSERT INTO metric_hour (node_id, ts, minutes, cpu, mem_used, swap_used, disk_used,
                                              net_rx, net_tx, load1, net_rx_max, net_tx_max)
                     VALUES (?1, ?2, 60, ?3, 0, 0, 0, 100, 100, NULL, 100, 100)",
                    rusqlite::params![n, ts, cpu],
                )
                .unwrap();
        }
        let all = db.overview_daily(1, None);
        let east = db.overview_daily(1, Some("east"));
        let west = db.overview_daily(1, Some("west"));
        assert!((all[0][5] - 23.333).abs() < 0.01, "全部的平均 {}", all[0][5]);
        assert!((east[0][5] - 15.0).abs() < 0.01, "east 的平均 {}", east[0][5]);
        assert!((west[0][5] - 40.0).abs() < 0.01, "west 的平均 {}", west[0][5]);
        assert_eq!(all[0][1], 300.0 * 3600.0);
        assert_eq!(east[0][1], 200.0 * 3600.0);
        assert_eq!(west[0][1], 100.0 * 3600.0);
        assert_eq!(east[0][1] + west[0][1], all[0][1], "各组合计必须等于全部");
        assert!(db.overview_daily(1, Some("nowhere")).is_empty(), "不存在的分组应是空结果而不是报错");
    }

    /// **证明它是 P95 而不是 MAX**：20 台节点，其中一台 cpu 99（离群），其余 19 台都是 10。
    /// 前 5% 是排名第 20 的那一个（`rn >= 20 * 0.95 = 19` → 19、20 都算）——
    /// SQLite 的 `rn >= n*0.95` 会把第 19、20 名都纳入，所以这里取到 99 是**符合定义**的；
    /// 关键是：把离群值压到第 **10%** 的位置时，P95 必须**不再**包含它。
    #[test]
    fn overview_daily_p95_is_not_the_max() {
        let db = Db::open(":memory:").unwrap();
        let ts = (Utc::now().timestamp() / 3600) * 3600;
        for i in 0..20 {
            let n = db
                .create_node(
                    &Node { name: format!("n{i}"), mem_total: 1000, disk_total: 1000, ..Default::default() },
                    &format!("t{i}"),
                )
                .unwrap();
            // 第 0 台是离群值（cpu 99），其余 19 台是 10
            let cpu = if i == 0 { 99.0 } else { 10.0 };
            db.conn()
                .execute(
                    "INSERT INTO metric_hour (node_id, ts, minutes, cpu, mem_used, swap_used, disk_used,
                                              net_rx, net_tx, load1, net_rx_max, net_tx_max)
                     VALUES (?1, ?2, 60, ?3, 0, 0, 0, 0, 0, NULL, 0, 0)",
                    rusqlite::params![n, ts, cpu],
                )
                .unwrap();
        }
        let rows = db.overview_daily(1, None);
        let r = rows[0];
        // 20 行里排名前 5% 是第 19、20 名（rn >= 19）→ 包含离群的 99。
        assert_eq!(r[9], 99.0, "rn>=19 时应当包含第 19 名，这里就是那台离群的");
        // 而**平均**只有 (99 + 19×10)/20 = 14.45 —— 平均与 P95 的差距就是「分布带」要表达的东西。
        assert!((r[5] - 14.45).abs() < 0.01, "平均应为 14.45，实际 {}", r[5]);
        assert!(r[9] > r[5] * 6.0, "P95 应当远高于平均，这正是分布带的意义");
    }
}

#[cfg(test)]
mod heatmap_tests {
    use super::*;

    /// 边界由分位数算，且**含首尾**、**严格递增**。
    #[test]
    fn band_edges_follow_the_window_quantiles() {
        let v: Vec<i64> = (1..=100).collect();
        let e = Db::band_edges(&v);
        // 6 条带 ⇒ 7 个边界，两端就是最小与最大（运维要看的是"最快/最慢到多少"）。
        assert_eq!(e[0], 1, "首个边界应当是最小值");
        assert_eq!(*e.last().unwrap(), 100, "末个边界应当是最大值");
        // 最近秩：100 个点时 p10 → 下标 round(0.10*99)=10 → 值 11。
        assert_eq!(e[1], 11, "p10");
        assert_eq!(e[2], 26, "p25");
        assert_eq!(e[3], 51, "p50");
        assert_eq!(e[4], 75, "p75");
        assert_eq!(e[5], 90, "p90");
        // 严格递增（重复值被去掉）—— 否则会出现宽度为 0 的带子（永远为空、只是噪音）。
        for w in e.windows(2) {
            assert!(w[0] < w[1], "边界必须严格递增：{:?}", e);
        }
    }

    /// 大量同值时边界会塌缩 —— 必须仍是严格递增，且至少留一条能用的带子。
    #[test]
    fn band_edges_survive_a_single_value() {
        let e = Db::band_edges(&[42, 42, 42, 42]);
        assert!(e.len() >= 2, "至少两个边界才成一条带子：{:?}", e);
        for w in e.windows(2) {
            assert!(w[0] < w[1], "边界必须严格递增：{:?}", e);
        }
    }

    /// 分带：区间按 `[lo, hi)`，**最后一个带包含上界**。
    #[test]
    fn band_of_keeps_the_top_sample_inside() {
        let e = vec![10, 20, 30, 40];
        assert_eq!(Db::band_of(&e, 10), 0, "等于下界 → 第一条带");
        assert_eq!(Db::band_of(&e, 19), 0, "下界之内");
        assert_eq!(Db::band_of(&e, 20), 1, "等于边界 → 归上一条带（[lo, hi)）");
        assert_eq!(Db::band_of(&e, 39), 2);
        // **恰好等于最大值**必须落在最后一条带里，否则"窗口里最慢的那个样本"
        // 会掉出所有带子 —— 总计数与采样数对不上，而图上只是"某格略浅" ✗。
        // `e` 有 4 个边界 ⇒ **3 条带**（下标 0..=2），所以"最后一条"是 2 —— 不是 3。
        assert_eq!(Db::band_of(&e, 40), 2, "等于上界 → 最后一条带");
        assert_eq!(Db::band_of(&e, 999), 2, "超出上界也归最后一条（防御，靠 .min 夹取）");
    }

    /// 没有样本时给一条退化带子，调用方不必特判。
    #[test]
    fn band_edges_handle_no_samples() {
        assert_eq!(Db::band_edges(&[]), vec![0, 1]);
    }
}
