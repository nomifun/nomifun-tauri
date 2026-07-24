//! Build channel (`NOMI_CHANNEL`) — the single source of truth that lets a
//! non-stable build (e.g. `dev`) coexist with the installed stable app by
//! deriving a per-channel data-directory suffix and runtime announcement.
//!
//! The channel is baked at compile time from the `NOMI_CHANNEL` env var (set by
//! the development scripts); unset means `stable`. Only the exact string
//! `stable` maps to the production data directory — every other value
//! (including typos) gets an isolated suffix, so a mis-set channel can never
//! write into the installed app's state.
//!
//! This module lives in the top-level application crate because only app hosts
//! consume it. Changing channels therefore recompiles the host boundary instead
//! of invalidating `nomifun-common` and every domain crate downstream of it.

/// The compile-time channel. `stable` when `NOMI_CHANNEL` is unset.
pub fn channel() -> &'static str {
    option_env!("NOMI_CHANNEL").unwrap_or("stable")
}

/// Path suffix appended to the `Nomi` data-dir leaf for this channel.
/// `stable` → "" (production); anything else → "-<channel>" (isolated).
pub fn dir_suffix() -> String {
    suffix_for(channel())
}

/// True only for the production (stable) channel.
pub fn is_stable() -> bool {
    channel() == "stable"
}

/// Whether `channel` is a recognized channel name (vs a typo). Used by the
/// startup self-check to warn on `NOMI_CHANNEL=Dev` and friends.
pub fn is_known(channel: &str) -> bool {
    matches!(channel, "stable" | "dev" | "beta" | "canary")
}

/// Pure mapping: only the exact `stable` yields the empty (production) suffix;
/// every other value is isolated under `-<channel>`.
fn suffix_for(channel: &str) -> String {
    if channel == "stable" {
        String::new()
    } else {
        format!("-{channel}")
    }
}

#[cfg(test)]
mod tests {
    use super::{is_known, suffix_for};

    #[test]
    fn stable_has_no_suffix() {
        assert_eq!(suffix_for("stable"), "");
    }

    #[test]
    fn dev_is_isolated_under_dash_dev() {
        assert_eq!(suffix_for("dev"), "-dev");
    }

    #[test]
    fn future_channels_get_their_own_suffix() {
        assert_eq!(suffix_for("beta"), "-beta");
        assert_eq!(suffix_for("canary"), "-canary");
    }

    #[test]
    fn only_exact_stable_maps_to_production_dir() {
        // Safety invariant: a typo must NOT silently land in the prod data dir.
        assert_eq!(suffix_for("Dev"), "-Dev");
        assert_ne!(suffix_for("Dev"), "");
        assert_ne!(suffix_for("stable "), "");
    }

    #[test]
    fn known_channels_recognized_typos_rejected() {
        assert!(is_known("stable") && is_known("dev") && is_known("beta") && is_known("canary"));
        assert!(!is_known("Dev"));
        assert!(!is_known("prod"));
    }
}
