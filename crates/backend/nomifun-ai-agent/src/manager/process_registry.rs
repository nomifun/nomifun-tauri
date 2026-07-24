use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use nomifun_common::{AgentType, AppError, ErrorChain};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::capability::cli_process::CliAgentProcess;

pub(crate) const AGENT_PROCESS_REGISTRY_RELATIVE_PATH: &str = "runtime/agent-process-registry.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RegisteredAgentProcess {
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_group_id: Option<u32>,
    pub conversation_id: String,
    pub agent_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_preview: Option<String>,
    pub registered_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ProcessRegistry {
    version: u32,
    processes: Vec<RegisteredAgentProcess>,
}

impl Default for ProcessRegistry {
    fn default() -> Self {
        Self {
            version: 1,
            processes: Vec::new(),
        }
    }
}

static REGISTRY_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub(crate) fn agent_process_registry_path(data_dir: &Path) -> PathBuf {
    data_dir.join(AGENT_PROCESS_REGISTRY_RELATIVE_PATH)
}

pub(crate) fn register_session_process(
    data_dir: &Path,
    process: Arc<CliAgentProcess>,
    conversation_id: impl Into<String>,
    agent_type: AgentType,
    backend: Option<String>,
    command_preview: Option<String>,
) -> Result<(), AppError> {
    let pid = process.pid();
    let process_group_id = process.process_group_id();
    // Observe exit through the independent watch channel.  Keeping `process`
    // in this task would create a lifecycle cycle: a cancelled construction
    // drops its real owner, but this watcher remains an owner waiting for a
    // process that nobody can now stop.
    let mut exit_rx = process.exit_receiver();
    let entry = RegisteredAgentProcess {
        pid,
        process_group_id,
        conversation_id: conversation_id.into(),
        agent_type: agent_type.serde_name().to_owned(),
        backend,
        command_preview,
        registered_at_ms: now_ms(),
    };

    register_agent_process(data_dir, entry).map_err(|e| {
        AppError::Internal(format!(
            "Failed to register agent process {pid} in runtime registry: {e}"
        ))
    })?;

    let data_dir = data_dir.to_path_buf();
    tokio::spawn(async move {
        if exit_rx.borrow().is_running() {
            let _ = exit_rx.changed().await;
        }
        let terminal = exit_rx.borrow().clone();
        if let Some(error) = terminal.failure() {
            // Retain the durable entry when the exact platform watchdog/Job
            // could not prove tree cleanup. PID/PGID liveness probes are not a
            // substitute: Windows has no Unix group to probe, and on Unix a
            // recycled PID can turn polling into false authority.
            warn!(
                pid,
                process_group_id,
                error,
                "Retaining failed agent process registry entry because process-tree cleanup was not proven"
            );
            return;
        }
        if terminal.exit_status().is_none() {
            warn!(
                pid,
                process_group_id,
                "Retaining agent process registry entry because exit monitor ended without proof"
            );
            return;
        }
        if let Err(e) = unregister_agent_process(&data_dir, pid) {
            warn!(
                pid,
                path = %agent_process_registry_path(&data_dir).display(),
                error = %ErrorChain(&e),
                "Failed to unregister exited agent process from runtime registry"
            );
        }
    });

    Ok(())
}

fn register_agent_process(data_dir: &Path, entry: RegisteredAgentProcess) -> io::Result<()> {
    with_registry_lock(|| {
        let path = agent_process_registry_path(data_dir);
        let mut registry = read_registry_file(&path)?;
        registry.processes.retain(|existing| existing.pid != entry.pid);
        registry.processes.push(entry);
        write_registry_file(&path, &registry)
    })
}

pub(crate) fn unregister_agent_process(data_dir: &Path, pid: u32) -> io::Result<()> {
    with_registry_lock(|| {
        let path = agent_process_registry_path(data_dir);
        let mut registry = read_registry_file(&path)?;
        let original_len = registry.processes.len();
        registry.processes.retain(|existing| existing.pid != pid);
        if registry.processes.len() == original_len {
            return Ok(());
        }
        write_registry_file(&path, &registry)
    })
}

fn read_registry_file(path: &Path) -> io::Result<ProcessRegistry> {
    match fs::read_to_string(path) {
        Ok(contents) => serde_json::from_str(&contents).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Failed to parse process registry {}: {e}", path.display()),
            )
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(ProcessRegistry::default()),
        Err(e) => Err(e),
    }
}

fn write_registry_file(path: &Path, registry: &ProcessRegistry) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let tmp_path = path.with_extension("tmp");
    let payload = serde_json::to_vec_pretty(registry).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Failed to serialize process registry {}: {e}", path.display()),
        )
    })?;

    fs::write(&tmp_path, payload)?;
    if path.exists() {
        let _ = fs::remove_file(path);
    }
    fs::rename(tmp_path, path)?;
    Ok(())
}

fn with_registry_lock<T>(f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    let _guard = REGISTRY_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
    f()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_path_is_scoped_under_runtime_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = agent_process_registry_path(dir.path());
        assert_eq!(path, dir.path().join("runtime/agent-process-registry.json"));
    }

    #[test]
    fn unregister_is_idempotent_for_missing_pid() {
        let dir = tempfile::tempdir().unwrap();
        unregister_agent_process(dir.path(), 42).unwrap();
        let registry = read_registry_file(&agent_process_registry_path(dir.path())).unwrap();
        assert!(registry.processes.is_empty());
    }

    #[test]
    fn register_then_unregister_updates_registry_file() {
        let dir = tempfile::tempdir().unwrap();
        let entry = RegisteredAgentProcess {
            pid: 42,
            process_group_id: Some(42),
            conversation_id: "0190f5fe-7c00-7a00-8000-000000000211".into(),
            agent_type: AgentType::Acp.serde_name().into(),
            backend: Some("codex".into()),
            command_preview: Some("codex-acp".into()),
            registered_at_ms: 123,
        };

        register_agent_process(dir.path(), entry.clone()).unwrap();
        let path = agent_process_registry_path(dir.path());
        let registry = read_registry_file(&path).unwrap();
        assert_eq!(registry.processes, vec![entry]);

        unregister_agent_process(dir.path(), 42).unwrap();
        let registry = read_registry_file(&path).unwrap();
        assert!(registry.processes.is_empty());
    }
}
