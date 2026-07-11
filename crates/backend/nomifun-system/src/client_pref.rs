use std::sync::Arc;

use nomifun_api_types::{ClientPreferencesResponse, UpdateClientPreferencesRequest};
use nomifun_common::AppError;
use nomifun_db::IClientPreferenceRepository;

/// Maximum allowed key length for client preferences.
const MAX_KEY_LENGTH: usize = 255;
const SYSTEM_RESERVED_PREFIXES: &[&str] = &["managedModel."];

/// Business logic for client preferences (generic key-value store).
#[derive(Clone)]
pub struct ClientPrefService {
    repo: Arc<dyn IClientPreferenceRepository>,
}

impl ClientPrefService {
    pub fn new(repo: Arc<dyn IClientPreferenceRepository>) -> Self {
        Self { repo }
    }

    /// Get all client preferences, or only the specified keys.
    pub async fn get_preferences(&self, keys: Option<&[&str]>) -> Result<ClientPreferencesResponse, AppError> {
        let rows = match keys {
            Some(k) if !k.is_empty() => self.repo.get_by_keys(k).await,
            _ => self.repo.get_all().await,
        }
        .map_err(|e| AppError::Internal(format!("Failed to get preferences: {e}")))?;

        let mut map = ClientPreferencesResponse::new();
        for row in rows {
            let value: serde_json::Value =
                serde_json::from_str(&row.value).unwrap_or(serde_json::Value::String(row.value));
            map.insert(row.key, value);
        }
        Ok(map)
    }

    /// Batch update client preferences. Null values delete the key.
    pub async fn update_preferences(&self, req: UpdateClientPreferencesRequest) -> Result<(), AppError> {
        let mut upserts: Vec<(String, String)> = Vec::new();
        let mut deletes: Vec<String> = Vec::new();

        for (key, value) in req {
            validate_key(&key)?;
            if SYSTEM_RESERVED_PREFIXES
                .iter()
                .any(|prefix| key.starts_with(prefix))
            {
                return Err(AppError::Forbidden(format!(
                    "Preference key '{key}' is managed by the system"
                )));
            }

            if value.is_null() {
                deletes.push(key);
            } else {
                upserts.push((
                    key,
                    serde_json::to_string(&value)
                        .map_err(|e| AppError::Internal(format!("Failed to serialize value: {e}")))?,
                ));
            }
        }

        if !upserts.is_empty() {
            let entries: Vec<(&str, &str)> = upserts.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            self.repo
                .upsert_batch(&entries)
                .await
                .map_err(|e| AppError::Internal(format!("Failed to upsert preferences: {e}")))?;
        }

        if !deletes.is_empty() {
            let keys: Vec<&str> = deletes.iter().map(|k| k.as_str()).collect();
            self.repo
                .delete_keys(&keys)
                .await
                .map_err(|e| AppError::Internal(format!("Failed to delete preferences: {e}")))?;
        }

        Ok(())
    }
}

fn validate_key(key: &str) -> Result<(), AppError> {
    if key.is_empty() {
        return Err(AppError::BadRequest("Preference key must not be empty".into()));
    }
    if key.len() > MAX_KEY_LENGTH {
        return Err(AppError::BadRequest(format!(
            "Preference key exceeds maximum length of {MAX_KEY_LENGTH} characters"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_db::{SqliteClientPreferenceRepository, init_database_memory};
    use serde_json::json;

    async fn setup() -> ClientPrefService {
        let db = init_database_memory().await.unwrap();
        let repo = Arc::new(SqliteClientPreferenceRepository::new(db.pool().clone()));
        std::mem::forget(db);
        ClientPrefService::new(repo)
    }

    #[test]
    fn validate_key_accepts_valid() {
        assert!(validate_key("theme").is_ok());
        assert!(validate_key("system.closeToTray").is_ok());
        assert!(validate_key("a").is_ok());
    }

    #[test]
    fn validate_key_rejects_empty() {
        assert!(validate_key("").is_err());
    }

    #[test]
    fn validate_key_rejects_too_long() {
        let long_key = "x".repeat(MAX_KEY_LENGTH + 1);
        assert!(validate_key(&long_key).is_err());
    }

    #[tokio::test]
    async fn update_rejects_system_managed_namespace() {
        let svc = setup().await;
        let mut req = UpdateClientPreferencesRequest::new();
        req.insert("managedModel.free.lastRefresh".into(), json!(1));
        let err = svc.update_preferences(req).await.unwrap_err();
        assert_eq!(err.status_code(), axum::http::StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn get_empty_returns_empty_map() {
        let svc = setup().await;
        let prefs = svc.get_preferences(None).await.unwrap();
        assert!(prefs.is_empty());
    }

    #[tokio::test]
    async fn update_and_get_boolean() {
        let svc = setup().await;
        let mut req = UpdateClientPreferencesRequest::new();
        req.insert("system.closeToTray".into(), json!(true));
        svc.update_preferences(req).await.unwrap();

        let prefs = svc.get_preferences(None).await.unwrap();
        assert_eq!(prefs["system.closeToTray"], json!(true));
    }

    #[tokio::test]
    async fn update_and_get_number() {
        let svc = setup().await;
        let mut req = UpdateClientPreferencesRequest::new();
        req.insert("companion.size".into(), json!(360));
        svc.update_preferences(req).await.unwrap();

        let prefs = svc.get_preferences(None).await.unwrap();
        assert_eq!(prefs["companion.size"], json!(360));
    }

    #[tokio::test]
    async fn update_and_get_string() {
        let svc = setup().await;
        let mut req = UpdateClientPreferencesRequest::new();
        req.insert("theme".into(), json!("dark"));
        svc.update_preferences(req).await.unwrap();

        let prefs = svc.get_preferences(None).await.unwrap();
        assert_eq!(prefs["theme"], json!("dark"));
    }

    #[tokio::test]
    async fn null_deletes_key() {
        let svc = setup().await;

        let mut req = UpdateClientPreferencesRequest::new();
        req.insert("theme".into(), json!("dark"));
        svc.update_preferences(req).await.unwrap();

        let mut req2 = UpdateClientPreferencesRequest::new();
        req2.insert("theme".into(), json!(null));
        svc.update_preferences(req2).await.unwrap();

        let prefs = svc.get_preferences(None).await.unwrap();
        assert!(!prefs.contains_key("theme"));
    }

    #[tokio::test]
    async fn get_by_keys_filters() {
        let svc = setup().await;

        let mut req = UpdateClientPreferencesRequest::new();
        req.insert("a".into(), json!(1));
        req.insert("b".into(), json!(2));
        req.insert("c".into(), json!(3));
        svc.update_preferences(req).await.unwrap();

        let prefs = svc.get_preferences(Some(&["a", "c"])).await.unwrap();
        assert_eq!(prefs.len(), 2);
        assert_eq!(prefs["a"], json!(1));
        assert_eq!(prefs["c"], json!(3));
    }

    #[tokio::test]
    async fn overwrite_existing_value() {
        let svc = setup().await;

        let mut req1 = UpdateClientPreferencesRequest::new();
        req1.insert("k".into(), json!("v1"));
        svc.update_preferences(req1).await.unwrap();

        let mut req2 = UpdateClientPreferencesRequest::new();
        req2.insert("k".into(), json!("v2"));
        svc.update_preferences(req2).await.unwrap();

        let prefs = svc.get_preferences(None).await.unwrap();
        assert_eq!(prefs["k"], json!("v2"));
    }

    #[tokio::test]
    async fn empty_key_rejected() {
        let svc = setup().await;
        let mut req = UpdateClientPreferencesRequest::new();
        req.insert("".into(), json!(true));
        let err = svc.update_preferences(req).await.unwrap_err();
        assert_eq!(err.status_code(), axum::http::StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn long_key_rejected() {
        let svc = setup().await;
        let mut req = UpdateClientPreferencesRequest::new();
        req.insert("x".repeat(256), json!(true));
        let err = svc.update_preferences(req).await.unwrap_err();
        assert_eq!(err.status_code(), axum::http::StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn batch_mixed_upsert_and_delete() {
        let svc = setup().await;

        let mut setup_req = UpdateClientPreferencesRequest::new();
        setup_req.insert("keep".into(), json!(1));
        setup_req.insert("remove".into(), json!(2));
        svc.update_preferences(setup_req).await.unwrap();

        let mut req = UpdateClientPreferencesRequest::new();
        req.insert("remove".into(), json!(null));
        req.insert("new".into(), json!(3));
        svc.update_preferences(req).await.unwrap();

        let prefs = svc.get_preferences(None).await.unwrap();
        assert_eq!(prefs.len(), 2);
        assert_eq!(prefs["keep"], json!(1));
        assert_eq!(prefs["new"], json!(3));
    }
}
