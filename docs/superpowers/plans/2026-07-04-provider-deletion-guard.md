# 模型供应商删除保护 + 优雅回退 + 友好文案 — 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让被"硬绑定"功能（伙伴/智能决策/智能编排）使用的模型供应商无法被误删，普通会话在供应商丢失时优雅回退不再崩，且任何残留报错以可操作文案展示而非裸 `prov_xxx`。

**Architecture:** 三层。(A) app 层聚合各子系统的 in-use 扫描，删除前拦截并返回结构化 409；(B) Nomi 工厂送出时对丢失供应商回退到首个可用（后端不崩），前端在会话加载时把会话模型自愈为用户默认并轻提示；(C) 无可用模型的终态错误带专用 code，前端按 code→i18n 展示。

**Tech Stack:** Rust（axum + sqlx + async-trait + tokio-test），前端 React + TypeScript + Arco Design + i18next + bun:test。

## Global Constraints

- **提交署名**：所有提交用 `git -c user.name=nomifun commit ...`（复用已配置 email）；**不加** `Co-Authored-By` / "Generated with Claude Code" 尾注。
- **DTO 落位**：`ProviderUsage` 系列放 `nomifun-common`（`nomifun-api-types` 依赖 `nomifun-common`，反之不行；`AppError` 在 `nomifun-common` 需引用它）。
- **前端验证**：改动前端后跑 `cd ui && bun run build`（不能只靠 tsc）。前端单测用 `bun:test`（`import { describe, expect, test } from 'bun:test'`），逻辑函数就地 `*.test.ts`。
- **`knowledge.autogenModel` 与 `nomi.defaultModel` 是纯前端配置**，后端不可清理/读取；归前端自愈。
- **IDMM 拦截 v1 仅覆盖全局备份** `idmm_backup_provider_id`（单键）；per-conversation watch 的 `bypass_model` 不在 v1 扫描范围（无跨用户列举会话的仓库方法），由组件 B 兜底——务必 `log` 说明此范围。
- **后端软清理 v1 仅 `agent.model_failover` 队列**；`idmm_backup_*` 是被保护引用（拦截删除、不清理）；渠道 `assistant.{platform}.defaultModel` 由组件 B 兜底，不在 v1 清理。
- 供应商 id 形如 `prov_{uuidv7}`；空 `provider_id` 视为未配置。
- 错误变体遵循 `nomifun-common/src/error.rs` 现有 `WorkspacePathEdgeWhitespace` 范式（专用变体 + `error_details()` 输出结构化 JSON）。

---

### Task 1: DTO + AppError 变体（nomifun-common）

**Files:**
- Create: `crates/backend/nomifun-common/src/provider_usage.rs`
- Modify: `crates/backend/nomifun-common/src/error.rs`（enum + status_code + error_code + error_details）
- Modify: `crates/backend/nomifun-common/src/lib.rs`（导出模块）

**Interfaces:**
- Produces:
  - `ProviderUsageFeature` enum：`DesktopCompanion | PublicCompanion | SmartDecision | Orchestrator`（serde `rename_all="camelCase"`）。
  - `ProviderUsage { feature: ProviderUsageFeature, label: String, target_id: Option<String> }`（serde camelCase）。
  - `ProviderInUseDetails { usages: Vec<ProviderUsage> }`。
  - `AppError::ProviderInUse(ProviderInUseDetails)` → 409 / `PROVIDER_IN_USE` / details=`{"usages":[...]}`。
  - `AppError::ProviderUnavailable(String)` → 400 / `PROVIDER_UNAVAILABLE`。

- [ ] **Step 1: 写失败测试（DTO 序列化 + error 映射）**

在 `crates/backend/nomifun-common/src/provider_usage.rs` 末尾（先创建文件含类型再加测试，见 Step 3；此步先写测试内容占位于同文件）：实际操作顺序是先建文件骨架。为遵循 TDD，先在 `error.rs` 的 `#[cfg(test)] mod tests` 追加：

```rust
    #[tokio::test]
    async fn provider_in_use_response_shape() {
        use crate::provider_usage::{ProviderInUseDetails, ProviderUsage, ProviderUsageFeature};
        let err = AppError::ProviderInUse(ProviderInUseDetails {
            usages: vec![ProviderUsage {
                feature: ProviderUsageFeature::DesktopCompanion,
                label: "大聪明".into(),
                target_id: Some("cmp_1".into()),
            }],
        });
        assert_eq!(err.status_code(), StatusCode::CONFLICT);
        assert_eq!(err.error_code(), "PROVIDER_IN_USE");
        let resp = err.into_response();
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["code"], "PROVIDER_IN_USE");
        assert_eq!(json["details"]["usages"][0]["feature"], "desktopCompanion");
        assert_eq!(json["details"]["usages"][0]["label"], "大聪明");
        assert_eq!(json["details"]["usages"][0]["targetId"], "cmp_1");
    }

    #[test]
    fn provider_unavailable_code_and_status() {
        let err = AppError::ProviderUnavailable("no enabled provider".into());
        assert_eq!(err.status_code(), StatusCode::BAD_REQUEST);
        assert_eq!(err.error_code(), "PROVIDER_UNAVAILABLE");
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p nomifun-common provider_ 2>&1 | tail -20`
Expected: 编译失败（`provider_usage` 模块不存在、`AppError::ProviderInUse`/`ProviderUnavailable` 未定义）。

- [ ] **Step 3: 建 DTO 文件**

`crates/backend/nomifun-common/src/provider_usage.rs`：

```rust
//! Provider-in-use reporting types shared between the deletion guard,
//! the `AppError::ProviderInUse` variant, and the HTTP error body.

use serde::{Deserialize, Serialize};

/// Which feature holds a live reference to a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProviderUsageFeature {
    DesktopCompanion,
    PublicCompanion,
    SmartDecision,
    Orchestrator,
}

/// One concrete usage of a provider by a feature (for the "cannot delete" UI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderUsage {
    pub feature: ProviderUsageFeature,
    /// Human-readable name of the referencing entity (companion name, fleet name, …).
    pub label: String,
    /// Optional id to deep-link the user to the unbind location.
    pub target_id: Option<String>,
}

/// Structured payload for `AppError::ProviderInUse` → HTTP `details`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderInUseDetails {
    pub usages: Vec<ProviderUsage>,
}
```

- [ ] **Step 4: 加 AppError 变体与映射**

`error.rs`：enum 追加两个变体（放 `Conflict` 之后）：

```rust
    /// A provider cannot be deleted because features still reference it.
    #[error("Provider is in use: {} reference(s)", .0.usages.len())]
    ProviderInUse(crate::provider_usage::ProviderInUseDetails),

    /// No usable (enabled) provider/model is configured to serve a request.
    #[error("No usable model provider is configured: {0}")]
    ProviderUnavailable(String),
```

`status_code()` 追加：

```rust
            Self::ProviderInUse(_) => StatusCode::CONFLICT,
            Self::ProviderUnavailable(_) => StatusCode::BAD_REQUEST,
```

`error_code()` 追加：

```rust
            Self::ProviderInUse(_) => "PROVIDER_IN_USE",
            Self::ProviderUnavailable(_) => "PROVIDER_UNAVAILABLE",
```

`error_details()` 的 match 追加（在 `_ => None` 之前）：

```rust
            Self::ProviderInUse(details) => Some(json!({ "usages": details.usages })),
```

`lib.rs` 追加导出（跟随现有 `pub mod` 风格）：

```rust
pub mod provider_usage;
pub use provider_usage::{ProviderInUseDetails, ProviderUsage, ProviderUsageFeature};
```

- [ ] **Step 5: 运行测试确认通过**

Run: `cargo test -p nomifun-common provider_ 2>&1 | tail -20`
Expected: `provider_in_use_response_shape` + `provider_unavailable_code_and_status` PASS。

- [ ] **Step 6: 提交**

```bash
git add crates/backend/nomifun-common/src/provider_usage.rs crates/backend/nomifun-common/src/error.rs crates/backend/nomifun-common/src/lib.rs
git -c user.name=nomifun commit -m "feat(common): ProviderUsage DTO + ProviderInUse/ProviderUnavailable AppError variants"
```

---

### Task 2: Coordinator trait + ProviderService 拦截（nomifun-system）

**Files:**
- Create: `crates/backend/nomifun-system/src/provider_deletion.rs`
- Modify: `crates/backend/nomifun-system/src/provider.rs`（字段 + builder + delete 逻辑）
- Modify: `crates/backend/nomifun-system/src/lib.rs`（导出 trait）
- Modify: `crates/backend/nomifun-system/Cargo.toml`（加 `async-trait`、dev-dep `tokio` 已有则跳过）

**Interfaces:**
- Consumes: `nomifun_common::{ProviderUsage, ProviderInUseDetails, AppError}`（Task 1）。
- Produces:
  - `trait ProviderDeletionCoordinator: Send + Sync { async fn usages(&self, provider_id:&str)->Result<Vec<ProviderUsage>,AppError>; async fn cleanup_soft_refs(&self, provider_id:&str)->Result<(),AppError>; }`
  - `ProviderService::with_deletion_coordinator(self, Arc<dyn ProviderDeletionCoordinator>) -> Self`
  - `ProviderService::delete` 行为：有 usage → `Err(AppError::ProviderInUse)`；否则删除后调用 `cleanup_soft_refs`（失败仅 `warn!`）。

- [ ] **Step 1: 写失败测试**

`provider.rs` 的 `#[cfg(test)] mod tests`（无则新建）加入。先需要一个 fake 仓库与 fake coordinator：

```rust
#[cfg(test)]
mod delete_guard_tests {
    use super::*;
    use nomifun_common::{ProviderUsage, ProviderUsageFeature};
    use nomifun_db::models::Provider;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct CountingRepo { deleted: AtomicBool }
    #[async_trait::async_trait]
    impl IProviderRepository for CountingRepo {
        async fn list(&self) -> Result<Vec<Provider>, nomifun_db::DbError> { Ok(vec![]) }
        async fn find_by_id(&self, _: &str) -> Result<Option<Provider>, nomifun_db::DbError> { Ok(None) }
        async fn create(&self, _: nomifun_db::CreateProviderParams<'_>) -> Result<Provider, nomifun_db::DbError> { unimplemented!() }
        async fn update(&self, _: &str, _: nomifun_db::UpdateProviderParams<'_>) -> Result<Provider, nomifun_db::DbError> { unimplemented!() }
        async fn delete(&self, _: &str) -> Result<(), nomifun_db::DbError> { self.deleted.store(true, Ordering::SeqCst); Ok(()) }
    }

    struct FakeCoord { usages: Vec<ProviderUsage>, cleaned: AtomicBool }
    #[async_trait::async_trait]
    impl crate::provider_deletion::ProviderDeletionCoordinator for FakeCoord {
        async fn usages(&self, _: &str) -> Result<Vec<ProviderUsage>, AppError> { Ok(self.usages.clone()) }
        async fn cleanup_soft_refs(&self, _: &str) -> Result<(), AppError> { self.cleaned.store(true, Ordering::SeqCst); Ok(()) }
    }

    #[tokio::test]
    async fn delete_blocked_when_in_use() {
        let repo = Arc::new(CountingRepo { deleted: AtomicBool::new(false) });
        let coord = Arc::new(FakeCoord {
            usages: vec![ProviderUsage { feature: ProviderUsageFeature::DesktopCompanion, label: "甲".into(), target_id: None }],
            cleaned: AtomicBool::new(false),
        });
        let svc = ProviderService::new(repo.clone(), [0u8; 32]).with_deletion_coordinator(coord);
        let err = svc.delete("prov_x").await.unwrap_err();
        assert!(matches!(err, AppError::ProviderInUse(_)));
        assert!(!repo.deleted.load(Ordering::SeqCst), "must not delete when in use");
    }

    #[tokio::test]
    async fn delete_proceeds_and_cleans_when_unused() {
        let repo = Arc::new(CountingRepo { deleted: AtomicBool::new(false) });
        let coord = Arc::new(FakeCoord { usages: vec![], cleaned: AtomicBool::new(false) });
        let svc = ProviderService::new(repo.clone(), [0u8; 32]).with_deletion_coordinator(coord.clone());
        svc.delete("prov_x").await.unwrap();
        assert!(repo.deleted.load(Ordering::SeqCst));
        assert!(coord.cleaned.load(Ordering::SeqCst));
    }
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p nomifun-system delete_guard 2>&1 | tail -20`
Expected: 编译失败（`provider_deletion` 模块 / `with_deletion_coordinator` 不存在）。

- [ ] **Step 3: 建 trait 模块**

`crates/backend/nomifun-system/src/provider_deletion.rs`：

```rust
//! Cross-subsystem provider-deletion guard hook. Implemented at the app layer
//! (the only place that sees companion/idmm/orchestrator), injected into
//! `ProviderService` so deletion can refuse in-use providers.

use nomifun_common::{AppError, ProviderUsage};
use std::sync::Arc;

#[async_trait::async_trait]
pub trait ProviderDeletionCoordinator: Send + Sync {
    /// Returns every hard-binding usage of `provider_id`; empty ⇒ safe to delete.
    async fn usages(&self, provider_id: &str) -> Result<Vec<ProviderUsage>, AppError>;

    /// Best-effort cleanup of soft references after a successful delete.
    async fn cleanup_soft_refs(&self, provider_id: &str) -> Result<(), AppError>;
}

pub type SharedProviderDeletionCoordinator = Arc<dyn ProviderDeletionCoordinator>;
```

`lib.rs` 追加：`pub mod provider_deletion;`

- [ ] **Step 4: 改 ProviderService**

`provider.rs`：

顶部 imports 追加：`use crate::provider_deletion::SharedProviderDeletionCoordinator;` 与 `use nomifun_common::{ProviderInUseDetails};`（`AppError` 已在）、`use tracing::warn;`（若无 tracing 依赖，见 note）。

结构体加字段：

```rust
#[derive(Clone)]
pub struct ProviderService {
    repo: Arc<dyn IProviderRepository>,
    encryption_key: [u8; 32],
    coordinator: Option<SharedProviderDeletionCoordinator>,
}
```

`new` 初始化 `coordinator: None`；新增 builder：

```rust
    pub fn new(repo: Arc<dyn IProviderRepository>, encryption_key: [u8; 32]) -> Self {
        Self { repo, encryption_key, coordinator: None }
    }

    pub fn with_deletion_coordinator(mut self, coordinator: SharedProviderDeletionCoordinator) -> Self {
        self.coordinator = Some(coordinator);
        self
    }
```

`delete` 改为：

```rust
    pub async fn delete(&self, id: &str) -> Result<(), AppError> {
        if let Some(coord) = &self.coordinator {
            let usages = coord.usages(id).await?;
            if !usages.is_empty() {
                return Err(AppError::ProviderInUse(ProviderInUseDetails { usages }));
            }
        }
        self.repo.delete(id).await?;
        if let Some(coord) = &self.coordinator {
            if let Err(e) = coord.cleanup_soft_refs(id).await {
                warn!(provider_id = %id, error = %e, "provider soft-ref cleanup failed (delete already committed)");
            }
        }
        Ok(())
    }
```

`Cargo.toml`：`[dependencies]` 加 `async-trait = { workspace = true }`；若无 `tracing` 则加 `tracing = { workspace = true }`。

- [ ] **Step 5: 运行确认通过**

Run: `cargo test -p nomifun-system delete_guard 2>&1 | tail -20`
Expected: 两个测试 PASS。

- [ ] **Step 6: 提交**

```bash
git add crates/backend/nomifun-system/src/provider_deletion.rs crates/backend/nomifun-system/src/provider.rs crates/backend/nomifun-system/src/lib.rs crates/backend/nomifun-system/Cargo.toml
git -c user.name=nomifun commit -m "feat(system): ProviderDeletionCoordinator hook + in-use guard in ProviderService::delete"
```

---

### Task 3: Fleet 按 provider 查询（nomifun-db）

**Files:**
- Modify: `crates/backend/nomifun-db/src/repository/orch_fleet.rs`（trait 加方法）
- Modify: `crates/backend/nomifun-db/src/repository/sqlite_orch_fleet.rs`（impl + 测试）

**Interfaces:**
- Produces: `IFleetRepository::fleets_using_provider(&self, provider_id: &str) -> Result<Vec<(String, String)>, sqlx::Error>`（返回 `(fleet_id, fleet_name)` 去重）。

- [ ] **Step 1: 写失败测试**

`sqlite_orch_fleet.rs` 的 `#[cfg(test)] mod tests` 追加（参照现有 `create_fleet`/`replace_members`/`list_members` 测试的构造方式）：

```rust
    #[tokio::test]
    async fn fleets_using_provider_returns_matching_fleet() {
        let db = crate::init_database_memory().await.unwrap();
        let repo = SqliteFleetRepository::new(db.pool().clone());
        let fleet = repo.create_fleet(CreateFleetParams {
            // 复制本文件其它测试里 CreateFleetParams 的字段填充方式
            id: "fleet_1", user_id: "u1", name: "研究舰队", /* 其余字段按现有测试填 */
            ..sample_fleet_params()
        }).await.unwrap();
        repo.replace_members("fleet_1", vec![NewFleetMember {
            id: "m1".into(), agent_id: "a1".into(),
            provider_id: Some("prov_x".into()), model: Some("m".into()),
            ..sample_member()
        }]).await.unwrap();

        let hits = repo.fleets_using_provider("prov_x").await.unwrap();
        assert_eq!(hits, vec![("fleet_1".to_string(), "研究舰队".to_string())]);
        let none = repo.fleets_using_provider("prov_other").await.unwrap();
        assert!(none.is_empty());
        let _ = fleet;
    }
```

> 注：`sample_fleet_params()`/`sample_member()` 为占位——实现时改用本文件既有测试的真实构造（`CreateFleetParams`/`NewFleetMember` 的确切字段见文件顶部结构体定义；`list_members` 的 roundtrip 测试 sqlite_orch_fleet.rs:179-199 已有可复制的成员构造）。

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p nomifun-db fleets_using_provider 2>&1 | tail -20`
Expected: 编译失败（方法不存在）。

- [ ] **Step 3: trait 加方法**

`orch_fleet.rs` 的 `IFleetRepository` 追加：

```rust
    /// Distinct (fleet_id, fleet_name) whose members reference `provider_id`.
    async fn fleets_using_provider(&self, provider_id: &str) -> Result<Vec<(String, String)>, sqlx::Error>;
```

- [ ] **Step 4: impl**

`sqlite_orch_fleet.rs` 的 `impl IFleetRepository for SqliteFleetRepository` 追加：

```rust
    async fn fleets_using_provider(&self, provider_id: &str) -> Result<Vec<(String, String)>, sqlx::Error> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT DISTINCT f.id, f.name \
             FROM fleet_members m JOIN fleets f ON f.id = m.fleet_id \
             WHERE m.provider_id = ? \
             ORDER BY f.name ASC",
        )
        .bind(provider_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }
```

- [ ] **Step 5: 运行确认通过**

Run: `cargo test -p nomifun-db fleets_using_provider 2>&1 | tail -20`
Expected: PASS。

- [ ] **Step 6: 提交**

```bash
git add crates/backend/nomifun-db/src/repository/orch_fleet.rs crates/backend/nomifun-db/src/repository/sqlite_orch_fleet.rs
git -c user.name=nomifun commit -m "feat(db): IFleetRepository::fleets_using_provider for provider-in-use scan"
```

---

### Task 4: 伙伴 in-use 扫描（nomifun-companion）

**Files:**
- Modify: `crates/backend/nomifun-companion/src/service.rs`（新方法 + 测试）
- Modify: `crates/backend/nomifun-companion/Cargo.toml`（确认依赖 `nomifun-common`，已有则跳过）

**Interfaces:**
- Consumes: `nomifun_common::{ProviderUsage, ProviderUsageFeature}`。
- Produces: `CompanionService::providers_in_use(&self, provider_id:&str) -> Vec<ProviderUsage>`（扫每个 companion 的 chat `model` + 共享 learn/evolve `model`；`feature=DesktopCompanion`；label=伙伴名/`"共享学习模型"`/`"共享进化模型"`；target_id=伙伴 id 或 None）。

- [ ] **Step 1: 写失败测试**（复用文件顶部 `service(dir)` helper、`registry.create`、`patch_companion`、`patch_config`）

```rust
    #[tokio::test]
    async fn providers_in_use_detects_companion_chat_model() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;
        let cid = svc.registry.create("大聪明", "ink").await.unwrap().id;
        svc.patch_companion(&cid, serde_json::json!({"model":{"provider_id":"prov_x","model":"m"}})).await.unwrap();

        let hits = svc.providers_in_use("prov_x").await;
        assert!(hits.iter().any(|u| u.label == "大聪明" && u.target_id.as_deref() == Some(cid.as_str())));
        assert!(svc.providers_in_use("prov_none").await.is_empty());
    }

    #[tokio::test]
    async fn providers_in_use_detects_shared_learn_model() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path()).await;
        svc.patch_config(serde_json::json!({"learn":{"model":{"provider_id":"prov_learn","model":"m"}}})).await.unwrap();
        let hits = svc.providers_in_use("prov_learn").await;
        assert!(hits.iter().any(|u| matches!(u.feature, nomifun_common::ProviderUsageFeature::DesktopCompanion)));
    }
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p nomifun-companion providers_in_use 2>&1 | tail -20`
Expected: 编译失败（方法不存在）。

- [ ] **Step 3: 实现方法**

`service.rs`（`use nomifun_common::{ProviderUsage, ProviderUsageFeature};` 若未引入则加）：

```rust
    /// Every desktop-companion reference to `provider_id`: per-companion chat
    /// model + the shared learn/evolve models. Empty ⇒ not used.
    pub async fn providers_in_use(&self, provider_id: &str) -> Vec<ProviderUsage> {
        let mut out = Vec::new();
        for p in self.list_companions().await {
            if p.model.provider_id == provider_id {
                out.push(ProviderUsage {
                    feature: ProviderUsageFeature::DesktopCompanion,
                    label: p.name.clone(),
                    target_id: Some(p.id.clone()),
                });
            }
        }
        let shared = self.get_config().await;
        if shared.learn.model.provider_id == provider_id {
            out.push(ProviderUsage {
                feature: ProviderUsageFeature::DesktopCompanion,
                label: "共享学习模型".into(),
                target_id: None,
            });
        }
        if shared.evolve.model.provider_id == provider_id {
            out.push(ProviderUsage {
                feature: ProviderUsageFeature::DesktopCompanion,
                label: "共享进化模型".into(),
                target_id: None,
            });
        }
        out
    }
```

- [ ] **Step 4: 运行确认通过**

Run: `cargo test -p nomifun-companion providers_in_use 2>&1 | tail -20`
Expected: PASS（两个测试）。

- [ ] **Step 5: 提交**

```bash
git add crates/backend/nomifun-companion/src/service.rs crates/backend/nomifun-companion/Cargo.toml
git -c user.name=nomifun commit -m "feat(companion): CompanionService::providers_in_use scan (chat + shared learn/evolve)"
```

---

### Task 5: 对外伙伴 in-use 扫描（nomifun-public-agent）

**Files:**
- Modify: `crates/backend/nomifun-public-agent/src/service.rs`（新方法 + 测试）

**Interfaces:**
- Produces: `PublicAgentService::providers_in_use(&self, provider_id:&str) -> Vec<ProviderUsage>`（扫 `list()` 每个 agent 的 `model.provider_id`；`feature=PublicCompanion`；label=agent 名；target_id=agent id）。

- [ ] **Step 1: 写失败测试**（复用文件既有 `PublicAgentService::start(d.path())` + `create` + `patch` 范式）

```rust
    #[tokio::test]
    async fn providers_in_use_detects_public_agent_model() {
        let d = tempfile::tempdir().unwrap();
        let svc = PublicAgentService::start(d.path());
        let a = svc.create("客服").await.unwrap();
        svc.patch(&a.id, serde_json::json!({"model":{"provider_id":"prov_x","model":"m"}})).await.unwrap();

        let hits = svc.providers_in_use("prov_x").await;
        assert!(hits.iter().any(|u| u.label == "客服" && u.target_id.as_deref() == Some(a.id.as_str())));
        assert!(svc.providers_in_use("prov_none").await.is_empty());
    }
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p nomifun-public-agent providers_in_use 2>&1 | tail -20`
Expected: 编译失败。

- [ ] **Step 3: 实现**

```rust
    pub async fn providers_in_use(&self, provider_id: &str) -> Vec<nomifun_common::ProviderUsage> {
        self.list()
            .await
            .into_iter()
            .filter(|a| a.model.provider_id == provider_id)
            .map(|a| nomifun_common::ProviderUsage {
                feature: nomifun_common::ProviderUsageFeature::PublicCompanion,
                label: a.name,
                target_id: Some(a.id),
            })
            .collect()
    }
```

- [ ] **Step 4: 运行确认通过**

Run: `cargo test -p nomifun-public-agent providers_in_use 2>&1 | tail -20`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add crates/backend/nomifun-public-agent/src/service.rs
git -c user.name=nomifun commit -m "feat(public-agent): PublicAgentService::providers_in_use scan"
```

---

### Task 6: app 层聚合协调器 + 装配（nomifun-app）

**Files:**
- Create: `crates/backend/nomifun-app/src/provider_deletion.rs`
- Modify: `crates/backend/nomifun-app/src/lib.rs` 或 `mod.rs`（挂 `mod provider_deletion;`）
- Modify: `crates/backend/nomifun-app/src/router/state.rs`（`build_system_state` 装配）

**Interfaces:**
- Consumes: `CompanionService::providers_in_use`（T4）、`PublicAgentService::providers_in_use`（T5）、`IFleetRepository::fleets_using_provider`（T3）、`IClientPreferenceRepository`（`get_by_keys`/`upsert_batch`）、`nomifun_conversation::model_failover::{get_global_failover_config, set_global_failover_config}`、`nomifun_idmm::sidecar::PREF_BACKUP_PROVIDER`。
- Produces: `AppProviderDeletionCoordinator` 实现 `ProviderDeletionCoordinator`；在 `build_system_state` 里注入。

- [ ] **Step 1: 写失败测试**（`crates/backend/nomifun-app/src/provider_deletion.rs` 内 `#[cfg(test)]`；用 `init_database_memory` + tempdir 服务）

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_common::ProviderUsageFeature;
    use nomifun_db::{init_database_memory, SqliteClientPreferenceRepository, SqliteFleetRepository};
    use std::sync::Arc;

    async fn coordinator(dir: &std::path::Path) -> (AppProviderDeletionCoordinator, Arc<nomifun_db::Database>) {
        let db = Arc::new(init_database_memory().await.unwrap());
        let companion = /* CompanionService::start(dir, …) 复制 nomifun-companion 测试 helper 的四参构造 */;
        let public_agent = nomifun_public_agent::PublicAgentService::start(dir);
        let client_prefs: Arc<dyn nomifun_db::IClientPreferenceRepository> =
            Arc::new(SqliteClientPreferenceRepository::new(db.pool().clone()));
        let fleet_repo: Arc<dyn nomifun_db::IFleetRepository> =
            Arc::new(SqliteFleetRepository::new(db.pool().clone()));
        (AppProviderDeletionCoordinator { companion, public_agent, client_prefs, fleet_repo }, db)
    }

    #[tokio::test]
    async fn aggregates_idmm_backup_usage() {
        let dir = tempfile::tempdir().unwrap();
        let (coord, _db) = coordinator(dir.path()).await;
        coord.client_prefs.upsert_batch(&[(nomifun_idmm::sidecar::PREF_BACKUP_PROVIDER, "prov_x")]).await.unwrap();
        let usages = coord.usages("prov_x").await.unwrap();
        assert!(usages.iter().any(|u| matches!(u.feature, ProviderUsageFeature::SmartDecision)));
        assert!(coord.usages("prov_none").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn cleanup_strips_failover_queue_entry() {
        use nomifun_conversation::model_failover::{get_global_failover_config, set_global_failover_config};
        let dir = tempfile::tempdir().unwrap();
        let (coord, _db) = coordinator(dir.path()).await;
        let mut cfg = get_global_failover_config(&coord.client_prefs).await;
        cfg.queue = vec![
            nomifun_common::types::ProviderWithModel { provider_id: "prov_x".into(), model: "m".into(), use_model: None },
            nomifun_common::types::ProviderWithModel { provider_id: "prov_keep".into(), model: "m2".into(), use_model: None },
        ];
        set_global_failover_config(&coord.client_prefs, &cfg).await.unwrap();

        coord.cleanup_soft_refs("prov_x").await.unwrap();
        let after = get_global_failover_config(&coord.client_prefs).await;
        assert_eq!(after.queue.len(), 1);
        assert_eq!(after.queue[0].provider_id, "prov_keep");
    }
}
```

> `ProviderWithModel` 的确切路径以 `nomifun-common/src/types.rs:34-39` 为准；`ModelFailoverConfig.queue` 字段名见 `nomifun-api-types/src/idmm.rs`。

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p nomifun-app aggregates_idmm_backup 2>&1 | tail -20`
Expected: 编译失败（`AppProviderDeletionCoordinator` 不存在）。

- [ ] **Step 3: 实现协调器**

`crates/backend/nomifun-app/src/provider_deletion.rs`：

```rust
//! App-layer aggregation of every subsystem's provider-in-use scan + soft-ref cleanup.

use std::sync::Arc;

use nomifun_common::{AppError, ProviderUsage, ProviderUsageFeature};
use nomifun_conversation::model_failover::{get_global_failover_config, set_global_failover_config};
use nomifun_db::{IClientPreferenceRepository, IFleetRepository};
use nomifun_idmm::sidecar::PREF_BACKUP_PROVIDER;
use nomifun_system::provider_deletion::ProviderDeletionCoordinator;

pub struct AppProviderDeletionCoordinator {
    pub companion: Arc<nomifun_companion::CompanionService>,
    pub public_agent: Arc<nomifun_public_agent::PublicAgentService>,
    pub client_prefs: Arc<dyn IClientPreferenceRepository>,
    pub fleet_repo: Arc<dyn IFleetRepository>,
}

#[async_trait::async_trait]
impl ProviderDeletionCoordinator for AppProviderDeletionCoordinator {
    async fn usages(&self, provider_id: &str) -> Result<Vec<ProviderUsage>, AppError> {
        let mut out = Vec::new();
        out.extend(self.companion.providers_in_use(provider_id).await);
        out.extend(self.public_agent.providers_in_use(provider_id).await);

        // 智能决策：v1 仅全局备份模型（per-conversation watch 不在扫描范围，见计划约束）。
        let rows = self
            .client_prefs
            .get_by_keys(&[PREF_BACKUP_PROVIDER])
            .await
            .map_err(|e| AppError::Internal(format!("read idmm backup pref: {e}")))?;
        if rows.iter().any(|r| r.key == PREF_BACKUP_PROVIDER && r.value == provider_id) {
            out.push(ProviderUsage {
                feature: ProviderUsageFeature::SmartDecision,
                label: "智能决策·备份模型".into(),
                target_id: None,
            });
        }

        let fleets = self
            .fleet_repo
            .fleets_using_provider(provider_id)
            .await
            .map_err(|e| AppError::Internal(format!("scan fleets: {e}")))?;
        for (id, name) in fleets {
            out.push(ProviderUsage {
                feature: ProviderUsageFeature::Orchestrator,
                label: name,
                target_id: Some(id),
            });
        }
        Ok(out)
    }

    async fn cleanup_soft_refs(&self, provider_id: &str) -> Result<(), AppError> {
        // v1 仅清理全局失败回退队列；idmm_backup_* 是被保护引用（不会走到此处）。
        let mut cfg = get_global_failover_config(&self.client_prefs).await;
        let before = cfg.queue.len();
        cfg.queue.retain(|m| m.provider_id != provider_id);
        if cfg.queue.len() != before {
            set_global_failover_config(&self.client_prefs, &cfg).await?;
        }
        Ok(())
    }
}
```

`mod.rs`/`lib.rs` 挂载：`mod provider_deletion;`（内部使用）+ 需要在 state.rs `use crate::provider_deletion::AppProviderDeletionCoordinator;`。

- [ ] **Step 4: 装配注入**

`state.rs` 的 `build_system_state`：把 `provider_service:` 一行改为注入协调器（`services` 已有 `companion_service`/`public_agent_service`；`client_prefs`/`fleet_repo` 从 `pool` 现建）：

```rust
    let client_pref_repo: Arc<dyn IClientPreferenceRepository> =
        Arc::new(SqliteClientPreferenceRepository::new(pool.clone()));
    let fleet_repo: Arc<dyn IFleetRepository> =
        Arc::new(SqliteFleetRepository::new(pool.clone()));
    let deletion_coordinator = Arc::new(crate::provider_deletion::AppProviderDeletionCoordinator {
        companion: services.companion_service.clone(),
        public_agent: services.public_agent_service.clone(),
        client_prefs: client_pref_repo,
        fleet_repo,
    });

    SystemRouterState {
        // …其余不变…
        provider_service: ProviderService::new(provider_repo.clone(), encryption_key)
            .with_deletion_coordinator(deletion_coordinator),
        // …
    }
```

顶部按需 `use` 引入 `SqliteClientPreferenceRepository`、`SqliteFleetRepository`、`IClientPreferenceRepository`、`IFleetRepository`（均 `nomifun_db` 根导出）。

- [ ] **Step 5: 运行确认通过 + 全后端编译**

Run: `cargo test -p nomifun-app aggregates_idmm_backup cleanup_strips 2>&1 | tail -20`
Expected: 两测试 PASS。
Run: `cargo build -p nomifun-app 2>&1 | tail -15`
Expected: 编译成功（装配无类型错误）。

- [ ] **Step 6: 提交**

```bash
git add crates/backend/nomifun-app/src/provider_deletion.rs crates/backend/nomifun-app/src/router/state.rs crates/backend/nomifun-app/src/lib.rs
git -c user.name=nomifun commit -m "feat(app): aggregate provider-in-use guard + failover-queue cleanup, wire into ProviderService"
```

---

### Task 7: Nomi 送出时供应商回退（nomifun-ai-agent）

**Files:**
- Modify: `crates/backend/nomifun-ai-agent/src/factory/provider_config.rs`（新 helper + 测试）
- Modify: `crates/backend/nomifun-ai-agent/src/factory/nomi.rs`（改用 helper）

**Interfaces:**
- Consumes: `resolve_provider_fields`（现有）、`crate::resolve_default_model`（现有，`Option<(String,String)>`）、`AppError::ProviderUnavailable`（T1）。
- Produces: `resolve_provider_fields_with_fallback(provider_repo, encryption_key, provider_id, model) -> Result<ResolvedProviderFields, AppError>`。

- [ ] **Step 1: 写失败测试**（复用 knowledge_completer.rs 测试的 `ListOnlyRepo` + `provider(...)` builder；把它们复制进 provider_config.rs 的 test mod，或 `use super::super::...` 视可见性）

```rust
    #[tokio::test]
    async fn fallback_uses_stored_provider_when_present() {
        let repo = list_only(vec![provider("prov_a", true, &["m1"], None)]);
        let f = resolve_provider_fields_with_fallback(&repo, &[0u8;32], "prov_a", "m1").await.unwrap();
        assert_eq!(f.model, "m1");
    }

    #[tokio::test]
    async fn fallback_to_first_enabled_when_provider_missing() {
        let repo = list_only(vec![provider("prov_a", true, &["m1"], None)]);
        // 会话里存的是已删除的 prov_dead
        let f = resolve_provider_fields_with_fallback(&repo, &[0u8;32], "prov_dead", "mX").await.unwrap();
        assert_eq!(f.model, "m1"); // 回退到首个可用
    }

    #[tokio::test]
    async fn fallback_on_empty_provider_id() {
        let repo = list_only(vec![provider("prov_a", true, &["m1"], None)]);
        let f = resolve_provider_fields_with_fallback(&repo, &[0u8;32], "", "").await.unwrap();
        assert_eq!(f.model, "m1");
    }

    #[tokio::test]
    async fn fallback_errors_provider_unavailable_when_none() {
        let repo = list_only(vec![provider("prov_a", false, &["m1"], None)]); // disabled
        let err = resolve_provider_fields_with_fallback(&repo, &[0u8;32], "prov_dead", "m").await.unwrap_err();
        assert!(matches!(err, AppError::ProviderUnavailable(_)));
    }
```

> `list_only(...)` / `provider(...)` = 复制自 knowledge_completer.rs:134-184 的 fixture（`ListOnlyRepo`、`fn provider(id, enabled, models, model_enabled)`）。放本 test mod 顶部。

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p nomifun-ai-agent resolve_provider_fields_with_fallback fallback_ 2>&1 | tail -20`
Expected: 编译失败（helper 不存在）。

- [ ] **Step 3: 实现 helper**

`provider_config.rs`（`use crate::resolve_default_model;` 或 `crate::knowledge_completer::resolve_default_model`；`tracing::warn` 可选）：

```rust
/// Resolve provider fields for a conversation send, never hard-failing on a
/// deleted/empty provider: fall back to the first enabled provider/model.
/// Only `AppError::ProviderUnavailable` when NOTHING is configured.
pub(crate) async fn resolve_provider_fields_with_fallback(
    provider_repo: &Arc<dyn IProviderRepository>,
    encryption_key: &[u8; 32],
    provider_id: &str,
    model: &str,
) -> Result<ResolvedProviderFields, AppError> {
    let stored_ok = !provider_id.is_empty()
        && provider_repo
            .find_by_id(provider_id)
            .await
            .map_err(|e| AppError::Internal(format!("Failed to load provider config: {e}")))?
            .is_some();

    if stored_ok {
        return resolve_provider_fields(provider_repo, encryption_key, provider_id, model).await;
    }

    match crate::resolve_default_model(provider_repo).await {
        Some((pid, m)) => {
            tracing::warn!(
                requested_provider = %provider_id,
                fallback_provider = %pid,
                fallback_model = %m,
                "conversation provider unavailable; falling back to first enabled model"
            );
            resolve_provider_fields(provider_repo, encryption_key, &pid, &m).await
        }
        None => Err(AppError::ProviderUnavailable(
            "no enabled model provider is configured".into(),
        )),
    }
}
```

- [ ] **Step 4: nomi.rs 改用 helper**

`nomi.rs:235` 处，把

```rust
    let fields = super::provider_config::resolve_provider_fields(
        &deps.provider_repo, &deps.encryption_key, provider_id, &model_id,
    ).await?;
```

改为：

```rust
    let fields = super::provider_config::resolve_provider_fields_with_fallback(
        &deps.provider_repo, &deps.encryption_key, provider_id, &model_id,
    ).await?;
```

`model_id` 在回退时不再等于 `fields.model`；后续 `NomiResolvedConfig { model: model_id, .. }`（nomi.rs:416）应改用解析后的模型，避免用到已删模型名：把该行改为 `model: fields.model.clone(),`（`ResolvedProviderFields.model` 已是最终解析模型）。

- [ ] **Step 5: 运行确认通过 + crate 编译**

Run: `cargo test -p nomifun-ai-agent fallback_ 2>&1 | tail -20`
Expected: 四测试 PASS。
Run: `cargo build -p nomifun-ai-agent 2>&1 | tail -15`
Expected: 成功。

- [ ] **Step 6: 提交**

```bash
git add crates/backend/nomifun-ai-agent/src/factory/provider_config.rs crates/backend/nomifun-ai-agent/src/factory/nomi.rs
git -c user.name=nomifun commit -m "feat(ai-agent): send-time provider fallback to first enabled model (no hard crash)"
```

---

### Task 8: 前端删除受阻 Modal（ModelModalContent.tsx）

**Files:**
- Create: `ui/src/renderer/components/settings/SettingsModal/contents/providerInUse.ts`（纯逻辑 + 测试）
- Create: `ui/src/renderer/components/settings/SettingsModal/contents/providerInUse.test.ts`
- Modify: `ui/src/renderer/components/settings/SettingsModal/contents/ModelModalContent.tsx`（removePlatform catch 分支 + Modal）
- Modify: `ui/src/renderer/services/i18n/locales/{zh-CN,en-US}/settings.json`

**Interfaces:**
- Consumes: `BackendHttpError`（`.code`,`.details`）、`isBackendHttpError`。
- Produces: `type ProviderUsage`（前端镜像）、`featureRoute(feature, targetId): string`、`groupUsagesByFeature(usages): {feature, labels}[]`。

- [ ] **Step 1: 写失败测试**

`providerInUse.test.ts`：

```ts
import { describe, expect, test } from 'bun:test';
import { featureRoute, groupUsagesByFeature, type ProviderUsage } from './providerInUse';

describe('providerInUse helpers', () => {
  test('featureRoute maps each feature', () => {
    expect(featureRoute('desktopCompanion')).toBe('/companion');
    expect(featureRoute('publicCompanion', 'pa_1')).toBe('/public-companions/pa_1');
    expect(featureRoute('publicCompanion')).toBe('/public-companions');
    expect(featureRoute('smartDecision')).toBe('/nomi');
    expect(featureRoute('orchestrator')).toBe('/guid');
  });

  test('groupUsagesByFeature groups labels', () => {
    const usages: ProviderUsage[] = [
      { feature: 'desktopCompanion', label: '甲', targetId: 'c1' },
      { feature: 'desktopCompanion', label: '乙', targetId: 'c2' },
      { feature: 'orchestrator', label: '舰队', targetId: 'f1' },
    ];
    const groups = groupUsagesByFeature(usages);
    expect(groups.find((g) => g.feature === 'desktopCompanion')?.labels).toEqual(['甲', '乙']);
    expect(groups.find((g) => g.feature === 'orchestrator')?.targetId).toBe('f1');
  });
});
```

- [ ] **Step 2: 运行确认失败**

Run: `cd ui && bun test src/renderer/components/settings/SettingsModal/contents/providerInUse.test.ts 2>&1 | tail -20`
Expected: 失败（模块不存在）。

- [ ] **Step 3: 实现纯逻辑**

`providerInUse.ts`：

```ts
export type ProviderUsageFeature = 'desktopCompanion' | 'publicCompanion' | 'smartDecision' | 'orchestrator';

export interface ProviderUsage {
  feature: ProviderUsageFeature;
  label: string;
  targetId?: string;
}

/** Deep-link route for a feature's unbind location. Verify against Router.tsx. */
export function featureRoute(feature: ProviderUsageFeature, targetId?: string): string {
  switch (feature) {
    case 'desktopCompanion':
      return '/companion';
    case 'publicCompanion':
      return targetId ? `/public-companions/${targetId}` : '/public-companions';
    case 'smartDecision':
      return '/nomi';
    case 'orchestrator':
      return '/guid';
  }
}

export interface ProviderUsageGroup {
  feature: ProviderUsageFeature;
  labels: string[];
  targetId?: string;
}

export function groupUsagesByFeature(usages: ProviderUsage[]): ProviderUsageGroup[] {
  const map = new Map<ProviderUsageFeature, ProviderUsageGroup>();
  for (const u of usages) {
    const g = map.get(u.feature) ?? { feature: u.feature, labels: [], targetId: u.targetId };
    g.labels.push(u.label);
    map.set(u.feature, g);
  }
  return [...map.values()];
}

/** Extract usages from a BackendHttpError.details payload. */
export function parseProviderInUseDetails(details: unknown): ProviderUsage[] {
  if (details && typeof details === 'object' && Array.isArray((details as { usages?: unknown }).usages)) {
    return (details as { usages: ProviderUsage[] }).usages;
  }
  return [];
}
```

- [ ] **Step 4: 运行确认通过**

Run: `cd ui && bun test src/renderer/components/settings/SettingsModal/contents/providerInUse.test.ts 2>&1 | tail -20`
Expected: PASS。

- [ ] **Step 5: 接入 Modal**

`ModelModalContent.tsx`：
- imports 追加：`import { Modal } from '@arco-design/web-react';`、`import { useNavigate } from 'react-router-dom';`、`import { isBackendHttpError } from '@/common/adapter/httpBridge';`、`import { featureRoute, groupUsagesByFeature, parseProviderInUseDetails, type ProviderUsageFeature } from './providerInUse';`
- 组件内加 `const navigate = useNavigate();`
- `removePlatform` catch 改为：

```tsx
      .catch((error) => {
        void mutate();
        console.error('Failed to delete provider:', error);
        if (isBackendHttpError(error) && error.code === 'PROVIDER_IN_USE') {
          const groups = groupUsagesByFeature(parseProviderInUseDetails(error.details));
          const featureName: Record<ProviderUsageFeature, string> = {
            desktopCompanion: t('settings.providerInUse.desktopCompanion'),
            publicCompanion: t('settings.providerInUse.publicCompanion'),
            smartDecision: t('settings.providerInUse.smartDecision'),
            orchestrator: t('settings.providerInUse.orchestrator'),
          };
          Modal.confirm({
            title: t('settings.providerInUse.title'),
            content: (
              <div className='flex flex-col gap-8px'>
                <div>{t('settings.providerInUse.desc')}</div>
                {groups.map((g) => (
                  <div key={g.feature}>
                    <b>{featureName[g.feature]}</b>：{g.labels.join('、')}
                  </div>
                ))}
              </div>
            ),
            okText: t('settings.providerInUse.goto'),
            cancelText: t('common.cancel', { defaultValue: '取消' }),
            onOk: () => {
              const first = groups[0];
              if (first) navigate(featureRoute(first.feature, first.targetId));
            },
          });
          return;
        }
        message.error(t('settings.saveModelConfigFailed'));
      });
```

- [ ] **Step 6: 加 i18n**

`zh-CN/settings.json` 顶层加：

```json
  "providerInUse": {
    "title": "该模型供应商正在被使用",
    "desc": "以下功能仍在使用该供应商，请先前往解绑或更换模型后再删除：",
    "goto": "前往处理",
    "desktopCompanion": "桌面伙伴",
    "publicCompanion": "对外伙伴",
    "smartDecision": "智能决策",
    "orchestrator": "智能编排"
  },
```

`en-US/settings.json` 顶层加：

```json
  "providerInUse": {
    "title": "This model provider is in use",
    "desc": "The following features still use this provider. Unbind or switch models first, then delete:",
    "goto": "Go fix it",
    "desktopCompanion": "Desktop companion",
    "publicCompanion": "Public companion",
    "smartDecision": "Smart decision",
    "orchestrator": "Orchestration"
  },
```

- [ ] **Step 7: 构建验证**

Run: `cd ui && bun run build 2>&1 | tail -20`
Expected: 构建成功（无类型错误）。

- [ ] **Step 8: 提交**

```bash
git add ui/src/renderer/components/settings/SettingsModal/contents/providerInUse.ts ui/src/renderer/components/settings/SettingsModal/contents/providerInUse.test.ts ui/src/renderer/components/settings/SettingsModal/contents/ModelModalContent.tsx ui/src/renderer/services/i18n/locales/zh-CN/settings.json ui/src/renderer/services/i18n/locales/en-US/settings.json
git -c user.name=nomifun commit -m "feat(ui): PROVIDER_IN_USE delete-block modal with per-feature unbind links"
```

---

### Task 9: 友好文案 code→i18n（conversationCreateError.ts）

**Files:**
- Modify: `ui/src/renderer/pages/conversation/utils/conversationCreateError.ts`（加 provider 错误码映射）
- Create: `ui/src/renderer/pages/conversation/utils/providerErrorMessage.test.ts`
- Modify: `ui/src/renderer/services/i18n/locales/{zh-CN,en-US}/conversation.json`（`agentError.codes.PROVIDER_UNAVAILABLE`）

**Interfaces:**
- Produces: `providerErrorI18nKey(code: string): string | undefined`（`PROVIDER_UNAVAILABLE` → `'conversation.agentError.codes.PROVIDER_UNAVAILABLE.body'`），并在 `getConversationRuntimeWorkspaceErrorMessage` 里优先命中。

- [ ] **Step 1: 写失败测试**

`providerErrorMessage.test.ts`：

```ts
import { describe, expect, test } from 'bun:test';
import { providerErrorI18nKey } from './conversationCreateError';

describe('providerErrorI18nKey', () => {
  test('maps PROVIDER_UNAVAILABLE', () => {
    expect(providerErrorI18nKey('PROVIDER_UNAVAILABLE')).toBe('conversation.agentError.codes.PROVIDER_UNAVAILABLE.body');
  });
  test('returns undefined for unrelated codes', () => {
    expect(providerErrorI18nKey('BAD_REQUEST')).toBeUndefined();
  });
});
```

- [ ] **Step 2: 运行确认失败**

Run: `cd ui && bun test src/renderer/pages/conversation/utils/providerErrorMessage.test.ts 2>&1 | tail -20`
Expected: 失败（`providerErrorI18nKey` 未导出）。

- [ ] **Step 3: 实现映射并接入**

`conversationCreateError.ts` 顶部加导出：

```ts
const PROVIDER_ERROR_CODES = new Set(['PROVIDER_UNAVAILABLE']);

/** i18n key for a provider-config error code, or undefined if not one. */
export function providerErrorI18nKey(code: string): string | undefined {
  return PROVIDER_ERROR_CODES.has(code) ? `conversation.agentError.codes.${code}.body` : undefined;
}
```

`getConversationRuntimeWorkspaceErrorMessage` 开头（取到 payload 后、走 workspace 分支前）插入优先分支：

```ts
export const getConversationRuntimeWorkspaceErrorMessage = (error: unknown, t: TFunction): string => {
  const payload = getWorkspacePathErrorPayload(error);
  const providerKey = payload?.code ? providerErrorI18nKey(payload.code) : undefined;
  if (providerKey) {
    return t(providerKey, { defaultValue: t('conversation.agentError.codes.PROVIDER_UNAVAILABLE.body') });
  }
  // …以下保持原有 normalizedCode / rawMessage 逻辑不变…
  const normalizedCode = normalizeConversationRuntimeWorkspaceErrorCode(error);
  const workspacePath = getWorkspacePathFromErrorDetails(error);
  const rawMessage = payload?.error || parseError(error) || t('common.unknownError');
  // …（原样）…
};
```

- [ ] **Step 4: 加 i18n**

`zh-CN/conversation.json` 的 `agentError.codes` 里加：

```json
      "PROVIDER_UNAVAILABLE": {
        "title": "当前没有可用的模型",
        "body": "当前没有可用的模型供应商，请到「模型&Agent」添加并启用一个模型后重试。"
      },
```

`en-US/conversation.json` 对应：

```json
      "PROVIDER_UNAVAILABLE": {
        "title": "No model available",
        "body": "No usable model provider is configured. Add and enable one in Models & Agent, then retry."
      },
```

- [ ] **Step 5: 运行确认通过 + 构建**

Run: `cd ui && bun test src/renderer/pages/conversation/utils/providerErrorMessage.test.ts 2>&1 | tail -20`
Expected: PASS。
Run: `cd ui && bun run build 2>&1 | tail -15`
Expected: 成功。

- [ ] **Step 6: 提交**

```bash
git add ui/src/renderer/pages/conversation/utils/conversationCreateError.ts ui/src/renderer/pages/conversation/utils/providerErrorMessage.test.ts ui/src/renderer/services/i18n/locales/zh-CN/conversation.json ui/src/renderer/services/i18n/locales/en-US/conversation.json
git -c user.name=nomifun commit -m "feat(ui): friendly PROVIDER_UNAVAILABLE message replacing raw provider-id error"
```

---

### Task 10: 普通会话前端自愈 + 轻提示（ChatConversation.tsx）

**Files:**
- Create: `ui/src/renderer/pages/conversation/platforms/nomi/healConversationModel.ts`（纯逻辑 + 测试）
- Create: `ui/src/renderer/pages/conversation/platforms/nomi/healConversationModel.test.ts`
- Modify: `ui/src/renderer/pages/conversation/components/ChatConversation.tsx`（加自愈 effect）

**Interfaces:**
- Consumes: `IProvider`、`TProviderWithModel`、`getAvailableModels`、`configService.get('nomi.defaultModel')`。
- Produces: `resolveHealModel(bound, providers, getAvailableModels, savedDefault): { provider: IProvider; use_model: string } | null`——当 `bound` 的 provider 在列表中不可用时，返回默认(或首个可用)模型；否则返回 `null`（无需自愈）。

- [ ] **Step 1: 写失败测试**

`healConversationModel.test.ts`：

```ts
import { describe, expect, test } from 'bun:test';
import { resolveHealModel } from './healConversationModel';

const getAvailable = (p: any) => (p.models ?? []) as string[];
const provs = [
  { id: 'prov_a', models: ['m1', 'm2'] },
  { id: 'prov_b', models: ['m3'] },
] as any[];

describe('resolveHealModel', () => {
  test('returns null when bound provider still available', () => {
    expect(resolveHealModel({ id: 'prov_a', use_model: 'm1' } as any, provs, getAvailable, undefined)).toBeNull();
  });
  test('heals to saved default when bound provider gone', () => {
    const r = resolveHealModel({ id: 'prov_dead', use_model: 'x' } as any, provs, getAvailable, { id: 'prov_b', use_model: 'm3' });
    expect(r?.provider.id).toBe('prov_b');
    expect(r?.use_model).toBe('m3');
  });
  test('heals to first available when no valid default', () => {
    const r = resolveHealModel({ id: 'prov_dead', use_model: 'x' } as any, provs, getAvailable, undefined);
    expect(r?.provider.id).toBe('prov_a');
    expect(r?.use_model).toBe('m1');
  });
  test('returns null when there are no providers at all', () => {
    expect(resolveHealModel({ id: 'prov_dead', use_model: 'x' } as any, [], getAvailable, undefined)).toBeNull();
  });
});
```

- [ ] **Step 2: 运行确认失败**

Run: `cd ui && bun test src/renderer/pages/conversation/platforms/nomi/healConversationModel.test.ts 2>&1 | tail -20`
Expected: 失败。

- [ ] **Step 3: 实现纯逻辑**

`healConversationModel.ts`：

```ts
import type { IProvider } from '@/common/config/storage';
import type { TProviderWithModel } from '@/common/adapter/ipcBridge';

type SavedDefault = { id: string; use_model: string } | undefined;

/**
 * If `bound` points at a provider/model no longer available, resolve a
 * replacement (saved default → first available). Returns null when no heal
 * is needed or nothing is available.
 */
export function resolveHealModel(
  bound: TProviderWithModel | undefined,
  providers: IProvider[],
  getAvailableModels: (p: IProvider) => string[],
  savedDefault: SavedDefault
): { provider: IProvider; use_model: string } | null {
  if (!providers.length) return null;

  const boundProvider = bound?.id ? providers.find((p) => p.id === bound.id) : undefined;
  const boundStillValid =
    !!boundProvider && !!bound?.use_model && getAvailableModels(boundProvider).includes(bound.use_model);
  if (boundStillValid) return null;
  // 如果会话本就没绑定任何模型（空 id），交给已有 noModelSelected 流程，不在此自愈
  if (!bound?.id) return null;

  if (savedDefault) {
    const dp = providers.find((p) => p.id === savedDefault.id);
    if (dp && getAvailableModels(dp).includes(savedDefault.use_model)) {
      return { provider: dp, use_model: savedDefault.use_model };
    }
  }
  const first = providers[0];
  const firstModel = getAvailableModels(first)[0];
  if (!firstModel) return null;
  return { provider: first, use_model: firstModel };
}
```

- [ ] **Step 4: 运行确认通过**

Run: `cd ui && bun test src/renderer/pages/conversation/platforms/nomi/healConversationModel.test.ts 2>&1 | tail -20`
Expected: 4 测试 PASS。

- [ ] **Step 5: 接入自愈 effect**

`ChatConversation.tsx`（已含 `onSelectModel`、`ipcBridge.conversation.update`、`saveNomiDefaultModel`、`useModelProviderList`/`getAvailableModels`；`Message` from arco、`configService`、`useTranslation`）新增 effect（放在 `onSelectModel`/`modelSelection` 定义之后）：

```tsx
  const { providers: healProviders, getAvailableModels: healGetAvailable } = useModelProviderList();
  useEffect(() => {
    if (!healProviders.length) return;
    const saved = configService.get('nomi.defaultModel');
    const heal = resolveHealModel(
      conversation.model,
      healProviders,
      healGetAvailable,
      saved && typeof saved === 'object' && 'id' in saved ? saved : undefined
    );
    if (!heal) return;
    void (async () => {
      const selected = { ...heal.provider, use_model: heal.use_model } as TProviderWithModel;
      const ok = await ipcBridge.conversation.update.invoke({ id: conversation.id, updates: { model: selected } });
      if (ok) {
        void saveNomiDefaultModel(heal.provider.id, heal.use_model);
        Message.info(t('conversation.chat.modelHealedToDefault', { model: heal.use_model }));
      }
    })();
    // 仅在会话或供应商列表变化时评估
  }, [conversation.id, conversation.model?.id, conversation.model?.use_model, healProviders, healGetAvailable, t]);
```

imports 补：`import { resolveHealModel } from '../platforms/nomi/healConversationModel';`（路径按文件相对位置调整）、`import { configService } from '@/common/config/configService';`（若未引入）、`useModelProviderList`、`Message`、`useTranslation`（多已在，缺则补）。

i18n（两语言 `conversation.json` 的 `chat` 段）加 `modelHealedToDefault`：
- zh-CN：`"modelHealedToDefault": "已回退到默认模型 {{model}}"`
- en-US：`"modelHealedToDefault": "Switched to your default model {{model}}"`

- [ ] **Step 6: 构建验证**

Run: `cd ui && bun run build 2>&1 | tail -20`
Expected: 成功。

- [ ] **Step 7: 提交**

```bash
git add ui/src/renderer/pages/conversation/platforms/nomi/healConversationModel.ts ui/src/renderer/pages/conversation/platforms/nomi/healConversationModel.test.ts ui/src/renderer/pages/conversation/components/ChatConversation.tsx ui/src/renderer/services/i18n/locales/zh-CN/conversation.json ui/src/renderer/services/i18n/locales/en-US/conversation.json
git -c user.name=nomifun commit -m "feat(ui): self-heal stale conversation model to default + notice"
```

---

### Task 11: 全量验证 + 手动回归

**Files:** 无（验证）

- [ ] **Step 1: 后端全测**

Run: `cargo test -p nomifun-common -p nomifun-system -p nomifun-db -p nomifun-companion -p nomifun-public-agent -p nomifun-app -p nomifun-ai-agent 2>&1 | tail -30`
Expected: 全绿。（`release:win` 偶发链接堆损坏与本改动无关，重跑即过。）

- [ ] **Step 2: 前端构建 + 单测**

Run: `cd ui && bun run build 2>&1 | tail -15`
Run: `cd ui && bun test src/renderer/components/settings/SettingsModal/contents/providerInUse.test.ts src/renderer/pages/conversation/utils/providerErrorMessage.test.ts src/renderer/pages/conversation/platforms/nomi/healConversationModel.test.ts 2>&1 | tail -20`
Expected: 构建成功 + 三测试文件全绿。

- [ ] **Step 3: 手动回归（对应截图场景）**

1. 建 provider A，用它建一个普通会话并发消息成功；再删 provider A → 打开该会话：**不再弹两条 `Bad request: Provider 'prov_...' not found`**；会话模型自愈为默认并提示「已回退到默认模型 X」，可正常发送。
2. 用 provider B 配一个桌面伙伴的模型；删 provider B → 弹 Modal「该模型供应商正在被使用 / 桌面伙伴：<名>」，点「前往处理」跳 `/companion`；删除被阻止。
3. 智能决策全局备份设 provider C；删 C → Modal 显示「智能决策：智能决策·备份模型」，删除被阻止。
4. 智能编排 fleet 引用 provider D；删 D → Modal 显示「智能编排：<fleet 名>」，删除被阻止。
5. 删一个仅被失败回退队列引用的 provider → 删除成功，且该队列项被清理（不阻止）。

- [ ] **Step 4: 收尾提交（若手动回归需微调）**

```bash
git -c user.name=nomifun commit -am "fix(provider-guard): manual-regression follow-ups"   # 仅当有修改
```

---

## Self-Review 记录

- **Spec 覆盖**：组件 A=Task1–6/8；组件 B=Task7（后端不崩）+Task10（前端自愈+提示）；组件 C=Task1（ProviderUnavailable 码）+Task9（i18n 文案）。软清理=Task6（failover）。✅
- **范围偏移修正**：`knowledge.autogenModel` 改判为前端-only（后端不清理）；IDMM 拦截 v1 缩到全局备份；渠道 defaultModel 由 B 兜底——均在 Global Constraints 标注。
- **类型一致性**：`ProviderUsage/ProviderUsageFeature/ProviderInUseDetails` 后端（nomifun-common）与前端（providerInUse.ts）镜像，camelCase 对齐（`desktopCompanion` 等）；`resolve_provider_fields_with_fallback` 在 T7 定义、nomi.rs 调用同名；`fleets_using_provider`/`providers_in_use`/`with_deletion_coordinator`/`cleanup_soft_refs` 全计划一致。
- **占位符**：Task3 的 `sample_fleet_params()`/`sample_member()` 明确标注为"以本文件既有测试真实构造替换"，非交付占位。
