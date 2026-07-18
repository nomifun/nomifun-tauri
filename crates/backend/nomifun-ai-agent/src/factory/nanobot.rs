use std::sync::Arc;

use nomifun_common::{AgentType, AppError};

use crate::runtime_handle::AgentRuntimeHandle;
use crate::factory::AgentFactoryDeps;
use crate::factory::context::FactoryContext;
use crate::manager::nanobot::NanobotAgentManager;
use crate::types::AgentRuntimeBuildOptions;

/// The post-construction lifecycle step every Nanobot runtime must pass through.
///
/// Keeping this behind a tiny seam makes the factory invariant testable without
/// spawning a real CLI process.  The production implementation still invokes
/// the manager's real relay exactly once.
trait NanobotRelayActivation {
    fn activate_relay(agent: &Arc<Self>);
}

impl NanobotRelayActivation for NanobotAgentManager {
    fn activate_relay(agent: &Arc<Self>) {
        NanobotAgentManager::start_relay(agent);
    }
}

fn activate_nanobot_runtime<T: NanobotRelayActivation>(agent: Arc<T>) -> Arc<T> {
    T::activate_relay(&agent);
    agent
}

pub(super) async fn build(
    deps: Arc<AgentFactoryDeps>,
    _options: AgentRuntimeBuildOptions,
    ctx: FactoryContext,
) -> Result<AgentRuntimeHandle, AppError> {
    // Nanobot lives in the catalog as an internal row; reuse the
    // registry-resolved path instead of re-running `which()`.
    let cli_path = deps
        .agent_registry
        .list_by_agent_type(AgentType::Nanobot)
        .await
        .into_iter()
        .find_map(|m| m.resolved_command)
        .ok_or_else(|| AppError::BadRequest("Nanobot CLI not found in PATH".into()))?;
    let agent = NanobotAgentManager::new(
        ctx.conversation_id,
        ctx.workspace,
        cli_path,
        deps.data_dir.clone(),
    )
    .await?;
    // Construction consumes the process's pre-subscribed raw receiver.  The
    // relay must be started exactly once here; without it no Finish/Error ever
    // reached the conversation relay, leaving every Nanobot turn stuck.
    let agent = activate_nanobot_runtime(Arc::new(agent));
    Ok(AgentRuntimeHandle::Nanobot(agent))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct RelayProbe {
        activations: AtomicUsize,
    }

    impl NanobotRelayActivation for RelayProbe {
        fn activate_relay(agent: &Arc<Self>) {
            agent.activations.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn production_factory_activation_starts_relay_exactly_once() {
        let probe = Arc::new(RelayProbe {
            activations: AtomicUsize::new(0),
        });

        let activated = activate_nanobot_runtime(Arc::clone(&probe));

        assert!(Arc::ptr_eq(&activated, &probe));
        assert_eq!(probe.activations.load(Ordering::SeqCst), 1);
    }
}
