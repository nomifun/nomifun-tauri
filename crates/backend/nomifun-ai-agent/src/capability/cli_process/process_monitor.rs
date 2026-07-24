use std::process::ExitStatus;
use std::sync::Arc;

use nomi_process_runtime::{ChildProcessCleanup, kill_process_tree};
use nomifun_common::AppError;
use tokio::process::Child;
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{debug, error, info, warn};

/// Authoritative process-monitor state. A monitor/cleanup failure is terminal,
/// never another spelling of `Running`; otherwise callers can wait forever
/// after the sole child waiter has already gone away.
#[derive(Clone, Debug)]
pub(crate) enum ProcessExitState {
    Running,
    Exited(ExitStatus),
    Failed {
        status: Option<ExitStatus>,
        error: Arc<str>,
    },
}

impl ProcessExitState {
    pub(crate) fn is_running(&self) -> bool {
        matches!(self, Self::Running)
    }

    pub(crate) fn exit_status(&self) -> Option<ExitStatus> {
        match self {
            Self::Running => None,
            Self::Exited(status) => Some(*status),
            Self::Failed { status, .. } => *status,
        }
    }

    pub(crate) fn failure(&self) -> Option<&str> {
        match self {
            Self::Failed { error, .. } => Some(error),
            Self::Running | Self::Exited(_) => None,
        }
    }

    pub(crate) fn into_proven_exit(self) -> Result<ExitStatus, AppError> {
        match self {
            Self::Exited(status) => Ok(status),
            Self::Failed { error, .. } => Err(AppError::Internal(error.to_string())),
            Self::Running => Err(AppError::Internal(
                "Process exit proof was requested while the process was still running"
                    .to_owned(),
            )),
        }
    }
}

pub(super) struct ForceKillRequest {
    pub(super) completion: Option<oneshot::Sender<Result<(), String>>>,
}

pub(super) type ForceKillSender = mpsc::UnboundedSender<ForceKillRequest>;

/// Keep the exact `Child` owner in one task and accept lifecycle commands over
/// a non-blocking channel. `ChildProcessBuilder` registers a process group on
/// Unix and a suspended-before-resume Job Object on Windows, so this path can
/// terminate and prove cleanup of the whole tree without PID reuse races or a
/// localized/permission-sensitive `taskkill` subprocess.
pub(super) fn spawn_exit_monitor(
    mut child: Child,
    pid: u32,
    tree_cleanup: ChildProcessCleanup,
) -> (
    ForceKillSender,
    watch::Receiver<ProcessExitState>,
    tokio::task::JoinHandle<()>,
) {
    let (force_kill_tx, mut force_kill_rx) = mpsc::unbounded_channel::<ForceKillRequest>();
    let (exit_tx, exit_rx) = watch::channel(ProcessExitState::Running);
    let exit_handle = tokio::spawn(async move {
        let tree_cleanup = tree_cleanup.wait();
        tokio::pin!(tree_cleanup);
        tokio::select! {
            wait_result = child.wait() => {
                match wait_result {
                    Ok(status) => {
                        match tree_cleanup.as_mut().await {
                            Ok(()) => publish_exit(&exit_tx, pid, status),
                            Err(cleanup_error) => {
                                let error: Arc<str> = format!(
                                    "Process {pid} exited but process-tree cleanup was not proven: {cleanup_error}"
                                ).into();
                                error!(pid, %error, "CLI process natural-exit cleanup failed");
                                let _ = exit_tx.send(ProcessExitState::Failed {
                                    status: Some(status),
                                    error,
                                });
                            }
                        }
                    }
                    Err(wait_error) => {
                        let (status, cleanup_detail) = recover_after_wait_failure(&mut child, pid).await;
                        let platform_detail = match tree_cleanup.as_mut().await {
                            Ok(()) => "platform process-tree cleanup completed".to_owned(),
                            Err(error) => format!(
                                "platform process-tree cleanup was not proven: {error}"
                            ),
                        };
                        let error: Arc<str> = format!(
                            "Process {pid} exit monitor failed: {wait_error}; {cleanup_detail}; {platform_detail}"
                        )
                        .into();
                        error!(pid, %error, "CLI process monitor failed and ran exact-owner cleanup");
                        let _ = exit_tx.send(ProcessExitState::Failed { status, error });
                    }
                }
            }
            request = force_kill_rx.recv() => {
                let Some(request) = request else {
                    // The process wrapper normally sends a detached request in
                    // Drop before closing this channel. If construction itself
                    // is unwound, Child's kill-on-drop + registered Job/group
                    // remain the final ownership backstop.
                    return;
                };

                let kill_tree_result = kill_process_tree(&mut child).await;
                let cleanup_result = match kill_tree_result {
                    Ok(()) => {
                        debug!(pid, "Exact process tree terminated");
                        Ok(())
                    }
                    Err(tree_error) => {
                        // Never leave the root alive merely because whole-tree
                        // cleanup could not be proved. The exact Child handle is
                        // still authoritative and cannot suffer PID reuse.
                        let root_result = child.kill().await;
                        let message = match root_result {
                            Ok(()) => format!(
                                "Process {pid} tree cleanup failed ({tree_error}); exact root fallback completed"
                            ),
                            Err(root_error) => format!(
                                "Process {pid} tree cleanup failed ({tree_error}); exact root fallback also failed ({root_error})"
                            ),
                        };
                        error!(pid, %message, "CLI process tree cleanup was not proven");
                        Err(message)
                    }
                };

                let wait_result = child.wait().await;
                let platform_result = tree_cleanup.as_mut().await;
                let terminal_result = match (cleanup_result, wait_result, platform_result) {
                    (Ok(()), Ok(status), Ok(())) => {
                        publish_exit(&exit_tx, pid, status);
                        Ok(())
                    }
                    (cleanup_result, wait_result, platform_result) => {
                        let status = wait_result.as_ref().ok().copied();
                        let mut errors = Vec::new();
                        if let Err(error) = cleanup_result {
                            errors.push(error);
                        }
                        if let Err(error) = wait_result {
                            errors.push(format!(
                                "Process {pid} could not be reaped after tree termination: {error}"
                            ));
                        }
                        if let Err(error) = platform_result {
                            errors.push(format!(
                                "Process {pid} platform tree cleanup was not proven: {error}"
                            ));
                        }
                        let message = errors.join("; ");
                        let error: Arc<str> = message.clone().into();
                        let _ = exit_tx.send(ProcessExitState::Failed {
                            status,
                            error,
                        });
                        Err(message)
                    }
                };

                if let Some(completion) = request.completion {
                    let _ = completion.send(terminal_result);
                }
            }
        }
    });

    (force_kill_tx, exit_rx, exit_handle)
}

fn publish_exit(exit_tx: &watch::Sender<ProcessExitState>, pid: u32, status: ExitStatus) {
    info!(pid, ?status, "CLI process exited");
    let _ = exit_tx.send(ProcessExitState::Exited(status));
}

async fn recover_after_wait_failure(child: &mut Child, pid: u32) -> (Option<ExitStatus>, String) {
    match kill_process_tree(child).await {
        Ok(()) => (
            child.wait().await.ok(),
            "exact process-tree cleanup completed after monitor failure".to_owned(),
        ),
        Err(tree_error) => {
            warn!(pid, error = %tree_error, "Tree cleanup after process-monitor failure did not complete");
            let root_result = child.kill().await;
            let status = child.wait().await.ok();
            let detail = match root_result {
                Ok(()) => format!(
                    "tree cleanup failed ({tree_error}); exact root fallback completed"
                ),
                Err(root_error) => format!(
                    "tree cleanup failed ({tree_error}); exact root fallback also failed ({root_error})"
                ),
            };
            (status, detail)
        }
    }
}

pub(super) async fn wait_for_terminal_state(
    rx: &mut watch::Receiver<ProcessExitState>,
) -> ProcessExitState {
    loop {
        let current = rx.borrow().clone();
        if !current.is_running() {
            return current;
        }
        if rx.changed().await.is_err() {
            return ProcessExitState::Failed {
                status: None,
                error: Arc::from("CLI process exit monitor closed without publishing a terminal state"),
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn successful_status() -> ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(0)
    }

    #[cfg(windows)]
    fn successful_status() -> ExitStatus {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(0)
    }

    #[test]
    fn cleanup_failure_with_root_status_is_not_a_proven_exit() {
        let state = ProcessExitState::Failed {
            status: Some(successful_status()),
            error: Arc::from("simulated descendant cleanup failure"),
        };

        let error = state
            .into_proven_exit()
            .expect_err("root ExitStatus must not hide process-tree cleanup failure");
        assert!(error.to_string().contains("descendant cleanup failure"));
    }
}
