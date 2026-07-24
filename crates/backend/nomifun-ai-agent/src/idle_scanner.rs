use std::sync::Arc;
use std::time::Duration;

use nomifun_common::ErrorChain;
use tracing::{debug, error, info};

use crate::runtime_registry::AgentRuntimeRegistry;

/// Default idle timeout for ACP agents (5 minutes).
const DEFAULT_IDLE_TIMEOUT_SECS: i64 = 5 * 60;

/// Scan interval for idle agent cleanup (1 minute).
const SCAN_INTERVAL_SECS: u64 = 60;

/// Start the background idle agent scanner.
///
/// Periodically scans active runtimes and terminates ACP Agents that have been
/// idle (finished + no activity) beyond the configured threshold.
///
/// The scanner runs until the provided `shutdown` signal resolves.
pub fn start_idle_scanner(
    runtime_state_registry: Arc<dyn AgentRuntimeRegistry>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    idle_timeout_secs: Option<i64>,
    scan_interval_secs: Option<u64>,
) -> tokio::task::JoinHandle<()> {
    let threshold = idle_timeout_secs.unwrap_or(DEFAULT_IDLE_TIMEOUT_SECS);
    let scan_interval = scan_interval_secs.unwrap_or(SCAN_INTERVAL_SECS);
    info!(
        threshold_secs = threshold,
        scan_interval_secs = scan_interval,
        "Starting idle agent scanner"
    );

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(scan_interval));

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    scan_and_cleanup(&runtime_state_registry, threshold*1000);
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!("Idle scanner received shutdown signal");
                        break;
                    }
                }
            }
        }

        info!("Idle scanner stopped");
    })
}

/// Perform one scan: find idle runtimes and terminate them.
fn scan_and_cleanup(registry: &Arc<dyn AgentRuntimeRegistry>, threshold_ms: i64) {
    let idle_ids = registry.collect_idle_runtimes(threshold_ms);

    if idle_ids.is_empty() {
        debug!(active = registry.active_runtime_count(), "Idle scan: no idle Agents found");
        return;
    }

    info!(count = idle_ids.len(), "Idle scan: cleaning up idle agents");

    for id in idle_ids {
        let registry = Arc::clone(registry);
        tokio::spawn(async move {
            info!(conversation_id = %id, "Idle scan: awaiting idle Agent runtime shutdown");
            match registry
                .terminate_idle_runtime_if_eligible(&id, threshold_ms)
                .await
            {
                Ok(true) => {}
                Ok(false) => debug!(
                    conversation_id = %id,
                    "Idle scan: runtime became active or was replaced before cleanup"
                ),
                Err(teardown_error) => error!(
                    conversation_id = %id,
                    error = %ErrorChain(&teardown_error),
                    "Idle scan: Agent process-tree teardown failed; runtime remains quarantined"
                ),
            }
        });
    }
}
