//! Built-in preset registry — embeds the manifest + rule/avatar
//! assets into the binary via `include_dir`, with an optional filesystem
//! fallback for E2E tests.
//!
//! This kills the "binary must live next to an on-disk assets/ sibling"
//! assumption, which was fragile in two ways:
//!
//! 1. Dev: Electron launches the backend through a symlink
//!    (`~/.cargo/bin/nomicore` → `target/debug/nomicore`) and
//!    `std::env::current_exe().parent()` would resolve to the symlink's
//!    directory, not the real binary's, missing the `assets/` sibling.
//! 2. Prod: `Nomi/scripts/prepareNomifunBackend.js` only copies the
//!    binary from GitHub releases — the `assets/` directory never shipped.
//!
//! Embedding avoids both. E2E tests that want to inject a custom fixture
//! still can, via the `NOMIFUN_BUILTIN_PRESETS_PATH` env var → disk
//! fallback path.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use include_dir::{Dir, include_dir};
use serde::Deserialize;
use tracing::{error, warn};

/// Assets compiled into the binary at build time. Paths are relative to
/// this embedded root, matching the on-disk layout under
/// `crates/nomifun-app/assets/builtin-presets/`.
static BUILTIN_ASSETS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../nomifun-app/assets/builtin-presets");

/// Single built-in preset entry, loaded from `presets.json`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuiltinPreset {
    /// Builtin catalog identity. Product-facing `preset_id` values are
    /// materialized UUIDv7 values and never reuse this natural key.
    pub source_key: String,
    pub name: String,
    #[serde(default)]
    pub name_i18n: HashMap<String, String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub description_i18n: HashMap<String, String>,
    #[serde(default)]
    pub avatar: Option<String>,
    pub preferred_agent_id: String,
    #[serde(default)]
    pub enabled_skills: Vec<String>,
    #[serde(default)]
    pub custom_skill_names: Vec<String>,
    #[serde(default)]
    pub disabled_builtin_skills: Vec<String>,
    /// Relative to the asset root; may contain `{locale}`.
    #[serde(default)]
    pub rule_file: Option<String>,
    #[serde(default)]
    pub prompts: Vec<String>,
    #[serde(default)]
    pub prompts_i18n: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub audience_tags: Vec<String>,
    #[serde(default)]
    pub scenario_tags: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct BuiltinManifest {
    #[serde(default)]
    #[allow(dead_code)]
    version: String,
    #[serde(default)]
    presets: Vec<BuiltinPreset>,
}

/// One built-in seed tag, loaded from `tags.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct BuiltinTag {
    pub key: String,
    /// "audience" | "scenario".
    pub dimension: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub label_i18n: HashMap<String, String>,
    #[serde(default)]
    pub sort_order: i32,
}

#[derive(Debug, Deserialize, Default)]
struct BuiltinTagManifest {
    #[serde(default)]
    tags: Vec<BuiltinTag>,
}

/// An avatar asset loaded from either the embedded bundle or a disk override.
///
/// Carries the raw bytes plus the file extension (lower-case, without the
/// leading dot) so the HTTP layer can set `Content-Type`.
#[derive(Debug, Clone)]
pub struct AvatarAsset {
    pub bytes: Vec<u8>,
    pub extension: Option<String>,
}

/// Source of built-in asset content.
///
/// The disk branch exists for E2E tests that point
/// `NOMIFUN_BUILTIN_PRESETS_PATH` at a fixture directory.
enum Source {
    Embedded,
    Disk(PathBuf),
}

/// In-memory registry of built-in presets.
pub struct BuiltinPresetRegistry {
    presets: HashMap<String, BuiltinPreset>,
    tags: Vec<BuiltinTag>,
    source: Source,
}

impl BuiltinPresetRegistry {
    /// Construct the registry.
    ///
    /// If `NOMIFUN_BUILTIN_PRESETS_PATH` is set and points to a readable
    /// directory, read from disk (test-only override). Otherwise use the
    /// assets embedded at compile time.
    pub fn load() -> Self {
        if let Ok(env) = std::env::var("NOMIFUN_BUILTIN_PRESETS_PATH") {
            let p = PathBuf::from(env);
            if p.exists() {
                return Self::load_from_dir(p);
            }
            warn!(
                "NOMIFUN_BUILTIN_PRESETS_PATH points to missing directory; \
                 falling back to embedded assets"
            );
        }
        Self::load_embedded()
    }

    /// Load the compiled-in assets.
    pub fn load_embedded() -> Self {
        let tags = BUILTIN_ASSETS
            .get_file("tags.json")
            .map(|f| parse_tags_bytes(f.contents()))
            .unwrap_or_default();
        let content = match BUILTIN_ASSETS.get_file("presets.json") {
            Some(f) => f.contents(),
            None => {
                // This can only happen if the embedded bundle itself is
                // missing the manifest — treat as a build error, but stay
                // graceful at runtime.
                error!("Embedded built-in manifest missing (presets.json)");
                return Self::with_presets(HashMap::new(), tags, Source::Embedded);
            }
        };
        let presets = parse_manifest_bytes(content);
        Self::with_presets(presets, tags, Source::Embedded)
    }

    /// Load from an explicit on-disk directory. Preserved for
    /// `NOMIFUN_BUILTIN_PRESETS_PATH` E2E fixtures — the three
    /// graceful-degradation branches below mirror the original filesystem
    /// behaviour.
    pub fn load_from_dir(assets_dir: PathBuf) -> Self {
        let tags = std::fs::read_to_string(assets_dir.join("tags.json"))
            .map(|c| parse_tags_str(&c))
            .unwrap_or_default();
        let manifest_path = assets_dir.join("presets.json");
        let content = match std::fs::read_to_string(&manifest_path) {
            Ok(c) => c,
            Err(e) => {
                warn!("Built-in manifest missing at {}: {}", manifest_path.display(), e);
                return Self::with_presets(HashMap::new(), tags, Source::Disk(assets_dir));
            }
        };
        let presets = parse_manifest_str(&content);
        Self::with_presets(presets, tags, Source::Disk(assets_dir))
    }

    fn with_presets(
        presets: HashMap<String, BuiltinPreset>,
        tags: Vec<BuiltinTag>,
        source: Source,
    ) -> Self {
        Self { presets, tags, source }
    }

    /// Construct an empty registry (safe fallback + test helper). Treated
    /// as embedded-source with zero entries; callers should prefer
    /// [`load`](Self::load) in production.
    pub fn empty() -> Self {
        Self::with_presets(HashMap::new(), Vec::new(), Source::Embedded)
    }

    pub fn has(&self, source_key: &str) -> bool {
        self.presets.contains_key(source_key)
    }

    pub fn get(&self, source_key: &str) -> Option<&BuiltinPreset> {
        self.presets.get(source_key)
    }

    pub fn all(&self) -> impl Iterator<Item = &BuiltinPreset> {
        self.presets.values()
    }

    pub fn is_empty(&self) -> bool {
        self.presets.is_empty()
    }

    pub fn len(&self) -> usize {
        self.presets.len()
    }

    /// The built-in seed tag vocabulary (from `tags.json`). Empty when the
    /// manifest is absent or malformed.
    pub fn tags(&self) -> &[BuiltinTag] {
        &self.tags
    }

    /// Read the rule file bytes for a built-in preset. Substitutes
    /// `{locale}` in the manifest-declared `rule_file` path. Returns `None`
    /// when the preset has no declared rule or the file is missing.
    pub fn rule_bytes(&self, source_key: &str, locale: &str) -> Option<Vec<u8>> {
        let rel = self.presets.get(source_key)?.rule_file.as_ref()?;
        self.read_asset(&rel.replace("{locale}", locale))
    }

    /// Read the avatar asset for a built-in preset along with its
    /// extension (for Content-Type inference). Returns `None` when the
    /// manifest does not declare an avatar or the file is missing.
    ///
    /// Note: when the manifest `avatar` field is an emoji string
    /// (like `"📝"`) rather than a relative path, no file is resolved and
    /// this method returns `None`. Callers treating an preset without a
    /// shipped avatar should fall back to the text avatar on the client.
    pub fn avatar_asset(&self, source_key: &str) -> Option<AvatarAsset> {
        let a = self.presets.get(source_key)?;
        let rel = a.avatar.as_ref()?;
        // Emoji / text avatars have no path separator and no extension.
        if !looks_like_relative_path(rel) {
            return None;
        }
        let bytes = self.read_asset(rel)?;
        let extension = Path::new(rel)
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase());
        Some(AvatarAsset { bytes, extension })
    }

    /// Dispatch read to embedded bundle or disk, depending on source.
    fn read_asset(&self, rel: &str) -> Option<Vec<u8>> {
        match &self.source {
            Source::Embedded => BUILTIN_ASSETS.get_file(rel).map(|f| f.contents().to_vec()),
            Source::Disk(root) => std::fs::read(root.join(rel)).ok(),
        }
    }
}

impl Default for BuiltinPresetRegistry {
    fn default() -> Self {
        Self::empty()
    }
}

fn parse_manifest_bytes(bytes: &[u8]) -> HashMap<String, BuiltinPreset> {
    match serde_json::from_slice::<BuiltinManifest>(bytes) {
        Ok(m) => m.presets.into_iter().map(|a| (a.source_key.clone(), a)).collect(),
        Err(e) => {
            error!("Embedded built-in manifest parse failed: {e}");
            HashMap::new()
        }
    }
}

fn parse_manifest_str(content: &str) -> HashMap<String, BuiltinPreset> {
    match serde_json::from_str::<BuiltinManifest>(content) {
        Ok(m) => m.presets.into_iter().map(|a| (a.source_key.clone(), a)).collect(),
        Err(e) => {
            error!("Built-in manifest parse failed: {e}");
            HashMap::new()
        }
    }
}

/// Parse the embedded `tags.json` bytes. Missing/malformed → empty vec
/// (built-in seed tags are optional; they ship in a later phase).
fn parse_tags_bytes(bytes: &[u8]) -> Vec<BuiltinTag> {
    serde_json::from_slice::<BuiltinTagManifest>(bytes).map(|m| m.tags).unwrap_or_default()
}

/// String variant of [`parse_tags_bytes`] for the disk-source loader.
fn parse_tags_str(content: &str) -> Vec<BuiltinTag> {
    serde_json::from_str::<BuiltinTagManifest>(content).map(|m| m.tags).unwrap_or_default()
}

/// Heuristic for distinguishing a relative-path avatar (`"rules/x.svg"`)
/// from an inline emoji/text avatar (`"📝"`). Path-like strings contain a
/// `/` or at least one `.` extension separator.
fn looks_like_relative_path(s: &str) -> bool {
    s.contains('/') || (Path::new(s).extension().is_some() && !s.starts_with('.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const GEMINI_AGENT_ID: &str = "0190f5fe-7c00-7a00-8000-000000000103";

    fn write_manifest(dir: &Path, body: &str) {
        std::fs::write(dir.join("presets.json"), body).unwrap();
    }

    // -----------------------------------------------------------------------
    // Embedded-source sanity. NomiFun currently ships no presets because
    // task workflows live in builtin Skills; user and extension presets are
    // still fully supported by the registry and service layers.
    // -----------------------------------------------------------------------

    #[test]
    fn load_embedded_manifest_is_intentionally_empty() {
        let reg = BuiltinPresetRegistry::load_embedded();
        assert!(reg.is_empty(), "task workflows must be shipped as Skills, not builtin presets");
    }

    #[test]
    fn load_embedded_tags_has_both_dimensions() {
        let reg = BuiltinPresetRegistry::load_embedded();
        assert!(reg.tags().iter().any(|t| t.dimension == "audience"));
        assert!(reg.tags().iter().any(|t| t.dimension == "scenario"));
        assert!(reg.tags().iter().any(|t| t.key == "office"));
    }

    // -----------------------------------------------------------------------
    // Disk-source fallback (used by E2E fixtures via
    // NOMIFUN_BUILTIN_PRESETS_PATH). Graceful-degradation semantics must
    // stay intact.
    // -----------------------------------------------------------------------

    #[test]
    fn load_from_dir_missing_dir_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("nope");
        let reg = BuiltinPresetRegistry::load_from_dir(missing);
        assert!(reg.is_empty());
    }

    #[test]
    fn load_from_dir_missing_manifest_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let reg = BuiltinPresetRegistry::load_from_dir(tmp.path().to_path_buf());
        assert!(reg.is_empty());
    }

    #[test]
    fn load_from_dir_malformed_manifest_returns_empty() {
        let tmp = TempDir::new().unwrap();
        write_manifest(tmp.path(), "{not valid json");
        let reg = BuiltinPresetRegistry::load_from_dir(tmp.path().to_path_buf());
        assert!(reg.is_empty());
    }

    #[test]
    fn load_from_dir_reads_bytes_from_disk() {
        let tmp = TempDir::new().unwrap();
        let rules_dir = tmp.path().join("rules");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(rules_dir.join("office.en-US.md"), "office rule body").unwrap();
        write_manifest(
            tmp.path(),
            r#"{
                "version": "1.0.0",
                "presets": [{
                    "source_key": "builtin-office",
                    "name": "Office",
                    "preferred_agent_id": "0190f5fe-7c00-7a00-8000-000000000103",
                    "rule_file": "rules/office.{locale}.md"
                }]
            }"#,
        );

        let reg = BuiltinPresetRegistry::load_from_dir(tmp.path().to_path_buf());
        assert_eq!(reg.len(), 1);
        assert!(reg.has("builtin-office"));

        let bytes = reg
            .rule_bytes("builtin-office", "en-US")
            .expect("disk-source rule_bytes should read the fixture");
        assert_eq!(bytes, b"office rule body");
    }

    #[test]
    fn load_from_dir_missing_asset_returns_none() {
        let tmp = TempDir::new().unwrap();
        write_manifest(
            tmp.path(),
            r#"{
                "presets": [{
                    "source_key": "x",
                    "name": "X",
                    "preferred_agent_id": "0190f5fe-7c00-7a00-8000-000000000103",
                    "rule_file": "rules/x.{locale}.md"
                }]
            }"#,
        );
        let reg = BuiltinPresetRegistry::load_from_dir(tmp.path().to_path_buf());
        assert!(reg.rule_bytes("x", "en-US").is_none());
    }

    // -----------------------------------------------------------------------
    // load() env-var routing
    // -----------------------------------------------------------------------

    #[test]
    fn load_respects_env_var_disk_override() {
        let tmp = TempDir::new().unwrap();
        write_manifest(
            tmp.path(),
            r#"{"presets":[{"source_key":"env-only","name":"E","preferred_agent_id":"0190f5fe-7c00-7a00-8000-000000000103"}]}"#,
        );
        // SAFETY: env-var mutation is only unsafe if another thread reads
        // environment concurrently. This test is self-contained.
        // SAFETY: cargo test runs tests in parallel by default, so guard
        // against interference from other tests by using a unique env-var
        // value and checking via a dedicated loader call.
        let key = "NOMIFUN_BUILTIN_PRESETS_PATH";
        let prev = std::env::var(key).ok();
        // SAFETY: set_var is sound when no other thread is concurrently
        // reading env. Tests within this module do not share mutation, and
        // the env key is not observed by other tests.
        unsafe {
            std::env::set_var(key, tmp.path());
        }
        let reg = BuiltinPresetRegistry::load();
        assert!(reg.has("env-only"));
        assert!(!reg.has("word-creator"));
        match prev {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
    }

    // -----------------------------------------------------------------------
    // Avatar asset — emoji vs file
    // -----------------------------------------------------------------------

    #[test]
    fn avatar_asset_returns_bytes_and_extension_for_file_avatar() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("duck.svg"), b"<svg/>").unwrap();
        write_manifest(
            tmp.path(),
            r#"{"presets":[{
                "source_key": "with-file-avatar",
                "name": "F",
                "preferred_agent_id": "0190f5fe-7c00-7a00-8000-000000000103",
                "avatar": "duck.svg"
            }]}"#,
        );
        let reg = BuiltinPresetRegistry::load_from_dir(tmp.path().to_path_buf());
        assert_eq!(
            reg.get("with-file-avatar").map(|preset| preset.preferred_agent_id.as_str()),
            Some(GEMINI_AGENT_ID)
        );
        let asset = reg.avatar_asset("with-file-avatar").unwrap();
        assert_eq!(asset.bytes, b"<svg/>");
        assert_eq!(asset.extension.as_deref(), Some("svg"));
    }
}
