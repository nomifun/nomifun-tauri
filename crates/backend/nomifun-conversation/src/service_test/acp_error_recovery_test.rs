use super::*;

#[tokio::test]
async fn send_message_evicts_acp_task_after_terminal_error() {
    let (svc, _broadcaster, _repo, _default_runtime_registry) = make_service();
    let runtime_registry = Arc::new(MockAgentRuntimeRegistry::new());
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();

    let scripted_agent = Arc::new(ScriptedAgent::new(
        &conv.conversation_id,
        vec![vec![AgentStreamEvent::Error(ErrorEventData::legacy(
            "Agent completed the turn without producing visible output.",
            Some(AgentErrorCode::UnknownUpstreamError),
        ))]],
    ));
    runtime_registry.insert_agent(&conv.conversation_id, AgentRuntimeHandle::Mock(scripted_agent));

    let runtime_registry_dyn: Arc<dyn AgentRuntimeRegistry> = runtime_registry.clone();
    svc.send_message_with_idempotency_key(
        TEST_USER_1,
        &conv.conversation_id,
        "acp-recovery-evicts-terminal-error",
        make_send_req(),
        &runtime_registry_dyn,
    )
    .await
    .unwrap();

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if runtime_registry.termination_count() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("ACP terminal error should evict the cached runtime");
    wait_for_turn_released(&svc, &conv.conversation_id).await;

    assert_eq!(runtime_registry.active_runtime_count(), 0);
    assert_eq!(
        runtime_registry.termination_records(),
        vec![(conv.conversation_id.clone(), Some(AgentKillReason::AgentErrorRecovery))]
    );
}

#[tokio::test]
async fn send_message_clears_persisted_acp_model_after_model_not_found() {
    let acp_session_repo = Arc::new(StubAcpSessionRepo::default());
    let (svc, _broadcaster, repo, _default_runtime_registry) = make_service_with_resolver_and_acp_session_repo(
        Arc::new(FixedSkillResolver { names: vec![] }),
        acp_session_repo.clone(),
    );
    let runtime_registry = Arc::new(MockAgentRuntimeRegistry::new());
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();
    let workspace = conv.extra["workspace"].clone();
    repo.update(
        &conv.conversation_id,
        &ConversationRowUpdate {
            extra: Some(
                serde_json::to_string(&json!({
                    "workspace": workspace,
                    "current_model_id": "deepseek-v4-pro",
                }))
                .unwrap(),
            ),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let scripted_agent = Arc::new(ScriptedAgent::new(
        &conv.conversation_id,
        vec![vec![AgentStreamEvent::Error(ErrorEventData::legacy(
            "The configured model was not found by the provider.",
            Some(AgentErrorCode::UserLlmProviderModelNotFound),
        ))]],
    ));
    runtime_registry.insert_agent(&conv.conversation_id, AgentRuntimeHandle::Mock(scripted_agent));

    let runtime_registry_dyn: Arc<dyn AgentRuntimeRegistry> = runtime_registry.clone();
    svc.send_message_with_idempotency_key(
        TEST_USER_1,
        &conv.conversation_id,
        "acp-recovery-clears-missing-model",
        make_send_req(),
        &runtime_registry_dyn,
    )
    .await
    .unwrap();

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let saves = acp_session_repo.runtime_state_saves();
            if saves
                .iter()
                .any(|call| call.conversation_id == conv.conversation_id.clone() && call.current_model_id == Some(None))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("model_not_found should clear persisted ACP model");
    wait_for_turn_released(&svc, &conv.conversation_id).await;

    assert_eq!(runtime_registry.active_runtime_count(), 0);
    assert_eq!(
        acp_session_repo.runtime_state_saves(),
        vec![RuntimeStateSaveCall {
            conversation_id: conv.conversation_id.clone(),
            current_model_id: Some(None),
        }]
    );

    let row = repo.get(&conv.conversation_id).await.unwrap().unwrap();
    let extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap();
    assert!(extra.get("workspace").is_some());
    assert!(
        extra.get("current_model_id").is_none(),
        "model_not_found must clear conversation.extra.current_model_id so rebuild cannot reseed stale desired model"
    );
}

#[tokio::test]
async fn send_message_does_not_clear_persisted_acp_model_for_other_terminal_errors() {
    let acp_session_repo = Arc::new(StubAcpSessionRepo::default());
    let (svc, _broadcaster, _repo, _default_runtime_registry) = make_service_with_resolver_and_acp_session_repo(
        Arc::new(FixedSkillResolver { names: vec![] }),
        acp_session_repo.clone(),
    );
    let runtime_registry = Arc::new(MockAgentRuntimeRegistry::new());
    let conv = svc.create(TEST_USER_1, make_create_req()).await.unwrap();

    let scripted_agent = Arc::new(ScriptedAgent::new(
        &conv.conversation_id,
        vec![vec![AgentStreamEvent::Error(ErrorEventData::legacy(
            "Unknown upstream error.",
            Some(AgentErrorCode::UnknownUpstreamError),
        ))]],
    ));
    runtime_registry.insert_agent(&conv.conversation_id, AgentRuntimeHandle::Mock(scripted_agent));

    let runtime_registry_dyn: Arc<dyn AgentRuntimeRegistry> = runtime_registry.clone();
    svc.send_message_with_idempotency_key(
        TEST_USER_1,
        &conv.conversation_id,
        "acp-recovery-keeps-model-other-error",
        make_send_req(),
        &runtime_registry_dyn,
    )
    .await
    .unwrap();
    wait_for_turn_released(&svc, &conv.conversation_id).await;

    assert_eq!(runtime_registry.active_runtime_count(), 0);
    assert!(acp_session_repo.runtime_state_saves().is_empty());
}
