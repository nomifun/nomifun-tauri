//! Exposure tier: an orthogonal-to-Surface trust axis attached to a companion /
//! token / channel binding. `Surface` says WHERE a call comes from; `ExposureMode`
//! says HOW MUCH the caller is trusted. `PublicService` is the untrusted-stranger
//! tier: the engine is hard-clamped to a safe allowlist, no gateway, no OS tools.
//!
//! This is the backbone of the "外呼员工 / 对外服务" feature. The clamp is applied
//! at execution time in the nomi factory as a backend-authoritative gate — it
//! overrides any client- or host-supplied tool grants, so an untrusted stranger
//! messaging a published companion physically cannot reach dangerous tools,
//! regardless of prompt injection.

use serde::{Deserialize, Serialize};

/// How much the caller of a session is trusted. Orthogonal to `Surface`
/// (transport origin): the same channel can host a private (owner) or a
/// public-service (stranger) companion, so trust cannot be inferred from origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExposureMode {
    /// Owner-local companion / conversation. Full capabilities (today's behavior).
    #[default]
    Private,
    /// Owner, or an owner-trusted external agent, over the Remote front door.
    /// All-or-nothing trust (the existing Remote posture); not clamped here.
    TrustedRemote,
    /// Untrusted strangers on a public channel. Hard-clamped to safe tools only.
    PublicService,
}

impl ExposureMode {
    /// The stricter (more locked-down) of two exposure tiers. `PublicService`
    /// dominates everything (it is the only clamped tier), so if EITHER an ingress
    /// stamp or the bound companion's own tier says `PublicService`, the session is
    /// clamped. Used to combine the ingress-supplied exposure with the companion's
    /// authoritative tier in the factory.
    pub fn stricter(self, other: ExposureMode) -> ExposureMode {
        use ExposureMode::*;
        match (self, other) {
            (PublicService, _) | (_, PublicService) => PublicService,
            (TrustedRemote, _) | (_, TrustedRemote) => TrustedRemote,
            _ => Private,
        }
    }
}

/// The ONLY native tools a `PublicService` session may keep. MUST stay non-empty:
/// an empty allowlist is a **no-op** in `ToolRegistry::retain_named` (= keep
/// everything), which is the exact footgun this tier exists to prevent. Grows
/// deliberately (e.g. a firewalled safe web-search tool in P2).
pub const SAFE_PUBLIC_SERVICE_TOOLS: &[&str] = &["knowledge_search", "knowledge_read"];

/// The forced session configuration for a clamped exposure tier. Every field is
/// a hard override the factory applies regardless of upstream values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExposureClamp {
    /// Native tool allowlist fed to `builtin_allowlist` → `retain_named`.
    pub allowed_tools: Vec<String>,
    /// Whether the Desktop Gateway MCP (`nomi_*` platform tools) is injected.
    pub desktop_gateway: bool,
    /// Whether the Computer (desktop control) tool is available.
    pub computer_use: bool,
    /// Whether the Browser (CDP automation) tool is available.
    pub browser_use: bool,
    /// Whether in-process sub-agent Spawn is available (fan-out blast radius).
    pub in_process_spawn: bool,
}

/// `Some(clamp)` = force these values regardless of what the client or host
/// asked for; `None` = leave the session unmodified (Private / TrustedRemote).
pub fn exposure_clamp(mode: ExposureMode) -> Option<ExposureClamp> {
    match mode {
        ExposureMode::Private | ExposureMode::TrustedRemote => None,
        ExposureMode::PublicService => Some(ExposureClamp {
            allowed_tools: SAFE_PUBLIC_SERVICE_TOOLS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            desktop_gateway: false,
            computer_use: false,
            browser_use: false,
            in_process_spawn: false,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_private() {
        assert_eq!(ExposureMode::default(), ExposureMode::Private);
    }

    #[test]
    fn public_service_clamp_is_locked_down() {
        let c = exposure_clamp(ExposureMode::PublicService).expect("public service clamps");
        // 白名单非空不变量（空 = retain_named no-op = 放开全部）
        assert!(
            !c.allowed_tools.is_empty(),
            "PublicService allowlist MUST be non-empty"
        );
        assert!(
            c.allowed_tools
                .iter()
                .all(|t| SAFE_PUBLIC_SERVICE_TOOLS.contains(&t.as_str())),
            "allowlist must be a subset of the vetted safe set"
        );
        assert!(!c.desktop_gateway, "no gateway for strangers");
        assert!(!c.computer_use, "no desktop control for strangers");
        assert!(!c.browser_use, "no browser for strangers");
        assert!(!c.in_process_spawn, "no fan-out for strangers");
    }

    #[test]
    fn private_and_trusted_do_not_clamp() {
        assert!(exposure_clamp(ExposureMode::Private).is_none());
        assert!(exposure_clamp(ExposureMode::TrustedRemote).is_none());
    }

    #[test]
    fn safe_tools_exclude_dangerous_names() {
        for bad in [
            "Bash",
            "Write",
            "Edit",
            "ApplyPatch",
            "ExecCommand",
            "WriteStdin",
            "Computer",
            "Browser",
            "Spawn",
            "save_memory",
            "recall_memories",
            "knowledge_write",
        ] {
            assert!(
                !SAFE_PUBLIC_SERVICE_TOOLS.contains(&bad),
                "{bad} must NOT be public-safe"
            );
        }
    }

    #[test]
    fn exposure_mode_json_roundtrips_snake_case() {
        assert_eq!(
            serde_json::to_value(ExposureMode::PublicService).unwrap(),
            serde_json::json!("public_service")
        );
        let m: ExposureMode = serde_json::from_value(serde_json::json!("private")).unwrap();
        assert_eq!(m, ExposureMode::Private);
    }

    #[test]
    fn stricter_is_public_service_dominant_and_symmetric() {
        use ExposureMode::*;
        // PublicService dominates from either side.
        assert_eq!(PublicService.stricter(Private), PublicService);
        assert_eq!(Private.stricter(PublicService), PublicService);
        assert_eq!(TrustedRemote.stricter(PublicService), PublicService);
        // Non-clamped tiers combine without escalating to clamped.
        assert_eq!(Private.stricter(Private), Private);
        assert_eq!(Private.stricter(TrustedRemote), TrustedRemote);
        assert_eq!(TrustedRemote.stricter(TrustedRemote), TrustedRemote);
    }
}
