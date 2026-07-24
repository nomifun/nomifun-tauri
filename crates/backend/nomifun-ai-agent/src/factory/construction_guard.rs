use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use nomifun_common::{AppError, ErrorChain};
use tracing::{error, warn};

const INITIAL_TEARDOWN_RETRY_DELAY: Duration = Duration::from_millis(25);
const MAX_TEARDOWN_RETRY_DELAY: Duration = Duration::from_secs(2);

/// Owns a resource that has already crossed an external side-effect boundary
/// (a spawned process or an established remote connection) but has not yet
/// been published as a runtime.
///
/// Ordinary factory errors must use [`Self::teardown_before_error`]. That
/// method deliberately does not return until exact teardown succeeds, so the
/// registry keeps the uninitialised slot and workspace authority while cleanup
/// is unproven. The synchronous callback in `Drop` is only a panic / abruptly
/// dropped-future backstop; it is not accepted as teardown proof.
pub(crate) struct ConstructionGuard<T> {
    resource: Option<Arc<T>>,
    resource_name: &'static str,
    drop_cleanup: fn(&T) -> Result<(), AppError>,
}

impl<T> ConstructionGuard<T> {
    pub(crate) fn new(
        resource: Arc<T>,
        resource_name: &'static str,
        drop_cleanup: fn(&T) -> Result<(), AppError>,
    ) -> Self {
        Self {
            resource: Some(resource),
            resource_name,
            drop_cleanup,
        }
    }

    pub(crate) fn disarm(&mut self) {
        self.resource = None;
    }

    /// Await exact teardown before exposing the original construction error.
    ///
    /// A teardown failure is not converted into another factory `Err`: doing
    /// so would let the registry remove the uninitialised slot and admit a
    /// replacement while the old process/connection may still be active.
    /// Retrying under the still-pending factory future is the fail-closed
    /// boundary. Operators retain diagnostics for both the construction error
    /// and every cleanup failure.
    pub(crate) async fn teardown_before_error<F, Fut>(
        &mut self,
        construction_error: AppError,
        mut teardown: F,
    ) -> AppError
    where
        F: FnMut(Arc<T>) -> Fut,
        Fut: Future<Output = Result<(), AppError>>,
    {
        let Some(resource) = self.resource.as_ref().cloned() else {
            return construction_error;
        };
        let mut retry_delay = INITIAL_TEARDOWN_RETRY_DELAY;
        let mut attempt = 0_u64;

        loop {
            attempt = attempt.saturating_add(1);
            match teardown(Arc::clone(&resource)).await {
                Ok(()) => {
                    self.disarm();
                    return construction_error;
                }
                Err(cleanup_error) => {
                    if attempt == 1 {
                        error!(
                            resource = self.resource_name,
                            construction_error = %ErrorChain(&construction_error),
                            cleanup_error = %ErrorChain(&cleanup_error),
                            "Agent factory cleanup failed; retaining construction authority and retrying exact teardown"
                        );
                    } else {
                        warn!(
                            resource = self.resource_name,
                            attempt,
                            cleanup_error = %ErrorChain(&cleanup_error),
                            "Agent factory exact teardown retry failed"
                        );
                    }
                    tokio::time::sleep(retry_delay).await;
                    retry_delay = retry_delay
                        .checked_mul(2)
                        .unwrap_or(MAX_TEARDOWN_RETRY_DELAY)
                        .min(MAX_TEARDOWN_RETRY_DELAY);
                }
            }
        }
    }
}

impl<T> Drop for ConstructionGuard<T> {
    fn drop(&mut self) {
        if let Some(resource) = self.resource.take()
            && let Err(cleanup_error) = (self.drop_cleanup)(resource.as_ref())
        {
            error!(
                resource = self.resource_name,
                cleanup_error = %ErrorChain(&cleanup_error),
                "Agent construction guard panic fallback could not start teardown"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::sync::{Notify, Semaphore};

    use super::*;
    use crate::capability::cli_process::CliAgentProcess;
    use crate::capability::cli_process::tests::simple_script_config;

    struct TestResource {
        fallback_calls: AtomicUsize,
    }

    fn fallback(resource: &TestResource) -> Result<(), AppError> {
        resource.fallback_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    #[tokio::test]
    async fn factory_error_is_not_returned_until_exact_teardown_completes() {
        let resource = Arc::new(TestResource {
            fallback_calls: AtomicUsize::new(0),
        });
        let teardown_started = Arc::new(Notify::new());
        let teardown_release = Arc::new(Semaphore::new(0));
        let mut guard = ConstructionGuard::new(Arc::clone(&resource), "test runtime", fallback);
        let started = Arc::clone(&teardown_started);
        let release = Arc::clone(&teardown_release);

        let task = tokio::spawn(async move {
            guard
                .teardown_before_error(
                    AppError::Internal("simulated factory failure".to_owned()),
                    move |_resource| {
                        let started = Arc::clone(&started);
                        let release = Arc::clone(&release);
                        async move {
                            started.notify_one();
                            let permit = release.acquire().await.expect("release semaphore open");
                            permit.forget();
                            Ok(())
                        }
                    },
                )
                .await
        });

        teardown_started.notified().await;
        assert!(
            !task.is_finished(),
            "factory exposed Err before exact teardown completed"
        );
        teardown_release.add_permits(1);
        let error = task.await.expect("factory cleanup task");
        assert!(error.to_string().contains("simulated factory failure"));
        assert_eq!(
            resource.fallback_calls.load(Ordering::SeqCst),
            0,
            "ordinary error path must disarm the panic-only Drop fallback"
        );
    }

    #[tokio::test]
    async fn cleanup_failure_keeps_factory_authority_until_a_retry_proves_exit() {
        let resource = Arc::new(TestResource {
            fallback_calls: AtomicUsize::new(0),
        });
        let attempts = Arc::new(AtomicUsize::new(0));
        let second_attempt_started = Arc::new(Notify::new());
        let teardown_release = Arc::new(Semaphore::new(0));
        let mut guard = ConstructionGuard::new(Arc::clone(&resource), "test runtime", fallback);
        let observed_attempts = Arc::clone(&attempts);
        let second_started = Arc::clone(&second_attempt_started);
        let release = Arc::clone(&teardown_release);

        let task = tokio::spawn(async move {
            guard
                .teardown_before_error(
                    AppError::Internal("simulated factory failure".to_owned()),
                    move |_resource| {
                        let attempt = observed_attempts.fetch_add(1, Ordering::SeqCst) + 1;
                        let second_started = Arc::clone(&second_started);
                        let release = Arc::clone(&release);
                        async move {
                            if attempt == 1 {
                                return Err(AppError::Internal(
                                    "simulated cleanup uncertainty".to_owned(),
                                ));
                            }
                            second_started.notify_one();
                            let permit = release.acquire().await.expect("release semaphore open");
                            permit.forget();
                            Ok(())
                        }
                    },
                )
                .await
        });

        second_attempt_started.notified().await;
        assert!(
            !task.is_finished(),
            "cleanup failure must not release the factory slot as Err"
        );
        assert!(attempts.load(Ordering::SeqCst) >= 2);
        teardown_release.add_permits(1);
        let error = task.await.expect("factory cleanup task");
        assert!(error.to_string().contains("simulated factory failure"));
        assert_eq!(resource.fallback_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn post_spawn_factory_error_waits_for_real_process_tree_exit() {
        let data_dir = tempfile::tempdir().expect("temporary data directory");
        let process = Arc::new(
            CliAgentProcess::spawn_for_sdk(
                simple_script_config("sleep 60"),
                data_dir.path(),
            )
            .await
            .expect("spawn test ACP process"),
        );
        assert!(process.is_running());
        let mut guard = ConstructionGuard::new(
            Arc::clone(&process),
            "test ACP CLI process",
            CliAgentProcess::request_exact_tree_cleanup,
        );

        let error = guard
            .teardown_before_error(
                AppError::Internal("simulated post-spawn failure".to_owned()),
                |process| async move {
                    process
                        .kill(Duration::from_millis(50))
                        .await
                },
            )
            .await;

        assert!(error.to_string().contains("simulated post-spawn failure"));
        assert!(
            !process.is_running(),
            "factory Err became visible while the spawned process was alive"
        );
    }
}
