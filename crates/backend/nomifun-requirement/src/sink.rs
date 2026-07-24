use std::sync::Arc;

use async_trait::async_trait;
use nomifun_ai_agent::RequirementSink;
use nomifun_api_types::{
    AutoWorkTargetKind, CreateRequirementRequest, RequirementStatus,
};
use nomifun_common::RequirementCreator;

use crate::service::RequirementService;

/// Backend implementation of the agent-side `RequirementSink` trait, delegating
/// to `RequirementService`. Injected into the nomi engine via the agent factory.
pub struct RequirementServiceSink {
    service: Arc<RequirementService>,
}

impl RequirementServiceSink {
    /// Build the sink as a trait object ready to inject into the agent factory.
    pub fn into_arc(service: Arc<RequirementService>) -> Arc<dyn RequirementSink> {
        Arc::new(Self { service })
    }

    /// Build the same sink as a [`RequirementCreator`] trait object for the
    /// opt-in IM → requirement pipeline (channel inbound → tracked requirement).
    pub fn creator_arc(service: Arc<RequirementService>) -> Arc<dyn RequirementCreator> {
        Arc::new(Self { service })
    }

    async fn verify_conversation_claim(
        &self,
        owner_conversation_id: &str,
        requirement_id: &str,
        claim_generation: i64,
        claim_token: &str,
    ) -> Result<(), String> {
        if claim_generation <= 0 {
            return Err("claim_generation must be a positive integer".into());
        }
        if claim_token.len() != 64
            || !claim_token
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err("claim_token must be a canonical opaque token".into());
        }
        let authorized = self
            .service
            .verify_active_claim_exact(
                requirement_id,
                claim_generation,
                claim_token,
                Some(owner_conversation_id),
                None,
            )
            .await
            .map_err(|error| error.to_string())?;
        if !authorized {
            return Err(format!(
                "stale or unauthorized requirement claim {requirement_id} generation {claim_generation}"
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl RequirementCreator for RequirementServiceSink {
    async fn create_from_message(
        &self,
        title: &str,
        content: &str,
        tag: &str,
        created_by: &str,
    ) -> Result<String, String> {
        let req = CreateRequirementRequest {
            title: title.to_string(),
            content: content.to_string(),
            tag: tag.to_string(),
            order_key: None,
            status: None, // None → Pending → wakes AutoWork
            created_by: Some(created_by.to_string()),
            attachments: Vec::new(),
        };
        self.service
            .create(req)
            .await
            .map(|r| r.requirement_id)
            .map_err(|e| e.to_string())
    }
}

#[async_trait]
impl RequirementSink for RequirementServiceSink {
    async fn complete(
        &self,
        owner_conversation_id: &str,
        requirement_id: &str,
        claim_generation: i64,
        claim_token: &str,
        note: &str,
    ) -> Result<(), String> {
        self.verify_conversation_claim(
            owner_conversation_id,
            requirement_id,
            claim_generation,
            claim_token,
        )
        .await?;
        let resolved = self
            .service
            .resolve_claim_verdict_exact(
                requirement_id,
                claim_generation,
                claim_token,
                owner_conversation_id,
                AutoWorkTargetKind::Conversation,
                RequirementStatus::Done,
                Some(note.to_string()),
            )
            .await
            .map_err(|error| error.to_string())?;
        match resolved {
            Some(requirement)
                if requirement.status == RequirementStatus::Done
                    && requirement.owner_conversation_id.as_deref()
                        == Some(owner_conversation_id)
                    && requirement.owner_terminal_id.is_none() =>
            {
                Ok(())
            }
            _ => Err(format!(
                "requirement claim {requirement_id} generation {claim_generation} lost authority before completion"
            )),
        }
    }

    async fn update_status(
        &self,
        owner_conversation_id: &str,
        requirement_id: &str,
        claim_generation: i64,
        claim_token: &str,
        status: &str,
        note: Option<&str>,
    ) -> Result<(), String> {
        self.verify_conversation_claim(
            owner_conversation_id,
            requirement_id,
            claim_generation,
            claim_token,
        )
        .await?;
        let parsed = match status {
            "in_progress" => RequirementStatus::InProgress,
            "done" => RequirementStatus::Done,
            "failed" => RequirementStatus::Failed,
            other => return Err(format!("invalid status '{other}'")),
        };
        // A claimed row is already `in_progress`; this declaration is an exact
        // generation validation, not a state mutation. Terminal verdicts go
        // through the single-SQL generation CAS below.
        if parsed == RequirementStatus::InProgress {
            return Ok(());
        }
        let resolved = self
            .service
            .resolve_claim_verdict_exact(
                requirement_id,
                claim_generation,
                claim_token,
                owner_conversation_id,
                AutoWorkTargetKind::Conversation,
                parsed,
                note.map(str::to_owned),
            )
            .await
            .map_err(|error| error.to_string())?;
        match resolved {
            Some(requirement)
                if requirement.status == parsed
                    && requirement.owner_conversation_id.as_deref()
                        == Some(owner_conversation_id)
                    && requirement.owner_terminal_id.is_none() =>
            {
                Ok(())
            }
            _ => Err(format!(
                "requirement claim {requirement_id} generation {claim_generation} lost authority before verdict"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::RequirementEventEmitter;
    use nomifun_common::ConversationId;
    use nomifun_db::{IRequirementRepository, SqliteRequirementRepository, init_database_memory};
    use nomifun_realtime::UserEventSink;

    #[derive(Default)]
    struct NoopBroadcaster;

    impl UserEventSink for NoopBroadcaster {
        fn send_to_user(
            &self,
            _user_id: &str,
            _event: nomifun_api_types::WebSocketMessage<serde_json::Value>,
        ) {
        }
    }

    async fn conversation_sink_with_claim() -> (
        RequirementServiceSink,
        String,
        String,
        String,
        sqlx::SqlitePool,
    ) {
        let db = init_database_memory().await.expect("in-memory database");
        let pool = db.pool().clone();
        let installation_owner = nomifun_db::installation_owner_id(db.pool())
            .await
            .expect("installation owner");
        let conversation_id = ConversationId::new().into_string();
        sqlx::query(
            "INSERT INTO conversations \
                 (conversation_id, user_id, name, type, created_at, updated_at) \
             VALUES (?1, ?2, 'Requirement Sink Conversation', 'nomi', 0, 0)",
        )
        .bind(&conversation_id)
        .bind(&installation_owner)
        .execute(db.pool())
        .await
        .expect("conversation");
        let repo: Arc<dyn IRequirementRepository> =
            Arc::new(SqliteRequirementRepository::new(pool.clone()));
        let emitter = RequirementEventEmitter::new(
            Arc::new(NoopBroadcaster),
            Arc::from(installation_owner.as_str()),
        );
        let service = Arc::new(RequirementService::new(repo, emitter));
        let requirement = service
            .create(CreateRequirementRequest {
                title: "Do X".into(),
                content: "body".into(),
                tag: "t".into(),
                order_key: None,
                status: None,
                created_by: None,
                attachments: vec![],
            })
            .await
            .expect("requirement");
        service
            .claim_next(
                "t",
                &conversation_id,
                AutoWorkTargetKind::Conversation,
                120_000,
            )
            .await
            .expect("claim")
            .expect("claimed requirement");
        let claim_token: Option<String> =
            sqlx::query_scalar("SELECT claim_token FROM requirements WHERE requirement_id=?")
                .bind(&requirement.requirement_id)
                .fetch_one(&pool)
                .await
                .expect("claim token query");
        (
            RequirementServiceSink { service },
            conversation_id,
            requirement.requirement_id,
            claim_token.expect("new claim capability"),
            pool,
        )
    }

    #[tokio::test]
    async fn stale_native_tool_token_cannot_resolve_new_claim_even_with_guessed_generation() {
        let (sink, conversation_id, requirement_id, claim_token_one, pool) =
            conversation_sink_with_claim().await;
        assert!(
            sink.service
                .release_claim_exact(&requirement_id, &conversation_id, 1, &claim_token_one)
                .await
                .expect("release generation one")
        );
        sink.service
            .claim_next(
                "t",
                &conversation_id,
                AutoWorkTargetKind::Conversation,
                120_000,
            )
            .await
            .expect("claim generation two")
            .expect("generation two requirement");
        let claim_token_two: Option<String> =
            sqlx::query_scalar("SELECT claim_token FROM requirements WHERE requirement_id=?")
                .bind(&requirement_id)
                .fetch_one(&pool)
                .await
                .expect("generation two token query");
        let claim_token_two = claim_token_two.expect("generation two capability");
        assert_ne!(claim_token_one, claim_token_two);

        let stale_result = sink
            .complete(
                &conversation_id,
                &requirement_id,
                2,
                &claim_token_one,
                "stale model guessed the new generation",
            )
            .await;
        assert!(stale_result.is_err(), "{stale_result:?}");
        assert_eq!(
            sink.service
                .get(&requirement_id)
                .await
                .expect("requirement after stale verdict")
                .status,
            RequirementStatus::InProgress
        );

        sink.complete(
            &conversation_id,
            &requirement_id,
            2,
            &claim_token_two,
            "current claim completed",
        )
        .await
        .expect("current capability resolves claim");
        assert_eq!(
            sink.service
                .get(&requirement_id)
                .await
                .expect("completed requirement")
                .status,
            RequirementStatus::Done
        );
    }
}
