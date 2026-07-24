//! Black-box integration tests for the v3 logical-reference channel schema.

use std::sync::Arc;

use nomifun_db::models::{
    ChannelPluginRow, ChannelUserRow, NewChannelPairingCodeRow, NewChannelPluginRow,
    NewChannelSessionRow, NewChannelUserRow,
};
use nomifun_db::{
    DbError, IChannelRepository, SqliteChannelRepository, UpdatePluginStatusParams,
    init_database_memory,
};

async fn repo() -> (Arc<dyn IChannelRepository>, nomifun_db::Database) {
    let db = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteChannelRepository::new(db.pool().clone()));
    (repo as Arc<dyn IChannelRepository>, db)
}

#[tokio::test]
async fn channel_schema_has_only_canonical_tables_and_no_physical_foreign_keys() {
    let (_repo, db) = repo().await;

    let canonical_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master \
         WHERE type = 'table' AND name IN \
         ('channel_plugins', 'channel_users', 'channel_sessions', 'channel_pairing_codes')",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(canonical_count, 4);

    let legacy_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master \
         WHERE type = 'table' AND name IN \
         ('assistant_plugins', 'assistant_users', 'assistant_sessions', 'assistant_pairing_codes')",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(legacy_count, 0);

    let mut physical_fk_count = 0_i64;
    for table in [
        "channel_plugins",
        "channel_users",
        "channel_sessions",
        "channel_pairing_codes",
    ] {
        let sql = format!("SELECT COUNT(*) FROM pragma_foreign_key_list('{table}')");
        physical_fk_count += sqlx::query_scalar::<_, i64>(&sql)
            .fetch_one(db.pool())
            .await
            .unwrap();
    }
    assert_eq!(physical_fk_count, 0);
}

#[tokio::test]
async fn channel_user_has_no_reverse_session_relation_or_index() {
    let (_repo, db) = repo().await;

    let columns: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pragma_table_info('channel_users') ORDER BY cid")
            .fetch_all(db.pool())
            .await
            .unwrap();
    assert!(!columns.iter().any(|column| column == "channel_session_id"));

    let indexes: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pragma_index_list('channel_users')")
            .fetch_all(db.pool())
            .await
            .unwrap();
    assert!(!indexes
        .iter()
        .any(|index| index == "idx_channel_users_channel_session_id"));
}

fn plugin_fixture(plugin_type: &str, bot_key: &str) -> NewChannelPluginRow {
    let now = nomifun_common::now_ms();
    NewChannelPluginRow {
        r#type: plugin_type.into(),
        name: format!("{plugin_type} bot"),
        enabled: false,
        config: r#"{"credentials":{}}"#.into(),
        status: None,
        last_connected: None,
        companion_id: None,
        public_agent_id: None,
        bot_key: Some(bot_key.into()),
        created_at: now,
        updated_at: now,
    }
}

async fn create_plugin(
    repo: &Arc<dyn IChannelRepository>,
    plugin_type: &str,
    bot_key: &str,
) -> ChannelPluginRow {
    repo.create_plugin(&plugin_fixture(plugin_type, bot_key))
        .await
        .unwrap()
}

fn user_fixture(
    channel_plugin_id: &str,
    platform_user_id: &str,
    platform_type: &str,
) -> NewChannelUserRow {
    let now = nomifun_common::now_ms();
    NewChannelUserRow {
        platform_user_id: platform_user_id.into(),
        platform_type: platform_type.into(),
        channel_plugin_id: Some(channel_plugin_id.to_owned()),
        display_name: Some(format!("User {platform_user_id}")),
        authorized_at: now,
        last_active: None,
    }
}

async fn create_user(
    repo: &Arc<dyn IChannelRepository>,
    channel_plugin_id: &str,
    platform_user_id: &str,
) -> ChannelUserRow {
    repo.create_user(&user_fixture(
        channel_plugin_id,
        platform_user_id,
        "telegram",
    ))
    .await
    .unwrap()
}

fn session_fixture(
    channel_user_id: &str,
    channel_plugin_id: &str,
    chat_id: &str,
) -> NewChannelSessionRow {
    let now = nomifun_common::now_ms();
    NewChannelSessionRow {
        channel_session_id: nomifun_common::ChannelSessionId::new().into_string(),
        channel_user_id: channel_user_id.to_owned(),
        agent_type: "acp".into(),
        conversation_id: None,
        workspace: None,
        chat_id: Some(chat_id.into()),
        channel_plugin_id: Some(channel_plugin_id.to_owned()),
        created_at: now,
        last_activity: now,
    }
}

fn pairing_fixture(
    code: &str,
    platform_user_id: &str,
    expires_offset_ms: i64,
) -> NewChannelPairingCodeRow {
    let now = nomifun_common::now_ms();
    NewChannelPairingCodeRow {
        code: code.into(),
        platform_user_id: platform_user_id.into(),
        platform_type: "telegram".into(),
        channel_plugin_id: None,
        display_name: Some("Tester".into()),
        requested_at: now,
        expires_at: now + expires_offset_ms,
        status: "pending".into(),
    }
}

#[tokio::test]
async fn plugin_full_lifecycle() {
    let (repo, _db) = repo().await;
    let telegram = create_plugin(&repo, "telegram", "telegram-bot").await;
    let lark = create_plugin(&repo, "lark", "lark-bot").await;

    repo.update_plugin_status(
        &telegram.channel_plugin_id,
        &UpdatePluginStatusParams {
            status: Some("running".into()),
            enabled: Some(true),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let telegram = repo
        .get_plugin(&telegram.channel_plugin_id)
        .await
        .unwrap()
        .unwrap();
    assert!(telegram.enabled);
    assert_eq!(telegram.status.as_deref(), Some("running"));

    repo.delete_plugin(&lark.channel_plugin_id).await.unwrap();
    assert_eq!(repo.get_all_plugins().await.unwrap().len(), 1);
}

#[tokio::test]
async fn duplicate_platform_user_is_rejected_within_one_plugin() {
    let (repo, _db) = repo().await;
    let plugin = create_plugin(&repo, "telegram", "telegram-bot").await;
    create_user(&repo, &plugin.channel_plugin_id, "tg_100").await;

    let duplicate = user_fixture(&plugin.channel_plugin_id, "tg_100", "telegram");
    assert!(matches!(
        repo.create_user(&duplicate).await,
        Err(DbError::Conflict(_))
    ));
}

#[tokio::test]
async fn deleting_user_transactionally_cascades_authoritative_sessions() {
    let (repo, _db) = repo().await;
    let plugin = create_plugin(&repo, "telegram", "telegram-bot").await;
    let user = create_user(&repo, &plugin.channel_plugin_id, "tg_1").await;

    for chat_id in ["chat-a", "chat-b"] {
        repo.get_or_create_session(
            &user.channel_user_id,
            chat_id,
            &plugin.channel_plugin_id,
            &session_fixture(
                &user.channel_user_id,
                &plugin.channel_plugin_id,
                chat_id,
            ),
        )
        .await
        .unwrap();
    }
    assert_eq!(repo.get_all_sessions().await.unwrap().len(), 2);

    repo.delete_user(&user.channel_user_id).await.unwrap();
    assert!(repo.get_all_sessions().await.unwrap().is_empty());
}

#[tokio::test]
async fn session_identity_is_scoped_by_plugin_user_and_chat() {
    let (repo, _db) = repo().await;
    let plugin = create_plugin(&repo, "telegram", "telegram-bot").await;
    let user_a = create_user(&repo, &plugin.channel_plugin_id, "tg_1").await;
    let user_b = create_user(&repo, &plugin.channel_plugin_id, "tg_2").await;

    let a1 = repo
        .get_or_create_session(
            &user_a.channel_user_id,
            "chat-a",
            &plugin.channel_plugin_id,
            &session_fixture(
                &user_a.channel_user_id,
                &plugin.channel_plugin_id,
                "chat-a",
            ),
        )
        .await
        .unwrap();
    let a1_replayed = repo
        .get_or_create_session(
            &user_a.channel_user_id,
            "chat-a",
            &plugin.channel_plugin_id,
            &session_fixture(
                &user_a.channel_user_id,
                &plugin.channel_plugin_id,
                "chat-a",
            ),
        )
        .await
        .unwrap();
    let a2 = repo
        .get_or_create_session(
            &user_a.channel_user_id,
            "chat-b",
            &plugin.channel_plugin_id,
            &session_fixture(
                &user_a.channel_user_id,
                &plugin.channel_plugin_id,
                "chat-b",
            ),
        )
        .await
        .unwrap();
    let b1 = repo
        .get_or_create_session(
            &user_b.channel_user_id,
            "chat-a",
            &plugin.channel_plugin_id,
            &session_fixture(
                &user_b.channel_user_id,
                &plugin.channel_plugin_id,
                "chat-a",
            ),
        )
        .await
        .unwrap();

    assert_eq!(a1.channel_session_id, a1_replayed.channel_session_id);
    assert_ne!(a1.channel_session_id, a2.channel_session_id);
    assert_ne!(a1.channel_session_id, b1.channel_session_id);
}

#[tokio::test]
async fn pairing_expiry_and_status_transitions() {
    let (repo, _db) = repo().await;
    let now = nomifun_common::now_ms();
    repo.create_pairing(&pairing_fixture("111111", "tg_1", -1_000))
        .await
        .unwrap();
    repo.create_pairing(&pairing_fixture("222222", "tg_2", 600_000))
        .await
        .unwrap();

    assert_eq!(repo.cleanup_expired_pairings(now).await.unwrap(), 1);
    assert_eq!(
        repo.get_pairing_by_code("111111")
            .await
            .unwrap()
            .unwrap()
            .status,
        "expired"
    );

    repo.update_pairing_status("222222", "approved")
        .await
        .unwrap();
    assert_eq!(
        repo.get_pairing_by_code("222222")
            .await
            .unwrap()
            .unwrap()
            .status,
        "approved"
    );
}
