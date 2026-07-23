use nomifun_db::{
    CreateAgentExecutionTemplateParams, IAgentExecutionTemplateRepository,
    IConversationRepository, NewAgentExecutionTemplateParticipant,
    SqliteAgentExecutionTemplateRepository, SqliteConversationRepository,
    UpdateAgentExecutionTemplateParams,
};
use nomifun_common::ConversationId;
use nomifun_db::models::ConversationRow;

const USER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000001";
const LIVE_OVERRIDE_PROVIDER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000007";
const NOMI_AGENT_ID: &str = "0190f5fe-7c00-7a00-8000-000000000114";
const PRESET_ONE_ID: &str = "0190f5fe-7c00-7a00-8000-000000000121";
const PRESET_AUDIT_ID: &str = "0190f5fe-7c00-7a00-8000-000000000122";

async fn init_database_memory() -> Result<nomifun_db::Database, nomifun_db::DbError> {
    nomifun_db::init_database_memory_with_owner(
        nomifun_common::UserId::parse(USER_ID.to_owned()).expect("canonical fixture owner"),
    )
    .await
}

async fn test_database() -> nomifun_db::Database {
    let database = init_database_memory().await.unwrap();
    for provider_id in [
        "0190f5fe-7c00-7a00-8000-000000000004",
        LIVE_OVERRIDE_PROVIDER_ID,
        "0190f5fe-7c00-7a00-8000-000000000006",
    ] {
        nomifun_db::sqlx::query(
            "INSERT INTO providers (\
                provider_id, platform, name, base_url, api_key_encrypted, models, enabled, \
                capabilities, created_at, updated_at\
             ) VALUES (?, 'openai', ?, 'https://example.invalid', \
                       'encrypted', '[]', 1, '[]', 1, 1)",
        )
        .bind(provider_id)
        .bind(provider_id)
        .execute(database.pool())
        .await
        .unwrap();
    }
    for (preset_id, source_key) in [
        (PRESET_ONE_ID, "test_preset_one"),
        (PRESET_AUDIT_ID, "test_preset_audit"),
    ] {
        nomifun_db::sqlx::query(
            "INSERT INTO presets \
             (preset_id, source_kind, source_key, name, instructions, created_at, updated_at) \
             VALUES (?, 'builtin', ?, ?, '', 1, 1)",
        )
        .bind(preset_id)
        .bind(source_key)
        .bind(source_key)
        .execute(database.pool())
        .await
        .unwrap();
        assert!(nomifun_common::validate_uuidv7(preset_id).is_ok());
    }
    database
}

fn participant(index: usize) -> NewAgentExecutionTemplateParticipant {
    NewAgentExecutionTemplateParticipant {
        template_participant_id: uuid::Uuid::now_v7().to_string(),
        source_agent_id: NOMI_AGENT_ID.to_owned(),
        preset_id: None,
        preset_revision: None,
        preset_snapshot: None,
        provider_id: Some("0190f5fe-7c00-7a00-8000-000000000006".to_owned()),
        model: Some(format!("model_{index}")),
        role: Some(format!("role {index}")),
        capability: Some(r#"{"coding":true}"#.to_owned()),
        constraints: Some(
            r#"{"max_concurrency":2,"allowed_profile_kinds":["agent"]}"#.to_owned(),
        ),
        description: Some(format!("participant {index}")),
        system_prompt: None,
        enabled_skills: r#"["skill-a"]"#.to_owned(),
        disabled_builtin_skills: "[]".to_owned(),
        sort_order: index as i64,
    }
}

#[tokio::test]
async fn template_crud_is_owner_scoped_and_keeps_only_executable_configuration() {
    let database = test_database().await;
    let repository = SqliteAgentExecutionTemplateRepository::new(database.pool().clone());
    let participants: Vec<_> = (0..64)
        .map(|index| {
            let mut participant = participant(index);
            participant.model = Some(format!("model_{}", index % 16));
            participant
        })
        .collect();
    let created = repository
        .create_template(
            USER_ID,
            &CreateAgentExecutionTemplateParams {
                name: "Large collaboration plan".to_owned(),
                description: Some("authoring configuration".to_owned()),
                max_parallel: Some(64),
                work_dir: Some("/workspace/project".to_owned()),
                context: Some(r#"{"ticket":"NOMI-37"}"#.to_owned()),
                participants,
            },
        )
        .await
        .unwrap();
    assert_eq!(created.template.version, 0);
    assert_eq!(created.template.max_parallel, Some(64));
    assert_eq!(created.participants.len(), 64);
    nomifun_db::sqlx::query(
        "UPDATE agent_execution_template_participants \
         SET constraints = '{\"max_concurrency\":65}' \
         WHERE template_id = ? AND template_participant_id = ?",
    )
    .bind(&created.template.execution_template_id)
    .bind(&created.participants[0].template_participant_id)
    .execute(database.pool())
    .await
    .unwrap();
    assert!(
        repository
            .update_template(
                USER_ID,
                &created.template.execution_template_id,
                created.template.version,
                &UpdateAgentExecutionTemplateParams {
                    participants: Some(vec![NewAgentExecutionTemplateParticipant {
                        constraints: Some(r#"{"max_concurrency":65}"#.to_owned()),
                        ..participant(0)
                    }]),
                    ..Default::default()
                },
            )
            .await
            .is_err(),
        "repository writes enforce the shared runtime concurrency ceiling"
    );
    assert!(
        repository
            .get_template(
                "0190f5fe-7c00-7a00-8000-000000000099",
                &created.template.execution_template_id,
            )
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        repository
            .list_templates(USER_ID, 20, 0)
            .await
            .unwrap()
            .len(),
        1
    );

    let usages = repository
        .list_templates_using_provider("0190f5fe-7c00-7a00-8000-000000000006")
        .await
        .unwrap();
    assert_eq!(
        usages,
        vec![(
            created.template.execution_template_id.clone(),
            "Large collaboration plan".to_owned()
        )]
    );

    assert!(
        repository
            .update_template(
                USER_ID,
                &created.template.execution_template_id,
                created.template.version,
                &UpdateAgentExecutionTemplateParams {
                    participants: Some(Vec::new()),
                    ..Default::default()
                },
            )
            .await
            .is_err(),
        "Template has no implicit empty draft state"
    );
    let updated = repository
        .update_template(
            USER_ID,
            &created.template.execution_template_id,
            created.template.version,
            &UpdateAgentExecutionTemplateParams {
                name: Some("Focused collaboration plan".to_owned()),
                participants: Some(vec![participant(0)]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.template.version, 1);
    assert_eq!(updated.participants.len(), 1);
    assert!(
        repository
            .update_template(
                USER_ID,
                &created.template.execution_template_id,
                0,
                &UpdateAgentExecutionTemplateParams::default(),
            )
            .await
            .is_err(),
        "a stale authoring write must not replace newer configuration"
    );
    assert!(
        repository
            .delete_template(
                USER_ID,
                &created.template.execution_template_id,
                updated.template.version,
            )
            .await
            .unwrap()
    );
    assert!(
        repository
            .get_template(USER_ID, &created.template.execution_template_id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn template_repository_rejects_runtime_ceiling_and_unresolved_model_debt() {
    let database = test_database().await;
    let repository = SqliteAgentExecutionTemplateRepository::new(database.pool().clone());
    let params = |max_parallel, participants| CreateAgentExecutionTemplateParams {
        name: "bounded template".to_owned(),
        description: None,
        max_parallel,
        work_dir: None,
        context: None,
        participants,
    };

    assert!(
        repository
            .create_template(USER_ID, &params(Some(65), vec![participant(0)]))
            .await
            .is_err()
    );
    let mut too_many_participants = Vec::new();
    for index in 0..65 {
        let mut member = participant(index);
        member.model = Some("shared".to_owned());
        too_many_participants.push(member);
    }
    assert!(
        repository
            .create_template(USER_ID, &params(Some(1), too_many_participants))
            .await
            .is_err()
    );
    assert!(
        repository
            .create_template(
                USER_ID,
                &params(Some(1), (0..17).map(participant).collect()),
            )
            .await
            .is_err()
    );
    let mut excessive_participant_concurrency = participant(0);
    excessive_participant_concurrency.constraints = Some(r#"{"max_concurrency":65}"#.to_owned());
    assert!(
        repository
            .create_template(
                USER_ID,
                &params(Some(1), vec![excessive_participant_concurrency]),
            )
            .await
            .is_err()
    );
    let mut unresolved = participant(0);
    unresolved.provider_id = None;
    unresolved.model = None;
    assert!(
        repository
            .create_template(USER_ID, &params(Some(1), vec![unresolved]))
            .await
            .is_err()
    );

    let mut preset_resolved = participant(0);
    preset_resolved.provider_id = None;
    preset_resolved.model = None;
    preset_resolved.preset_id = Some(PRESET_ONE_ID.to_owned());
    preset_resolved.preset_revision = Some(1);
    preset_resolved.preset_snapshot = Some(
        format!(
            r#"{{"preset_id":"{PRESET_ONE_ID}","preset_revision":1,"target":"execution_step","resolved_model":{{"provider_id":"0190f5fe-7c00-7a00-8000-000000000004","model":"model_from_preset"}}}}"#
        ),
    );
    let created = repository
        .create_template(USER_ID, &params(Some(1), vec![preset_resolved]))
        .await
        .unwrap();
    assert_eq!(
        created.participants[0].provider_id.as_deref(),
        Some("0190f5fe-7c00-7a00-8000-000000000004")
    );
    assert_eq!(
        created.participants[0].model.as_deref(),
        Some("model_from_preset")
    );

    let mut explicit_override = participant(1);
    explicit_override.provider_id = Some(LIVE_OVERRIDE_PROVIDER_ID.to_owned());
    explicit_override.model = Some("model_live_override".to_owned());
    explicit_override.preset_id = Some(PRESET_AUDIT_ID.to_owned());
    explicit_override.preset_revision = Some(3);
    explicit_override.preset_snapshot = Some(
        format!(
            r#"{{"preset_id":"{PRESET_AUDIT_ID}","preset_revision":3,"target":"execution_step","resolved_model":{{"provider_id":"0190f5fe-7c00-7a00-8000-000000000005","model":"model_snapshot_only"}}}}"#
        ),
    );
    let overridden = repository
        .create_template(USER_ID, &params(Some(1), vec![explicit_override]))
        .await
        .unwrap();
    assert_eq!(
        repository
            .list_templates_using_provider(LIVE_OVERRIDE_PROVIDER_ID)
            .await
            .unwrap(),
        vec![(
            overridden.template.execution_template_id.clone(),
            overridden.template.name.clone(),
        )],
    );
    assert!(
        repository
            .list_templates_using_provider("0190f5fe-7c00-7a00-8000-000000000005")
            .await
            .unwrap()
            .is_empty(),
        "preset_snapshot is frozen audit data; provider usage follows the materialized concrete row only"
    );
}

#[tokio::test]
async fn conversation_template_selection_is_typed_owner_scoped_and_cleared_on_delete() {
    let database = test_database().await;
    let templates = SqliteAgentExecutionTemplateRepository::new(database.pool().clone());
    let conversations = SqliteConversationRepository::new(database.pool().clone());
    let template = templates
        .create_template(
            USER_ID,
            &CreateAgentExecutionTemplateParams {
                name: "selected template".to_owned(),
                description: None,
                max_parallel: Some(1),
                work_dir: None,
                context: None,
                participants: vec![participant(0)],
            },
        )
        .await
        .unwrap();
    let now = nomifun_common::now_ms();
    let row = |user_id: &str, selection: Option<String>| ConversationRow {
        id: 0,
        conversation_id: ConversationId::new().into_string(),
        user_id: user_id.to_owned(),
        name: "template selection".to_owned(),
        r#type: "nomi".to_owned(),
        extra: r#"{"keep":"typed-only"}"#.to_owned(),
        delegation_policy: "automatic".to_owned(),
        execution_model_pool: None,
        decision_policy: "automatic".to_owned(),
        execution_template_id: selection,
        model: Some(
            r#"{"provider_id":"0190f5fe-7c00-7a00-8000-000000000006","model":"model_0"}"#
                .to_owned(),
        ),
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
    };

    let conversation_id = conversations
        .create(&row(
            USER_ID,
            Some(template.template.execution_template_id.clone()),
        ))
        .await
        .unwrap();
    assert_eq!(
        conversations
            .get(&conversation_id)
            .await
            .unwrap()
            .unwrap()
            .execution_template_id
            .as_deref(),
        Some(template.template.execution_template_id.as_str())
    );
    assert!(
        conversations
            .create(&row(USER_ID, Some("missing".to_owned())))
            .await
            .is_err()
    );
    let mut mismatched_lead = row(
        USER_ID,
        Some(template.template.execution_template_id.clone()),
    );
    mismatched_lead.model = Some(
        r#"{"provider_id":"0190f5fe-7c00-7a00-8000-000000000007","model":"model_outside"}"#.to_owned(),
    );
    assert!(
        conversations.create(&mismatched_lead).await.is_err(),
        "a selected template must contain the effective Conversation lead"
    );

    nomifun_db::sqlx::query(
        "INSERT INTO users (user_id, username, password_hash, created_at, updated_at) \
         VALUES ('0190f5fe-7c00-7a00-8000-000000000002', 'other_user', 'hash', ?, ?)",
    )
    .bind(now)
    .bind(now)
    .execute(database.pool())
    .await
    .unwrap();
    assert!(
        conversations
            .create(&row(
                "0190f5fe-7c00-7a00-8000-000000000002",
                Some(template.template.execution_template_id.clone()),
            ))
            .await
            .is_err()
    );
    assert!(
        nomifun_db::sqlx::query(
            "UPDATE conversations SET execution_template_id = 'missing' \
             WHERE conversation_id = ?",
        )
        .bind(&conversation_id)
        .execute(database.pool())
        .await
        .is_ok(),
        "cross-row target validity is repository-owned"
    );
    nomifun_db::sqlx::query(
        "UPDATE conversations SET execution_template_id = ? WHERE conversation_id = ?",
    )
    .bind(&template.template.execution_template_id)
    .bind(&conversation_id)
    .execute(database.pool())
    .await
    .unwrap();
    assert!(
        nomifun_db::sqlx::query(
            "UPDATE conversations \
             SET model = '{\"provider_id\":\"0190f5fe-7c00-7a00-8000-000000000007\",\"model\":\"model_outside\"}' \
             WHERE conversation_id = ?",
        )
        .bind(&conversation_id)
        .execute(database.pool())
        .await
        .is_ok(),
        "cross-row template/model consistency is repository-owned"
    );
    nomifun_db::sqlx::query(
        "UPDATE conversations \
         SET model = '{\"provider_id\":\"0190f5fe-7c00-7a00-8000-000000000006\",\"model\":\"model_0\"}' \
         WHERE conversation_id = ?",
    )
    .bind(&conversation_id)
    .execute(database.pool())
    .await
    .unwrap();

    let replacement = templates
        .update_template(
            USER_ID,
            &template.template.execution_template_id,
            template.template.version,
            &UpdateAgentExecutionTemplateParams {
                participants: Some(vec![participant(1)]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        conversations
            .get(&conversation_id)
            .await
            .unwrap()
            .unwrap()
            .execution_template_id,
        None,
        "participant replacement atomically clears selections whose lead is no longer present"
    );

    assert!(
        templates
            .delete_template(
                USER_ID,
                &template.template.execution_template_id,
                replacement.template.version,
            )
            .await
            .unwrap()
    );
    assert_eq!(
        conversations
            .get(&conversation_id)
            .await
            .unwrap()
            .unwrap()
            .execution_template_id,
        None,
        "template deletion explicitly clears future authoring selection"
    );
}

#[tokio::test]
async fn template_repository_rejects_lossy_or_legacy_participant_shapes() {
    let database = test_database().await;
    let repository = SqliteAgentExecutionTemplateRepository::new(database.pool().clone());

    let mut unpaired_model = participant(0);
    unpaired_model.model = None;
    assert!(
        repository
            .create_template(
                USER_ID,
                &CreateAgentExecutionTemplateParams {
                    name: "invalid model".to_owned(),
                    description: None,
                    max_parallel: None,
                    work_dir: None,
                    context: None,
                    participants: vec![unpaired_model],
                },
            )
            .await
            .is_err()
    );

    let mut legacy_constraints = participant(1);
    legacy_constraints.constraints = Some(r#"{"allowed_task_kinds":["agent"]}"#.to_owned());
    assert!(
        repository
            .create_template(
                USER_ID,
                &CreateAgentExecutionTemplateParams {
                    name: "legacy constraints".to_owned(),
                    description: None,
                    max_parallel: None,
                    work_dir: None,
                    context: None,
                    participants: vec![legacy_constraints],
                },
            )
            .await
            .is_err()
    );

    let mut invalid_snapshot = participant(2);
    invalid_snapshot.preset_id = Some(PRESET_ONE_ID.to_owned());
    invalid_snapshot.preset_revision = Some(1);
    invalid_snapshot.preset_snapshot = Some(
        format!(
            r#"{{"preset_id":"{PRESET_ONE_ID}","preset_revision":1,"target":"cluster_member"}}"#
        ),
    );
    assert!(
        repository
            .create_template(
                USER_ID,
                &CreateAgentExecutionTemplateParams {
                    name: "legacy preset target".to_owned(),
                    description: None,
                    max_parallel: None,
                    work_dir: None,
                    context: None,
                    participants: vec![invalid_snapshot],
                },
            )
            .await
            .is_err()
    );
}
