//! Black-box integration tests for `ICronRepository`.
//!
//! Tests exercise the repository trait interface without knowledge of
//! the underlying SQLite implementation details.
//!
//! Covers test-plan items from Phase 12 test-plan:
//! - Section A (CRUD): CJ-1..CJ-12 (data-layer portion)
//! - Section C (Skill): SK-1..SK-7 (data-layer portion)
//! - Section D (Schedule Calculation): SC-1..SC-8 (data-layer portion)
//! - Section H (Cascade Delete): CD-1 (data-layer portion)

use std::sync::Arc;

use nomifun_common::now_ms;
use nomifun_db::models::{CronJobRow, CronJobRunRow};
use nomifun_db::{
    AdvanceCronOccurrenceParams, DbError, FinalizeCronRunOutcome, FinalizeCronRunParams,
    ICronRepository, ReserveCronRunParams, SettleCronRunParams, SqliteCronRepository,
    UpdateCronJobParams,
};

const INSTALLATION_OWNER: &str = "0190f5fe-7c00-7a00-8000-000000000001";
const FOREIGN_OWNER: &str = "0190f5fe-7c00-7a00-8000-000000000002";
const CONV_1: &str = "0190f5fe-7c00-7a00-8abc-012345678901";
const CONV_2: &str = "0190f5fe-7c00-7a00-8abc-012345678902";
const MISSING_CRON_JOB_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678999";

async fn init_database_memory() -> Result<nomifun_db::Database, nomifun_db::DbError> {
    nomifun_db::init_database_memory_with_owner(
        nomifun_common::UserId::parse(INSTALLATION_OWNER.to_owned())
            .expect("canonical fixture owner"),
    )
    .await
}

async fn repo() -> (Arc<dyn ICronRepository>, nomifun_db::Database) {
    let db = init_database_memory().await.unwrap();

    // This target exercises the complete Cron aggregate, including host-agent
    // and skill fields. Its fixture therefore belongs to the installation
    // owner; secondary model-only acceptance is covered by the authority tests.
    sqlx::query(
        "INSERT INTO conversations (conversation_id, user_id, name, type, created_at, updated_at) \
         VALUES (?1, ?2, 'Conv 1', 'acp', 0, 0)",
    )
    .bind(CONV_1)
    .bind(INSTALLATION_OWNER)
    .execute(db.pool())
    .await
    .unwrap();

    let r = Arc::new(SqliteCronRepository::new(db.pool().clone()));
    (r as Arc<dyn ICronRepository>, db)
}

fn make_job() -> CronJobRow {
    let now = now_ms();
    CronJobRow {
        id: 0,
        cron_job_id: nomifun_common::CronJobId::new().into_string(),
        user_id: INSTALLATION_OWNER.into(),
        name: "Test Job".into(),
        enabled: true,
        schedule_revision: 1,
        schedule_kind: "every".into(),
        schedule_value: "60000".into(),
        schedule_tz: None,
        schedule_description: Some("Every minute".into()),
        payload_message: "Run report".into(),
        execution_mode: "existing".into(),
        agent_config: None,
        preset_id: None,
        preset_revision: None,
        preset_snapshot: None,
        conversation_id: Some(CONV_1.to_owned()),
        conversation_title: Some("Conv 1".into()),
        agent_type: "acp".into(),
        created_by: "user".into(),
        skill_content: None,
        description: None,
        created_at: now,
        updated_at: now,
        next_run_at: Some(now + 60_000),
        last_run_at: None,
        last_status: None,
        last_error: None,
        run_count: 0,
        retry_count: 0,
        max_retries: 3,
    }
}

async fn reserve_scheduled_occurrence(
    repository: &dyn ICronRepository,
    cron_job_id: &str,
    schedule_revision: i64,
    planned_at_ms: i64,
) -> String {
    let cron_job_run_id = nomifun_common::CronJobRunId::new().into_string();
    let (_, inserted) = repository
        .reserve_run(
            INSTALLATION_OWNER,
            &ReserveCronRunParams {
                cron_job_run_id: cron_job_run_id.clone(),
                cron_job_id: cron_job_id.to_owned(),
                trigger_kind: "scheduled".to_owned(),
                operation_key: format!(
                    "cron:scheduled:{cron_job_id}:{schedule_revision}:{planned_at_ms}"
                ),
                request_fingerprint: format!(
                    "scheduled:v1:{cron_job_id}:{schedule_revision}:{planned_at_ms}"
                ),
                schedule_revision: Some(schedule_revision),
                planned_at_ms: Some(planned_at_ms),
                now: now_ms(),
            },
        )
        .await
        .unwrap();
    assert!(inserted, "fixture must reserve a fresh scheduled occurrence");
    cron_job_run_id
}

async fn settle_scheduled_occurrence(
    repository: &dyn ICronRepository,
    cron_job_run_id: &str,
) {
    assert!(
        repository
            .settle_run(
                INSTALLATION_OWNER,
                &SettleCronRunParams {
                    cron_job_run_id: cron_job_run_id.to_owned(),
                    status: "ok".to_owned(),
                    conversation_id: None,
                    result_error: None,
                    now: now_ms(),
                },
            )
            .await
            .unwrap(),
        "fixture must be the sole settlement leader"
    );
}

fn successful_run_projection(
    cron_job_run_id: &str,
    conversation_id: Option<&str>,
    now: i64,
) -> FinalizeCronRunParams {
    FinalizeCronRunParams {
        cron_job_run_id: cron_job_run_id.to_owned(),
        status: "ok".to_owned(),
        conversation_id: conversation_id.map(str::to_owned),
        result_error: None,
        now,
        last_run_at: Some(now),
        last_status: Some("ok".to_owned()),
        last_error: Some(None),
        increment_run_count: true,
        reset_retry_count: true,
        bind_job_conversation_if_unbound: false,
    }
}

// ── A. CRUD ──────────────────────────────────────────────────────────

#[tokio::test]
async fn cj1_insert_returns_all_fields() {
    let (r, _db) = repo().await;
    let job = make_job();
    let cron_job_id = job.cron_job_id.clone();
    r.insert(&job).await.unwrap();

    let found = r
        .get_by_cron_job_id(INSTALLATION_OWNER, &cron_job_id)
        .await
        .unwrap()
        .expect("found");
    assert!(found.id > 0);
    assert_eq!(found.cron_job_id, cron_job_id);
    assert_eq!(found.name, "Test Job");
    assert!(found.enabled);
    assert_eq!(found.schedule_kind, "every");
    assert_eq!(found.schedule_value, "60000");
    assert_eq!(found.payload_message, "Run report");
    assert_eq!(found.execution_mode, "existing");
    assert_eq!(found.conversation_id.as_deref(), Some(CONV_1));
    assert_eq!(found.agent_type, "acp");
    assert_eq!(found.created_by, "user");
    assert_eq!(found.run_count, 0);
    assert_eq!(found.retry_count, 0);
    assert_eq!(found.max_retries, 3);
}

#[tokio::test]
async fn repository_hides_crud_and_run_history_from_foreign_owners() {
    let (r, _db) = repo().await;
    let job = make_job();
    let job_id = job.cron_job_id.clone();
    r.insert(&job).await.unwrap();
    let run = CronJobRunRow {
        id: 0,
        cron_job_run_id: nomifun_common::CronJobRunId::new().into_string(),
        cron_job_id: job_id.clone(),
        executed_at_ms: 10,
        status: "ok".into(),
        created_at_ms: 10,
    };
    r.insert_run_pruned(INSTALLATION_OWNER, &run).await.unwrap();

    assert!(
        r.get_by_cron_job_id(FOREIGN_OWNER, &job_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(r.list_all(FOREIGN_OWNER).await.unwrap().is_empty());
    assert!(
        r.list_by_conversation(FOREIGN_OWNER, CONV_1)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        r.update(
            FOREIGN_OWNER,
            &job_id,
            &UpdateCronJobParams {
                name: Some("forged".into()),
                ..Default::default()
            },
        )
        .await,
        Err(DbError::NotFound(_))
    ));
    assert!(matches!(
        r.update(FOREIGN_OWNER, &job_id, &UpdateCronJobParams::default())
            .await,
        Err(DbError::NotFound(_))
    ));
    assert!(matches!(
        r.delete(FOREIGN_OWNER, &job_id).await,
        Err(DbError::NotFound(_))
    ));
    assert_eq!(
        r.delete_by_conversation(FOREIGN_OWNER, CONV_1)
            .await
            .unwrap(),
        0
    );
    assert!(
        r.get_by_cron_job_id(INSTALLATION_OWNER, &job_id)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        r.list_runs_by_job(FOREIGN_OWNER, &job_id, 7)
            .await
            .unwrap()
            .is_empty()
    );
    let forged_run = run;
    assert!(matches!(
        r.insert_run_pruned(FOREIGN_OWNER, &forged_run).await,
        Err(DbError::NotFound(_))
    ));

    let scheduler_row = r
        .get_by_cron_job_id_for_scheduler(&job_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(scheduler_row.user_id, INSTALLATION_OWNER);
    assert_eq!(
        r.list_runs_by_job(INSTALLATION_OWNER, &job_id, 7)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn cj2_three_schedule_kinds() {
    let (r, _db) = repo().await;

    let mut at_job = make_job();
    at_job.schedule_kind = "at".into();
    at_job.schedule_value = "1700000000000".into();
    let at_id = at_job.cron_job_id.clone();
    r.insert(&at_job).await.unwrap();

    let mut every_job = make_job();
    every_job.schedule_kind = "every".into();
    every_job.schedule_value = "60000".into();
    let every_id = every_job.cron_job_id.clone();
    r.insert(&every_job).await.unwrap();

    let mut cron_job = make_job();
    cron_job.schedule_kind = "cron".into();
    cron_job.schedule_value = "0 */5 * * * *".into();
    cron_job.schedule_tz = Some("Asia/Shanghai".into());
    let cron_id = cron_job.cron_job_id.clone();
    r.insert(&cron_job).await.unwrap();

    let at = r
        .get_by_cron_job_id(INSTALLATION_OWNER, &at_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(at.schedule_kind, "at");

    let every = r
        .get_by_cron_job_id(INSTALLATION_OWNER, &every_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(every.schedule_kind, "every");

    let cron = r
        .get_by_cron_job_id(INSTALLATION_OWNER, &cron_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cron.schedule_kind, "cron");
    assert_eq!(cron.schedule_tz.as_deref(), Some("Asia/Shanghai"));
}

#[tokio::test]
async fn cj4_get_by_cron_job_id_existing() {
    let (r, _db) = repo().await;
    let job = make_job();
    let cron_job_id = job.cron_job_id.clone();
    r.insert(&job).await.unwrap();
    let found = r
        .get_by_cron_job_id(INSTALLATION_OWNER, &cron_job_id)
        .await
        .unwrap();
    assert!(found.is_some());
}

#[tokio::test]
async fn cj5_get_by_cron_job_id_nonexistent() {
    let (r, _db) = repo().await;
    let found = r
        .get_by_cron_job_id(INSTALLATION_OWNER, MISSING_CRON_JOB_ID)
        .await
        .unwrap();
    assert!(found.is_none());
}

#[tokio::test]
async fn cj6_list_all() {
    let (r, _db) = repo().await;
    r.insert(&make_job()).await.unwrap();
    r.insert(&make_job()).await.unwrap();
    r.insert(&make_job()).await.unwrap();

    let all = r.list_all(INSTALLATION_OWNER).await.unwrap();
    assert!(all.len() >= 3);
}

#[tokio::test]
async fn cj7_list_by_conversation() {
    let (r, db) = repo().await;

    sqlx::query(
        "INSERT INTO conversations (conversation_id, user_id, name, type, created_at, updated_at) \
         VALUES (?1, ?2, 'Conv 2', 'acp', 0, 0)",
    )
    .bind(CONV_2)
    .bind(INSTALLATION_OWNER)
    .execute(db.pool())
    .await
    .unwrap();

    r.insert(&make_job()).await.unwrap();
    r.insert(&make_job()).await.unwrap();

    let mut other = make_job();
    other.conversation_id = Some(CONV_2.to_owned());
    r.insert(&other).await.unwrap();

    let conv1 = r.list_by_conversation(INSTALLATION_OWNER, CONV_1).await.unwrap();
    assert_eq!(conv1.len(), 2);

    let conv2 = r.list_by_conversation(INSTALLATION_OWNER, CONV_2).await.unwrap();
    assert_eq!(conv2.len(), 1);
}

#[tokio::test]
async fn cj8_update_name_and_enabled() {
    let (r, _db) = repo().await;
    let job = make_job();
    let cron_job_id = job.cron_job_id.clone();
    r.insert(&job).await.unwrap();

    let params = UpdateCronJobParams {
        name: Some("Renamed".into()),
        enabled: Some(false),
        ..Default::default()
    };
    r.update(INSTALLATION_OWNER, &cron_job_id, &params)
        .await
        .unwrap();

    let updated = r
        .get_by_cron_job_id(INSTALLATION_OWNER, &cron_job_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.name, "Renamed");
    assert!(!updated.enabled);
    assert!(updated.updated_at >= updated.created_at);
}

#[tokio::test]
async fn cj9_update_schedule_type() {
    let (r, _db) = repo().await;
    let job = make_job();
    let cron_job_id = job.cron_job_id.clone();
    r.insert(&job).await.unwrap();

    let params = UpdateCronJobParams {
        schedule_kind: Some("cron".into()),
        schedule_value: Some("0 0 9 * * *".into()),
        schedule_tz: Some(Some("UTC".into())),
        next_run_at: Some(Some(9999999)),
        ..Default::default()
    };
    r.update(INSTALLATION_OWNER, &cron_job_id, &params)
        .await
        .unwrap();

    let updated = r
        .get_by_cron_job_id(INSTALLATION_OWNER, &cron_job_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.schedule_kind, "cron");
    assert_eq!(updated.schedule_value, "0 0 9 * * *");
    assert_eq!(updated.schedule_tz.as_deref(), Some("UTC"));
    assert_eq!(updated.next_run_at, Some(9999999));
}

#[tokio::test]
async fn stale_expected_schedule_revision_update_preserves_newer_successor() {
    let (r, _db) = repo().await;
    let job = make_job();
    let cron_job_id = job.cron_job_id.clone();
    let successor_revision = job.schedule_revision + 1;
    let successor_at = job.next_run_at.expect("scheduled fixture") + 120_000;
    r.insert(&job).await.unwrap();

    r.update(
        INSTALLATION_OWNER,
        &cron_job_id,
        &UpdateCronJobParams {
            expected_schedule_revision: Some(job.schedule_revision),
            schedule_revision: Some(successor_revision),
            next_run_at: Some(Some(successor_at)),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let stale_result = r
        .update(
            INSTALLATION_OWNER,
            &cron_job_id,
            &UpdateCronJobParams {
                expected_schedule_revision: Some(job.schedule_revision),
                schedule_revision: Some(successor_revision),
                next_run_at: Some(Some(successor_at + 60_000)),
                ..Default::default()
            },
        )
        .await;
    assert!(
        matches!(stale_result, Err(DbError::Conflict(_))),
        "an update based on the superseded revision must fail closed"
    );

    let preserved = r
        .get_by_cron_job_id(INSTALLATION_OWNER, &cron_job_id)
        .await
        .unwrap()
        .expect("job remains present");
    assert_eq!(preserved.schedule_revision, successor_revision);
    assert_eq!(
        preserved.next_run_at,
        Some(successor_at),
        "the stale update must not overwrite the committed successor"
    );
}

#[tokio::test]
async fn cj10_update_nonexistent() {
    let (r, _db) = repo().await;
    let params = UpdateCronJobParams {
        name: Some("x".into()),
        ..Default::default()
    };
    let err = r
        .update(INSTALLATION_OWNER, MISSING_CRON_JOB_ID, &params)
        .await
        .unwrap_err();
    assert!(matches!(err, DbError::NotFound(_)));
}

#[tokio::test]
async fn cj11_delete() {
    let (r, _db) = repo().await;
    let job = make_job();
    let cron_job_id = job.cron_job_id.clone();
    r.insert(&job).await.unwrap();
    r.delete(INSTALLATION_OWNER, &cron_job_id).await.unwrap();

    let found = r
        .get_by_cron_job_id(INSTALLATION_OWNER, &cron_job_id)
        .await
        .unwrap();
    assert!(found.is_none());
}

#[tokio::test]
async fn cj12_delete_nonexistent() {
    let (r, _db) = repo().await;
    let err = r
        .delete(INSTALLATION_OWNER, MISSING_CRON_JOB_ID)
        .await
        .unwrap_err();
    assert!(matches!(err, DbError::NotFound(_)));
}

// ── List enabled ─────────────────────────────────────────────────────

#[tokio::test]
async fn list_enabled_filters_disabled_jobs() {
    let (r, _db) = repo().await;
    let enabled_job = make_job();
    let enabled_id = enabled_job.cron_job_id.clone();
    r.insert(&enabled_job).await.unwrap();

    let mut disabled = make_job();
    disabled.enabled = false;
    r.insert(&disabled).await.unwrap();

    let enabled = r.list_enabled_for_scheduler().await.unwrap();
    assert_eq!(enabled.len(), 1);
    assert!(enabled[0].id > 0);
    assert_eq!(enabled[0].cron_job_id, enabled_id);
}

// ── C. Skill (data layer) ────────────────────────────────────────────

#[tokio::test]
async fn sk1_save_skill_content() {
    let (r, _db) = repo().await;
    let job = make_job();
    let cron_job_id = job.cron_job_id.clone();
    r.insert(&job).await.unwrap();

    let params = UpdateCronJobParams {
        skill_content: Some(Some("---\nname: test\n---\nDo something".into())),
        ..Default::default()
    };
    r.update(INSTALLATION_OWNER, &cron_job_id, &params)
        .await
        .unwrap();

    let updated = r
        .get_by_cron_job_id(INSTALLATION_OWNER, &cron_job_id)
        .await
        .unwrap()
        .unwrap();
    assert!(updated.skill_content.is_some());
    assert!(updated.skill_content.unwrap().contains("Do something"));
}

#[tokio::test]
async fn sk2_has_skill_after_save() {
    let (r, _db) = repo().await;
    let mut job = make_job();
    job.skill_content = Some("---\nname: s\n---\ncontent".into());
    let cron_job_id = job.cron_job_id.clone();
    r.insert(&job).await.unwrap();

    let found = r
        .get_by_cron_job_id(INSTALLATION_OWNER, &cron_job_id)
        .await
        .unwrap()
        .unwrap();
    assert!(found.skill_content.is_some());
}

#[tokio::test]
async fn sk3_no_skill_by_default() {
    let (r, _db) = repo().await;
    let job = make_job();
    let cron_job_id = job.cron_job_id.clone();
    r.insert(&job).await.unwrap();

    let found = r
        .get_by_cron_job_id(INSTALLATION_OWNER, &cron_job_id)
        .await
        .unwrap()
        .unwrap();
    assert!(found.skill_content.is_none());
}

#[tokio::test]
async fn sk7_delete_clears_skill() {
    let (r, _db) = repo().await;
    let mut job = make_job();
    job.skill_content = Some("content".into());
    let cron_job_id = job.cron_job_id.clone();
    r.insert(&job).await.unwrap();

    r.delete(INSTALLATION_OWNER, &cron_job_id).await.unwrap();
    let found = r
        .get_by_cron_job_id(INSTALLATION_OWNER, &cron_job_id)
        .await
        .unwrap();
    assert!(found.is_none());
}

// ── H. Cascade delete (data layer) ──────────────────────────────────

#[tokio::test]
async fn cd1_delete_by_conversation_removes_all() {
    let (r, _db) = repo().await;
    r.insert(&make_job()).await.unwrap();
    r.insert(&make_job()).await.unwrap();

    let deleted = r.delete_by_conversation(INSTALLATION_OWNER, CONV_1).await.unwrap();
    assert_eq!(deleted, 2);

    let remaining = r.list_all(INSTALLATION_OWNER).await.unwrap();
    assert!(remaining.is_empty());
}

#[tokio::test]
async fn delete_by_conversation_no_match_returns_zero() {
    let (r, _db) = repo().await;
    let deleted = r
        .delete_by_conversation(INSTALLATION_OWNER, "0190f5fe-7c00-7a00-8abc-012345679999")
        .await
        .unwrap();
    assert_eq!(deleted, 0);
}

// ── Execution state tracking ────────────────────────────────────────

#[tokio::test]
async fn update_execution_state() {
    let (r, _db) = repo().await;
    let job = make_job();
    let cron_job_id = job.cron_job_id.clone();
    r.insert(&job).await.unwrap();

    let now = now_ms();
    let params = UpdateCronJobParams {
        last_run_at: Some(Some(now)),
        last_status: Some(Some("ok".into())),
        run_count: Some(1),
        retry_count: Some(0),
        next_run_at: Some(Some(now + 60_000)),
        ..Default::default()
    };
    r.update(INSTALLATION_OWNER, &cron_job_id, &params)
        .await
        .unwrap();

    let updated = r
        .get_by_cron_job_id(INSTALLATION_OWNER, &cron_job_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.last_run_at, Some(now));
    assert_eq!(updated.last_status.as_deref(), Some("ok"));
    assert_eq!(updated.run_count, 1);
    assert_eq!(updated.retry_count, 0);
}

#[tokio::test]
async fn update_error_state() {
    let (r, _db) = repo().await;
    let job = make_job();
    let cron_job_id = job.cron_job_id.clone();
    r.insert(&job).await.unwrap();

    let params = UpdateCronJobParams {
        last_status: Some(Some("error".into())),
        last_error: Some(Some("timeout after 30s".into())),
        retry_count: Some(1),
        ..Default::default()
    };
    r.update(INSTALLATION_OWNER, &cron_job_id, &params)
        .await
        .unwrap();

    let updated = r
        .get_by_cron_job_id(INSTALLATION_OWNER, &cron_job_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.last_status.as_deref(), Some("error"));
    assert_eq!(updated.last_error.as_deref(), Some("timeout after 30s"));
    assert_eq!(updated.retry_count, 1);
}

// ── Agent config JSON ───────────────────────────────────────────────

#[tokio::test]
async fn insert_and_retrieve_agent_config() {
    let (r, _db) = repo().await;
    let mut job = make_job();
    job.agent_config = Some(r#"{"backend":"openai","name":"GPT-4","model":"gpt-4","workspace":"/home/user"}"#.into());
    let cron_job_id = job.cron_job_id.clone();
    r.insert(&job).await.unwrap();

    let found = r
        .get_by_cron_job_id(INSTALLATION_OWNER, &cron_job_id)
        .await
        .unwrap()
        .unwrap();
    let config = found.agent_config.unwrap();
    assert!(config.contains("openai"));
    assert!(config.contains("gpt-4"));
}

// ── new_conversation execution mode ─────────────────────────────────

#[tokio::test]
async fn insert_new_conversation_mode() {
    let (r, _db) = repo().await;
    let mut job = make_job();
    job.execution_mode = "new_conversation".into();
    let cron_job_id = job.cron_job_id.clone();
    r.insert(&job).await.unwrap();

    let found = r
        .get_by_cron_job_id(INSTALLATION_OWNER, &cron_job_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.execution_mode, "new_conversation");
}

#[tokio::test]
async fn durable_scheduled_occurrence_has_one_non_prunable_identity() {
    let (r, db) = repo().await;
    let job = make_job();
    let job_id = job.cron_job_id.clone();
    let planned_at = job.next_run_at.expect("scheduled fixture");
    r.insert(&job).await.unwrap();
    let operation_key = format!("cron:scheduled:{job_id}:1:{planned_at}");
    let fingerprint = format!("scheduled:v1:{job_id}:1:{planned_at}");
    let first_run_id = nomifun_common::CronJobRunId::new().into_string();
    let params = ReserveCronRunParams {
        cron_job_run_id: first_run_id.clone(),
        cron_job_id: job_id.clone(),
        trigger_kind: "scheduled".to_owned(),
        operation_key: operation_key.clone(),
        request_fingerprint: fingerprint.clone(),
        schedule_revision: Some(1),
        planned_at_ms: Some(planned_at),
        now: now_ms(),
    };

    let (first, inserted) = r
        .reserve_run(INSTALLATION_OWNER, &params)
        .await
        .unwrap();
    assert!(inserted);
    assert_eq!(first.cron_job_run_id, first_run_id);

    let mut replay = params.clone();
    replay.cron_job_run_id = nomifun_common::CronJobRunId::new().into_string();
    let (same, inserted) = r
        .reserve_run(INSTALLATION_OWNER, &replay)
        .await
        .unwrap();
    assert!(!inserted);
    assert_eq!(same.cron_job_run_id, first_run_id);

    let mut conflicting = replay.clone();
    conflicting.operation_key.push_str(":different");
    assert!(matches!(
        r.reserve_run(INSTALLATION_OWNER, &conflicting).await,
        Err(DbError::Conflict(_))
    ));

    let attached = r
        .attach_run_conversation(
            INSTALLATION_OWNER,
            &first_run_id,
            CONV_1,
            now_ms(),
        )
        .await
        .unwrap();
    assert_eq!(attached.conversation_id.as_deref(), Some(CONV_1));
    assert!(
        r.settle_run(
            INSTALLATION_OWNER,
            &SettleCronRunParams {
                cron_job_run_id: first_run_id.clone(),
                status: "ok".to_owned(),
                conversation_id: Some(CONV_1.to_owned()),
                result_error: None,
                now: now_ms(),
            },
        )
        .await
        .unwrap()
    );
    assert!(
        !r.settle_run(
            INSTALLATION_OWNER,
            &SettleCronRunParams {
                cron_job_run_id: first_run_id.clone(),
                status: "ok".to_owned(),
                conversation_id: Some(CONV_1.to_owned()),
                result_error: None,
                now: now_ms() + 1,
            },
        )
        .await
        .unwrap()
    );
    assert_eq!(
        r.list_runs_by_job(INSTALLATION_OWNER, &job_id, 7)
            .await
            .unwrap()
            .len(),
        1
    );
    let durable_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM cron_run_reservations WHERE cron_job_run_id = ?",
    )
    .bind(&first_run_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(durable_count, 1);
}

#[tokio::test]
async fn exact_run_projection_is_applied_once_and_replay_is_absorbing() {
    let (r, _db) = repo().await;
    let job = make_job();
    let job_id = job.cron_job_id.clone();
    let planned_at = job.next_run_at.expect("scheduled fixture");
    r.insert(&job).await.unwrap();
    let run_id =
        reserve_scheduled_occurrence(r.as_ref(), &job_id, job.schedule_revision, planned_at)
            .await;
    let finalized_at = now_ms();
    let params = successful_run_projection(&run_id, job.conversation_id.as_deref(), finalized_at);

    assert_eq!(
        r.finalize_run_with_job_projection(INSTALLATION_OWNER, &params)
            .await
            .unwrap(),
        FinalizeCronRunOutcome::Applied
    );
    assert_eq!(
        r.finalize_run_with_job_projection(INSTALLATION_OWNER, &params)
            .await
            .unwrap(),
        FinalizeCronRunOutcome::AlreadyApplied
    );
    let mut contradictory = params.clone();
    contradictory.status = "error".to_owned();
    contradictory.result_error = Some("contradictory replay".to_owned());
    assert!(matches!(
        r.finalize_run_with_job_projection(INSTALLATION_OWNER, &contradictory)
            .await,
        Err(DbError::Conflict(_))
    ));

    let projected = r
        .get_by_cron_job_id(INSTALLATION_OWNER, &job_id)
        .await
        .unwrap()
        .expect("job remains present");
    assert_eq!(projected.run_count, 1);
    assert_eq!(projected.last_run_at, Some(finalized_at));
    assert_eq!(projected.last_status.as_deref(), Some("ok"));
    assert_eq!(
        r.list_runs_by_job(INSTALLATION_OWNER, &job_id, 7)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn concurrent_exact_run_finalizers_have_one_projection_leader() {
    let (r, db) = repo().await;
    let job = make_job();
    let job_id = job.cron_job_id.clone();
    let planned_at = job.next_run_at.expect("scheduled fixture");
    r.insert(&job).await.unwrap();
    let run_id =
        reserve_scheduled_occurrence(r.as_ref(), &job_id, job.schedule_revision, planned_at)
            .await;
    let params = successful_run_projection(&run_id, job.conversation_id.as_deref(), now_ms());
    let first = SqliteCronRepository::new(db.pool().clone());
    let second = SqliteCronRepository::new(db.pool().clone());

    let (left, right) = tokio::join!(
        first.finalize_run_with_job_projection(INSTALLATION_OWNER, &params),
        second.finalize_run_with_job_projection(INSTALLATION_OWNER, &params),
    );
    let mut outcomes = [left.unwrap(), right.unwrap()];
    outcomes.sort_by_key(|outcome| match outcome {
        FinalizeCronRunOutcome::Applied => 0,
        FinalizeCronRunOutcome::AlreadyApplied => 1,
        FinalizeCronRunOutcome::LegacyProjectionUnknown => 2,
    });
    assert_eq!(
        outcomes,
        [
            FinalizeCronRunOutcome::Applied,
            FinalizeCronRunOutcome::AlreadyApplied
        ]
    );

    let projected = r
        .get_by_cron_job_id(INSTALLATION_OWNER, &job_id)
        .await
        .unwrap()
        .expect("job remains present");
    assert_eq!(projected.run_count, 1);
    assert_eq!(
        r.list_runs_by_job(INSTALLATION_OWNER, &job_id, 7)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn exact_projection_failure_rolls_back_reservation_and_job_together() {
    let (r, db) = repo().await;
    let job = make_job();
    let job_id = job.cron_job_id.clone();
    let planned_at = job.next_run_at.expect("scheduled fixture");
    r.insert(&job).await.unwrap();
    let run_id =
        reserve_scheduled_occurrence(r.as_ref(), &job_id, job.schedule_revision, planned_at)
            .await;
    sqlx::query(
        "CREATE TRIGGER reject_cron_history_projection \
         BEFORE INSERT ON cron_job_runs \
         BEGIN SELECT RAISE(ABORT, 'injected cron projection failure'); END",
    )
    .execute(db.pool())
    .await
    .unwrap();

    let result = r
        .finalize_run_with_job_projection(
            INSTALLATION_OWNER,
            &successful_run_projection(&run_id, job.conversation_id.as_deref(), now_ms()),
        )
        .await;
    assert!(result.is_err(), "injected history failure must escape");

    let reservation: (String, String, Option<i64>) = sqlx::query_as(
        "SELECT status, job_projection_state, job_projected_at_ms \
         FROM cron_run_reservations WHERE cron_job_run_id = ?",
    )
    .bind(&run_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(reservation, ("reserved".into(), "pending".into(), None));
    let projected = r
        .get_by_cron_job_id(INSTALLATION_OWNER, &job_id)
        .await
        .unwrap()
        .expect("job remains present");
    assert_eq!(projected.run_count, 0);
    assert_eq!(projected.last_run_at, None);
    assert_eq!(projected.last_status, None);
}

#[tokio::test]
async fn reserved_legacy_projection_is_quarantined_without_guessing() {
    let (r, db) = repo().await;
    let job = make_job();
    let job_id = job.cron_job_id.clone();
    let planned_at = job.next_run_at.expect("scheduled fixture");
    r.insert(&job).await.unwrap();
    let run_id =
        reserve_scheduled_occurrence(r.as_ref(), &job_id, job.schedule_revision, planned_at)
            .await;
    sqlx::query(
        "UPDATE cron_run_reservations SET job_projection_state = 'legacy_unknown' \
         WHERE cron_job_run_id = ?",
    )
    .bind(&run_id)
    .execute(db.pool())
    .await
    .unwrap();

    assert_eq!(
        r.finalize_run_with_job_projection(
            INSTALLATION_OWNER,
            &successful_run_projection(&run_id, job.conversation_id.as_deref(), now_ms()),
        )
        .await
        .unwrap(),
        FinalizeCronRunOutcome::LegacyProjectionUnknown
    );
    let reservation: (String, String) = sqlx::query_as(
        "SELECT status, job_projection_state FROM cron_run_reservations \
         WHERE cron_job_run_id = ?",
    )
    .bind(&run_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(reservation, ("reserved".into(), "legacy_unknown".into()));
    let projected = r
        .get_by_cron_job_id(INSTALLATION_OWNER, &job_id)
        .await
        .unwrap()
        .expect("job remains present");
    assert_eq!(projected.run_count, 0);
    assert!(
        r.list_runs_by_job(INSTALLATION_OWNER, &job_id, 7)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn terminal_scheduled_occurrence_advances_exactly_once() {
    let (r, _db) = repo().await;
    let job = make_job();
    let job_id = job.cron_job_id.clone();
    let planned_at = job.next_run_at.expect("scheduled fixture");
    let successor_at = planned_at + 60_000;
    r.insert(&job).await.unwrap();

    let run_id =
        reserve_scheduled_occurrence(r.as_ref(), &job_id, job.schedule_revision, planned_at)
            .await;
    settle_scheduled_occurrence(r.as_ref(), &run_id).await;
    let advance = AdvanceCronOccurrenceParams {
        cron_job_run_id: run_id,
        cron_job_id: job_id.clone(),
        expected_schedule_revision: job.schedule_revision,
        expected_planned_at_ms: planned_at,
        next_run_at: Some(successor_at),
        disable: false,
        now: now_ms(),
    };

    let advanced = r
        .advance_scheduled_occurrence(INSTALLATION_OWNER, &advance)
        .await
        .unwrap()
        .expect("the exact terminal occurrence must win its first CAS");
    assert!(advanced.enabled);
    assert_eq!(advanced.schedule_revision, job.schedule_revision);
    assert_eq!(advanced.next_run_at, Some(successor_at));

    assert!(
        r.advance_scheduled_occurrence(INSTALLATION_OWNER, &advance)
            .await
            .unwrap()
            .is_none(),
        "a replay must be absorbing"
    );
    let preserved = r
        .get_by_cron_job_id(INSTALLATION_OWNER, &job_id)
        .await
        .unwrap()
        .expect("job remains present");
    assert!(preserved.enabled);
    assert_eq!(preserved.schedule_revision, job.schedule_revision);
    assert_eq!(
        preserved.next_run_at,
        Some(successor_at),
        "replay must not advance or otherwise mutate the successor"
    );
}

#[tokio::test]
async fn reserved_scheduled_occurrence_cannot_advance() {
    let (r, _db) = repo().await;
    let job = make_job();
    let job_id = job.cron_job_id.clone();
    let planned_at = job.next_run_at.expect("scheduled fixture");
    r.insert(&job).await.unwrap();

    let run_id =
        reserve_scheduled_occurrence(r.as_ref(), &job_id, job.schedule_revision, planned_at)
            .await;
    let result = r
        .advance_scheduled_occurrence(
            INSTALLATION_OWNER,
            &AdvanceCronOccurrenceParams {
                cron_job_run_id: run_id,
                cron_job_id: job_id.clone(),
                expected_schedule_revision: job.schedule_revision,
                expected_planned_at_ms: planned_at,
                next_run_at: Some(planned_at + 60_000),
                disable: false,
                now: now_ms(),
            },
        )
        .await
        .unwrap();
    assert!(
        result.is_none(),
        "a reservation must be terminal before it can authorize advancement"
    );

    let preserved = r
        .get_by_cron_job_id(INSTALLATION_OWNER, &job_id)
        .await
        .unwrap()
        .expect("job remains present");
    assert!(preserved.enabled);
    assert_eq!(preserved.schedule_revision, job.schedule_revision);
    assert_eq!(preserved.next_run_at, Some(planned_at));
}

#[tokio::test]
async fn stale_terminal_finalizer_cannot_overwrite_reconfigured_successor() {
    let (r, _db) = repo().await;
    let job = make_job();
    let job_id = job.cron_job_id.clone();
    let old_planned_at = job.next_run_at.expect("scheduled fixture");
    r.insert(&job).await.unwrap();

    let old_run_id = reserve_scheduled_occurrence(
        r.as_ref(),
        &job_id,
        job.schedule_revision,
        old_planned_at,
    )
    .await;
    settle_scheduled_occurrence(r.as_ref(), &old_run_id).await;

    let successor_revision = job.schedule_revision + 1;
    let successor_at = old_planned_at + 5 * 60_000;
    r.update(
        INSTALLATION_OWNER,
        &job_id,
        &UpdateCronJobParams {
            schedule_revision: Some(successor_revision),
            next_run_at: Some(Some(successor_at)),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert!(
        r.advance_scheduled_occurrence(
            INSTALLATION_OWNER,
            &AdvanceCronOccurrenceParams {
                cron_job_run_id: old_run_id,
                cron_job_id: job_id.clone(),
                expected_schedule_revision: job.schedule_revision,
                expected_planned_at_ms: old_planned_at,
                next_run_at: Some(old_planned_at + 60_000),
                disable: false,
                now: now_ms(),
            },
        )
        .await
        .unwrap()
        .is_none(),
        "the old finalizer must lose after a successor revision is committed"
    );
    let preserved = r
        .get_by_cron_job_id(INSTALLATION_OWNER, &job_id)
        .await
        .unwrap()
        .expect("job remains present");
    assert!(preserved.enabled);
    assert_eq!(preserved.schedule_revision, successor_revision);
    assert_eq!(
        preserved.next_run_at,
        Some(successor_at),
        "the stale finalizer must not replace the committed successor"
    );
}

#[tokio::test]
async fn scheduled_revision_and_run_now_operation_reuse_fail_closed() {
    let (r, _db) = repo().await;
    let job = make_job();
    let job_id = job.cron_job_id.clone();
    r.insert(&job).await.unwrap();
    let planned_at = now_ms() + 60_000;
    assert!(matches!(
        r.reserve_run(
            INSTALLATION_OWNER,
            &ReserveCronRunParams {
                cron_job_run_id: nomifun_common::CronJobRunId::new().into_string(),
                cron_job_id: job_id.clone(),
                trigger_kind: "scheduled".to_owned(),
                operation_key: format!("cron:scheduled:{job_id}:2:{planned_at}"),
                request_fingerprint: format!("scheduled:v1:{job_id}:2:{planned_at}"),
                schedule_revision: Some(2),
                planned_at_ms: Some(planned_at),
                now: now_ms(),
            },
        )
        .await,
        Err(DbError::Conflict(_))
    ));

    let operation_key = format!("cron:run-now:{INSTALLATION_OWNER}:http:retry-key");
    let first = ReserveCronRunParams {
        cron_job_run_id: nomifun_common::CronJobRunId::new().into_string(),
        cron_job_id: job_id.clone(),
        trigger_kind: "run_now".to_owned(),
        operation_key,
        request_fingerprint: format!("run-now:v1:{INSTALLATION_OWNER}:{job_id}"),
        schedule_revision: None,
        planned_at_ms: None,
        now: now_ms(),
    };
    assert!(
        r.reserve_run(INSTALLATION_OWNER, &first)
            .await
            .unwrap()
            .1
    );
    let mut mismatch = first.clone();
    mismatch.cron_job_run_id = nomifun_common::CronJobRunId::new().into_string();
    mismatch.request_fingerprint.push_str(":changed");
    assert!(matches!(
        r.reserve_run(INSTALLATION_OWNER, &mismatch).await,
        Err(DbError::Conflict(_))
    ));
}

#[tokio::test]
async fn fresh_scheduled_claim_requires_active_exact_next_but_replay_is_absorbing() {
    let (r, db) = repo().await;
    let job = make_job();
    let job_id = job.cron_job_id.clone();
    let planned_at = job.next_run_at.expect("scheduled fixture");
    r.insert(&job).await.unwrap();
    let run_id = nomifun_common::CronJobRunId::new().into_string();
    let params = ReserveCronRunParams {
        cron_job_run_id: run_id.clone(),
        cron_job_id: job_id.clone(),
        trigger_kind: "scheduled".to_owned(),
        operation_key: format!("cron:scheduled:{job_id}:1:{planned_at}"),
        request_fingerprint: format!("scheduled:v1:{job_id}:1:{planned_at}"),
        schedule_revision: Some(1),
        planned_at_ms: Some(planned_at),
        now: now_ms(),
    };
    assert!(
        r.reserve_run(INSTALLATION_OWNER, &params)
            .await
            .unwrap()
            .1
    );

    let replacement_at = planned_at + 60_000;
    r.update(
        INSTALLATION_OWNER,
        &job_id,
        &UpdateCronJobParams {
            enabled: Some(false),
            next_run_at: Some(Some(replacement_at)),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let mut replay = params.clone();
    replay.cron_job_run_id = nomifun_common::CronJobRunId::new().into_string();
    let (canonical, inserted) = r
        .reserve_run(INSTALLATION_OWNER, &replay)
        .await
        .unwrap();
    assert!(!inserted);
    assert_eq!(canonical.cron_job_run_id, run_id);

    let replacement = ReserveCronRunParams {
        cron_job_run_id: nomifun_common::CronJobRunId::new().into_string(),
        cron_job_id: job_id.clone(),
        trigger_kind: "scheduled".to_owned(),
        operation_key: format!("cron:scheduled:{job_id}:1:{replacement_at}"),
        request_fingerprint: format!("scheduled:v1:{job_id}:1:{replacement_at}"),
        schedule_revision: Some(1),
        planned_at_ms: Some(replacement_at),
        now: now_ms(),
    };
    assert!(matches!(
        r.reserve_run(INSTALLATION_OWNER, &replacement).await,
        Err(DbError::Conflict(_))
    ));
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM cron_run_reservations WHERE cron_job_id = ?",
    )
    .bind(&job_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(count, 1, "a disabled job cannot mint a fresh occurrence");
}

#[tokio::test]
async fn dual_file_repositories_choose_one_scheduled_claim_leader() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cron-dual-claim.db");
    let db_a = nomifun_db::init_database(&path).await.unwrap();
    let owner: String = sqlx::query_scalar(
        "SELECT owner_user_id FROM installation_identity WHERE singleton_key = 'installation'",
    )
    .fetch_one(db_a.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversations \
            (conversation_id, user_id, name, type, created_at, updated_at) \
         VALUES (?, ?, 'Cron dual claim', 'acp', 0, 0)",
    )
    .bind(CONV_1)
    .bind(&owner)
    .execute(db_a.pool())
    .await
    .unwrap();
    let db_b = nomifun_db::init_database(&path).await.unwrap();
    let repo_a = Arc::new(SqliteCronRepository::new(db_a.pool().clone()));
    let repo_b = Arc::new(SqliteCronRepository::new(db_b.pool().clone()));
    let mut job = make_job();
    job.user_id = owner.clone();
    let job_id = job.cron_job_id.clone();
    let planned_at = job.next_run_at.expect("scheduled fixture");
    repo_a.insert(&job).await.unwrap();

    let operation_key = format!("cron:scheduled:{job_id}:1:{planned_at}");
    let fingerprint = format!("scheduled:v1:{job_id}:1:{planned_at}");
    let make_params = || ReserveCronRunParams {
        cron_job_run_id: nomifun_common::CronJobRunId::new().into_string(),
        cron_job_id: job_id.clone(),
        trigger_kind: "scheduled".to_owned(),
        operation_key: operation_key.clone(),
        request_fingerprint: fingerprint.clone(),
        schedule_revision: Some(1),
        planned_at_ms: Some(planned_at),
        now: now_ms(),
    };
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let task_a = {
        let repo = Arc::clone(&repo_a);
        let barrier = Arc::clone(&barrier);
        let owner = owner.clone();
        let params = make_params();
        tokio::spawn(async move {
            barrier.wait().await;
            repo.reserve_run(&owner, &params).await
        })
    };
    let task_b = {
        let repo = Arc::clone(&repo_b);
        let barrier = Arc::clone(&barrier);
        let owner = owner.clone();
        let params = make_params();
        tokio::spawn(async move {
            barrier.wait().await;
            repo.reserve_run(&owner, &params).await
        })
    };
    barrier.wait().await;
    let result_a = task_a.await.unwrap().unwrap();
    let result_b = task_b.await.unwrap().unwrap();
    assert_ne!(result_a.1, result_b.1, "exactly one INSERT may lead");
    assert_eq!(
        result_a.0.cron_job_run_id, result_b.0.cron_job_run_id,
        "both repositories must observe the same durable occurrence"
    );

    drop(repo_a);
    drop(repo_b);
    db_a.close().await;
    db_b.close().await;
}

#[tokio::test]
async fn committed_reschedule_blocks_waiting_old_callback_claim() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cron-reschedule-race.db");
    let db_a = nomifun_db::init_database(&path).await.unwrap();
    let owner: String = sqlx::query_scalar(
        "SELECT owner_user_id FROM installation_identity WHERE singleton_key = 'installation'",
    )
    .fetch_one(db_a.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversations \
            (conversation_id, user_id, name, type, created_at, updated_at) \
         VALUES (?, ?, 'Cron reschedule race', 'acp', 0, 0)",
    )
    .bind(CONV_1)
    .bind(&owner)
    .execute(db_a.pool())
    .await
    .unwrap();
    let db_b = nomifun_db::init_database(&path).await.unwrap();
    let repo_a = Arc::new(SqliteCronRepository::new(db_a.pool().clone()));
    let repo_b = Arc::new(SqliteCronRepository::new(db_b.pool().clone()));
    let mut job = make_job();
    job.user_id = owner.clone();
    let job_id = job.cron_job_id.clone();
    let old_planned_at = job.next_run_at.expect("scheduled fixture");
    repo_a.insert(&job).await.unwrap();

    // Hold the SQLite writer while changing the authoritative occurrence.
    // The callback announces that it has been dispatched, then blocks inside
    // its INSERT..SELECT until the reschedule commits.
    let mut reschedule_tx = db_a.pool().begin().await.unwrap();
    let replacement_at = old_planned_at + 60_000;
    sqlx::query(
        "UPDATE cron_jobs SET next_run_at = ? \
         WHERE cron_job_id = ? AND user_id = ?",
    )
    .bind(replacement_at)
    .bind(&job_id)
    .bind(&owner)
    .execute(&mut *reschedule_tx)
    .await
    .unwrap();

    let (dispatched_tx, dispatched_rx) = tokio::sync::oneshot::channel();
    let callback = {
        let repo = Arc::clone(&repo_b);
        let owner = owner.clone();
        let job_id = job_id.clone();
        tokio::spawn(async move {
            let _ = dispatched_tx.send(());
            repo.reserve_run(
                &owner,
                &ReserveCronRunParams {
                    cron_job_run_id: nomifun_common::CronJobRunId::new().into_string(),
                    cron_job_id: job_id.clone(),
                    trigger_kind: "scheduled".to_owned(),
                    operation_key: format!("cron:scheduled:{job_id}:1:{old_planned_at}"),
                    request_fingerprint: format!(
                        "scheduled:v1:{job_id}:1:{old_planned_at}"
                    ),
                    schedule_revision: Some(1),
                    planned_at_ms: Some(old_planned_at),
                    now: now_ms(),
                },
            )
            .await
        })
    };
    dispatched_rx.await.unwrap();
    tokio::task::yield_now().await;
    reschedule_tx.commit().await.unwrap();
    assert!(matches!(
        callback.await.unwrap(),
        Err(DbError::Conflict(_))
    ));
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM cron_run_reservations WHERE cron_job_id = ?",
    )
    .bind(&job_id)
    .fetch_one(db_a.pool())
    .await
    .unwrap();
    assert_eq!(count, 0, "revoked planned_at must not gain a reservation");

    drop(repo_a);
    drop(repo_b);
    db_a.close().await;
    db_b.close().await;
}
