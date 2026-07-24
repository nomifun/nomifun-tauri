//! 真实重水合源(design 2026-06-23):会话库 `messages` 表 = 内容唯一事实源。
//!
//! 给定 wire `conversation_id`（即 canonical UUIDv7 `conversations.conversation_id`，不是
//! 数字主键 `conversations.id`），按 [`TranscriptAnchor`]
//! 框出窗口,把消息转成**脱敏**转录喂给 drafter。装配见 `service::attach_companion`(会话服务
//! 晚于伴随服务构建,故晚装配)。走仓储层 `get_messages`(user 无关,绕开 list_messages 的
//! 鉴权与 type 过滤)。会话不存在/为空 → `None`(drafter 降级回工具名步骤)。

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use nomifun_common::{AppError, ConversationId};
use nomifun_db::{IConversationRepository, SortOrder};

use crate::evolution::transcript::{TranscriptAnchor, TranscriptSource, TranscriptTurn};

/// 单条文本/参数/结果脱敏后的字符上限(控转录体量)。
const FIELD_CHARS: usize = 600;
/// 单会话最多取多少条消息来框窗口(防超大会话拖垮起草)。
const MAX_FETCH: u32 = 1000;

pub struct ConversationTranscriptSource {
    repo: Arc<dyn IConversationRepository>,
}

impl ConversationTranscriptSource {
    pub fn new(repo: Arc<dyn IConversationRepository>) -> Self {
        Self { repo }
    }
}

/// 一行消息解析后的最小投影。
struct Parsed {
    ty: String,
    position: String,
    content: serde_json::Value,
    /// tool_call/acp_tool_call 的 call_id(用于按锚精确框窗)。
    call_id: Option<String>,
}

/// 脱敏 + 截断(secrets 永不入转录;过长字段裁断带省略号)。
fn redact_clip(s: &str) -> String {
    let red = nomi_redact::redact_secrets_owned(s.to_owned());
    if red.chars().count() <= FIELD_CHARS {
        red
    } else {
        let head: String = red.chars().take(FIELD_CHARS).collect();
        format!("{head}…")
    }
}

/// 从 tool_call/acp_tool_call 的 content 提取 (name, args, result)。
fn extract_tool(ty: &str, content: &serde_json::Value) -> Option<(String, Option<String>, Option<String>)> {
    match ty {
        "tool_call" => {
            let name = content.get("name").and_then(|v| v.as_str())?.to_owned();
            let args = content
                .get("args")
                .or_else(|| content.get("input"))
                .filter(|v| !v.is_null())
                .map(|v| v.to_string());
            let result = content.get("output").and_then(|v| v.as_str()).map(|s| s.to_owned());
            Some((name, args, result))
        }
        "acp_tool_call" => {
            let upd = content.get("update")?;
            let name = upd.get("title").and_then(|v| v.as_str()).unwrap_or("tool").to_owned();
            let args = upd.get("raw_input").filter(|v| !v.is_null()).map(|v| v.to_string());
            let result = upd.get("raw_output").filter(|v| !v.is_null()).map(|v| v.to_string());
            Some((name, args, result))
        }
        _ => None,
    }
}

#[async_trait]
impl TranscriptSource for ConversationTranscriptSource {
    async fn window(&self, anchor: &TranscriptAnchor) -> Result<Option<Vec<TranscriptTurn>>, AppError> {
        // The wire ID is the canonical UUIDv7 conversations.conversation_id, not
        // the numeric conversations.id primary key; malformed IDs cannot be looked up.
        let Ok(conv_id) = ConversationId::try_from(anchor.conversation_id.as_str()) else {
            return Ok(None);
        };
        let page = self
            .repo
            .get_messages(conv_id.as_str(), 1, MAX_FETCH, SortOrder::Asc)
            .await
            .map_err(|e| AppError::Internal(format!("rehydrate get_messages: {e}")))?;
        if page.items.is_empty() {
            return Ok(None); // 会话已删/为空
        }

        let parsed: Vec<Parsed> = page
            .items
            .iter()
            .filter(|r| !r.hidden)
            .map(|r| {
                let content: serde_json::Value = serde_json::from_str(&r.content).unwrap_or(serde_json::Value::Null);
                let call_id = match r.r#type.as_str() {
                    "tool_call" => content.get("call_id").and_then(|v| v.as_str()).map(|s| s.to_owned()),
                    "acp_tool_call" => content
                        .get("update")
                        .and_then(|u| u.get("tool_call_id"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_owned()),
                    _ => None,
                };
                Parsed { ty: r.r#type.clone(), position: r.position.clone().unwrap_or_default(), content, call_id }
            })
            .collect();
        if parsed.is_empty() {
            return Ok(None);
        }

        // 框窗口：call_id 命中优先(挖矿/示范都带 call_ids)；命中为空则退回整段(capped)。
        let want: HashSet<&str> = anchor.call_ids.iter().map(|s| s.as_str()).collect();
        let hits: Vec<usize> = if want.is_empty() {
            Vec::new()
        } else {
            parsed
                .iter()
                .enumerate()
                .filter(|(_, p)| p.call_id.as_deref().map(|c| want.contains(c)).unwrap_or(false))
                .map(|(i, _)| i)
                .collect()
        };
        let (lo, hi) = if hits.is_empty() {
            (0, parsed.len() - 1) // 无精确命中 → 整段(已 capped 到 MAX_FETCH)
        } else {
            let lo = hits.iter().min().copied().unwrap().saturating_sub(anchor.pad_turns);
            let hi = (hits.iter().max().copied().unwrap() + anchor.pad_turns).min(parsed.len() - 1);
            (lo, hi)
        };

        let mut turns = Vec::new();
        for p in &parsed[lo..=hi] {
            match p.ty.as_str() {
                "text" if p.position == "right" => {
                    if let Some(t) = p.content.get("content").and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty()) {
                        turns.push(TranscriptTurn::user(redact_clip(t)));
                    }
                }
                "text" => {
                    if let Some(t) = p.content.get("content").and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty()) {
                        turns.push(TranscriptTurn::assistant(redact_clip(t)));
                    }
                }
                "tool_call" | "acp_tool_call" => {
                    if let Some((name, args, result)) = extract_tool(&p.ty, &p.content) {
                        turns.push(TranscriptTurn::tool(
                            name,
                            args.map(|a| redact_clip(&a)),
                            result.map(|r| redact_clip(&r)),
                        ));
                    }
                }
                _ => {} // thinking/tips 跳过(对起草是噪声)
            }
        }
        Ok(Some(turns))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_common::{ConversationId, MessageId, now_ms};
    use nomifun_db::models::{ConversationRow, MessageRow};
    use nomifun_db::{init_database_memory, SqliteConversationRepository};

    fn conv_row(user_id: &str) -> ConversationRow {
        ConversationRow {
            id: 0,
            conversation_id: ConversationId::new().into_string(),
            user_id: user_id.to_owned(),
            name: "t".into(),
            r#type: "acp".into(),
            extra: "{}".into(),
            delegation_policy: "automatic".into(),
            execution_model_pool: None,
            decision_policy: "automatic".into(),
            execution_template_id: None,
            model: None,
            status: Some("finished".into()),
            source: None,
            channel_chat_id: None,
            pinned: false,
            pinned_at: None,
            cron_job_id: None,
            preset_id: None,
            preset_revision: None,
            preset_snapshot: None,
            created_at: now_ms(),
            updated_at: now_ms(),
        }
    }

    fn text_msg(conv: &str, content: &str, position: &str, ts: i64) -> MessageRow {
        MessageRow {
            id: 0,
            message_id: MessageId::new().into_string(),
            conversation_id: conv.to_owned(),
            msg_id: None,
            r#type: "text".into(),
            content: serde_json::json!({ "content": content }).to_string(),
            position: Some(position.into()),
            status: Some("finish".into()),
            hidden: false,
            created_at: ts,
        }
    }

    fn tool_msg(conv: &str, call_id: &str, args: serde_json::Value, output: &str, ts: i64) -> MessageRow {
        MessageRow {
            id: 0,
            message_id: MessageId::new().into_string(),
            conversation_id: conv.to_owned(),
            msg_id: None,
            r#type: "tool_call".into(),
            content: serde_json::json!({
                "call_id": call_id, "name": "grep", "args": args, "status": "completed", "output": output
            })
            .to_string(),
            position: Some("left".into()),
            status: Some("finish".into()),
            hidden: false,
            created_at: ts,
        }
    }

    async fn repo_with_conv() -> (Arc<SqliteConversationRepository>, String) {
        let db = init_database_memory().await.unwrap();
        let installation_owner = nomifun_db::installation_owner_id(db.pool()).await.unwrap();
        let repo = Arc::new(SqliteConversationRepository::new(db.pool().clone()));
        let id = repo.create(&conv_row(&installation_owner)).await.unwrap();
        (repo, id)
    }

    /// 守门:重水合命中 → 真实 user/tool/assistant 内容入转录;secret 脱敏;thinking/hidden 排除。
    #[tokio::test]
    async fn rehydrates_window_and_redacts_secrets() {
        let (repo, conv) = repo_with_conv().await;
        repo.insert_message(&text_msg(&conv, "把日志里的错误改掉", "right", 1)).await.unwrap();
        repo.insert_message(&tool_msg(
            &conv,
            "tc-1",
            serde_json::json!({ "pattern": "ERROR", "key": "sk-ABCDEFGHIJ0123456789xyz" }),
            "命中 3 处",
            2,
        ))
        .await
        .unwrap();
        repo.insert_message(&text_msg(&conv, "改好了", "left", 3)).await.unwrap();
        let mut hidden = text_msg(&conv, "隐藏内容", "left", 4);
        hidden.hidden = true;
        repo.insert_message(&hidden).await.unwrap();
        let mut thinking = text_msg(&conv, "内心独白", "left", 5);
        thinking.r#type = "thinking".into();
        repo.insert_message(&thinking).await.unwrap();

        let src = ConversationTranscriptSource::new(repo.clone());
        let anchor = TranscriptAnchor {
            conversation_id: conv.to_string(),
            start_ts: 0,
            end_ts: 0,
            pad_turns: 2,
            call_ids: vec!["tc-1".into()],
        };
        let turns = src.window(&anchor).await.unwrap().expect("conversation present");
        let rendered = crate::evolution::render_transcript(&turns, 600).join("\n");
        assert!(rendered.contains("把日志里的错误改掉"), "user content: {rendered}");
        assert!(rendered.contains("grep"), "tool name: {rendered}");
        assert!(rendered.contains("命中 3 处"), "tool result: {rendered}");
        assert!(rendered.contains("改好了"), "assistant content: {rendered}");
        // 脱敏:secret 永不入转录(纵深防御要害)。
        assert!(!rendered.contains("sk-ABCDEFGHIJ"), "secret leaked: {rendered}");
        assert!(rendered.contains("[REDACTED_SECRET]"), "redaction marker missing: {rendered}");
        // thinking/hidden 是噪声,不入转录。
        assert!(!rendered.contains("内心独白"), "thinking leaked: {rendered}");
        assert!(!rendered.contains("隐藏内容"), "hidden leaked: {rendered}");
    }

    /// 守门:非法 ID / 不存在的会话 → None(drafter 降级,不报错)。
    #[tokio::test]
    async fn missing_or_invalid_conversation_returns_none() {
        let (repo, _conv) = repo_with_conv().await;
        let src = ConversationTranscriptSource::new(repo);
        let malformed = TranscriptAnchor { conversation_id: "not-a-conversation-id".into(), ..Default::default() };
        assert!(src.window(&malformed).await.unwrap().is_none(), "invalid id → None");
        let gone = TranscriptAnchor {
            conversation_id: ConversationId::new().into_string(),
            ..Default::default()
        };
        assert!(src.window(&gone).await.unwrap().is_none(), "missing conversation → None");
    }
}
