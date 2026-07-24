//! `CompanionRegistry` — the in-memory roster of companion profiles, mirrored to disk as
//! one `companion/companions/{companion_id}/config.json` per companion. Boot does a synchronous [`scan`]
//! of the companions dir; afterwards every mutation (create/patch/remove) saves the
//! profile first and only then updates the map under the write lock, so the
//! map never claims a companion whose file failed to persist.
//!
//! The registry also owns companion short numbers ([`CompanionProfileConfig::seq`]) and
//! their high-watermark, persisted in a registry-private state file
//! ([`SEQ_STATE_FILE`] under the shared dir) that no API config write path
//! can reach: [`create`] allocates the next number from the watermark and
//! New v3 profiles receive their short number at creation inside the same
//! critical section that mutates the roster, so concurrent creates cannot mint
//! the same number.
//!
//! [`scan`]: CompanionRegistry::scan
//! [`create`]: CompanionRegistry::create

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use nomifun_common::{
    AppError, CompanionId, ProviderWithModel, SharedProviderLifecycleBarrier,
};
use nomifun_db::IProviderRepository;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::profile::CompanionProfileConfig;

/// Maximum companion display-name length, counted in chars (not bytes) so CJK
/// names get the same budget as ASCII ones.
const MAX_NAME_CHARS: usize = 40;

/// File under the shared dir holding the registry-private seq watermark.
pub(crate) const SEQ_STATE_FILE: &str = "companion_seq.json";

/// Registry-private high-watermark for companion short numbers: the largest seq
/// ever allocated on this machine, persisted as `{shared_dir}/companion_seq.json`
/// (`{"last_companion_seq": N}`). It deliberately does NOT live on
/// [`crate::profile::SharedCompanionConfig`]: that object is user-writable
/// wholesale (full-object `PUT /api/companion/config`, future import paths, …), so
/// keeping the watermark there would make "never reuse a deleted companion's
/// number" depend on every present and future config write path remembering
/// to clamp it. A missing file starts at 0; an existing but malformed state
/// file fails startup rather than silently reusing display numbers.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct CompanionSeqState {
    pub(crate) last_companion_seq: u64,
}

impl CompanionSeqState {
    /// Load from `{shared_dir}/companion_seq.json`. Only absence means the
    /// initial zero state; unreadable or malformed data is an error.
    pub(crate) fn load(shared_dir: &Path) -> Result<Self, AppError> {
        let path = shared_dir.join(SEQ_STATE_FILE);
        crate::fsio::load_json_missing_or_default(&path).map_err(|error| {
            AppError::Internal(format!(
                "load companion sequence watermark {}: {error}",
                path.display()
            ))
        })
    }

    /// Atomically persist to `{shared_dir}/companion_seq.json`.
    pub(crate) fn save(&self, shared_dir: &Path) -> std::io::Result<()> {
        crate::fsio::save_json_atomic(shared_dir, SEQ_STATE_FILE, self)
    }
}

/// RFC 7396 JSON merge patch: objects merge recursively, `null` deletes,
/// everything else replaces.
pub(crate) fn json_merge_patch(target: &mut serde_json::Value, patch: &serde_json::Value) {
    if let (Some(target_map), Some(patch_map)) = (target.as_object_mut(), patch.as_object()) {
        for (key, value) in patch_map {
            if value.is_null() {
                target_map.remove(key);
            } else if value.is_object() && target_map.get(key).is_some_and(|t| t.is_object()) {
                json_merge_patch(target_map.get_mut(key).unwrap(), value);
            } else {
                target_map.insert(key.clone(), value.clone());
            }
        }
    } else {
        *target = patch.clone();
    }
}

/// Trimmed, non-empty, at most [`MAX_NAME_CHARS`] chars — or `BadRequest`.
fn validate_name(name: &str) -> Result<String, AppError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(AppError::BadRequest("companion name must not be empty".into()));
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(AppError::BadRequest(format!(
            "companion name must be at most {MAX_NAME_CHARS} characters"
        )));
    }
    Ok(name.to_owned())
}

/// The largest seq carried by any companion in the map (0 when none carries one).
/// Lets allocation self-heal a stale/clobbered watermark while the
/// highest-numbered companion is still alive.
fn max_live_seq(companions: &HashMap<String, CompanionProfileConfig>) -> u64 {
    companions.values().map(|p| p.seq).max().unwrap_or(0)
}

pub struct CompanionRegistry {
    companions_dir: PathBuf,
    /// Shared multi-companion home (`{data_dir}/companion/shared`) — where the seq
    /// watermark state file ([`SEQ_STATE_FILE`]) is persisted.
    shared_dir: PathBuf,
    /// In-memory seq watermark, mirrored to disk via [`CompanionSeqState`].
    /// Registry-owned and only ever advanced.
    ///
    /// Lock order: this lock is always acquired BEFORE the roster map below.
    watermark: RwLock<u64>,
    inner: RwLock<HashMap<String, CompanionProfileConfig>>,
    provider_repo: Option<std::sync::Arc<dyn IProviderRepository>>,
    provider_lifecycle: Option<SharedProviderLifecycleBarrier>,
}

impl CompanionRegistry {
    /// Synchronous fail-closed boot-time scan. Every directory below
    /// `companions_dir` is a durable entity directory and therefore must carry a
    /// valid `config.json` whose canonical UUIDv7 matches the directory name.
    pub fn scan(
        companions_dir: PathBuf,
        shared_dir: PathBuf,
    ) -> Result<Self, AppError> {
        Self::scan_with_provider_lifecycle(companions_dir, shared_dir, None, None)
    }

    pub fn scan_with_provider_lifecycle(
        companions_dir: PathBuf,
        shared_dir: PathBuf,
        provider_repo: Option<std::sync::Arc<dyn IProviderRepository>>,
        provider_lifecycle: Option<SharedProviderLifecycleBarrier>,
    ) -> Result<Self, AppError> {
        let mut companions = HashMap::new();
        match std::fs::read_dir(&companions_dir) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry.map_err(|error| {
                        AppError::Internal(format!(
                            "scan companion directory {}: {error}",
                            companions_dir.display()
                        ))
                    })?;
                    let path = entry.path();
                    let file_type = entry.file_type().map_err(|error| {
                        AppError::Internal(format!(
                            "inspect companion entry {}: {error}",
                            path.display()
                        ))
                    })?;
                    if file_type.is_symlink() || !file_type.is_dir() {
                        return Err(AppError::Internal(format!(
                            "companion roster contains unexpected entry {}",
                            path.display()
                        )));
                    }
                    let dir_name = entry.file_name().to_string_lossy().into_owned();
                    let mut local_figure_path = None;
                    let contents = std::fs::read_dir(&path).map_err(|error| {
                        AppError::Internal(format!(
                            "scan companion directory {}: {error}",
                            path.display()
                        ))
                    })?;
                    for child in contents {
                        let child = child.map_err(|error| {
                            AppError::Internal(format!(
                                "scan companion directory {}: {error}",
                                path.display()
                            ))
                        })?;
                        let child_path = child.path();
                        let child_type = child.file_type().map_err(|error| {
                            AppError::Internal(format!(
                                "inspect companion entry {}: {error}",
                                child_path.display()
                            ))
                        })?;
                        let child_name = child.file_name().to_string_lossy().into_owned();
                        if child_type.is_symlink()
                            || (child_name != "config.json"
                                && child_name != crate::figure::FIGURE_FILE)
                        {
                            return Err(AppError::Internal(format!(
                                "companion directory contains unexpected entry {}",
                                child_path.display()
                            )));
                        }
                        if !child_type.is_file() {
                            return Err(AppError::Internal(format!(
                                "companion artifact {} is not a regular file",
                                child_path.display()
                            )));
                        }
                        if child_name == crate::figure::FIGURE_FILE {
                            local_figure_path = Some(child_path);
                        }
                    }
                    let profile = match CompanionProfileConfig::load(&path) {
                        Ok(Some(profile)) => profile,
                        Ok(None) => {
                            return Err(AppError::Internal(format!(
                                "companion directory {} is missing config.json",
                                path.display()
                            )));
                        }
                        Err(error) => {
                            return Err(AppError::Internal(format!(
                                "load companion profile {}: {error}",
                                path.display()
                            )));
                        }
                    };
                    if profile.companion_id != dir_name {
                        return Err(AppError::Internal(format!(
                            "companion profile {} has companion_id '{}' that does not match its directory",
                            path.display(),
                            profile.companion_id
                        )));
                    }
                    let requires_local_figure = profile
                        .appearance
                        .custom_figure
                        .as_ref()
                        .is_some_and(|figure| figure.figure_id.is_none());
                    match (requires_local_figure, local_figure_path.as_deref()) {
                        (true, None) => {
                            return Err(AppError::Internal(format!(
                                "companion profile '{}' references a missing local figure",
                                profile.companion_id
                            )));
                        }
                        (false, Some(figure_path)) => {
                            return Err(AppError::Internal(format!(
                                "companion directory contains orphaned local figure {}",
                                figure_path.display()
                            )));
                        }
                        (true, Some(figure_path)) => {
                            let bytes = std::fs::read(figure_path).map_err(|error| {
                                AppError::Internal(format!(
                                    "read companion figure {}: {error}",
                                    figure_path.display()
                                ))
                            })?;
                            crate::figure::validate_figure_bytes(&bytes).map_err(|error| {
                                AppError::Internal(format!(
                                    "companion profile '{}' has an invalid local figure: {error}",
                                    profile.companion_id
                                ))
                            })?;
                        }
                        (false, None) => {}
                    }
                    if companions
                        .insert(profile.companion_id.clone(), profile)
                        .is_some()
                    {
                        return Err(AppError::Internal(
                            "duplicate companion identity found during scan".into(),
                        ));
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(AppError::Internal(format!(
                    "scan companion directory {}: {error}",
                    companions_dir.display()
                )));
            }
        }
        let watermark = CompanionSeqState::load(&shared_dir)?
            .last_companion_seq
            .max(max_live_seq(&companions));
        Ok(Self {
            companions_dir,
            shared_dir,
            watermark: RwLock::new(watermark),
            inner: RwLock::new(companions),
            provider_repo,
            provider_lifecycle,
        })
    }

    /// All companions, oldest first (`created_at` ascending, `companion_id` as tie-break so the
    /// order is stable even for same-millisecond creations).
    pub async fn list(&self) -> Vec<CompanionProfileConfig> {
        let mut companions: Vec<CompanionProfileConfig> = self.inner.read().await.values().cloned().collect();
        companions.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.companion_id.cmp(&b.companion_id))
        });
        companions
    }

    /// Root of the per-companion directories (`{data_dir}/companion/companions`) — where
    /// non-config per-companion artifacts (e.g. the DIY figure image) live too.
    pub(crate) fn companions_dir(&self) -> &Path {
        &self.companions_dir
    }

    /// 伙伴工作区树根：companions_dir 的兄弟目录 `{data_dir}/companion/workspaces`。
    /// （companions_dir == `{data_dir}/companion/companions`，取 parent 再 join。）
    /// 见名知意的每伙伴工作目录落在此树下，与 home 目录解耦。
    pub(crate) fn workspaces_dir(&self) -> std::path::PathBuf {
        self.companions_dir
            .parent()
            .map(|p| p.join("workspaces"))
            .unwrap_or_else(|| self.companions_dir.join("workspaces"))
    }

    pub async fn get(&self, id: &str) -> Option<CompanionProfileConfig> {
        let id = CompanionId::try_from(id).ok()?;
        self.inner.read().await.get(id.as_str()).cloned()
    }

    /// Companion ids in the same order as [`list`](Self::list).
    pub async fn ids(&self) -> Vec<String> {
        self.list()
            .await
            .into_iter()
            .map(|p| p.companion_id)
            .collect()
    }

    /// 解析"代表全家发声"的伙伴 id（单一事实源，learner 与 evolution 引擎共用）。
    /// 存活的显式默认体优先；否则首个注册伙伴；空 roster 返回 `None`。
    /// liveness 检查同时修掉"默认体已删除却仍被当 owner"的潜伏问题。
    pub async fn resolve_default(&self, default_companion_id: Option<&str>) -> Option<String> {
        let ids = self.ids().await;
        if let Some(default_companion_id) = default_companion_id
            && ids.iter().any(|id| id == default_companion_id)
        {
            return Some(default_companion_id.to_owned());
        }
        ids.into_iter().next()
    }

    /// Create a companion: validate the name, allocate its short number from the
    /// registry watermark, durably advance the watermark, persist
    /// `{companions_dir}/{companion_id}/config.json`, then insert into the map under the
    /// write lock. A failed watermark write publishes nothing. A later profile
    /// failure may burn a display number, which is intentional: allocated
    /// numbers are never reused.
    pub async fn create(&self, name: &str, character: &str) -> Result<CompanionProfileConfig, AppError> {
        let name = validate_name(name)?;
        // Lock order: watermark before the roster map (see struct docs).
        let mut watermark = self.watermark.write().await;
        let mut companions = self.inner.write().await;
        // Never reuse: one past the watermark or the largest live seq,
        // whichever is bigger.
        let seq = (*watermark).max(max_live_seq(&companions)) + 1;
        let profile = CompanionProfileConfig::new(&name, character, seq);
        let dir = self.companions_dir.join(&profile.companion_id);
        self.advance_watermark(&mut watermark, seq)?;
        profile
            .save(&dir)
            .map_err(|e| AppError::Internal(format!("save companion profile: {e}")))?;
        companions.insert(profile.companion_id.clone(), profile.clone());
        Ok(profile)
    }

    /// Durably advance the watermark to `seq` (never backwards) before
    /// publishing the entity that consumes that number.
    fn advance_watermark(
        &self,
        watermark: &mut u64,
        seq: u64,
    ) -> Result<(), AppError> {
        if seq <= *watermark {
            return Ok(());
        }
        (CompanionSeqState { last_companion_seq: seq })
            .save(&self.shared_dir)
            .map_err(|error| {
                AppError::Internal(format!(
                    "save companion sequence watermark: {error}"
                ))
            })?;
        *watermark = seq;
        Ok(())
    }

    /// RFC 7396 partial update of one profile. `companion_id`, `seq` and `created_at`
    /// are immutable — whatever the patch says, they are restored from the
    /// current profile before saving.
    pub async fn patch(&self, id: &str, patch: serde_json::Value) -> Result<CompanionProfileConfig, AppError> {
        if !patch.is_object() {
            return Err(AppError::BadRequest("companion patch must be a JSON object".into()));
        }
        let id = CompanionId::try_from(id)
            .map_err(|error| AppError::BadRequest(format!("invalid companion id: {error}")))?;
        // Lock order is always Provider lifecycle barrier before the side-store
        // roster. Provider deletion holds the write side, then scans the roster.
        let _provider_guard = if let Some(barrier) = self.provider_lifecycle.as_ref() {
            Some(barrier.read().await)
        } else {
            None
        };
        let mut companions = self.inner.write().await;
        let current = companions
            .get(id.as_str())
            .ok_or_else(|| AppError::NotFound(format!("companion '{id}' not found")))?;
        let mut value = serde_json::to_value(current)
            .map_err(|e| AppError::Internal(format!("serialize companion profile: {e}")))?;
        json_merge_patch(&mut value, &patch);
        value["companion_id"] =
            serde_json::Value::String(current.companion_id.clone());
        value["seq"] = serde_json::Value::from(current.seq);
        value["created_at"] = serde_json::Value::Number(current.created_at.into());
        let mut merged: CompanionProfileConfig = serde_json::from_value(value)
            .map_err(|e| AppError::BadRequest(format!("invalid companion patch: {e}")))?;
        merged.companion_id = current.companion_id.clone();
        merged.seq = current.seq;
        merged.created_at = current.created_at;
        merged.name = validate_name(&merged.name)?;
        validate_provider_model(self.provider_repo.as_ref(), merged.model.as_ref()).await?;
        merged
            .save(&self.companions_dir.join(&merged.companion_id))
            .map_err(|e| AppError::Internal(format!("save companion profile: {e}")))?;
        companions.insert(merged.companion_id.clone(), merged.clone());
        Ok(merged)
    }

    /// Remove a companion from the map and delete its directory (an already-missing
    /// directory is tolerated). Returns the removed profile.
    pub async fn remove(&self, id: &str) -> Result<CompanionProfileConfig, AppError> {
        let id = CompanionId::try_from(id)
            .map_err(|error| AppError::BadRequest(format!("invalid companion id: {error}")))?;
        let _provider_guard = if let Some(barrier) = self.provider_lifecycle.as_ref() {
            Some(barrier.read().await)
        } else {
            None
        };
        let mut companions = self.inner.write().await;
        let profile = companions
            .get(id.as_str())
            .cloned()
            .ok_or_else(|| AppError::NotFound(format!("companion '{id}' not found")))?;
        crate::fsio::remove_path_entry(&self.companions_dir.join(id.as_str()))
            .map_err(|error| AppError::Internal(format!("remove companion dir: {error}")))?;
        companions.remove(id.as_str());
        Ok(profile)
    }

    /// Verify every loaded profile's Provider parent while the caller holds a
    /// shared lifecycle read guard. A missing Provider is an orphaned hard
    /// reference and fails startup.
    pub(crate) async fn validate_provider_references_under_guard(
        &self,
    ) -> Result<(), AppError> {
        let profiles: Vec<_> = self.inner.read().await.values().cloned().collect();
        for profile in profiles {
            validate_provider_model(self.provider_repo.as_ref(), profile.model.as_ref())
                .await
                .map_err(|error| {
                    AppError::Internal(format!(
                        "companion '{}' has an orphaned provider reference: {error}",
                        profile.companion_id
                    ))
                })?;
        }
        Ok(())
    }

    /// Standalone audit helper for callers that are not already under the
    /// lifecycle barrier.
    pub async fn validate_provider_references(&self) -> Result<(), AppError> {
        let _provider_guard = if let Some(barrier) = self.provider_lifecycle.as_ref() {
            Some(barrier.read().await)
        } else {
            None
        };
        self.validate_provider_references_under_guard().await
    }

    /// Audit every profile-to-library figure reference. The profile field is a
    /// hard logical reference regardless of the currently selected character:
    /// stale hidden links are rejected instead of allowing deletion to create
    /// silent orphans.
    pub(crate) async fn validate_figure_references(
        &self,
        live_figure_ids: &std::collections::HashSet<String>,
    ) -> Result<(), AppError> {
        for profile in self.inner.read().await.values() {
            let Some(figure_id) = profile
                .appearance
                .custom_figure
                .as_ref()
                .and_then(|figure| figure.figure_id.as_deref())
            else {
                continue;
            };
            if !live_figure_ids.contains(figure_id) {
                return Err(AppError::Internal(format!(
                    "companion '{}' references missing figure '{}'",
                    profile.companion_id, figure_id
                )));
            }
        }
        Ok(())
    }
}

async fn validate_provider_model(
    provider_repo: Option<&std::sync::Arc<dyn IProviderRepository>>,
    model: Option<&ProviderWithModel>,
) -> Result<(), AppError> {
    let (Some(provider_repo), Some(model)) = (provider_repo, model) else {
        return Ok(());
    };
    if provider_repo
        .find_by_id(&model.provider_id)
        .await
        .map_err(|error| AppError::Internal(format!("check companion model provider: {error}")))?
        .is_none()
    {
        return Err(AppError::NotFound(format!(
            "provider '{}' not found",
            model.provider_id
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Registry over `{dir}/companions` with its watermark state at
    /// `{dir}/shared/companion_seq.json` (the production sibling layout).
    fn scan_at(
        dir: &std::path::Path,
    ) -> Result<CompanionRegistry, AppError> {
        scan_companions_at(dir, "companions")
    }

    /// Same as [`scan_at`] but over `{dir}/{companions}`.
    fn scan_companions_at(
        dir: &std::path::Path,
        companions: &str,
    ) -> Result<CompanionRegistry, AppError> {
        CompanionRegistry::scan(dir.join(companions), dir.join("shared"))
    }

    fn registry(dir: &std::path::Path) -> CompanionRegistry {
        scan_at(dir).unwrap()
    }

    #[test]
    fn merge_patch_merges_nested_and_replaces_scalars() {
        let mut base = serde_json::json!({
            "appearance": {"companion_enabled": false, "companion_x": 10, "quiet_start": ""},
            "learn": {"enabled": true, "interval_minutes": 60}
        });
        json_merge_patch(
            &mut base,
            &serde_json::json!({"appearance": {"companion_x": 99, "companion_y": 42}}),
        );
        assert_eq!(base["appearance"]["companion_x"], 99);
        assert_eq!(base["appearance"]["companion_y"], 42);
        assert_eq!(base["appearance"]["companion_enabled"], false);
        assert_eq!(base["learn"]["interval_minutes"], 60);
    }

    #[tokio::test]
    async fn resolve_default_prefers_alive_explicit_then_first() {
        let dir = tempfile::tempdir().unwrap();
        let reg = CompanionRegistry::scan(
            dir.path().join("companions"),
            dir.path().join("shared"),
        )
        .unwrap();
        // 空 roster → 无默认伙伴
        assert_eq!(reg.resolve_default(None).await, None);
        let _a = reg.create("甲", "ink").await.unwrap();
        let b = reg.create("乙", "ink").await.unwrap();
        let first = reg.ids().await.into_iter().next().unwrap();
        // 显式默认体且存活 → 用之
        assert_eq!(reg.resolve_default(Some(&b.companion_id)).await.as_deref(), Some(b.companion_id.as_str()));
        // 显式默认体已删（不在 roster）→ 回退首个注册
        assert_eq!(reg.resolve_default(Some("malformed-companion-id")).await.as_deref(), Some(first.as_str()));
        // 未配置默认体 → 首个注册
        assert_eq!(reg.resolve_default(None).await.as_deref(), Some(first.as_str()));
    }

    #[tokio::test]
    async fn create_persists_and_lists() {
        let dir = tempfile::tempdir().unwrap();
        let reg = registry(dir.path());
        assert!(reg.list().await.is_empty());

        let companion = reg.create("  毛球  ", "ink").await.unwrap();
        assert_eq!(companion.companion_id.len(), nomifun_common::UUID_STRING_LEN);
        assert_eq!(companion.name, "毛球"); // trimmed
        assert_eq!(companion.character, "ink");
        assert_eq!(companion.seq, 1);

        // Persisted on disk under {companions_dir}/{companion_id}/config.json.
        let on_disk =
            CompanionProfileConfig::load(&dir.path().join("companions").join(&companion.companion_id))
                .unwrap()
                .unwrap();
        assert_eq!(on_disk, companion);

        assert_eq!(reg.get(&companion.companion_id).await.unwrap(), companion);
        assert_eq!(reg.ids().await, vec![companion.companion_id.clone()]);
    }

    #[tokio::test]
    async fn list_sorts_by_created_at_ascending() {
        let dir = tempfile::tempdir().unwrap();
        let companions_dir = dir.path().join("companions");
        // Hand-build two profiles with crafted created_at, newer one first
        // alphabetically so the sort genuinely exercises created_at.
        let mut newer = CompanionProfileConfig::new("新宠", "boo", 2);
        newer.created_at = 2_000;
        newer.save(&companions_dir.join(&newer.companion_id)).unwrap();
        let mut older = CompanionProfileConfig::new("老宠", "mochi", 1);
        older.created_at = 1_000;
        older.save(&companions_dir.join(&older.companion_id)).unwrap();

        let reg = scan_at(dir.path()).unwrap();
        let listed = reg.list().await;
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].companion_id, older.companion_id);
        assert_eq!(listed[1].companion_id, newer.companion_id);
        assert_eq!(reg.ids().await, vec![older.companion_id, newer.companion_id]);
    }

    #[tokio::test]
    async fn name_validation_rejects_empty_and_over_40_chars() {
        let dir = tempfile::tempdir().unwrap();
        let reg = registry(dir.path());

        assert!(matches!(reg.create("", "ink").await, Err(AppError::BadRequest(_))));
        assert!(matches!(reg.create("   ", "ink").await, Err(AppError::BadRequest(_))));

        // 40 chars (counted in chars, not bytes) is fine, 41 is not.
        let ok = "宠".repeat(40);
        let too_long = "宠".repeat(41);
        let companion = reg.create(&ok, "ink").await.unwrap();
        assert_eq!(companion.name.chars().count(), 40);
        assert!(matches!(reg.create(&too_long, "ink").await, Err(AppError::BadRequest(_))));

        // patch enforces the same rules.
        let err = reg.patch(&companion.companion_id, serde_json::json!({"name": too_long})).await;
        assert!(matches!(err, Err(AppError::BadRequest(_))));
        let err = reg.patch(&companion.companion_id, serde_json::json!({"name": "  "})).await;
        assert!(matches!(err, Err(AppError::BadRequest(_))));
    }

    #[tokio::test]
    async fn patch_renames_but_never_changes_companion_id_or_created_at() {
        let dir = tempfile::tempdir().unwrap();
        let reg = registry(dir.path());
        let companion = reg.create("旧名", "ink").await.unwrap();

        let patched = reg
            .patch(
                &companion.companion_id,
                serde_json::json!({
                    "name": "新名",
                    "companion_id": "not-a-companion-id",
                    "seq": 99,
                    "created_at": 1,
                    "appearance": {"companion_enabled": true, "companion_x": 7}
                }),
            )
            .await
            .unwrap();
        assert_eq!(patched.companion_id, companion.companion_id);
        assert_eq!(patched.seq, companion.seq, "seq is immutable through patches");
        assert_eq!(patched.created_at, companion.created_at);
        assert_eq!(patched.name, "新名");
        assert!(patched.appearance.companion_enabled);
        assert_eq!(patched.appearance.companion_x, Some(7));
        // Untouched fields survive the merge.
        assert_eq!(patched.character, "ink");

        // Persisted and visible through the map.
        let on_disk =
            CompanionProfileConfig::load(&dir.path().join("companions").join(&companion.companion_id))
                .unwrap()
                .unwrap();
        assert_eq!(on_disk, patched);
        assert_eq!(reg.get(&companion.companion_id).await.unwrap(), patched);

        let missing_id = CompanionId::new().into_string();
        assert!(matches!(
            reg.patch(&missing_id, serde_json::json!({"name": "x"})).await,
            Err(AppError::NotFound(_))
        ));
        assert!(matches!(
            reg.patch(&companion.companion_id, serde_json::json!(42)).await,
            Err(AppError::BadRequest(_))
        ));
    }

    #[tokio::test]
    async fn remove_deletes_dir_and_returns_profile() {
        let dir = tempfile::tempdir().unwrap();
        let reg = registry(dir.path());
        let companion = reg.create("一郎", "ink").await.unwrap();
        let keep = reg.create("二郎", "boo").await.unwrap();
        let companion_dir = dir.path().join("companions").join(&companion.companion_id);
        assert!(companion_dir.exists());

        let removed = reg.remove(&companion.companion_id).await.unwrap();
        assert_eq!(removed.companion_id, companion.companion_id);
        assert!(!companion_dir.exists());
        assert!(reg.get(&companion.companion_id).await.is_none());
        assert!(reg.get(&keep.companion_id).await.is_some());

        assert!(matches!(reg.remove(&companion.companion_id).await, Err(AppError::NotFound(_))));

        // An already-missing directory is tolerated.
        std::fs::remove_dir_all(dir.path().join("companions").join(&keep.companion_id)).unwrap();
        let removed = reg.remove(&keep.companion_id).await.unwrap();
        assert_eq!(removed.companion_id, keep.companion_id);
    }

    #[tokio::test]
    async fn scan_fails_closed_on_corrupt_mismatched_or_missing_configs() {
        let dir = tempfile::tempdir().unwrap();
        let companions_dir = dir.path().join("companions");

        let corrupt_dir = companions_dir.join(CompanionId::new().as_str());
        std::fs::create_dir_all(&corrupt_dir).unwrap();
        std::fs::write(corrupt_dir.join("config.json"), "{not json").unwrap();
        assert!(scan_at(dir.path()).is_err());

        std::fs::remove_dir_all(&corrupt_dir).unwrap();
        let homeless = CompanionProfileConfig::new("流浪", "boo", 2);
        let wrong_dir = companions_dir.join(CompanionId::new().as_str());
        homeless.save(&wrong_dir).unwrap();
        assert!(scan_at(dir.path()).is_err());

        std::fs::remove_dir_all(&wrong_dir).unwrap();
        std::fs::create_dir_all(companions_dir.join(CompanionId::new().as_str()))
            .unwrap();
        assert!(scan_at(dir.path()).is_err());

        // A missing companions dir is a valid empty registry.
        let empty = scan_companions_at(dir.path(), "nonexistent").unwrap();
        assert!(empty.list().await.is_empty());
    }

    #[tokio::test]
    async fn scan_rejects_missing_or_orphaned_local_figure_files() {
        let dir = tempfile::tempdir().unwrap();
        let companions_dir = dir.path().join("companions");

        let mut missing = CompanionProfileConfig::new("缺图", "custom", 1);
        missing.appearance.custom_figure = Some(crate::profile::CustomFigureMeta {
            aspect: 1.0,
            head_box: crate::profile::HeadBox {
                x: 0.1,
                y: 0.1,
                w: 0.5,
                h: 0.5,
            },
            size_tier: "m".into(),
            size_px: None,
            figure_id: None,
        });
        missing.save(&companions_dir.join(&missing.companion_id)).unwrap();
        assert!(scan_at(dir.path()).is_err());

        std::fs::remove_dir_all(&companions_dir).unwrap();
        let orphaned = CompanionProfileConfig::new("孤图", "ink", 1);
        let orphaned_dir = companions_dir.join(&orphaned.companion_id);
        orphaned.save(&orphaned_dir).unwrap();
        std::fs::write(orphaned_dir.join(crate::figure::FIGURE_FILE), b"not an image").unwrap();
        assert!(scan_at(dir.path()).is_err());
    }

    #[tokio::test]
    async fn create_allocates_monotonic_seq_never_reusing_deleted_numbers() {
        let dir = tempfile::tempdir().unwrap();
        let reg = registry(dir.path());

        let first = reg.create("一号", "ink").await.unwrap();
        let second = reg.create("二号", "boo").await.unwrap();
        assert_eq!(first.seq, 1);
        assert_eq!(second.seq, 2);

        // Deleting the highest-numbered companion must not free its number.
        reg.remove(&second.companion_id).await.unwrap();
        let third = reg.create("三号", "mochi").await.unwrap();
        assert_eq!(third.seq, 3);

        // The watermark is persisted in the registry's own state file (never
        // in the user-writable shared config, which the registry must not
        // touch at all)…
        assert_eq!(CompanionSeqState::load(&dir.path().join("shared")).unwrap().last_companion_seq, 3);
        assert!(!crate::profile::SharedCompanionConfig::config_path(&dir.path().join("shared")).exists());
        // …and the number on the profile itself.
        let on_disk =
            CompanionProfileConfig::load(&dir.path().join("companions").join(&third.companion_id))
                .unwrap()
                .unwrap();
        assert_eq!(on_disk.seq, 3);

        // A rescan (fresh process) keeps counting past the watermark even
        // when the highest-numbered companion is gone.
        reg.remove(&third.companion_id).await.unwrap();
        let reg2 = registry(dir.path());
        let fourth = reg2.create("四号", "boo").await.unwrap();
        assert_eq!(fourth.seq, 4);
    }

    #[tokio::test]
    async fn failed_create_does_not_publish_but_burns_allocated_sequence() {
        let dir = tempfile::tempdir().unwrap();
        // A regular file where the companions dir should be makes every profile
        // save fail (create_dir_all over a file errors on all platforms).
        std::fs::write(dir.path().join("companions"), "blocker").unwrap();
        let reg = CompanionRegistry {
            companions_dir: dir.path().join("companions"),
            shared_dir: dir.path().join("shared"),
            watermark: RwLock::new(0),
            inner: RwLock::new(HashMap::new()),
            provider_repo: None,
            provider_lifecycle: None,
        };

        assert!(matches!(reg.create("一号", "ink").await, Err(AppError::Internal(_))));
        // The entity was not published, but its allocated display number is
        // durable and can never be reused.
        assert_eq!(CompanionSeqState::load(&dir.path().join("shared")).unwrap().last_companion_seq, 1);
        assert!(reg.list().await.is_empty());

        // The retry (after the cause is fixed) gets #2.
        std::fs::remove_file(dir.path().join("companions")).unwrap();
        let companion = reg.create("一号", "ink").await.unwrap();
        assert_eq!(companion.seq, 2);
        assert_eq!(CompanionSeqState::load(&dir.path().join("shared")).unwrap().last_companion_seq, 2);
    }


}
