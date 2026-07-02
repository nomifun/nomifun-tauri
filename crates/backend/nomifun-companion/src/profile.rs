//! Multi-companion configuration split: a per-companion profile (`companion/companions/{id}/config.json`)
//! holding identity/persona/model/window settings, plus a shared config
//! (`companion/shared/config.json`) holding collection switches, the shared learn
//! loop and the default-companion pointer. Both reuse the legacy building blocks
//! from [`crate::config`] and the same atomic temp+rename write pattern.

use std::path::{Path, PathBuf};

use nomifun_common::{generate_prefixed_id, now_ms};
use serde::{Deserialize, Serialize};

use crate::config::{CollectConfig, DEFAULT_CHARACTER, ModelConfig, PersonaConfig};

/// Desktop-companion window settings for one companion — the legacy `AppearanceConfig`
/// minus `character`, which now lives directly on [`CompanionProfileConfig`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CompanionWindowConfig {
    /// Whether this companion's desktop window should be visible.
    pub companion_enabled: bool,
    /// Saved companion window position (physical px), if the user dragged it.
    pub companion_x: Option<i32>,
    pub companion_y: Option<i32>,
    /// Quiet hours "HH:mm" — within this window the companion only accrues badges
    /// and never pops bubbles. Empty strings disable quiet hours.
    pub quiet_start: String,
    pub quiet_end: String,
    /// DIY single-image figure metadata (character == "custom"). Absent for
    /// roster characters — and omitted from JSON so pre-DIY configs round-trip
    /// byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_figure: Option<CustomFigureMeta>,
}

/// Head-and-shoulders crop over the figure image in image-fraction coordinates:
/// left `x` and width `w` are fractions of image WIDTH; top `y` and height `h`
/// are fractions of image HEIGHT. `h == 0` marks a legacy square box (created
/// before free-rectangle framing) — the frontend resolves it to `w * aspect`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HeadBox {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    /// Box height as a fraction of image height. `0` ⇒ legacy square (resolved
    /// frontend-side to `w * aspect`); `#[serde(default)]` so old configs load.
    #[serde(default)]
    pub h: f32,
}

/// Metadata for a user-supplied single-image figure (`character == "custom"`),
/// mirrored by `CustomFigureMeta` in the UI (`characters/types.ts`). The image
/// bytes themselves live next to the profile as
/// `{companions_dir}/{companion_id}/{FIGURE_FILE}` (see [`crate::figure`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CustomFigureMeta {
    /// width / height of the cutout image.
    pub aspect: f32,
    pub head_box: HeadBox,
    /// Desk size tier: "s" | "m" | "l".
    pub size_tier: String,
    /// Per-companion continuous figure-height override (logical px). When set it
    /// supersedes `size_tier` for THIS companion's desktop window (the 总览 size
    /// slider writes it); absent ⇒ fall back to the tier's height. The frontend
    /// clamps it to its [SIZE_MIN, SIZE_MAX] range. `skip_serializing_if` keeps
    /// pre-slider configs byte-identical (no migration), like `figure_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_px: Option<f32>,
    /// Library figure this companion draws from (`figure_…`). When set, the image is
    /// served from the shared figure library (`/api/companion/figures/{id}/image`),
    /// so one figure can back many companions. Absent for legacy per-companion figures
    /// installed before the library (those still serve from
    /// `/api/companion/companions/{id}/figure`), keeping old configs byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub figure_id: Option<String>,
}

/// Per-companion profile persisted as `companion/companions/{id}/config.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CompanionProfileConfig {
    /// Stable id (`companion_…`). An empty id after `load` means the file was
    /// missing/corrupt — callers must discard such profiles.
    pub id: String,
    /// Display-only short number (`#1`, `#2`, …) for companion lists. Monotonic
    /// within this machine — allocated by the registry from its private
    /// high-watermark state file (`companion/shared/companion_seq.json`) so a deleted
    /// companion's number is never reused. `None` only for profiles written before
    /// the seq rollout; the boot scan backfills those.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    /// Display name chosen by the user.
    pub name: String,
    /// Which character renders in the companion window (mochi/ink/roux/pixel/bolt/boo).
    pub character: String,
    pub persona: PersonaConfig,
    /// Per-companion companion-chat model (the shared learn loop has its own).
    pub model: ModelConfig,
    pub appearance: CompanionWindowConfig,
    pub created_at: i64,
    /// 对外服务信任档。`PublicService` 把这个伙伴变成"外呼员工"：它承载的每一条会话
    /// （桌面气泡 / 聊天 Tab / IM 渠道）都被 nomi 工厂硬性收窄到安全白名单（无 shell /
    /// 文件 / computer / browser / 网关），且工厂 LIVE 读取本字段——翻档下一轮即生效。
    /// 缺省 `Private` = 今日全能力伙伴，旧 config.json 零迁移 round-trip。
    #[serde(default)]
    pub exposure: nomifun_api_types::ExposureMode,
}

impl CompanionProfileConfig {
    /// Fresh profile with a generated id. An empty `character` falls back to
    /// the default roster character.
    pub fn new(name: &str, character: &str) -> Self {
        let character = if character.is_empty() { DEFAULT_CHARACTER } else { character };
        Self {
            id: generate_prefixed_id("companion"),
            // Allocated by the registry under its lock (never here, where no
            // watermark is in scope).
            seq: None,
            name: name.to_owned(),
            character: character.to_owned(),
            persona: PersonaConfig::default(),
            model: ModelConfig::default(),
            appearance: CompanionWindowConfig::default(),
            created_at: now_ms(),
            exposure: nomifun_api_types::ExposureMode::Private,
        }
    }

    pub fn config_path(dir: &Path) -> PathBuf {
        dir.join("config.json")
    }

    /// Load from `{dir}/config.json`, falling back to defaults when the file
    /// is missing or unreadable (a corrupt profile must never brick boot).
    /// The default has an empty `id` — callers detect and discard it.
    pub fn load(dir: &Path) -> Self {
        crate::fsio::load_json_or_default(&Self::config_path(dir))
    }

    /// Atomically persist to `{dir}/config.json` (unique temp file + rename,
    /// so two concurrent saves can never rename each other's half-written
    /// temp into place).
    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        crate::fsio::save_json_atomic(dir, "config.json", self)
    }
}

/// Shared learn-loop settings: one schedule + one model distilling events for
/// every companion (the per-companion `model` only drives companion chat).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SharedLearnConfig {
    pub enabled: bool,
    /// Minutes between learning runs.
    pub interval_minutes: u32,
    pub model: ModelConfig,
}

impl Default for SharedLearnConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_minutes: 60,
            model: ModelConfig::default(),
        }
    }
}

/// Shared skill-evolution settings (design §6): the background EvolutionEngine
/// mines repeated multi-step tool sequences from real work and drafts them into
/// reviewable skills. Independent schedule/model from the lightweight learner.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SharedEvolveConfig {
    pub enabled: bool,
    /// Minutes between evolution runs.
    pub interval_minutes: u32,
    pub model: ModelConfig,
    /// A pattern must occur at least this many times total to be drafted.
    pub min_pattern_count: i64,
    /// A pattern must appear across at least this many distinct sessions.
    pub min_distinct_sessions: usize,
    /// Also reflect on single complex work sessions (not just repeated patterns) — design §5.5 任务后反思.
    pub reflect_enabled: bool,
    /// Auto-activate a drafted skill (skip human review) when confidence ≥ `auto_threshold`.
    /// Default off (gated): the user opts into high-confidence auto-activation.
    pub auto_activate: bool,
    /// Confidence cutoff for `auto_activate` (repetition-derived; single-session reflections stay below it).
    pub auto_threshold: f64,
    /// Skill strength half-life in days (decay clock = time since last use). Used skills reinforce.
    pub skill_half_life_days: f64,
    /// Below this strength a mined skill is auto-archived (restorable; manual skills never decay).
    pub skill_archive_threshold: f64,
}

impl Default for SharedEvolveConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_minutes: 30,
            model: ModelConfig::default(),
            min_pattern_count: 3,
            min_distinct_sessions: 2,
            reflect_enabled: true,
            auto_activate: false,
            auto_threshold: 0.85,
            skill_half_life_days: 45.0,
            skill_archive_threshold: 0.05,
        }
    }
}

/// Session-window archiving settings (伙伴会话窗口归档): when a companion's chat
/// window goes idle for `idle_minutes`, compress it into a day-partitioned
/// digest, then reset the live engine context so the next window starts small.
/// Default OFF (opt-in), mirroring the learn loop — these background LLM loops
/// cost tokens and (here) reset live context, so the user opts in explicitly.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SharedArchiveConfig {
    /// Master switch. Off = the archiver is a complete no-op (companion behaves
    /// exactly as before this feature).
    pub enabled: bool,
    /// Close & archive a window after this many minutes with no activity.
    pub idle_minutes: u32,
    /// Skip summarizing (roll boundary only, no digest, no reset) windows whose
    /// total content is shorter than this many chars — avoids burning tokens on
    /// trivial "hi/bye" sessions.
    pub min_chars: usize,
    /// How many recent day-digests to inject into a new window's system prompt.
    pub inject_recent_days: u32,
}

impl Default for SharedArchiveConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            idle_minutes: 30,
            min_chars: 60,
            inject_recent_days: 3,
        }
    }
}

/// Cross-companion shared configuration persisted as `companion/shared/config.json`.
/// Deliberately user-writable wholesale (full-object `PUT /api/companion/config`),
/// so nothing registry-owned (e.g. the companion-seq watermark, which lives in
/// `companion/shared/companion_seq.json`) may be carried here.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SharedCompanionConfig {
    pub collect: CollectConfig,
    pub learn: SharedLearnConfig,
    #[serde(default)]
    pub evolve: SharedEvolveConfig,
    #[serde(default)]
    pub archive: SharedArchiveConfig,
    /// 智能编排（默认 OFF, opt-in）：开启后，本地伙伴会话获得"调度官"能力提示——
    /// 遇到复杂/多步大任务时用 `nomi_run_create` 把活拆给隔离子 agent 并行处理，
    /// 伙伴只负责调度与汇总，保持自己的对话上下文清爽（与会话归档协同：主线程只留
    /// 总结、更易归档）。工具本身随桌面网关（desktopGateway）提供；远程 IM 会话不注入
    /// （caps_orchestrator 对 Remote 硬拒）。
    #[serde(default)]
    pub smart_orchestration: bool,
    /// Which companion new/unattributed activity defaults to.
    pub default_companion_id: String,
    /// Opt-in (default None = off): when set to a directory path, companion
    /// `save` memories are ALSO mirrored into the nomi agent's file-memory there
    /// (the §3.4 "消两库割裂" bridge), so the agent recalls companion-learned
    /// facts. Enabling it intentionally surfaces companion memories in agent
    /// sessions — that is the feature; default-off keeps the libraries separate.
    #[serde(default)]
    pub bridge_to_memory_dir: Option<String>,
}

impl SharedCompanionConfig {
    pub fn config_path(dir: &Path) -> PathBuf {
        dir.join("config.json")
    }

    /// Load from `{dir}/config.json` (dir is the shared dir), falling back to
    /// defaults when the file is missing or unreadable.
    pub fn load(dir: &Path) -> Self {
        crate::fsio::load_json_or_default(&Self::config_path(dir))
    }

    /// Atomically persist to `{dir}/config.json` (unique temp file + rename).
    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        crate::fsio::save_json_atomic(dir, "config.json", self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_roundtrip_and_default_on_missing() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = CompanionProfileConfig::load(dir.path());
        assert_eq!(loaded, CompanionProfileConfig::default());
        assert!(loaded.id.is_empty()); // caller-discard sentinel

        let mut profile = CompanionProfileConfig::new("毛球", "ink");
        profile.model.provider_id = "prov_x".into();
        profile.model.model = "claude-fable-5".into();
        profile.appearance.companion_enabled = true;
        profile.save(dir.path()).unwrap();

        let again = CompanionProfileConfig::load(dir.path());
        assert_eq!(again, profile);
        assert!(again.id.starts_with("companion_"));
        assert!(again.created_at > 0);
    }

    #[test]
    fn profile_new_falls_back_to_default_character() {
        let p = CompanionProfileConfig::new("无名", "");
        assert_eq!(p.character, "mochi");
        let q = CompanionProfileConfig::new("有名", "boo");
        assert_eq!(q.character, "boo");
    }

    #[test]
    fn corrupt_profile_falls_back_to_default() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(CompanionProfileConfig::config_path(dir.path()), "{not json").unwrap();
        let loaded = CompanionProfileConfig::load(dir.path());
        assert_eq!(loaded, CompanionProfileConfig::default());
        assert!(loaded.id.is_empty());
    }

    #[test]
    fn custom_figure_roundtrips_and_stays_absent_for_old_configs() {
        let dir = tempfile::tempdir().unwrap();

        // A pre-DIY profile (no custom_figure key) deserializes to None and
        // serializes without the key (skip_serializing_if).
        let mut profile = CompanionProfileConfig::new("自定", "custom");
        assert_eq!(profile.appearance.custom_figure, None);
        profile.save(dir.path()).unwrap();
        let raw = std::fs::read_to_string(CompanionProfileConfig::config_path(dir.path())).unwrap();
        assert!(!raw.contains("custom_figure"));

        profile.appearance.custom_figure = Some(CustomFigureMeta {
            aspect: 0.9444,
            head_box: HeadBox { x: 0.321, y: 0.0, w: 0.281, h: 0.3 },
            size_tier: "m".into(),
            size_px: None,
            figure_id: None,
        });
        profile.save(dir.path()).unwrap();
        // A None figure_id / size_px must not appear in the JSON (old configs stay byte-clean).
        let raw_none = std::fs::read_to_string(CompanionProfileConfig::config_path(dir.path())).unwrap();
        assert!(!raw_none.contains("figure_id"));
        assert!(!raw_none.contains("size_px"));
        let again = CompanionProfileConfig::load(dir.path());
        assert_eq!(again, profile);
        let meta = again.appearance.custom_figure.unwrap();
        assert_eq!(meta.size_tier, "m");
        assert_eq!(meta.size_px, None);
        assert!((meta.head_box.w - 0.281).abs() < f32::EPSILON);

        // A library-linked figure_id + a per-companion size_px override round-trip.
        profile.appearance.custom_figure = Some(CustomFigureMeta {
            aspect: 0.9444,
            head_box: HeadBox { x: 0.321, y: 0.0, w: 0.281, h: 0.3 },
            size_tier: "m".into(),
            size_px: Some(333.0),
            figure_id: Some("figure_abc".into()),
        });
        profile.save(dir.path()).unwrap();
        let linked = CompanionProfileConfig::load(dir.path());
        let linked_cf = linked.appearance.custom_figure.unwrap();
        assert_eq!(linked_cf.figure_id.as_deref(), Some("figure_abc"));
        assert_eq!(linked_cf.size_px, Some(333.0));
    }

    #[test]
    fn shared_roundtrip_and_default_on_missing() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = SharedCompanionConfig::load(dir.path());
        assert_eq!(loaded, SharedCompanionConfig::default());
        assert_eq!(loaded.learn.interval_minutes, 60);
        assert!(!loaded.learn.enabled);

        let mut cfg = SharedCompanionConfig::default();
        cfg.collect.chat_user_messages = true;
        cfg.learn.enabled = true;
        cfg.learn.model.provider_id = "prov_y".into();
        cfg.learn.model.model = "claude-fable-5".into();
        cfg.default_companion_id = "companion_abc".into();
        cfg.save(dir.path()).unwrap();

        let again = SharedCompanionConfig::load(dir.path());
        assert_eq!(again, cfg);
        assert!(again.learn.model.is_configured());
    }

    #[test]
    fn corrupt_shared_config_falls_back_to_default() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(SharedCompanionConfig::config_path(dir.path()), "[oops").unwrap();
        assert_eq!(SharedCompanionConfig::load(dir.path()), SharedCompanionConfig::default());
    }
}
