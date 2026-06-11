//! Update check (notify-only).
//!
//! At startup oproxy makes a single best-effort call to the GitHub Releases API
//! to learn the latest published version, compares it to the running version,
//! and caches the result so the UI can show an "update available" badge. It
//! never downloads or replaces anything — the right way to update is to pull a
//! new Docker image or release binary. The check is the only outbound request
//! oproxy makes on its own behalf and can be disabled with
//! `OPROXY_UPDATE_CHECK=false`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{Json, extract::State, response::IntoResponse};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::AppState;

const REPO: &str = "sauravrao637/oproxy";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
/// Re-check at most once per day when the status endpoint is polled.
const RECHECK_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone, Serialize)]
pub(crate) struct UpdateStatus {
    /// Version of the running binary (`CARGO_PKG_VERSION`).
    pub current: String,
    /// Latest published release version, once known.
    pub latest: Option<String>,
    pub update_available: bool,
    pub release_url: Option<String>,
    pub release_name: Option<String>,
    pub published_at: Option<String>,
    /// True once a check has completed (success or failure).
    pub checked: bool,
    /// Set when the last check failed (offline, rate-limited, etc.).
    pub error: Option<String>,
    #[serde(skip)]
    checked_at: Option<Instant>,
}

impl UpdateStatus {
    fn initial() -> Self {
        Self {
            current: CURRENT_VERSION.to_string(),
            latest: None,
            update_available: false,
            release_url: None,
            release_name: None,
            published_at: None,
            checked: false,
            error: None,
            checked_at: None,
        }
    }

    fn is_stale(&self) -> bool {
        match self.checked_at {
            None => true,
            Some(at) => at.elapsed() >= RECHECK_AFTER,
        }
    }
}

pub(crate) type SharedUpdateStatus = Arc<RwLock<UpdateStatus>>;

pub(crate) fn new_update_status() -> SharedUpdateStatus {
    Arc::new(RwLock::new(UpdateStatus::initial()))
}

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    published_at: Option<String>,
}

/// Best-effort refresh: fetch the latest release and update the shared status.
/// Never panics; failures are recorded in `status.error`.
pub(crate) async fn refresh_update_status(shared: SharedUpdateStatus) {
    let outcome = fetch_latest_release().await;
    let mut status = shared.write().await;
    status.checked = true;
    status.checked_at = Some(Instant::now());
    match outcome {
        Ok(release) => {
            let latest = release.tag_name.trim().to_string();
            status.update_available = is_newer(&latest, CURRENT_VERSION);
            status.release_url = release.html_url;
            status.release_name = release.name;
            status.published_at = release.published_at;
            status.latest = Some(latest);
            status.error = None;
        }
        Err(e) => {
            status.error = Some(e);
        }
    }
}

async fn fetch_latest_release() -> Result<GitHubRelease, String> {
    let client = reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .user_agent(format!("oproxy/{CURRENT_VERSION}"))
        .build()
        .map_err(|e| format!("update client error: {e}"))?;
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let response = client
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("update check request failed: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("update check returned {}", response.status()));
    }
    response
        .json::<GitHubRelease>()
        .await
        .map_err(|e| format!("update check response was not valid JSON: {e}"))
}

pub(super) async fn get_update_status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    // Lazily re-check in the background when the cached result is stale, so a
    // long-running instance still notices new releases without polling GitHub
    // on every request. Disabled entirely when the user opted out.
    if state.config.update_check {
        let stale = state.update_status.read().await.is_stale();
        if stale {
            tokio::spawn(refresh_update_status(state.update_status.clone()));
        }
    }
    let status = state.update_status.read().await.clone();
    // Surface whether checking is enabled so the UI can distinguish "disabled"
    // from "still checking".
    let mut body = serde_json::to_value(&status).unwrap_or_else(|_| serde_json::json!({}));
    if let Some(obj) = body.as_object_mut() {
        obj.insert(
            "enabled".to_string(),
            serde_json::json!(state.config.update_check),
        );
    }
    Json(body)
}

/// Compare dotted versions, tolerating a leading `v` and ignoring pre-release /
/// build suffixes. Returns true only when `latest` is strictly greater than
/// `current`. Unparseable versions are treated as "no update" (fail safe).
fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

fn parse_version(value: &str) -> Option<(u64, u64, u64)> {
    let trimmed = value.trim().trim_start_matches(['v', 'V']);
    let core = trimmed.split(['-', '+']).next().unwrap_or(trimmed);
    let mut parts = core.split('.');
    let major = parts.next()?.parse::<u64>().ok()?;
    let minor = parts.next().unwrap_or("0").parse::<u64>().ok()?;
    let patch = parts.next().unwrap_or("0").parse::<u64>().ok()?;
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions_with_prefix_and_suffix() {
        assert_eq!(parse_version("v0.1.6"), Some((0, 1, 6)));
        assert_eq!(parse_version("0.2.0"), Some((0, 2, 0)));
        assert_eq!(parse_version("1.0"), Some((1, 0, 0)));
        assert_eq!(parse_version("v2.3.4-rc1"), Some((2, 3, 4)));
        assert_eq!(parse_version("not-a-version"), None);
    }

    #[test]
    fn is_newer_compares_semver_numerically() {
        assert!(is_newer("v0.1.6", "0.1.0"));
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.6")); // older release never flags an update
        assert!(!is_newer("garbage", "0.1.0")); // fail safe
    }

    #[test]
    fn initial_status_is_unchecked_with_current_version() {
        let status = UpdateStatus::initial();
        assert_eq!(status.current, CURRENT_VERSION);
        assert!(!status.checked);
        assert!(!status.update_available);
        assert!(status.is_stale()); // never checked => stale => triggers first check
    }
}
