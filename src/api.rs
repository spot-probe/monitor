//! The panel and public-status HTTP surface.

use axum::extract::rejection::JsonRejection;
use axum::extract::ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{Local, NaiveDate, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use tracing::debug;

use crate::agent_ws::Agent;
use crate::auth::{
    authed, client_ip, current_session, hash_password, issue_session, issued_at, random_token, with_cookies,
};
use crate::db::{Node, NodePatch, PingTask, Traffic, TrafficPatch};
use crate::{agent_ws, App, Shared};

/// Present only on requests carrying a valid session. Handlers taking it cannot
/// be reached unauthenticated, so the check cannot be omitted.
pub struct Admin;

impl FromRequestParts<Shared> for Admin {
    type Rejection = StatusCode;

    async fn from_request_parts(parts: &mut Parts, app: &Shared) -> Result<Self, Self::Rejection> {
        if authed(app, &parts.headers) {
            Ok(Admin)
        } else {
            Err(StatusCode::UNAUTHORIZED)
        }
    }
}

fn fail(e: impl std::fmt::Display) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
}

fn bad(message: &str) -> Response {
    (StatusCode::BAD_REQUEST, message.to_owned()).into_response()
}

fn no_such_node() -> Response {
    (StatusCode::NOT_FOUND, "no such node").into_response()
}

// ---- read paths, shared between the panel and the public page ----

/// Everything a report may expose under `metrics` on the public page: the agent
/// contract minus the raw kernel counters, which are a wire-protocol detail
/// disclosing a machine's entire lifetime traffic, plus the four figures the hub
/// folds in itself. The panel sees the report as it arrived.
pub(crate) const PUBLIC_METRICS: [&str; 18] = [
    "uptime",
    "cpu",
    "load",
    "mem_total",
    "mem_used",
    "swap_total",
    "swap_used",
    "disk_total",
    "disk_used",
    "net_rx",
    "net_tx",
    "tcp",
    "udp",
    "procs",
    "total_rx",
    "total_tx",
    "month_rx",
    "month_tx",
];

/// Fraction of each uptime window a node was reporting in, and the window it was
/// measured over.
///
/// This is *node* availability -- whether the agent was talking to the hub --
/// and not probe availability, which is whether a target answered and lives on
/// `ping_record` instead. The two are different questions and the page keeps
/// them apart.
///
/// The window travels with the figure because it is not always the nominal seven
/// or thirty days: it is clamped to the node's own life and to the history the
/// hub still retains, and a fraction quoted against a window the reader did not
/// expect is the one way this number can lie. A caller that prints "last 30
/// days" from `d30` alone would be claiming a span the hub may not have; it has
/// `from30`/`to` to say how much was actually measured.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct Uptime {
    pub d7: f64,
    pub d30: f64,
    /// The minute each window starts at, for this node.
    pub from7: i64,
    pub from30: i64,
    /// The minute the windows end at: the start of the minute in progress. The
    /// same for both, and for every node, since it is the clock and not the
    /// node that sets it.
    pub to: i64,
}

/// The earliest minute the hub still holds rows for.
///
/// `housekeeping` prunes `metric` every hour to `retention_days`, so a window
/// reaching further back than this divides a numerator that has been deleted by
/// a denominator that has not. With the default seven days that reads as
/// twenty-three percent available over thirty days for every node on the page --
/// exactly the shape of a wrong number a status page cannot afford. The
/// denominator is therefore clamped to the retained history as well, and
/// `Uptime` carries the window that resulted.
fn retained_from(app: &App, now: i64) -> i64 {
    now - app.db.retention_days() * 86_400
}

/// The half-open minute grid a node's uptime is measured over.
///
/// The start is the later of `since` -- which the caller has already clamped to
/// the history the hub retains -- and the moment the node was added, rounded up
/// to the next whole minute; the end is the start of the minute now in progress.
/// Both ends land on the grid the metric rows are stamped on, so the rows counted
/// and the minutes divided by are the same kind of thing and a node added
/// mid-window cannot be charged for time before it existed.
///
/// The minute in progress is excluded on purpose: the agent may not have
/// reported for it yet, and counting it would make every node's uptime fall
/// by one minute every minute until its next report.
fn uptime_window(created_at: i64, since: i64, now: i64) -> (i64, i64) {
    let to = now.div_euclid(60) * 60;
    // `+ 59` then truncate is a ceiling for the positive seconds these always
    // are, and it leaves an already-aligned boundary where it is: a node added
    // on the minute keeps that minute, which it did report in.
    let from = (created_at.max(since) + 59).div_euclid(60) * 60;
    (from.min(to), to)
}

/// Reported minutes over the minutes the node was expected to report.
///
/// A window with nothing expected -- a node added within the current minute --
/// reads as fully available rather than as a failure: there is no missing report
/// to hold against it. Capped at one because a restored database can carry a
/// report stamped after the window it is being counted in.
fn uptime_fraction(count: i64, from: i64, to: i64) -> f64 {
    let expected = (to - from) / 60;
    if expected <= 0 {
        return 1.0;
    }
    (count as f64 / expected as f64).min(1.0)
}

/// Every node's reporting fraction, rebuilt at most once a minute.
///
/// Cached because the two figures move on a scale of days while this runs on the
/// path every browser stream takes twice a second. Measured on a 17-node,
/// 30-day fixture (652k rows) the aggregate is a full scan at 28 ms, since the
/// metric table is keyed `(node_id, ts)` and a range on `ts` alone cannot seek;
/// the same query per snapshot would spend that scan to move the second decimal
/// of a number nobody reads to that precision. A minute is the resolution the
/// rows underneath are written at.
fn uptime_map(app: &App, now: i64, nodes: &[Node]) -> HashMap<i64, Uptime> {
    const TTL: i64 = 60;
    let mut cache = app.uptime.lock().unwrap_or_else(|e| e.into_inner());
    // Non-negative age, for the reason `live_snapshot` gives: a wall clock that
    // steps backwards would otherwise read as young and pin a stale map.
    if (0..TTL).contains(&now.saturating_sub(cache.0)) {
        return cache.1.clone();
    }
    // The windows the *rows* are filtered by are the full ones; each node's own
    // clamp is applied below, per node, because it is the denominator that
    // differs and the query cannot know it. Passing a zero birthday here instead
    // was a real bug: it divides a node added three days ago by thirty days and
    // shows it at ten percent, which is the failure this whole clamp exists to
    // prevent. `kept` is the other clamp, and it is what stops a pruned database
    // from answering "thirty days" with seven days of rows.
    let kept = retained_from(app, now);
    let week = (now - 7 * 86_400).max(kept);
    let month = (now - 30 * 86_400).max(kept);
    let (since7, to) = uptime_window(0, week, now);
    let (since30, _) = uptime_window(0, month, now);
    let counts = match app.db.uptime_counts(since7, since30, to) {
        Ok(counts) => counts,
        // A failed read leaves every node at the same figure rather than the
        // whole node list failing with it: uptime is an addition to this view,
        // and a hub that cannot count minutes should still show its nodes.
        Err(e) => {
            tracing::warn!("uptime counts failed: {e:#}");
            HashMap::new()
        }
    };
    let mut map: HashMap<i64, Uptime> = HashMap::with_capacity(nodes.len());
    for node in nodes {
        // Absent from the aggregate means the node reported nothing in either
        // window, which is a real answer and not a missing one.
        let (c7, c30) = counts.get(&node.id).copied().unwrap_or((0, 0));
        let (from7, to7) = uptime_window(node.created_at, week, now);
        let (from30, _) = uptime_window(node.created_at, month, now);
        map.insert(
            node.id,
            Uptime {
                d7: uptime_fraction(c7, from7, to7),
                d30: uptime_fraction(c30, from30, to7),
                from7,
                from30,
                to: to7,
            },
        );
    }
    cache.0 = now;
    cache.1 = map.clone();
    map
}

/// The hourly bar and the outage list a node's detail page draws.
///
/// `minutes` is every reported minute in `[from, to)`, ascending. A bucket
/// carries `{ts, n, m}`: `n` reported minutes inside it, and `m` the minutes of
/// it that fell inside the node's own life in the window. `m` is carried rather
/// than assumed to be 60 because the first and last buckets are partial -- a
/// node added mid-hour, or the hour in progress -- and a bar that assumed a full
/// hour would draw either of them as an outage.
///
/// The outages are computed here rather than left to the theme because an hourly
/// bucket cannot say *where* inside the hour the missing minutes were, and an
/// outage is shown to the minute. Each is `{start, minutes}`; consecutive
/// missing minutes are one entry, and a silent node has one entry spanning the
/// whole window rather than none.
fn availability(minutes: &[i64], from: i64, to: i64) -> Value {
    const HOUR: i64 = 3_600;
    let mut buckets = Vec::new();
    let mut cursor = from.div_euclid(HOUR) * HOUR;
    let mut i = 0;
    while cursor < to {
        let lo = cursor.max(from);
        let hi = (cursor + HOUR).min(to);
        // Every minute in the slice is already within [from, to), so this only
        // has to consume the ones belonging to this bucket.
        let start = i;
        while i < minutes.len() && minutes[i] < cursor + HOUR {
            i += 1;
        }
        if hi > lo {
            buckets.push(json!({"ts": cursor, "n": i - start, "m": (hi - lo) / 60}));
        }
        cursor += HOUR;
    }

    // Gaps between consecutive reported minutes, plus the run before the first
    // and after the last, which are outages too: a node that stopped reporting
    // half an hour ago is still down, and one that was added and stayed silent
    // was never up.
    let mut incidents = Vec::new();
    let mut prev = from;
    for &t in minutes {
        if t - prev >= 60 {
            incidents.push(json!({"start": prev, "minutes": (t - prev) / 60}));
        }
        prev = t + 60;
    }
    if to - prev >= 60 {
        incidents.push(json!({"start": prev, "minutes": (to - prev) / 60}));
    }
    json!({"from": from, "to": to, "buckets": buckets, "incidents": incidents})
}

/// One node as the UI consumes it: stored config, live metrics and the hub's
/// accumulated traffic in a single object.
fn node_view(
    node: &Node,
    current: Option<&Agent>,
    traffic: &Traffic,
    uptime: Uptime,
    full: bool,
    today: NaiveDate,
) -> Value {
    // The three capacities arrive twice: once in `Facts`, sent at the handshake
    // and stored, and again in every `Metrics`. A machine that gains a disk while
    // the agent is running -- the agent re-reads its mount table every sample so
    // that it appears -- then has a stored figure that is stale until the next
    // reconnect, possibly days away. Using the report while a node is connected
    // keeps every consumer of this view on one number: the card reads the live
    // metrics and the detail page reads these, and they previously showed the
    // same machine two different capacities. Offline, the stored figure is all
    // there is. No floor is applied: a host whose swap has just been disabled
    // reports zero and means it. A node connected but not yet reporting holds
    // `Null`, where `get` returns nothing and the stored figure stands.
    let live = |key: &str, stored: i64| {
        current.and_then(|a| a.metrics.get(key).and_then(serde_json::Value::as_i64)).unwrap_or(stored)
    };
    let mut view = json!({
        "id": node.id,
        "name": node.name,
        // A country rather than an address: it indicates which region a node sits
        // in, which is what a status page conveys, without locating it. The
        // address it was derived from remains behind the panel.
        "country": node.country,
        "sort": node.sort,
        // The bucket, not a private note: it is what the page's tabs are built from.
        "group": node.group,
        "public": node.public,
        "online": current.is_some(),
        // The live entry while connected, the stored one afterwards. Zero means
        // connected but not yet reporting, which is not a timestamp, so it falls
        // back to the stored value and "offline since" survives the gap.
        "last_seen": current.map(|a| a.last_seen).filter(|t| *t > 0).unwrap_or(node.last_seen),
        "metrics": current.map(|a| a.metrics.clone()).unwrap_or(Value::Null),
        "os": node.os,
        "kernel": node.kernel,
        "arch": node.arch,
        "virt": node.virt,
        "cpu_name": node.cpu_name,
        "cpu_cores": node.cpu_cores,
        "mem_total": live("mem_total", node.mem_total),
        "swap_total": live("swap_total", node.swap_total),
        "disk_total": live("disk_total", node.disk_total),
        "agent_version": node.agent_version,
        "price": node.price,
        "currency": node.currency,
        "billing_cycle": node.billing_cycle,
        "expires_at": node.expires_at,
        // Counted on the hub's calendar, the one renewal follows. A page counting
        // on the visitor's clock would, with the hub on UTC and the visitor on
        // UTC+8, show every online node expired for eight hours each cycle
        // before the hub rolls its date forward.
        "expires_in": node.expires_at.as_deref().and_then(|d| d.parse::<NaiveDate>().ok()).map(|d| (d - today).num_days()),
        "traffic_limit": node.traffic_limit,
        "traffic_mode": node.traffic_mode,
        "traffic_reset_day": node.traffic_reset_day,
        "total_rx": traffic.total_rx,
        "total_tx": traffic.total_tx,
        "month_rx": traffic.month_rx,
        "month_tx": traffic.month_tx,
        "month_start": traffic.month_start,
        // Of the same nature as the month and lifetime figures beside it, which
        // the public page already shows, so this one is public as well.
        "day_rx": traffic.day_rx,
        "day_tx": traffic.day_tx,
        // Reporting fraction over the two windows the public page quotes, and
        // the windows themselves so the page can say which span it is quoting. A
        // fraction rather than a percentage so the rounding is the reader's.
        "uptime": {
            "d7": uptime.d7,
            "d30": uptime.d30,
            "from7": uptime.from7,
            "from30": uptime.from30,
            "to": uptime.to,
        },
    });
    // An allowlist rather than a denylist: the agent ships from its own
    // repository, so a field added there would otherwise reach anonymous visitors
    // the day it is released. No address, hostname or note may ever do so.
    if !full {
        if let Some(m) = view["metrics"].as_object_mut() {
            m.retain(|k, _| PUBLIC_METRICS.contains(&k.as_str()));
        }
    }
    // Address, private notes and the token never leave the panel. The token is
    // included so the install command can be displayed without reissuing it.
    if full {
        view["hostname"] = json!(node.hostname);
        view["ip"] = json!(node.ip);
        view["ipv4"] = json!(node.ipv4);
        view["ipv6"] = json!(node.ipv6);
        view["remark"] = json!(node.remark);
        view["token"] = json!(node.token);
        view["notify"] = json!(node.notify);
    }
    view
}

fn visible_nodes(app: &App, full: bool) -> Result<Vec<Value>, anyhow::Error> {
    // One traffic query, one uptime aggregate and one lock for the whole list,
    // since this is what every visitor to the public page loads. Uptime is keyed
    // on each node's `created_at`, so the map is built from the node list just
    // read rather than from the aggregate alone.
    let nodes = app.db.nodes()?;
    let traffic = app.db.all_traffic();
    let uptime = uptime_map(app, Utc::now().timestamp(), &nodes);
    let agents = app.agents.read().unwrap_or_else(|e| e.into_inner());
    let none = Traffic::default();
    let today = Local::now().date_naive();
    Ok(nodes
        .iter()
        .filter(|n| full || n.public)
        .map(|n| {
            node_view(
                n,
                agents.get(&n.id),
                traffic.get(&n.id).unwrap_or(&none),
                uptime.get(&n.id).copied().unwrap_or_default(),
                full,
                today,
            )
        })
        .collect())
}

pub async fn nodes(State(app): State<Shared>, headers: HeaderMap) -> Response {
    let full = authed(&app, &headers);
    if !full && !app.public_page() {
        return (StatusCode::UNAUTHORIZED, "sign-in required").into_response();
    }
    // The same rendered frame the browser streams receive, for the same reason:
    // otherwise every visitor would rebuild every node's row against the
    // connection the agents write through.
    ([(axum::http::header::CONTENT_TYPE, "application/json")], live_snapshot(&app, full).as_str().to_owned())
        .into_response()
}

#[derive(Deserialize)]
pub struct Window {
    #[serde(default = "default_hours")]
    hours: i64,
    /// How many points the caller can draw. Absent means the full budget.
    points: Option<i64>,
    /// Which series the caller will draw, comma-separated: `metrics`, `ping`,
    /// `availability`. Each tab draws a subset, and the rest accounted for a
    /// third to two thirds of every response. Absent means all of them, which is
    /// what a bare curl gets.
    ///
    /// A list rather than one name because a tab can draw more than one: the
    /// detail page draws the availability bar over whichever chart it is showing,
    /// and asking twice would open the page with a bar that follows the chart.
    /// Naming the series also matters for reproducibility -- `availability`
    /// carries the minute the window ends on, so a caller comparing two
    /// responses byte for byte must be able to leave it out.
    series: Option<String>,
}

fn default_hours() -> i64 {
    6
}

/// How many history windows are built concurrently.
///
/// `PUBLIC_HOURS` bounds what one request costs; this bounds how many may run,
/// closing the same gap `main::RELAY_GATE` and `auth::PASSWORD_GATE` close on
/// the other two paths an anonymous caller can make expensive. This is the most
/// expensive of the three: every request holds the single connection the agents
/// report through for its entire scan, measured at 118 ms for a week of four
/// probes and growing with `retention_days`. Without a gate, 120 requests from
/// one machine took the panel's own node list from 1 ms to 2.8 s.
///
/// Four, because the requests serialise on that one connection regardless: a
/// fifth in flight buys no throughput and merely places another scan ahead of
/// the next agent report. What the number actually sets is how long that wait
/// can become -- four at roughly 120 ms is half a second -- while leaving room
/// for several people opening charts simultaneously.
///
/// Refused rather than queued, as in `auth`: a queue admits the same flood,
/// merely later.
///
/// **This gate is ineffective without the `spawn_blocking` below.** The body of
/// this handler never awaits, so a permit taken and dropped within it is held
/// only while a worker thread is actually running the handler -- at most one per
/// worker, three on this hub. Measured at eight: 120 concurrent requests, zero
/// refusals, the panel still at 14 s. This is the same constraint
/// `PASSWORD_CHECKS` is sized against, approached from the other side: not a
/// value too high for the machine, but a handler that cannot hold more permits
/// than the machine has threads. Moving the scan off the runtime is what makes
/// "in flight" meaningful, and is what every other heavy query here already
/// does.
const HISTORY_SLOTS: usize = 4;
static HISTORY_GATE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(HISTORY_SLOTS);

pub async fn metrics(
    State(app): State<Shared>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Query(w): Query<Window>,
) -> Response {
    let full = authed(&app, &headers);
    if !readable(&app, full, id) {
        return (StatusCode::UNAUTHORIZED, "sign-in required").into_response();
    }
    // After the two point lookups above, so an unauthorised caller is told so
    // rather than asked to retry later.
    let Ok(_permit) = HISTORY_GATE.try_acquire() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "too many history queries in flight, try again")
            .into_response();
    };
    let hours = w.hours.clamp(1, if full { ADMIN_HOURS } else { PUBLIC_HOURS });
    let now = Utc::now().timestamp();
    let since = now - hours * 3_600;
    let step = sample_step(hours, w.points);
    // Comma-separated, so a tab that draws two series can name both and get them
    // on one request. Empty (the field absent) means every series.
    let series: Vec<&str> = w.series.as_deref().unwrap_or("").split(',').filter(|s| !s.is_empty()).collect();
    let wants = |name: &str| series.is_empty() || series.contains(&name);
    let (want_metrics, want_ping, want_availability) =
        (wants("metrics"), wants("ping"), wants("availability"));
    // Off the runtime, for the reason given in `db_stats` below: this reads every
    // probe result the node has retained within the window and holds the
    // connection the agents report through throughout. That route is behind
    // `Admin` and cheaper than this one, which anyone can reach.
    //
    // It is also what makes the gate above effective: the permit is held across
    // an await, so exactly four callers are inside it at once rather than however
    // many worker threads happen to exist.
    let built = tokio::task::spawn_blocking(move || {
        // Probe names accompany the samples they label, so the page needs no
        // second request. Names only: targets and assignments remain behind
        // `Admin`. Skipped when probes were not requested, since the resources tab
        // has nothing to label and this costs a turn at the write connection.
        let probes =
            if want_ping { app.db.ping_task_names(id).unwrap_or_else(|_| json!({})) } else { json!({}) };
        let metrics = if want_metrics { app.db.metrics(id, since, step)? } else { vec![] };
        // `loss` is per probe across the whole window, alongside the per-bucket
        // `loss` on the rows. Both are required and neither replaces the other:
        // the row figure is what a tooltip reads, while the window figure is the
        // only one that can be accurate, since the denominators it divides by are
        // gone by the time the rows are built. Additive, so a theme unaware of it
        // continues to work.
        let (ping, loss) =
            if want_ping { app.db.ping_records(id, since, step)? } else { (vec![], json!({})) };
        // The bar rides with whichever chart is drawn, and is skipped for a
        // caller that did not name it: unlike the charts it is a few hundred
        // numbers, but it is also the one series stamped with the minute the
        // window ends on, so a caller comparing responses needs to be able to
        // leave it out.
        let availability = if want_availability {
            // The node's own start, so the bar cannot darken the part of the
            // window before it existed, and the retention boundary, so it cannot
            // either paint history the hub has pruned as an outage. One row by
            // primary key, inside the permit rather than before it: read outside,
            // this lookup would lengthen the pre-gate path for *every* caller,
            // including the refused ones, and `HISTORY_GATE` is what decides how
            // long a refusal is pinned to the agents' write connection.
            let created = app.db.node(id).ok().flatten().map(|n| n.created_at).unwrap_or(0);
            let (from, to) = uptime_window(created, since.max(retained_from(&app, now)), now);
            let minutes = app.db.reported_minutes(id, from, to)?;
            availability(&minutes, from, to)
        } else {
            Value::Null
        };
        anyhow::Ok(json!({"metrics": metrics, "ping": ping, "probes": probes, "loss": loss,
                   "availability": availability}))
    })
    .await;
    match built.map_err(|e| anyhow::anyhow!(e)).and_then(|r| r) {
        Ok(body) => Json(body).into_response(),
        Err(e) => fail(e),
    }
}

/// Widest history window each audience may request.
///
/// The thinning below bounds the response, not the scan behind it: `hours=2160`
/// returns 320 rows after reading every probe result the node has retained. At a
/// month of retention that is 224 ms holding the single write connection the
/// agents report through, growing with `retention_days`.
///
/// The public ceiling is a week because that is the widest chart the themes
/// draw, so nothing in use is lost. The panel retains the quarter year, being
/// one signed-in operator rather than an anonymous caller.
const PUBLIC_HOURS: i64 = 24 * 7;
const ADMIN_HOURS: i64 = 24 * 90;

/// Seconds between the samples a window is drawn from.
///
/// Thinning exists for what the screen cannot draw rather than as a convention:
/// where the samples fit, every one is sent. A chart of a hundred points reads
/// as a hundred samples taken, which for a probe is a claim about the network.
/// Whole minutes, matching the grid the metric rows sit on.
///
/// `points` is what the caller reports it can draw, and can only lower the
/// budget: `SAMPLES` is the hub's ceiling rather than the caller's, set at a day
/// of minutes so the widest charted probe window returns intact.
// ponytail: the budget is per series, so a response is SAMPLES × (1 + probes) --
// bounded by how many probes the admin created, not by the caller. Four probes
// at a day is ~90 kB gzipped; if that list ever grows long, scale SAMPLES by
// the probe count.
fn sample_step(hours: i64, points: Option<i64>) -> i64 {
    const SAMPLES: i64 = 1_440;
    let budget = points.unwrap_or(SAMPLES).clamp(60, SAMPLES);
    // Rounded up, or the budget would not be one: a window that does not divide
    // evenly would keep the finer step and exceed it. `i64::div_ceil` is still
    // unstable, and both operands are positive here.
    60 * ((hours * 60 + budget - 1) / budget).max(1)
}

/// Guards a per-node read: the panel sees everything, while the public page sees
/// only nodes explicitly published. `full` is the caller's own `authed`, passed
/// in because the handler also needs it for the window ceiling.
fn readable(app: &App, full: bool, id: i64) -> bool {
    full || (app.public_page() && app.db.node(id).ok().flatten().is_some_and(|n| n.public))
}

/// Per-connection read buffer for both WebSocket surfaces. The 128 KiB default
/// would be tens of megabytes across a few hundred agents, for frames a few
/// hundred bytes long.
pub const SOCKET_BUFFER: usize = 4 * 1024;

/// Largest frame either socket accepts, matching the 64 KiB cap on the HTTP
/// body. That limit is a tower layer and never applies here, where the default
/// ceiling is 64 MiB -- reachable with a node's own token, for content that is
/// stored and then served to every viewer of the public page.
pub const MAX_FRAME: usize = 64 * 1024;

/// How long one rendered snapshot is reused. Just under the push interval, so
/// every tick rebuilds once and no viewer receives a stale frame twice.
const SNAPSHOT_TTL_MS: i64 = 1_900;

/// The payload every browser stream sends, built at most once per tick however
/// many tabs are watching: the public page is anonymous, so a per-connection
/// build would make viewer count a multiplier on database work. Two slots,
/// because the admin view carries fields the public one must never expose.
fn live_snapshot(app: &App, full: bool) -> Utf8Bytes {
    let now = Utc::now().timestamp_millis();
    let slot = usize::from(full);
    let mut cache = app.snapshot.lock().unwrap_or_else(|e| e.into_inner());
    // A cached frame's age must be non-negative. A wall clock can step backwards
    // -- NTP correcting a fresh boot -- and against a bare upper bound the
    // resulting negative reads as young, pinning the panel to a stale frame until
    // real time catches up.
    if (0..SNAPSHOT_TTL_MS).contains(&now.saturating_sub(cache[slot].0)) {
        return cache[slot].1.clone();
    }
    let nodes = visible_nodes(app, full).unwrap_or_default();
    // `admin` is included so the panel's first fetch and its stream share one
    // cached frame.
    let payload = Utf8Bytes::from(json!({"nodes": nodes, "admin": full}).to_string());
    cache[slot] = (now, payload.clone());
    payload
}

/// Drops the cached frames so the next push rebuilds. Without it a node just
/// added in the panel would disappear from the list until the frame expires.
///
/// The uptime aggregate is dropped with them despite its longer life: a node
/// added now has a `created_at` the cached map was built without, and waiting a
/// minute for a new node to be counted is exactly the window in which its
/// figures are being checked.
fn invalidate_snapshot(app: &App) {
    for slot in app.snapshot.lock().unwrap_or_else(|e| e.into_inner()).iter_mut() {
        slot.0 = 0;
    }
    app.uptime.lock().unwrap_or_else(|e| e.into_inner()).0 = 0;
}

/// What one tick of a browser stream may send: the admin frame while the session
/// that opened it remains live, the public frame while the status page remains
/// open to anonymous callers, and nothing once either ceases to hold.
///
/// Both are checked every tick rather than at the handshake alone, because a
/// socket outlives both answers. The admin frame carries every node's token in
/// the clear, so one outliving its session would distribute credentials that
/// survive revocation -- the same gap `reset_token` closes on the agent side by
/// dropping its sender. The public frame is what an operator withdraws by
/// switching the status page off, and a socket opened a minute earlier would
/// continue sending it for as long as the tab stayed open: `live_ws` refuses new
/// anonymous connections from that moment and `nodes` answers them 401, leaving
/// this the only remaining route. Whatever the handshake tested must be tested
/// here as well.
fn stream_audience(app: &App, session: Option<&str>) -> Option<bool> {
    match session {
        Some(hash) => app.db.session_valid(hash).then_some(true),
        None => app.public_page().then_some(false),
    }
}

/// Live stream for the browser. Each connection runs its own timer -- simpler to
/// reason about than a fan-out channel -- over a shared snapshot, so a timer
/// costs no more than a send.
pub async fn live_ws(State(app): State<Shared>, headers: HeaderMap, upgrade: WebSocketUpgrade) -> Response {
    // The digest rather than the result: signing out must reach a stream already
    // running, and only the row it names can report whether it has.
    let session = current_session(&headers).filter(|hash| app.db.session_valid(hash));
    if session.is_none() && !app.public_page() {
        return (StatusCode::UNAUTHORIZED, "sign-in required").into_response();
    }
    upgrade
        .read_buffer_size(SOCKET_BUFFER)
        .max_message_size(MAX_FRAME)
        .on_upgrade(move |socket| stream_live(app, socket, session))
}

async fn stream_live(app: Shared, mut socket: WebSocket, session: Option<String>) {
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(2));
    loop {
        ticker.tick().await;
        // Closed rather than downgraded to the public frame, which would leave the
        // panel rendering a list with every admin field missing. The close allows
        // a client to re-query /api/me and determine its current state.
        let Some(full) = stream_audience(&app, session.as_deref()) else { break };
        if socket.send(Message::Text(live_snapshot(&app, full))).await.is_err() {
            break;
        }
    }
}

// ---- panel write paths ----

/// Names all three causes. A reverse proxy that does not preserve Host forwards
/// its own upstream address, which is an IP and therefore never an https domain
/// entry, while the admin reading this is already on the domain -- so the first
/// clause alone would point them in the wrong direction. The third is `--site`,
/// the one input to this decision that nothing about the request reveals: a hub
/// started with `--site https://198.51.100.7` refuses every provisioning call
/// from an otherwise valid https domain entry. `main` warns about that at
/// startup; this is for whoever reads the panel rather than the journal.
const PROVISIONING_DENIED: &str = "请通过 HTTPS 域名访问面板后添加或安装节点；\
     如果已经是域名访问，检查反向代理是否透传了 Host 与 X-Forwarded-Proto（见 README 的反代配置）；\
     两者都没问题就检查 hub 的启动参数 --site，它必须是 https:// 加域名，不能是 IP、不能带路径";

pub(crate) fn https_domain(site: &str) -> Option<reqwest::Url> {
    let url = reqwest::Url::parse(site).ok()?;
    (url.scheme() == "https"
        && url.domain().is_some_and(|d| d != "localhost" && !d.ends_with(".localhost"))
        && url.username().is_empty()
        && url.password().is_none()
        && url.path() == "/"
        && url.query().is_none()
        && url.fragment().is_none())
    .then_some(url)
}

/// Host and the proxy's scheme describe this request; --site must not turn an IP
/// entry point into a domain entry point. The listener remains behind the trusted
/// reverse proxy, which must preserve Host and set X-Forwarded-Proto.
///
/// Every refusal names which half failed. Without that, a proxy configured with a
/// bare `proxy_pass` -- nginx then forwards `Host: 127.0.0.1:28080`, as does
/// Apache under its default `ProxyPreserveHost Off` -- is indistinguishable from
/// a genuine IP entry point: provisioning stops working across an upgrade, the
/// message implicates the address bar, and nothing records the header actually
/// responsible.
fn provisioning_allowed(app: &App, headers: &HeaderMap) -> bool {
    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        debug!("provisioning refused: the request carries no readable Host header");
        return false;
    };
    let forwarded = crate::forwarded_proto(headers);
    let https = forwarded.map_or_else(|| app.site.starts_with("https://"), |scheme| scheme == "https");
    if !https || (!app.site.is_empty() && https_domain(&app.site).is_none()) {
        debug!(
            "provisioning refused: not an https domain entry (X-Forwarded-Proto={forwarded:?}, --site={:?}); \
             a TLS-terminating proxy has to send X-Forwarded-Proto: https",
            app.site
        );
        return false;
    }
    let Some(url) = https_domain(&format!("https://{host}")) else {
        debug!(
            "provisioning refused: Host {host:?} is not an https domain entry; a reverse proxy that does \
             not preserve Host sends its own upstream address here -- nginx needs \
             `proxy_set_header Host $host`, Apache `ProxyPreserveHost On`"
        );
        return false;
    };
    let expected = url.origin().ascii_serialization();
    let allowed =
        headers.get(header::ORIGIN).is_none_or(|origin| origin.to_str().ok() == Some(expected.as_str()));
    if !allowed {
        debug!("provisioning refused: Origin {:?} is not {expected}", headers.get(header::ORIGIN));
    }
    allowed
}

/// Range and sign limits every stored node must satisfy, or the reason it does
/// not. Shared because both writers must enforce them: the create path formerly
/// accepted a whole `Node` unchecked, leaving the values the update path refuses
/// reachable by another route, and an out-of-range reset day remained harmless
/// only because `period_start` clamps what it reads.
fn node_limits(reset_day: Option<u32>, price: Option<f64>, limit: Option<i64>) -> Option<&'static str> {
    if reset_day.is_some_and(|d| !(1..=31).contains(&d)) {
        return Some("reset day must be from 1 to 31");
    }
    if price.is_some_and(|v| !v.is_finite() || v < 0.0) || limit.is_some_and(|v| v < 0) {
        return Some("price and traffic limit must be non-negative");
    }
    None
}

pub async fn me(State(app): State<Shared>, headers: HeaderMap) -> Response {
    let authed = authed(&app, &headers);
    // Which login this browser holds -- and only for the browser holding it. The public
    // page calls this endpoint too, and an anonymous visitor gets an empty string. The
    // response varies by cookie, so it must never be cached: `no-store` says exactly
    // that to any proxy between here and the panel.
    let login = if authed {
        current_session(&headers).and_then(|h| app.db.session_login(&h)).unwrap_or_default()
    } else {
        String::new()
    };
    // The panel shows which hub it is talking to. Reported only to a signed-in browser:
    // an anonymous visitor has no use for it, and a version is worth not handing out.
    let version = if authed { env!("CARGO_PKG_VERSION") } else { "" };
    let body = json!({
        "authed": authed,
        "login": login,
        "version": version,
        "github": app.db.get("github_client_id").is_some_and(|v| !v.is_empty()),
        "site_name": app.db.get("site_name").unwrap_or_else(|| "Monitor".into()),
        "public_page": app.public_page(),
        "can_provision": provisioning_allowed(&app, &headers),
        // The hub's own public URL when one was given, which is what belongs in an
        // install command and in the OAuth callback -- not whichever address this
        // browser used, which behind a proxy may be a loopback port. Empty by
        // default, in which case the browser's address is the only one available
        // and the panel falls back to its own origin.
        "site": app.site,
    });
    ([(header::CACHE_CONTROL, "no-store")], Json(body)).into_response()
}

pub async fn create_node(
    _: Admin,
    State(app): State<Shared>,
    headers: HeaderMap,
    body: Result<Json<Node>, JsonRejection>,
) -> Response {
    if !provisioning_allowed(&app, &headers) {
        return (StatusCode::FORBIDDEN, PROVISIONING_DENIED).into_response();
    }
    let Ok(Json(mut node)) = body else { return bad("invalid node") };
    if node.name.trim().is_empty() {
        return bad("name is required");
    }
    if let Some(message) =
        node_limits(Some(node.traffic_reset_day), Some(node.price), Some(node.traffic_limit))
    {
        return bad(message);
    }
    node.name = node.name.trim().to_owned();
    let token = random_token();
    match app.db.create_node(&node, &token) {
        // Usable immediately: the install command is readable from the node list,
        // so adding and deploying require no reissue in between.
        Ok(id) => {
            invalidate_snapshot(&app);
            Json(json!({"id": id})).into_response()
        }
        Err(e) => fail(e),
    }
}

// ---- automatic registration ----

/// How long a registration window stays open.
///
/// Provisioning a batch of machines takes minutes, and the window expires on its
/// own rather than depending on someone returning to close it.
const REGISTER_WINDOW: i64 = 3600;

/// How many nodes one window may register.
///
/// Without it, whoever holds the key for the hour could fill the node table. A
/// hundred is well beyond a plausible batch and well short of a problem.
const REGISTER_LIMIT: i64 = 100;

/// Exchanges a registration key for a node token, so a batch of machines can be
/// installed with one command rather than one panel visit each.
///
/// No session stands behind this route: the caller is `install.sh` on a machine
/// that has never contacted the hub. A key issued by the panel, valid only within
/// [`REGISTER_WINDOW`], serves in place of a session.
///
/// One request costs two setting reads, a `COUNT` and an `INSERT`. It makes no
/// outbound request, and the router's 64 KiB body limit bounds the name.
pub async fn agent_register(
    State(app): State<Shared>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    // Plain text in, plain text out. The caller is a shell script, as with this
    // route's neighbours: `/install.sh` and `/agent/{arch}` return a script and a
    // binary. A bare token is one `$(curl ...)` away, requiring no JSON parser in
    // a POSIX `sh`.
    name: String,
) -> Response {
    if !provisioning_allowed(&app, &headers) {
        return (StatusCode::FORBIDDEN, PROVISIONING_DENIED).into_response();
    }
    let ip = client_ip(&headers, peer.ip());
    // Counted separately from the sign-in page: a batch install started with a
    // stale key is a misconfigured deploy rather than an attack on the panel, and
    // a shared counter would lock the operator out of their own hub for LOCKOUT.
    if app.registrations.locked(ip) {
        return (StatusCode::TOO_MANY_REQUESTS, "too many attempts, try again later").into_response();
    }
    // One answer for both "no window is open" and "that key is wrong": the
    // difference is only useful to someone who has neither.
    let closed = || (StatusCode::FORBIDDEN, "registration is closed").into_response();
    let until = app.db.get("register_until").and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
    let Some(key) = app.db.get("register_key").filter(|k| !k.is_empty() && Utc::now().timestamp() < until)
    else {
        return closed();
    };
    if agent_ws::bearer(&headers) != Some(key.as_str()) {
        // Only an incorrect key counts against the address. With the window
        // closed there is no secret to guess, and counting then would let anyone
        // lock an address they name in `X-Forwarded-For` out of the sign-in page.
        app.registrations.record_failure(ip);
        return closed();
    }
    match app.db.nodes_created_since(until - REGISTER_WINDOW) {
        Ok(n) if n >= REGISTER_LIMIT => {
            return (StatusCode::FORBIDDEN, "this window has registered enough nodes").into_response()
        }
        Err(e) => return fail(e),
        Ok(_) => {}
    }

    // The name comes from a machine not yet vouched for: control characters would
    // break the panel's rows, and the length must be bounded. `chars()` rather
    // than bytes, so the cut falls on a character boundary.
    let name: String = name.trim().chars().filter(|c| !c.is_control()).take(64).collect();
    let name = if name.is_empty() { "unnamed".to_owned() } else { name };
    // Field defaults live in `Node`'s serde attributes and nowhere else.
    // `Node::default()` is a different set of values -- private, reset day 0 --
    // and a node registered here must match one added through the panel.
    let node = match serde_json::from_value::<Node>(json!({ "name": name })) {
        Ok(node) => node,
        Err(e) => return fail(e),
    };
    let token = random_token();
    match app.db.create_node(&node, &token) {
        Ok(_) => {
            app.registrations.clear(ip);
            invalidate_snapshot(&app);
            token.into_response()
        }
        Err(e) => fail(e),
    }
}

/// Opens a registration window with a fresh key. Any previous key stops working
/// the moment this returns.
pub async fn open_register(_: Admin, State(app): State<Shared>, headers: HeaderMap) -> Response {
    if !provisioning_allowed(&app, &headers) {
        return (StatusCode::FORBIDDEN, PROVISIONING_DENIED).into_response();
    }
    let key = random_token();
    let until = (Utc::now().timestamp() + REGISTER_WINDOW).to_string();
    match app.db.set("register_key", &key).and_then(|()| app.db.set("register_until", &until)) {
        Ok(()) => Json(json!({"register_key": key, "register_until": until})).into_response(),
        Err(e) => fail(e),
    }
}

/// Closes the window early, before the hour elapses.
pub async fn close_register(_: Admin, State(app): State<Shared>) -> Response {
    match app.db.set("register_key", "").and_then(|()| app.db.set("register_until", "0")) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => fail(e),
    }
}

pub async fn update_node(
    _: Admin,
    State(app): State<Shared>,
    Path(id): Path<i64>,
    body: Result<Json<NodePatch>, JsonRejection>,
) -> Response {
    let Ok(Json(mut node)) = body else { return bad("invalid node") };
    if let Some(name) = &mut node.name {
        *name = name.trim().to_owned();
        if name.is_empty() {
            return bad("name is required");
        }
    }
    if let Some(message) = node_limits(node.traffic_reset_day, node.price, node.traffic_limit) {
        return bad(message);
    }
    match app.db.update_node(id, &node) {
        Ok(true) => {
            invalidate_snapshot(&app);
            Json(json!({"ok": true})).into_response()
        }
        Ok(false) => no_such_node(),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
pub struct NodeOrder {
    ids: Vec<i64>,
}

/// The list must name every node exactly once, checked inside the transaction
/// that renumbers rather than here: re-reading the node list first would only
/// race the write it guards.
pub async fn reorder_nodes(_: Admin, State(app): State<Shared>, Json(order): Json<NodeOrder>) -> Response {
    match app.db.reorder_nodes(&order.ids) {
        Ok(()) => {
            invalidate_snapshot(&app);
            Json(json!({"ok": true})).into_response()
        }
        // Every failure here indicates a malformed list from the caller.
        Err(e) => bad(&e.to_string()),
    }
}

pub async fn delete_node(_: Admin, State(app): State<Shared>, Path(id): Path<i64>) -> Response {
    match app.db.delete_node(id) {
        Ok(true) => {}
        Ok(false) => return no_such_node(),
        Err(e) => return fail(e),
    }
    // The token is checked only at the handshake, so deleting the row does not
    // end a connection already open on it; dropping the sender does. Without
    // this the agent would keep reporting under an id SQLite reassigns to the
    // next node created, which would then appear online on another node's
    // metrics. Dropped after the delete, so the reconnect that follows finds no
    // token to accept. The same reasoning applies in `reset_token` below.
    app.agents.write().unwrap_or_else(|e| e.into_inner()).remove(&id);
    invalidate_snapshot(&app);
    Json(json!({"ok": true})).into_response()
}

/// Issues a fresh token, invalidating the old one immediately.
///
/// Always an explicit action: rotate a token believed to have leaked, then
/// reinstall the agent. Reading the install command does not pass through here.
pub async fn reset_token(_: Admin, State(app): State<Shared>, Path(id): Path<i64>) -> Response {
    let token = random_token();
    match app.db.reset_token(id, &token) {
        Ok(true) => {}
        Ok(false) => return no_such_node(),
        Err(e) => return fail(e),
    }
    // The token is checked only at the handshake, so a session opened with the
    // old one would continue reporting. Dropping the sender ends that loop; the
    // agent reconnects and is refused. Its own teardown leaves the entry
    // untouched, because the session tag no longer matches.
    app.agents.write().unwrap_or_else(|e| e.into_inner()).remove(&id);
    // The token is part of the admin frame, which would otherwise continue to
    // display an install command for the credential just retired.
    invalidate_snapshot(&app);
    // The token alone: the panel builds the command, and one place needs to know
    // its form.
    Json(json!({"token": token})).into_response()
}

pub async fn patch_traffic(
    _: Admin,
    State(app): State<Shared>,
    Path(id): Path<i64>,
    Json(p): Json<TrafficPatch>,
) -> Response {
    if [p.total_rx, p.total_tx, p.month_rx, p.month_tx].into_iter().flatten().any(|v| v < 0) {
        return bad("traffic must be non-negative");
    }
    match app.db.set_traffic(id, &p) {
        Ok(true) => {
            invalidate_snapshot(&app);
            Json(json!({"ok": true})).into_response()
        }
        Ok(false) => no_such_node(),
        Err(e) => fail(e),
    }
}

pub async fn ping_tasks(_: Admin, State(app): State<Shared>) -> Response {
    match app.db.ping_tasks() {
        Ok(tasks) => Json(json!({"tasks": tasks})).into_response(),
        Err(e) => fail(e),
    }
}

/// A probe target the agent can resolve: `host:port`, with an IPv6 literal
/// bracketed as a URL writes one.
///
/// A bare `contains(':')` admitted three forms that never connect: a bare IPv6
/// address, which is all colons; `:443` with no host; and `host:` with no port.
/// The agent's `lookup_host` errors on each, `tcp_ping` returns -1, and the chart
/// draws a probe at 100% loss indefinitely with nothing in any log identifying
/// the target as the cause.
fn valid_target(target: &str) -> bool {
    let (host, port) = match target.strip_prefix('[') {
        Some(rest) => match rest.split_once("]:") {
            Some(pair) => pair,
            None => return false,
        },
        // Unbracketed, so the last colon is the port separator; anything still
        // containing a colon is an IPv6 address that required brackets.
        None => match target.rsplit_once(':') {
            Some((host, port)) if !host.contains(':') => (host, port),
            _ => return false,
        },
    };
    !host.is_empty() && port.parse::<u16>().is_ok_and(|p| p > 0)
}

pub async fn save_ping_task(_: Admin, State(app): State<Shared>, Json(mut task): Json<PingTask>) -> Response {
    // Trimmed into the stored value rather than a discarded copy: what reaches
    // the agent is `task.target`, and a trailing space from a paste passes
    // `valid_target` while `lookup_host` rejects the stored string outright,
    // leaving the probe reporting -1 indefinitely. The name is trimmed for the
    // same reason, as it travels to the public page as a chart label.
    task.name = task.name.trim().to_owned();
    task.target = task.target.trim().to_owned();
    if task.name.is_empty() || task.target.is_empty() {
        return bad("name and target are required");
    }
    // A TCP probe requires an explicit port; a bare host would silently never
    // connect.
    if !valid_target(&task.target) {
        return bad("target must be host:port, for example 1.1.1.1:443 or [2606:4700:4700::1111]:443");
    }
    // Refused rather than clamped, for the reason `setting_error` gives for
    // `retention_days`: the agent clamps this again on arrival, so an
    // out-of-range value never fails but silently becomes a different number
    // while the panel still displays what was entered. Below the floor that
    // number is 5 seconds, the fastest probe available, run by every node the
    // task is assigned to; the panel reaches 0 simply by having its interval
    // field cleared.
    if !(5..=3_600).contains(&task.interval) {
        return bad("interval must be from 5 to 3600 seconds");
    }
    match app.db.save_ping_task(&task) {
        Ok(id) => {
            agent_ws::push_ping_tasks(&app);
            Json(json!({"id": id})).into_response()
        }
        // Every failure here originates with the caller: a node id that does not
        // exist, or more probes on one node than the agent will run. The same
        // reasoning as `reorder_nodes`.
        Err(e) => bad(&e.to_string()),
    }
}

pub async fn delete_ping_task(_: Admin, State(app): State<Shared>, Path(id): Path<i64>) -> Response {
    match app.db.delete_ping_task(id) {
        Ok(()) => {
            agent_ws::push_ping_tasks(&app);
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => fail(e),
    }
}

/// Settings the panel may read. Secrets are deliberately excluded: the client can
/// set the GitHub secret but never read it back.
const READABLE_SETTINGS: &[&str] = &[
    "site_name",
    "public_page",
    "github_client_id",
    "github_allowed_users",
    "retention_days",
    "theme",
    "github_proxy",
];

// ---- the database itself ----

/// The largest single request the two upload routes accept, and the reason they
/// sit outside the router's 64 KiB body limit. It is twice the 4 MiB the panel
/// sends, so the chunk size remains the panel's concern alone and requires no
/// negotiated handshake.
///
/// **This, not the two ceilings below, is what a reverse proxy must pass.** A
/// backup of any size arrives 4 MiB at a time, so `client_max_body_size` no
/// longer tracks the size of the database.
pub const MAX_CHUNK: usize = 8 * 1024 * 1024;

/// Whole-file ceilings, one per route, checked against the declared `total` on
/// the first request rather than by counting bytes as they arrive, so an
/// oversized upload is refused before a byte is sent.
///
/// The backup ceiling is set where it is because restoring holds the connection
/// every read and write passes through: at the measured ~40 MB/s that is roughly
/// 6.5 seconds during which the panel and the public page also wait. Database
/// sizes reachable with a few hundred nodes sit two orders of magnitude below
/// it.
pub const MAX_RESTORE: u64 = 256 * 1024 * 1024;
pub const MAX_THEME: u64 = 32 * 1024 * 1024;

/// One request of an upload: `total` is the whole file, `offset` where this piece
/// belongs within it.
///
/// There is no upload id, session or server-side bookkeeping: the state of an
/// upload is the length of the file on disk. A piece continues an upload only if
/// it begins exactly where the last ended, `offset = 0` truncates whatever an
/// interrupted attempt left behind, and nothing is ever left to collect.
#[derive(Deserialize)]
pub struct Chunk {
    offset: u64,
    total: u64,
}

/// Appends one piece to `path`, returning the file's length afterwards; the
/// caller compares that against `total` to determine completion.
///
/// A piece lands whole or not at all -- a failure truncates back to where it
/// began -- so retrying one always aligns on the same offset.
///
/// ponytail: strictly sequential, one round trip per chunk. Concurrent pieces
/// would require pwrite, a commit step and a hash to prove there are no gaps,
/// and would save about a second on a 6.7 MB backup.
async fn receive(path: &str, chunk: &Chunk, max: u64, body: axum::body::Body) -> Result<u64, anyhow::Error> {
    if chunk.total == 0 || chunk.total > max {
        anyhow::bail!("文件必须在 1 字节到 {} MiB 之间", max / 1024 / 1024);
    }
    if chunk.offset > chunk.total {
        anyhow::bail!("分片位置越过了文件末尾");
    }

    let mut options = std::fs::OpenOptions::new();
    // Only the first piece may create the file, and it truncates: whatever an
    // interrupted upload left behind is overwritten rather than accumulated.
    if chunk.offset == 0 {
        options.write(true).create(true).truncate(true);
    } else {
        options.append(true);
    }
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            anyhow::bail!("这次上传已经不在了，请从头开始")
        }
        Err(e) => return Err(e.into()),
    };

    let already = file.metadata()?.len();
    if already != chunk.offset {
        anyhow::bail!("分片接不上：已经收到 {already} 字节，这一片却从 {} 开始", chunk.offset);
    }

    match append(&mut file, chunk, body).await {
        Ok(received) => Ok(chunk.offset + received),
        Err(e) => {
            // Undo a partially written piece so a retry aligns again.
            let _ = file.set_len(chunk.offset);
            Err(e)
        }
    }
}

/// Streams one request body onto the end of `file`. The byte count is checked
/// here as well as by the route's body limit: these are the only paths on the hub
/// that write a caller's bytes to disk, so they do not depend on a layer that
/// could be reordered away.
async fn append(
    file: &mut std::fs::File,
    chunk: &Chunk,
    body: axum::body::Body,
) -> Result<u64, anyhow::Error> {
    use std::io::Write;
    use std::pin::Pin;

    let mut stream = body.into_data_stream();
    let mut received = 0u64;
    while let Some(piece) =
        std::future::poll_fn(|cx| futures_core::Stream::poll_next(Pin::new(&mut stream), cx)).await
    {
        let piece = piece?;
        received += piece.len() as u64;
        if chunk.offset + received > chunk.total {
            anyhow::bail!("这一片超出了声明的文件大小");
        }
        file.write_all(&piece)?;
    }
    Ok(received)
}

/// A scratch file beside the database, so the copy lands on the same filesystem
/// the database has room on. The random component keeps two concurrent calls
/// apart, since `VACUUM INTO` refuses an existing file.
fn scratch_path(app: &App, kind: &str) -> String {
    format!("{}.{kind}-{}.tmp", app.db.file(), &random_token()[..16])
}

/// The data page's figures.
///
/// Off the runtime, like the three routes below: `stats` counts every row of
/// `metric` and `ping_record` -- both WITHOUT ROWID, so each count is a full
/// index scan -- holding the connection the agents report through throughout. At
/// 2.2M rows that is 127 ms during which the public page and every agent report
/// also wait, growing with `retention_days`.
pub async fn db_stats(_: Admin, State(app): State<Shared>) -> Response {
    match tokio::task::spawn_blocking(move || app.db.stats()).await {
        Ok(Ok(stats)) => Json(stats).into_response(),
        Ok(Err(e)) => fail(e),
        Err(e) => fail(anyhow::anyhow!(e)),
    }
}

/// Returns a compact copy of the whole database.
///
/// The copy is written beside the live file and then unlinked while still open,
/// so it exists only for the duration of this response: a client that
/// disconnects partway through leaves nothing behind, and nothing on disk
/// outlives the download.
pub async fn db_backup(_: Admin, State(app): State<Shared>) -> Response {
    let path = scratch_path(&app, "backup");
    // Off the runtime: this reads the entire database while holding the
    // connection the agents write through.
    let copied = {
        let (app, path) = (app.clone(), path.clone());
        tokio::task::spawn_blocking(move || app.db.backup_into(&path)).await
    };
    if let Err(e) = copied.map_err(|e| anyhow::anyhow!(e)).and_then(|r| r) {
        let _ = std::fs::remove_file(&path);
        return fail(e);
    }
    let opened = tokio::fs::File::open(&path).await;
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let _ = std::fs::remove_file(&path);
    match opened {
        Ok(file) => (
            [
                (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
                (header::CONTENT_LENGTH, size.to_string()),
                // The entire credential store: no shared cache may retain a copy.
                (header::CACHE_CONTROL, "no-store".to_owned()),
                (
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"monitor-{}.db\"", Local::now().format("%Y%m%d-%H%M%S")),
                ),
            ],
            axum::body::Body::from_stream(tokio_util::io::ReaderStream::new(file)),
        )
            .into_response(),
        Err(e) => fail(e),
    }
}

/// Replaces the live database with an uploaded backup, one chunk per request.
///
/// The upload streams to a file beside the database and is validated in full
/// before a single page is copied; see `Db::check_backup`. Afterwards every
/// session in the restored file is dropped and the caller is issued a new one: a
/// backup carries the session rows it held when taken, and restoring it must not
/// revive logged-out sessions.
pub async fn db_restore(
    _: Admin,
    State(app): State<Shared>,
    Query(chunk): Query<Chunk>,
    headers: HeaderMap,
    body: axum::body::Body,
) -> Response {
    // One fixed path, which is what allows the file's own length to constitute
    // the entire protocol.
    // ponytail: one upload in flight per hub. Two started simultaneously land on
    // this same name, and equal-sized chunks align their offsets, so they splice
    // rather than collide. The cost is a failed upload; distinguishing them would
    // require the upload id the protocol deliberately omits.
    let path = format!("{}.upload", app.db.file());
    let received = match receive(&path, &chunk, MAX_RESTORE, body).await {
        Ok(received) => received,
        Err(e) => return bad(&format!("{e:#}")),
    };
    if received < chunk.total {
        return Json(json!({"received": received})).into_response();
    }

    // Moved off the upload name before a byte is read. Splicing costs an upload;
    // what it must not cost is the live database, which without this it could:
    // the other upload would continue appending through its own handle while
    // `check_backup` reads the file and the page copy follows, and SQLite cannot
    // observe a write it did not make. A file that passed every gate would then
    // be copied over in a different state. Afterwards the other upload's next
    // chunk finds nothing and is told to restart, which is the error it already
    // has for an upload that disappeared.
    //
    // ponytail: the rename itself is not covered by a test. What it changes is
    // which path is open during the read, and reaching that would require a
    // second upload landing inside the copy. What is verified afterwards is that
    // neither name is left behind, in
    // `a_finished_restore_leaves_no_scratch_file_behind`.
    let source = scratch_path(&app, "restoring");
    if let Err(e) = std::fs::rename(&path, &source) {
        let _ = std::fs::remove_file(&path);
        return bad(&format!("上传收齐了却取不到文件：{e}"));
    }

    let outcome = restore(&app, &source).await;
    // SQLite writes a -wal and a -shm beside any file it opens in WAL mode, and a
    // plain copy of a running hub's database is exactly that. They are removed
    // when the connection closes cleanly; these three lines cover the case where
    // it does not.
    for leftover in [source.clone(), format!("{source}-wal"), format!("{source}-shm")] {
        let _ = std::fs::remove_file(leftover);
    }
    match outcome {
        Ok(()) => {
            // Agents authenticate at the handshake, and the tokens they hold may
            // now belong to different nodes, or to none. Dropping the senders ends
            // those loops; each reconnects against the restored database.
            app.agents.write().unwrap_or_else(|e| e.into_inner()).clear();
            invalidate_snapshot(&app);
            // Read the caller's own login before the wipe: the replacement session is
            // theirs, and telling someone who signed in with GitHub that they used the
            // emergency password would be a lie the panel cannot detect.
            let who = current_session(&headers).and_then(|h| app.db.session_login(&h)).unwrap_or_default();
            // No IP on these two paths: they reissue a session from a request that carries
            // no peer address, so the row records what it knows. The next sign-in fills it.
            let cookie =
                match app.db.drop_all_sessions().and_then(|()| issue_session(&app, &headers, &who, "")) {
                    Ok(cookie) => cookie,
                    Err(e) => return fail(e),
                };
            with_cookies(Json(json!({"ok": true})), [cookie])
        }
        Err(e) => bad(&format!("{e:#}")),
    }
}

async fn restore(app: &Shared, path: &str) -> Result<(), anyhow::Error> {
    // Both halves read the whole file, off the runtime: `PRAGMA integrity_check`
    // on a 256 MiB upload is not runtime work, and the copy that follows holds
    // the connection the agents write through.
    let (app, source) = (app.clone(), path.to_owned());
    tokio::task::spawn_blocking(move || {
        app.db.check_backup(&source)?;
        app.db.restore_from(&source)
    })
    .await?
}

/// Drops history beyond the retention window and rebuilds the file around what
/// remains, which is the only way SQLite returns the space to the filesystem.
pub async fn db_vacuum(_: Admin, State(app): State<Shared>) -> Response {
    let keep = app.db.retention_days();
    let app = app.clone();
    // A rebuild of the whole file, holding the connection the agents write
    // through, so it belongs on a blocking thread.
    let done = tokio::task::spawn_blocking(move || {
        let pruned = app.db.prune(keep)?;
        app.db.vacuum().map(|freed| json!({"pruned": pruned, "freed": freed}))
    })
    .await;
    match done.map_err(|e| anyhow::anyhow!(e)).and_then(|r| r) {
        Ok(result) => Json(result).into_response(),
        Err(e) => fail(e),
    }
}

/// Installs an uploaded theme archive, one chunk per request.
///
/// The archive lands in the themes directory under a name `valid_short` rejects,
/// so a partial upload is invisible to both the theme list and the public page.
/// Installation is performed by `frontend::install`, which unpacks to a staging
/// directory and publishes with a rename: the switch is atomic, and the page is
/// never served from a partially written directory.
pub async fn upload_theme(
    _: Admin,
    State(app): State<Shared>,
    Query(chunk): Query<Chunk>,
    body: axum::body::Body,
) -> Response {
    let path = app.themes.join(".upload.tar.gz");
    let name = path.to_string_lossy().into_owned();
    let received = match receive(&name, &chunk, MAX_THEME, body).await {
        Ok(received) => received,
        Err(e) => return bad(&format!("{e:#}")),
    };
    if received < chunk.total {
        return Json(json!({"received": received})).into_response();
    }

    // Moved off the shared upload name for the same reason as the restore path: a
    // second upload landing on it could continue writing while this archive is
    // read, leaving the unpacker reading a file changed beneath it. Named so
    // `valid_short` still rejects it, keeping a partial archive out of the theme
    // list.
    let source = app.themes.join(format!(".installing-{}.tar.gz", &random_token()[..16]));
    if let Err(e) = std::fs::rename(&path, &source) {
        let _ = std::fs::remove_file(&path);
        return bad(&format!("上传收齐了却取不到文件：{e}"));
    }

    // Off the runtime: gunzip plus a few thousand small writes.
    let installed = {
        let (app, path) = (app.clone(), source.clone());
        tokio::task::spawn_blocking(move || {
            crate::frontend::install(&app.themes, std::fs::File::open(&path)?, None)
        })
        .await
    };
    let _ = std::fs::remove_file(&source);
    match installed.map_err(|e| anyhow::anyhow!(e)).and_then(|r| r) {
        Ok(theme) => {
            crate::theme::installed(&app, &theme.short, &theme.version);
            Json(json!({"theme": theme})).into_response()
        }
        Err(e) => bad(&format!("{e:#}")),
    }
}

/// Reinstalls one theme from the latest GitHub release of the repository its
/// manifest names.
///
/// The manifest supplies `<owner>/<repo>` and nothing more: the release is read
/// from api.github.com and the archive from github.com, both at addresses the hub
/// constructs itself, so no URL from the theme is ever followed. The installed
/// version is compared against the release tag first, which is all most
/// invocations do, making this also the check-for-updates action -- the daily
/// check in `theme` only reports what this would find.
pub async fn update_theme(_: Admin, State(app): State<Shared>, Path(short): Path<String>) -> Response {
    match update(&app, &short).await {
        Ok((updated, version)) => Json(json!({"updated": updated, "version": version})).into_response(),
        Err(e) => bad(&format!("{e:#}")),
    }
}

async fn update(app: &App, short: &str) -> Result<(bool, String), anyhow::Error> {
    use anyhow::Context;

    let installed = crate::frontend::themes(app)?
        .into_iter()
        .find(|theme| theme.short == short)
        .context("没有这个主题")?;
    let (owner, repo) = crate::theme::repo(&installed.url)
        .context("这个主题的 url 不是 https://github.com/<owner>/<repo>，只能手动上传新包")?;
    let release = crate::theme::latest(app, owner, repo).await?;

    // Tags read `v1.2.3` while manifests carry `1.2.3`. Equal means up to date;
    // anything else is installed, including a deliberate downgrade, since the
    // release is what the author published.
    let version = crate::theme::strip_v(&release.tag_name);
    if version == installed.version {
        return Ok((false, installed.version));
    }
    let archive = crate::theme::archive(app, owner, repo, &release.tag_name).await?;

    // The same unpacking, validation and atomic replace an upload undergoes,
    // constrained to the theme it may replace. The built-in theme has no
    // directory until this runs: updating it writes one, which then serves in
    // place of the embedded copy until it is deleted.
    let (themes, short) = (app.themes.clone(), short.to_owned());
    let theme = tokio::task::spawn_blocking(move || {
        crate::frontend::install(&themes, std::io::Cursor::new(archive), Some(&short))
    })
    .await??;
    crate::theme::installed(app, &theme.short, &theme.version);
    Ok((true, theme.version))
}

/// The thumbnail the theme list displays, where the theme provides one. A theme
/// without one returns 404, on which the panel hides the image, so nothing need
/// report whether a preview exists.
pub async fn theme_preview(_: Admin, State(app): State<Shared>, Path(short): Path<String>) -> Response {
    match crate::frontend::preview(&app.themes, &short) {
        // Not cached: reinstalling a theme under the same name also replaces the
        // image, and this is a panel-only request for a local file.
        Some(png) => {
            ([(header::CONTENT_TYPE, "image/png"), (header::CACHE_CONTROL, "no-cache")], png).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Deletes an installed theme. Deleting the one in use is permitted: the public
/// page falls back to the built-in theme from the next request, the same path a
/// broken theme already takes, and leaving the setting intact means reinstalling
/// the theme restores it.
pub async fn delete_theme(_: Admin, State(app): State<Shared>, Path(short): Path<String>) -> Response {
    match crate::frontend::remove(&app.themes, &short) {
        Ok(()) => {
            crate::theme::removed(&app, &short);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => bad(&format!("{e:#}")),
    }
}

pub async fn themes(_: Admin, State(app): State<Shared>) -> Response {
    match crate::frontend::themes(&app) {
        // The update check's answer rides along with the list: it is about these
        // same themes, the panel is looking at them, and it is already polling
        // nothing else. A hub that has never reached GitHub reports an empty
        // result rather than an error, since a theme with no update to offer and
        // a repository that could not be read look the same to the reader.
        Ok(themes) => Json(json!({"themes": themes, "updates": crate::theme::state(&app)})).into_response(),
        Err(e) => fail(e),
    }
}

/// Every live session, with the caller's own marked.
///
/// `id` is the stored SHA-256 of the session token rather than the token itself:
/// it identifies a row without being presentable as a cookie.
pub async fn sessions(_: Admin, State(app): State<Shared>, headers: HeaderMap) -> Response {
    let mine = current_session(&headers);
    match app.db.sessions() {
        Ok(rows) => Json(
            rows.into_iter()
                .map(|(hash, expires_at, github_login, created_at, ip, user_agent, last_seen)| {
                    json!({
                        "current": mine.as_deref() == Some(hash.as_str()),
                        // Derived only as a fallback: a session from before schema 7 has no
                        // stored issue time, and this at least places it in time.
                        "created_at": if created_at > 0 { created_at } else { issued_at(expires_at) },
                        "id": hash,
                        "ip": ip,
                        "last_seen": last_seen,
                        "login": github_login,
                        "user_agent": user_agent,
                    })
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => fail(e),
    }
}

/// Deleting a row that no longer exists is not an error: two panels open on the
/// same list both achieve the requested sign-out.
pub async fn delete_session(_: Admin, State(app): State<Shared>, Path(id): Path<String>) -> Response {
    match app.db.drop_session(&id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => fail(e),
    }
}

pub async fn settings(_: Admin, State(app): State<Shared>) -> Json<Value> {
    let mut out = serde_json::Map::new();
    for key in READABLE_SETTINGS {
        out.insert((*key).to_owned(), json!(app.db.get(key).unwrap_or_default()));
    }
    // The one readable key with a default that also rejects the empty string:
    // `setting_error` below refuses "" and `save_settings` writes nothing when any
    // key fails, so a hub where this was never set returned "" here and then
    // rejected the entire settings form, naming a field that was never edited.
    // `retention_days()` already holds the default `prune` and the data page read,
    // so it answers here as well.
    out.insert("retention_days".into(), json!(app.db.retention_days().to_string()));
    out.insert(
        "github_secret_set".into(),
        json!(app.db.get("github_client_secret").is_some_and(|v| !v.is_empty())),
    );
    // Read-only here. A window is opened and closed through its own route, so the
    // key is always one the hub generated, and `save_settings` continues to refuse
    // both names.
    for key in ["register_key", "register_until"] {
        out.insert(key.into(), json!(app.db.get(key).unwrap_or_default()));
    }
    crate::notify::settings(&app, &mut out);
    Json(Value::Object(out))
}

/// Why one setting cannot be stored, or `None` when it can.
///
/// Separate from the write below because every key is validated before any is
/// written: changing the password drops every session, and a 400 raised
/// afterwards -- on a later key, in whatever order the map iterates -- carries no
/// Set-Cookie, signing the admin out of every device through a password change
/// the UI reported as rejected.
fn setting_error(app: &App, key: &str, value: &Value) -> Option<String> {
    // Settings are stored as text. A caller sending the natural JSON type --
    // `{"public_page": false}`, `{"retention_days": 7}` -- was formerly skipped by
    // a bare `continue`, so nothing was written while the response reported
    // success.
    let Some(value) = value.as_str() else { return Some(format!("{key} must be a string")) };
    match key {
        "theme" if !crate::frontend::selectable(app, value) => Some("theme is not installed".into()),
        // Housekeeping clamps whatever it reads, so an unparsable value would be
        // stored, echoed back, and silently mean 7 days indefinitely.
        "retention_days" if !value.parse::<i64>().is_ok_and(|d| (1..=3_650).contains(&d)) => {
            Some("retention days must be a number from 1 to 3650".into())
        }
        // The hub fetches this URL itself, so it must be one: a scheme it cannot
        // speak turns every agent download into a 502 that says nothing about the
        // setting responsible.
        //
        // https only. What returns from this host is the agent binary, which
        // `install.sh` writes to /opt/monitor and starts on every node provisioned
        // here; over http:// anyone on the path between the hub and the mirror
        // chooses that binary, while the node still sees a valid TLS connection to
        // the hub.
        "github_proxy" if !(value.is_empty() || value.starts_with("https://")) => {
            Some("GitHub proxy must start with https://: the agent binary is fetched through it and installed on every node".into())
        }
        "admin_password" if value.len() < 12 => Some("password must be at least 12 characters".into()),
        "admin_password" => None,
        k if k.starts_with("notify_") => crate::notify::setting_error(k, value),
        k if READABLE_SETTINGS.contains(&k) || k == "github_client_secret" => None,
        _ => Some(format!("unknown setting: {key}")),
    }
}

pub async fn save_settings(
    _: Admin,
    State(app): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let Some(map) = body.as_object() else { return bad("expected an object") };
    for (key, value) in map {
        if let Some(message) = setting_error(&app, key, value) {
            return bad(&message);
        }
    }
    // Set when the password changed, so the caller receives a fresh session rather
    // than being logged out by their own change.
    let mut reissued = String::new();
    for (key, value) in map {
        let value = value.as_str().unwrap_or_default();
        // Changing the password logs out every existing session; the caller
        // receives a replacement.
        if key == "admin_password" {
            let who = current_session(&headers).and_then(|h| app.db.session_login(&h)).unwrap_or_default();
            match hash_password(value).and_then(|h| {
                app.db.replace_password(&h)?;
                issue_session(&app, &headers, &who, "")
            }) {
                Ok(cookie) => reissued = cookie,
                Err(e) => return fail(e),
            }
            continue;
        }
        if let Err(e) = app.db.set(key, value) {
            return fail(e);
        }
    }
    with_cookies(Json(json!({"ok": true})), [reissued])
}

#[cfg(test)]
mod tests {
    use super::*;
    // Sessions remain hashed; only node tokens are stored in the clear.
    use crate::auth::sha256;
    use crate::db::Db;

    fn domain_headers() -> HeaderMap {
        HeaderMap::from_iter([
            (header::HOST, "monitor.example.com".parse().unwrap()),
            (header::HeaderName::from_static("x-forwarded-proto"), "https".parse().unwrap()),
        ])
    }

    fn app() -> App {
        App::for_test(Db::open(":memory:").unwrap())
    }

    #[tokio::test]
    async fn provisioning_requires_the_current_https_domain_entry() {
        let no_site = app();
        let mut state = app();
        state.site = "https://monitor.example.com".into();
        let app = std::sync::Arc::new(state);
        let good = domain_headers();
        assert!(provisioning_allowed(&app, &good));
        let mut plain = good.clone();
        plain.insert("x-forwarded-proto", "http".parse().unwrap());
        assert!(
            !provisioning_allowed(&app, &plain),
            "--site cannot override an explicitly plaintext request"
        );
        for host in ["127.0.0.1:9911", "[::1]:9911", "198.51.100.1", "2130706433", "localhost"] {
            let mut headers = good.clone();
            headers.insert(header::HOST, host.parse().unwrap());
            let node = serde_json::from_value(json!({"name":"blocked"})).unwrap();
            assert_eq!(
                create_node(Admin, State(app.clone()), headers.clone(), Ok(Json(node))).await.status(),
                StatusCode::FORBIDDEN
            );
            assert_eq!(
                open_register(Admin, State(app.clone()), headers.clone()).await.status(),
                StatusCode::FORBIDDEN
            );
            assert_eq!(
                agent_register(
                    State(app.clone()),
                    ConnectInfo("127.0.0.1:1".parse().unwrap()),
                    headers,
                    "blocked".into()
                )
                .await
                .status(),
                StatusCode::FORBIDDEN
            );
        }
        let mut headers = good.clone();
        headers.insert(header::ORIGIN, "http://127.0.0.1:9911".parse().unwrap());
        assert!(!provisioning_allowed(&app, &headers));
        assert!(app.db.nodes().unwrap().is_empty());
        assert!(app.db.get("register_key").is_none());
        headers = good;
        headers.remove("x-forwarded-proto");
        assert!(!provisioning_allowed(&no_site, &headers));
        for site in [
            "http://monitor.example.com",
            "https://198.51.100.1",
            "https://user@monitor.example.com",
            "https://monitor.example.com/path",
        ] {
            assert!(https_domain(site).is_none());
        }
    }

    /// Whatever this accepts is pushed to every assigned agent and passed directly
    /// to `lookup_host`. Forms it cannot resolve return -1 indefinitely, which the
    /// chart draws as a probe losing every packet, so the check must match what
    /// the error message claims.
    #[test]
    fn a_probe_target_must_be_something_the_agent_can_resolve() {
        // Each of these causes `lookup_host` to return an error, verified against
        // it: a bare IPv6 address is all colons, and the other two omit the half
        // the message requires.
        for bad in
            ["2606:4700:4700::1111", ":443", "example.com:", "1.1.1.1", "1.1.1.1:0", "[::1]:x", "[::1]"]
        {
            assert!(!valid_target(bad), "{bad}");
        }
        for good in ["1.1.1.1:443", "[2606:4700:4700::1111]:443", "example.com:80", "[::1]:1"] {
            assert!(valid_target(good), "{good}");
        }
    }

    /// The check above is meaningful only if it runs on the stored string: what
    /// reaches the agent is the stored value, and `lookup_host` rejects
    /// `"1.1.1.1:443 "` outright -- the permanent -1 `valid_target` exists to
    /// prevent, reachable through a check that passed.
    #[tokio::test]
    async fn a_probe_target_is_stored_as_the_string_that_was_checked() {
        let app = std::sync::Arc::new(app());
        let save = |name: &str, target: &str| {
            let task = PingTask {
                id: 0,
                name: name.to_owned(),
                target: target.to_owned(),
                interval: 60,
                nodes: vec![],
            };
            save_ping_task(Admin, State(app.clone()), Json(task))
        };
        assert_eq!(save(" 探测 ", "1.1.1.1:443 ").await.status(), StatusCode::OK);
        let stored = &app.db.ping_tasks().unwrap()[0];
        assert_eq!(stored.target, "1.1.1.1:443", "the agent gets this string, not the one that was checked");
        assert_eq!(stored.name, "探测", "and it labels an anonymous chart");
        // Trimming must not turn a blank entry into a saved row.
        assert_eq!(save("   ", "   ").await.status(), StatusCode::BAD_REQUEST);
        assert_eq!(app.db.ping_tasks().unwrap().len(), 1);
    }

    /// The same rule as `retention_days`, applied to the other value this hub
    /// clamps downstream: an out-of-range value must fail, or it silently becomes
    /// a different one. Below the floor that value is 5 seconds, the fastest probe
    /// available, and the panel reaches 0 simply by clearing its interval field,
    /// since `Number("")` is 0.
    #[tokio::test]
    async fn a_probe_interval_out_of_range_is_refused_rather_than_clamped() {
        let app = std::sync::Arc::new(app());
        let save = |interval| {
            let task = PingTask {
                id: 0,
                name: "probe".into(),
                target: "1.1.1.1:443".into(),
                interval,
                nodes: vec![],
            };
            save_ping_task(Admin, State(app.clone()), Json(task))
        };
        for refused in [0, -1, 4, 3_601, i64::MAX] {
            assert_eq!(save(refused).await.status(), StatusCode::BAD_REQUEST, "{refused}");
        }
        assert!(app.db.ping_tasks().unwrap().is_empty(), "a refused interval must not store a probe");

        // Both ends of the range still save, storing exactly what was sent.
        for ok in [5, 60, 3_600] {
            assert_eq!(save(ok).await.status(), StatusCode::OK, "{ok}");
        }
        let stored: Vec<i64> = app.db.ping_tasks().unwrap().iter().map(|t| t.interval).collect();
        assert_eq!(stored, vec![5, 60, 3_600]);
    }

    /// The entire chunked-upload protocol: an upload is only ever as long as what
    /// has landed, so a piece continues it, restarts it, or is refused.
    #[tokio::test]
    async fn a_chunk_continues_an_upload_only_where_the_last_one_ended() {
        let path = std::env::temp_dir().join(format!("monitor-chunk-{}", std::process::id()));
        let path = path.to_str().unwrap();
        let piece = |offset, total| Chunk { offset, total };
        let body = |bytes: &'static [u8]| axum::body::Body::from(bytes);

        // Two pieces in order, with the length indicating where the next begins.
        assert_eq!(receive(path, &piece(0, 6), 1024, body(b"abc")).await.unwrap(), 3);
        assert_eq!(receive(path, &piece(3, 6), 1024, body(b"def")).await.unwrap(), 6);
        assert_eq!(std::fs::read(path).unwrap(), b"abcdef");

        // A gap, a rewind and an overshoot all produce the same refusal.
        assert!(receive(path, &piece(9, 12), 1024, body(b"xyz")).await.is_err());
        assert!(receive(path, &piece(3, 12), 1024, body(b"xyz")).await.is_err());
        assert!(receive(path, &piece(6, 7), 1024, body(b"toolong")).await.is_err());
        // None of them modified the file, so the upload can continue.
        assert_eq!(std::fs::metadata(path).unwrap().len(), 6);

        // The ceiling is checked against the declared total, before any bytes
        // arrive.
        assert!(receive(path, &piece(0, 4096), 1024, body(b"a")).await.is_err());
        assert!(receive(path, &piece(0, 0), 1024, body(b"")).await.is_err());

        // Starting over truncates whatever an interrupted attempt left behind.
        assert_eq!(receive(path, &piece(0, 2), 1024, body(b"hi")).await.unwrap(), 2);
        assert_eq!(std::fs::read(path).unwrap(), b"hi");
        std::fs::remove_file(path).unwrap();
    }

    /// Both scratch names a restore uses sit beside the live database, and one left
    /// behind is what the next upload fails on: `receive` refuses a first chunk
    /// that does not align with an existing file.
    ///
    /// This does not cover the rename in `db_restore`, which changes which path is
    /// open during the copy and would require a second upload landing inside it.
    /// It covers the part that outlives the request, which is what a later edit
    /// could silently drop.
    #[tokio::test]
    async fn a_finished_restore_leaves_no_scratch_file_behind() {
        let dir = std::env::temp_dir().join(format!("monitor-restore-{}", &random_token()[..16]));
        std::fs::create_dir_all(&dir).unwrap();
        let live = dir.join("live.db").to_string_lossy().into_owned();
        let app = std::sync::Arc::new(App::for_test(Db::open(&live).unwrap()));
        node(&app, "kept", true);

        // What a restore actually receives: a backup of a hub database.
        let copy = format!("{live}.copy");
        app.db.backup_into(&copy).unwrap();
        let bytes = std::fs::read(&copy).unwrap();
        std::fs::remove_file(&copy).unwrap();

        let done = db_restore(
            Admin,
            State(app.clone()),
            Query(Chunk { offset: 0, total: bytes.len() as u64 }),
            HeaderMap::new(),
            axum::body::Body::from(bytes),
        )
        .await;
        assert_eq!(done.status(), StatusCode::OK);
        assert_eq!(app.db.nodes().unwrap().len(), 1, "the backup went in");

        // The database and its journal are the only files that may remain.
        let left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
            .filter(|name| !matches!(name.as_str(), "live.db" | "live.db-wal" | "live.db-shm"))
            .collect();
        assert!(left.is_empty(), "left beside the database: {left:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A connected agent holding one report. The receiver is returned because
    /// dropping it closes the channel, which is the signal `reset_token` is tested
    /// for.
    fn connect(app: &App, id: i64, metrics: Value) -> tokio::sync::mpsc::Receiver<String> {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let mut agent = crate::agent_ws::Agent::new(7, tx);
        agent.metrics = metrics;
        agent.last_seen = Utc::now().timestamp();
        app.agents.write().unwrap().insert(id, agent);
        rx
    }

    /// A probe assigned to `nodes`. The window query draws only a node's current
    /// assignments, so a fixture holding ping records requires one.
    fn task(app: &App, nodes: Vec<i64>) -> i64 {
        app.db
            .save_ping_task(&PingTask {
                id: 0,
                name: "probe".into(),
                target: "1.1.1.1:443".into(),
                interval: 60,
                nodes,
            })
            .unwrap()
    }

    fn node(app: &App, name: &str, public: bool) -> i64 {
        app.db
            .create_node(
                &Node { name: name.into(), public, remark: "secret note".into(), ..Default::default() },
                &format!("token-of-{name}"),
            )
            .unwrap()
    }

    /// A chart request costs roughly the same whatever it spans. This path
    /// requires no session, so an unbounded window would be megabytes of JSON any
    /// caller could have the hub build on the connection the agents report
    /// through.
    #[test]
    fn a_history_window_costs_the_same_however_wide_it_is() {
        let app = app();
        let id = node(&app, "n", true);
        let now = Utc::now().timestamp();
        // A month of history at the rate the hub writes it. Two probes, because
        // the budget is per series and a single-probe fixture would conceal that.
        const PROBES: i64 = 2;
        for _ in 0..PROBES {
            task(&app, vec![id]);
        }
        for i in 0..30 * 1440 {
            app.db.insert_metric(id, now - i * 60, &json!({"cpu": 1.0})).unwrap();
            for task in 1..=PROBES {
                app.db.insert_ping(id, task, now - i * 20, 42).unwrap();
            }
        }

        // Including windows that do not divide evenly, which are where a step
        // rounded the wrong way overruns.
        for hours in [1, 6, 13, 23, 24, 168, 2_160] {
            let step = sample_step(hours, None);
            let since = now - hours * 3_600;
            let metrics = app.db.metrics(id, since, step).unwrap();
            let (ping, _) = app.db.ping_records(id, since, step).unwrap();
            // Against the budget itself rather than whatever the step produced:
            // derived from the step, this would only demonstrate that division
            // works. One bucket of slack, as the window rarely divides evenly.
            let cap = 1_441;
            assert!(metrics.len() <= cap, "{hours}h returned {} metric rows", metrics.len());
            assert!(
                ping.len() <= cap * PROBES as usize,
                "{hours}h returned {} ping rows for {PROBES} probes",
                ping.len()
            );
            // Thinned, but neither empty nor reaching outside the window.
            assert!(!metrics.is_empty() && !ping.is_empty(), "{hours}h returned nothing");
            // A bucket the window opens partway through begins before it.
            assert!(
                metrics.iter().all(|m| m["ts"].as_i64().unwrap() >= since - step),
                "{hours}h reached back too far"
            );
        }
        // The widest window costs no more than a narrow one: unthinned, a month of
        // history is 43,200 rows.
        assert!(app.db.metrics(id, now - 2_160 * 3_600, sample_step(2_160, None)).unwrap().len() <= 1_441);

        // A day returns every minute it holds: thinning exists only for what the
        // screen cannot draw.
        assert_eq!(sample_step(24, Some(2_000)), 60, "a day of minutes fits under the ceiling");
        assert_eq!(sample_step(6, Some(2_000)), 60, "and so does six hours");

        // A caller may request less than the budget, never more: the ceiling
        // belongs to the hub, since this path takes no credentials.
        assert!(sample_step(24, Some(390)) > sample_step(24, None));
        assert_eq!(sample_step(24, Some(100_000)), sample_step(24, None));
        assert_eq!(sample_step(24, Some(0)), sample_step(24, Some(60)));

        // Requesting one half leaves the other empty rather than sending it: on
        // the day window that half was two thirds of the response.
        let series = |q: &str| serde_urlencoded::from_str::<Window>(q).unwrap().series;
        assert_eq!(series("hours=24&series=ping").as_deref(), Some("ping"));
        assert!(series("hours=24").is_none(), "no series means both, which is what curl gets");
    }

    /// What a thinned bucket may return. Keeping one row and discarding the rest
    /// made the seven-day chart integrate to twice the traffic the minutes hold,
    /// and drew a probe losing half its packets as an unbroken line.
    #[test]
    fn a_thinned_bucket_answers_with_its_mean_and_says_what_it_lost() {
        let app = app();
        let id = node(&app, "n", true);
        // Anchored on a bucket boundary, one whole bucket in the past. Anchored on
        // `now`, the rows would straddle the boundary depending on the second the
        // suite runs at.
        let base = Utc::now().timestamp() / 120 * 120 - 120;
        // One bucket: a quiet minute and a busy one, then a probe that answered
        // once and timed out three times.
        app.db.insert_metric(id, base + 10, &json!({"cpu": 0.0, "net_rx": 0})).unwrap();
        app.db.insert_metric(id, base + 70, &json!({"cpu": 40.0, "net_rx": 1_000})).unwrap();
        for _ in 0..3 {
            task(&app, vec![id]);
        }
        for (i, latency) in [30, -1, -1, -1].into_iter().enumerate() {
            app.db.insert_ping(id, 1, base + 10 + i as i64 * 20, latency).unwrap();
        }
        // A second probe that never answered, and a third that answered cleanly.
        app.db.insert_ping(id, 2, base + 10, -1).unwrap();
        app.db.insert_ping(id, 3, base + 10, 12).unwrap();

        let m = &app.db.metrics(id, base, 120).unwrap()[0];
        assert_eq!(m["cpu"], 20.0, "the bucket is its mean, not one row of it");
        assert_eq!(m["net_rx"], 500);
        assert_eq!(m["ts"], base, "stamped with the bucket, so every series shares a grid");

        // Keyed by task rather than index: the rows share a timestamp, so
        // `ORDER BY ts` leaves their order to SQLite.
        let (rows, window_loss) = app.db.ping_records(id, base, 120).unwrap();
        let probe = |task: i64| {
            rows.iter().find(|r| r["task_id"] == task).unwrap_or_else(|| panic!("no probe {task}"))
        };
        assert_eq!(probe(1)["latency"], 30, "the median of what answered, not of the timeouts");
        assert_eq!(probe(1)["loss"], 75);
        assert_eq!(probe(2)["latency"], json!(null), "a bucket that was all timeout has no latency");
        assert_eq!(probe(2)["loss"], 100);
        // One answer, so there is nothing for a band to span.
        assert!(probe(1).get("band").is_none(), "{:?}", probe(1));
        // A clean bucket carries no loss key, which is why the percentage rounds
        // up: the key's absence denotes no loss, so no loss must be the only way
        // to produce it.
        assert!(probe(3).get("loss").is_none(), "{:?}", probe(3));
        // Each probe has one bucket here, so the window and bucket figures agree
        // -- precisely the fixture shape that concealed the difference between
        // them. The test below separates the two.
        assert_eq!(window_loss["2"], 100.0);
        assert!(window_loss.get("3").is_none(), "a probe that lost nothing is left out");

        // One timeout in a bucket too large for it to reach a whole percent:
        // truncating would report the same as a clean bucket.
        let wide = node(&app, "wide", true);
        let wide_probe = task(&app, vec![wide]);
        let wide_base = base / 180 * 180;
        for i in 0..180 {
            app.db.insert_ping(wide, wide_probe, wide_base + i, if i == 0 { -1 } else { 20 }).unwrap();
        }
        let (rows, _) = app.db.ping_records(wide, wide_base, 180).unwrap();
        assert_eq!(rows.len(), 1, "the fixture has to be one bucket for this to mean anything");
        let row = &rows[0];
        assert_eq!(row["loss"], 1, "a bucket that lost one of 180 has not lost none");

        // What the band conveys: the median reading and the two extremes the
        // bucket reached. Drawing 20 alone would render a 40 ms swing as a flat
        // point.
        let jitter = node(&app, "jitter", true);
        let jitter_probe = task(&app, vec![jitter]);
        for (i, latency) in [10, 20, 50, 20, 20].into_iter().enumerate() {
            app.db.insert_ping(jitter, jitter_probe, wide_base + i as i64, latency).unwrap();
        }
        let row = &app.db.ping_records(jitter, wide_base, 180).unwrap().0[0];
        assert_eq!(row["latency"], 20, "the middle answer, not the mean of 24");
        assert_eq!(row["band"], json!([10, 50]));

        // An even count has no single middle value, so it is the mean of the two
        // straddling it. Every neighbouring pair differs, so selecting one rank
        // either way would yield 20 or 30 rather than 25.
        let even = node(&app, "even", true);
        let even_probe = task(&app, vec![even]);
        for (i, latency) in [40, 10, 30, 20].into_iter().enumerate() {
            app.db.insert_ping(even, even_probe, wide_base + i as i64, latency).unwrap();
        }
        assert_eq!(app.db.ping_records(even, wide_base, 180).unwrap().0[0]["latency"], 25);
    }

    /// What a window lost is the proportion of its samples lost, and only the hub
    /// can determine it: `close_bucket` divides within each bucket and keeps the
    /// quotient, so the denominators are gone by the time a reader sees the rows.
    /// Averaging the bucket percentages would weight a bucket holding one sample
    /// equally with one holding twelve, and unequal buckets are the ordinary case
    /// rather than an edge one. The window's first and last are partial by
    /// construction, and a probe that starts, stops, loses its node or skips a
    /// round on a slow resolver produces more.
    #[test]
    fn a_probe_reports_the_share_of_the_window_it_lost_not_the_mean_of_its_buckets() {
        let app = app();
        let id = node(&app, "n", true);
        let probe = task(&app, vec![id]);
        let base = Utc::now().timestamp() / 60 * 60 - 120;
        // A full minute at five seconds per round with no loss, then a minute
        // holding one sample, which was lost, before the probe stopped.
        for i in 0..12 {
            app.db.insert_ping(id, probe, base + i * 5, 20).unwrap();
        }
        app.db.insert_ping(id, probe, base + 60, -1).unwrap();

        let (rows, loss) = app.db.ping_records(id, base, 60).unwrap();
        let per_bucket: Vec<i64> = rows.iter().map(|r| r["loss"].as_i64().unwrap_or(0)).collect();
        assert_eq!(per_bucket, vec![0, 100], "the buckets are right about themselves");

        // Their mean is 50%, while one round of thirteen did not answer.
        let window = loss.get(probe.to_string()).and_then(|v| v.as_f64()).expect("this probe lost one");
        assert!((window - 100.0 / 13.0).abs() < 1e-9, "{window}");
        assert!(window < 8.0, "the window lost {window}%, not the 50% its buckets average to");
    }

    #[test]
    fn the_public_view_hides_private_nodes_and_sensitive_fields() {
        let app = app();
        let open = node(&app, "open", true);
        node(&app, "hidden", false);
        app.db.save_facts(open, &json!({"hostname": "vps-1"}), "198.51.100.9").unwrap();

        // A live report, so the public view has metrics to strip. `hostname` is
        // what a node token in the wrong hands can insert, and what the agent
        // repository could add to the contract.
        let _held = connect(
            &app,
            open,
            json!({"boot_id": "abc", "net_rx_total": 134_000_000_000i64, "cpu": 1.0,
                   "hostname": "db-prod-01", "ip": "203.0.113.7"}),
        );

        let public = visible_nodes(&app, false).unwrap();
        assert_eq!(public.len(), 1, "a node marked private must not be listed");
        assert_eq!(public[0]["name"], "open");
        // Disclosing the token would let any visitor impersonate the node.
        for hidden in ["ip", "remark", "hostname", "token"] {
            assert!(public[0].get(hidden).is_none(), "{hidden} must not be public");
        }
        assert!(
            !serde_json::to_string(&public).unwrap().contains("token-of-open"),
            "no node's token may appear anywhere in a public payload"
        );
        // Raw kernel counters would disclose the machine's lifetime traffic, and
        // anything the contract does not name is not published at all, the report
        // coming from a machine holding one node's token.
        for hidden in ["boot_id", "net_rx_total", "net_tx_total", "hostname", "ip"] {
            assert!(public[0]["metrics"].get(hidden).is_none(), "{hidden} must not be public");
        }
        assert_eq!(public[0]["metrics"]["cpu"], 1.0, "the rest of the report still goes out");

        let admin = visible_nodes(&app, true).unwrap();
        assert_eq!(admin.len(), 2);
        assert_eq!(admin[0]["ip"], "198.51.100.9");
        assert_eq!(admin[0]["remark"], "secret note");
    }

    #[tokio::test]
    async fn rotating_a_token_closes_the_session_the_old_one_opened() {
        let app = std::sync::Arc::new(app());
        let id = node(&app, "n", true);
        let mut rx = connect(&app, id, Value::Null);

        let response = reset_token(Admin, axum::extract::State(app.clone()), Path(id)).await;
        assert_eq!(response.status(), StatusCode::OK);
        // The agent loop selects on this receiver, so a closed channel is how it
        // learns to stop. `try_recv`, because `recv().await` on a channel
        // incorrectly left open would hang the suite rather than fail it.
        assert!(
            matches!(rx.try_recv(), Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)),
            "the old agent's channel must be closed"
        );
        assert!(app.agents.read().unwrap().is_empty(), "the node must read as offline at once");
    }

    /// A write naming a node that no longer exists, such as one deleted from
    /// another tab, is refused rather than reported as saved.
    #[tokio::test]
    async fn writes_to_a_missing_node_are_not_found() {
        let app = std::sync::Arc::new(app());
        let state = || axum::extract::State(app.clone());
        let patch = Ok(Json(NodePatch { notify: Some(true), ..Default::default() }));
        assert_eq!(update_node(Admin, state(), Path(9), patch).await.status(), StatusCode::NOT_FOUND);
        assert_eq!(delete_node(Admin, state(), Path(9)).await.status(), StatusCode::NOT_FOUND);
        assert_eq!(reset_token(Admin, state(), Path(9)).await.status(), StatusCode::NOT_FOUND);
        let traffic = Json(TrafficPatch { total_rx: Some(1), ..Default::default() });
        assert_eq!(patch_traffic(Admin, state(), Path(9), traffic).await.status(), StatusCode::NOT_FOUND);
    }

    /// Deleting a node must reach the connection it opened, for the same reason
    /// rotating its token does, and more urgently: SQLite reassigns the freed id
    /// to the next node created. Left connected, the old machine reports under
    /// that id, so an undeployed node appears online with another machine's
    /// metrics, and its traffic and history are booked to it.
    #[tokio::test]
    async fn deleting_a_node_closes_its_session_so_the_next_id_does_not_inherit_it() {
        let app = std::sync::Arc::new(app());
        let old = node(&app, "old", true);
        let mut rx = connect(&app, old, json!({"cpu": 42.0}));

        assert_eq!(
            delete_node(Admin, axum::extract::State(app.clone()), Path(old)).await.status(),
            StatusCode::OK
        );
        assert!(
            matches!(rx.try_recv(), Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)),
            "the deleted node's agent must be told to go"
        );

        // SQLite reuses the id; nothing of the old machine may accompany it.
        let fresh = node(&app, "fresh", true);
        assert_eq!(fresh, old, "the fixture only means anything if the id is reused");
        let nodes = visible_nodes(&app, true).unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0]["online"], json!(false), "a node nobody deployed is not online");
        assert_eq!(nodes[0]["metrics"], Value::Null, "and it has nobody else's metrics");
    }

    /// Both writers enforce the same limits. The create path formerly accepted a
    /// whole `Node` unchecked, leaving everything the update path refuses
    /// reachable by another route.
    #[tokio::test]
    async fn both_write_paths_refuse_the_same_out_of_range_values() {
        let app = std::sync::Arc::new(app());
        let id = node(&app, "n", true);
        for bad in [
            json!({"name": "x", "traffic_reset_day": 99}),
            json!({"name": "x", "price": -5.0}),
            json!({"name": "x", "traffic_limit": -1}),
        ] {
            let created = create_node(
                Admin,
                axum::extract::State(app.clone()),
                domain_headers(),
                Ok(Json(serde_json::from_value(bad.clone()).unwrap())),
            )
            .await;
            assert_eq!(created.status(), StatusCode::BAD_REQUEST, "create accepted {bad}");
            let updated = update_node(
                Admin,
                axum::extract::State(app.clone()),
                Path(id),
                Ok(Json(serde_json::from_value(bad.clone()).unwrap())),
            )
            .await;
            assert_eq!(updated.status(), StatusCode::BAD_REQUEST, "update accepted {bad}");
        }
        assert_eq!(app.db.nodes().unwrap().len(), 1, "nothing was created");
    }

    /// A stream outlives the request that opened it, so everything the handshake
    /// tested must be re-read rather than captured -- both answers, not one. The
    /// admin frame carries every node's token in the clear, and the public frame
    /// is what switching the status page off is meant to withdraw; a socket
    /// surviving either decision would continue sending what was withdrawn.
    #[test]
    fn a_stream_re_reads_both_answers_its_handshake_tested() {
        let app = app();
        let hash = sha256("live-token");
        app.db.create_session(&hash, Utc::now().timestamp() + 3_600, "", "", "").unwrap();

        assert_eq!(stream_audience(&app, Some(&hash)), Some(true), "a live session gets the admin frame");
        assert_eq!(stream_audience(&app, None), Some(false), "an anonymous stream gets the public one");

        // Signing out, another device revoking this one, a password change and a
        // restore all manifest as this row disappearing.
        app.db.drop_session(&hash).unwrap();
        assert_eq!(stream_audience(&app, Some(&hash)), None, "a revoked session must end its stream");

        // The other half. `live_ws` refuses a new anonymous connection from here
        // and `nodes` answers 401, so a stream that continued was the only
        // remaining route, for as long as the tab stayed open.
        app.db.create_session(&hash, Utc::now().timestamp() + 3_600, "", "", "").unwrap();
        app.db.set("public_page", "off").unwrap();
        assert_eq!(stream_audience(&app, None), None, "closing the status page must end anonymous streams");
        assert_eq!(stream_audience(&app, Some(&hash)), Some(true), "a signed-in operator still gets theirs");
    }

    #[test]
    fn the_shared_snapshot_keeps_the_two_audiences_apart() {
        let app = app();
        let open = node(&app, "open", true);
        node(&app, "hidden", false);
        app.db.save_facts(open, &json!({"hostname": "vps-1"}), "198.51.100.9").unwrap();

        let public = live_snapshot(&app, false);
        let admin = live_snapshot(&app, true);
        // Caching must never let one audience's payload reach the other.
        assert!(!public.as_str().contains("198.51.100.9"), "the public frame must carry no address");
        assert!(!public.as_str().contains("hidden"), "the public frame must carry no private node");
        assert!(admin.as_str().contains("198.51.100.9") && admin.as_str().contains("hidden"));

        // Two reads over unchanged data prove nothing, since a rebuild returns the
        // same bytes, so the data is modified first.
        node(&app, "late", true);
        assert_eq!(live_snapshot(&app, false), public, "the frame is reused, not rebuilt per viewer");
    }

    #[test]
    fn a_clock_stepping_backwards_does_not_pin_a_stale_frame() {
        let app = app();
        node(&app, "first", true);
        live_snapshot(&app, false);

        // NTP correcting a fresh boot leaves the cached stamp in the future, which
        // does not constitute a young frame.
        app.snapshot.lock().unwrap()[0].0 = Utc::now().timestamp_millis() + 60_000;
        node(&app, "added-after", true);
        assert!(live_snapshot(&app, false).as_str().contains("added-after"));
    }

    /// The panel sends only a name, and expects the node just added to appear in
    /// the frame it is already streaming.
    #[tokio::test]
    async fn a_node_added_from_the_panel_needs_only_a_name_and_shows_up_at_once() {
        let app = std::sync::Arc::new(app());
        node(&app, "existing", true);
        assert!(!live_snapshot(&app, true).as_str().contains("added"));

        let added: Node = serde_json::from_value(json!({"name": "added"})).unwrap();
        // The defaults the panel relies on by omitting them, `public` above all:
        // the alternative would publish a node that was never published.
        assert!(added.public);
        assert_eq!(added.billing_cycle, "monthly");
        assert_eq!(added.traffic_reset_day, 1);

        let created = create_node(Admin, State(app.clone()), domain_headers(), Ok(Json(added))).await;
        assert_eq!(created.status(), StatusCode::OK);
        // Frames are cached for nearly two seconds, so without dropping the cache
        // the node just added would disappear from the list.
        assert!(live_snapshot(&app, true).as_str().contains("added"));

        // A name consisting only of spaces is refused and leaves no node behind.
        let blank = Json(serde_json::from_value::<Node>(json!({"name": "   "})).unwrap());
        let refused = create_node(Admin, State(app.clone()), domain_headers(), Ok(blank)).await;
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
        assert_eq!(app.db.nodes().unwrap().len(), 2);
    }

    /// Every gate on the anonymous route, in the order a batch install encounters
    /// them: closed, wrong key, open, expired, closed manually.
    #[tokio::test]
    async fn registration_only_works_inside_a_window_the_panel_opened() {
        let app = std::sync::Arc::new(app());
        let register = |key: Option<&str>, name: &str| {
            let mut headers = domain_headers();
            if let Some(key) = key {
                headers.insert("authorization", format!("Bearer {key}").parse().unwrap());
            }
            agent_register(
                State(app.clone()),
                ConnectInfo("198.51.100.7:40000".parse().unwrap()),
                headers,
                name.to_owned(),
            )
        };

        // Nothing was opened, so no key is correct.
        assert_eq!(register(Some("guess"), "a").await.status(), StatusCode::FORBIDDEN);
        assert!(app.db.nodes().unwrap().is_empty());

        assert_eq!(open_register(Admin, State(app.clone()), domain_headers()).await.status(), StatusCode::OK);
        let key = app.db.get("register_key").unwrap();
        assert_eq!(register(Some("guess"), "a").await.status(), StatusCode::FORBIDDEN);
        assert_eq!(register(None, "a").await.status(), StatusCode::FORBIDDEN);
        assert!(app.db.nodes().unwrap().is_empty());

        let issued = register(Some(&key), "  web-01\n").await;
        assert_eq!(issued.status(), StatusCode::OK);
        let token = axum::body::to_bytes(issued.into_body(), usize::MAX).await.unwrap().to_vec();
        let token = String::from_utf8(token).unwrap();
        // The purpose of the route: what returned is a token an agent can connect
        // with, not merely a 200.
        let id = app.db.node_by_token(&token).unwrap().expect("token opens a node");
        let node = app.db.nodes().unwrap().into_iter().find(|n| n.id == id).unwrap();
        assert_eq!(node.name, "web-01");
        // Registered nodes take the panel's defaults rather than `Node::default()`.
        assert!(node.public);
        assert_eq!(node.traffic_reset_day, 1);

        // An hour later the same key is worthless, which is what makes leaving the
        // window open harmless.
        app.db.set("register_until", &(Utc::now().timestamp() - 1).to_string()).unwrap();
        assert_eq!(register(Some(&key), "b").await.status(), StatusCode::FORBIDDEN);

        // Reopened, then closed manually: the key from the open window stops
        // working.
        open_register(Admin, State(app.clone()), domain_headers()).await;
        let key = app.db.get("register_key").unwrap();
        assert_eq!(close_register(Admin, State(app.clone())).await.status(), StatusCode::NO_CONTENT);
        assert_eq!(register(Some(&key), "c").await.status(), StatusCode::FORBIDDEN);
        assert_eq!(app.db.nodes().unwrap().len(), 1);
    }

    /// The ceiling on the anonymous route: a leaked key cannot fill the table.
    #[tokio::test]
    async fn one_window_stops_registering_at_the_limit() {
        let app = std::sync::Arc::new(app());
        open_register(Admin, State(app.clone()), domain_headers()).await;
        let key = app.db.get("register_key").unwrap();
        for i in 0..REGISTER_LIMIT {
            node(&app, &format!("n{i}"), true);
        }
        let mut headers = domain_headers();
        headers.insert("authorization", format!("Bearer {key}").parse().unwrap());
        let refused = agent_register(
            State(app.clone()),
            ConnectInfo("198.51.100.7:40000".parse().unwrap()),
            headers,
            "one-too-many".to_owned(),
        )
        .await;
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
        assert_eq!(app.db.nodes().unwrap().len() as i64, REGISTER_LIMIT);
    }

    #[test]
    fn a_node_view_carries_traffic_even_while_offline() {
        let app = app();
        let id = node(&app, "n", true);
        app.db.accumulate(id, "b", Some((100, 100))).unwrap();
        app.db.accumulate(id, "b", Some((900, 500))).unwrap();
        app.db.touch_seen(id, 1_700_000_000).unwrap();

        let view = &visible_nodes(&app, true).unwrap()[0];
        assert_eq!(view["online"], false);
        assert_eq!(view["metrics"], Value::Null);
        assert_eq!(view["total_rx"], 800, "traffic is stored, not derived from the live state");
        assert_eq!(view["total_tx"], 400);
        // The live entry went with the connection, so "offline since" must come
        // from the node row.
        assert_eq!(view["last_seen"], 1_700_000_000);
    }

    /// Days to expiry are counted on the hub's calendar and are public, like the
    /// date itself; no date counts nothing.
    #[test]
    fn days_to_expiry_follow_the_hubs_calendar() {
        let app = app();
        let id = node(&app, "a", true);
        let expires_in = || visible_nodes(&app, false).unwrap()[0]["expires_in"].clone();
        assert_eq!(expires_in(), Value::Null);
        let today = Local::now().date_naive();
        for days in [3, 0, -1] {
            app.db.set_expiry(id, &(today + chrono::Duration::days(days)).to_string()).unwrap();
            assert_eq!(expires_in(), json!(days));
        }
    }

    /// A capacity arrives twice -- once in the facts stored at the handshake, and
    /// again in every report -- and the two diverge as soon as a disk is mounted
    /// on a running machine, which the agent detects by re-reading its mount table
    /// every sample. Drawn from the stored copy, the card and the detail page
    /// showed the same host two different sizes until it reconnected.
    #[test]
    fn a_capacity_that_changed_since_the_handshake_is_the_reported_one() {
        let app = app();
        let id = node(&app, "n", true);
        // What the handshake stored: 30 GB of disk, 1 GB of swap.
        app.db
            .save_facts(
                id,
                &json!({"mem_total": 1_000, "swap_total": 1i64 << 30, "disk_total": 30i64 << 30}),
                "ip",
            )
            .unwrap();

        let offline = &visible_nodes(&app, true).unwrap()[0];
        assert_eq!(
            offline["disk_total"],
            30i64 << 30,
            "with nobody connected the stored facts are all there is"
        );

        // A 5 GB volume is mounted and swap is disabled. The same session, with no
        // second hello, so the stored facts do not change.
        let _held = connect(
            &app,
            id,
            json!({"mem_total": 1_000, "swap_total": 0, "disk_total": 35i64 << 30, "cpu": 1.0}),
        );
        let live = &visible_nodes(&app, true).unwrap()[0];
        assert_eq!(live["disk_total"], 35i64 << 30, "the report is the truth while the agent is connected");
        assert_eq!(live["swap_total"], 0, "swapoff means zero, not the gigabyte that was there at connect");
        assert_eq!(live["disk_total"], live["metrics"]["disk_total"], "one number, not two");
        assert_eq!(app.db.node(id).unwrap().unwrap().disk_total, 30i64 << 30, "and no extra write to get it");
    }

    /// The history gate is process-wide while tests are not: the harness runs
    /// every test in its own thread at once, so the two tests that drive
    /// `metrics` have to take turns. Held for the whole test, not just around the
    /// assertions, since the gate is held across an await and the interference is
    /// exactly that overlap.
    ///
    /// A blocking mutex across an await is what `clippy::await_holding_lock`
    /// warns about, and the callers allow it deliberately: these run on
    /// `#[tokio::test]`'s current-thread runtime, where no other task can need
    /// this thread while it waits, and the alternative -- a `tokio` mutex shared
    /// between two independent runtimes -- trades a real guarantee for a lint.
    fn gate_tests() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The window starts when the node did, not when the window did. Without the
    /// clamp a node added yesterday divides its day of reports by thirty days and
    /// reads as three percent available.
    #[test]
    fn an_uptime_window_starts_when_the_node_did() {
        // Minute-aligned, so every expectation below is exact rather than
        // "within a minute".
        let now = 1_800_000_000;
        let (from7, to) = uptime_window(0, now - 7 * 86_400, now);
        assert_eq!(to, now, "the end is the start of the minute in progress");
        assert_eq!(from7, now - 7 * 86_400, "a node that predates the window gets all of it");
        assert_eq!(uptime_fraction(7 * 1_440, from7, to), 1.0);

        // Added three days ago: three days of denominator, not the week's.
        let born = now - 3 * 86_400;
        let (from, _) = uptime_window(born, now - 7 * 86_400, now);
        assert_eq!(from, born);
        assert_eq!(uptime_fraction(3 * 1_440, from, to), 1.0);
        assert_eq!(
            uptime_fraction(3 * 1_440, from7, to),
            (3 * 1_440) as f64 / (7 * 1_440) as f64,
            "which is what the un-clamped denominator would have reported"
        );

        // Born mid-minute: the whole minute it was born in is one it could have
        // reported in, so the window starts at the next.
        let (from, _) = uptime_window(born + 30, now - 7 * 86_400, now);
        assert_eq!(from, born + 60);
        assert_eq!(uptime_fraction(3 * 1_440 - 1, from, to), 1.0);

        // Added this minute: nothing was expected yet, so nothing was missed.
        let (from, _) = uptime_window(now, now - 7 * 86_400, now);
        assert_eq!(from, to, "an empty window is clamped to empty");
        assert_eq!(uptime_fraction(0, from, to), 1.0);

        // A restored database can hold a report stamped inside a window the
        // node's own life does not reach; a fraction above one is not meaningful.
        assert_eq!(uptime_fraction(20_000, from7, to), 1.0);
    }

    /// The bar and the outage list come from the same minute list, and the
    /// partial hours at either end must not be drawn as downtime.
    #[test]
    fn availability_marks_partial_hours_full_and_merges_gaps() {
        let from = 1_800_000_000;
        let to = from + 7_200;
        // Hour zero: every minute but two, ten minutes in. Hour one: the first
        // half only, so the node is still down when the window ends.
        let mut minutes: Vec<i64> =
            (0..60).filter(|i| !(10..12).contains(i)).map(|i| from + i * 60).collect();
        minutes.extend((0..30).map(|i| from + 3_600 + i * 60));

        let a = availability(&minutes, from, to);
        assert_eq!(a["from"], from);
        assert_eq!(a["to"], to);
        let buckets = a["buckets"].as_array().unwrap();
        assert_eq!(buckets.len(), 2, "two whole hours");
        assert_eq!((buckets[0]["n"].as_i64().unwrap(), buckets[0]["m"].as_i64().unwrap()), (58, 60));
        assert_eq!((buckets[1]["n"].as_i64().unwrap(), buckets[1]["m"].as_i64().unwrap()), (30, 60));

        let incidents = a["incidents"].as_array().unwrap();
        assert_eq!(incidents.len(), 2, "the dug run, and the run still going at the end");
        assert_eq!(incidents[0]["start"], from + 10 * 60);
        assert_eq!(incidents[0]["minutes"], 2);
        assert_eq!(incidents[1]["start"], from + 3_600 + 30 * 60, "from the first minute it missed");
        assert_eq!(incidents[1]["minutes"], 30);

        // A node added mid-hour that reports every minute since: the first bucket
        // is short, and `m` says so -- read as a full hour it would draw a
        // brand-new node as already half an hour down.
        let from = 1_800_000_000 + 25 * 60;
        let to = from + 3_600;
        let minutes: Vec<i64> = (0..60).map(|i| from + i * 60).collect();
        let a = availability(&minutes, from, to);
        let buckets = a["buckets"].as_array().unwrap();
        assert_eq!(buckets.len(), 2);
        for bucket in buckets {
            assert_eq!(bucket["n"], bucket["m"], "every expected minute of each bucket was reported");
            assert!(bucket["m"].as_i64().unwrap() < 60, "and the bucket really is partial");
        }
        assert!(a["incidents"].as_array().unwrap().is_empty());

        // A node that never reported has one outage covering the window, not an
        // empty list, which would read as a node with a clean record.
        let a = availability(&[], from, to);
        let incidents = a["incidents"].as_array().unwrap();
        assert_eq!(incidents.len(), 1);
        assert_eq!(incidents[0]["start"], from);
        assert_eq!(incidents[0]["minutes"], 60);
    }

    /// The card's figure is reported minutes over the minutes the node was
    /// expected to report, and it comes from one aggregate rather than a query
    /// per node.
    #[test]
    fn a_node_view_carries_the_share_of_the_window_it_reported() {
        let app = app();
        let id = node(&app, "n", true);
        // Minute-aligned, so the expectation below is exact.
        let now = Utc::now().timestamp().div_euclid(60) * 60;
        app.db.set_created_at(id, now - 40 * 86_400).unwrap();
        // A month of history is only measurable if the hub is keeping a month:
        // the windows are clamped to the retention setting, and the default of
        // seven days would make the thirty-day figure the week's.
        app.db.set("retention_days", "30").unwrap();
        // Every minute of the month but ten hours in the last week, so both
        // windows are measured from the same fixture.
        for i in 0..30 * 1_440 {
            if (0..600).contains(&i) {
                continue;
            }
            app.db.insert_metric(id, now - i * 60, &json!({"cpu": 1.0})).unwrap();
        }

        let view = &visible_nodes(&app, false).unwrap()[0];
        let d7 = view["uptime"]["d7"].as_f64().unwrap();
        // 600 minutes of 10080. The window's own ends can drift by a minute while
        // the test runs, which is 6e-6 of the answer.
        assert!(
            (d7 - (1.0 - 600.0 / (7.0 * 1_440.0))).abs() < 1e-4,
            "600 missing minutes of a week is 94.05%, got {d7}"
        );
        let d30 = view["uptime"]["d30"].as_f64().unwrap();
        assert!(
            (d30 - (1.0 - 600.0 / (30.0 * 1_440.0))).abs() < 1e-4,
            "over thirty days the same 600 minutes is 98.61%, got {d30}"
        );
        assert_eq!(visible_nodes(&app, true).unwrap()[0]["uptime"], view["uptime"], "same answer either way");
    }

    /// A node with no history is measured against its own life: one added this
    /// minute has missed nothing, and one silent for a day is down for the day.
    #[test]
    fn a_node_that_never_reported_is_measured_against_its_own_life() {
        let app = app();
        let id = node(&app, "n", true);
        let now = Utc::now().timestamp();
        let view = visible_nodes(&app, false).unwrap();
        assert_eq!(view[0]["uptime"]["d7"], 1.0, "created this minute, so nothing was expected yet");

        app.db.set_created_at(id, now - 86_400).unwrap();
        // The map is cached for a minute, and this node's birthday just moved.
        invalidate_snapshot(&app);
        let view = visible_nodes(&app, false).unwrap();
        assert_eq!(view[0]["uptime"]["d7"], 0.0, "a day of silence against a day of life");
        assert_eq!(view[0]["uptime"]["d30"], 0.0);
    }

    /// A node that has reported since it was added reads as fully available, and
    /// not as a fraction of a window it was not alive for.
    ///
    /// This is the failure the aggregate alone cannot see: one query counts every
    /// node's rows, and only each node's own birthday makes its denominator
    /// right. Taken as the window, a node added three days ago reads as ten
    /// percent available over a month -- which is what the live preview showed
    /// before this test existed.
    #[test]
    fn a_node_added_days_ago_is_not_measured_against_the_whole_month() {
        let app = app();
        let id = node(&app, "new", true);
        let now = Utc::now().timestamp().div_euclid(60) * 60;
        app.db.set_created_at(id, now - 3 * 86_400).unwrap();
        // Every minute since it was added, and nothing before it.
        for i in 1..=3 * 1_440 {
            app.db.insert_metric(id, now - i * 60, &json!({"cpu": 1.0})).unwrap();
        }

        let view = &visible_nodes(&app, false).unwrap()[0];
        let d30 = view["uptime"]["d30"].as_f64().unwrap();
        let d7 = view["uptime"]["d7"].as_f64().unwrap();
        assert!(d30 > 0.999, "three days of life, every minute of it reported, got {d30}");
        assert!(d7 > 0.999, "and the same over the week, got {d7}");
    }

    /// A window longer than the retained history is shortened to fit it, and the
    /// window that resulted is reported so no reader is told a span the hub
    /// cannot back with rows.
    ///
    /// Pruning to the default seven days is what makes this matter: unclamped,
    /// the thirty-day figure divides seven days of surviving rows by thirty days
    /// of minutes, and every node on a stock hub reads as twenty-three percent
    /// available. The clamp is what makes that unrepresentable. The test is here
    /// because the acceptance fixture sets retention to thirty -- as it must, or
    /// pruning would eat most of it -- and so would never catch this.
    #[test]
    fn a_window_longer_than_the_retained_history_shrinks_to_fit() {
        let app = app();
        let id = node(&app, "n", true);
        let now = Utc::now().timestamp().div_euclid(60) * 60;
        app.db.set_created_at(id, now - 40 * 86_400).unwrap();
        // A month of rows, which the hub would not have been keeping.
        for i in 0..30 * 1_440 {
            app.db.insert_metric(id, now - i * 60, &json!({"cpu": 1.0})).unwrap();
        }

        // The default is a week, so thirty days is not answerable -- and is not
        // claimed to have been answered.
        let view = visible_nodes(&app, false).unwrap();
        let u = &view[0]["uptime"];
        assert_eq!(u["from30"], u["from7"], "the month is not longer than the history kept");
        assert_eq!(u["d30"], u["d7"], "so both figures are the same measurement");
        let span = u["to"].as_i64().unwrap() - u["from7"].as_i64().unwrap();
        assert!((7 * 86_400 - 60..=7 * 86_400).contains(&span), "and it is a week, got {span}s");

        // Raised to thirty, the month is there to be measured.
        app.db.set("retention_days", "30").unwrap();
        invalidate_snapshot(&app);
        let view = visible_nodes(&app, false).unwrap();
        let u = &view[0]["uptime"];
        let to = u["to"].as_i64().unwrap();
        let span7 = to - u["from7"].as_i64().unwrap();
        let span30 = to - u["from30"].as_i64().unwrap();
        assert!((7 * 86_400 - 60..=7 * 86_400).contains(&span7), "a week, got {span7}s");
        assert!((30 * 86_400 - 60..=30 * 86_400).contains(&span30), "a month, got {span30}s");
        assert!(u["d30"].as_f64().unwrap() > 0.999, "and every minute of it was reported in");
    }

    /// The bar arrives with the chart on one request, can be asked for alone, and
    /// is left out when it was not named -- two windows that differ only in the
    /// minute they end on must still compare equal byte for byte.
    #[allow(clippy::await_holding_lock)] // see `gate_tests`
    #[tokio::test]
    async fn a_history_window_carries_the_availability_bar_only_when_asked() {
        let _serial = gate_tests();
        let app = std::sync::Arc::new(app());
        let id = node(&app, "n", true);
        let now = Utc::now().timestamp().div_euclid(60) * 60;
        app.db.set_created_at(id, now - 40 * 86_400).unwrap();
        // A few minutes either side of the window, so its ends cannot fall
        // outside the rows while the test runs.
        for i in -5..=7 * 1_440 + 5 {
            app.db.insert_metric(id, now - i * 60, &json!({"cpu": 1.0})).unwrap();
        }
        let body = async |series: &str| {
            let query = format!("hours=168&series={series}");
            let asked = metrics(
                State(app.clone()),
                HeaderMap::new(),
                Path(id),
                Query(serde_urlencoded::from_str::<Window>(&query).unwrap()),
            );
            let bytes = axum::body::to_bytes(asked.await.into_body(), usize::MAX).await.unwrap();
            serde_json::from_slice::<Value>(&bytes).unwrap()
        };

        let with = body("metrics,availability").await;
        let buckets = with["availability"]["buckets"].as_array().unwrap();
        assert!(buckets.len() >= 167, "a week of hours, got {}", buckets.len());
        assert!(with["availability"]["incidents"].as_array().unwrap().is_empty());
        assert!(with["availability"]["from"].as_i64().unwrap() <= now - 7 * 86_400 + 60);
        assert!(!with["metrics"].as_array().unwrap().is_empty(), "and the chart came too");

        assert!(body("metrics").await["availability"].is_null(), "not named, not sent");

        let only = body("availability").await;
        assert!(only["metrics"].as_array().unwrap().is_empty());
        assert!(!only["availability"]["buckets"].as_array().unwrap().is_empty());
    }

    /// `PUBLIC_HOURS` bounds one window; this bounds how many are built
    /// concurrently. Each holds the connection the agents report through for its
    /// entire scan, and the path takes no credentials -- the same arrangement
    /// `RELAY_GATE` and `PASSWORD_GATE` enforce on the other two anonymous paths
    /// that make this process work hard.
    ///
    /// Serialised against the other test that drives the handler, because the
    /// gate is one process-wide semaphore while the test harness runs every test
    /// at once: this one takes all four permits, so any other test inside
    /// `metrics` at that instant either steals one from it or is refused by it.
    /// Measured at one run in ten failing before the lock, which is a failing CI
    /// run for whoever pushed next.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // see `gate_tests`
    async fn history_queries_past_the_gate_are_refused_rather_than_queued() {
        let _serial = gate_tests();
        let app = std::sync::Arc::new(app());
        let id = node(&app, "n", true);
        let ask = || {
            metrics(
                State(app.clone()),
                HeaderMap::new(),
                Path(id),
                Query(Window { hours: 1, points: None, series: None }),
            )
        };

        let held: Vec<_> =
            (0..HISTORY_SLOTS).map(|_| HISTORY_GATE.try_acquire().expect("up to the limit")).collect();
        assert_eq!(ask().await.status(), StatusCode::SERVICE_UNAVAILABLE);
        drop(held);
        assert_eq!(ask().await.status(), StatusCode::OK, "a finished query gives its slot back");

        // An unauthorised caller is told so rather than asked to retry later: the
        // gate sits behind the visibility check deliberately.
        app.db.set("public_page", "off").unwrap();
        let held: Vec<_> =
            (0..HISTORY_SLOTS).map(|_| HISTORY_GATE.try_acquire().expect("up to the limit")).collect();
        assert_eq!(ask().await.status(), StatusCode::UNAUTHORIZED);
        drop(held);
    }

    /// A settings write lands whole or not at all. Changing the password drops
    /// every session and places the replacement cookie on the response, so a 400
    /// raised afterwards -- on a later key, in whatever order the map iterates --
    /// would sign the admin out of every device without explanation.
    #[tokio::test]
    async fn a_settings_write_is_all_or_nothing() {
        let app = std::sync::Arc::new(app());
        app.db.set("admin_password_hash", "the-old-hash").unwrap();
        let save = |body: Value| save_settings(Admin, State(app.clone()), HeaderMap::new(), Json(body));

        // BTreeMap order places the password first, which is the failing case.
        let refused = save(json!({"admin_password": "a-long-enough-one", "retention_days": "abc"})).await;
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
        assert_eq!(app.db.get("admin_password_hash").as_deref(), Some("the-old-hash"));

        // A correctly named key carrying the wrong type is refused rather than
        // discarded while the response reports success.
        let refused = save(json!({"public_page": false})).await;
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
        assert_eq!(app.db.get("public_page"), None);

        let saved = save(json!({"public_page": "off", "retention_days": "7"})).await;
        assert_eq!(saved.status(), StatusCode::OK);
        assert_eq!(app.db.get("retention_days").as_deref(), Some("7"));
    }

    /// The install script and the sign-in page keep separate counters: five
    /// machines started with a stale key is a misconfigured deploy, and a shared
    /// counter would lock the operator out of the panel for the lockout window.
    #[tokio::test]
    async fn a_wrong_registration_key_does_not_lock_the_sign_in_page() {
        let app = std::sync::Arc::new(app());
        app.db.set("register_key", "the-key").unwrap();
        app.db.set("register_until", &(Utc::now().timestamp() + 60).to_string()).unwrap();
        let mut headers = domain_headers();
        headers.insert("authorization", "Bearer wrong".parse().unwrap());
        let peer: std::net::SocketAddr = "198.51.100.7:9000".parse().unwrap();

        // The attempt after the last permitted one answers 429 rather than 403.
        for _ in 0..5 {
            let refused =
                agent_register(State(app.clone()), ConnectInfo(peer), headers.clone(), "n".into()).await;
            assert_eq!(refused.status(), StatusCode::FORBIDDEN);
        }
        assert!(app.registrations.locked(peer.ip()), "the register route counts its own failures");
        assert!(!app.throttle.locked(peer.ip()), "and the panel's sign-in page is not one of them");
    }

    #[test]
    fn per_node_reads_follow_the_public_flag_and_the_public_page_switch() {
        let app = app();
        let open = node(&app, "open", true);
        let hidden = node(&app, "hidden", false);

        assert!(readable(&app, false, open), "a published node is readable by anyone");
        assert!(!readable(&app, false, hidden), "a private node is not");
        assert!(!readable(&app, false, 9999), "an unknown id is not");
        assert!(readable(&app, true, hidden), "the panel sees a private node");

        // Switching the public page off closes even a published node.
        app.db.set("public_page", "off").unwrap();
        assert!(!readable(&app, false, open));
        assert!(readable(&app, true, open), "and never closes it for the panel");
    }

    /// The window ceiling is a scan bound rather than a response bound: the
    /// thinning already limits the row count, while a quarter-year still reads
    /// every row behind it holding the write connection.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // see `gate_tests`
    async fn an_anonymous_history_window_stops_at_a_week() {
        let _serial = gate_tests();
        let app = std::sync::Arc::new(app());
        let id = node(&app, "n", true);
        let now = Utc::now().timestamp();
        // One sample per day for a month, so a row's presence identifies its
        // window. The minute of slack keeps day seven clear of the 168-hour cutoff:
        // exactly on the boundary, a second elapsing between these inserts and the
        // query below would drop it and leave the count one short.
        for day in 0..30 {
            app.db.insert_metric(id, now - day * 86_400 + 60, &json!({"cpu": 1.0})).unwrap();
        }
        let ask = |hours| {
            let query = format!("hours={hours}&series=metrics");
            metrics(
                State(app.clone()),
                HeaderMap::new(),
                Path(id),
                Query(serde_urlencoded::from_str::<Window>(&query).unwrap()),
            )
        };
        let rows =
            |body: &str| serde_json::from_str::<Value>(body).unwrap()["metrics"].as_array().unwrap().len();

        let week = axum::body::to_bytes(ask(168).await.into_body(), usize::MAX).await.unwrap();
        assert_eq!(rows(std::str::from_utf8(&week).unwrap()), 8, "a week reaches back seven days");

        // Requesting the quarter year formerly available to an anonymous caller
        // returns the week: the extra rows exist, and reading them is the cost.
        let quarter = axum::body::to_bytes(ask(2_160).await.into_body(), usize::MAX).await.unwrap();
        assert_eq!(quarter, week, "an anonymous window past a week is clamped to one");
    }

    #[tokio::test]
    async fn changing_the_password_kills_other_sessions_but_not_the_caller() {
        let app = std::sync::Arc::new(app());
        let stale = random_token();
        app.db.create_session(&sha256(&stale), Utc::now().timestamp() + 3_600, "", "", "").unwrap();

        let body = Json(json!({"admin_password": "a-long-enough-password"}));
        let response = save_settings(Admin, axum::extract::State(app.clone()), HeaderMap::new(), body).await;

        assert!(!app.db.session_valid(&sha256(&stale)), "sessions must not outlive the old password");

        // The caller receives a replacement rather than being logged out by its
        // own password change.
        let cookie = response
            .headers()
            .get(axum::http::header::SET_COOKIE)
            .expect("a replacement session")
            .to_str()
            .unwrap();
        let token = cookie.split(';').next().unwrap().split('=').nth(1).unwrap();
        assert!(app.db.session_valid(&sha256(token)), "the replacement session must work");
    }

    /// The panel hides the delete button on the caller's own row, so the mark is
    /// all that prevents an admin from signing themselves out. The row also carries
    /// which login it belongs to, so the list can say 应急密码 or a GitHub user.
    #[tokio::test]
    async fn the_session_list_marks_the_caller_and_hides_expired_rows() {
        let app = std::sync::Arc::new(app());
        let (mine, theirs, stale) = (random_token(), random_token(), random_token());
        let now = Utc::now().timestamp();
        app.db.create_session(&sha256(&mine), now + 3_600, "jacob-bytes", "", "").unwrap();
        app.db.create_session(&sha256(&theirs), now + 7_200, "", "", "").unwrap();
        app.db.create_session(&sha256(&stale), now - 1, "jacob-bytes", "", "").unwrap();

        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, format!("monitor_session={mine}").parse().unwrap());
        let body = axum::body::to_bytes(
            sessions(Admin, axum::extract::State(app.clone()), headers).await.into_body(),
            usize::MAX,
        )
        .await
        .unwrap();
        let rows: Vec<Value> = serde_json::from_slice(&body).unwrap();

        assert_eq!(rows.len(), 2, "an expired session is not a session");
        assert_eq!(rows[0]["id"], sha256(&theirs), "newest first");
        assert_eq!(rows[0]["current"], false);
        assert_eq!(rows[1]["id"], sha256(&mine));
        assert_eq!(rows[1]["current"], true, "the caller's own row must be marked");
        // created_at is stored now rather than derived from the expiry: it is the moment
        // of the insert, not the expiry minus a fixed lifetime. Deriving it meant that
        // changing the session lifetime silently rewrote the history.
        let issued = rows[1]["created_at"].as_i64().unwrap();
        assert!(issued >= now && issued <= now + 5, "issued at {issued}, expected around {now}");
        assert_eq!(rows[1]["last_seen"], issued, "a session nobody used yet was last used when issued");
        assert_eq!(rows[1]["ip"], "", "one created directly in a test carries no address");
        assert_eq!(rows[1]["user_agent"], "");
        assert_eq!(rows[1]["login"], "jacob-bytes", "a GitHub session carries its user");
        assert_eq!(rows[0]["login"], "", "the emergency password has no name to carry");

        delete_session(Admin, axum::extract::State(app.clone()), Path(sha256(&theirs))).await;
        assert!(!app.db.session_valid(&sha256(&theirs)), "the deleted device is signed out");
        assert!(app.db.session_valid(&sha256(&mine)), "and nobody else is");
    }

    #[tokio::test]
    async fn a_short_password_is_refused_and_changes_nothing() {
        let app = std::sync::Arc::new(app());
        let live = random_token();
        app.db.create_session(&sha256(&live), Utc::now().timestamp() + 3_600, "", "", "").unwrap();

        let body = Json(json!({"admin_password": "short"}));
        let response = save_settings(Admin, axum::extract::State(app.clone()), HeaderMap::new(), body).await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(app.db.get("admin_password_hash").is_none(), "the password must not have changed");
        assert!(app.db.session_valid(&sha256(&live)), "a rejected change must not log anyone out");
    }

    /// Housekeeping clamps whatever it finds, so an unparsable value is not an
    /// error downstream: it silently means 7 days, in a field still displaying
    /// what was entered.
    #[tokio::test]
    async fn a_retention_window_that_would_never_apply_is_refused() {
        let app = std::sync::Arc::new(app());
        let put = |v: &str| {
            save_settings(
                Admin,
                State(app.clone()),
                HeaderMap::new(),
                Json(json!({"retention_days": v.to_owned()})),
            )
        };
        for junk in ["", "abc", "0", "-1", "9999"] {
            assert_eq!(put(junk).await.status(), StatusCode::BAD_REQUEST, "{junk:?}");
        }
        assert!(app.db.get("retention_days").is_none(), "a refused window must not be stored");
        assert_eq!(put("7").await.status(), StatusCode::OK);
        assert_eq!(app.db.get("retention_days").as_deref(), Some("7"));
    }

    /// What `settings` returns must be what `save_settings` accepts. The panel
    /// echoes the whole form back and the write is all-or-nothing, so one key
    /// returned in a form the write refuses fails the entire page, naming a field
    /// that was never edited.
    #[tokio::test]
    async fn a_fresh_hub_answers_settings_that_it_will_take_back() {
        let app = std::sync::Arc::new(app());
        let Json(read) = settings(Admin, State(app.clone())).await;
        assert_eq!(read["retention_days"], "7", "the default belongs in the answer, not in each caller");

        // Exactly what the panel sends, on a hub where nothing was ever set.
        let echoed = json!({
            "site_name": read["site_name"],
            "retention_days": read["retention_days"],
            "github_proxy": read["github_proxy"],
            "public_page": "on",
            "notify_grace": read["notify_grace"],
            "notify_traffic": read["notify_traffic"],
            "notify_expiry": read["notify_expiry"],
            "notify_login": read["notify_login"],
            "notify_telegram_chat": read["notify_telegram_chat"],
            "notify_telegram_text": read["notify_telegram_text"],
            "notify_webhook_body": read["notify_webhook_body"],
        });
        assert_eq!(
            save_settings(Admin, State(app.clone()), HeaderMap::new(), Json(echoed)).await.status(),
            StatusCode::OK,
            "a fresh hub's own settings must survive a round trip"
        );
        assert_eq!(app.db.retention_days(), 7, "and the stored window is the one that was shown");
    }

    #[tokio::test]
    async fn settings_never_hand_back_a_secret() {
        let app = app();
        app.db.set("github_client_secret", "super-secret").unwrap();
        app.db.set("github_client_id", "public-id").unwrap();
        app.db.set("notify_telegram_token", "123:bot-secret").unwrap();
        app.db.set("notify_webhook_url", "https://hooks.example/url-secret").unwrap();
        app.db.set("notify_webhook_headers", "Authorization: header-secret").unwrap();

        let Json(body) = settings(Admin, axum::extract::State(std::sync::Arc::new(app))).await;
        assert_eq!(body["github_client_id"], "public-id");
        assert_eq!(body["github_secret_set"], true);
        assert_eq!(body["notify_webhook_url_set"], true);
        assert!(body.get("github_client_secret").is_none());
        for secret in ["super-secret", "bot-secret", "url-secret", "header-secret"] {
            assert!(!body.to_string().contains(secret), "{secret}");
        }
    }
}
