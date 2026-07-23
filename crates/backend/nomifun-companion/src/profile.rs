//! Multi-companion configuration split: a per-companion profile (`companion/companions/{companion_id}/config.json`)
//! holding identity/persona/model/window settings, plus a shared config
//! (`companion/shared/config.json`) holding collection switches, the shared learn
//! loop and the default-companion pointer. Both reuse the shared config value
//! types from [`crate::config`] and the same atomic temp+rename write pattern.

use std::path::{Path, PathBuf};

use nomifun_common::{CompanionId, FigureId, ProviderWithModel, now_ms};
use serde::{Deserialize, Serialize};

use crate::config::{
    CollectConfig, DEFAULT_CHARACTER, PersonaConfig, deserialize_optional_model,
    serialize_optional_model,
};

/// Desktop-companion window settings for one companion. `character` lives
/// directly on [`CompanionProfileConfig`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
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
    /// roster characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_figure: Option<CustomFigureMeta>,
}

/// Head-and-shoulders crop over the figure image in image-fraction coordinates:
/// left `x` and width `w` are fractions of image WIDTH; top `y` and height `h`
/// are fractions of image HEIGHT. `h == 0` means a square box; the frontend
/// resolves it to `w * aspect`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HeadBox {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    /// Box height as a fraction of image height. `0` means a square crop,
    /// resolved frontend-side to `w * aspect`.
    pub h: f32,
}

/// Metadata for a user-supplied single-image figure (`character == "custom"`),
/// mirrored by `CustomFigureMeta` in the UI (`characters/types.ts`). The image
/// bytes themselves live next to the profile as
/// `{companions_dir}/{companion_id}/{FIGURE_FILE}` (see [`crate::figure`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CustomFigureMeta {
    /// width / height of the cutout image.
    pub aspect: f32,
    pub head_box: HeadBox,
    /// Desk size tier: "s" | "m" | "l".
    pub size_tier: String,
    /// Per-companion continuous figure-height override (logical px). When set it
    /// supersedes `size_tier` for THIS companion's desktop window (the 总览 size
    /// slider writes it); absent ⇒ fall back to the tier's height. The frontend
    /// clamps it to its [SIZE_MIN, SIZE_MAX] range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_px: Option<f32>,
    /// Library figure this companion draws from (a bare UUIDv7). When set, the image is
    /// served from the shared figure library (`/api/companion/figures/{figure_id}/image`),
    /// so one figure can back many companions. When absent, the companion-owned
    /// figure endpoint is used.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_figure_id"
    )]
    pub figure_id: Option<String>,
}

/// Per-companion profile persisted as `companion/companions/{companion_id}/config.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CompanionProfileConfig {
    /// Stable canonical bare UUIDv7 companion ID. [`Self::load`] returns `None` only
    /// when the profile file is absent; corrupt or non-canonical data is an
    /// error.
    #[serde(deserialize_with = "deserialize_companion_profile_id")]
    pub companion_id: String,
    /// Display-only short number (`#1`, `#2`, …) for companion lists. Monotonic
    /// within this machine, allocated by the registry from its private
    /// high-watermark state file (`companion/shared/companion_seq.json`) so a
    /// deleted companion's number is never reused.
    pub seq: u64,
    /// Display name chosen by the user.
    pub name: String,
    /// Which character renders in the companion window (mochi/ink/roux/pixel/bolt/boo).
    pub character: String,
    pub persona: PersonaConfig,
    /// Per-companion companion-chat model (the shared learn loop has its own).
    #[serde(
        deserialize_with = "deserialize_optional_model",
        serialize_with = "serialize_optional_model"
    )]
    pub model: Option<ProviderWithModel>,
    pub appearance: CompanionWindowConfig,
    /// Frozen reusable configuration applied to this companion. Identity,
    /// memories, evolved skills, window state and channel credentials remain
    /// companion-owned; this snapshot only supplies execution preferences.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_preset: Option<nomifun_api_types::ResolvedPresetSnapshot>,
    pub created_at: i64,
}

impl CompanionProfileConfig {
    /// Fresh profile with a generated companion ID. An empty `character` falls back to
    /// the default roster character.
    pub fn new(name: &str, character: &str, seq: u64) -> Self {
        assert!(seq > 0, "companion display sequence must be positive");
        let character = if character.is_empty() { DEFAULT_CHARACTER } else { character };
        Self {
            companion_id: CompanionId::new().into_string(),
            seq,
            name: name.to_owned(),
            character: character.to_owned(),
            persona: PersonaConfig::default(),
            model: None,
            appearance: CompanionWindowConfig::default(),
            applied_preset: None,
            created_at: now_ms(),
        }
    }

    pub fn config_path(dir: &Path) -> PathBuf {
        dir.join("config.json")
    }

    /// Load and validate `{dir}/config.json`. Only a missing file is absent;
    /// malformed or non-canonical durable data fails closed.
    pub fn load(dir: &Path) -> Result<Option<Self>, nomifun_common::AppError> {
        let path = Self::config_path(dir);
        let Some(profile): Option<Self> = crate::fsio::load_json_optional(&path)
            .map_err(|error| {
                nomifun_common::AppError::Internal(format!(
                    "load companion profile {}: {error}",
                    path.display()
                ))
            })?
        else {
            return Ok(None);
        };
        CompanionId::try_from(profile.companion_id.as_str()).map_err(|error| {
            nomifun_common::AppError::Internal(format!(
                "companion profile {} has invalid companion_id: {error}",
                path.display()
            ))
        })?;
        if profile.seq == 0 {
            return Err(nomifun_common::AppError::Internal(format!(
                "companion profile {} has invalid zero sequence",
                path.display()
            )));
        }
        validate_persisted_model(profile.model.as_ref()).map_err(|error| {
            nomifun_common::AppError::Internal(format!(
                "companion profile {} has invalid model: {error}",
                path.display()
            ))
        })?;
        validate_persisted_appearance(&profile.appearance).map_err(|error| {
            nomifun_common::AppError::Internal(format!(
                "companion profile {} has invalid custom figure: {error}",
                path.display()
            ))
        })?;
        if profile
            .applied_preset
            .as_ref()
            .is_some_and(|snapshot| snapshot.resolved_model.is_some())
        {
            return Err(nomifun_common::AppError::Internal(format!(
                "companion profile {} duplicates a Provider reference inside applied_preset",
                path.display()
            )));
        }
        Ok(Some(profile))
    }

    /// Atomically persist to `{dir}/config.json` (unique temp file + rename,
    /// so two concurrent saves can never rename each other's half-written
    /// temp into place).
    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        validate_persisted_model(self.model.as_ref()).map_err(std::io::Error::other)?;
        validate_persisted_appearance(&self.appearance).map_err(std::io::Error::other)?;
        if self
            .applied_preset
            .as_ref()
            .is_some_and(|snapshot| snapshot.resolved_model.is_some())
        {
            return Err(std::io::Error::other(
                "companion side store keeps Provider references only in the fixed model field",
            ));
        }
        crate::fsio::save_json_atomic(dir, "config.json", self)
    }
}

/// Shared learn-loop settings: one schedule + one model distilling events for
/// every companion (the per-companion `model` only drives companion chat).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SharedLearnConfig {
    pub enabled: bool,
    /// Minutes between learning runs.
    pub interval_minutes: u32,
    #[serde(
        deserialize_with = "deserialize_optional_model",
        serialize_with = "serialize_optional_model"
    )]
    pub model: Option<ProviderWithModel>,
}

impl Default for SharedLearnConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_minutes: 60,
            model: None,
        }
    }
}

/// Shared skill-evolution settings (design §6): the background EvolutionEngine
/// mines repeated multi-step tool sequences from real work and drafts them into
/// reviewable skills. Independent schedule/model from the lightweight learner.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SharedEvolveConfig {
    pub enabled: bool,
    /// Minutes between evolution runs.
    pub interval_minutes: u32,
    #[serde(
        deserialize_with = "deserialize_optional_model",
        serialize_with = "serialize_optional_model"
    )]
    pub model: Option<ProviderWithModel>,
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
            model: None,
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct SharedCompanionConfig {
    pub collect: CollectConfig,
    pub learn: SharedLearnConfig,
    pub evolve: SharedEvolveConfig,
    pub archive: SharedArchiveConfig,
    /// 智能协作（默认 OFF）：开启后，本地伙伴会话可通过
    /// `nomi_delegate` 把复杂工作交给多个 Agent，并在当前会话汇总结果。
    /// 能力由桌面网关的 Agent Execution 域提供，远程 IM 会话不注入。
    pub smart_collaboration: bool,
    /// Which companion new/unattributed activity defaults to.
    #[serde(deserialize_with = "deserialize_optional_companion_id")]
    pub default_companion_id: Option<String>,
    /// Opt-in (default None = off): when set to a directory path, companion
    /// `save` memories are ALSO mirrored into the nomi agent's file-memory there
    /// (the §3.4 "消两库割裂" bridge), so the agent recalls companion-learned
    /// facts. Enabling it intentionally surfaces companion memories in agent
    /// sessions — that is the feature; default-off keeps the libraries separate.
    pub bridge_to_memory_dir: Option<String>,
}

fn deserialize_optional_companion_id<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    value
        .map(|raw| {
            CompanionId::try_from(raw.as_str())
                .map(CompanionId::into_string)
                .map_err(serde::de::Error::custom)
        })
        .transpose()
}

fn deserialize_optional_figure_id<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    value
        .map(|raw| {
            FigureId::try_from(raw.as_str())
                .map(FigureId::into_string)
                .map_err(serde::de::Error::custom)
        })
        .transpose()
}

fn deserialize_companion_profile_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    CompanionId::try_from(raw.as_str())
        .map(CompanionId::into_string)
        .map_err(serde::de::Error::custom)
}

impl SharedCompanionConfig {
    pub fn config_path(dir: &Path) -> PathBuf {
        dir.join("config.json")
    }

    /// Load from `{dir}/config.json` (dir is the shared dir). Only a missing
    /// file uses defaults; unreadable or malformed data fails closed.
    pub fn load(dir: &Path) -> Result<Self, nomifun_common::AppError> {
        let path = Self::config_path(dir);
        crate::fsio::load_json_missing_or_default(&path).map_err(|error| {
            nomifun_common::AppError::Internal(format!(
                "load shared companion config {}: {error}",
                path.display()
            ))
        })
    }

    /// Atomically persist to `{dir}/config.json` (unique temp file + rename).
    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        validate_persisted_model(self.learn.model.as_ref()).map_err(std::io::Error::other)?;
        validate_persisted_model(self.evolve.model.as_ref()).map_err(std::io::Error::other)?;
        crate::fsio::save_json_atomic(dir, "config.json", self)
    }
}

fn validate_persisted_model(model: Option<&ProviderWithModel>) -> Result<(), String> {
    let Some(model) = model else {
        return Ok(());
    };
    model.validate()?;
    if model.use_model.is_some() {
        return Err(
            "companion side-store model must use exactly {provider_id, model}".into(),
        );
    }
    Ok(())
}

fn validate_persisted_appearance(appearance: &CompanionWindowConfig) -> Result<(), String> {
    let Some(figure) = appearance.custom_figure.as_ref() else {
        return Ok(());
    };
    if !figure.aspect.is_finite() || figure.aspect <= 0.0 {
        return Err("custom figure aspect must be finite and greater than zero".into());
    }
    let values = [
        figure.head_box.x,
        figure.head_box.y,
        figure.head_box.w,
        figure.head_box.h,
    ];
    if values.iter().any(|value| !value.is_finite()) {
        return Err("custom figure head_box values must be finite".into());
    }
    if figure.head_box.x < 0.0
        || figure.head_box.y < 0.0
        || figure.head_box.w <= 0.0
        || figure.head_box.h < 0.0
        || figure.head_box.x + figure.head_box.w > 1.0
        || figure.head_box.y + figure.head_box.h > 1.0
    {
        return Err("custom figure head_box must fit inside normalized image bounds".into());
    }
    if !matches!(figure.size_tier.as_str(), "s" | "m" | "l") {
        return Err("custom figure size_tier must be one of s, m, l".into());
    }
    if figure
        .size_px
        .is_some_and(|size| !size.is_finite() || size <= 0.0)
    {
        return Err("custom figure size_px must be finite and greater than zero".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_roundtrip_and_default_on_missing() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = CompanionProfileConfig::load(dir.path()).unwrap();
        assert_eq!(loaded, None);

        let mut profile = CompanionProfileConfig::new("毛球", "ink", 1);
        profile.model = Some(ProviderWithModel {
            provider_id: nomifun_common::ProviderId::new().into_string(),
            model: "claude-fable-5".into(),
            use_model: None,
        });
        profile.appearance.companion_enabled = true;
        profile.save(dir.path()).unwrap();

        let again = CompanionProfileConfig::load(dir.path()).unwrap().unwrap();
        assert_eq!(again, profile);
        assert!(CompanionId::parse(&again.companion_id).is_ok());
        assert!(again.created_at > 0);
    }

    #[test]
    fn profile_new_falls_back_to_default_character() {
        let p = CompanionProfileConfig::new("无名", "", 1);
        assert_eq!(p.character, "mochi");
        let q = CompanionProfileConfig::new("有名", "boo", 1);
        assert_eq!(q.character, "boo");
    }

    #[test]
    fn corrupt_profile_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(CompanionProfileConfig::config_path(dir.path()), "{not json").unwrap();
        assert!(CompanionProfileConfig::load(dir.path()).is_err());
    }

    #[test]
    fn custom_figure_roundtrips_and_omits_absent_fields() {
        let dir = tempfile::tempdir().unwrap();

        // A profile with no custom_figure key deserializes to None and
        // serializes without the key (skip_serializing_if).
        let mut profile = CompanionProfileConfig::new("自定", "custom", 1);
        assert_eq!(profile.appearance.custom_figure, None);
        profile.save(dir.path()).unwrap();
        let raw = std::fs::read_to_string(CompanionProfileConfig::config_path(dir.path())).unwrap();
        assert!(!raw.contains("custom_figure"));

        let figure_id = FigureId::new().into_string();
        profile.appearance.custom_figure = Some(CustomFigureMeta {
            aspect: 0.9444,
            head_box: HeadBox { x: 0.321, y: 0.0, w: 0.281, h: 0.3 },
            size_tier: "m".into(),
            size_px: None,
            figure_id: None,
        });
        profile.save(dir.path()).unwrap();
        // A None figure_id / size_px must not appear in the JSON.
        let raw_none = std::fs::read_to_string(CompanionProfileConfig::config_path(dir.path())).unwrap();
        assert!(!raw_none.contains("figure_id"));
        assert!(!raw_none.contains("size_px"));
        let again = CompanionProfileConfig::load(dir.path()).unwrap().unwrap();
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
            figure_id: Some(figure_id.clone()),
        });
        profile.save(dir.path()).unwrap();
        let linked = CompanionProfileConfig::load(dir.path()).unwrap().unwrap();
        let linked_cf = linked.appearance.custom_figure.unwrap();
        assert_eq!(linked_cf.figure_id.as_deref(), Some(figure_id.as_str()));
        assert_eq!(linked_cf.size_px, Some(333.0));
    }

    #[test]
    fn shared_roundtrip_and_default_on_missing() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = SharedCompanionConfig::load(dir.path()).unwrap();
        assert_eq!(loaded, SharedCompanionConfig::default());
        assert_eq!(loaded.learn.interval_minutes, 60);
        assert!(!loaded.learn.enabled);

        let mut cfg = SharedCompanionConfig::default();
        cfg.collect.chat_user_messages = true;
        cfg.learn.enabled = true;
        cfg.learn.model = Some(ProviderWithModel {
            provider_id: nomifun_common::ProviderId::new().into_string(),
            model: "claude-fable-5".into(),
            use_model: None,
        });
        cfg.default_companion_id = Some(nomifun_common::CompanionId::new().into_string());
        cfg.save(dir.path()).unwrap();

        let again = SharedCompanionConfig::load(dir.path()).unwrap();
        assert_eq!(again, cfg);
        assert!(again.learn.model.is_some());
    }

    #[test]
    fn shared_config_rejects_retired_smart_orchestration_key() {
        let result = serde_json::from_value::<SharedCompanionConfig>(serde_json::json!({
            "smart_orchestration": true
        }));
        assert!(result.is_err());
    }

    #[test]
    fn shared_config_rejects_empty_or_malformed_default_companion_id() {
        for default_companion_id in ["", "not-a-companion-id"] {
            let result = serde_json::from_value::<SharedCompanionConfig>(serde_json::json!({
                "default_companion_id": default_companion_id
            }));
            assert!(result.is_err());
        }
    }

    #[test]
    fn profile_and_shared_models_persist_exact_provider_id_and_model_shape() {
        let canonical_provider = nomifun_common::ProviderId::new().into_string();
        let model = ProviderWithModel {
            provider_id: canonical_provider.clone(),
            model: "chat".into(),
            use_model: None,
        };

        let mut profile = CompanionProfileConfig::new("严格模型", "ink", 1);
        profile.model = Some(model.clone());
        let profile_json = serde_json::to_value(&profile).unwrap();
        assert_eq!(
            profile_json["model"],
            serde_json::json!({
                "provider_id": canonical_provider.clone(),
                "model": "chat"
            })
        );

        let mut shared = SharedCompanionConfig::default();
        shared.learn.model = Some(model.clone());
        shared.evolve.model = Some(model);
        let shared_json = serde_json::to_value(shared).unwrap();
        for persisted in [&shared_json["learn"]["model"], &shared_json["evolve"]["model"]] {
            assert_eq!(
                persisted
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(String::as_str)
                    .collect::<std::collections::BTreeSet<_>>(),
                ["model", "provider_id"].into_iter().collect()
            );
        }

        for invalid in [
            serde_json::json!({"provider_id": "", "model": "chat"}),
            serde_json::json!({"provider_id": "not-a-provider-id", "model": "chat"}),
            serde_json::json!({"provider_id": canonical_provider, "model": " "}),
            serde_json::json!({
                "provider_id": canonical_provider,
                "model": "chat",
                "use_model": "chat"
            }),
            serde_json::json!({
                "provider_id": canonical_provider,
                "model": "chat",
                "backend": "openai"
            }),
        ] {
            let result = serde_json::from_value::<SharedCompanionConfig>(serde_json::json!({
                "learn": {"model": invalid}
            }));
            assert!(
                result.is_err(),
                "non-v3 companion side-store model must be rejected"
            );
        }
    }

    #[test]
    fn corrupt_shared_config_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(SharedCompanionConfig::config_path(dir.path()), "[oops").unwrap();
        assert!(SharedCompanionConfig::load(dir.path()).is_err());
    }
}
