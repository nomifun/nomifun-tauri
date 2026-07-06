//! Materialize the embedded builtin-skills corpus to disk.

use std::path::Path;

use anyhow::Result;
use tracing::warn;

/// Gated by a `.version` file so this is a no-op on subsequent starts with
/// the same binary. When `NOMIFUN_BUILTIN_SKILLS_PATH` is set, skip
/// materialization — the override path is the source of truth in that mode.
pub(super) async fn materialize_builtin_skills(data_dir: &Path) -> Result<()> {
    let skip = std::env::var(nomifun_extension::BUILTIN_SKILLS_ENV_VAR)
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    if skip {
        return Ok(());
    }

    let materialize_version = nomifun_extension::builtin_skills_materialize_version(env!("CARGO_PKG_VERSION"));
    nomifun_extension::materialize_if_needed(
        data_dir,
        nomifun_extension::builtin_skills_corpus(),
        &materialize_version,
    )
    .await
    .map_err(|e| anyhow::anyhow!("Failed to materialize builtin skills: {e}"))?;

    // Best-effort cleanup of directories left behind by pre-symlink
    // refactors. Failures are non-fatal — stale empty dirs are harmless.
    for stale in ["builtin-skills-view", "tmp", "agent-skills"] {
        let path = data_dir.join(stale);
        if path.exists()
            && let Err(e) = std::fs::remove_dir_all(&path)
        {
            warn!(
                path = %path.display(),
                error = %e,
                "failed to clean up stale data dir entry",
            );
        }
    }
    Ok(())
}
