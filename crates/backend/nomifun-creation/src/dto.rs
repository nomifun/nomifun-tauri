//! Wire DTO for the `/api/creation/tasks` surface (contract §3.3). snake_case
//! (serde default). Owned by this crate (the shared `api-types` crate is not in
//! this module's ownership).

use nomifun_common::TimestampMs;
use nomifun_db::CreationTaskRow;
use serde::Serialize;
use serde_json::Value;

/// A generation task as seen over the wire.
#[derive(Debug, Clone, Serialize)]
pub struct CreationTask {
    pub id: String,
    pub canvas_id: Option<String>,
    pub node_id: Option<String>,
    pub provider_id: String,
    pub model: String,
    pub capability: String,
    pub params: Value,
    pub status: String,
    pub error: Option<Value>,
    pub result_asset_ids: Vec<String>,
    pub attempt: i64,
    pub submitted_at: TimestampMs,
    pub started_at: Option<TimestampMs>,
    pub finished_at: Option<TimestampMs>,
}

impl From<CreationTaskRow> for CreationTask {
    fn from(row: CreationTaskRow) -> Self {
        // JSON TEXT columns parsed leniently: a corrupt value degrades to a safe
        // default rather than failing the whole response.
        let params = serde_json::from_str::<Value>(&row.params).unwrap_or_else(|_| Value::Object(Default::default()));
        let error = row.error.as_deref().and_then(|s| serde_json::from_str::<Value>(s).ok());
        let result_asset_ids = serde_json::from_str::<Vec<String>>(&row.result_asset_ids).unwrap_or_default();
        Self {
            id: row.id,
            canvas_id: row.canvas_id,
            node_id: row.node_id,
            provider_id: row.provider_id,
            model: row.model,
            capability: row.capability,
            params,
            status: row.status,
            error,
            result_asset_ids,
            attempt: row.attempt,
            submitted_at: row.submitted_at,
            started_at: row.started_at,
            finished_at: row.finished_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_dto_parses_json_columns() {
        let row = CreationTaskRow {
            id: "wst_1".into(),
            canvas_id: Some("wsc_1".into()),
            node_id: None,
            provider_id: "prov_x".into(),
            model: "m".into(),
            capability: "t2i".into(),
            params: r#"{"prompt":"cat"}"#.into(),
            status: "failed".into(),
            error: Some(r#"{"kind":"adapter_unavailable","message":"x"}"#.into()),
            result_asset_ids: r#"["wsa_a"]"#.into(),
            remote_task_id: None,
            attempt: 0,
            submitted_at: 1,
            started_at: None,
            finished_at: Some(2),
        };
        let dto = CreationTask::from(row);
        assert_eq!(dto.params["prompt"], "cat");
        assert_eq!(dto.error.unwrap()["kind"], "adapter_unavailable");
        assert_eq!(dto.result_asset_ids, vec!["wsa_a".to_string()]);
        assert_eq!(dto.finished_at, Some(2));
    }
}
