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
use nomifun_db::{DbError, ICronRepository, SqliteCronRepository, UpdateCronJobParams};

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
