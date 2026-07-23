//! Shared test fixtures for the knowledge crate: an in-memory
//! `IKnowledgeRepository`, a no-op event broadcaster, and a service factory.

use std::path::Path;
use std::sync::{Arc, Mutex};

use nomifun_common::{
    CompanionId, ConversationId, KnowledgeBindingId, TerminalId, TimestampMs,
};
use nomifun_db::models::{CreateKnowledgeTagParams, KnowledgeBaseRow, KnowledgeBindingRow, KnowledgeTagRow, UpdateKnowledgeTagParams};
use nomifun_db::{DbError, IKnowledgeRepository};

use crate::events::KnowledgeEventEmitter;
use crate::service::KnowledgeService;

#[derive(Default)]
pub(crate) struct MemRepo {
    pub bases: Mutex<Vec<KnowledgeBaseRow>>,
    /// Each binding row plus its ordered `kb_id` list (mirrors the
    /// `knowledge_binding_bases` junction).
    pub bindings: Mutex<Vec<(KnowledgeBindingRow, Vec<String>)>>,
    pub tags: Mutex<Vec<KnowledgeTagRow>>,
}

/// Build a binding row with the `target_id` written to the column selected by
/// `target_kind` (the in-memory analogue of the CHECK-constrained columns).
fn binding_row(
    id: i64,
    knowledge_binding_id: KnowledgeBindingId,
    kind: &str,
    target_id: &str,
    enabled: bool,
    writeback: bool,
    writeback_mode: &str,
    writeback_eagerness: &str,
    channel_write_enabled: bool,
    updated_at: TimestampMs,
) -> KnowledgeBindingRow {
    let mut row = KnowledgeBindingRow {
        id,
        knowledge_binding_id,
        target_kind: kind.to_owned(),
        target_workpath: None,
        target_conversation_id: None,
        target_terminal_id: None,
        target_companion_id: None,
        enabled,
        writeback,
        writeback_mode: writeback_mode.to_owned(),
        writeback_eagerness: writeback_eagerness.to_owned(),
        channel_write_enabled,
        updated_at,
    };
    match kind {
        "workpath" => row.target_workpath = Some(target_id.to_owned()),
        "conversation" => {
            row.target_conversation_id = ConversationId::parse(target_id).ok()
        }
        "terminal" => row.target_terminal_id = TerminalId::parse(target_id).ok(),
        "companion" => row.target_companion_id = CompanionId::parse(target_id).ok(),
        _ => {}
    }
    row
}

#[async_trait::async_trait]
impl IKnowledgeRepository for MemRepo {
    async fn insert_base(&self, row: &KnowledgeBaseRow) -> Result<(), DbError> {
        let mut bases = self.bases.lock().unwrap();
        let mut row = row.clone();
        if row.id == 0 {
            row.id = bases.iter().map(|base| base.id).max().unwrap_or(0) + 1;
        }
        bases.push(row);
        Ok(())
    }
    async fn update_base(&self, row: &KnowledgeBaseRow) -> Result<(), DbError> {
        let mut bases = self.bases.lock().unwrap();
        match bases
            .iter_mut()
            .find(|r| r.knowledge_base_id == row.knowledge_base_id)
        {
            Some(r) => {
                let technical_id = r.id;
                *r = row.clone();
                r.id = technical_id;
                Ok(())
            }
            None => Err(DbError::NotFound(row.knowledge_base_id.clone())),
        }
    }
    async fn delete_base(&self, id: &str) -> Result<(), DbError> {
        let mut bases = self.bases.lock().unwrap();
        let before = bases.len();
        bases.retain(|r| r.knowledge_base_id != id);
        if bases.len() == before {
            Err(DbError::NotFound(id.to_owned()))
        } else {
            Ok(())
        }
    }
    async fn get_base(&self, id: &str) -> Result<Option<KnowledgeBaseRow>, DbError> {
        Ok(self
            .bases
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.knowledge_base_id == id)
            .cloned())
    }
    async fn list_bases(&self) -> Result<Vec<KnowledgeBaseRow>, DbError> {
        Ok(self.bases.lock().unwrap().clone())
    }
    async fn get_binding(
        &self,
        kind: &str,
        id: &str,
    ) -> Result<Option<(KnowledgeBindingRow, Vec<String>)>, DbError> {
        Ok(self
            .bindings
            .lock()
            .unwrap()
            .iter()
            .find(|(b, _)| b.target_kind == kind && b.target_id().as_deref() == Some(id))
            .cloned())
    }
    async fn set_binding(
        &self,
        kind: &str,
        id: &str,
        kb_ids: &[String],
        enabled: bool,
        writeback: bool,
        writeback_mode: &str,
        writeback_eagerness: &str,
        channel_write_enabled: bool,
        updated_at: TimestampMs,
    ) -> Result<String, DbError> {
        let mut bindings = self.bindings.lock().unwrap();
        // Preserve both identities on upsert. The local integer is only the
        // technical row key; callers receive the stable UUIDv7 business ID.
        let (technical_id, knowledge_binding_id) = bindings
            .iter()
            .find(|(b, _)| b.target_kind == kind && b.target_id().as_deref() == Some(id))
            .map(|(b, _)| (b.id, b.knowledge_binding_id.clone()))
            .unwrap_or_else(|| {
                (
                    bindings.iter().map(|(b, _)| b.id).max().unwrap_or(0) + 1,
                    KnowledgeBindingId::new(),
                )
            });
        bindings.retain(|(b, _)| !(b.target_kind == kind && b.target_id().as_deref() == Some(id)));
        let business_id = knowledge_binding_id.to_string();
        let row = binding_row(
            technical_id,
            knowledge_binding_id,
            kind,
            id,
            enabled,
            writeback,
            writeback_mode,
            writeback_eagerness,
            channel_write_enabled,
            updated_at,
        );
        bindings.push((row, kb_ids.to_vec()));
        Ok(business_id)
    }
    async fn delete_binding(&self, kind: &str, id: &str) -> Result<(), DbError> {
        self.bindings
            .lock()
            .unwrap()
            .retain(|(b, _)| !(b.target_kind == kind && b.target_id().as_deref() == Some(id)));
        Ok(())
    }
    async fn list_bindings_using_kb(&self, kb_id: &str) -> Result<Vec<KnowledgeBindingRow>, DbError> {
        let mut rows: Vec<KnowledgeBindingRow> = self
            .bindings
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, kb_ids)| kb_ids.iter().any(|k| k == kb_id))
            .map(|(b, _)| b.clone())
            .collect();
        rows.sort_by(|a, b| a.target_kind.cmp(&b.target_kind).then(a.id.cmp(&b.id)));
        Ok(rows)
    }

    async fn list_knowledge_tags(&self) -> Result<Vec<KnowledgeTagRow>, DbError> {
        let mut tags = self.tags.lock().unwrap().clone();
        tags.sort_by(|a, b| a.sort_order.cmp(&b.sort_order).then(a.key.cmp(&b.key)));
        Ok(tags)
    }

    async fn create_knowledge_tag(&self, params: CreateKnowledgeTagParams) -> Result<(), DbError> {
        let mut tags = self.tags.lock().unwrap();
        let id = tags.iter().map(|tag| tag.id).max().unwrap_or(0) + 1;
        tags.push(KnowledgeTagRow {
            id,
            key: params.key,
            label: params.label,
            color: params.color,
            sort_order: params.sort_order,
            created_at: params.created_at,
        });
        Ok(())
    }

    async fn update_knowledge_tag(&self, key: &str, params: UpdateKnowledgeTagParams) -> Result<(), DbError> {
        let mut tags = self.tags.lock().unwrap();
        match tags.iter_mut().find(|t| t.key == key) {
            Some(t) => {
                if let Some(label) = params.label {
                    t.label = label;
                }
                if let Some(color) = params.color {
                    t.color = color;
                }
                if let Some(sort_order) = params.sort_order {
                    t.sort_order = sort_order;
                }
                Ok(())
            }
            None => Err(DbError::NotFound(format!("knowledge tag {key}"))),
        }
    }

    async fn delete_knowledge_tag(&self, key: &str) -> Result<(), DbError> {
        let mut tags = self.tags.lock().unwrap();
        let before = tags.len();
        tags.retain(|t| t.key != key);
        if tags.len() == before {
            Err(DbError::NotFound(format!("knowledge tag {key}")))
        } else {
            Ok(())
        }
    }
}

pub(crate) struct NoopBroadcaster;

impl nomifun_realtime::UserEventSink for NoopBroadcaster {
    fn send_to_user(
        &self,
        _user_id: &str,
        _event: nomifun_api_types::WebSocketMessage<serde_json::Value>,
    ) {
    }
}

pub(crate) fn make_service(data_dir: &Path) -> KnowledgeService {
    let owner_id = nomifun_common::UserId::new();
    KnowledgeService::new(
        Arc::new(MemRepo::default()),
        data_dir,
        KnowledgeEventEmitter::new(
            Arc::new(NoopBroadcaster),
            Arc::from(owner_id.into_string()),
        ),
    )
}
