use std::str::FromStr;

use nomifun_api_types::{
    AgentExecution, AgentExecutionDetail, AgentExecutionEvent, ExecutionAttempt, ExecutionParticipant,
    ExecutionStep, ExecutionStepDependency, ParticipantConstraints,
};
use nomifun_common::{
    AgentExecutionActorType, AgentExecutionEventKind, AgentExecutionId, AgentStepMode, AppError,
    CompanionId, ConversationId, EntityId, ExecutionAttemptStatus, ExecutionStepKind,
    ExecutionStepStatus, MAX_AGENT_EXECUTION_PARALLELISM, ParticipantAssignmentSource, ProviderId,
    StepFailurePolicy, UserId, validate_uuidv7,
};
use nomifun_db::models::{
    AgentExecutionAttemptDetailRow, AgentExecutionDetailRows, AgentExecutionParticipantRow,
    AgentExecutionEventRow, AgentExecutionRow, AgentExecutionStepDependencyRow, AgentExecutionStepRow,
};
use serde::de::DeserializeOwned;

fn persisted_error(field: &str, error: impl std::fmt::Display) -> AppError {
    AppError::Internal(format!("invalid persisted Agent Execution {field}: {error}"))
}

fn parse_enum<T: FromStr>(field: &str, value: &str) -> Result<T, AppError>
where
    T::Err: std::fmt::Display,
{
    value.parse().map_err(|error| persisted_error(field, error))
}

fn require_id<T: EntityId>(field: &str, value: &str) -> Result<(), AppError> {
    value
        .parse::<T>()
        .map(|_| ())
        .map_err(|error| persisted_error(field, error))
}

fn require_optional_id<T: EntityId>(
    field: &str,
    value: Option<&str>,
) -> Result<(), AppError> {
    value.map(|value| require_id::<T>(field, value)).transpose()?;
    Ok(())
}

fn require_uuidv7(field: &str, value: &str) -> Result<(), AppError> {
    validate_uuidv7(value)
        .map(|_| ())
        .map_err(|error| persisted_error(field, error))
}

fn require_optional_uuidv7(field: &str, value: Option<&str>) -> Result<(), AppError> {
    value
        .map(|value| require_uuidv7(field, value))
        .transpose()?;
    Ok(())
}

pub(crate) fn event_kind(value: &str) -> Result<AgentExecutionEventKind, AppError> {
    parse_enum("event.event_type", value)
}

fn parse_json<T: DeserializeOwned>(field: &str, value: &str) -> Result<T, AppError> {
    serde_json::from_str(value).map_err(|error| persisted_error(field, error))
}

fn parse_optional_json<T: DeserializeOwned>(
    field: &str,
    value: Option<String>,
) -> Result<Option<T>, AppError> {
    value
        .map(|value| parse_json(field, &value))
        .transpose()
}

pub(crate) fn execution(
    row: AgentExecutionRow,
    lead_conversation_id: Option<String>,
) -> Result<AgentExecution, AppError> {
    require_id::<AgentExecutionId>("execution.execution_id", &row.execution_id)?;
    require_optional_id::<ConversationId>(
        "execution.lead_conversation_id",
        lead_conversation_id.as_deref(),
    )?;
    if !(1..=MAX_AGENT_EXECUTION_PARALLELISM).contains(&row.max_parallel) {
        return Err(persisted_error(
            "max_parallel",
            format!("must be between 1 and {MAX_AGENT_EXECUTION_PARALLELISM}"),
        ));
    }
    Ok(AgentExecution {
        execution_id: row.execution_id,
        goal: row.goal,
        lead_conversation_id,
        work_dir: row.work_dir,
        delegation_policy: parse_enum("delegation_policy", &row.delegation_policy)?,
        plan_gate: parse_enum("plan_gate", &row.plan_gate)?,
        adaptation_policy: parse_enum("adaptation_policy", &row.adaptation_policy)?,
        decision_policy: parse_enum("decision_policy", &row.decision_policy)?,
        max_parallel: row.max_parallel,
        status: parse_enum("status", &row.status)?,
        summary: row.summary,
        total_tokens: row.total_tokens,
        version: row.version,
        plan_revision: row.plan_revision,
        event_sequence: row.event_sequence,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

pub(crate) fn participant(
    row: AgentExecutionParticipantRow,
) -> Result<ExecutionParticipant, AppError> {
    require_uuidv7("participant.participant_id", &row.participant_id)?;
    require_id::<AgentExecutionId>("participant.execution_id", &row.execution_id)?;
    match (row.provider_id.as_deref(), row.model.as_deref()) {
        (Some(provider_id), Some(model))
            if ProviderId::try_from(provider_id).is_ok()
                && !model.is_empty()
                && model.trim() == model => {}
        (None, None) => {}
        _ => {
            return Err(persisted_error(
                "participant provider/model",
                "must be absent together or contain a canonical provider_id and trimmed model",
            ));
        }
    }
    let constraints: Option<ParticipantConstraints> =
        parse_optional_json("participant.constraints", row.constraints)?;
    if let Some(constraints) = constraints.as_ref() {
        constraints.validate().map_err(|error| {
            persisted_error("participant.constraints.max_concurrency", error)
        })?;
    }
    Ok(ExecutionParticipant {
        participant_id: row.participant_id,
        execution_id: row.execution_id,
        source_agent_id: row.source_agent_id,
        preset_id: row.preset_id,
        preset_revision: row.preset_revision,
        preset_snapshot: parse_optional_json("participant.preset_snapshot", row.preset_snapshot)?,
        provider_id: row.provider_id,
        model: row.model,
        role: row.role,
        capability: parse_optional_json("participant.capability", row.capability)?,
        constraints,
        description: row.description,
        system_prompt: row.system_prompt,
        enabled_skills: parse_json("participant.enabled_skills", &row.enabled_skills)?,
        disabled_builtin_skills: parse_json(
            "participant.disabled_builtin_skills",
            &row.disabled_builtin_skills,
        )?,
        sort_order: row.sort_order,
        introduced_in_revision: row.introduced_in_revision,
        retired_in_revision: row.retired_in_revision,
        created_at: row.created_at,
    })
}

pub(crate) fn step(row: AgentExecutionStepRow) -> Result<ExecutionStep, AppError> {
    require_uuidv7("step.step_id", &row.step_id)?;
    require_id::<AgentExecutionId>("step.execution_id", &row.execution_id)?;
    require_optional_uuidv7(
        "step.assigned_participant_id",
        row.assigned_participant_id.as_deref(),
    )?;
    Ok(ExecutionStep {
        step_id: row.step_id,
        execution_id: row.execution_id,
        title: row.title,
        spec: row.spec,
        profile: parse_optional_json("step.profile", row.profile)?,
        kind: parse_enum::<ExecutionStepKind>("step.kind", &row.kind)?,
        agent_mode: row
            .agent_mode
            .map(|value| parse_enum::<AgentStepMode>("step.agent_mode", &value))
            .transpose()?,
        status: parse_enum::<ExecutionStepStatus>("step.status", &row.status)?,
        tool_policy: parse_enum("step.tool_policy", &row.tool_policy)?,
        role: row.role,
        fanout_group: row.fanout_group,
        control_policy: parse_optional_json("step.control_policy", row.control_policy)?,
        failure_policy: parse_enum::<StepFailurePolicy>("step.failure_policy", &row.failure_policy)?,
        assigned_participant_id: row.assigned_participant_id,
        assignment_source: row
            .assignment_source
            .map(|value| {
                parse_enum::<ParticipantAssignmentSource>("step.assignment_source", &value)
            })
            .transpose()?,
        assignment_score: row.assignment_score,
        assignment_rationale: row.assignment_rationale,
        assignment_locked: row.assignment_locked,
        preset_prompt: row.preset_prompt,
        graph_x: row.graph_x,
        graph_y: row.graph_y,
        dispatch_after: row.dispatch_after,
        introduced_in_revision: row.introduced_in_revision,
        superseded_in_revision: row.superseded_in_revision,
        version: row.version,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

pub(crate) fn dependency(
    row: AgentExecutionStepDependencyRow,
) -> Result<ExecutionStepDependency, AppError> {
    require_id::<AgentExecutionId>("dependency.execution_id", &row.execution_id)?;
    require_uuidv7("dependency.blocker_step_id", &row.blocker_step_id)?;
    require_uuidv7("dependency.blocked_step_id", &row.blocked_step_id)?;
    Ok(ExecutionStepDependency {
        execution_id: row.execution_id,
        blocker_step_id: row.blocker_step_id,
        blocked_step_id: row.blocked_step_id,
        introduced_in_revision: row.introduced_in_revision,
        superseded_in_revision: row.superseded_in_revision,
    })
}

pub(crate) fn attempt(
    row: AgentExecutionAttemptDetailRow,
) -> Result<ExecutionAttempt, AppError> {
    let attempt = row.attempt;
    require_uuidv7("attempt.attempt_id", &attempt.attempt_id)?;
    require_id::<AgentExecutionId>("attempt.execution_id", &attempt.execution_id)?;
    require_uuidv7("attempt.step_id", &attempt.step_id)?;
    require_optional_uuidv7(
        "attempt.participant_id",
        attempt.participant_id.as_deref(),
    )?;
    require_optional_id::<ConversationId>(
        "attempt.conversation_id",
        row.conversation_id.as_deref(),
    )?;
    Ok(ExecutionAttempt {
        attempt_id: attempt.attempt_id,
        execution_id: attempt.execution_id,
        step_id: attempt.step_id,
        attempt_no: attempt.attempt_no,
        participant_id: attempt.participant_id,
        conversation_id: row.conversation_id,
        status: parse_enum::<ExecutionAttemptStatus>("attempt.status", &attempt.status)?,
        trigger_reason: attempt.trigger_reason,
        effective_config: parse_json("attempt.effective_config", &attempt.effective_config)?,
        question: attempt.question,
        error: attempt.error,
        output_summary: attempt.output_summary,
        output_files: parse_json("attempt.output_files", &attempt.output_files)?,
        tokens: attempt.tokens,
        retry_after: attempt.retry_after,
        runtime_state: parse_optional_json("attempt.runtime_state", attempt.runtime_state)?,
        started_at: attempt.started_at,
        finished_at: attempt.finished_at,
        version: attempt.version,
        created_at: attempt.created_at,
        updated_at: attempt.updated_at,
    })
}

pub(crate) fn detail(rows: AgentExecutionDetailRows) -> Result<AgentExecutionDetail, AppError> {
    Ok(AgentExecutionDetail {
        execution: execution(rows.execution, rows.lead_conversation_id)?,
        participants: rows
            .participants
            .into_iter()
            .map(participant)
            .collect::<Result<_, _>>()?,
        steps: rows.steps.into_iter().map(step).collect::<Result<_, _>>()?,
        dependencies: rows
            .dependencies
            .into_iter()
            .map(dependency)
            .collect::<Result<_, _>>()?,
        attempts: rows
            .attempts
            .into_iter()
            .map(attempt)
            .collect::<Result<_, _>>()?,
    })
}

pub(crate) fn event(row: AgentExecutionEventRow) -> Result<AgentExecutionEvent, AppError> {
    require_id::<AgentExecutionId>("event.execution_id", &row.execution_id)?;
    require_optional_uuidv7("event.step_id", row.step_id.as_deref())?;
    require_optional_uuidv7("event.attempt_id", row.attempt_id.as_deref())?;
    require_optional_id::<ConversationId>(
        "event.actor_conversation_id",
        row.actor_conversation_id.as_deref(),
    )?;
    require_optional_uuidv7(
        "event.actor_attempt_id",
        row.actor_attempt_id.as_deref(),
    )?;
    require_id::<UserId>("event.on_behalf_of_user_id", &row.on_behalf_of_user_id)?;
    let actor_type = parse_enum("event.actor_type", &row.actor_type)?;
    match actor_type {
        AgentExecutionActorType::System => {
            if row.actor_id.is_some()
                || row.actor_conversation_id.is_some()
                || row.actor_attempt_id.is_some()
            {
                return Err(persisted_error(
                    "event actor",
                    "system actors must not carry durable actor IDs",
                ));
            }
        }
        AgentExecutionActorType::User => {
            let actor_id = row
                .actor_id
                .as_deref()
                .ok_or_else(|| persisted_error("event.actor_id", "user actor ID is missing"))?;
            require_id::<UserId>("event.actor_id", actor_id)?;
            if row.actor_conversation_id.is_some() || row.actor_attempt_id.is_some() {
                return Err(persisted_error(
                    "event actor context",
                    "user actors must not carry Agent conversation/attempt IDs",
                ));
            }
        }
        AgentExecutionActorType::Agent => {
            let actor_id = row
                .actor_id
                .as_deref()
                .ok_or_else(|| persisted_error("event.actor_id", "Agent actor ID is missing"))?;
            if ConversationId::try_from(actor_id).is_err()
                && CompanionId::try_from(actor_id).is_err()
            {
                return Err(persisted_error(
                    "event.actor_id",
                    "expected a canonical ConversationId or CompanionId",
                ));
            }
            if row.actor_attempt_id.is_some() && row.actor_conversation_id.is_none() {
                return Err(persisted_error(
                    "event.actor_attempt_id",
                    "attempt context requires an Agent conversation",
                ));
            }
        }
    }
    Ok(AgentExecutionEvent {
        execution_id: row.execution_id,
        sequence: row.sequence,
        event_type: event_kind(&row.event_type)?,
        step_id: row.step_id,
        attempt_id: row.attempt_id,
        actor_type,
        actor_id: row.actor_id,
        actor_conversation_id: row.actor_conversation_id,
        actor_attempt_id: row.actor_attempt_id,
        on_behalf_of_user_id: row.on_behalf_of_user_id,
        payload: parse_json("event.payload", &row.payload)?,
        created_at: row.created_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event_row(event_type: &str) -> AgentExecutionEventRow {
        AgentExecutionEventRow {
            execution_id: "0190f5fe-7c00-7a00-8000-000000000001".to_owned(),
            sequence: 1,
            event_type: event_type.to_owned(),
            step_id: None,
            attempt_id: None,
            actor_type: "system".to_owned(),
            actor_id: None,
            actor_conversation_id: None,
            actor_attempt_id: None,
            on_behalf_of_user_id: "0190f5fe-7c00-7a00-8000-000000000002".to_owned(),
            payload: "{}".to_owned(),
            created_at: 1,
            published_at: None,
        }
    }

    #[test]
    fn persisted_event_kind_is_parsed_into_the_canonical_enum() {
        let mapped = event(event_row("step_changed")).unwrap();
        assert_eq!(mapped.event_type, AgentExecutionEventKind::StepChanged);
    }

    #[test]
    fn unknown_persisted_event_kind_fails_closed() {
        let error = event(event_row("worker_changed")).unwrap_err();
        assert!(error.to_string().contains("event.event_type"));
        assert!(error.to_string().contains("worker_changed"));
    }

    #[test]
    fn persisted_event_rejects_noncanonical_entity_ids() {
        let mut row = event_row("step_changed");
        row.execution_id = "execution_1".to_owned();
        let error = event(row).unwrap_err();
        assert!(error.to_string().contains("event.execution_id"));
    }
}
