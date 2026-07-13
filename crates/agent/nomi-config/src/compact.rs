use serde::{Deserialize, Serialize};

/// Configuration for the multi-level context compaction system.
///
/// All token-related fields are in tokens (not bytes or characters).
/// The defaults are tuned for Claude models with a 200k context window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactConfig {
    /// Context window size in tokens (e.g. 200_000 for Claude).
    #[serde(default = "default_context_window")]
    pub context_window: usize,

    /// Tokens reserved for output generation.
    /// Subtracted from `context_window` to get the effective input budget.
    #[serde(default = "default_output_reserve")]
    pub output_reserve: usize,

    /// Buffer below the effective window that triggers autocompact.
    /// `threshold = context_window - output_reserve - autocompact_buffer`
    #[serde(default = "default_autocompact_buffer")]
    pub autocompact_buffer: usize,

    /// Tokens from context_window limit to trigger emergency block.
    /// `emergency_limit = context_window - emergency_buffer`
    #[serde(default = "default_emergency_buffer")]
    pub emergency_buffer: usize,

    /// Max consecutive autocompact failures before the circuit breaker trips.
    #[serde(default = "default_max_failures")]
    pub max_failures: u32,

    /// Microcompact: keep the N most recent compactable tool results.
    #[serde(default = "default_micro_keep_recent")]
    pub micro_keep_recent: usize,

    /// Microcompact: gap threshold in seconds for time-based trigger.
    /// When the last assistant message is older than this, microcompact fires.
    #[serde(default = "default_micro_gap_seconds")]
    pub micro_gap_seconds: u64,

    /// Tool names whose results are eligible for microcompact content clearing.
    #[serde(default = "default_compactable_tools")]
    pub compactable_tools: Vec<String>,

    /// Optional percentage override for the autocompact trigger threshold.
    /// When set, threshold = context_window * pct / 100, ignoring
    /// output_reserve and autocompact_buffer.
    #[serde(default)]
    pub autocompact_threshold_pct: Option<u8>,

    /// Whether the compaction system is enabled.
    /// When false, microcompact and autocompact are skipped
    /// (emergency truncation still applies).
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Enable prompt cache diagnostics output to user.
    /// When true, cache hit/miss info is shown via OutputSink.
    /// Default: false.
    #[serde(default)]
    pub cache_diagnostics: bool,

    #[serde(default)]
    pub compaction: nomi_compact::CompactionLevel,

    #[serde(default)]
    pub toon: bool,
}

impl Default for CompactConfig {
    fn default() -> Self {
        Self {
            context_window: default_context_window(),
            output_reserve: default_output_reserve(),
            autocompact_buffer: default_autocompact_buffer(),
            emergency_buffer: default_emergency_buffer(),
            max_failures: default_max_failures(),
            micro_keep_recent: default_micro_keep_recent(),
            micro_gap_seconds: default_micro_gap_seconds(),
            compactable_tools: default_compactable_tools(),
            autocompact_threshold_pct: None,
            enabled: default_true(),
            cache_diagnostics: false,
            compaction: nomi_compact::CompactionLevel::default(),
            toon: false,
        }
    }
}

// --- Default value functions ---

fn default_context_window() -> usize {
    200_000
}
fn default_output_reserve() -> usize {
    20_000
}
fn default_autocompact_buffer() -> usize {
    13_000
}
fn default_emergency_buffer() -> usize {
    3_000
}
fn default_max_failures() -> u32 {
    3
}
fn default_micro_keep_recent() -> usize {
    5
}
fn default_micro_gap_seconds() -> u64 {
    3600
}
fn default_compactable_tools() -> Vec<String> {
    vec![
        "Read".into(),
        "Bash".into(),
        "Grep".into(),
        "Glob".into(),
        "Write".into(),
        "Edit".into(),
    ]
}
fn default_true() -> bool {
    true
}

/// Resolve the effective context window: an explicit per-provider limit when
/// set and positive, else the engine default. Keeps the gauge denominator and
/// the engine's compaction window in agreement.
pub fn resolve_context_window(context_limit: Option<u64>, default_window: usize) -> usize {
    match context_limit {
        Some(v) if v > 0 => usize::try_from(v).unwrap_or(usize::MAX),
        _ => default_window,
    }
}

/// Fit the response and compaction budgets inside the resolved context window.
///
/// Nomi's defaults target a 200k context window. Reusing them unchanged for a
/// small provider can reserve more tokens than the provider accepts before the
/// first user message is considered. The effective response limit is capped at
/// one quarter of the window. Oversized compaction defaults are reduced to a
/// response-sized output reserve plus an eighth-window autocompact buffer,
/// while already-balanced custom settings are preserved.
pub fn fit_context_budget(config: &mut CompactConfig, requested_max_tokens: u32) -> u32 {
    let context_window = config.context_window.max(1);
    let output_cap = (context_window / 4).max(1);
    let output_cap_u32 = u32::try_from(output_cap).unwrap_or(u32::MAX);
    let max_tokens = requested_max_tokens.min(output_cap_u32);

    // The compactor must reserve at least as much as the provider may emit.
    config.output_reserve = config.output_reserve.max(max_tokens as usize);

    if config
        .output_reserve
        .saturating_add(config.autocompact_buffer)
        > context_window / 2
    {
        config.output_reserve = max_tokens as usize;
        config.autocompact_buffer = config
            .autocompact_buffer
            .min((context_window / 8).max(1));
    }

    config.emergency_buffer = config
        .emergency_buffer
        .min((context_window / 16).max(1));

    max_tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_values_match_spec() {
        let cfg = CompactConfig::default();
        assert_eq!(cfg.context_window, 200_000);
        assert_eq!(cfg.output_reserve, 20_000);
        assert_eq!(cfg.autocompact_buffer, 13_000);
        assert_eq!(cfg.emergency_buffer, 3_000);
        assert_eq!(cfg.max_failures, 3);
        assert_eq!(cfg.micro_keep_recent, 5);
        assert_eq!(cfg.micro_gap_seconds, 3600);
        assert!(cfg.enabled);
        assert_eq!(cfg.autocompact_threshold_pct, None);
        assert_eq!(
            cfg.compactable_tools,
            vec!["Read", "Bash", "Grep", "Glob", "Write", "Edit"]
        );
    }

    #[test]
    fn toml_full_override() {
        let toml_str = r#"
context_window = 128000
output_reserve = 10000
autocompact_buffer = 8000
emergency_buffer = 2000
max_failures = 5
micro_keep_recent = 3
micro_gap_seconds = 1800
compactable_tools = ["Read", "Bash"]
enabled = false
"#;
        let cfg: CompactConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.context_window, 128_000);
        assert_eq!(cfg.output_reserve, 10_000);
        assert_eq!(cfg.autocompact_buffer, 8_000);
        assert_eq!(cfg.emergency_buffer, 2_000);
        assert_eq!(cfg.max_failures, 5);
        assert_eq!(cfg.micro_keep_recent, 3);
        assert_eq!(cfg.micro_gap_seconds, 1800);
        assert_eq!(cfg.compactable_tools, vec!["Read", "Bash"]);
        assert!(!cfg.enabled);
    }

    #[test]
    fn toml_partial_override_uses_defaults() {
        let toml_str = r#"
context_window = 128000
"#;
        let cfg: CompactConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.context_window, 128_000);
        // Everything else should be default
        assert_eq!(cfg.output_reserve, 20_000);
        assert_eq!(cfg.autocompact_buffer, 13_000);
        assert_eq!(cfg.emergency_buffer, 3_000);
        assert_eq!(cfg.max_failures, 3);
        assert_eq!(cfg.micro_keep_recent, 5);
        assert_eq!(cfg.micro_gap_seconds, 3600);
        assert!(cfg.enabled);
    }

    #[test]
    fn toml_empty_uses_all_defaults() {
        let cfg: CompactConfig = toml::from_str("").unwrap();
        let default = CompactConfig::default();
        assert_eq!(cfg.context_window, default.context_window);
        assert_eq!(cfg.output_reserve, default.output_reserve);
        assert_eq!(cfg.autocompact_buffer, default.autocompact_buffer);
        assert_eq!(cfg.emergency_buffer, default.emergency_buffer);
        assert_eq!(cfg.max_failures, default.max_failures);
        assert_eq!(cfg.micro_keep_recent, default.micro_keep_recent);
        assert_eq!(cfg.micro_gap_seconds, default.micro_gap_seconds);
        assert_eq!(cfg.enabled, default.enabled);
    }

    #[test]
    fn cache_diagnostics_defaults_to_false() {
        let cfg = CompactConfig::default();
        assert!(!cfg.cache_diagnostics);
    }

    #[test]
    fn toml_cache_diagnostics_override() {
        let toml_str = r#"
cache_diagnostics = true
"#;
        let cfg: CompactConfig = toml::from_str(toml_str).unwrap();
        assert!(cfg.cache_diagnostics);
    }

    #[test]
    fn default_compaction_is_safe() {
        let cfg = CompactConfig::default();
        assert_eq!(cfg.compaction, nomi_compact::CompactionLevel::Safe);
    }

    #[test]
    fn default_toon_is_false() {
        let cfg = CompactConfig::default();
        assert!(!cfg.toon);
    }

    #[test]
    fn toml_compaction_level_override() {
        let toml_str = r#"compaction = "full""#;
        let cfg: CompactConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.compaction, nomi_compact::CompactionLevel::Full);
    }

    #[test]
    fn toml_compaction_off() {
        let toml_str = r#"compaction = "off""#;
        let cfg: CompactConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.compaction, nomi_compact::CompactionLevel::Off);
    }

    #[test]
    fn toml_toon_enabled() {
        let toml_str = r#"toon = true"#;
        let cfg: CompactConfig = toml::from_str(toml_str).unwrap();
        assert!(cfg.toon);
    }

    #[test]
    fn json_serialization_roundtrip() {
        let cfg = CompactConfig {
            context_window: 100_000,
            output_reserve: 15_000,
            ..Default::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: CompactConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.context_window, 100_000);
        assert_eq!(back.output_reserve, 15_000);
        assert_eq!(back.autocompact_buffer, cfg.autocompact_buffer);
    }

    #[test]
    fn toml_autocompact_threshold_pct() {
        let toml_str = r#"autocompact_threshold_pct = 50"#;
        let cfg: CompactConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.autocompact_threshold_pct, Some(50));
    }

    #[test]
    fn toml_absent_threshold_pct_is_none() {
        let toml_str = r#"context_window = 128000"#;
        let cfg: CompactConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.autocompact_threshold_pct, None);
    }

    #[test]
    fn resolve_context_window_uses_override_else_default() {
        assert_eq!(resolve_context_window(Some(128_000), 200_000), 128_000);
        assert_eq!(resolve_context_window(None, 200_000), 200_000);
        assert_eq!(resolve_context_window(Some(0), 200_000), 200_000); // 0 treated as unset
    }

    #[test]
    fn context_budget_preserves_balanced_large_window_defaults() {
        let mut cfg = CompactConfig::default();

        assert_eq!(fit_context_budget(&mut cfg, 8192), 8192);
        assert_eq!(cfg.output_reserve, 20_000);
        assert_eq!(cfg.autocompact_buffer, 13_000);
        assert_eq!(cfg.emergency_buffer, 3_000);
    }

    #[test]
    fn context_budget_scales_defaults_for_local_window() {
        let mut cfg = CompactConfig {
            context_window: 65_536,
            ..Default::default()
        };

        assert_eq!(fit_context_budget(&mut cfg, 8192), 8192);
        assert_eq!(cfg.output_reserve, 8192);
        assert_eq!(cfg.autocompact_buffer, 8192);
        assert_eq!(cfg.emergency_buffer, 3_000);
    }

    #[test]
    fn context_budget_clamps_response_and_buffers_for_small_window() {
        let mut cfg = CompactConfig {
            context_window: 4096,
            ..Default::default()
        };

        assert_eq!(fit_context_budget(&mut cfg, 8192), 1024);
        assert_eq!(cfg.output_reserve, 1024);
        assert_eq!(cfg.autocompact_buffer, 512);
        assert_eq!(cfg.emergency_buffer, 256);
    }

    #[test]
    fn context_budget_keeps_custom_values_when_they_already_fit() {
        let mut cfg = CompactConfig {
            context_window: 32_000,
            output_reserve: 6000,
            autocompact_buffer: 4000,
            emergency_buffer: 1000,
            ..Default::default()
        };

        assert_eq!(fit_context_budget(&mut cfg, 4096), 4096);
        assert_eq!(cfg.output_reserve, 6000);
        assert_eq!(cfg.autocompact_buffer, 4000);
        assert_eq!(cfg.emergency_buffer, 1000);
    }
}
