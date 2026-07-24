use nomifun_common::{
    AdaptationPolicy, AgentExecutionEventKind, AgentExecutionStatus, AgentToolPolicy,
    ConversationId, DecisionPolicy, DelegationPolicy, ExecutionStepKind,
    ExecutionStepStatus, PlanGate, StepFailurePolicy,
};
use nomifun_db::models::ConversationRow;
use nomifun_db::{
    AgentExecutionAttemptRecoveryDisposition, AgentExecutionLeaseToken,
    AgentExecutionTurnAuthority, ConversationRowUpdate, CreateAgentExecutionAttemptParams,
    CreateAgentExecutionParams, IAgentExecutionRepository, IConversationRepository,
    NewAgentExecutionEvent, NewAgentExecutionParticipant, NewAgentExecutionStep,
    NewAgentExecutionStepDependency, ReconcileAgentExecutionPlanParams,
    SqliteAgentExecutionRepository, SqliteConversationRepository,
    TurnLifecycleTransition,
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

struct RunningAttemptFixture {
    pool: nomifun_db::sqlx::SqlitePool,
    execution_repo: SqliteAgentExecutionRepository,
    conversation_repo: SqliteConversationRepository,
    execution_id: String,
    step_id: String,
    attempt_id: String,
    conversation_id: String,
    step_version: i64,
    attempt_version: i64,
    lease: AgentExecutionLeaseToken,
    lease_expiry: i64,
}

async fn running_attempt_fixture() -> RunningAttemptFixture {
    let db = database().await;
    let pool = db.pool().clone();
    let execution_repo = SqliteAgentExecutionRepository::new(db.pool().clone());
    let conversation_repo = SqliteConversationRepository::new(db.pool().clone());
    let created = create_execution(&execution_repo).await;
    let participant_id = execution_repo
        .list_participants(OWNER_ID, &created.execution_id)
        .await
        .unwrap()[0]
        .participant_id
        .clone();
    let step_id = nomifun_common::generate_id();
    let planned = execution_repo
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
                    "recover",
                )],
                new_dependencies: Vec::new(),
                execution_status: AgentExecutionStatus::Running,
            },
            &event(AgentExecutionEventKind::PlanChanged),
        )
        .await
        .unwrap();
    let lease = AgentExecutionLeaseToken::new("fixture:original-generation".to_owned());
    let lease_expiry = nomifun_common::now_ms() + 120_000;
    execution_repo
        .try_acquire_lease(
            &created.execution_id,
            planned.execution.version,
            lease.owner(),
            lease_expiry,
        )
        .await
        .unwrap()
        .expect("original scheduler lease");
    let conversation = conversation_row();
    let conversation_id = conversation.conversation_id.clone();
    conversation_repo.create(&conversation).await.unwrap();
    let queued = execution_repo
        .create_attempt(
            OWNER_ID,
            &created.execution_id,
            &step_id,
            planned.steps[0].version,
            Some(&lease),
            &CreateAgentExecutionAttemptParams {
                participant_id: Some(participant_id),
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
    let queued_attempt = queued.current_attempt.as_ref().unwrap();
    let attempt_id = queued_attempt.attempt.attempt_id.clone();
    let running = execution_repo
        .start_attempt(
            OWNER_ID,
            &created.execution_id,
            &step_id,
            queued.step.version,
            &attempt_id,
            queued_attempt.attempt.version,
            &conversation_id,
            Some(&lease),
            &event(AgentExecutionEventKind::AttemptChanged),
        )
        .await
        .unwrap();
    let running_attempt = running.current_attempt.as_ref().unwrap();
    RunningAttemptFixture {
        pool,
        execution_repo,
        conversation_repo,
        execution_id: created.execution_id,
        step_id,
        attempt_id,
        conversation_id,
        step_version: running.step.version,
        attempt_version: running_attempt.attempt.version,
        lease,
        lease_expiry,
    }
}

fn turn_authority(fixture: &RunningAttemptFixture) -> AgentExecutionTurnAuthority {
    AgentExecutionTurnAuthority {
        execution_id: fixture.execution_id.clone(),
        step_id: fixture.step_id.clone(),
        attempt_id: fixture.attempt_id.clone(),
        expected_step_version: fixture.step_version,
        expected_attempt_version: fixture.attempt_version,
        lease_owner: fixture.lease.owner().to_owned(),
    }
}

fn turn_payload(authority: &AgentExecutionTurnAuthority) -> String {
    serde_json::json!({
        "delivery": {"content":"execute", "files":[], "inject_skills":[], "hidden":false},
        "agent_execution_authority": authority,
    })
    .to_string()
}

async fn replace_fixture_lease(
    fixture: &RunningAttemptFixture,
) -> AgentExecutionLeaseToken {
    fixture
        .execution_repo
        .release_lease(
            &fixture.execution_id,
            fixture.lease.owner(),
            fixture.lease_expiry,
        )
        .await
        .unwrap()
        .expect("release old generation");
    let execution = fixture
        .execution_repo
        .get_execution(OWNER_ID, &fixture.execution_id)
        .await
        .unwrap()
        .unwrap();
    let successor = AgentExecutionLeaseToken::new("fixture:successor-generation".to_owned());
    fixture
        .execution_repo
        .try_acquire_lease(
            &fixture.execution_id,
            execution.version,
            successor.owner(),
            nomifun_common::now_ms() + 120_000,
        )
        .await
        .unwrap()
        .expect("successor scheduler lease");
    successor
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

#[tokio::test]
async fn recovery_adopts_completed_initial_turn_receipt_without_rescheduling() {
    let fixture = running_attempt_fixture().await;
    let authority = turn_authority(&fixture);
    let payload = turn_payload(&authority);
    let operation_id = format!("{}:initial-turn", fixture.attempt_id);
    let claim = fixture
        .execution_repo
        .claim_attempt_turn_delivery_receipt(
            OWNER_ID,
            &fixture.conversation_id,
            &operation_id,
            &nomifun_common::generate_id(),
            "turn",
            &payload,
            &authority,
            0,
            nomifun_common::now_ms(),
        )
        .await
        .unwrap();
    assert!(claim.claimed_new);
    fixture
        .conversation_repo
        .complete_delivery_receipt(
            OWNER_ID,
            &fixture.conversation_id,
            &operation_id,
            true,
            Some("durable result"),
            None,
            nomifun_common::now_ms(),
        )
        .await
        .unwrap();
    let successor = replace_fixture_lease(&fixture).await;
    let recovered = fixture
        .execution_repo
        .reconcile_recovered_attempt(
            OWNER_ID,
            &fixture.execution_id,
            &fixture.step_id,
            fixture.step_version,
            &fixture.attempt_id,
            fixture.attempt_version,
            &successor,
            &event(AgentExecutionEventKind::AttemptChanged),
        )
        .await
        .unwrap();
    assert_eq!(
        recovered.disposition,
        AgentExecutionAttemptRecoveryDisposition::CompletedReceiptAdopted
    );
    assert_eq!(recovered.detail.step.status, "completed");
    let attempt = &recovered.detail.current_attempt.unwrap().attempt;
    assert_eq!(attempt.status, "completed");
    assert_eq!(attempt.output_summary.as_deref(), Some("durable result"));
    assert_ne!(recovered.detail.step.status, "pending");
}

#[tokio::test]
async fn recovery_parks_accepted_initial_turn_receipt_and_restart_cannot_reschedule() {
    let fixture = running_attempt_fixture().await;
    let authority = turn_authority(&fixture);
    let payload = turn_payload(&authority);
    let operation_id = format!("{}:initial-turn", fixture.attempt_id);
    fixture
        .execution_repo
        .claim_attempt_turn_delivery_receipt(
            OWNER_ID,
            &fixture.conversation_id,
            &operation_id,
            &nomifun_common::generate_id(),
            "turn",
            &payload,
            &authority,
            0,
            nomifun_common::now_ms(),
        )
        .await
        .unwrap();
    let successor = replace_fixture_lease(&fixture).await;
    let recovered = fixture
        .execution_repo
        .reconcile_recovered_attempt(
            OWNER_ID,
            &fixture.execution_id,
            &fixture.step_id,
            fixture.step_version,
            &fixture.attempt_id,
            fixture.attempt_version,
            &successor,
            &event(AgentExecutionEventKind::AttemptChanged),
        )
        .await
        .unwrap();
    assert_eq!(
        recovered.disposition,
        AgentExecutionAttemptRecoveryDisposition::ReviewBlocked
    );
    assert_eq!(recovered.detail.step.status, "waiting_input");
    let attempt = recovered.detail.current_attempt.unwrap().attempt;
    assert_eq!(attempt.status, "waiting_input");
    assert!(attempt.question.as_deref().unwrap().contains("accepted"));
    assert!(
        attempt
            .runtime_state
            .as_deref()
            .unwrap()
            .contains("\"review_blocked\"")
    );
    assert_ne!(recovered.detail.step.status, "pending");
}

#[tokio::test]
async fn recovery_parks_running_attempt_when_initial_turn_receipt_is_missing() {
    let fixture = running_attempt_fixture().await;
    let successor = replace_fixture_lease(&fixture).await;
    let recovered = fixture
        .execution_repo
        .reconcile_recovered_attempt(
            OWNER_ID,
            &fixture.execution_id,
            &fixture.step_id,
            fixture.step_version,
            &fixture.attempt_id,
            fixture.attempt_version,
            &successor,
            &event(AgentExecutionEventKind::AttemptChanged),
        )
        .await
        .unwrap();
    assert_eq!(
        recovered.disposition,
        AgentExecutionAttemptRecoveryDisposition::ReviewBlocked
    );
    assert_eq!(recovered.detail.step.status, "waiting_input");
    let attempt = recovered.detail.current_attempt.unwrap().attempt;
    assert_eq!(attempt.status, "waiting_input");
    assert!(attempt.question.as_deref().unwrap().contains("No terminal receipt"));
    assert_ne!(recovered.detail.step.status, "pending");
}

#[tokio::test]
async fn recovery_quarantines_exact_active_conversation_when_its_receipt_is_missing() {
    let fixture = running_attempt_fixture().await;
    let authority = turn_authority(&fixture);
    let payload = turn_payload(&authority);
    let operation_id = format!("{}:initial-turn", fixture.attempt_id);
    fixture
        .execution_repo
        .claim_attempt_turn_delivery_receipt(
            OWNER_ID,
            &fixture.conversation_id,
            &operation_id,
            &nomifun_common::generate_id(),
            "turn",
            &payload,
            &authority,
            0,
            nomifun_common::now_ms(),
        )
        .await
        .unwrap();
    // Production retains receipts indefinitely. This isolated fixture removes
    // the guard only to prove recovery quarantines a pre-existing corruption.
    nomifun_db::sqlx::query(
        "DROP TRIGGER trg_conversation_delivery_receipts_no_delete",
    )
    .execute(&fixture.pool)
    .await
    .unwrap();
    nomifun_db::sqlx::query(
        "DELETE FROM conversation_delivery_receipts WHERE operation_id = ?",
    )
    .bind(&operation_id)
    .execute(&fixture.pool)
    .await
    .unwrap();

    let successor = replace_fixture_lease(&fixture).await;
    let error = fixture
        .execution_repo
        .reconcile_recovered_attempt(
            OWNER_ID,
            &fixture.execution_id,
            &fixture.step_id,
            fixture.step_version,
            &fixture.attempt_id,
            fixture.attempt_version,
            &successor,
            &event(AgentExecutionEventKind::AttemptChanged),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            &error,
            nomifun_db::DbError::Conflict(message)
                if message.contains("lost its exact completed receipt")
        ),
        "an exact active generation without its receipt must be quarantined: {error}"
    );

    let state = fixture
        .conversation_repo
        .get_turn_admission_state(OWNER_ID, &fixture.conversation_id)
        .await
        .unwrap();
    assert_eq!(
        state.active_operation_id.as_deref(),
        Some(operation_id.as_str())
    );
    assert_eq!(
        fixture
            .conversation_repo
            .get(&fixture.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("running"),
        "quarantine must leave the durable generation intact for explicit repair"
    );
    let detail = fixture
        .execution_repo
        .get_step_detail(OWNER_ID, &fixture.execution_id, &fixture.step_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(detail.step.status, "running");
    assert_eq!(
        detail.current_attempt.unwrap().attempt.status,
        "running",
        "the failed reconciliation transaction must not partially park the attempt"
    );
}

#[tokio::test]
async fn pause_parks_running_missing_receipt_instead_of_returning_step_to_pending() {
    let fixture = running_attempt_fixture().await;
    let current = fixture
        .execution_repo
        .get_execution(OWNER_ID, &fixture.execution_id)
        .await
        .unwrap()
        .unwrap();
    let paused = fixture
        .execution_repo
        .pause_execution(
            OWNER_ID,
            &fixture.execution_id,
            current.version,
            &event(AgentExecutionEventKind::StatusChanged),
        )
        .await
        .unwrap();
    assert_eq!(paused.status, "paused");
    let detail = fixture
        .execution_repo
        .get_step_detail(OWNER_ID, &fixture.execution_id, &fixture.step_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(detail.step.status, "waiting_input");
    assert_eq!(
        detail.current_attempt.unwrap().attempt.status,
        "waiting_input"
    );
}

#[tokio::test]
async fn stale_lease_generation_cannot_late_claim_initial_turn_receipt() {
    let fixture = running_attempt_fixture().await;
    let stale_authority = turn_authority(&fixture);
    let payload = turn_payload(&stale_authority);
    let operation_id = format!("{}:initial-turn", fixture.attempt_id);
    let successor = replace_fixture_lease(&fixture).await;
    let rejected = fixture
        .execution_repo
        .claim_attempt_turn_delivery_receipt(
            OWNER_ID,
            &fixture.conversation_id,
            &operation_id,
            &nomifun_common::generate_id(),
            "turn",
            &payload,
            &stale_authority,
            0,
            nomifun_common::now_ms(),
        )
        .await;
    assert!(rejected.is_err());
    assert!(
        fixture
            .conversation_repo
            .get_delivery_receipt(OWNER_ID, &fixture.conversation_id, &operation_id)
            .await
            .unwrap()
            .is_none()
    );
    let recovered = fixture
        .execution_repo
        .reconcile_recovered_attempt(
            OWNER_ID,
            &fixture.execution_id,
            &fixture.step_id,
            fixture.step_version,
            &fixture.attempt_id,
            fixture.attempt_version,
            &successor,
            &event(AgentExecutionEventKind::AttemptChanged),
        )
        .await
        .unwrap();
    assert_eq!(
        recovered.disposition,
        AgentExecutionAttemptRecoveryDisposition::ReviewBlocked
    );
    assert_ne!(recovered.detail.step.status, "pending");
}

#[tokio::test]
async fn agent_execution_turn_admission_rejects_an_edit_resubmit_fence() {
    let fixture = running_attempt_fixture().await;
    let authority = turn_authority(&fixture);
    let payload = turn_payload(&authority);
    let operation_id = format!("{}:initial-turn", fixture.attempt_id);
    let row = fixture
        .conversation_repo
        .get(&fixture.conversation_id)
        .await
        .unwrap()
        .unwrap();
    let mut extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap();
    extra["_edit_resubmit_fence"] = serde_json::json!({
        "operation_id": "public-edit-resubmit:v1:competing-owner",
        "phase": "accepted",
    });
    fixture
        .conversation_repo
        .update(
            &fixture.conversation_id,
            &ConversationRowUpdate {
                extra: Some(extra.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert!(matches!(
        fixture
            .execution_repo
            .claim_attempt_turn_delivery_receipt(
                OWNER_ID,
                &fixture.conversation_id,
                &operation_id,
                &nomifun_common::generate_id(),
                "turn",
                &payload,
                &authority,
                0,
                nomifun_common::now_ms(),
            )
            .await,
        Err(nomifun_db::DbError::Conflict(_))
    ));
    assert!(
        fixture
            .conversation_repo
            .get_delivery_receipt(OWNER_ID, &fixture.conversation_id, &operation_id)
            .await
            .unwrap()
            .is_none(),
        "the rejected Agent admission must roll its receipt INSERT back"
    );
}

#[tokio::test]
async fn exact_candidate_abandon_settles_only_its_attempt_turn_generation() {
    let fixture = running_attempt_fixture().await;
    let authority = turn_authority(&fixture);
    let payload = turn_payload(&authority);
    let operation_id = format!("{}:initial-turn", fixture.attempt_id);
    let candidate_message_id = nomifun_common::generate_id();
    let claimed = fixture
        .execution_repo
        .claim_attempt_turn_delivery_receipt(
            OWNER_ID,
            &fixture.conversation_id,
            &operation_id,
            &candidate_message_id,
            "turn",
            &payload,
            &authority,
            0,
            nomifun_common::now_ms(),
        )
        .await
        .unwrap();
    assert!(claimed.claimed_new);
    let admitted = fixture
        .conversation_repo
        .get_turn_admission_state(OWNER_ID, &fixture.conversation_id)
        .await
        .unwrap();
    assert_eq!(admitted.epoch, 1);
    assert_eq!(
        admitted.active_operation_id.as_deref(),
        Some(operation_id.as_str())
    );

    let reason = "request future was dropped before the execution owner started";
    assert_eq!(
        fixture
            .execution_repo
            .abandon_exact_attempt_turn_admission(
                OWNER_ID,
                &fixture.conversation_id,
                &operation_id,
                &candidate_message_id,
                &payload,
                &authority,
                admitted.epoch,
                reason,
                nomifun_common::now_ms(),
            )
            .await
            .unwrap(),
        TurnLifecycleTransition::Committed
    );

    let receipt = fixture
        .conversation_repo
        .get_delivery_receipt(OWNER_ID, &fixture.conversation_id, &operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.message_id, candidate_message_id);
    assert_eq!(receipt.status, "completed");
    assert_eq!(receipt.result_ok, Some(false));
    assert_eq!(receipt.result_error.as_deref(), Some(reason));
    let finalized = fixture
        .conversation_repo
        .get_turn_admission_state(OWNER_ID, &fixture.conversation_id)
        .await
        .unwrap();
    assert_eq!(finalized.epoch, 2);
    assert!(finalized.active_operation_id.is_none());
    assert_eq!(
        fixture
            .conversation_repo
            .get(&fixture.conversation_id)
            .await
            .unwrap()
            .unwrap()
            .status
            .as_deref(),
        Some("finished")
    );

    assert_eq!(
        fixture
            .execution_repo
            .abandon_exact_attempt_turn_admission(
                OWNER_ID,
                &fixture.conversation_id,
                &operation_id,
                &candidate_message_id,
                &payload,
                &authority,
                admitted.epoch,
                reason,
                nomifun_common::now_ms(),
            )
            .await
            .unwrap(),
        TurnLifecycleTransition::AlreadyApplied
    );
    let replay = fixture
        .execution_repo
        .claim_attempt_turn_delivery_receipt(
            OWNER_ID,
            &fixture.conversation_id,
            &operation_id,
            &nomifun_common::generate_id(),
            "turn",
            &payload,
            &authority,
            finalized.epoch,
            nomifun_common::now_ms(),
        )
        .await
        .unwrap();
    assert!(!replay.claimed_new);
    assert_eq!(replay.receipt.message_id, candidate_message_id);
    assert_eq!(replay.receipt.status, "completed");
    assert_eq!(
        fixture
            .conversation_repo
            .get_turn_admission_state(OWNER_ID, &fixture.conversation_id)
            .await
            .unwrap(),
        finalized,
        "a terminal receipt replay must not reopen the finished Conversation"
    );
}

#[tokio::test]
async fn claim_loser_cannot_abandon_attempt_turn_winner() {
    let fixture = running_attempt_fixture().await;
    let authority = turn_authority(&fixture);
    let payload = turn_payload(&authority);
    let operation_id = format!("{}:initial-turn", fixture.attempt_id);
    let winner_candidate = nomifun_common::generate_id();
    fixture
        .execution_repo
        .claim_attempt_turn_delivery_receipt(
            OWNER_ID,
            &fixture.conversation_id,
            &operation_id,
            &winner_candidate,
            "turn",
            &payload,
            &authority,
            0,
            nomifun_common::now_ms(),
        )
        .await
        .unwrap();
    let admitted = fixture
        .conversation_repo
        .get_turn_admission_state(OWNER_ID, &fixture.conversation_id)
        .await
        .unwrap();
    let loser_candidate = nomifun_common::generate_id();
    let loser_claim = fixture
        .execution_repo
        .claim_attempt_turn_delivery_receipt(
            OWNER_ID,
            &fixture.conversation_id,
            &operation_id,
            &loser_candidate,
            "turn",
            &payload,
            &authority,
            admitted.epoch,
            nomifun_common::now_ms(),
        )
        .await
        .unwrap();
    assert!(!loser_claim.claimed_new);
    assert_eq!(loser_claim.receipt.message_id, winner_candidate);

    assert_eq!(
        fixture
            .execution_repo
            .abandon_exact_attempt_turn_admission(
                OWNER_ID,
                &fixture.conversation_id,
                &operation_id,
                &loser_candidate,
                &payload,
                &authority,
                admitted.epoch,
                "losing duplicate request was dropped",
                nomifun_common::now_ms(),
            )
            .await
            .unwrap(),
        TurnLifecycleTransition::Stale
    );

    let receipt = fixture
        .conversation_repo
        .get_delivery_receipt(OWNER_ID, &fixture.conversation_id, &operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.message_id, winner_candidate);
    assert_eq!(receipt.status, "accepted");
    let unchanged = fixture
        .conversation_repo
        .get_turn_admission_state(OWNER_ID, &fixture.conversation_id)
        .await
        .unwrap();
    assert_eq!(unchanged, admitted);
}

#[tokio::test]
async fn late_attempt_custodian_cannot_touch_successor_conversation_generation() {
    let fixture = running_attempt_fixture().await;
    let authority = turn_authority(&fixture);
    let payload = turn_payload(&authority);
    let operation_a = format!("{}:initial-turn:a", fixture.attempt_id);
    let candidate_a = nomifun_common::generate_id();
    fixture
        .execution_repo
        .claim_attempt_turn_delivery_receipt(
            OWNER_ID,
            &fixture.conversation_id,
            &operation_a,
            &candidate_a,
            "turn",
            &payload,
            &authority,
            0,
            nomifun_common::now_ms(),
        )
        .await
        .unwrap();
    let admitted_a = fixture
        .conversation_repo
        .get_turn_admission_state(OWNER_ID, &fixture.conversation_id)
        .await
        .unwrap();
    assert_eq!(
        fixture
            .conversation_repo
            .finalize_exact_cancelled_turn_generation(
                OWNER_ID,
                &fixture.conversation_id,
                admitted_a.epoch,
                Some(&operation_a),
                "generation A was cancelled",
                nomifun_common::now_ms(),
            )
            .await
            .unwrap(),
        TurnLifecycleTransition::Committed
    );

    let after_a = fixture
        .conversation_repo
        .get_turn_admission_state(OWNER_ID, &fixture.conversation_id)
        .await
        .unwrap();
    let operation_b = format!("{}:initial-turn:b", fixture.attempt_id);
    let candidate_b = nomifun_common::generate_id();
    fixture
        .execution_repo
        .claim_attempt_turn_delivery_receipt(
            OWNER_ID,
            &fixture.conversation_id,
            &operation_b,
            &candidate_b,
            "turn",
            &payload,
            &authority,
            after_a.epoch,
            nomifun_common::now_ms(),
        )
        .await
        .unwrap();
    let admitted_b = fixture
        .conversation_repo
        .get_turn_admission_state(OWNER_ID, &fixture.conversation_id)
        .await
        .unwrap();

    assert_eq!(
        fixture
            .execution_repo
            .abandon_exact_attempt_turn_admission(
                OWNER_ID,
                &fixture.conversation_id,
                &operation_a,
                &candidate_a,
                &payload,
                &authority,
                admitted_a.epoch,
                "late generation A custodian",
                nomifun_common::now_ms(),
            )
            .await
            .unwrap(),
        TurnLifecycleTransition::Stale
    );

    let unchanged_b = fixture
        .conversation_repo
        .get_turn_admission_state(OWNER_ID, &fixture.conversation_id)
        .await
        .unwrap();
    assert_eq!(unchanged_b, admitted_b);
    assert_eq!(
        unchanged_b.active_operation_id.as_deref(),
        Some(operation_b.as_str())
    );
    let receipt_b = fixture
        .conversation_repo
        .get_delivery_receipt(OWNER_ID, &fixture.conversation_id, &operation_b)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt_b.message_id, candidate_b);
    assert_eq!(receipt_b.status, "accepted");
}

#[tokio::test]
async fn stale_attempt_authority_cannot_abandon_exact_active_candidate() {
    let fixture = running_attempt_fixture().await;
    let stale_authority = turn_authority(&fixture);
    let payload = turn_payload(&stale_authority);
    let operation_id = format!("{}:initial-turn", fixture.attempt_id);
    let candidate_message_id = nomifun_common::generate_id();
    fixture
        .execution_repo
        .claim_attempt_turn_delivery_receipt(
            OWNER_ID,
            &fixture.conversation_id,
            &operation_id,
            &candidate_message_id,
            "turn",
            &payload,
            &stale_authority,
            0,
            nomifun_common::now_ms(),
        )
        .await
        .unwrap();
    let admitted = fixture
        .conversation_repo
        .get_turn_admission_state(OWNER_ID, &fixture.conversation_id)
        .await
        .unwrap();
    let _successor = replace_fixture_lease(&fixture).await;

    assert!(matches!(
        fixture
            .execution_repo
            .abandon_exact_attempt_turn_admission(
                OWNER_ID,
                &fixture.conversation_id,
                &operation_id,
                &candidate_message_id,
                &payload,
                &stale_authority,
                admitted.epoch,
                "stale scheduler tried to settle",
                nomifun_common::now_ms(),
            )
            .await,
        Err(nomifun_db::DbError::Conflict(_))
    ));

    let receipt = fixture
        .conversation_repo
        .get_delivery_receipt(OWNER_ID, &fixture.conversation_id, &operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.status, "accepted");
    assert_eq!(
        fixture
            .conversation_repo
            .get_turn_admission_state(OWNER_ID, &fixture.conversation_id)
            .await
            .unwrap(),
        admitted
    );
}

#[tokio::test]
async fn missing_nonactive_attempt_candidate_is_stale_after_writer_serialization() {
    let fixture = running_attempt_fixture().await;
    let authority = turn_authority(&fixture);
    let payload = turn_payload(&authority);
    let operation_id = format!("{}:never-committed-turn", fixture.attempt_id);
    let candidate_message_id = nomifun_common::generate_id();

    assert_eq!(
        fixture
            .execution_repo
            .abandon_exact_attempt_turn_admission(
                OWNER_ID,
                &fixture.conversation_id,
                &operation_id,
                &candidate_message_id,
                &payload,
                &authority,
                1,
                "claim transaction never committed",
                nomifun_common::now_ms(),
            )
            .await
            .unwrap(),
        TurnLifecycleTransition::Stale
    );
    assert!(
        fixture
            .conversation_repo
            .get_delivery_receipt(OWNER_ID, &fixture.conversation_id, &operation_id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        fixture
            .conversation_repo
            .get_turn_admission_state(OWNER_ID, &fixture.conversation_id)
            .await
            .unwrap()
            .epoch,
        0
    );
}
