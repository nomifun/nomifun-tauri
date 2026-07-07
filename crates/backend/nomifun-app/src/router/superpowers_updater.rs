//! Periodic auto-update of the built-in superpowers skills corpus from GitHub.
//!
//! A best-effort background janitor (mirrors `spawn_idmm_record_janitor`): on
//! boot and every N hours it checks the upstream GitHub release and installs a
//! newer version as the overlay corpus (`{data_dir}/superpowers`), broadcasting
//! `superpowers.updated` to the UI. Enabled by default; every step is fail-soft
//! so it never blocks startup or disturbs a running session.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use nomifun_api_types::WebSocketMessage;
use nomifun_realtime::{BroadcastEventBus, EventBroadcaster};

const ENV_AUTOUPDATE: &str = "NOMIFUN_SUPERPOWERS_AUTOUPDATE";
const ENV_INTERVAL: &str = "NOMIFUN_SUPERPOWERS_UPDATE_INTERVAL_SECS";

/// Default check cadence: every 6 hours (well under GitHub's unauthenticated
/// 60 req/hr/IP limit).
const DEFAULT_INTERVAL_SECS: u64 = 6 * 60 * 60;
/// Floor so a misconfigured interval can't hammer the API.
const MIN_INTERVAL_SECS: u64 = 300;

/// Spawn the periodic superpowers auto-update janitor. No-op when disabled via
/// [`ENV_AUTOUPDATE`].
pub fn spawn_superpowers_updater(data_dir: PathBuf, event_bus: Arc<BroadcastEventBus>) {
    if !autoupdate_enabled_from(std::env::var(ENV_AUTOUPDATE).ok().as_deref()) {
        tracing::info!("superpowers auto-update disabled ({ENV_AUTOUPDATE})");
        return;
    }
    let interval = interval_secs_from(std::env::var(ENV_INTERVAL).ok().as_deref());
    tracing::info!(interval_secs = interval, "superpowers auto-update janitor started");
    tokio::spawn(async move {
        // The first `interval` tick fires immediately → a boot-time check for free.
        let mut ticker = tokio::time::interval(Duration::from_secs(interval));
        loop {
            ticker.tick().await;
            run_superpowers_update_once(&data_dir, &event_bus).await;
        }
    });
}

/// One update cycle: fetch latest → compare → install if newer → broadcast.
/// Returns whether an update was installed. Fail-soft: any error is logged and
/// yields `false` (retried on the next tick).
async fn run_superpowers_update_once(data_dir: &Path, event_bus: &Arc<BroadcastEventBus>) -> bool {
    let latest = match nomifun_extension::fetch_latest_release().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "superpowers update check failed (will retry)");
            return false;
        }
    };
    let current = nomifun_extension::installed_overlay_version(data_dir)
        .unwrap_or_else(|| nomifun_extension::SUPERPOWERS_BUNDLED_VERSION.trim().to_owned());

    if !nomifun_extension::should_install_release(&latest.version, &current) {
        tracing::debug!(latest = %latest.version, current = %current, "superpowers already up to date");
        return false;
    }

    tracing::info!(from = %current, to = %latest.version, "superpowers update available; installing");
    match nomifun_extension::install_superpowers_overlay(data_dir, &latest).await {
        Ok(()) => {
            event_bus.broadcast(WebSocketMessage::new(
                "superpowers.updated",
                serde_json::json!({ "version": latest.version }),
            ));
            true
        }
        Err(e) => {
            tracing::warn!(error = %e, version = %latest.version, "superpowers overlay install failed (will retry)");
            false
        }
    }
}

/// Auto-update is ON by default; disabled only by an explicit falsey value.
fn autoupdate_enabled_from(v: Option<&str>) -> bool {
    match v.map(str::trim) {
        Some(s) => !(s == "0" || s.eq_ignore_ascii_case("false") || s.eq_ignore_ascii_case("off")),
        None => true,
    }
}

/// Parse the configured interval, flooring at [`MIN_INTERVAL_SECS`] and
/// defaulting to [`DEFAULT_INTERVAL_SECS`] when unset/invalid.
fn interval_secs_from(v: Option<&str>) -> u64 {
    v.and_then(|s| s.trim().parse::<u64>().ok())
        .map(|s| s.max(MIN_INTERVAL_SECS))
        .unwrap_or(DEFAULT_INTERVAL_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autoupdate_on_by_default_off_only_when_falsey() {
        assert!(autoupdate_enabled_from(None), "unset → on by default");
        assert!(autoupdate_enabled_from(Some("")), "blank → on");
        assert!(autoupdate_enabled_from(Some("1")));
        assert!(autoupdate_enabled_from(Some("true")));
        assert!(!autoupdate_enabled_from(Some("0")));
        assert!(!autoupdate_enabled_from(Some("false")));
        assert!(!autoupdate_enabled_from(Some("  OFF ")));
    }

    #[test]
    fn interval_defaults_and_floors() {
        assert_eq!(interval_secs_from(None), DEFAULT_INTERVAL_SECS);
        assert_eq!(interval_secs_from(Some("not-a-number")), DEFAULT_INTERVAL_SECS);
        assert_eq!(interval_secs_from(Some("60")), MIN_INTERVAL_SECS, "floored to minimum");
        assert_eq!(interval_secs_from(Some("7200")), 7200);
    }
}
