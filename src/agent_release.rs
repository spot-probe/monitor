//! Which agent release exists, so the panel can say which nodes are behind it.
//!
//! The hub already knows what each node runs -- the agent sends its own version
//! at the handshake, and `node.agent_version` keeps it -- but not whether that
//! version is current. That needs the repository, which is the one thing the
//! relay path does not carry: `/agent/{arch}` streams bytes from
//! `releases/latest` and a byte stream has no tag in it.
//!
//! One tag a day is enough for a question whose answer changes only when a
//! release is cut, and it is the cadence `theme::watch` already runs at, for the
//! same reasons: a repository that cannot be read today is not more readable in
//! an hour.
//!
//! Nothing here installs or offers to install. Upgrading a node means running
//! its install command again *on that machine*, so the hub's whole job is to say
//! which ones are behind; the panel points at the per-node install dialog for
//! the command.

use std::time::Duration;

use axum::http::header;
use serde::Deserialize;
use tracing::{info, warn};

use crate::{proxied, App, Shared};

/// How often the repository is read, and how long after startup the first read
/// happens.
const INTERVAL: Duration = Duration::from_secs(24 * 3_600);
const FIRST: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Default)]
pub struct Check {
    pub checked_at: i64,
    /// The newest release, without its `v`. `None` while it has never been read
    /// -- and again after a round that failed, because a version this hub cannot
    /// confirm right now is not one it should be marking nodes against. The
    /// panel draws nothing when this is `None`.
    pub latest: Option<String>,
    /// Kept for the log line; the panel deliberately shows nothing for it
    /// (a hub that cannot reach GitHub would otherwise carry a permanent
    /// complaint about a feature that is merely unavailable).
    pub error: Option<String>,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
}

pub fn state(app: &App) -> Check {
    app.agent_release.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// The daily loop.
pub async fn watch(app: Shared) {
    tokio::time::sleep(FIRST).await;
    loop {
        refresh(&app).await;
        tokio::time::sleep(INTERVAL).await;
    }
}

/// One read of the latest agent release.
pub async fn refresh(app: &App) {
    let url = format!("https://api.github.com/repos/{}/releases/latest", crate::AGENT_REPO);
    // Through the panel's GitHub proxy, which the theme check does not do: this
    // is the one call whose whole purpose is to learn a version, and a hub that
    // cannot reach api.github.com directly is exactly the one whose operator
    // configured a proxy.
    let read = app
        .http
        .get(proxied(app, url))
        .header(header::USER_AGENT, "monitor-hub")
        .send()
        .await
        .and_then(|r| r.error_for_status());
    let check = match read {
        Ok(response) => match response.json::<Release>().await {
            Ok(release) => Check {
                checked_at: chrono::Utc::now().timestamp(),
                latest: Some(crate::theme::strip_v(&release.tag_name).to_owned()),
                error: None,
            },
            Err(e) => failed(format!("读不出 release 的 tag：{e}")),
        },
        Err(e) => failed(format!("{e:#}")),
    };
    match &check.latest {
        Some(latest) => info!("agent release check: latest is {latest}"),
        None => warn!("agent release check: {}", check.error.as_deref().unwrap_or("no version")),
    }
    *app.agent_release.lock().unwrap_or_else(|e| e.into_inner()) = check;
}

fn failed(reason: String) -> Check {
    Check { checked_at: chrono::Utc::now().timestamp(), latest: None, error: Some(reason) }
}

/// Whether `node_version` is behind `latest`.
///
/// An empty `node_version` is a node that has never connected, and it is not
/// behind anything -- `theme::newer` would call it different and mark it. The
/// numeric comparison itself is the theme check's, so `1.10.0` and `1.9.0` are
/// ordered by value rather than as strings.
pub fn behind(latest: Option<&str>, node_version: &str) -> bool {
    !node_version.is_empty() && latest.is_some_and(|latest| crate::theme::newer(latest, node_version))
}

#[cfg(test)]
mod tests {
    use super::behind;

    #[test]
    fn only_an_older_node_version_counts_as_behind() {
        assert!(behind(Some("1.1.0"), "1.0.0"));
        assert!(behind(Some("1.1.0"), "1.0.9"));
        // Numeric, not lexicographic: 1.10.0 is newer than 1.9.0.
        assert!(behind(Some("1.10.0"), "1.9.0"));
        assert!(!behind(Some("1.9.0"), "1.10.0"));
        assert!(!behind(Some("1.1.0"), "1.1.0"));
        // A node built from a later commit is ahead, not behind.
        assert!(!behind(Some("1.1.0"), "1.2.0"));
    }

    #[test]
    fn an_unknown_latest_or_an_unreported_version_is_not_behind() {
        // Nothing read yet, or the read failed: no claim either way.
        assert!(!behind(None, "1.0.0"));
        // Never connected -- there is no installed version to be behind.
        assert!(!behind(Some("1.1.0"), ""));
    }
}
