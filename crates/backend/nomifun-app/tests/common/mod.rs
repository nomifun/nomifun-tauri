//! Shared test helpers for nomifun-app E2E tests.
#![allow(dead_code)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use tower::ServiceExt;
use wiremock::MockServer;

use nomifun_ai_agent::{AgentRuntimeHandle, AgentRuntimeControl, MockAgentRuntime, InMemoryAgentRuntimeRegistry};
use nomifun_app::{AppConfig, AppServices, build_module_states, create_router, create_router_with_states};
use nomifun_extension::{ExternalPathsManager, SkillPaths, SkillRouterState};
use nomifun_file::FileService;
use nomifun_system::VersionCheckService;

pub async fn build_app() -> (axum::Router, AppServices) {
    let db = nomifun_db::init_database_memory().await.unwrap();
    let services = AppServices::from_config(db, &AppConfig::default()).await.unwrap();
    let router = create_router(&services).await;
    (router, services)
}

/// Build an app whose skill router reads from the given temp directories.
///
/// Use for HTTP integration tests that need deterministic on-disk layouts
/// (E1 `/api/skills`, E2 `/api/skills/builtin-auto`, E3/E4 built-in reads,
/// E5 `/api/skills/info`). Returns the router, services, and the
/// `SkillPaths` so the test can seed fixtures at known locations.
#[allow(dead_code)]
pub async fn build_app_with_skill_paths(root: &std::path::Path) -> (axum::Router, AppServices, SkillPaths) {
    let db = nomifun_db::init_database_memory().await.unwrap();
    let services = AppServices::from_config(db, &AppConfig::default()).await.unwrap();
    let (mut states, _) = build_module_states(&services).await;

    let builtin_dir = root.join("builtin-skills");
    let paths = SkillPaths {
        data_dir: root.to_path_buf(),
        user_skills_dir: root.join("skills"),
        cron_skills_dir: root.join("cron").join("skills"),
        builtin_skills_dir: builtin_dir.clone(),
        builtin_rules_dir: root.join("builtin-rules"),
        preset_rules_dir: root.join("preset-rules"),
        preset_skills_dir: root.join("preset-skills"),
    };
    for dir in [
        &paths.user_skills_dir,
        &builtin_dir,
        &paths.builtin_rules_dir,
        &paths.preset_rules_dir,
        &paths.preset_skills_dir,
    ] {
        std::fs::create_dir_all(dir).unwrap();
    }

    let ext_paths_mgr = std::sync::Arc::new(ExternalPathsManager::with_file(root.join("paths.json")).await);
    states.skill = SkillRouterState {
        skill_paths: paths.clone(),
        external_paths_manager: ext_paths_mgr,
        preset_dispatcher: states.skill.preset_dispatcher.clone(),
        skill_tag_repo: std::sync::Arc::new(nomifun_db::SqliteSkillTagRepository::new(
            services.database.pool().clone(),
        )),
        builtin_skill_tags: std::sync::Arc::new(std::collections::HashMap::new()),
    };

    let router = create_router_with_states(&services, states);
    (router, services, paths)
}

pub async fn build_app_with_noop_opener() -> (axum::Router, AppServices) {
    let db = nomifun_db::init_database_memory().await.unwrap();
    let services = AppServices::from_config(db, &AppConfig::default()).await.unwrap();
    let (mut states, _) = build_module_states(&services).await;
    states.shell.shell_service = std::sync::Arc::new(nomifun_shell::ShellService::new(std::sync::Arc::new(
        nomifun_shell::NoopSystemOpener,
    )));
    let router = create_router_with_states(&services, states);
    (router, services)
}

pub async fn build_app_with_file_roots(allowed_roots: Vec<std::path::PathBuf>) -> (axum::Router, AppServices) {
    let db = nomifun_db::init_database_memory().await.unwrap();
    let services = AppServices::from_config(db, &AppConfig::default()).await.unwrap();
    let (mut states, _) = build_module_states(&services).await;
    states.file.file_service = std::sync::Arc::new(FileService::new(services.event_bus.clone(), allowed_roots));
    let router = create_router_with_states(&services, states);
    (router, services)
}

pub async fn build_app_with_mock_version(
    current_version: &str,
    mock_server: &MockServer,
) -> (axum::Router, AppServices) {
    let db = nomifun_db::init_database_memory().await.unwrap();
    let services = AppServices::from_config(db, &AppConfig::default()).await.unwrap();
    let (mut states, _) = build_module_states(&services).await;
    let http_client = reqwest::Client::builder().no_proxy().build().unwrap();
    states.system.version_check_service =
        VersionCheckService::with_api_base(http_client, current_version.to_owned(), mock_server.uri());
    let router = create_router_with_states(&services, states);
    (router, services)
}

/// Build app with a mock Agent runtime registry that returns noop agents.
///
/// Use for tests that exercise session warmup and send-message paths where
/// spawning a real CLI process is not feasible.
pub async fn build_app_with_mock_agents() -> (axum::Router, AppServices) {
    build_app_with_mock_agents_config(AppConfig::default()).await
}

pub async fn build_isolated_app_with_mock_agents(
    root: &std::path::Path,
) -> (axum::Router, AppServices) {
    build_app_with_mock_agents_config(AppConfig {
        data_dir: root.join("data"),
        work_dir: root.join("work"),
        ..AppConfig::default()
    })
    .await
}

async fn build_app_with_mock_agents_config(config: AppConfig) -> (axum::Router, AppServices) {
    let db = nomifun_db::init_database_memory().await.unwrap();
    let factory: std::sync::Arc<
        dyn Fn(
                nomifun_ai_agent::types::AgentRuntimeBuildOptions,
            ) -> futures_util::future::BoxFuture<'static, Result<AgentRuntimeHandle, nomifun_common::AppError>>
            + Send
            + Sync,
    > = std::sync::Arc::new(|opts| {
        Box::pin(async move {
            Ok(AgentRuntimeHandle::Mock(std::sync::Arc::new(NoopMockAgent {
                conversation_id: opts.conversation_id,
            })))
        })
    });
    let runtime_registry: std::sync::Arc<dyn nomifun_ai_agent::AgentRuntimeRegistry> =
        std::sync::Arc::new(InMemoryAgentRuntimeRegistry::new(factory));
    let services = AppServices::from_config(db, &config)
        .await
        .unwrap()
        .with_agent_runtime_registry(runtime_registry);
    let router = create_router(&services).await;
    (router, services)
}

struct NoopMockAgent {
    conversation_id: String,
}

#[async_trait::async_trait]
impl AgentRuntimeControl for NoopMockAgent {
    fn agent_type(&self) -> nomifun_common::AgentType {
        nomifun_common::AgentType::Acp
    }
    fn conversation_id(&self) -> &str {
        &self.conversation_id
    }
    fn workspace(&self) -> &str {
        "/tmp/test"
    }
    fn status(&self) -> Option<nomifun_common::ConversationStatus> {
        None
    }
    fn is_transport_healthy(&self) -> bool {
        true
    }
    fn last_activity_at(&self) -> nomifun_common::TimestampMs {
        nomifun_common::now_ms()
    }
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<nomifun_ai_agent::AgentStreamEvent> {
        let (tx, _) = tokio::sync::broadcast::channel(1);
        tx.subscribe()
    }
    async fn send_message(
        &self,
        _data: nomifun_ai_agent::types::SendMessageData,
    ) -> Result<(), nomifun_ai_agent::AgentSendError> {
        Ok(())
    }
    async fn cancel(&self) -> Result<(), nomifun_common::AppError> {
        Ok(())
    }
    fn kill(&self, _reason: Option<nomifun_common::AgentKillReason>) -> Result<(), nomifun_common::AppError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl MockAgentRuntime for NoopMockAgent {}

pub async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

pub fn extract_csrf_token(resp: &axum::response::Response) -> Option<String> {
    resp.headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|s| s.starts_with("nomifun-csrf-token="))
        .map(|s| {
            s.strip_prefix("nomifun-csrf-token=")
                .unwrap()
                .split(';')
                .next()
                .unwrap()
                .to_owned()
        })
}

pub fn get_request(uri: &str) -> Request<Body> {
    Request::builder().method("GET").uri(uri).body(Body::empty()).unwrap()
}

pub fn get_with_token(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

fn is_public_conversation_send(method: &str, uri: &str) -> bool {
    if !method.eq_ignore_ascii_case("POST") {
        return false;
    }
    let Ok(uri) = uri.parse::<axum::http::Uri>() else {
        return false;
    };
    if uri.query().is_some() {
        return false;
    }
    let Some(conversation_id) = uri
        .path()
        .strip_prefix("/api/conversations/")
        .and_then(|path| path.strip_suffix("/messages"))
    else {
        return false;
    };

    !conversation_id.is_empty() && !conversation_id.contains('/')
}

pub fn json_with_token(method_str: &str, uri: &str, body: serde_json::Value, token: &str, csrf: &str) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method_str)
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .header("x-csrf-token", csrf)
        .header("cookie", format!("nomifun-csrf-token={csrf}"));
    if is_public_conversation_send(method_str, uri) {
        builder = builder.header("idempotency-key", nomifun_common::generate_id());
    }
    builder
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

pub fn delete_with_token(uri: &str, token: &str, csrf: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .header("x-csrf-token", csrf)
        .header("cookie", format!("nomifun-csrf-token={csrf}"))
        .body(Body::empty())
        .unwrap()
}

/// Set up a user and login, returning (session_token, csrf_token).
///
/// The canonical installation owner already uses `username = "admin"`; if
/// the test asks for that username, overwrite the owner row's empty credentials
/// in place instead of trying to INSERT a duplicate.
pub async fn setup_and_login(
    app: &mut axum::Router,
    services: &AppServices,
    username: &str,
    password: &str,
) -> (String, String) {
    let hash = nomifun_auth::hash_password(password).unwrap();
    if username == "admin" {
        services
            .user_repo
            .set_system_user_credentials(username, &hash)
            .await
            .unwrap();
    } else {
        services.user_repo.create_user(username, &hash).await.unwrap();
    }

    let resp = app.clone().oneshot(get_request("/api/auth/status")).await.unwrap();
    let csrf = extract_csrf_token(&resp).expect("CSRF cookie should be set");

    let body = format!(r#"{{"username":"{username}","password":"{password}"}}"#);
    let req = Request::builder()
        .method("POST")
        .uri("/login")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "login should succeed");

    let json = body_json(resp).await;
    let token = json["token"].as_str().unwrap().to_owned();

    (token, csrf)
}
