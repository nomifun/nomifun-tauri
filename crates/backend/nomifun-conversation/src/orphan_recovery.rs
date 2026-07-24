//! Restart-orphan policy for durable `running` Conversations.
//!
//! A missing process-local registry entry is only evidence that this process
//! no longer owns a runtime.  It is not evidence that work hosted by another
//! process or machine has stopped.  Keep this classification centralized so a
//! newly-added backend fails closed until its crash/parent-death contract has
//! been audited explicitly.

use nomifun_common::{AgentType, AppError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunningOrphanDisposition {
    /// Work may still be executing outside this application process.  A
    /// protocol-specific terminal proof is required before durable finalization.
    ExternalTerminalProofRequired,
}

pub(crate) fn running_orphan_disposition(
    persisted_agent_type: &str,
) -> Result<RunningOrphanDisposition, AppError> {
    let disposition = match persisted_agent_type {
        // Parent-death/Job/process-group containment can initiate teardown but
        // a replacement process cannot query the prior authority and prove
        // that its complete descendant tree is already empty. Nomi also has
        // direct MCP/tool child paths not yet uniformly contained. Remote and
        // OpenClaw may execute outside this process entirely. Therefore every
        // current backend fails closed after restart.
        value
            if value == AgentType::Nomi.serde_name()
                || value == AgentType::Acp.serde_name()
                || value == AgentType::Nanobot.serde_name()
                || value == AgentType::Remote.serde_name()
                || value == AgentType::OpenclawGateway.serde_name() =>
        {
            RunningOrphanDisposition::ExternalTerminalProofRequired
        }
        unknown => {
            return Err(AppError::Conflict(format!(
                "Conversation uses unknown Agent backend '{unknown}'; refusing to finalize an unproven running turn"
            )));
        }
    };
    Ok(disposition)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_current_backend_requires_queryable_terminal_proof_after_restart() {
        for backend in [
            AgentType::Nomi.serde_name(),
            AgentType::Acp.serde_name(),
            AgentType::Nanobot.serde_name(),
            AgentType::Remote.serde_name(),
            AgentType::OpenclawGateway.serde_name(),
        ] {
            assert_eq!(
                running_orphan_disposition(backend).unwrap(),
                RunningOrphanDisposition::ExternalTerminalProofRequired
            );
        }
    }

    #[test]
    fn unknown_backend_fails_closed() {
        assert!(matches!(
            running_orphan_disposition("future-backend"),
            Err(AppError::Conflict(_))
        ));
    }
}
