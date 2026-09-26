//! Keeping installed themes current.
//!
//! Two callers read a theme's own repository: the panel's update button, which a
//! maintainer presses, and a daily check that only reports. Both go through this
//! module so the addresses are built in one place, the archive is verified the
//! same way whichever path fetched it, and what the panel offers to install
//! cannot drift from what the check found.
//!
//! The check exists because the hub and its theme are released separately. The
//! binary embeds a theme, so a fresh install works with no network at all, but
//! that copy is frozen at build time -- before this, the only way its operator
//! learned a new theme existed was to press a button and be told, which is not a
//! way to learn anything.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use axum::http::header;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::api::MAX_THEME;
use crate::{frontend, proxied, App, Shared};

/// The archive a theme release publishes, and the checksum published beside it.
/// The name is the contract between the hub and the theme repository's release
/// workflow; the checksum is what makes a mirror or a truncated transfer safe to
/// install from.
const ARCHIVE: &str = "theme.tar.gz";
const SUM: &str = "theme.tar.gz.sha256";

/// How long between checks. A day: a theme release is a rare event, and the
/// release API is unauthenticated -- one request per theme per day is nowhere
/// near the 60 an hour GitHub allows one address.
const INTERVAL: std::time::Duration = std::time::Duration::from_secs(24 * 3_600);

/// How long after startup the first check runs. Long enough that the panel and
/// the agents have settled, and that a hub started by a service manager is not
/// competing with its own boot.
const FIRST: std::time::Duration = std::time::Duration::from_secs(30);

/// How many themes one round checks, so a directory full of third-party themes
/// cannot turn a daily check into a burst of requests.
const MOST: usize = 8;

/// What the last check found.
#[derive(Default, Clone, Serialize)]
pub struct Check {
    /// When a round last finished, epoch seconds; 0 before the first one.
    pub checked_at: i64,
    pub themes: BTreeMap<String, Status>,
}

#[derive(Clone, Serialize)]
pub struct Status {
    /// The version the repository's latest release carries, once it was read.
    pub latest: Option<String>,
    /// Whether that release is later than the installed copy. This is what the
    /// panel turns into a badge; the update itself installs whatever *differs*,
    /// a deliberate downgrade included, because the release is what its author
    /// published.
    pub newer: bool,
    pub error: Option<String>,
}

/// The part of a GitHub release this module reads.
#[derive(Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
}

/// The last check's answer, for the panel.
pub fn state(app: &App) -> Check {
    app.theme_check.lock().unwrap().clone()
}

/// Records that `short` now stands at `version`.
///
/// An install makes the check's answer stale by construction -- the copy on disk
/// is the one the repository publishes -- and the next round is a day away. Left
/// alone, the panel would keep offering an update that has already happened.
/// Called from every path that writes a theme: the update button, the upload
/// button, and (by removal) a delete.
pub fn installed(app: &App, short: &str, version: &str) {
    app.theme_check
        .lock()
        .unwrap()
        .themes
        .insert(short.to_owned(), Status { latest: Some(version.to_owned()), newer: false, error: None });
}

/// Forgets one theme's entry, for a theme that is no longer installed.
pub fn removed(app: &App, short: &str) {
    app.theme_check.lock().unwrap().themes.remove(short);
}

/// Checks every installed theme that names a GitHub repository, once, and
/// remembers what it found.
pub async fn check_all(app: &App) {
    let themes = match frontend::themes(app) {
        Ok(themes) => themes,
        Err(e) => {
            warn!("theme update check could not list themes: {e:#}");
            return;
        }
    };
    let mut found = BTreeMap::new();
    for theme in themes.iter().take(MOST) {
        // A theme with no repository to read is not a failure: an uploaded theme
        // or one built locally simply has no source of updates, which is what
        // the panel already says by not offering the button.
        let Some((owner, repo)) = repo(&theme.url) else { continue };
        let status = match latest(app, owner, repo).await {
            Ok(release) => {
                let latest = strip_v(&release.tag_name).to_owned();
                Status { newer: newer(&latest, &theme.version), latest: Some(latest), error: None }
            }
            Err(e) => Status { latest: None, newer: false, error: Some(format!("{e:#}")) },
        };
        found.insert(theme.short.clone(), status);
    }
    let checked = found.len();
    let failed = found.values().filter(|s| s.error.is_some()).count();
    *app.theme_check.lock().unwrap() = Check { checked_at: Utc::now().timestamp(), themes: found };
    // One line per round rather than one per failure: an offline hub would
    // otherwise write the same warning every day forever.
    if failed > 0 {
        warn!("theme update check: {checked} theme(s) checked, {failed} could not be read");
    } else {
        info!("theme update check: {checked} theme(s) checked");
    }
}

/// The daily loop behind the check.
///
/// Failure is not retried sooner: the hub is a server, the check is a courtesy,
/// and a repository that cannot be read today is not more readable in an hour.
pub async fn watch(app: Shared) {
    tokio::time::sleep(FIRST).await;
    loop {
        check_all(&app).await;
        tokio::time::sleep(INTERVAL).await;
    }
}

/// The latest release of one repository: the version the panel offers and, on
/// the update path, the tag it installs.
pub async fn latest(app: &App, owner: &str, repo: &str) -> Result<Release> {
    // Unauthenticated, and the tag rather than the assets: the archive is
    // fetched separately, and through the panel's GitHub proxy when one is set.
    let release: Release = app
        .http
        .get(format!("https://api.github.com/repos/{owner}/{repo}/releases/latest"))
        .header(header::USER_AGENT, "monitor-hub")
        .send()
        .await?
        .error_for_status()
        .with_context(|| format!("读不到 {owner}/{repo} 的最新 release"))?
        .json()
        .await?;
    if !path_segment(&release.tag_name) {
        bail!("release 的 tag {:?} 不能出现在下载地址里", release.tag_name);
    }
    // Checked here rather than by downloading and reading a 404: the asset name
    // is the contract, and stating so is the entire error message.
    if !release.assets.iter().any(|asset| asset.name == ARCHIVE) {
        bail!("release {} 里没有 {ARCHIVE}", release.tag_name);
    }
    Ok(release)
}

/// One theme release's archive, checksummed against the file published beside it.
///
/// The checksum is fetched first and compared after: it is a tenth of a kilobyte,
/// and a hub that cannot read it must not install anything at all. What this
/// proves is integrity rather than authorship -- the sum comes from the same
/// release as the archive -- which is exactly what the build-time pin proves for
/// the embedded copy, and all either can prove without a signature.
pub async fn archive(app: &App, owner: &str, repo: &str, tag: &str) -> Result<Vec<u8>> {
    if !path_segment(tag) {
        bail!("release 的 tag {tag:?} 不能出现在下载地址里");
    }
    let base = format!("https://github.com/{owner}/{repo}/releases/download/{tag}");
    let sum = app
        .http
        .get(proxied(app, format!("{base}/{SUM}")))
        .timeout(std::time::Duration::from_secs(120))
        .send()
        .await?
        .error_for_status()
        .with_context(|| format!("读不到 {owner}/{repo} {tag} 的 {SUM}"))?
        .text()
        .await?;
    let want = sum.split_whitespace().next().unwrap_or_default().to_ascii_lowercase();
    if want.len() != 64 || !want.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("{tag} 的 {SUM} 里不是 sha256");
    }

    // Through the panel's GitHub proxy when one is configured, the archive being
    // the part a blocked network cannot reach.
    let response = app
        .http
        .get(proxied(app, format!("{base}/{ARCHIVE}")))
        .timeout(std::time::Duration::from_secs(120))
        .send()
        .await?
        .error_for_status()?;
    // The transfer stops at Content-Length, so checking it checks the body: a
    // header understating the archive cannot make more arrive. GitHub always
    // sends one; a proxy that omits it is refused rather than read unbounded.
    match response.content_length() {
        Some(size) if size <= MAX_THEME => {}
        Some(size) => bail!("主题包 {} MiB，超过 {} MiB 的上限", size / 1024 / 1024, MAX_THEME / 1024 / 1024),
        None => bail!("下载没有给出大小，无法确认它在 {} MiB 以内", MAX_THEME / 1024 / 1024),
    }
    let archive = response.bytes().await?;
    let got = hex::encode(Sha256::digest(&archive));
    if got != want {
        bail!("主题包 sha256 是 {got}，而 release 里写的是 {want}；这个包不能装");
    }
    Ok(archive.to_vec())
}

/// Whether `latest` names a later version than `installed`.
///
/// Dotted numbers compared component by component, which is what these manifests
/// carry (`1.6.0` against a tag's `v1.6.0`). Anything that is not all numbers --
/// a date, a hash, a `1.7.0-rc1` -- falls back to "different", so an odd version
/// string shows a badge that a click resolves rather than hiding an update.
pub fn newer(latest: &str, installed: &str) -> bool {
    match (parts(latest), parts(installed)) {
        (Some(latest), Some(installed)) => {
            for i in 0..latest.len().max(installed.len()) {
                let (a, b) = (latest.get(i).copied().unwrap_or(0), installed.get(i).copied().unwrap_or(0));
                if a != b {
                    return a > b;
                }
            }
            false
        }
        _ => latest != installed,
    }
}

fn parts(version: &str) -> Option<Vec<u64>> {
    version.split('.').map(|part| part.parse().ok()).collect()
}

/// The version a release tag names. Tags read `v1.2.3`, manifests `1.2.3`.
pub fn strip_v(tag: &str) -> &str {
    tag.strip_prefix('v').unwrap_or(tag)
}

/// The `<owner>/<repo>` a theme's `url` names, where it names a GitHub repository
/// at all.
///
/// An allowlist rather than a filter. Every address this module fetches is
/// constructed from these two strings, so nothing in a manifest can direct the
/// hub at a host it did not choose, which is why no private-address check is
/// needed here. The only host that is not github.com is the GitHub proxy in the
/// panel's settings, configured by the operator and already used by the agent
/// relay.
pub fn repo(url: &str) -> Option<(&str, &str)> {
    let (owner, rest) = url.strip_prefix("https://github.com/")?.split_once('/')?;
    // A link to a branch or a file is still a link to the repository.
    let repo = rest.split('/').next()?;
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    (path_segment(owner) && path_segment(repo)).then_some((owner, repo))
}

/// One URL path segment the hub will build a github.com address from: nothing
/// that opens a new segment, and nothing that escapes the current one.
fn path_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && segment.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The update path follows a manifest's `url` to build a download address, so
    /// what counts as a GitHub repository constitutes the entire trust boundary:
    /// whatever this accepts, the hub will fetch.
    #[test]
    fn only_a_github_repository_url_can_name_a_release_to_download() {
        assert_eq!(repo("https://github.com/monitor-probe/monitor"), Some(("monitor-probe", "monitor")));
        // A link to the repository, in whatever form the author wrote it.
        assert_eq!(repo("https://github.com/a/b.git"), Some(("a", "b")));
        assert_eq!(repo("https://github.com/a/b/tree/main"), Some(("a", "b")));
        assert_eq!(repo("https://github.com/a/b/"), Some(("a", "b")));

        for hostile in [
            "",
            // Not github.com, however much of it appears in the string.
            "http://github.com/a/b",
            "https://github.com.evil.test/a/b",
            "https://github.com@evil.test/a/b",
            "https://evil.test/https://github.com/a/b",
            // On github.com, but naming no repository to fetch from.
            "https://github.com/a",
            "https://github.com//b",
            "https://github.com/../../etc/passwd",
            "https://github.com/a/..",
            // Anything that could open a segment of its own in the URL built from
            // it, whether encoded, queried or fragmented.
            "https://github.com/a/b%2f..%2fc",
            "https://github.com/a/b?x=1",
            "https://github.com/a b",
        ] {
            assert_eq!(repo(hostile), None, "{hostile} must not name a download");
        }

        // The release tag also lands in that URL, arriving from the API rather
        // than the manifest.
        assert!(path_segment("v0.1.15") && path_segment("2024.1"));
        assert!(!path_segment("release/1.0") && !path_segment("..") && !path_segment(""));
    }

    #[test]
    fn a_release_is_newer_only_when_its_version_is() {
        // The ordinary case, and the one a badge turns on.
        assert!(newer("1.6.0", "1.5.1"));
        assert!(newer("1.10.0", "1.9.9"), "components compare as numbers, not as text");
        assert!(newer("2.0.0", "1.99.99"));
        assert!(!newer("1.5.1", "1.5.1"));
        assert!(!newer("1.5.0", "1.5.1"), "a local build ahead of the release shows no badge");
        // Shorter and longer spellings of the same version.
        assert!(!newer("1.5", "1.5.0"));
        assert!(newer("1.5.1", "1.5"));
        // Anything unparseable is offered rather than hidden: the cost of a
        // wrong badge is one wasted click, the cost of a missing one is the
        // update nobody sees. See the note on `newer`.
        assert!(newer("1.7.0-rc1", "1.6.0"));
        assert!(!newer("1.7.0-rc1", "1.7.0-rc1"));
        assert!(newer("2026-09-26", "1.6.0"));
    }

    #[test]
    fn a_tag_is_read_with_or_without_its_v() {
        assert_eq!(strip_v("v1.6.0"), "1.6.0");
        assert_eq!(strip_v("1.6.0"), "1.6.0");
    }
}
