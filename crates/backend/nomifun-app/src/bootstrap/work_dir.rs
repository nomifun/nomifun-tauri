//! Resolve the conversation workspace directory.

use std::path::{Path, PathBuf};

/// Priority: `--work-dir` CLI flag → persisted UI choice (`dir-config.json` in
/// `data_dir`, see [`nomifun_common::dir_config`]) → `NOMIFUN_WORK_DIR` env
/// (when non-empty) → `--data-dir` fallback.
///
/// The persisted choice sits **above** the env var on purpose: `relaunch()`
/// restarts the whole process and the child can inherit the `NOMIFUN_WORK_DIR`
/// the previous boot exported (see `environment.rs`), so a UI change must win
/// over that stale inherited value.
pub(super) fn resolve_work_dir(cli_work_dir: Option<PathBuf>, data_dir: &Path) -> PathBuf {
    if let Some(cli) = cli_work_dir {
        return cli;
    }
    if let Some(persisted) = nomifun_common::dir_config::persisted_work_dir(data_dir) {
        return persisted;
    }
    std::env::var("NOMIFUN_WORK_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| data_dir.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_common::{dir_config, now_ms};

    fn temp_data_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nomifun-wdres-{tag}-{}", now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn persisted_work_dir_is_used_when_no_cli_flag() {
        let data_dir = temp_data_dir("persisted");
        let chosen = data_dir.join("chosen-ws");
        dir_config::set_work_dir(&data_dir, &chosen).unwrap();

        // Persisted value takes priority over the data_dir fallback even with no
        // CLI flag — this is what makes a UI-chosen work dir stick across boots.
        assert_eq!(resolve_work_dir(None, &data_dir), chosen);

        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn cli_flag_wins_over_persisted_config() {
        let data_dir = temp_data_dir("cliwins");
        let persisted = data_dir.join("persisted-ws");
        dir_config::set_work_dir(&data_dir, &persisted).unwrap();
        let cli = data_dir.join("cli-ws");

        assert_eq!(resolve_work_dir(Some(cli.clone()), &data_dir), cli);

        let _ = std::fs::remove_dir_all(&data_dir);
    }
}
