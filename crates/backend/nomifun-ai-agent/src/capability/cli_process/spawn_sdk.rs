use nomifun_common::{AppError, CommandSpec, ErrorChain};
use nomi_process_runtime::ChildProcessBuilder as CmdBuilder;
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Child;
use tokio::sync::{Mutex, broadcast};
use tracing::{debug, error, info, warn};

use super::{
    CliAgentProcess, EVENT_CHANNEL_CAPACITY, STDERR_BUFFER_MAX, prepare_command_cwd,
    spawn_exit_monitor, tracked_process_group_id,
};

impl CliAgentProcess {
    /// Spawn a new CLI subprocess in **SDK mode**.
    ///
    /// Unlike [`spawn`](Self::spawn), this does NOT start a stdout reader task.
    /// Instead, the raw stdin/stdout handles are available via [`take_stdio`](Self::take_stdio)
    /// for the ACP SDK transport to own.
    ///
    /// `data_dir` is the backend's `AppConfig.data_dir` — used as the root
    /// for child-process bun cache / tmp directories so they honour the
    /// operator's `--data-dir` choice instead of falling back to the OS
    /// local data dir.
    ///
    /// Background tasks are still spawned for:
    /// - stderr buffering
    /// - Process exit monitoring
    pub async fn spawn_for_sdk(config: CommandSpec, data_dir: &Path) -> Result<Self, AppError> {
        let proxy_env =
            nomifun_net::proxy::child_proxy_env(config.env.iter().map(|e| e.name.as_str()));
        let mut cmd = CmdBuilder::new(&config.command);
        cmd.args(&config.args)
            .envs(config.env.iter().map(|e| (&e.name, &e.value)))
            .envs(Self::agent_spawn_env(data_dir))
            .envs(
                proxy_env
                    .iter()
                    .map(|(name, value)| (name.as_str(), value.as_str())),
            )
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        if let Some(ref cwd) = config.cwd {
            cmd.current_dir(prepare_command_cwd(cwd)?);
        }
        let preview = cmd.to_string();
        info!(command = %preview, "Spawning CLI process (SDK mode)");
        let mut child: Child = cmd.spawn().map_err(|e| {
            error!(command = %preview, error = %ErrorChain(&e), "Failed to spawn CLI process");
            AppError::Internal(format!("Failed to spawn CLI process '{preview}': {e}"))
        })?;

        let pid = child.id().ok_or_else(|| {
            error!(command = %preview, "Failed to obtain PID from spawned process");
            AppError::Internal("Failed to obtain PID from spawned process".into())
        })?;
        info!(pid, command = %preview, "CLI process spawned (SDK mode)");

        let stdout = child.stdout.take().ok_or_else(|| {
            error!(pid, "Failed to capture stdout from child process");
            AppError::Internal("Failed to capture stdout from child process".into())
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            error!(pid, "Failed to capture stderr from child process");
            AppError::Internal("Failed to capture stderr from child process".into())
        })?;
        let stdin = child.stdin.take().ok_or_else(|| {
            error!(pid, "Failed to capture stdin for child process");
            AppError::Internal("Failed to capture stdin for child process".into())
        })?;

        let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);

        // Background task: read stderr → ring buffer + log
        let stderr_buffer = Arc::new(Mutex::new(String::new()));
        let stderr_buf_clone = Arc::clone(&stderr_buffer);
        let stderr_handle = tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();

            while let Ok(Some(line)) = lines.next_line().await {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    warn!(pid, stderr = trimmed, "CLI process stderr");
                }
                let mut buf = stderr_buf_clone.lock().await;
                buf.push_str(&line);
                buf.push('\n');
                if buf.len() > STDERR_BUFFER_MAX {
                    let cut = buf.len() - STDERR_BUFFER_MAX;
                    buf.drain(..cut);
                }
            }

            debug!(pid, "Stderr reader finished");
        });

        let (force_kill_tx, exit_rx, exit_handle) = spawn_exit_monitor(child, pid);

        Ok(Self {
            stdin: Mutex::new(Some(stdin)),
            stdout: Mutex::new(Some(stdout)),
            pid,
            process_group_id: tracked_process_group_id(pid),
            event_tx,
            force_kill_tx,
            exit_rx,
            initial_rx: std::sync::Mutex::new(None),
            stderr_buffer,
            _stdout_handle: None,
            _stderr_handle: Arc::new(stderr_handle),
            _exit_handle: Arc::new(exit_handle),
        })
    }

    /// Build environment variables for agent subprocess spawn.
    /// Mirrors the frontend `acpConnectors.ts::getCleanAgentEnv` logic:
    /// - Set BUN_INSTALL_CACHE_DIR / BUN_TMPDIR to stable paths under
    ///   the backend's `AppConfig.data_dir`
    /// - Set CLAUDE_CODE_EXECUTABLE so claude-agent-sdk finds the CLI
    fn agent_spawn_env(data_dir: &Path) -> Vec<(String, String)> {
        let bun_cache = data_dir.join("bun-cache");
        let bun_tmp = data_dir.join("bun-tmp");

        let mut env = vec![
            ("BUN_INSTALL_CACHE_DIR".into(), bun_cache.to_string_lossy().into_owned()),
            ("BUN_TMPDIR".into(), bun_tmp.to_string_lossy().into_owned()),
        ];

        // PATH enrichment (including bundled bun dir) is handled globally by
        // `nomifun_runtime::enhance_process_path` during startup; children
        // inherit it automatically. No per-spawn injection needed.

        if let Some(claude_path) = Self::find_native_claude() {
            env.push(("CLAUDE_CODE_EXECUTABLE".into(), claude_path));
        }

        env
    }

    /// Find the native Claude Code binary so `claude-agent-sdk` can spawn it
    /// directly via `CLAUDE_CODE_EXECUTABLE`.
    ///
    /// Walks `PATH` in declared order. The actual binary check is delegated
    /// to `nomifun_runtime::resolve_command_in`, which honours `PATHEXT` on
    /// Windows and adds the `.cmd / .ps1 / .bat` shim fallback for
    /// npm-installed CLIs.
    fn find_native_claude() -> Option<String> {
        let path_var = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&path_var) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            if let Some(found) = nomifun_runtime::resolve_command_in("claude", &dir) {
                return Some(found.to_string_lossy().into_owned());
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::simple_script_config;
    use super::*;
    use std::time::Duration;

    // ── SDK mode tests ───────────────────────────────────────────────

    #[tokio::test]
    async fn spawn_for_sdk_take_stdio() {
        let config = simple_script_config("read line && echo \"$line\"");
        let tmp = std::env::temp_dir();
        let proc = CliAgentProcess::spawn_for_sdk(config, &tmp).await.unwrap();

        let stdio = proc.take_stdio().await;
        assert!(stdio.is_some(), "First take_stdio should succeed");

        let stdio_again = proc.take_stdio().await;
        assert!(stdio_again.is_none(), "Second take_stdio should return None");

        proc.kill(Duration::from_millis(100)).await.unwrap();
    }
}
