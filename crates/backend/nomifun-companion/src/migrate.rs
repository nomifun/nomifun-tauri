//! One-shot migration from the legacy single-companion layout (`companion/nomi/`) to the
//! multi-companion split: shared artifacts (db + events + shared config) move to
//! `companion/shared/`, and the old identity/persona/window settings become the
//! first companion profile — named "Nomi" — under `companion/companions/{id}/`.
//!
//! Runs at boot, before the store/registry open. Idempotency is keyed on two
//! things only: no legacy dir, or a `.migrated` marker in the legacy dir. An
//! existing `shared` dir deliberately does *not* short-circuit — without the
//! marker it can only be the debris of a partially failed earlier attempt,
//! and the migration must re-run rather than silently strand (or lose) the
//! legacy memory. To make that re-run safe, all products are built in a
//! staging dir first and the legacy originals are only deleted after the
//! marker is written.
//!
//! Two crash windows shape the re-run rules:
//!
//! * **Window 1 — stale legacy must not overwrite fresh shared.** If the
//!   marker write failed after the commit, the session still runs on the
//!   committed `shared` db and writes new data. The re-run therefore stages
//!   per artifact preferring `shared` over the legacy originals (a committed
//!   `shared` is always ⊇ legacy), and adopts the previously minted first
//!   companion ("Nomi") instead of generating a fresh id (which would orphan its
//!   per-companion rows and leave a ghost duplicate companion).
//! * **Window 2 — never delete the only live copy.** Commit moves a
//!   half-built `shared` aside (`companion/.migrating-displaced-<ts>`) instead of
//!   removing it, and only sweeps displaced dirs after the marker is
//!   written. Symmetrically, the staging-residue cleanup at entry first
//!   rescues any artifact whose only copy lives in staging (a crash between
//!   "shared displaced" and "staging renamed into place").

use std::path::{Path, PathBuf};

use nomifun_common::{generate_prefixed_id, now_ms};

use crate::config::CompanionConfig;
use crate::profile::{CompanionProfileConfig, CompanionWindowConfig, SharedLearnConfig, SharedCompanionConfig};

/// Marker file written into the legacy dir after a successful migration;
/// its content is the generated first-companion id.
pub(crate) const MIGRATED_MARKER: &str = ".migrated";

/// Carry a pre-rename `{data}/pet` tree forward to `{data}/companion` (and the
/// per-entity `companion/pets` subdir to `companion/companions`), so installs
/// created under the old "pet" naming keep their data — db, figures, per-companion
/// configs — after the pet→companion rename. Runs FIRST in [`CompanionService::start`],
/// before [`migrate_legacy_layout`] and before the store opens. Only renames when
/// the target doesn't already exist (a fresh `companion` dir wins); best-effort —
/// an io error must never brick boot. Companion ids keep their opaque on-disk
/// values (a `pet_…` id stays valid; only new companions mint `companion_…`).
pub fn migrate_pet_dir_to_companion(data_dir: &Path) {
    let legacy_pet = data_dir.join("pet");
    let companion = data_dir.join("companion");
    if legacy_pet.is_dir() && !companion.exists() {
        if let Err(e) = std::fs::rename(&legacy_pet, &companion) {
            tracing::warn!(error = %e, "migrate '{{data}}/pet' -> '{{data}}/companion' failed; continuing");
            return;
        }
        tracing::info!("migrated legacy '{{data}}/pet' dir to '{{data}}/companion'");
    }
    let legacy_pets = companion.join("pets");
    let companions = companion.join("companions");
    if legacy_pets.is_dir() && !companions.exists() {
        if let Err(e) = std::fs::rename(&legacy_pets, &companions) {
            tracing::warn!(error = %e, "migrate 'companion/pets' -> 'companion/companions' failed; continuing");
        }
    }
    migrate_companion_config_keys(data_dir);
}

/// One-time rewrite of legacy `pet_*` JSON keys in the on-disk companion config
/// files to their `companion_*` names, so window prefs / the default-companion
/// pointer / the seq watermark survive the rename (profile id/name/character/
/// model already use rename-stable keys; memories migrate inside the store).
/// Surgical quoted-key replacement only — opaque `id` values (`pet_…`) are left
/// intact. Best-effort and idempotent (the legacy keys are gone after one run).
fn migrate_companion_config_keys(data_dir: &Path) {
    fn rewrite_keys(path: &Path, pairs: &[(&str, &str)]) {
        if !path.is_file() {
            return;
        }
        let Ok(orig) = std::fs::read_to_string(path) else { return };
        let mut s = orig.clone();
        for (from, to) in pairs {
            s = s.replace(from, to);
        }
        if s != orig {
            let _ = std::fs::write(path, s);
        }
    }

    let companion = data_dir.join("companion");
    let shared = companion.join("shared");

    // Seq watermark: rename the file and its inner key.
    let old_seq = shared.join("pet_seq.json");
    let new_seq = shared.join("companion_seq.json");
    if old_seq.is_file() && !new_seq.exists() {
        if let Ok(s) = std::fs::read_to_string(&old_seq) {
            let s = s.replace("\"last_pet_seq\"", "\"last_companion_seq\"");
            if std::fs::write(&new_seq, s).is_ok() {
                let _ = std::fs::remove_file(&old_seq);
            }
        }
    }

    rewrite_keys(
        &shared.join("config.json"),
        &[
            ("\"default_pet_id\"", "\"default_companion_id\""),
            ("\"pet_dialogues\"", "\"companion_dialogues\""),
        ],
    );

    if let Ok(entries) = std::fs::read_dir(companion.join("companions")) {
        for e in entries.flatten() {
            if e.path().is_dir() {
                rewrite_keys(
                    &e.path().join("config.json"),
                    &[
                        ("\"pet_enabled\"", "\"companion_enabled\""),
                        ("\"pet_x\"", "\"companion_x\""),
                        ("\"pet_y\"", "\"companion_y\""),
                    ],
                );
            }
        }
    }
}

/// Scratch dir (sibling of `shared`/`companions`) where all migration products are
/// staged before being committed into place. Leftovers from a crashed run
/// are wiped on the next attempt — after rescuing any artifact whose only
/// copy lives there (see [`salvage_unique_staging_copies`]).
const STAGING_REL_DIR: &str = "companion/.migrating";

/// Name prefix (under `companion/`) for the dir a pre-existing `shared` is moved
/// into during commit. Displaced dirs are only deleted after the marker is
/// written; stale ones from crashed runs are swept on the next success.
const DISPLACED_DIR_PREFIX: &str = ".migrating-displaced-";

/// The db plus its WAL sidecars (present only after an unclean shutdown).
/// Always staged as one family keyed on `memory.db`, never mixed across
/// source dirs.
const DB_FILES: [&str; 3] = ["memory.db", "memory.db-wal", "memory.db-shm"];

/// One-shot legacy migration: companion/nomi -> companion/shared + first companion "Nomi".
/// Idempotent and crash-safe via staging:
///
/// 1. Gate: only `legacy` missing or the `.migrated` marker skip the run.
/// 2. Stage: copy (never move) db/WAL/events — per artifact preferring the
///    committed `shared` copy over the legacy original (window 1) — and
///    write the split configs under [`STAGING_REL_DIR`]; the sources stay
///    untouched, so a crash anywhere in this phase restarts cleanly.
/// 3. Commit: displace any pre-existing `shared` aside (window 2: never
///    delete what might be the freshest copy before its replacement is in
///    place), rename the staged trees into `shared`/`companions`, then write the
///    marker.
/// 4. Cleanup: only now delete the legacy db/events copies, the displaced
///    dirs and the staging scratch; failures here are warn-only because the
///    marker already prevents a re-run.
///
/// Returns Some(first_companion_id) only when migration ran.
pub fn migrate_legacy_layout(data_dir: &Path) -> std::io::Result<Option<String>> {
    let legacy = data_dir.join(crate::COMPANION_REL_DIR);
    let shared = data_dir.join(crate::COMPANION_SHARED_REL_DIR);
    let companions = data_dir.join(crate::COMPANION_COMPANIONS_REL_DIR);

    if !legacy.exists() || legacy.join(MIGRATED_MARKER).exists() {
        return Ok(None);
    }

    // ----- staging residue: rescue, then wipe -----
    let staging = data_dir.join(STAGING_REL_DIR);
    if staging.exists() {
        salvage_unique_staging_copies(&staging.join("shared"), &shared, &legacy)?;
        std::fs::remove_dir_all(&staging)?;
    }
    let staging_shared = staging.join("shared");
    let staging_companions = staging.join("companions");
    std::fs::create_dir_all(&staging_shared)?;
    std::fs::create_dir_all(&staging_companions)?;

    // Copy the db family and the whole events dir into staging — copy, not
    // move: the sources must survive until the marker is written. Per
    // artifact the committed `shared` copy wins over the legacy original
    // (window 1): when a previous run committed but failed to write the
    // marker, the session kept writing into `shared`, so the legacy copy is
    // stale. The pre-staging implementation's half-moved files are covered
    // by the same preference.
    let db_src = [&shared, &legacy].into_iter().find(|dir| dir.join(DB_FILES[0]).exists());
    if let Some(src) = db_src {
        for file in DB_FILES {
            let from = src.join(file);
            if from.exists() {
                std::fs::copy(&from, staging_shared.join(file))?;
            }
        }
    }
    let legacy_events = legacy.join("events");
    let shared_events = shared.join("events");
    let events_src = [&shared_events, &legacy_events].into_iter().find(|dir| dir.exists());
    if let Some(src) = events_src {
        copy_dir_recursive(src, &staging_shared.join("events"))?;
    }
    // The registry's seq-watermark state file: only a committed `shared` can
    // hold one (the legacy layout predates companion numbering), so a copy here is
    // exactly the window-1 carry-over — without it the re-run would reset
    // the watermark and let deleted companion numbers be reused. A fresh migration
    // has none and starts at 0 (the registry mints the file on first
    // allocation).
    let seq_state = shared.join(crate::registry::SEQ_STATE_FILE);
    if seq_state.exists() {
        std::fs::copy(&seq_state, staging_shared.join(crate::registry::SEQ_STATE_FILE))?;
    }

    // Split the legacy config: collection + learn loop go shared (the learn
    // model inherits the old single model), identity/persona/window settings
    // become the first companion profile. A re-run adopts the id of an already
    // committed first companion (window 1) instead of minting a second one — a
    // fresh id would orphan the per-companion rows written meanwhile and leave a
    // ghost duplicate "Nomi" in the roster.
    let old = CompanionConfig::load(&legacy);
    let companion_id = existing_first_companion_id(&companions).unwrap_or_else(|| generate_prefixed_id("companion"));

    let shared_cfg = SharedCompanionConfig {
        collect: old.collect.clone(),
        learn: SharedLearnConfig {
            enabled: old.learn.enabled,
            interval_minutes: old.learn.interval_minutes,
            model: old.model.clone(),
        },
        default_companion_id: companion_id.clone(),
        bridge_to_memory_dir: None,
        ..Default::default()
    };
    shared_cfg.save(&staging_shared)?;

    let profile = CompanionProfileConfig {
        id: companion_id.clone(),
        // Window-1 adoption keeps the companion's short number too — rebuilding it
        // as None would let the boot backfill renumber an already-numbered
        // companion. A freshly minted first companion stays None and is numbered by the
        // boot backfill right after the registry scan.
        seq: CompanionProfileConfig::load(&companions.join(&companion_id)).seq,
        name: "Nomi".into(),
        character: old.appearance.character.clone(),
        persona: old.persona.clone(),
        model: old.model.clone(),
        appearance: CompanionWindowConfig {
            companion_enabled: old.appearance.companion_enabled,
            companion_x: old.appearance.companion_x,
            companion_y: old.appearance.companion_y,
            quiet_start: old.appearance.quiet_start.clone(),
            quiet_end: old.appearance.quiet_end.clone(),
            // Legacy installs predate the DIY figure feature.
            custom_figure: None,
        },
        created_at: now_ms(),
    };
    profile.save(&staging_companions.join(&companion_id))?;

    // ----- commit -----
    // Window 2: never delete the pre-existing shared dir here — between this
    // point and the rename below, a copy of the data could otherwise exist
    // only in staging, which the next run's residue wipe would destroy.
    // Move it aside instead; displaced dirs are only swept after the marker.
    if shared.exists() {
        move_path(&shared, &next_displaced_path(data_dir))?;
    }
    move_path(&staging_shared, &shared)?;

    if companions.exists() {
        // Defensive: merge just the staged first companion into an existing companions
        // tree instead of clobbering it.
        let target = companions.join(&companion_id);
        if target.exists() {
            std::fs::remove_dir_all(&target)?;
        }
        move_path(&staging_companions.join(&companion_id), &target)?;
    } else {
        move_path(&staging_companions, &companions)?;
    }

    std::fs::write(legacy.join(MIGRATED_MARKER), &companion_id)?;

    // ----- cleanup (best-effort: the marker already gates re-runs) -----
    sweep_displaced_dirs(data_dir);
    for file in DB_FILES {
        let from = legacy.join(file);
        if from.exists() {
            if let Err(e) = std::fs::remove_file(&from) {
                tracing::warn!("companion migrate: failed to remove legacy {file}: {e}");
            }
        }
    }
    if legacy_events.exists() {
        if let Err(e) = std::fs::remove_dir_all(&legacy_events) {
            tracing::warn!("companion migrate: failed to remove legacy events dir: {e}");
        }
    }
    if let Err(e) = std::fs::remove_dir_all(&staging) {
        tracing::warn!("companion migrate: failed to remove staging dir: {e}");
    }

    Ok(Some(companion_id))
}

/// Window 2 rescue, run before the staging-residue wipe: a crash between
/// "shared displaced" and "staging renamed into place" (or the legacy
/// pre-displacement implementation's `remove_dir_all(shared)` and the same
/// rename) leaves staging holding the *only* copy of the db/events. Any such
/// artifact — present in staging but in neither `shared` nor `legacy` — is
/// moved back into `shared` so the wipe cannot destroy it (the stage phase
/// then picks it up via the shared-first preference). Artifacts that still
/// have a source copy are deliberately left to the wipe: residue from a
/// crashed *stage* phase may be a torn partial copy, and an existing
/// original always wins over it.
fn salvage_unique_staging_copies(staging_shared: &Path, shared: &Path, legacy: &Path) -> std::io::Result<()> {
    if staging_shared.join(DB_FILES[0]).exists()
        && !shared.join(DB_FILES[0]).exists()
        && !legacy.join(DB_FILES[0]).exists()
    {
        std::fs::create_dir_all(shared)?;
        for file in DB_FILES {
            let from = staging_shared.join(file);
            if from.exists() {
                move_path(&from, &shared.join(file))?;
            }
        }
    }
    let staged_events = staging_shared.join("events");
    if staged_events.exists() && !shared.join("events").exists() && !legacy.join("events").exists() {
        std::fs::create_dir_all(shared)?;
        move_path(&staged_events, &shared.join("events"))?;
    }
    Ok(())
}

/// Window 1 id adoption: the first companion a previous (marker-less) run already
/// committed under `companions/`. Recognized by its migration-given name "Nomi"
/// with a profile that passes the registry's sanity rule (non-empty id
/// matching its directory). Oldest wins should several qualify.
fn existing_first_companion_id(companions: &Path) -> Option<String> {
    let entries = std::fs::read_dir(companions).ok()?;
    let mut found: Option<CompanionProfileConfig> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let profile = CompanionProfileConfig::load(&path);
        if profile.id.is_empty() || profile.id != entry.file_name().to_string_lossy() || profile.name != "Nomi" {
            continue;
        }
        if found.as_ref().is_none_or(|f| profile.created_at < f.created_at) {
            found = Some(profile);
        }
    }
    found.map(|p| p.id)
}

/// A fresh, unoccupied displacement dir under `companion/` for the commit phase.
fn next_displaced_path(data_dir: &Path) -> PathBuf {
    let companion_root = data_dir.join("companion");
    let base = now_ms();
    let mut n = 0u32;
    loop {
        let candidate = companion_root.join(format!("{DISPLACED_DIR_PREFIX}{base}-{n}"));
        if !candidate.exists() {
            return candidate;
        }
        n += 1;
    }
}

/// Delete every displacement dir under `companion/` (the current run's and stale
/// ones from crashed runs). Only called after the marker is written, so a
/// failure merely leaves debris behind — warn, never fail the migration.
fn sweep_displaced_dirs(data_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(data_dir.join("companion")) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with(DISPLACED_DIR_PREFIX) {
            continue;
        }
        if let Err(e) = std::fs::remove_dir_all(entry.path()) {
            tracing::warn!(dir = %entry.path().display(), "companion migrate: failed to remove displaced dir: {e}");
        }
    }
}

/// `fs::rename`, falling back to copy + remove when rename fails (e.g. the
/// target lands on another volume, or Windows holds a mapping on the source).
fn move_path(from: &Path, to: &Path) -> std::io::Result<()> {
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(_) => {
            if from.is_dir() {
                copy_dir_recursive(from, to)?;
                std::fs::remove_dir_all(from)
            } else {
                std::fs::copy(from, to)?;
                std::fs::remove_file(from)
            }
        }
    }
}

fn copy_dir_recursive(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir_recursive(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A legacy `companion/nomi` install: old-format config.json, a fake db file
    /// and one events JSONL.
    fn build_legacy(data_dir: &Path) {
        let legacy = data_dir.join(crate::COMPANION_REL_DIR);
        std::fs::create_dir_all(legacy.join("events")).unwrap();
        std::fs::write(
            legacy.join("config.json"),
            serde_json::json!({
                "collect": {"chat_user_messages": true, "cron_runs": true},
                "model": {"provider_id": "prov_x", "model": "claude-fable-5"},
                "learn": {"enabled": true, "interval_minutes": 30},
                "appearance": {
                    "companion_enabled": true,
                    "character": "ink",
                    "companion_x": 12,
                    "companion_y": 34,
                    "quiet_start": "22:00",
                    "quiet_end": "08:00"
                },
                "persona": {"preset": "calm", "custom": "多用颜文字"}
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(legacy.join("memory.db"), "fake-db-bytes").unwrap();
        std::fs::write(legacy.join("events").join("20260101.jsonl"), "{\"e\":1}\n").unwrap();
    }

    #[test]
    fn migrates_legacy_layout_once() {
        let dir = tempfile::tempdir().unwrap();
        build_legacy(dir.path());
        let legacy = dir.path().join(crate::COMPANION_REL_DIR);
        let shared = dir.path().join(crate::COMPANION_SHARED_REL_DIR);
        let companions = dir.path().join(crate::COMPANION_COMPANIONS_REL_DIR);

        let companion_id = migrate_legacy_layout(dir.path()).unwrap().expect("migration ran");
        assert!(companion_id.starts_with("companion_"));

        // Shared config: collect + learn inherited, learn model = old model,
        // default companion points at the new first companion.
        let shared_cfg = SharedCompanionConfig::load(&shared);
        assert!(shared_cfg.collect.chat_user_messages);
        assert!(shared_cfg.collect.cron_runs);
        assert!(!shared_cfg.collect.requirements);
        assert!(shared_cfg.learn.enabled);
        assert_eq!(shared_cfg.learn.interval_minutes, 30);
        assert_eq!(shared_cfg.learn.model.provider_id, "prov_x");
        assert_eq!(shared_cfg.learn.model.model, "claude-fable-5");
        assert_eq!(shared_cfg.default_companion_id, companion_id);

        // First companion profile: named Nomi, everything else from the old config.
        let profile = CompanionProfileConfig::load(&companions.join(&companion_id));
        assert_eq!(profile.id, companion_id);
        // A freshly minted first companion carries no number yet — the boot
        // backfill right after the registry scan assigns it.
        assert_eq!(profile.seq, None);
        assert_eq!(profile.name, "Nomi");
        assert_eq!(profile.character, "ink");
        assert_eq!(profile.persona.preset, "calm");
        assert_eq!(profile.persona.custom, "多用颜文字");
        assert_eq!(profile.model.provider_id, "prov_x");
        assert!(profile.appearance.companion_enabled);
        assert_eq!(profile.appearance.companion_x, Some(12));
        assert_eq!(profile.appearance.companion_y, Some(34));
        assert_eq!(profile.appearance.quiet_start, "22:00");
        assert_eq!(profile.appearance.quiet_end, "08:00");
        assert!(profile.created_at > 0);

        // Db and events moved (not copied) into shared.
        assert_eq!(std::fs::read_to_string(shared.join("memory.db")).unwrap(), "fake-db-bytes");
        assert!(!legacy.join("memory.db").exists());
        assert_eq!(
            std::fs::read_to_string(shared.join("events").join("20260101.jsonl")).unwrap(),
            "{\"e\":1}\n"
        );
        assert!(!legacy.join("events").exists());

        // Marker holds the new companion id.
        assert_eq!(std::fs::read_to_string(legacy.join(MIGRATED_MARKER)).unwrap(), companion_id);

        // Staging scratch dir is gone after a successful run.
        assert!(!dir.path().join(STAGING_REL_DIR).exists());

        // Second run is a no-op.
        assert_eq!(migrate_legacy_layout(dir.path()).unwrap(), None);
    }

    #[test]
    fn no_legacy_dir_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(migrate_legacy_layout(dir.path()).unwrap(), None);
        assert!(!dir.path().join(crate::COMPANION_SHARED_REL_DIR).exists());
        assert!(!dir.path().join(crate::COMPANION_COMPANIONS_REL_DIR).exists());
    }

    #[test]
    fn marker_blocks_migration_even_with_legacy_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        build_legacy(dir.path());
        let legacy = dir.path().join(crate::COMPANION_REL_DIR);
        std::fs::write(legacy.join(MIGRATED_MARKER), "companion_done").unwrap();

        assert_eq!(migrate_legacy_layout(dir.path()).unwrap(), None);
        // Nothing was touched: no shared dir, legacy artifacts intact.
        assert!(!dir.path().join(crate::COMPANION_SHARED_REL_DIR).exists());
        assert!(legacy.join("memory.db").exists());
        assert!(legacy.join("events").join("20260101.jsonl").exists());
    }

    #[test]
    fn half_built_shared_without_marker_is_redone() {
        // Simulate a partial failure of a previous attempt: the shared dir
        // exists (with junk, but no memory.db) and no marker was written.
        // The migration must re-run completely instead of skipping, and the
        // legacy memory must come through intact.
        let dir = tempfile::tempdir().unwrap();
        build_legacy(dir.path());
        let legacy = dir.path().join(crate::COMPANION_REL_DIR);
        let shared = dir.path().join(crate::COMPANION_SHARED_REL_DIR);
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(shared.join("config.json"), "{\"broken\":").unwrap();
        // Staging residue from the crashed run must be cleaned up too.
        let staging = dir.path().join(STAGING_REL_DIR);
        std::fs::create_dir_all(staging.join("shared")).unwrap();
        std::fs::write(staging.join("shared").join("memory.db"), "stale-staging-bytes").unwrap();

        let companion_id = migrate_legacy_layout(dir.path()).unwrap().expect("migration re-ran");

        // Full products, fed from legacy (not from the stale staging residue).
        assert_eq!(std::fs::read_to_string(shared.join("memory.db")).unwrap(), "fake-db-bytes");
        assert_eq!(
            std::fs::read_to_string(shared.join("events").join("20260101.jsonl")).unwrap(),
            "{\"e\":1}\n"
        );
        let shared_cfg = SharedCompanionConfig::load(&shared);
        assert_eq!(shared_cfg.default_companion_id, companion_id);
        let companions = dir.path().join(crate::COMPANION_COMPANIONS_REL_DIR);
        assert_eq!(CompanionProfileConfig::load(&companions.join(&companion_id)).id, companion_id);
        // Marker written, legacy db/events cleaned, staging gone.
        assert_eq!(std::fs::read_to_string(legacy.join(MIGRATED_MARKER)).unwrap(), companion_id);
        assert!(!legacy.join("memory.db").exists());
        assert!(!legacy.join("events").exists());
        assert!(!staging.exists());

        // And the redo is itself final: a third run is a no-op.
        assert_eq!(migrate_legacy_layout(dir.path()).unwrap(), None);
    }

    #[test]
    fn half_moved_db_is_salvaged_from_shared() {
        // The pre-staging implementation moved memory.db into shared before
        // writing the marker. If it crashed in that window, legacy has no db
        // but shared does — the re-run must salvage it instead of replacing
        // shared with an empty tree.
        let dir = tempfile::tempdir().unwrap();
        build_legacy(dir.path());
        let legacy = dir.path().join(crate::COMPANION_REL_DIR);
        let shared = dir.path().join(crate::COMPANION_SHARED_REL_DIR);
        std::fs::create_dir_all(shared.join("events")).unwrap();
        std::fs::rename(legacy.join("memory.db"), shared.join("memory.db")).unwrap();
        std::fs::rename(
            legacy.join("events").join("20260101.jsonl"),
            shared.join("events").join("20260101.jsonl"),
        )
        .unwrap();
        std::fs::remove_dir_all(legacy.join("events")).unwrap();

        let companion_id = migrate_legacy_layout(dir.path()).unwrap().expect("migration re-ran");

        assert_eq!(std::fs::read_to_string(shared.join("memory.db")).unwrap(), "fake-db-bytes");
        assert_eq!(
            std::fs::read_to_string(shared.join("events").join("20260101.jsonl")).unwrap(),
            "{\"e\":1}\n"
        );
        assert_eq!(std::fs::read_to_string(legacy.join(MIGRATED_MARKER)).unwrap(), companion_id);
        assert!(!dir.path().join(STAGING_REL_DIR).exists());
    }

    /// Dirs under `companion/` left by the commit's shared displacement.
    fn displaced_dirs(data_dir: &Path) -> Vec<std::path::PathBuf> {
        std::fs::read_dir(data_dir.join("companion"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(DISPLACED_DIR_PREFIX))
            .map(|e| e.path())
            .collect()
    }

    #[test]
    fn rerun_after_marker_write_failure_keeps_fresh_shared_and_reuses_companion_id() {
        // Crash window 1: a previous run committed shared + the first companion
        // but the marker write failed. The session then kept running on the
        // committed shared db and wrote new data; the legacy originals (the
        // cleanup never ran) hold only the stale pre-migration state.
        let dir = tempfile::tempdir().unwrap();
        build_legacy(dir.path());
        let legacy = dir.path().join(crate::COMPANION_REL_DIR);
        let shared = dir.path().join(crate::COMPANION_SHARED_REL_DIR);
        let companions = dir.path().join(crate::COMPANION_COMPANIONS_REL_DIR);

        let first_id = migrate_legacy_layout(dir.path()).unwrap().expect("first run");

        // Simulate the boot that followed: the registry backfill numbered the
        // first companion and advanced its watermark state file under shared.
        let mut numbered = CompanionProfileConfig::load(&companions.join(&first_id));
        numbered.seq = Some(1);
        numbered.save(&companions.join(&first_id)).unwrap();
        crate::registry::CompanionSeqState { last_companion_seq: 1 }.save(&shared).unwrap();

        // Reconstruct the window: marker gone, stale legacy artifacts still
        // in place, fresh post-commit writes in shared.
        std::fs::remove_file(legacy.join(MIGRATED_MARKER)).unwrap();
        std::fs::write(legacy.join("memory.db"), "fake-db-bytes").unwrap();
        std::fs::create_dir_all(legacy.join("events")).unwrap();
        std::fs::write(legacy.join("events").join("20260101.jsonl"), "{\"e\":1}\n").unwrap();
        std::fs::write(shared.join("memory.db"), "fresh-session-bytes").unwrap();
        std::fs::write(shared.join("events").join("20260102.jsonl"), "{\"e\":2}\n").unwrap();

        let second_id = migrate_legacy_layout(dir.path()).unwrap().expect("re-ran");

        // The fresh shared data wins over the stale legacy copy…
        assert_eq!(
            std::fs::read_to_string(shared.join("memory.db")).unwrap(),
            "fresh-session-bytes"
        );
        assert_eq!(
            std::fs::read_to_string(shared.join("events").join("20260102.jsonl")).unwrap(),
            "{\"e\":2}\n"
        );
        // …and the already-minted first companion is adopted: same id, exactly one
        // companion in the roster, no ghost duplicate.
        assert_eq!(second_id, first_id);
        let companion_dirs: Vec<_> = std::fs::read_dir(&companions).unwrap().flatten().collect();
        assert_eq!(companion_dirs.len(), 1);
        assert_eq!(CompanionProfileConfig::load(&companions.join(&first_id)).name, "Nomi");
        // The adopted companion keeps its short number, and the registry's
        // watermark state file is carried into the rebuilt shared dir
        // instead of being reset.
        assert_eq!(CompanionProfileConfig::load(&companions.join(&first_id)).seq, Some(1));
        assert_eq!(crate::registry::CompanionSeqState::load(&shared).last_companion_seq, 1);
        assert_eq!(SharedCompanionConfig::load(&shared).default_companion_id, first_id);
        assert_eq!(std::fs::read_to_string(legacy.join(MIGRATED_MARKER)).unwrap(), first_id);
        // The displaced previous shared was swept after the marker.
        assert!(displaced_dirs(dir.path()).is_empty());
        assert!(!dir.path().join(STAGING_REL_DIR).exists());
    }

    #[test]
    fn staging_only_db_survives_rerun() {
        // Crash window 2 (legacy implementation): the commit removed shared
        // and crashed before renaming staging into place — the staged copies
        // are the only ones left (the legacy db/events were consumed by an
        // even earlier pre-staging attempt). The re-run's residue cleanup
        // must rescue them, not wipe them.
        let dir = tempfile::tempdir().unwrap();
        build_legacy(dir.path());
        let legacy = dir.path().join(crate::COMPANION_REL_DIR);
        std::fs::remove_file(legacy.join("memory.db")).unwrap();
        std::fs::remove_dir_all(legacy.join("events")).unwrap();
        let staging = dir.path().join(STAGING_REL_DIR);
        std::fs::create_dir_all(staging.join("shared").join("events")).unwrap();
        std::fs::write(staging.join("shared").join("memory.db"), "fake-db-bytes").unwrap();
        std::fs::write(
            staging.join("shared").join("events").join("20260101.jsonl"),
            "{\"e\":1}\n",
        )
        .unwrap();

        let companion_id = migrate_legacy_layout(dir.path()).unwrap().expect("migration ran");

        let shared = dir.path().join(crate::COMPANION_SHARED_REL_DIR);
        assert_eq!(std::fs::read_to_string(shared.join("memory.db")).unwrap(), "fake-db-bytes");
        assert_eq!(
            std::fs::read_to_string(shared.join("events").join("20260101.jsonl")).unwrap(),
            "{\"e\":1}\n"
        );
        assert_eq!(std::fs::read_to_string(legacy.join(MIGRATED_MARKER)).unwrap(), companion_id);
        assert!(!staging.exists());
        assert!(displaced_dirs(dir.path()).is_empty());
    }
}
