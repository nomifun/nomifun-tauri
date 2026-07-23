use nomifun_common::{
    AdaptationPolicy, AgentExecutionEventKind, AgentExecutionStatus, AgentToolPolicy,
    ConversationId, DecisionPolicy, DelegationPolicy, ExecutionStepKind, ExecutionStepStatus,
    PlanGate, StepFailurePolicy,
};
use nomifun_db::models::ConversationRow;
use nomifun_db::{
    CreateAgentExecutionAttemptParams, CreateAgentExecutionParams,
    IAgentExecutionRepository, IConversationRepository, NewAgentExecutionEvent,
    NewAgentExecutionParticipant, NewAgentExecutionStep, NewAgentExecutionStepDependency,
    ReconcileAgentExecutionPlanParams, SqliteAgentExecutionRepository,
    SqliteConversationRepository,
};

const OWNER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000001";
const PROVIDER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000002";
const SOURCE_AGENT_ID: &str = "0190f5fe-7c00-7a00-8000-000000000114";

async fn database() -> nomifun_db::Database {
    let database = nomifun_db::init_database_memory_with_owner(
        nomifun_common::UserId::parse(OWNER_ID).unwrap(),
    )
    .await
    .unwrap();
    nomifun_db::sqlx::query(
        "INSERT INTO providers (\
            provider_id, platform, name, base_url, api_key_encrypted, models, enabled, \
            capabilities, created_at, updated_at\
         ) VALUES (?, 'openai', 'Fixture provider', 'https://example.invalid', \
                   'encrypted', '[]', 1, '[]', 1, 1)",
    )
    .bind(PROVIDER_ID)
    .execute(database.pool())
    .await
    .unwrap();
    database
}

fn event(kind: AgentExecutionEventKind) -> NewAgentExecutionEvent {
    NewAgentExecutionEvent {
        event_type: kind,
        step_id: None,
        attempt_id: None,
        actor: nomifun_common::AgentExecutionActor::system(),
        payload: "{}".to_owned(),
    }
}

fn participant(participant_id: impl Into<String>) -> NewAgentExecutionParticipant {
    NewAgentExecutionParticipant {
        participant_id: participant_id.into(),
        source_agent_id: SOURCE_AGENT_ID.to_owned(),
        preset_id: None,
        preset_revision: None,
        preset_snapshot: None,
        provider_id: Some(PROVIDER_ID.to_owned()),
        model: Some("model_test".to_owned()),
        role: Some("builder".to_owned()),
        capability: Some(r#"{"coding":true}"#.to_owned()),
        constraints: Some(r#"{"max_concurrency":1}"#.to_owned()),
        description: Some("repository fixture".to_owned()),
        system_prompt: None,
        enabled_skills: "[]".to_owned(),
        disabled_builtin_skills: "[]".to_owned(),
        sort_order: 0,
    }
}

fn step(
    step_id: impl Into<String>,
    assigned_participant_id: Option<String>,
    title: &str,
) -> NewAgentExecutionStep {
    NewAgentExecutionStep {
        step_id: step_id.into(),
        title: title.to_owned(),
        spec: format!("execute {title}"),
        role: Some("builder".to_owned()),
        tool_policy: AgentToolPolicy::Full,
        kind: ExecutionStepKind::Agent,
        agent_mode: Some(nomifun_common::AgentStepMode::Normal),
        profile: Some("{}".to_owned()),
        fanout_group: None,
        control_policy: None,
        status: ExecutionStepStatus::Pending,
        assigned_participant_id,
        assignment_score: Some(1.0),
        assignment_rationale: Some("fixture".to_owned()),
        assignment_source: Some(nomifun_common::ParticipantAssignmentSource::Planner),
        assignment_locked: false,
        failure_policy: StepFailurePolicy::FailExecution,
        preset_prompt: None,
        graph_x: None,
        graph_y: None,
    }
}

fn execution_params() -> CreateAgentExecutionParams {
    CreateAgentExecutionParams {
        goal: "verify v3 row identity separation".to_owned(),
        status: AgentExecutionStatus::Planning,
        plan_gate: PlanGate::Automatic,
        adaptation_policy: AdaptationPolicy::Fixed,
        decision_policy: DecisionPolicy::Automatic,
        delegation_policy: DelegationPolicy::Automatic,
        max_parallel: 2,
        work_dir: None,
        lead_conversation_id: None,
        initial_plan_input: r#"{"mode":"automatic"}"#.to_owned(),
    }
}

fn conversation_row() -> ConversationRow {
    let now = nomifun_common::now_ms();
    ConversationRow {
        id: 0,
        conversation_id: ConversationId::new().into_string(),
        user_id: OWNER_ID.to_owned(),
        name: "Agent execution fixture".to_owned(),
        r#type: "nomi".to_owned(),
        extra: "{}".to_owned(),
        delegation_policy: "automatic".to_owned(),
        execution_model_pool: None,
        decision_policy: "automatic".to_owned(),
        execution_template_id: None,
        model: None,
        status: Some("pending".to_owned()),
        source: Some("nomifun".to_owned()),
        channel_chat_id: None,
        pinned: false,
        pinned_at: None,
        cron_job_id: None,
        preset_id: None,
        preset_revision: None,
        preset_snapshot: None,
        created_at: now,
        updated_at: now,
    }
}

async fn create_execution(
    repository: &SqliteAgentExecutionRepository,
) -> nomifun_db::models::AgentExecutionRow {
    let participant_id = nomifun_common::generate_id();
    repository
        .create_execution_with_participants(
            OWNER_ID,
            &execution_params(),
            &[participant(participant_id)],
            &event(AgentExecutionEventKind::Created),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn agent_execution_rows_expose_business_uuidv7_identity() {
    let db = database().await;
    let repository = SqliteAgentExecutionRepository::new(db.pool().clone());
    let created = create_execution(&repository).await;

    assert!(nomifun_common::AgentExecutionId::parse(&created.execution_id).is_ok());
    assert_eq!(created.user_id, OWNER_ID);
    assert_eq!(created.goal, "verify v3 row identity separation");
    assert_eq!(created.status, "planning");

    let fetched = repository
        .get_execution(OWNER_ID, &created.execution_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fetched.execution_id, created.execution_id);
    assert_eq!(fetched.user_id, created.user_id);
    assert_eq!(fetched.goal, created.goal);
    assert_eq!(fetched.status, created.status);
    assert_eq!(fetched.version, created.version);
}

#[tokio::test]
async fn agent_execution_from_row_models_match_every_baseline_column() {
    let db = database().await;
    let repository = SqliteAgentExecutionRepository::new(db.pool().clone());
    let conversations = SqliteConversationRepository::new(db.pool().clone());
    let created = create_execution(&repository).await;
    let participant_id = repository
        .list_participants(OWNER_ID, &created.execution_id)
        .await
        .unwrap()
        .first()
        .unwrap()
        .participant_id
        .clone();
    let step_id = nomifun_common::generate_id();

    let detail = repository
        .reconcile_plan(
            OWNER_ID,
            &created.execution_id,
            created.version,
            &ReconcileAgentExecutionPlanParams {
                goal: None,
                plan_gate: None,
                adaptation_policy: None,
                decision_policy: None,
                delegation_policy: None,
                keep_step_ids: Vec::new(),
                new_participants: Vec::new(),
                retire_participant_ids: Vec::new(),
                new_steps: vec![step(
                    step_id.clone(),
                    Some(participant_id.clone()),
                    "compile",
                )],
                new_dependencies: Vec::new(),
                execution_status: AgentExecutionStatus::Running,
            },
            &event(AgentExecutionEventKind::PlanChanged),
        )
        .await
        .unwrap();

    assert_eq!(detail.participants.len(), 1);
    assert_eq!(detail.steps.len(), 1);
    assert!(nomifun_common::validate_uuidv7(&detail.participants[0].participant_id).is_ok());
    assert!(nomifun_common::validate_uuidv7(&detail.steps[0].step_id).is_ok());
    assert_eq!(detail.participants[0].participant_id, participant_id);
    assert_eq!(detail.steps[0].step_id, step_id);
    assert_eq!(detail.participants[0].execution_id, created.execution_id);
    assert_eq!(detail.steps[0].execution_id, created.execution_id);
    assert_eq!(
        detail.steps[0].assigned_participant_id.as_deref(),
        Some(detail.participants[0].participant_id.as_str())
    );

    let conversation = conversation_row();
    let conversation_id = conversation.conversation_id.clone();
    conversations.create(&conversation).await.unwrap();

    let queued = repository
        .create_attempt(
            OWNER_ID,
            &created.execution_id,
            &detail.steps[0].step_id,
            detail.steps[0].version,
            None,
            &CreateAgentExecutionAttemptParams {
                participant_id: Some(detail.participants[0].participant_id.clone()),
                start_immediately: false,
                trigger_reason: "initial".to_owned(),
                effective_config: "{}".to_owned(),
                retry_after: None,
                runtime_state: None,
            },
            &event(AgentExecutionEventKind::AttemptChanged),
        )
        .await
        .unwrap();
    let queued_attempt = queued.current_attempt.as_ref().unwrap().attempt.clone();
    assert!(nomifun_common::validate_uuidv7(&queued_attempt.attempt_id).is_ok());
    assert_eq!(queued_attempt.execution_id, created.execution_id);
    assert_eq!(queued_attempt.step_id, detail.steps[0].step_id);
    assert_eq!(
        queued_attempt.participant_id.as_deref(),
        Some(detail.participants[0].participant_id.as_str())
    );

    let running = repository
        .start_attempt(
            OWNER_ID,
            &created.execution_id,
            &detail.steps[0].step_id,
            queued.step.version,
            &queued_attempt.attempt_id,
            queued_attempt.version,
            &conversation_id,
            None,
            &event(AgentExecutionEventKind::AttemptChanged),
        )
        .await
        .unwrap();
    let running_attempt = running.current_attempt.unwrap().attempt;
    assert_eq!(running_attempt.attempt_id, queued_attempt.attempt_id);

    let links = repository
        .list_conversation_links(OWNER_ID, &created.execution_id)
        .await
        .unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].execution_id, created.execution_id);
    assert_eq!(links[0].relation, "attempt");
    assert!(links[0].active);
    assert_eq!(
        links[0].step_id.as_deref(),
        Some(detail.steps[0].step_id.as_str())
    );
    assert_eq!(
        links[0].attempt_id.as_deref(),
        Some(running_attempt.attempt_id.as_str())
    );

    let events = repository
        .list_events(OWNER_ID, &created.execution_id, 0, 100)
        .await
        .unwrap();
    assert!(events.len() >= 3);
    assert!(events.iter().all(|row| row.sequence > 0));
    assert!(events.iter().all(|row| row.execution_id == created.execution_id));
    assert!(events
        .iter()
        .filter_map(|row| row.step_id.as_deref())
        .all(|id| nomifun_common::validate_uuidv7(id).is_ok()));
    assert!(events
        .iter()
        .filter_map(|row| row.attempt_id.as_deref())
        .all(|id| nomifun_common::validate_uuidv7(id).is_ok()));
    assert!(events
        .iter()
        .any(|row| row.step_id.as_deref() == Some(detail.steps[0].step_id.as_str())));
    assert!(events
        .iter()
        .any(|row| row.attempt_id.as_deref() == Some(running_attempt.attempt_id.as_str())));
}

#[tokio::test]
async fn agent_execution_business_ids_are_uuidv7_and_dependencies_use_them() {
    let db = database().await;
    let repository = SqliteAgentExecutionRepository::new(db.pool().clone());
    let created = create_execution(&repository).await;
    let participant_id = repository
        .list_participants(OWNER_ID, &created.execution_id)
        .await
        .unwrap()[0]
        .participant_id
        .clone();
    let first_step_id = nomifun_common::generate_id();
    let second_step_id = nomifun_common::generate_id();

    let detail = repository
        .reconcile_plan(
            OWNER_ID,
            &created.execution_id,
            created.version,
            &ReconcileAgentExecutionPlanParams {
                goal: None,
                plan_gate: None,
                adaptation_policy: None,
                decision_policy: None,
                delegation_policy: None,
                keep_step_ids: Vec::new(),
                new_participants: Vec::new(),
                retire_participant_ids: Vec::new(),
                new_steps: vec![
                    step(
                        first_step_id.clone(),
                        Some(participant_id.clone()),
                        "first",
                    ),
                    step(
                        second_step_id.clone(),
                        Some(participant_id),
                        "second",
                    ),
                ],
                new_dependencies: vec![NewAgentExecutionStepDependency {
                    blocker_step_id: first_step_id.clone(),
                    blocked_step_id: second_step_id.clone(),
                }],
                execution_status: AgentExecutionStatus::Running,
            },
            &event(AgentExecutionEventKind::PlanChanged),
        )
        .await
        .unwrap();

    assert_eq!(detail.steps.len(), 2);
    assert_eq!(detail.dependencies.len(), 1);
    assert!(nomifun_common::validate_uuidv7(&detail.dependencies[0].blocker_step_id).is_ok());
    assert!(nomifun_common::validate_uuidv7(&detail.dependencies[0].blocked_step_id).is_ok());
    assert_eq!(detail.dependencies[0].execution_id, created.execution_id);
    assert_eq!(
        detail.dependencies[0].blocker_step_id,
        detail.steps
            .iter()
            .find(|row| row.title == "first")
            .unwrap()
            .step_id
    );
    assert_eq!(
        detail.dependencies[0].blocked_step_id,
        detail.steps
            .iter()
            .find(|row| row.title == "second")
            .unwrap()
            .step_id
    );

    let raw_columns: Vec<String> = nomifun_db::sqlx::query_scalar(
        "SELECT name FROM pragma_table_info('agent_execution_steps') ORDER BY cid",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert_eq!(
        raw_columns,
        vec![
            "id",
            "step_id",
            "execution_id",
            "title",
            "spec",
            "role",
            "tool_policy",
            "kind",
            "agent_mode",
            "profile",
            "fanout_group",
            "control_policy",
            "delegation_depth",
            "status",
            "assigned_participant_id",
            "assignment_score",
            "assignment_rationale",
            "assignment_source",
            "assignment_locked",
            "failure_policy",
            "preset_prompt",
            "graph_x",
            "graph_y",
            "dispatch_after",
            "version",
            "introduced_in_revision",
            "superseded_in_revision",
            "created_at",
            "updated_at",
        ]
    );
}
