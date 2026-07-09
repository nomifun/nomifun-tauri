use nomifun_runtime::Builder as CmdBuilder;

use crate::error::ShellError;

#[async_trait::async_trait]
pub trait ISystemOpener: Send + Sync {
    fn open_detached(&self, target: &str) -> Result<(), ShellError>;
    /// Open `target` with a specific application (e.g. open a URL in a named
    /// browser). On Windows this is ShellExecute via the registered app, which
    /// avoids the `cmd /c start` window-title argument quirk.
    fn open_with_detached(&self, target: &str, app: &str) -> Result<(), ShellError>;
    async fn run_command(&self, program: &str, args: &[&str]) -> Result<(), ShellError>;
    fn is_tool_available(&self, tool_name: &str) -> bool;
}

pub struct DefaultSystemOpener;

#[async_trait::async_trait]
impl ISystemOpener for DefaultSystemOpener {
    fn open_detached(&self, target: &str) -> Result<(), ShellError> {
        open::that_detached(target).map_err(|e| ShellError::CommandFailed(format!("open: {e}")))?;
        Ok(())
    }

    fn open_with_detached(&self, target: &str, app: &str) -> Result<(), ShellError> {
        open::with_detached(target, app)
            .map_err(|e| ShellError::CommandFailed(format!("open {target:?} with {app:?}: {e}")))?;
        Ok(())
    }

    async fn run_command(&self, program: &str, args: &[&str]) -> Result<(), ShellError> {
        let mut builder = CmdBuilder::clean_cli(program);
        builder
            .args(args)
            // Everything launched here is handed off to the user (a terminal
            // window, an editor) and must survive this app exiting — keep it
            // out of the force-kill safety nets (Windows cleanup job / Linux
            // PDEATHSIG), which propagate to descendants like the opened
            // window.
            .hand_off()
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        let output = builder
            .spawn()
            .map_err(|e| ShellError::CommandFailed(format!("{program}: {e}")))?;

        let result = output
            .wait_with_output()
            .await
            .map_err(|e| ShellError::CommandFailed(format!("{program}: {e}")))?;

        if !result.status.success() {
            let stderr = String::from_utf8_lossy(&result.stderr);
            tracing::warn!(program, ?args, %stderr, "command exited with non-zero status");
            return Err(ShellError::CommandFailed(format!(
                "{program} exited with status {}: {}",
                result.status,
                stderr.trim()
            )));
        }
        Ok(())
    }

    fn is_tool_available(&self, tool_name: &str) -> bool {
        which::which(tool_name).is_ok()
    }
}

pub struct NoopSystemOpener;

#[async_trait::async_trait]
impl ISystemOpener for NoopSystemOpener {
    fn open_detached(&self, _target: &str) -> Result<(), ShellError> {
        Ok(())
    }

    fn open_with_detached(&self, _target: &str, _app: &str) -> Result<(), ShellError> {
        Ok(())
    }

    async fn run_command(&self, _program: &str, _args: &[&str]) -> Result<(), ShellError> {
        Ok(())
    }

    fn is_tool_available(&self, _tool_name: &str) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_opener_detects_nonexistent_tool() {
        let opener = DefaultSystemOpener;
        assert!(!opener.is_tool_available("__nonexistent_tool_xyz__"));
    }

    #[test]
    fn noop_opener_open_detached_succeeds() {
        let opener = NoopSystemOpener;
        assert!(opener.open_detached("https://example.com").is_ok());
    }

    #[tokio::test]
    async fn noop_opener_run_command_succeeds() {
        let opener = NoopSystemOpener;
        assert!(opener.run_command("fake-program", &["arg1"]).await.is_ok());
    }

    #[test]
    fn noop_opener_is_tool_available_always_true() {
        let opener = NoopSystemOpener;
        assert!(opener.is_tool_available("__nonexistent__"));
    }
}
