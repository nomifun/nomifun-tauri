use nomifun_db::{
    ChannelInboundClaim, IChannelRepository, IConversationRepository,
    SqliteChannelRepository, SqliteConversationRepository, init_database,
    init_database_memory, init_database_memory_with_owner,
};
use sqlx::Row;

const BASELINE: &str = include_str!("../migrations/001_v3_baseline.sql");
const IDMM_ACTION_RESERVATIONS: &str =
    include_str!("../migrations/002_idmm_action_reservations.sql");
const CHANNEL_INBOUND_RECEIPTS: &str =
    include_str!("../migrations/003_channel_inbound_receipts.sql");
const CRON_RUN_RESERVATIONS: &str =
    include_str!("../migrations/004_cron_run_reservations.sql");
const AUTOWORK_CLAIM_GENERATION: &str =
    include_str!("../migrations/005_autowork_claim_generation.sql");
const CONVERSATION_RECEIPT_PROJECTIONS: &str =
    include_str!("../migrations/006_conversation_receipt_projections.sql");
const TERMINAL_TURN_ADMISSIONS: &str =
    include_str!("../migrations/007_terminal_turn_admissions.sql");
const CONVERSATION_TURN_AUTHORITY: &str =
    include_str!("../migrations/008_conversation_turn_authority.sql");
const REQUIREMENT_CLAIM_CAPABILITIES: &str =
    include_str!("../migrations/009_requirement_claim_capabilities.sql");
const REQUIREMENT_PRE_EFFECT_ABANDON: &str =
    include_str!("../migrations/010_requirement_pre_effect_abandon.sql");
const CRON_RUN_JOB_PROJECTION: &str =
    include_str!("../migrations/011_cron_run_job_projection.sql");
const CONVERSATION_RECEIPT_LIFECYCLE: &str =
    include_str!("../migrations/012_conversation_receipt_lifecycle.sql");
const AUTOWORK_PROVENANCE_AUTHORITY_RECOVERY: &str =
    include_str!("../migrations/013_autowork_provenance_authority_recovery.sql");
const AUTOWORK_PROVENANCE_CONFLICT_NOTE: &str = "AutoWork did not start another turn because \
    durable Conversation state is ambiguous: AutoWork Requirement authority was revoked, \
    superseded, or targets another Conversation. Explicit reset or human review is required.";

fn executable_baseline_sql() -> String {
    BASELINE
        .lines()
        .filter(|line| !line.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn sql_tokens(sql: &str) -> Vec<String> {
    sql.split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
        .filter(|token| !token.is_empty())
        .map(str::to_ascii_uppercase)
        .collect()
}

async fn owner_id(pool: &sqlx::SqlitePool) -> String {
    sqlx::query_scalar(
        "SELECT owner_user_id FROM installation_identity \
         WHERE singleton_key = 'installation'",
    )
    .fetch_one(pool)
    .await
    .expect("installation owner")
}

#[tokio::test]
async fn provenance_authority_recovery_only_requeues_proven_pre_admission_failures() {
    let database_pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::raw_sql(&executable_baseline_sql())
        .execute(&database_pool)
        .await
        .unwrap();
    for migration in [
        IDMM_ACTION_RESERVATIONS,
        CHANNEL_INBOUND_RECEIPTS,
        CRON_RUN_RESERVATIONS,
        AUTOWORK_CLAIM_GENERATION,
        CONVERSATION_RECEIPT_PROJECTIONS,
        TERMINAL_TURN_ADMISSIONS,
        CONVERSATION_TURN_AUTHORITY,
        REQUIREMENT_CLAIM_CAPABILITIES,
        REQUIREMENT_PRE_EFFECT_ABANDON,
        CRON_RUN_JOB_PROJECTION,
        CONVERSATION_RECEIPT_LIFECYCLE,
    ] {
        sqlx::raw_sql(migration)
            .execute(&database_pool)
            .await
            .unwrap();
    }
    let pool = &database_pool;
    let owner = nomifun_common::UserId::new().into_string();
    sqlx::query(
        "INSERT INTO users \
         (user_id, username, password_hash, created_at, updated_at) \
         VALUES (?, 'recovery-owner', 'hash', 1, 1)",
    )
    .bind(&owner)
    .execute(pool)
    .await
    .unwrap();
    let enabled_conversation = nomifun_common::ConversationId::new();
    let disabled_conversation = nomifun_common::ConversationId::new();

    for (conversation_id, enabled) in [
        (&enabled_conversation, true),
        (&disabled_conversation, false),
    ] {
        let extra = serde_json::json!({
            "autowork": {
                "enabled": enabled,
                "tag": "game",
            }
        })
        .to_string();
        sqlx::query(
            "INSERT INTO conversations \
             (conversation_id, user_id, name, type, extra, status, created_at, updated_at) \
             VALUES (?, ?, 'recovery fixture', 'nomi', ?, 'pending', 1, 1)",
        )
        .bind(conversation_id.as_str())
        .bind(&owner)
        .bind(extra)
        .execute(pool)
        .await
        .unwrap();
    }

    let recoverable = nomifun_common::RequirementId::new();
    let receipt_protected = nomifun_common::RequirementId::new();
    let terminal_protected = nomifun_common::RequirementId::new();
    let disabled_binding = nomifun_common::RequirementId::new();
    let unrelated_note = nomifun_common::RequirementId::new();
    let fixtures = [
        (
            &recoverable,
            &enabled_conversation,
            AUTOWORK_PROVENANCE_CONFLICT_NOTE,
        ),
        (
            &receipt_protected,
            &enabled_conversation,
            AUTOWORK_PROVENANCE_CONFLICT_NOTE,
        ),
        (
            &terminal_protected,
            &enabled_conversation,
            AUTOWORK_PROVENANCE_CONFLICT_NOTE,
        ),
        (
            &disabled_binding,
            &disabled_conversation,
            AUTOWORK_PROVENANCE_CONFLICT_NOTE,
        ),
        (
            &unrelated_note,
            &enabled_conversation,
            "A different review reason must remain parked.",
        ),
    ];
    for (display_no, (requirement_id, conversation_id, note)) in
        (1_i64..).zip(fixtures.into_iter())
    {
        sqlx::query(
            "INSERT INTO requirements \
             (requirement_id, display_no, title, tag, status, completion_note, \
              owner_conversation_id, active_turn_started_at, started_at, attempt_count, \
              created_by, created_at, updated_at, claim_generation, claim_token) \
             VALUES (?, ?, 'recovery fixture', 'game', 'needs_review', ?, ?, 10, 10, 2, \
                     'user', 1, 11, 2, ?)",
        )
        .bind(requirement_id.as_str())
        .bind(display_no)
        .bind(note)
        .bind(conversation_id.as_str())
        .bind(format!("{display_no:x}").repeat(64))
        .execute(pool)
        .await
        .unwrap();
    }

    let receipt_payload = serde_json::json!({
        "autowork_authority": {
            "requirement_id": receipt_protected.as_str(),
            "claim_generation": 2,
        }
    })
    .to_string();
    sqlx::query(
        "INSERT INTO conversation_delivery_receipts \
         (operation_id, message_id, conversation_id, user_id, kind, request_payload, \
          status, result_ok, created_at, updated_at, completed_at) \
         VALUES ('autowork:recovery-receipt', ?, ?, ?, 'turn', ?, \
                 'completed', 1, 10, 20, 20)",
    )
    .bind(nomifun_common::MessageId::new().as_str())
    .bind(enabled_conversation.as_str())
    .bind(&owner)
    .bind(receipt_payload)
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO terminal_turn_admissions \
         (turn_token, terminal_id, pty_epoch, requirement_id, claim_generation, \
          phase, outcome, detail, admitted_at, settled_at, claim_token) \
         VALUES (?, ?, 0, ?, 2, 'settled', 'needs_review', \
                 'receiver evidence', 10, 20, ?)",
    )
    .bind(nomifun_common::generate_id())
    .bind(nomifun_common::TerminalId::new().as_str())
    .bind(terminal_protected.as_str())
    .bind("3".repeat(64))
    .execute(pool)
    .await
    .unwrap();

    sqlx::raw_sql(AUTOWORK_PROVENANCE_AUTHORITY_RECOVERY)
        .execute(pool)
        .await
        .unwrap();

    let recovered: (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<i64>,
        i64,
        i64,
        Option<String>,
        Option<i64>,
        Option<i64>,
    ) = sqlx::query_as(
        "SELECT status, completion_note, owner_conversation_id, owner_terminal_id, \
                active_turn_started_at, lease_expires_at, attempt_count, \
                claim_generation, claim_token, started_at, completed_at \
         FROM requirements WHERE requirement_id = ?",
    )
    .bind(recoverable.as_str())
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(recovered.0, "pending");
    assert_eq!(recovered.1, None);
    assert_eq!(recovered.2, None);
    assert_eq!(recovered.3, None);
    assert_eq!(recovered.4, None);
    assert_eq!(recovered.5, None);
    assert_eq!(recovered.6, 1, "only the rejected attempt is refunded");
    assert_eq!(recovered.7, 2, "claim generation remains monotonic");
    assert_eq!(recovered.8, None);
    assert_eq!(recovered.9, Some(10), "the audit start time is retained");
    assert_eq!(recovered.10, None);

    for protected in [
        &receipt_protected,
        &terminal_protected,
        &disabled_binding,
        &unrelated_note,
    ] {
        let (status, attempts, owner, token): (String, i64, Option<String>, Option<String>) =
            sqlx::query_as(
                "SELECT status, attempt_count, owner_conversation_id, claim_token \
                 FROM requirements WHERE requirement_id = ?",
            )
            .bind(protected.as_str())
            .fetch_one(pool)
            .await
            .unwrap();
        assert_eq!(
            status,
            "needs_review",
            "ambiguous or user-disabled work must remain parked"
        );
        assert_eq!(attempts, 2);
        assert!(owner.is_some());
        assert!(token.is_some());
    }

    sqlx::raw_sql(AUTOWORK_PROVENANCE_AUTHORITY_RECOVERY)
        .execute(pool)
        .await
        .unwrap();
    let attempts_after_replay: i64 =
        sqlx::query_scalar("SELECT attempt_count FROM requirements WHERE requirement_id = ?")
            .bind(recoverable.as_str())
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(
        attempts_after_replay, 1,
        "the one-shot recovery must be idempotent"
    );
}

#[tokio::test]
async fn init_creates_v3_users_table_and_owner() {
    let db = init_database_memory().await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(count, 1);
    assert!(nomifun_common::validate_uuidv7(&owner_id(db.pool()).await).is_ok());
}

#[tokio::test]
async fn cron_projection_upgrade_quarantines_preexisting_reserved_rows() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::raw_sql(&executable_baseline_sql())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(CRON_RUN_RESERVATIONS)
        .execute(&pool)
        .await
        .unwrap();
    let run_id = nomifun_common::CronJobRunId::new();
    let job_id = nomifun_common::CronJobId::new();
    sqlx::query(
        "INSERT INTO cron_run_reservations \
         (cron_job_run_id, cron_job_id, trigger_kind, operation_key, request_fingerprint, \
          status, created_at_ms, updated_at_ms) \
         VALUES (?, ?, 'run_now', 'legacy-run', 'legacy-fingerprint', 'reserved', 1, 1)",
    )
    .bind(run_id.as_str())
    .bind(job_id.as_str())
    .execute(&pool)
    .await
    .unwrap();

    sqlx::raw_sql(CRON_RUN_JOB_PROJECTION)
        .execute(&pool)
        .await
        .unwrap();

    let migrated: (String, Option<i64>) = sqlx::query_as(
        "SELECT job_projection_state, job_projected_at_ms \
         FROM cron_run_reservations WHERE cron_job_run_id = ?",
    )
    .bind(run_id.as_str())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(migrated, ("legacy_unknown".to_owned(), None));
}

#[tokio::test]
async fn conversation_turn_authority_upgrade_preserves_legacy_running_without_owner() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::raw_sql(&executable_baseline_sql())
        .execute(&pool)
        .await
        .unwrap();
    for migration in [
        IDMM_ACTION_RESERVATIONS,
        CHANNEL_INBOUND_RECEIPTS,
        CRON_RUN_RESERVATIONS,
        AUTOWORK_CLAIM_GENERATION,
        CONVERSATION_RECEIPT_PROJECTIONS,
        TERMINAL_TURN_ADMISSIONS,
    ] {
        sqlx::raw_sql(migration).execute(&pool).await.unwrap();
    }

    let conversation_id = nomifun_common::ConversationId::new();
    let owner = nomifun_common::UserId::new();
    sqlx::query(
        "INSERT INTO users (user_id, username, password_hash, created_at, updated_at) \
         VALUES (?, 'legacy-running-owner', 'hash', 1, 1)",
    )
    .bind(owner.as_str())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversations \
         (conversation_id, user_id, name, type, status, created_at, updated_at) \
         VALUES (?, ?, 'legacy running', 'nomi', 'running', 1, 1)",
    )
    .bind(conversation_id.as_str())
    .bind(owner.as_str())
    .execute(&pool)
    .await
    .unwrap();

    sqlx::raw_sql(CONVERSATION_TURN_AUTHORITY)
        .execute(&pool)
        .await
        .unwrap();

    let migrated: (String, i64, Option<String>) = sqlx::query_as(
        "SELECT status, admission_epoch, active_turn_operation_id \
         FROM conversations WHERE conversation_id = ?",
    )
    .bind(conversation_id.as_str())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(migrated, ("running".to_owned(), 0, None));

    // Trigger installation must not make the retained legacy aggregate
    // unreadable or block metadata-only maintenance.
    sqlx::query("UPDATE conversations SET name = 'legacy retained' WHERE conversation_id = ?")
        .bind(conversation_id.as_str())
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn conversation_turn_authority_is_null_safe_for_nullable_legacy_status() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    let nullable_status_baseline = executable_baseline_sql().replacen(
        "status                TEXT NOT NULL DEFAULT 'pending'",
        "status                TEXT DEFAULT 'pending'",
        1,
    );
    assert!(
        nullable_status_baseline.contains("status                TEXT DEFAULT 'pending'"),
        "test fixture must model a legacy nullable Conversation status"
    );
    sqlx::raw_sql(&nullable_status_baseline)
        .execute(&pool)
        .await
        .unwrap();
    for migration in [
        IDMM_ACTION_RESERVATIONS,
        CHANNEL_INBOUND_RECEIPTS,
        CRON_RUN_RESERVATIONS,
        AUTOWORK_CLAIM_GENERATION,
        CONVERSATION_RECEIPT_PROJECTIONS,
        TERMINAL_TURN_ADMISSIONS,
        CONVERSATION_TURN_AUTHORITY,
    ] {
        sqlx::raw_sql(migration).execute(&pool).await.unwrap();
    }

    let conversation_id = nomifun_common::ConversationId::new();
    let owner = nomifun_common::UserId::new();
    sqlx::query(
        "INSERT INTO users (user_id, username, password_hash, created_at, updated_at) \
         VALUES (?, 'nullable-status-owner', 'hash', 1, 1)",
    )
    .bind(owner.as_str())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversations \
         (conversation_id, user_id, name, type, status, created_at, updated_at) \
         VALUES (?, ?, 'nullable status', 'nomi', 'pending', 1, 1)",
    )
    .bind(conversation_id.as_str())
    .bind(owner.as_str())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE conversations SET status = NULL WHERE conversation_id = ?")
        .bind(conversation_id.as_str())
        .execute(&pool)
        .await
        .unwrap();

    let null_reopen = sqlx::query(
        "UPDATE conversations \
         SET status = 'running', active_turn_operation_id = 'turn:null-reopen', \
             admission_epoch = admission_epoch + 1 \
         WHERE conversation_id = ?",
    )
    .bind(conversation_id.as_str())
    .execute(&pool)
    .await
    .unwrap_err();
    assert!(
        null_reopen
            .to_string()
            .contains("Conversation Running admission requires an exact accepted turn receipt"),
        "NULL-to-Running must not bypass exact receipt admission: {null_reopen}"
    );

    sqlx::query("UPDATE conversations SET status = 'pending' WHERE conversation_id = ?")
        .bind(conversation_id.as_str())
        .execute(&pool)
        .await
        .unwrap();
    let operation_id = "turn:null-safe-exit";
    let message_id = nomifun_common::MessageId::new();
    sqlx::query(
        "INSERT INTO conversation_delivery_receipts \
         (operation_id, message_id, conversation_id, user_id, kind, request_payload, \
          status, created_at, updated_at) \
         VALUES (?, ?, ?, ?, 'turn', '{}', 'accepted', 2, 2)",
    )
    .bind(operation_id)
    .bind(message_id.as_str())
    .bind(conversation_id.as_str())
    .bind(owner.as_str())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE conversations \
         SET status = 'running', active_turn_operation_id = ?, \
             admission_epoch = admission_epoch + 1 \
         WHERE conversation_id = ?",
    )
    .bind(operation_id)
    .bind(conversation_id.as_str())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE conversation_delivery_receipts \
         SET status = 'completed', result_ok = 1, completed_at = 3, updated_at = 3 \
         WHERE operation_id = ?",
    )
    .bind(operation_id)
    .execute(&pool)
    .await
    .unwrap();

    let null_exit = sqlx::query(
        "UPDATE conversations \
         SET status = NULL, active_turn_operation_id = NULL, \
             admission_epoch = admission_epoch + 1 \
         WHERE conversation_id = ?",
    )
    .bind(conversation_id.as_str())
    .execute(&pool)
    .await
    .unwrap_err();
    assert!(
        null_exit
            .to_string()
            .contains("Conversation Running exit requires completed turn receipts"),
        "completed receipt plus valid cleanup shape must still reject Running-to-NULL: {null_exit}"
    );
    let retained: (Option<String>, i64, Option<String>) = sqlx::query_as(
        "SELECT status, admission_epoch, active_turn_operation_id \
         FROM conversations WHERE conversation_id = ?",
    )
    .bind(conversation_id.as_str())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        retained,
        (Some("running".to_owned()), 1, Some(operation_id.to_owned()))
    );
}

#[tokio::test]
async fn claim_generation_upgrade_parks_legacy_active_work_without_minting_execution_authority() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::raw_sql(&executable_baseline_sql())
        .execute(&pool)
        .await
        .unwrap();
    let owner = nomifun_common::ConversationId::new();
    let active = nomifun_common::RequirementId::new();
    let pending = nomifun_common::RequirementId::new();
    sqlx::query(
        "INSERT INTO requirements \
         (requirement_id, display_no, title, tag, status, owner_conversation_id, \
          active_turn_started_at, lease_expires_at, attempt_count, created_at, updated_at) \
         VALUES (?1, 1, 'legacy active', 'legacy', 'in_progress', ?2, 10, 20, 1, 1, 1), \
                (?3, 2, 'legacy pending', 'legacy', 'pending', NULL, NULL, NULL, 0, 1, 1)",
    )
    .bind(active.as_str())
    .bind(owner.as_str())
    .bind(pending.as_str())
    .execute(&pool)
    .await
    .unwrap();

    sqlx::raw_sql(AUTOWORK_CLAIM_GENERATION)
        .execute(&pool)
        .await
        .unwrap();

    let active_row: (String, i64, Option<String>, Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT status, claim_generation, owner_conversation_id, \
                active_turn_started_at, lease_expires_at \
         FROM requirements WHERE requirement_id=?",
    )
    .bind(active.as_str())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(active_row.0, "needs_review");
    assert_eq!(active_row.1, 0, "migration must not mint execution authority");
    assert_eq!(active_row.2.as_deref(), Some(owner.as_str()));
    assert_eq!(active_row.3, Some(10));
    assert_eq!(active_row.4, None);

    let pending_row: (String, i64) = sqlx::query_as(
        "SELECT status, claim_generation FROM requirements WHERE requirement_id=?",
    )
    .bind(pending.as_str())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(pending_row, ("pending".into(), 0));
}

#[tokio::test]
async fn claim_capability_upgrade_parks_tokenless_active_work_and_open_terminal_receipts() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::raw_sql(&executable_baseline_sql())
        .execute(&pool)
        .await
        .unwrap();
    for migration in [
        IDMM_ACTION_RESERVATIONS,
        CHANNEL_INBOUND_RECEIPTS,
        CRON_RUN_RESERVATIONS,
        AUTOWORK_CLAIM_GENERATION,
        CONVERSATION_RECEIPT_PROJECTIONS,
        TERMINAL_TURN_ADMISSIONS,
        CONVERSATION_TURN_AUTHORITY,
    ] {
        sqlx::raw_sql(migration).execute(&pool).await.unwrap();
    }

    let owner = nomifun_common::ConversationId::new();
    let active = nomifun_common::RequirementId::new();
    let duplicate_active = nomifun_common::RequirementId::new();
    let pending = nomifun_common::RequirementId::new();
    let dirty_pending = nomifun_common::RequirementId::new();
    let terminal = nomifun_common::TerminalId::new();
    let turn_token = nomifun_common::generate_id();
    sqlx::query(
        "INSERT INTO requirements \
         (requirement_id, display_no, title, tag, status, owner_conversation_id, \
          active_turn_started_at, lease_expires_at, attempt_count, claim_generation, \
          created_at, updated_at) \
         VALUES (?1, 1, 'pre-capability active', 'legacy-token', 'in_progress', ?2, \
                 10, 20, 1, 7, 1, 1), \
                (?3, 2, 'duplicate pre-capability active', 'legacy-token-b', \
                 'in_progress', ?2, 12, 22, 1, 8, 1, 1), \
                (?4, 3, 'pre-capability pending', 'legacy-token', 'pending', NULL, \
                 NULL, NULL, 0, 4, 1, 1), \
                (?5, 4, 'half-authorized pending', 'legacy-token', 'pending', ?2, \
                 13, 23, 1, 6, 1, 1)",
    )
    .bind(active.as_str())
    .bind(owner.as_str())
    .bind(duplicate_active.as_str())
    .bind(pending.as_str())
    .bind(dirty_pending.as_str())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO terminal_turn_admissions \
         (turn_token, terminal_id, pty_epoch, requirement_id, claim_generation, \
          phase, admitted_at) \
         VALUES (?1, ?2, 3, ?3, 7, 'admitted', 11)",
    )
    .bind(&turn_token)
    .bind(terminal.as_str())
    .bind(active.as_str())
    .execute(&pool)
    .await
    .unwrap();

    sqlx::raw_sql(REQUIREMENT_CLAIM_CAPABILITIES)
        .execute(&pool)
        .await
        .unwrap();

    let active_row: (
        String,
        i64,
        Option<String>,
        Option<i64>,
        Option<i64>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT status, claim_generation, owner_conversation_id, \
                active_turn_started_at, lease_expires_at, claim_token \
         FROM requirements WHERE requirement_id=?",
    )
    .bind(active.as_str())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(active_row.0, "needs_review");
    assert_eq!(active_row.1, 7, "upgrade must preserve claim generation");
    assert_eq!(active_row.2.as_deref(), Some(owner.as_str()));
    assert_eq!(active_row.3, Some(10));
    assert_eq!(active_row.4, None);
    assert_eq!(active_row.5, None, "upgrade must never mint authority");
    let duplicate_row: (String, i64, Option<String>, Option<i64>, Option<String>) =
        sqlx::query_as(
            "SELECT status, claim_generation, owner_conversation_id, \
                    lease_expires_at, claim_token \
             FROM requirements WHERE requirement_id=?",
        )
        .bind(duplicate_active.as_str())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(duplicate_row.0, "needs_review");
    assert_eq!(duplicate_row.1, 8);
    assert_eq!(duplicate_row.2.as_deref(), Some(owner.as_str()));
    assert_eq!(duplicate_row.3, None);
    assert_eq!(duplicate_row.4, None);

    let pending_row: (String, i64, Option<String>) = sqlx::query_as(
        "SELECT status, claim_generation, claim_token \
         FROM requirements WHERE requirement_id=?",
    )
    .bind(pending.as_str())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(pending_row, ("pending".into(), 4, None));

    let dirty_pending_row: (
        String,
        i64,
        Option<String>,
        Option<i64>,
        Option<i64>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT status, claim_generation, owner_conversation_id, \
                active_turn_started_at, lease_expires_at, claim_token \
         FROM requirements WHERE requirement_id=?",
    )
    .bind(dirty_pending.as_str())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(dirty_pending_row.0, "needs_review");
    assert_eq!(dirty_pending_row.1, 6);
    assert_eq!(dirty_pending_row.2.as_deref(), Some(owner.as_str()));
    assert_eq!(dirty_pending_row.3, Some(13));
    assert_eq!(dirty_pending_row.4, None);
    assert_eq!(dirty_pending_row.5, None);

    let receipt: (String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT phase, outcome, claim_token \
         FROM terminal_turn_admissions WHERE turn_token=?",
    )
    .bind(&turn_token)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        receipt,
        ("settled".into(), Some("needs_review".into()), None)
    );

    sqlx::raw_sql(REQUIREMENT_PRE_EFFECT_ABANDON)
        .execute(&pool)
        .await
        .unwrap();
    let abandon_guards: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM requirement_pre_effect_abandon_guards")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        abandon_guards, 0,
        "installing the abandon command must not mint authority for dirty legacy rows"
    );
    let still_parked: (String, i64, Option<String>, Option<i64>) = sqlx::query_as(
        "SELECT status, claim_generation, owner_conversation_id, active_turn_started_at \
         FROM requirements WHERE requirement_id=?",
    )
    .bind(dirty_pending.as_str())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(still_parked.0, "needs_review");
    assert_eq!(still_parked.1, 6);
    assert_eq!(still_parked.2.as_deref(), Some(owner.as_str()));
    assert_eq!(still_parked.3, Some(13));
}

#[tokio::test]
async fn requirement_claim_triggers_reject_forged_or_reopened_execution_authority() {
    let db = init_database_memory().await.unwrap();
    let pool = db.pool();
    let owner = nomifun_common::ConversationId::new();
    let replacement_owner = nomifun_common::ConversationId::new();
    let pending = nomifun_common::RequirementId::new();
    let direct_active = nomifun_common::RequirementId::new();
    let failed = nomifun_common::RequirementId::new();
    let token_a = "a".repeat(64);
    let token_b = "b".repeat(64);

    let direct_insert = sqlx::query(
        "INSERT INTO requirements \
         (requirement_id, display_no, title, tag, status, owner_conversation_id, \
          claim_generation, claim_token, created_at, updated_at) \
         VALUES (?1, 1, 'forged active', 'guard', 'in_progress', ?2, 1, ?3, 1, 1)",
    )
    .bind(direct_active.as_str())
    .bind(owner.as_str())
    .bind(&token_a)
    .execute(pool)
    .await;
    assert!(
        direct_insert.is_err(),
        "an active claim cannot be manufactured by INSERT"
    );

    sqlx::query(
        "INSERT INTO requirements \
         (requirement_id, display_no, title, tag, status, claim_generation, \
          created_at, updated_at) \
         VALUES (?1, 2, 'pending', 'guard', 'pending', 3, 1, 1), \
                (?2, 3, 'failed', 'guard', 'failed', 8, 1, 1)",
    )
    .bind(pending.as_str())
    .bind(failed.as_str())
    .execute(pool)
    .await
    .unwrap();

    let pending_with_authority = sqlx::query(
        "UPDATE requirements SET claim_token=?1 WHERE requirement_id=?2",
    )
    .bind(&token_a)
    .bind(pending.as_str())
    .execute(pool)
    .await;
    assert!(
        pending_with_authority.is_err(),
        "pending rows cannot retain an execution capability"
    );

    let skipped_generation = sqlx::query(
        "UPDATE requirements \
         SET status='in_progress', owner_conversation_id=?1, \
             active_turn_started_at=10, lease_expires_at=20, started_at=10, \
             attempt_count=attempt_count + 1, claim_generation=5, claim_token=?2 \
         WHERE requirement_id=?3",
    )
    .bind(owner.as_str())
    .bind(&token_a)
    .bind(pending.as_str())
    .execute(pool)
    .await;
    assert!(
        skipped_generation.is_err(),
        "pending claims must increment exactly one generation"
    );

    sqlx::query(
        "UPDATE requirements \
         SET status='in_progress', owner_conversation_id=?1, \
             active_turn_started_at=10, lease_expires_at=20, started_at=10, \
             attempt_count=attempt_count + 1, \
             claim_generation=claim_generation + 1, claim_token=?2 \
         WHERE requirement_id=?3",
    )
    .bind(owner.as_str())
    .bind(&token_a)
    .bind(pending.as_str())
    .execute(pool)
    .await
    .expect("the structurally valid atomic claim transition");

    let mutate_active_authority = sqlx::query(
        "UPDATE requirements \
         SET owner_conversation_id=?1, claim_generation=5, claim_token=?2 \
         WHERE requirement_id=?3",
    )
    .bind(replacement_owner.as_str())
    .bind(&token_b)
    .bind(pending.as_str())
    .execute(pool)
    .await;
    assert!(
        mutate_active_authority.is_err(),
        "an active claim's owner, generation, and capability are immutable"
    );
    sqlx::query(
        "UPDATE requirements SET lease_expires_at=99 WHERE requirement_id=?",
    )
    .bind(pending.as_str())
    .execute(pool)
    .await
    .expect("lease-only renewal preserves exact authority");

    let reopen_failed = sqlx::query(
        "UPDATE requirements \
         SET status='in_progress', owner_conversation_id=?1, \
             active_turn_started_at=10, lease_expires_at=20, started_at=10, \
             attempt_count=attempt_count + 1, claim_generation=9, claim_token=?2 \
         WHERE requirement_id=?3",
    )
    .bind(owner.as_str())
    .bind(&token_b)
    .bind(failed.as_str())
    .execute(pool)
    .await;
    assert!(
        reopen_failed.is_err(),
        "failed work must first return to pending through an explicit resume"
    );

    sqlx::query("UPDATE requirements SET status='done' WHERE requirement_id=?")
        .bind(pending.as_str())
        .execute(pool)
        .await
        .expect("active claim may resolve done");
    let reopen_done =
        sqlx::query("UPDATE requirements SET status='pending' WHERE requirement_id=?")
            .bind(pending.as_str())
            .execute(pool)
            .await;
    assert!(
        reopen_done.is_err(),
        "done is absorbing even for direct SQL callers"
    );
    let null_done = sqlx::query("UPDATE requirements SET status=NULL WHERE requirement_id=?")
        .bind(pending.as_str())
        .execute(pool)
        .await;
    assert!(
        null_done.is_err(),
        "SQL NULL must not bypass the absorbing done/cancelled trigger"
    );
    assert!(
        null_done
            .unwrap_err()
            .to_string()
            .contains("completed or cancelled Requirement status is immutable"),
        "the null-safe trigger, not only the column constraint, must reject the write"
    );

    let persisted: (String, i64, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT status, claim_generation, claim_token, owner_conversation_id \
         FROM requirements WHERE requirement_id=?",
    )
    .bind(pending.as_str())
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(persisted.0, "done");
    assert_eq!(persisted.1, 4);
    assert_eq!(persisted.2.as_deref(), Some(token_a.as_str()));
    assert_eq!(persisted.3.as_deref(), Some(owner.as_str()));
    let internal_row: nomifun_db::models::RequirementRow =
        sqlx::query_as("SELECT * FROM requirements WHERE requirement_id=?")
            .bind(pending.as_str())
            .fetch_one(pool)
            .await
            .unwrap();
    let serialized = serde_json::to_value(&internal_row).unwrap();
    assert!(
        serialized.get("claim_token").is_none(),
        "internal Requirement capability must never leak through serde"
    );
    assert!(
        !format!("{internal_row:?}").contains(token_a.as_str()),
        "internal Requirement capability must be redacted from debug/log snapshots"
    );
}

#[tokio::test]
async fn terminal_turn_trigger_requires_and_freezes_the_claim_capability() {
    let db = init_database_memory().await.unwrap();
    let pool = db.pool();
    let terminal = nomifun_common::TerminalId::new();
    let requirement = nomifun_common::RequirementId::new();
    let missing_token_turn = nomifun_common::generate_id();
    let admitted_turn = nomifun_common::generate_id();
    let token = "c".repeat(64);

    let missing_token = sqlx::query(
        "INSERT INTO terminal_turn_admissions \
         (turn_token, terminal_id, pty_epoch, requirement_id, claim_generation, \
          phase, admitted_at) \
         VALUES (?1, ?2, 1, ?3, 1, 'admitted', 1)",
    )
    .bind(&missing_token_turn)
    .bind(terminal.as_str())
    .bind(requirement.as_str())
    .execute(pool)
    .await;
    assert!(
        missing_token.is_err(),
        "an open PTY receipt cannot exist without claim authority"
    );
    let null_phase_without_token = sqlx::query(
        "INSERT INTO terminal_turn_admissions \
         (turn_token, terminal_id, pty_epoch, requirement_id, claim_generation, \
          phase, admitted_at) \
         VALUES (?1, ?2, 1, ?3, 1, NULL, 1)",
    )
    .bind(nomifun_common::generate_id())
    .bind(terminal.as_str())
    .bind(requirement.as_str())
    .execute(pool)
    .await;
    assert!(
        null_phase_without_token.is_err(),
        "SQL NULL must not make a tokenless terminal admission look settled"
    );
    assert!(
        null_phase_without_token
            .unwrap_err()
            .to_string()
            .contains("open terminal turn admission requires a Requirement capability"),
        "the null-safe receipt guard must fire before the phase NOT NULL constraint"
    );

    sqlx::query(
        "INSERT INTO terminal_turn_admissions \
         (turn_token, terminal_id, pty_epoch, requirement_id, claim_generation, \
          phase, admitted_at, claim_token) \
         VALUES (?1, ?2, 1, ?3, 1, 'admitted', 1, ?4)",
    )
    .bind(&admitted_turn)
    .bind(terminal.as_str())
    .bind(requirement.as_str())
    .bind(&token)
    .execute(pool)
    .await
    .unwrap();
    let receipt: nomifun_db::models::TerminalTurnAdmissionRow =
        sqlx::query_as("SELECT * FROM terminal_turn_admissions WHERE turn_token=?")
            .bind(&admitted_turn)
            .fetch_one(pool)
            .await
            .unwrap();
    let serialized = serde_json::to_value(&receipt).unwrap();
    assert!(serialized.get("claim_token").is_none());
    assert!(
        serialized.get("turn_token").is_none(),
        "the receipt mutation capability is backend-internal"
    );
    let receipt_debug = format!("{receipt:?}");
    assert!(!receipt_debug.contains(token.as_str()));
    assert!(!receipt_debug.contains(admitted_turn.as_str()));
    let admission_key = nomifun_db::TerminalTurnAdmissionKey::from_row(&receipt).unwrap();
    let key_debug = format!("{admission_key:?}");
    assert!(!key_debug.contains(token.as_str()));
    assert!(!key_debug.contains(admitted_turn.as_str()));

    let token_mutation = sqlx::query(
        "UPDATE terminal_turn_admissions SET claim_token=?1 WHERE turn_token=?2",
    )
    .bind("d".repeat(64))
    .bind(&admitted_turn)
    .execute(pool)
    .await;
    assert!(
        token_mutation.is_err(),
        "the capability recorded at PTY admission is immutable"
    );
}

#[tokio::test]
async fn sqlite_busy_timeout_is_configured() {
    let db = init_database_memory().await.unwrap();
    let busy_timeout: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(busy_timeout, 5000);
}

#[tokio::test]
async fn file_reopen_preserves_rows_and_named_business_ids() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let db = init_database(&path).await.unwrap();
    let user_id = nomifun_common::UserId::new();
    sqlx::query(
        "INSERT INTO users (user_id, username, password_hash, created_at, updated_at) \
         VALUES (?, 'alice', 'hash123', 1000, 1000)",
    )
    .bind(user_id.as_str())
    .execute(db.pool())
    .await
    .unwrap();
    db.close().await;

    let db = init_database(&path).await.unwrap();
    let row = sqlx::query("SELECT id, user_id, username FROM users WHERE user_id = ?")
        .bind(user_id.as_str())
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert!(row.get::<i64, _>("id") > 0);
    assert_eq!(row.get::<String, _>("user_id"), user_id.as_str());
    assert_eq!(row.get::<String, _>("username"), "alice");
    db.close().await;
}

#[tokio::test]
async fn migrations_preserve_the_published_v3_baseline_and_apply_additive_upgrades() {
    let db = init_database_memory().await.unwrap();
    let migrations: Vec<(i64, String)> =
        sqlx::query_as(
            "SELECT version, description FROM _sqlx_migrations \
             WHERE success = 1 ORDER BY version",
        )
            .fetch_all(db.pool())
            .await
            .unwrap();
    assert!(migrations.len() >= 2);
    assert_eq!(migrations[0].0, 1);
    assert!(migrations[0].1.contains("v3 baseline"));
    assert!(migrations
        .iter()
        .any(|(version, description)| *version == 2
            && description.contains("idmm action reservations")));
    assert!(migrations
        .iter()
        .any(|(version, description)| *version == 3
            && description.contains("channel inbound receipts")));
    assert!(migrations
        .iter()
        .any(|(version, description)| *version == 4
            && description.contains("cron run reservations")));
    assert!(migrations
        .iter()
        .any(|(version, description)| *version == 5
            && description.contains("autowork claim generation")));
    assert!(migrations
        .iter()
        .any(|(version, description)| *version == 6
            && description.contains("conversation receipt projections")));
    assert!(migrations
        .iter()
        .any(|(version, description)| *version == 7
            && description.contains("terminal turn admissions")));
    assert!(migrations
        .iter()
        .any(|(version, description)| *version == 8
            && description.contains("conversation turn authority")));
    assert!(migrations
        .iter()
        .any(|(version, description)| *version == 9
            && description.contains("requirement claim capabilities")));
    assert!(migrations
        .iter()
        .any(|(version, description)| *version == 10
            && description.contains("requirement pre effect abandon")));
    assert!(migrations
        .iter()
        .any(|(version, description)| *version == 11
            && description.contains("cron run job projection")));
    assert!(migrations
        .iter()
        .any(|(version, description)| *version == 12
            && description.contains("conversation receipt lifecycle")));
    assert!(migrations
        .iter()
        .any(|(version, description)| *version == 13
            && description.contains("autowork provenance authority recovery")));
    assert!(CRON_RUN_JOB_PROJECTION.contains("job_projection_state"));
    assert!(CRON_RUN_JOB_PROJECTION.contains("'legacy_unknown'"));
    assert!(CONVERSATION_RECEIPT_LIFECYCLE.contains("OLD.status = 'completed'"));
    assert!(CONVERSATION_RECEIPT_LIFECYCLE.contains("NEW.status IS NOT OLD.status"));
    assert!(AUTOWORK_PROVENANCE_AUTHORITY_RECOVERY.contains("created_by IN ('user', 'agent')"));
    assert_eq!(BASELINE.matches("CREATE TABLE ").count(), 64);
}

#[tokio::test]
async fn published_baseline_database_upgrades_in_place_without_checksum_rewrite() {
    let dir = tempfile::tempdir().unwrap();
    let legacy_migrations = dir.path().join("legacy-migrations");
    std::fs::create_dir(&legacy_migrations).unwrap();
    std::fs::write(legacy_migrations.join("001_v3_baseline.sql"), BASELINE).unwrap();
    let path = dir.path().join("legacy-v3.db");

    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true),
        )
        .await
        .unwrap();
    let legacy_migrator = sqlx::migrate::Migrator::new(legacy_migrations.clone())
        .await
        .unwrap();
    legacy_migrator.run(&pool).await.unwrap();
    let conversation_id = nomifun_common::ConversationId::new();
    let legacy_owner = nomifun_common::UserId::new();
    let legacy_plugin_id = nomifun_common::ChannelPluginId::new();
    let legacy_channel_user_id = nomifun_common::ChannelUserId::new();
    let legacy_session_a = nomifun_common::ChannelSessionId::new();
    let legacy_session_b = nomifun_common::ChannelSessionId::new();
    let legacy_message_id = nomifun_common::MessageId::new();
    let legacy_requirement_id = nomifun_common::RequirementId::new();
    sqlx::query(
        "INSERT INTO users \
         (user_id, username, password_hash, created_at, updated_at) \
         VALUES (?, 'legacy-owner', 'hash', 1, 1)",
    )
    .bind(legacy_owner.as_str())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO requirements \
         (requirement_id, display_no, title, content, tag, order_key, sort_seq, \
          status, owner_conversation_id, active_turn_started_at, lease_expires_at, \
          started_at, attempt_count, created_by, created_at, updated_at) \
         VALUES (?, 1, 'legacy active work', 'ambiguous pre-fence turn', 'legacy', \
                 'legacy', 'legacy', 'in_progress', ?, 10, 999999, 10, 1, 'user', 1, 1)",
    )
    .bind(legacy_requirement_id.as_str())
    .bind(conversation_id.as_str())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO messages \
         (message_id, conversation_id, type, content, position, status, hidden, created_at) \
         VALUES (?, ?, 'text', '{\"content\":\"legacy\"}', 'right', 'finish', 0, 1)",
    )
    .bind(legacy_message_id.as_str())
    .bind(conversation_id.as_str())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversation_delivery_receipts \
         (operation_id, message_id, conversation_id, user_id, kind, request_payload, status, \
          result_ok, created_at, updated_at, completed_at) \
         VALUES ('legacy-receipt-operation', ?, ?, ?, 'turn', '{}', 'completed', 1, 1, 1, 1)",
    )
    .bind(legacy_message_id.as_str())
    .bind(conversation_id.as_str())
    .bind(legacy_owner.as_str())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO installation_identity (singleton_key, owner_user_id) \
         VALUES ('installation', ?)",
    )
    .bind(legacy_owner.as_str())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversations \
         (conversation_id, user_id, name, type, extra, status, created_at, updated_at) \
         VALUES (?, ?, 'legacy-row', 'nomi', '{}', 'pending', 1, 1)",
    )
    .bind(conversation_id.as_str())
    .bind(legacy_owner.as_str())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO channel_plugins \
         (channel_plugin_id, type, name, enabled, config, created_at, updated_at) \
         VALUES (?, 'telegram', 'legacy-plugin', 1, '{}', 1, 1)",
    )
    .bind(legacy_plugin_id.as_str())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO channel_users \
         (channel_user_id, platform_user_id, platform_type, channel_plugin_id, authorized_at) \
         VALUES (?, 'legacy-user', 'telegram', ?, 1)",
    )
    .bind(legacy_channel_user_id.as_str())
    .bind(legacy_plugin_id.as_str())
    .execute(&pool)
    .await
    .unwrap();
    for (session_id, created_at) in [(&legacy_session_a, 10_i64), (&legacy_session_b, 20_i64)] {
        sqlx::query(
            "INSERT INTO channel_sessions \
             (channel_session_id, channel_user_id, agent_type, chat_id, channel_plugin_id, \
              created_at, last_activity) \
             VALUES (?, ?, 'nomi', 'legacy-chat', ?, ?, ?)",
        )
        .bind(session_id.as_str())
        .bind(legacy_channel_user_id.as_str())
        .bind(legacy_plugin_id.as_str())
        .bind(created_at)
        .bind(created_at)
        .execute(&pool)
        .await
        .unwrap();
    }
    pool.close().await;

    let upgraded = init_database(&path)
        .await
        .expect("published baseline should receive migration 002 in place");
    let migration_versions: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(upgraded.pool())
            .await
            .unwrap();
    assert_eq!(migration_versions.first(), Some(&1));
    assert!(migration_versions.contains(&2));
    assert!(migration_versions.contains(&3));
    assert!(migration_versions.contains(&4));
    assert!(migration_versions.contains(&5));
    assert!(migration_versions.contains(&6));
    assert!(migration_versions.contains(&7));
    assert!(migration_versions.contains(&8));
    assert!(migration_versions.contains(&9));
    assert!(migration_versions.contains(&10));
    assert!(migration_versions.contains(&11));
    assert!(migration_versions.contains(&12));
    assert!(migration_versions.contains(&13));
    let preserved: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM conversations WHERE conversation_id = ?")
            .bind(conversation_id.as_str())
            .fetch_one(upgraded.pool())
            .await
            .unwrap();
    assert_eq!(preserved, 1);
    let reservation_table_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(\
             SELECT 1 FROM sqlite_schema \
             WHERE type = 'table' AND name = 'idmm_action_reservations'\
         )",
    )
    .fetch_one(upgraded.pool())
    .await
    .unwrap();
    assert!(reservation_table_exists);
    let pre_effect_abandon_schema: (bool, bool) = sqlx::query_as(
        "SELECT \
             EXISTS(SELECT 1 FROM sqlite_schema \
                     WHERE type = 'table' \
                       AND name = 'requirement_pre_effect_abandon_guards'), \
             EXISTS(SELECT 1 FROM sqlite_schema \
                     WHERE type = 'trigger' \
                       AND name = 'trg_requirements_pre_effect_abandon_guard_apply')",
    )
    .fetch_one(upgraded.pool())
    .await
    .unwrap();
    assert_eq!(
        pre_effect_abandon_schema,
        (true, true),
        "published baseline upgrades must install the atomic pre-effect abandon command"
    );
    let cron_projection_columns: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_table_info('cron_run_reservations') \
         WHERE name IN ('job_projection_state', 'job_projected_at_ms')",
    )
    .fetch_one(upgraded.pool())
    .await
    .unwrap();
    assert_eq!(
        cron_projection_columns, 2,
        "published baseline upgrades must install exact Cron projection state"
    );
    let receipt_lifecycle_trigger: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema \
                       WHERE type = 'trigger' \
                         AND name = 'trg_conversation_delivery_receipts_lifecycle_update_guard')",
    )
    .fetch_one(upgraded.pool())
    .await
    .unwrap();
    assert!(
        receipt_lifecycle_trigger,
        "published baseline upgrades must make completed receipts absorbing"
    );
    let receipt_projection: (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT projected_conversation_id, projected_message_id \
         FROM conversation_delivery_receipts \
         WHERE operation_id = 'legacy-receipt-operation'",
    )
    .fetch_one(upgraded.pool())
    .await
    .unwrap();
    assert_eq!(
        receipt_projection.0.as_deref(),
        Some(conversation_id.as_str())
    );
    assert_eq!(
        receipt_projection.1.as_deref(),
        Some(legacy_message_id.as_str())
    );
    let legacy_requirement: (String, i64, Option<String>, Option<i64>, Option<String>) =
        sqlx::query_as(
            "SELECT status, claim_generation, owner_conversation_id, \
                    lease_expires_at, completion_note \
             FROM requirements WHERE requirement_id = ?",
        )
        .bind(legacy_requirement_id.as_str())
        .fetch_one(upgraded.pool())
        .await
        .unwrap();
    assert_eq!(legacy_requirement.0, "needs_review");
    assert_eq!(legacy_requirement.1, 0);
    assert_eq!(
        legacy_requirement.2.as_deref(),
        Some(conversation_id.as_str()),
        "migration must retain the typed owner as audit evidence"
    );
    assert_eq!(
        legacy_requirement.3, None,
        "a generation-zero claim must not retain a renewable lease"
    );
    assert!(
        legacy_requirement
            .4
            .as_deref()
            .is_some_and(|note| note.contains("no durable generation")),
        "migration must explain why the ambiguous legacy claim was parked"
    );

    let legacy_sessions_preserved: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM channel_sessions \
         WHERE channel_plugin_id = ? AND channel_user_id = ? AND chat_id = 'legacy-chat'",
    )
    .bind(legacy_plugin_id.as_str())
    .bind(legacy_channel_user_id.as_str())
    .fetch_one(upgraded.pool())
    .await
    .unwrap();
    assert_eq!(
        legacy_sessions_preserved, 2,
        "migration must not delete duplicate legacy sessions"
    );
    let canonical_session_id: String = sqlx::query_scalar(
        "SELECT channel_session_id FROM channel_session_bindings \
         WHERE channel_plugin_id = ? AND channel_user_id = ? AND chat_id = 'legacy-chat'",
    )
    .bind(legacy_plugin_id.as_str())
    .bind(legacy_channel_user_id.as_str())
    .fetch_one(upgraded.pool())
    .await
    .unwrap();
    assert_eq!(
        canonical_session_id,
        legacy_session_a.as_str(),
        "the earliest technical legacy row must remain the stable canonical session"
    );
}

#[tokio::test]
async fn retained_channel_receipt_survives_projection_deletes_and_database_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("channel-receipt.db");
    let db = init_database(&path).await.unwrap();
    let owner = owner_id(db.pool()).await;
    let plugin_id = nomifun_common::ChannelPluginId::new();
    let conversation_id = nomifun_common::ConversationId::new();
    let message_id = nomifun_common::MessageId::new();
    let operation_key = format!("channel-inbound:v1:{}", "a".repeat(64));
    let payload_hash = "1".repeat(64);

    sqlx::query(
        "INSERT INTO channel_plugins \
         (channel_plugin_id, type, name, enabled, config, created_at, updated_at) \
         VALUES (?, 'telegram', 'temporary', 1, '{}', 1, 1)",
    )
    .bind(plugin_id.as_str())
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversations \
         (conversation_id, user_id, name, type, extra, status, created_at, updated_at) \
         VALUES (?, ?, 'temporary', 'nomi', '{}', 'finished', 1, 1)",
    )
    .bind(conversation_id.as_str())
    .bind(&owner)
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO messages \
         (message_id, conversation_id, type, content, hidden, created_at) \
         VALUES (?, ?, 'text', '{\"content\":\"hello\"}', 0, 1)",
    )
    .bind(message_id.as_str())
    .bind(conversation_id.as_str())
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO channel_inbound_receipts \
            (operation_key, user_scope_id, user_id, channel_plugin_scope_id, \
             channel_plugin_id, platform, chat_id, provider_event_id, payload_hash, \
             status, phase, owner_generation, conversation_scope_id, message_scope_id, \
             conversation_id, message_id, outcome_json, \
             created_at, updated_at, completed_at) \
         VALUES (?, ?, ?, ?, ?, 'telegram', 'chat-a', 'provider-event', ?, \
                 'completed', 'settled', 1, ?, ?, ?, ?, \
                 '{\"kind\":\"dispatched\"}', 1, 2, 2)",
    )
    .bind(&operation_key)
    .bind(&owner)
    .bind(&owner)
    .bind(plugin_id.as_str())
    .bind(plugin_id.as_str())
    .bind(&payload_hash)
    .bind(conversation_id.as_str())
    .bind(message_id.as_str())
    .bind(conversation_id.as_str())
    .bind(message_id.as_str())
    .execute(db.pool())
    .await
    .unwrap();

    let channel_repo = SqliteChannelRepository::new(db.pool().clone());
    let conversation_repo = SqliteConversationRepository::new(db.pool().clone());
    channel_repo.delete_plugin(plugin_id.as_str()).await.unwrap();
    conversation_repo
        .delete(conversation_id.as_str())
        .await
        .unwrap();
    db.close().await;

    let reopened = init_database(&path)
        .await
        .expect("nullable receipt projections must not orphan startup validation");
    let projections: (Option<String>, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT channel_plugin_id, conversation_id, message_id \
         FROM channel_inbound_receipts WHERE operation_key = ?",
    )
    .bind(&operation_key)
    .fetch_one(reopened.pool())
    .await
    .unwrap();
    assert_eq!(projections, (None, None, None));

    let replay = SqliteChannelRepository::new(reopened.pool().clone())
        .claim_inbound_receipt(&nomifun_db::models::NewChannelInboundReceiptRow {
            operation_key,
            user_id: owner,
            channel_plugin_id: plugin_id.into_string(),
            platform: "telegram".into(),
            chat_id: "chat-a".into(),
            provider_event_id: "provider-event".into(),
            payload_hash,
            created_at: i64::MAX - 1,
        })
        .await
        .unwrap();
    assert!(matches!(replay, ChannelInboundClaim::Replay(_)));
}

#[test]
fn migration_files_keep_published_baseline_and_v3_has_no_relationship_tokens() {
    let migrations_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let mut files = std::fs::read_dir(&migrations_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    files.sort();
    assert_eq!(files[0], "001_v3_baseline.sql");
    assert!(files
        .iter()
        .any(|file| file == "002_idmm_action_reservations.sql"));
    assert!(files
        .iter()
        .any(|file| file == "003_channel_inbound_receipts.sql"));
    assert!(files
        .iter()
        .any(|file| file == "011_cron_run_job_projection.sql"));

    let tokens = sql_tokens(&executable_baseline_sql());
    for forbidden in [["FOREIGN", "KEY"], ["ON", "DELETE"], ["ON", "UPDATE"]] {
        assert!(
            !tokens
                .windows(forbidden.len())
                .any(|window| window.iter().map(String::as_str).eq(forbidden)),
            "forbidden v3 tokens: {forbidden:?}"
        );
    }
    for forbidden in ["REFERENCES", "CASCADE", "TRIGGER"] {
        assert!(
            !tokens.iter().any(|token| token == forbidden),
            "forbidden v3 token: {forbidden}"
        );
    }
    assert!(
        !BASELINE.contains("_row_id"),
        "v3 baseline must not reintroduce dual-key row-id columns"
    );
}

#[tokio::test]
async fn deterministic_memory_fixture_records_requested_owner_as_business_id() {
    let requested = nomifun_common::UserId::new();
    let db = init_database_memory_with_owner(requested.clone()).await.unwrap();
    assert_eq!(owner_id(db.pool()).await, requested.as_str());
    // `users.id` is the SQLite technical primary key; the business identity
    // asserted above is `users.user_id`.
    let user_technical_id: i64 =
        sqlx::query_scalar("SELECT id FROM users WHERE user_id = ?")
        .bind(requested.as_str())
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert!(user_technical_id > 0);
}

#[tokio::test]
async fn installation_identity_is_a_singleton_by_named_key() {
    let db = init_database_memory().await.unwrap();
    let result = sqlx::query(
        "INSERT INTO installation_identity (singleton_key, owner_user_id) \
         VALUES ('installation', ?)",
    )
    .bind(owner_id(db.pool()).await)
    .execute(db.pool())
    .await;
    assert!(result.is_err(), "singleton_key must remain unique");
}

#[tokio::test]
async fn users_table_accepts_nullable_columns_with_named_user_id() {
    let db = init_database_memory().await.unwrap();
    let user_id = nomifun_common::UserId::new();
    sqlx::query(
        "INSERT INTO users \
         (user_id, username, email, password_hash, avatar_path, jwt_secret, created_at, updated_at, last_login) \
         VALUES (?, 'testuser', 'test@example.com', 'hash', '/avatar.png', 'secret', 1000, 2000, 3000)",
    )
    .bind(user_id.as_str())
    .execute(db.pool())
    .await
    .unwrap();
    let row = sqlx::query("SELECT * FROM users WHERE user_id = ?")
        .bind(user_id.as_str())
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("email"), "test@example.com");
    assert_eq!(
        row.get::<Option<String>, _>("avatar_path"),
        Some("/avatar.png".to_owned())
    );
    assert_eq!(row.get::<Option<String>, _>("jwt_secret"), Some("secret".to_owned()));
    assert_eq!(row.get::<Option<i64>, _>("last_login"), Some(3000));
}

#[tokio::test]
async fn username_and_email_remain_unique() {
    let db = init_database_memory().await.unwrap();
    let first = nomifun_common::UserId::new();
    let second = nomifun_common::UserId::new();
    for (user_id, username, email) in [
        (first.as_str(), "duplicate", Some("same@example.com")),
        (second.as_str(), "duplicate", Some("same@example.com")),
    ] {
        let result = sqlx::query(
            "INSERT INTO users (user_id, username, email, password_hash, created_at, updated_at) \
             VALUES (?, ?, ?, 'h', 1, 1)",
        )
        .bind(user_id)
        .bind(username)
        .bind(email)
        .execute(db.pool())
        .await;
        if user_id == second.as_str() {
            assert!(result.is_err());
        } else {
            result.unwrap();
        }
    }
}

#[tokio::test]
async fn migration_lineage_mismatch_fails_fast_without_replacing_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nomifun-backend.db");
    let db = init_database(&path).await.unwrap();
    let old_id = nomifun_common::UserId::new();
    sqlx::query(
        "INSERT INTO users (user_id, username, password_hash, created_at, updated_at) \
         VALUES (?, 'old_dev_user', '', 1, 1)",
    )
    .bind(old_id.as_str())
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query("UPDATE _sqlx_migrations SET checksum = X'00'")
        .execute(db.pool())
        .await
        .unwrap();
    db.close().await;

    let error = init_database(&path)
        .await
        .expect_err("migration lineage mismatch must fail fast");
    let message = error.to_string().to_ascii_lowercase();
    assert!(
        message.contains("migration")
            || message.contains("version")
            || message.contains("checksum"),
        "unexpected lineage error: {error}"
    );
    assert!(path.exists(), "DB initialization must not replace the source file");
    assert!(
        !std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().contains("pre-id")),
        "DB layer must not create lineage quarantine files"
    );
}

#[tokio::test]
async fn creates_parent_directories() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sub").join("nested").join("test.db");
    let db = init_database(&path).await.unwrap();
    assert!(path.exists());
    db.close().await;
}

#[tokio::test]
async fn corruption_fails_closed_without_replacing_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let corrupt_bytes = b"not a valid sqlite database";
    std::fs::write(&path, corrupt_bytes).unwrap();

    init_database(&path)
        .await
        .expect_err("the database layer must not replace corrupted input");

    assert_eq!(std::fs::read(&path).unwrap(), corrupt_bytes);
    assert!(
        !std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().contains("backup")),
        "the database layer must not create an ad-hoc recovery dataset"
    );
}

#[test]
fn concurrent_initializers_converge_on_one_v3_migration() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nomifun-backend.db");
    let mut handles = Vec::new();
    for _ in 0..4 {
        let path = path.clone();
        handles.push(std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move { init_database(&path).await })
        }));
    }
    for handle in handles {
        let database = handle.join().expect("initializer thread");
        assert!(database.is_ok(), "concurrent v3 initialization failed: {database:?}");
    }
}
