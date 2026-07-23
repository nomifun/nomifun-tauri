use nomifun_common::{ConversationId, MessageId};
use nomifun_db::{
    IConversationRepository, SqliteConversationRepository, init_database_memory,
    installation_owner_id,
    models::{ConversationRow, MessageRow},
};
use sqlx::Row;

async fn conversation_fixture(pool: &sqlx::SqlitePool) -> String {
    let conversation_id = ConversationId::new().into_string();
    let user_id = installation_owner_id(pool).await.unwrap();
    sqlx::query(
        "INSERT INTO conversations
            (conversation_id, user_id, name, type, created_at, updated_at)
         VALUES (?, ?, 'Fixture conversation', 'nomi', 1000, 1000)",
    )
    .bind(&conversation_id)
    .bind(user_id)
    .execute(pool)
    .await
    .unwrap();
    conversation_id
}

async fn message_fixture(pool: &sqlx::SqlitePool, conversation_id: &str) -> String {
    let message_id = MessageId::new().into_string();
    sqlx::query(
        "INSERT INTO messages
            (message_id, conversation_id, msg_id, type, content, position, status, created_at)
         VALUES (?, ?, ?, 'text', '{\"content\":\"fixture\"}', 'right', 'finish', 1000)",
    )
    .bind(&message_id)
    .bind(conversation_id)
    .bind(&message_id)
    .execute(pool)
    .await
    .unwrap();
    message_id
}

fn conversation_row(user_id: &str) -> ConversationRow {
    let conversation_id = ConversationId::new().into_string();
    ConversationRow {
        id: 0,
        conversation_id,
        user_id: user_id.to_owned(),
        name: "FromRow conversation".to_owned(),
        r#type: "acp".to_owned(),
        extra: r#"{"workspace":"/tmp"}"#.to_owned(),
        delegation_policy: "automatic".to_owned(),
        execution_model_pool: None,
        decision_policy: "automatic".to_owned(),
        execution_template_id: None,
        model: None,
        status: Some("running".to_owned()),
        source: Some("nomifun".to_owned()),
        channel_chat_id: Some("group:42".to_owned()),
        pinned: true,
        pinned_at: Some(1700000000000),
        cron_job_id: None,
        preset_id: None,
        preset_revision: None,
        preset_snapshot: None,
        created_at: 1000,
        updated_at: 2000,
    }
}

#[tokio::test]
async fn baseline_creates_conversation_and_message_tables() {
    let db = init_database_memory().await.unwrap();

    for table in ["conversations", "messages"] {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?",
        )
        .bind(table)
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(count, 1, "{table} must exist in the v3 baseline");
    }
}

#[tokio::test]
async fn product_tables_use_integer_autoincrement_primary_ids() {
    let db = init_database_memory().await.unwrap();

    for table in ["conversations", "messages"] {
        let sql: String = sqlx::query_scalar(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?",
        )
        .bind(table)
        .fetch_one(db.pool())
        .await
        .unwrap();
        let normalized = sql.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            normalized.contains("id INTEGER PRIMARY KEY AUTOINCREMENT"),
            "{table} must use the v3 technical primary-key contract"
        );
    }

    let conversation_id = conversation_fixture(db.pool()).await;
    let row = sqlx::query("SELECT id FROM conversations WHERE conversation_id = ?")
        .bind(conversation_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert!(row.get::<i64, _>("id") > 0);
}

#[tokio::test]
async fn business_ids_are_lowercase_uuidv7_without_prefixes() {
    let db = init_database_memory().await.unwrap();
    let conversation_id = conversation_fixture(db.pool()).await;
    let message_id = message_fixture(db.pool(), &conversation_id).await;

    assert!(ConversationId::parse(&conversation_id).is_ok());
    assert!(MessageId::parse(&message_id).is_ok());
    assert!(!conversation_id.starts_with("conv_"));
    assert!(!message_id.starts_with("msg_"));
}

#[tokio::test]
async fn conversation_defaults_match_the_baseline() {
    let db = init_database_memory().await.unwrap();
    let conversation_id = conversation_fixture(db.pool()).await;
    let row = sqlx::query(
        "SELECT extra, pinned, pinned_at, model, source, delegation_policy,
                decision_policy
           FROM conversations WHERE conversation_id = ?",
    )
    .bind(conversation_id)
    .fetch_one(db.pool())
    .await
    .unwrap();

    assert_eq!(row.get::<String, _>("extra"), "{}");
    assert_eq!(row.get::<i64, _>("pinned"), 0);
    assert!(row.get::<Option<i64>, _>("pinned_at").is_none());
    assert!(row.get::<Option<String>, _>("model").is_none());
    assert!(row.get::<Option<String>, _>("source").is_none());
    assert_eq!(row.get::<String, _>("delegation_policy"), "automatic");
    assert_eq!(row.get::<String, _>("decision_policy"), "automatic");
}

#[tokio::test]
async fn conversation_checks_reject_invalid_values() {
    let db = init_database_memory().await.unwrap();
    let user_id = installation_owner_id(db.pool()).await.unwrap();

    let invalid_status = sqlx::query(
        "INSERT INTO conversations
            (conversation_id, user_id, name, type, status, created_at, updated_at)
         VALUES (?, ?, 'invalid', 'nomi', 'not-a-status', 1000, 1000)",
    )
    .bind(ConversationId::new().into_string())
    .bind(&user_id)
    .execute(db.pool())
    .await;
    assert!(invalid_status.is_err());

    let invalid_policy = sqlx::query(
        "INSERT INTO conversations
            (conversation_id, user_id, name, type, delegation_policy, created_at, updated_at)
         VALUES (?, ?, 'invalid', 'nomi', 'not-a-policy', 1000, 1000)",
    )
    .bind(ConversationId::new().into_string())
    .bind(&user_id)
    .execute(db.pool())
    .await;
    assert!(invalid_policy.is_err());
}

#[tokio::test]
async fn message_defaults_and_checks_match_the_baseline() {
    let db = init_database_memory().await.unwrap();
    let conversation_id = conversation_fixture(db.pool()).await;
    let message_id = MessageId::new().into_string();
    sqlx::query(
        "INSERT INTO messages (message_id, conversation_id, type, created_at)
         VALUES (?, ?, 'text', 1000)",
    )
    .bind(&message_id)
    .bind(&conversation_id)
    .execute(db.pool())
    .await
    .unwrap();

    let row = sqlx::query(
        "SELECT content, hidden, msg_id, position, status
           FROM messages WHERE message_id = ?",
    )
    .bind(&message_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("content"), "{}");
    assert_eq!(row.get::<i64, _>("hidden"), 0);
    assert!(row.get::<Option<String>, _>("msg_id").is_none());
    assert!(row.get::<Option<String>, _>("position").is_none());
    assert!(row.get::<Option<String>, _>("status").is_none());

    let invalid_position = sqlx::query(
        "INSERT INTO messages
            (message_id, conversation_id, type, position, created_at)
         VALUES (?, ?, 'text', 'not-a-position', 1000)",
    )
    .bind(MessageId::new().into_string())
    .bind(&conversation_id)
    .execute(db.pool())
    .await;
    assert!(invalid_position.is_err());
}

#[tokio::test]
async fn raw_deletes_leave_logical_children_unchanged() {
    let db = init_database_memory().await.unwrap();
    let conversation_id = conversation_fixture(db.pool()).await;
    let message_id = message_fixture(db.pool(), &conversation_id).await;

    let orphan_message_id = MessageId::new().into_string();
    sqlx::query(
        "INSERT INTO messages (message_id, conversation_id, type, created_at)
         VALUES (?, ?, 'text', 1000)",
    )
    .bind(&orphan_message_id)
    .bind(ConversationId::new().into_string())
    .execute(db.pool())
    .await
    .unwrap();

    sqlx::query("DELETE FROM conversations WHERE conversation_id = ?")
        .bind(&conversation_id)
        .execute(db.pool())
        .await
        .unwrap();

    let retained: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM messages WHERE message_id IN (?, ?)",
    )
    .bind(&message_id)
    .bind(&orphan_message_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(retained, 2);
}

#[tokio::test]
async fn conversation_repository_owns_logical_cleanup() {
    let db = init_database_memory().await.unwrap();
    let repository = SqliteConversationRepository::new(db.pool().clone());
    let user_id = installation_owner_id(db.pool()).await.unwrap();
    let row = conversation_row(&user_id);
    let conversation_id = row.conversation_id.clone();
    repository.create(&row).await.unwrap();
    message_fixture(db.pool(), &conversation_id).await;

    repository.delete(&conversation_id).await.unwrap();

    let conversations: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM conversations WHERE conversation_id = ?",
    )
    .bind(&conversation_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    let messages: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id = ?")
            .bind(&conversation_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(conversations, 0);
    assert_eq!(messages, 0);
}

#[tokio::test]
async fn conversation_and_message_from_row_match_select_star() {
    let db = init_database_memory().await.unwrap();
    let repository = SqliteConversationRepository::new(db.pool().clone());
    let user_id = installation_owner_id(db.pool()).await.unwrap();
    let conversation = conversation_row(&user_id);
    let conversation_id = conversation.conversation_id.clone();
    repository.create(&conversation).await.unwrap();
    let message_id = message_fixture(db.pool(), &conversation_id).await;

    let stored: ConversationRow =
        sqlx::query_as("SELECT * FROM conversations WHERE conversation_id = ?")
            .bind(&conversation_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert!(stored.id > 0);
    assert_eq!(stored.conversation_id, conversation_id);
    assert_eq!(stored.user_id, user_id);
    assert_eq!(stored.name, "FromRow conversation");
    assert_eq!(stored.r#type, "acp");
    assert_eq!(stored.extra, r#"{"workspace":"/tmp"}"#);
    assert!(stored.pinned);
    assert_eq!(stored.pinned_at, Some(1700000000000));

    let stored_message: MessageRow =
        sqlx::query_as("SELECT * FROM messages WHERE message_id = ?")
            .bind(&message_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert!(stored_message.id > 0);
    assert_eq!(stored_message.message_id, message_id);
    assert_eq!(stored_message.conversation_id, conversation_id);
    assert_eq!(stored_message.msg_id.as_deref(), Some(message_id.as_str()));
    assert_eq!(stored_message.r#type, "text");
    assert_eq!(stored_message.position.as_deref(), Some("right"));
    assert_eq!(stored_message.status.as_deref(), Some("finish"));
}

#[tokio::test]
async fn conversation_and_message_indexes_exist() {
    let db = init_database_memory().await.unwrap();
    let indexes: Vec<(String, String)> = sqlx::query_as(
        "SELECT name, tbl_name FROM sqlite_master
           WHERE type = 'index' AND name IN (
             'idx_conversations_user_id',
             'idx_conversations_cron_job_id',
             'idx_conversations_preset_id',
             'idx_conversations_execution_template_id',
             'idx_messages_conversation_id',
             'idx_messages_conv_created'
           )",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert_eq!(indexes.len(), 6);
}
