use nomifun_ai_agent::AgentSendError;
use nomifun_common::{AppError, ErrorChain, now_ms};
use nomifun_db::models::MessageRow;
use tracing::warn;

use crate::service::ConversationService;

/// 构造"图片已移除"提示行的 content JSON 串(与 persist 分离便于测试)。
pub(crate) fn images_stripped_tip_content() -> String {
    serde_json::json!({
        "content": "当前模型不支持图片输入，已自动移除图片并重试。",
        "type": "warning",
        "source": "images_stripped",
    })
    .to_string()
}

impl ConversationService {
    pub(crate) async fn persist_send_failure_tip(&self, conversation_id: &str, err: &AppError) -> Option<MessageRow> {
        let Ok(conv_id) = conversation_id.parse::<i64>() else {
            warn!(
                conversation_id,
                "persist_send_failure_tip: non-numeric conversation id; skipping error tip persist"
            );
            return None;
        };
        let stream_error = AgentSendError::from_app_error_ref(err).into_stream_error();
        let row = MessageRow {
            id: Self::mint_msg_id(),
            conversation_id: conv_id,
            msg_id: None,
            r#type: "tips".into(),
            content: serde_json::json!({
                "content": &stream_error.message,
                "type": "error",
                "source": "send_failed",
                "code": err.error_code(),
                "details": err.error_details(),
                "error": stream_error,
            })
            .to_string(),
            position: Some("center".into()),
            status: Some("error".into()),
            hidden: false,
            created_at: now_ms(),
        };

        if let Err(store_err) = self.conversation_repo().insert_message(&row).await {
            warn!(
                conversation_id,
                error = %ErrorChain(&store_err),
                "Failed to persist send failure error tip"
            );
            return None;
        }

        Some(row)
    }

    /// 在会话里插入一条"图片已移除"警告提示(tips)。仅供用户查看,不回传模型。
    pub(crate) async fn persist_images_stripped_tip(&self, conversation_id: &str) -> Option<MessageRow> {
        let Ok(conv_id) = conversation_id.parse::<i64>() else {
            warn!(
                conversation_id,
                "persist_images_stripped_tip: non-numeric conversation id; skipping"
            );
            return None;
        };
        let row = MessageRow {
            id: Self::mint_msg_id(),
            conversation_id: conv_id,
            msg_id: None,
            r#type: "tips".into(),
            content: images_stripped_tip_content(),
            position: Some("center".into()),
            status: None,
            hidden: false,
            created_at: now_ms(),
        };
        if let Err(store_err) = self.conversation_repo().insert_message(&row).await {
            warn!(
                conversation_id,
                error = %ErrorChain(&store_err),
                "Failed to persist images-stripped tip"
            );
            return None;
        }
        Some(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_stripped_tip_has_warning_type_and_source() {
        let s = images_stripped_tip_content();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["type"], "warning");
        assert_eq!(v["source"], "images_stripped");
        assert!(v["content"].as_str().unwrap().contains("图片"));
    }
}
