use std::path::PathBuf;

use nomifun_api_types::AgentMetadata;
use nomifun_common::{AppError, CommandSpec, EnvVar, ErrorChain};
use tokio::fs;
use tracing::{info, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CodexSandboxMode {
    WorkspaceWrite,
    DangerFullAccess,
}

impl CodexSandboxMode {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CodexSandboxSyncOutcome {
    SkippedNonCodex,
    Synced(CodexSandboxMode),
    Failed(CodexSandboxMode),
}

pub(super) fn sandbox_mode_for_requested_mode(mode: Option<&str>) -> CodexSandboxMode {
    match mode.map(str::trim) {
        Some("full-access" | "yoloNoSandbox") => CodexSandboxMode::DangerFullAccess,
        _ => CodexSandboxMode::WorkspaceWrite,
    }
}

pub(super) async fn prepare_command_spec_for_agent(
    metadata: &AgentMetadata,
    requested_mode: Option<&str>,
    data_dir: &std::path::Path,
    command_spec: &mut CommandSpec,
) -> CodexSandboxSyncOutcome {
    let outcome = sync_managed_home_for_agent(metadata, requested_mode, data_dir).await;
    if matches!(outcome, CodexSandboxSyncOutcome::Synced(_)) {
        set_env(
            command_spec,
            "CODEX_HOME",
            managed_codex_home(data_dir).to_string_lossy().into_owned(),
        );
    }
    outcome
}

pub(super) async fn sync_managed_home_for_agent(
    metadata: &AgentMetadata,
    requested_mode: Option<&str>,
    data_dir: &std::path::Path,
) -> CodexSandboxSyncOutcome {
    if metadata.backend.as_deref() != Some("codex") {
        return CodexSandboxSyncOutcome::SkippedNonCodex;
    }

    let sandbox_mode = sandbox_mode_for_requested_mode(requested_mode);
    let managed_home = managed_codex_home(data_dir);
    let config_path = managed_home.join("config.toml");
    let outcome = match write_managed_codex_config(sandbox_mode, &config_path).await {
        Ok(()) => {
            info!(
                requested_mode = requested_mode.unwrap_or_default(),
                sandbox_mode = sandbox_mode.as_str(),
                codex_home = %managed_home.display(),
                "Codex ACP managed config synced"
            );
            CodexSandboxSyncOutcome::Synced(sandbox_mode)
        }
        Err(e) => {
            warn!(
                requested_mode = requested_mode.unwrap_or_default(),
                sandbox_mode = sandbox_mode.as_str(),
                codex_home = %managed_home.display(),
                error = %ErrorChain(&e),
                "Codex ACP managed config sync failed; continuing with existing Codex config"
            );
            CodexSandboxSyncOutcome::Failed(sandbox_mode)
        }
    };

    if matches!(outcome, CodexSandboxSyncOutcome::Synced(_)) {
        if let Err(e) = mirror_codex_auth_files(&managed_home).await {
            warn!(
                codex_home = %managed_home.display(),
                error = %ErrorChain(&e),
                "Codex ACP auth mirror failed; continuing with managed config"
            );
        }
    }

    outcome
}

async fn write_managed_codex_config(
    mode: CodexSandboxMode,
    path: &std::path::Path,
) -> Result<(), AppError> {
    let source_config_path = match source_codex_home() {
        Ok(home) => Some(home.join("config.toml")),
        Err(e) => {
            warn!(
                error = %ErrorChain(&e),
                "Codex ACP source config path resolution failed; using sandbox-only managed config"
            );
            None
        }
    };
    let source_config = read_source_codex_config(source_config_path.as_deref(), path).await;
    let rendered = render_managed_config(mode, source_config.as_deref());

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await.map_err(|e| {
            AppError::Internal(format!(
                "Failed to create managed Codex config directory: {}",
                ErrorChain(&e)
            ))
        })?;
    }

    fs::write(path, rendered).await.map_err(|e| {
        AppError::Internal(format!(
            "Failed to write managed Codex config: {}",
            ErrorChain(&e)
        ))
    })?;
    Ok(())
}

async fn read_source_codex_config(
    source: Option<&std::path::Path>,
    managed_path: &std::path::Path,
) -> Option<String> {
    let source = source?;
    if source == managed_path {
        return None;
    }

    match fs::read_to_string(source).await {
        Ok(content) => Some(content),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            warn!(
                source = %source.display(),
                error = %ErrorChain(&e),
                "Codex ACP source config read failed; using sandbox-only managed config"
            );
            None
        }
    }
}

fn managed_codex_home(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("codex-acp-home")
}

fn source_codex_home() -> Result<PathBuf, AppError> {
    if let Some(home) = std::env::var_os("CODEX_HOME")
        && !home.is_empty()
    {
        return Ok(PathBuf::from(home));
    }

    let home = dirs::home_dir().ok_or_else(|| {
        AppError::Internal("Failed to resolve home directory for Codex auth files".into())
    })?;
    Ok(home.join(".codex"))
}

async fn mirror_codex_auth_files(managed_home: &std::path::Path) -> Result<(), AppError> {
    let source_home = source_codex_home()?;
    for file_name in ["auth.json", ".env"] {
        mirror_codex_file(&source_home.join(file_name), &managed_home.join(file_name)).await?;
    }
    Ok(())
}

async fn mirror_codex_file(
    source: &std::path::Path,
    dest: &std::path::Path,
) -> Result<(), AppError> {
    if source == dest {
        return Ok(());
    }
    if !source.exists() {
        return Ok(());
    }

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).await.map_err(|e| {
            AppError::Internal(format!(
                "Failed to create managed Codex auth directory: {}",
                ErrorChain(&e)
            ))
        })?;
    }

    let _ = fs::remove_file(dest).await;
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(source, dest).map_err(|e| {
            AppError::Internal(format!(
                "Failed to symlink Codex auth file '{}': {}",
                source.display(),
                ErrorChain(&e)
            ))
        })?;
    }
    #[cfg(windows)]
    {
        fs::copy(source, dest).await.map_err(|e| {
            AppError::Internal(format!(
                "Failed to copy Codex auth file '{}': {}",
                source.display(),
                ErrorChain(&e)
            ))
        })?;
    }
    Ok(())
}

fn set_env(command_spec: &mut CommandSpec, name: &str, value: String) {
    command_spec.env.retain(|env| env.name != name);
    command_spec.env.push(EnvVar {
        name: name.to_owned(),
        value,
    });
}

fn render_managed_config(mode: CodexSandboxMode, source_config: Option<&str>) -> String {
    let Some(source_config) = source_config else {
        return render_managed_config_with_sandbox_mode(mode);
    };

    match render_managed_config_from_source(mode, source_config) {
        Ok(rendered) => rendered,
        Err(error) => {
            warn!(
                error,
                "Codex ACP source config parse failed; using sandbox-only managed config"
            );
            render_managed_config_with_sandbox_mode(mode)
        }
    }
}

fn render_managed_config_from_source(
    mode: CodexSandboxMode,
    source_config: &str,
) -> Result<String, String> {
    let mut doc: toml::Value = source_config
        .parse()
        .map_err(|e| format!("failed to parse source Codex config: {e}"))?;
    let root = doc
        .as_table_mut()
        .ok_or_else(|| "source Codex config root is not a table".to_owned())?;

    root.remove("service_tier");
    root.remove("priority");
    root.insert(
        "sandbox_mode".to_owned(),
        toml::Value::String(mode.as_str().to_owned()),
    );

    if mode == CodexSandboxMode::DangerFullAccess {
        ensure_windows_unelevated_sandbox_value(root);
    }

    toml::to_string_pretty(&doc)
        .map_err(|e| format!("failed to serialize managed Codex config: {e}"))
}

fn ensure_windows_unelevated_sandbox_value(root: &mut toml::map::Map<String, toml::Value>) {
    let mut windows_table = match root.remove("windows") {
        Some(toml::Value::Table(table)) => table,
        _ => toml::map::Map::new(),
    };
    windows_table.insert(
        "sandbox".to_owned(),
        toml::Value::String("unelevated".to_owned()),
    );
    root.insert("windows".to_owned(), toml::Value::Table(windows_table));
}

fn render_managed_config_with_sandbox_mode(mode: CodexSandboxMode) -> String {
    let newline = "\n";
    let content = format!("sandbox_mode = \"{}\"{newline}", mode.as_str());
    if mode == CodexSandboxMode::DangerFullAccess {
        ensure_windows_unelevated_sandbox(&content, newline)
    } else {
        content
    }
}

fn ensure_windows_unelevated_sandbox(content: &str, newline: &str) -> String {
    let sandbox_line = "sandbox = \"unelevated\"";
    let mut lines: Vec<String> = content.lines().map(ToOwned::to_owned).collect();
    let Some(windows_start) = lines.iter().position(|line| line.trim() == "[windows]") else {
        let mut rendered = content.trim_end().to_owned();
        if !rendered.is_empty() {
            rendered.push_str(newline);
            rendered.push_str(newline);
        }
        rendered.push_str("[windows]");
        rendered.push_str(newline);
        rendered.push_str(sandbox_line);
        rendered.push_str(newline);
        return rendered;
    };

    let windows_end = lines
        .iter()
        .enumerate()
        .skip(windows_start + 1)
        .find_map(|(index, line)| line.trim_start().starts_with('[').then_some(index))
        .unwrap_or(lines.len());

    if let Some(sandbox_index) = lines[windows_start + 1..windows_end]
        .iter()
        .position(|line| {
            line.trim_start()
                .strip_prefix("sandbox")
                .is_some_and(|rest| rest.trim_start().starts_with('='))
        })
        .map(|offset| windows_start + 1 + offset)
    {
        lines[sandbox_index] = sandbox_line.to_owned();
    } else {
        lines.insert(windows_start + 1, sandbox_line.to_owned());
    }

    let mut rendered = lines.join(newline);
    if content.ends_with('\n') {
        rendered.push_str(newline);
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;

    static CODEX_HOME_ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct CodexHomeEnvGuard {
        old: Option<std::ffi::OsString>,
    }

    impl CodexHomeEnvGuard {
        fn set(path: &std::path::Path) -> Self {
            let old = std::env::var_os("CODEX_HOME");
            unsafe {
                std::env::set_var("CODEX_HOME", path);
            }
            Self { old }
        }
    }

    impl Drop for CodexHomeEnvGuard {
        fn drop(&mut self) {
            unsafe {
                if let Some(old) = self.old.as_ref() {
                    std::env::set_var("CODEX_HOME", old);
                } else {
                    std::env::remove_var("CODEX_HOME");
                }
            }
        }
    }

    fn metadata_with_backend(backend: Option<&str>) -> AgentMetadata {
        AgentMetadata {
            id: "agent-1".into(),
            icon: None,
            name: "Codex CLI".into(),
            name_i18n: None,
            description: None,
            description_i18n: None,
            backend: backend.map(str::to_owned),
            agent_type: nomifun_common::AgentType::Acp,
            agent_source: nomifun_api_types::AgentSource::Builtin,
            agent_source_info: nomifun_api_types::AgentSourceInfo::default(),
            enabled: true,
            available: true,
            command: None,
            resolved_command: None,
            args: vec![],
            env: vec![],
            native_skills_dirs: None,
            behavior_policy: nomifun_api_types::BehaviorPolicy::default(),
            yolo_id: Some("full-access".into()),
            sort_order: 3110,
            handshake: nomifun_api_types::AgentHandshake::default(),
        }
    }

    #[test]
    fn full_access_maps_to_danger_full_access() {
        assert_eq!(
            sandbox_mode_for_requested_mode(Some("full-access")).as_str(),
            "danger-full-access"
        );
    }

    #[test]
    fn non_full_access_modes_map_to_workspace_write() {
        for mode in [
            None,
            Some(""),
            Some("auto"),
            Some("read-only"),
            Some("default"),
        ] {
            assert_eq!(
                sandbox_mode_for_requested_mode(mode).as_str(),
                "workspace-write"
            );
        }
    }

    #[test]
    fn managed_config_omits_user_codex_fields_that_old_acp_cannot_parse() {
        let rendered = render_managed_config_with_sandbox_mode(CodexSandboxMode::DangerFullAccess);

        assert!(rendered.contains(r#"sandbox_mode = "danger-full-access""#));
        assert!(rendered.contains("[windows]\nsandbox = \"unelevated\""));
        assert!(!rendered.contains("service_tier"));
        assert!(!rendered.contains("priority"));
    }

    #[tokio::test]
    async fn managed_config_preserves_user_model_provider_base_url() {
        let _env_lock = CODEX_HOME_ENV_MUTEX.lock().unwrap();
        let source_home = tempfile::tempdir().unwrap();
        let _codex_home_guard = CodexHomeEnvGuard::set(source_home.path());
        fs::write(
            source_home.path().join("config.toml"),
            r#"
model = "gpt-4o"
model_provider = "openai-compatible"
service_tier = "auto"
priority = 10

[model_providers.openai-compatible]
name = "OpenAI Compatible"
base_url = "https://llm.example.test/v1"
env_key = "OPENAI_API_KEY"
wire_api = "chat"
"#,
        )
        .await
        .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let mut spec = nomifun_common::CommandSpec {
            command: std::path::PathBuf::from("codex-acp"),
            args: vec![],
            env: vec![],
            cwd: None,
        };

        let outcome = prepare_command_spec_for_agent(
            &metadata_with_backend(Some("codex")),
            Some("full-access"),
            dir.path(),
            &mut spec,
        )
        .await;

        assert_eq!(
            outcome,
            CodexSandboxSyncOutcome::Synced(CodexSandboxMode::DangerFullAccess)
        );

        let rendered = fs::read_to_string(dir.path().join("codex-acp-home/config.toml"))
            .await
            .unwrap();
        assert!(rendered.contains(r#"sandbox_mode = "danger-full-access""#));
        assert!(rendered.contains(r#"model_provider = "openai-compatible""#));
        assert!(rendered.contains(r#"base_url = "https://llm.example.test/v1""#));
        assert!(rendered.contains(r#"env_key = "OPENAI_API_KEY""#));
        assert!(!rendered.contains("service_tier"));
        assert!(!rendered.contains("priority"));
    }

    #[tokio::test]
    async fn prepare_command_spec_uses_managed_codex_home() {
        let dir = tempfile::tempdir().unwrap();
        let mut spec = nomifun_common::CommandSpec {
            command: std::path::PathBuf::from("codex-acp"),
            args: vec![],
            env: vec![nomifun_common::EnvVar {
                name: "CODEX_HOME".into(),
                value: "/old/global/codex".into(),
            }],
            cwd: None,
        };

        let outcome = prepare_command_spec_for_agent(
            &metadata_with_backend(Some("codex")),
            Some("full-access"),
            dir.path(),
            &mut spec,
        )
        .await;

        let managed_home = dir.path().join("codex-acp-home");
        assert_eq!(
            outcome,
            CodexSandboxSyncOutcome::Synced(CodexSandboxMode::DangerFullAccess)
        );
        assert_eq!(
            spec.env.iter().filter(|e| e.name == "CODEX_HOME").count(),
            1,
            "CODEX_HOME should be replaced, not duplicated"
        );
        assert_eq!(
            spec.env
                .iter()
                .find(|e| e.name == "CODEX_HOME")
                .map(|e| e.value.as_str()),
            Some(managed_home.to_string_lossy().as_ref())
        );

        let rendered = fs::read_to_string(managed_home.join("config.toml"))
            .await
            .unwrap();
        assert!(rendered.contains(r#"sandbox_mode = "danger-full-access""#));
        assert!(!rendered.contains("service_tier"));
        assert!(!rendered.contains("priority"));
    }
}
