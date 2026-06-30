# 多 Agent 智能编排引擎 · P0 实施计划（基础层）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 落地「智能编排」功能的基础层：新 crate `nomifun-orchestrator` + 迁移 018（全部 8 张表）+ 仓库 + 编队(Fleet)/工作间(Workspace) CRUD 后端 + 前端侧栏 tab/路由/页面壳 + 编队管理 UI。交付后用户可在真实 app 内创建/编辑/删除编队与工作间。

**Architecture:** 镜像 `nomifun-webhook`（最小域 crate 模板）+ `nomifun-requirement`（持久服务）的分层。后端：迁移 → Row 模型 → 仓库(trait+sqlite impl) → api-types DTO → 域 crate(service/routes/state) → app 接线。前端：ipcBridge REST 客户端 → 侧栏 entry+路由+i18n → ContentSider 三段页壳 → 编队管理卡片网格。P0 **不**碰 Run 执行引擎/主管/调度器/画布（那是 P1–P3），但迁移 018 一次性建好全部 8 张表（schema 在 spec 已定，避免迁移churn）。

**Tech Stack:** Rust（axum 0.8 / sqlx / async-trait / thiserror / ts-rs 12）、SQLite、React 19 + Arco Design + UnoCSS + react-router HashRouter、SWR。

**设计文档（spec）：** `docs/superpowers/specs/2026-06-26-multi-agent-orchestrator-design.md`（权威；本计划实现其 §3/§5/§9 的 P0 切片）。

## Global Constraints

以下为全工程约束，**每个任务隐含包含**（逐条来自 spec 与项目记忆，值照抄）：

- **迁移 append-only**：新增 `018_orchestrator.sql`，**绝不**编辑 001 或任何已有迁移。`PRAGMA foreign_keys=ON`（连接已设），叶子 `CREATE TABLE`+`CREATE INDEX`，无表重建。最新已有迁移 = 017。
- **设备边界 ID 规则**：跨 gateway/Remote 暴露的实体用字符串前缀 id `generate_prefixed_id(prefix)`（`{prefix}_{9time}{7rand}`）；FK 列类型必须等于被引用表 PK 类型。本功能：`fleet_`/`fmem_`/`ows_`/`run_`/`rtask_`/`asg_` 全用字符串 TEXT PK；worker 的 `conversation_id` 是本机 `conversations.id` INTEGER。
- **仓库模式**：每表一个 `I{Name}Repository` trait（`repository/{name}.rs`）+ 一个 `Sqlite{Name}Repository` impl（`repository/sqlite_{name}.rs`），都持 cloned `SqlitePool`；SQL 手写 `sqlx::query/query_as`（**无** compile-time 宏）；插入时忽略 row 的 id 字段（TEXT PK 由应用层 `generate_prefixed_id` 生成并显式写入，**非** autoincrement）。
- **域 crate 锚定模板** = `crates/backend/nomifun-webhook`（Cargo.toml / lib.rs / routes.rs / state.rs / service.rs / error.rs 的形状照搬）。
- **路由层薄**：handler 只做 extract（`State` + `Extension<CurrentUser>` + `Path`/`Json`）→ 调 service → `ApiResponse` 包装；逻辑全在 service；service **无** axum import，方法返回 `Result<_, AppError>`；auth 中间件在 nomifun-app 外层施加。
- **ts-rs i64/u64 字段**必须 `#[ts(type = "number")]` 否则前端收到 bigint。（P0 无 ts-rs 事件，仅约束后续）
- **前端 typecheck 必须归零**：用 `cd ui && npm run typecheck`（**不是** `npx tsc`，会误报 0）；本机无 vitest，**不新增前端单测文件**；改 locale 后跑 `bun run gen:i18n`（仓库根）+ 同步 `i18n-keys.d.ts`。
- **前端铁律**：禁 `any`/`ts-ignore`/改无关行为；颜色一律 CSS 主题变量（`var(--*)` / UnoCSS 语义 token），禁硬编码 hex；`@icon-park/react` 具名导入**不起别名**；Arco 弹窗经 `useArcoMessage` 包装（不裸用 `Message.useMessage`）；无 UnoCSS button reset → 用 `<div onClick>` 不用裸 `<button>`；桌面壳判断用 `isDesktopShell()`（**非** `isElectronDesktop()` 死桩）。
- **测试**：开发中只跑触碰的 crate（`cargo nextest run -p <crate>`），全量仅收尾一次。
- **品牌**：字样 NomiFun（非 Nomifun）；用户可见名「智能编排」；内部标识符 `orchestrator`/`fleet` 不动。
- **提交**：每任务末尾 commit（feature 分支 `feat/multi-agent-orchestrator`，已建）；提交前 `git pull --rebase`（多端并行，注意迁移号撞号）。

## File Structure（P0 创建/修改）

**后端**
- 创建 `crates/backend/nomifun-db/migrations/018_orchestrator.sql` — 全 8 表 DDL。
- 创建 `crates/backend/nomifun-db/src/models/orchestrator.rs` — 8 个 `*Row` 结构。
- 修改 `crates/backend/nomifun-db/src/models/mod.rs` — `mod orchestrator;` + 8 个 `pub use`。
- 创建 `crates/backend/nomifun-db/src/repository/orch_fleet.rs` + `sqlite_orch_fleet.rs` — `IFleetRepository`（含 fleet + fleet_members）。
- 创建 `crates/backend/nomifun-db/src/repository/orch_workspace.rs` + `sqlite_orch_workspace.rs` — `IOrchWorkspaceRepository`。
- 修改 `crates/backend/nomifun-db/src/repository/mod.rs` — 各 4 行 re-export。
- 创建 `crates/backend/nomifun-api-types/src/orchestrator.rs` — DTO（Fleet/FleetMember/Workspace + Create/Update + CapabilityProfile/MemberConstraints）。
- 修改 `crates/backend/nomifun-api-types/src/lib.rs` — `pub mod orchestrator;` + re-export。
- 创建 crate `crates/backend/nomifun-orchestrator/`：`Cargo.toml`、`src/lib.rs`、`src/error.rs`、`src/state.rs`、`src/service.rs`（FleetService + WorkspaceService）、`src/routes.rs`。
- 修改根 `Cargo.toml` — 加 `nomifun-orchestrator` workspace member + `[workspace.dependencies]` 条目。
- 修改 `crates/backend/nomifun-app/src/router/state.rs` — `use` + `ModuleStates` 字段 + `build_orchestrator_state` + `build_module_states` 调用。
- 修改 `crates/backend/nomifun-app/src/router/routes.rs` — `use nomifun_orchestrator::orchestrator_routes;` + `let orchestrator_authenticated = ...` + `.merge(orchestrator_authenticated)`。

**前端**
- 修改 `ui/src/common/adapter/ipcBridge.ts` — `orchestrator = {...}` REST 客户端块。
- 创建 `ui/src/common/types/orchestrator/orchestratorTypes.ts` — 手写 TS 类型镜像 DTO。
- 创建 `ui/src/renderer/components/layout/Sider/SiderNav/SiderOrchestratorEntry.tsx` — 侧栏 entry（copy `SiderModelHubEntry.tsx`）。
- 修改 `ui/src/renderer/components/layout/Sider/SiderNav/index.ts` — re-export。
- 修改 `ui/src/renderer/components/layout/Sider/index.tsx` — 常用组「会话」下插入 entry + navTo + isActive。
- 修改 `ui/src/renderer/components/layout/Router.tsx` — `/orchestrator` lazy 路由。
- 修改 `ui/src/renderer/services/i18n/locales/zh-CN/common.json` + `en-US/common.json` — 新增导航 label；创建 `locales/{zh-CN,en-US}/orchestrator.json` 命名空间；同步 `i18n-keys.d.ts`。
- 创建 `ui/src/renderer/pages/orchestrator/index.tsx` — 页壳（ContentSider 三段 `?section=`）。
- 创建 `ui/src/renderer/pages/orchestrator/FleetManager.tsx` + `FleetCard.tsx` + `FleetEditDrawer.tsx` — 编队管理卡片网格。
- 创建 `ui/src/renderer/pages/orchestrator/WorkspaceList.tsx` — 工作间列表 + 创建。
- 创建 `ui/src/renderer/pages/orchestrator/RunHistory.tsx` — P0 占位（空态卡，标「即将上线」）。
- 创建 `ui/src/renderer/pages/orchestrator/useOrchestratorData.ts` — SWR fetch hooks。

---

## Task 1: 迁移 018 + Row 模型

**Files:**
- Create: `crates/backend/nomifun-db/migrations/018_orchestrator.sql`
- Create: `crates/backend/nomifun-db/src/models/orchestrator.rs`
- Modify: `crates/backend/nomifun-db/src/models/mod.rs`
- Test: `crates/backend/nomifun-db/src/models/orchestrator.rs`（`#[cfg(test)]` 内联）

**Interfaces:**
- Consumes: `nomifun_common::TimestampMs`（= i64）；`sqlx::FromRow`；现有 `init_database_memory()`（跑全部迁移于内存 DB）。
- Produces: `FleetRow`、`FleetMemberRow`、`OrchWorkspaceRow`、`OrchRunRow`、`OrchRunTaskRow`、`OrchRunTaskDepRow`、`OrchAssignmentRow`（注：P0 仅用前三个，但全部建表+建模以免后续迁移 churn）。字段类型见下。

参照模板：`crates/backend/nomifun-db/migrations/017_cron_job_runs.sql`（迁移格式）、`crates/backend/nomifun-db/src/models/webhook.rs`（Row 形状）、`001_baseline.sql` 头部注释（ID 规则）。

- [ ] **Step 1: 写迁移 SQL**

`crates/backend/nomifun-db/migrations/018_orchestrator.sql`（照抄 spec §5 DDL，全 8 表——注意 spec §5 列了 7 个 CREATE TABLE，本步全部落地）：

```sql
-- 018 智能编排引擎(取代遗留 team)。append-only；PRAGMA foreign_keys=ON 已由连接设定。
-- ID 规则: 跨 gateway/Remote 暴露的实体用 TEXT 前缀 id(应用层 generate_prefixed_id 生成);
-- worker conversation_id 是本机 conversations.id INTEGER。

CREATE TABLE fleets (
  id            TEXT PRIMARY KEY,
  user_id       TEXT NOT NULL,
  name          TEXT NOT NULL,
  description   TEXT,
  max_parallel  INTEGER,
  created_at    INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL
);

CREATE TABLE fleet_members (
  id                 TEXT PRIMARY KEY,
  fleet_id           TEXT NOT NULL REFERENCES fleets(id) ON DELETE CASCADE,
  agent_id           TEXT NOT NULL,
  provider_id        TEXT,
  model              TEXT,
  role_hint          TEXT,
  capability_profile TEXT,
  constraints        TEXT,
  sort_order         INTEGER NOT NULL DEFAULT 0,
  created_at         INTEGER NOT NULL,
  updated_at         INTEGER NOT NULL
);
CREATE INDEX idx_fleet_members_fleet ON fleet_members(fleet_id);

CREATE TABLE orch_workspaces (
  id                TEXT PRIMARY KEY,
  user_id           TEXT NOT NULL,
  name              TEXT NOT NULL,
  default_fleet_id  TEXT REFERENCES fleets(id) ON DELETE SET NULL,
  workspace_dir     TEXT,
  context           TEXT,
  created_at        INTEGER NOT NULL,
  updated_at        INTEGER NOT NULL
);

CREATE TABLE orch_runs (
  id              TEXT PRIMARY KEY,
  workspace_id    TEXT NOT NULL REFERENCES orch_workspaces(id) ON DELETE CASCADE,
  user_id         TEXT NOT NULL,
  goal            TEXT NOT NULL,
  fleet_snapshot  TEXT NOT NULL,
  autonomy        TEXT NOT NULL,
  max_parallel    INTEGER,
  lead_conv_id    INTEGER,
  status          TEXT NOT NULL,
  summary         TEXT,
  total_tokens    INTEGER,
  forked_from     TEXT,
  created_at      INTEGER NOT NULL,
  updated_at      INTEGER NOT NULL
);
CREATE INDEX idx_orch_runs_workspace ON orch_runs(workspace_id);

CREATE TABLE orch_run_tasks (
  id              TEXT PRIMARY KEY,
  run_id          TEXT NOT NULL REFERENCES orch_runs(id) ON DELETE CASCADE,
  title           TEXT NOT NULL,
  spec            TEXT NOT NULL,
  task_profile    TEXT,
  status          TEXT NOT NULL,
  conversation_id INTEGER,
  output_summary  TEXT,
  output_files    TEXT,
  attempt         INTEGER NOT NULL DEFAULT 0,
  tokens          INTEGER,
  graph_x         REAL,
  graph_y         REAL,
  created_at      INTEGER NOT NULL,
  updated_at      INTEGER NOT NULL
);
CREATE INDEX idx_orch_run_tasks_run ON orch_run_tasks(run_id);

CREATE TABLE orch_run_task_deps (
  blocker_task_id TEXT NOT NULL REFERENCES orch_run_tasks(id) ON DELETE CASCADE,
  blocked_task_id TEXT NOT NULL REFERENCES orch_run_tasks(id) ON DELETE CASCADE,
  PRIMARY KEY (blocker_task_id, blocked_task_id),
  CHECK (blocker_task_id <> blocked_task_id)
);

CREATE TABLE orch_assignments (
  id          TEXT PRIMARY KEY,
  task_id     TEXT NOT NULL REFERENCES orch_run_tasks(id) ON DELETE CASCADE,
  member_id   TEXT NOT NULL,
  score       REAL,
  rationale   TEXT,
  source      TEXT NOT NULL,
  locked      INTEGER NOT NULL DEFAULT 0,
  created_at  INTEGER NOT NULL
);
CREATE INDEX idx_orch_assignments_task ON orch_assignments(task_id);
```

- [ ] **Step 2: 写 Row 模型**

`crates/backend/nomifun-db/src/models/orchestrator.rs`（每个 Row `#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize, serde::Deserialize)]`，时间戳用 `nomifun_common::TimestampMs`，可空列用 `Option<...>`，JSON 列存原始 `Option<String>`）。P0 必须的三个示例（其余 4 个同构照建）：

```rust
use nomifun_common::TimestampMs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, sqlx::FromRow, Serialize, Deserialize)]
pub struct FleetRow {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub description: Option<String>,
    pub max_parallel: Option<i64>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, sqlx::FromRow, Serialize, Deserialize)]
pub struct FleetMemberRow {
    pub id: String,
    pub fleet_id: String,
    pub agent_id: String,
    pub provider_id: Option<String>,
    pub model: Option<String>,
    pub role_hint: Option<String>,
    pub capability_profile: Option<String>, // JSON
    pub constraints: Option<String>,        // JSON
    pub sort_order: i64,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, sqlx::FromRow, Serialize, Deserialize)]
pub struct OrchWorkspaceRow {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub default_fleet_id: Option<String>,
    pub workspace_dir: Option<String>,
    pub context: Option<String>, // JSON
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}
```
（再补 `OrchRunRow`、`OrchRunTaskRow`、`OrchRunTaskDepRow`、`OrchAssignmentRow`，字段 1:1 对应 DDL；`graph_x/graph_y` 用 `Option<f64>`，`locked` 用 `i64`（0/1），`score` 用 `Option<f64>`。）

- [ ] **Step 3: 接 models/mod.rs**

在 `crates/backend/nomifun-db/src/models/mod.rs` 加（仿现有行）：
```rust
mod orchestrator;
pub use orchestrator::{
    FleetRow, FleetMemberRow, OrchWorkspaceRow, OrchRunRow, OrchRunTaskRow,
    OrchRunTaskDepRow, OrchAssignmentRow,
};
```

- [ ] **Step 4: 写迁移应用测试（失败优先）**

在 `crates/backend/nomifun-db/src/models/orchestrator.rs` 底部加：
```rust
#[cfg(test)]
mod tests {
    use crate::database::init_database_memory;

    #[tokio::test]
    async fn migration_018_creates_orchestrator_tables() {
        let db = init_database_memory().await.expect("db init runs all migrations");
        let pool = db.pool();
        // 断言 7 张表存在
        for t in [
            "fleets", "fleet_members", "orch_workspaces", "orch_runs",
            "orch_run_tasks", "orch_run_task_deps", "orch_assignments",
        ] {
            let row: (i64,) = sqlx::query_as(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?",
            )
            .bind(t)
            .fetch_one(pool)
            .await
            .unwrap();
            assert_eq!(row.0, 1, "table {t} should exist");
        }
    }
}
```
> 若 `init_database_memory` 的确切签名/返回不同，对照 `crates/backend/nomifun-db/src/database.rs` 调整（它跑 `sqlx::migrate!()`）。

- [ ] **Step 5: 跑测试看它先失败再通过**

Run: `cargo nextest run -p nomifun-db migration_018`
Expected：先因迁移未建/Row 未声明失败 → 实现后 PASS。

- [ ] **Step 6: 提交**

```bash
git add crates/backend/nomifun-db/migrations/018_orchestrator.sql crates/backend/nomifun-db/src/models/orchestrator.rs crates/backend/nomifun-db/src/models/mod.rs
git commit -m "feat(orchestrator): 迁移018 + Row 模型(8表)"
```

---

## Task 2: Fleet & FleetMember 仓库

**Files:**
- Create: `crates/backend/nomifun-db/src/repository/orch_fleet.rs`（trait `IFleetRepository`）
- Create: `crates/backend/nomifun-db/src/repository/sqlite_orch_fleet.rs`（`SqliteFleetRepository`）
- Modify: `crates/backend/nomifun-db/src/repository/mod.rs`
- Test: `sqlite_orch_fleet.rs` `#[cfg(test)]` 内联

**Interfaces:**
- Consumes: `FleetRow`、`FleetMemberRow`（Task 1）；`SqlitePool`；`nomifun_common::generate_prefixed_id`、`now_ms()`（对照 `nomifun-common`）。
- Produces:
```rust
#[async_trait::async_trait]
pub trait IFleetRepository: Send + Sync {
    async fn create_fleet(&self, p: CreateFleetParams) -> Result<FleetRow, sqlx::Error>;
    async fn list_fleets(&self, user_id: &str) -> Result<Vec<FleetRow>, sqlx::Error>;
    async fn get_fleet(&self, id: &str) -> Result<Option<FleetRow>, sqlx::Error>;
    async fn update_fleet(&self, id: &str, p: UpdateFleetParams) -> Result<(), sqlx::Error>;
    async fn delete_fleet(&self, id: &str) -> Result<(), sqlx::Error>;
    async fn list_members(&self, fleet_id: &str) -> Result<Vec<FleetMemberRow>, sqlx::Error>;
    async fn replace_members(&self, fleet_id: &str, members: Vec<NewFleetMember>) -> Result<(), sqlx::Error>;
}
// 参数结构(本文件内 pub):
pub struct CreateFleetParams { pub user_id: String, pub name: String, pub description: Option<String>, pub max_parallel: Option<i64> }
pub struct UpdateFleetParams { pub name: Option<String>, pub description: Option<Option<String>>, pub max_parallel: Option<Option<i64>> }
pub struct NewFleetMember { pub agent_id: String, pub provider_id: Option<String>, pub model: Option<String>, pub role_hint: Option<String>, pub capability_profile: Option<String>, pub constraints: Option<String>, pub sort_order: i64 }
```
> `replace_members` = 事务内删旧 + 批量插新（编队成员是整体编辑语义）。

参照模板：`crates/backend/nomifun-db/src/repository/webhook.rs` + `sqlite_webhook.rs`（trait+impl 形状、手写 SQL、`generate_prefixed_id`）。

- [ ] **Step 1: 写仓库往返测试（失败优先）**

`sqlite_orch_fleet.rs` 内：
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::init_database_memory;

    #[tokio::test]
    async fn fleet_crud_and_member_replace_roundtrip() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteFleetRepository::new(db.pool().clone());
        let f = repo.create_fleet(CreateFleetParams {
            user_id: "u1".into(), name: "团队A".into(), description: None, max_parallel: Some(3),
        }).await.unwrap();
        assert!(f.id.starts_with("fleet_"));
        repo.replace_members(&f.id, vec![NewFleetMember {
            agent_id: "agent_builtin_claude".into(), provider_id: Some("prov_x".into()),
            model: Some("claude-opus-4-8".into()), role_hint: Some("后端".into()),
            capability_profile: Some("{\"strengths\":[\"coding\"]}".into()),
            constraints: None, sort_order: 0,
        }]).await.unwrap();
        let members = repo.list_members(&f.id).await.unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].agent_id, "agent_builtin_claude");
        // 删 fleet → 成员级联删
        repo.delete_fleet(&f.id).await.unwrap();
        assert!(repo.get_fleet(&f.id).await.unwrap().is_none());
        assert_eq!(repo.list_members(&f.id).await.unwrap().len(), 0);
    }
}
```

- [ ] **Step 2: 跑测试确认失败** — `cargo nextest run -p nomifun-db fleet_crud` → FAIL（类型未定义）。

- [ ] **Step 3: 写 trait（`orch_fleet.rs`）** — 照上 Interfaces 定义 trait + 参数结构。

- [ ] **Step 4: 写 sqlite impl（`sqlite_orch_fleet.rs`）** — `SqliteFleetRepository { pool: SqlitePool }` + `new(pool)`；手写 SQL；`create_fleet` 用 `generate_prefixed_id("fleet")` 显式写 id；`replace_members` 用 `pool.begin()` 事务（成员 id 用 `generate_prefixed_id("fmem")`）。参照 `sqlite_webhook.rs` 的 query 写法。

- [ ] **Step 5: 接 repository/mod.rs** — 加 4 行：
```rust
pub mod orch_fleet;
mod sqlite_orch_fleet;
pub use orch_fleet::{IFleetRepository, CreateFleetParams, UpdateFleetParams, NewFleetMember};
pub use sqlite_orch_fleet::SqliteFleetRepository;
```

- [ ] **Step 6: 跑测试确认通过** — `cargo nextest run -p nomifun-db fleet_crud` → PASS。

- [ ] **Step 7: 提交** — `git commit -m "feat(orchestrator): Fleet/FleetMember 仓库"`

---

## Task 3: OrchWorkspace 仓库

**Files:**
- Create: `crates/backend/nomifun-db/src/repository/orch_workspace.rs` + `sqlite_orch_workspace.rs`
- Modify: `crates/backend/nomifun-db/src/repository/mod.rs`
- Test: `sqlite_orch_workspace.rs` 内联

**Interfaces:**
- Produces:
```rust
#[async_trait::async_trait]
pub trait IOrchWorkspaceRepository: Send + Sync {
    async fn create(&self, p: CreateOrchWorkspaceParams) -> Result<OrchWorkspaceRow, sqlx::Error>;
    async fn list(&self, user_id: &str) -> Result<Vec<OrchWorkspaceRow>, sqlx::Error>;
    async fn get(&self, id: &str) -> Result<Option<OrchWorkspaceRow>, sqlx::Error>;
    async fn update(&self, id: &str, p: UpdateOrchWorkspaceParams) -> Result<(), sqlx::Error>;
    async fn delete(&self, id: &str) -> Result<(), sqlx::Error>;
}
pub struct CreateOrchWorkspaceParams { pub user_id: String, pub name: String, pub default_fleet_id: Option<String>, pub workspace_dir: Option<String>, pub context: Option<String> }
pub struct UpdateOrchWorkspaceParams { pub name: Option<String>, pub default_fleet_id: Option<Option<String>>, pub workspace_dir: Option<Option<String>>, pub context: Option<Option<String>> }
```
- Consumes: `OrchWorkspaceRow`、`generate_prefixed_id("ows")`。

- [ ] **Step 1: 写往返测试（失败优先）** — create→list→get→update(改名)→delete，断言 id `starts_with("ows_")`，update 后字段变化，delete 后 get 为 None。
- [ ] **Step 2: 跑确认失败** — `cargo nextest run -p nomifun-db orch_workspace`。
- [ ] **Step 3: 写 trait + sqlite impl**（仿 Task 2）。
- [ ] **Step 4: 接 mod.rs**（4 行 re-export）。
- [ ] **Step 5: 跑确认通过**。
- [ ] **Step 6: 提交** — `git commit -m "feat(orchestrator): OrchWorkspace 仓库"`

---

## Task 4: api-types DTO

**Files:**
- Create: `crates/backend/nomifun-api-types/src/orchestrator.rs`
- Modify: `crates/backend/nomifun-api-types/src/lib.rs`
- Test: 内联 serde round-trip

**Interfaces:**
- Produces（DTO，`#[derive(Debug, Clone, Serialize, Deserialize)]`，Request 体可只 Deserialize）：
```rust
// 响应 DTO
pub struct Fleet { pub id: String, pub name: String, pub description: Option<String>, pub max_parallel: Option<i64>, pub members: Vec<FleetMember>, pub created_at: i64, pub updated_at: i64 }
pub struct FleetMember { pub id: String, pub agent_id: String, pub provider_id: Option<String>, pub model: Option<String>, pub role_hint: Option<String>, pub capability_profile: Option<CapabilityProfile>, pub constraints: Option<MemberConstraints>, pub sort_order: i64 }
pub struct CapabilityProfile { pub strengths: Vec<String>, pub modalities: Vec<String>, pub tools: bool, pub reasoning: String, pub cost_tier: String, pub speed_tier: String }
pub struct MemberConstraints { pub max_concurrency: Option<i64>, pub cost_tier: Option<String>, pub allowed_task_kinds: Option<Vec<String>> }
pub struct OrchWorkspace { pub id: String, pub name: String, pub default_fleet_id: Option<String>, pub workspace_dir: Option<String>, pub created_at: i64, pub updated_at: i64 }
// 请求 DTO
pub struct CreateFleetRequest { pub name: String, pub description: Option<String>, pub max_parallel: Option<i64>, pub members: Vec<FleetMemberInput> }
pub struct UpdateFleetRequest { pub name: Option<String>, pub description: Option<Option<String>>, pub max_parallel: Option<Option<i64>>, pub members: Option<Vec<FleetMemberInput>> }
pub struct FleetMemberInput { pub agent_id: String, pub provider_id: Option<String>, pub model: Option<String>, pub role_hint: Option<String>, pub capability_profile: Option<CapabilityProfile>, pub constraints: Option<MemberConstraints>, pub sort_order: Option<i64> }
pub struct CreateWorkspaceRequest { pub name: String, pub default_fleet_id: Option<String>, pub workspace_dir: Option<String> }
pub struct UpdateWorkspaceRequest { pub name: Option<String>, pub default_fleet_id: Option<Option<String>> }
```
- Consumes: `serde`；`Option<Option<T>>` patch 字段照搬 `nomifun-api-types` 既有 `double_option` deserializer（见模板文件，role: "double_option"）。

参照模板：`crates/backend/nomifun-api-types/src/webhook.rs`（或 conversation DTO）。

- [ ] **Step 1: 写 serde round-trip 测试（失败优先）** — 序列化 `CreateFleetRequest`（含一个 member、capability_profile）→ 反序列化回来字段一致；`UpdateFleetRequest` 的 `description: Some(None)`（清空）vs 缺省（保留）经 double_option 正确区分。
- [ ] **Step 2: 跑确认失败** — `cargo nextest run -p nomifun-api-types orchestrator`。
- [ ] **Step 3: 写 DTO + double_option** — 在 `orchestrator.rs` 定义全部结构；patch 字段用 `#[serde(default, deserialize_with = "double_option")]`。
- [ ] **Step 4: 接 lib.rs** — `pub mod orchestrator;` + `pub use orchestrator::*;`（对照现有 re-export 风格）。
- [ ] **Step 5: 跑确认通过**。
- [ ] **Step 6: 提交** — `git commit -m "feat(orchestrator): api-types DTO"`

---

## Task 5: nomifun-orchestrator crate + FleetService

**Files:**
- Create: `crates/backend/nomifun-orchestrator/Cargo.toml`、`src/lib.rs`、`src/error.rs`、`src/state.rs`、`src/service.rs`
- Modify: 根 `Cargo.toml`（workspace member + `[workspace.dependencies]` 条目）
- Test: `src/service.rs` 内联

**Interfaces:**
- Consumes: `IFleetRepository`（Task 2）、api-types DTO（Task 4）、`AppError`。
- Produces:
```rust
#[derive(Clone)]
pub struct FleetService { fleet_repo: Arc<dyn IFleetRepository> }
impl FleetService {
    pub fn new(fleet_repo: Arc<dyn IFleetRepository>) -> Self;
    pub async fn list(&self, user_id: &str) -> Result<Vec<Fleet>, AppError>;
    pub async fn get(&self, id: &str) -> Result<Fleet, AppError>;          // not found → AppError::NotFound
    pub async fn create(&self, user_id: &str, req: CreateFleetRequest) -> Result<Fleet, AppError>;
    pub async fn update(&self, id: &str, req: UpdateFleetRequest) -> Result<Fleet, AppError>;
    pub async fn delete(&self, id: &str) -> Result<(), AppError>;
}
pub enum OrchestratorError { /* thiserror; 映射到 AppError */ }
```
- service 负责 Row↔DTO 映射 + JSON 编解码（`capability_profile`/`constraints` 字符串 ↔ 结构，fail-soft：解析失败记 warn 返 None，仿 team `decode_tags`）；`create` 校验 `name` 非空、`members` 至少 1 个；写成员经 `replace_members`。

参照模板：`crates/backend/nomifun-webhook/src/{service.rs,error.rs,state.rs,lib.rs}` + `Cargo.toml`。

- [ ] **Step 1: 建 Cargo.toml + 根 workspace 接线**

`crates/backend/nomifun-orchestrator/Cargo.toml`（仿 webhook，依赖 common/db/api-types/auth + axum/tokio/serde_json/thiserror/tracing/async-trait；dev-dep tokio test-util + sqlx）。根 `Cargo.toml`：`members` 数组加 `"crates/backend/nomifun-orchestrator"`（注：成员是 glob `crates/backend/*` 则自动包含，确认后决定是否需手加）；`[workspace.dependencies]` 加 `nomifun-orchestrator = { path = "crates/backend/nomifun-orchestrator" }`。

- [ ] **Step 2: 写 FleetService 测试（失败优先）**

`src/service.rs` 内：用 `SqliteFleetRepository` over `init_database_memory()` 构造 service；测 `create`（含 1 成员）→ 返回 Fleet 带 members 且 capability_profile 解析为结构；`get` 未知 id → `AppError::NotFound`；`create` name 空 → `AppError::BadRequest`；`update` 改名 + 替换成员；`delete` 后 `list` 为空。
```rust
#[tokio::test]
async fn fleet_service_create_get_update_delete() { /* 如上断言 */ }
```

- [ ] **Step 3: 跑确认失败** — `cargo nextest run -p nomifun-orchestrator fleet_service`。
- [ ] **Step 4: 写 error.rs / lib.rs / state.rs / service.rs（FleetService）** — `lib.rs` 声明模块 + re-export `OrchestratorRouterState`、`orchestrator_routes`（routes 在 Task 7，先留 `pub mod routes;` 占位或本任务暂不导出 routes）。本任务先实现 error + FleetService + state 壳。
- [ ] **Step 5: 跑确认通过 + 全 crate 编译** — `cargo nextest run -p nomifun-orchestrator` + `cargo build -p nomifun-orchestrator`。
- [ ] **Step 6: 提交** — `git commit -m "feat(orchestrator): crate 骨架 + FleetService"`

---

## Task 6: WorkspaceService

**Files:**
- Modify: `crates/backend/nomifun-orchestrator/src/service.rs`（加 `WorkspaceService`）
- Test: 内联

**Interfaces:**
- Consumes: `IOrchWorkspaceRepository`（Task 3）、`OrchWorkspace` DTO。
- Produces:
```rust
#[derive(Clone)]
pub struct WorkspaceService { ws_repo: Arc<dyn IOrchWorkspaceRepository> }
impl WorkspaceService {
    pub fn new(ws_repo: Arc<dyn IOrchWorkspaceRepository>) -> Self;
    pub async fn list(&self, user_id: &str) -> Result<Vec<OrchWorkspace>, AppError>;
    pub async fn get(&self, id: &str) -> Result<OrchWorkspace, AppError>;
    pub async fn create(&self, user_id: &str, req: CreateWorkspaceRequest) -> Result<OrchWorkspace, AppError>;
    pub async fn update(&self, id: &str, req: UpdateWorkspaceRequest) -> Result<OrchWorkspace, AppError>;
    pub async fn delete(&self, id: &str) -> Result<(), AppError>;
}
```

- [ ] **Step 1: 写测试（失败优先）** — create→get→update(改名/换默认编队)→delete；name 空 → BadRequest。
- [ ] **Step 2: 跑确认失败** — `cargo nextest run -p nomifun-orchestrator workspace_service`。
- [ ] **Step 3: 实现 WorkspaceService** + Row↔DTO 映射。
- [ ] **Step 4: 跑确认通过**。
- [ ] **Step 5: 提交** — `git commit -m "feat(orchestrator): WorkspaceService"`

---

## Task 7: routes.rs + RouterState

**Files:**
- Create: `crates/backend/nomifun-orchestrator/src/routes.rs`
- Modify: `crates/backend/nomifun-orchestrator/src/state.rs`（`OrchestratorRouterState { fleet: FleetService, workspace: WorkspaceService }`）、`src/lib.rs`（re-export `orchestrator_routes`、`OrchestratorRouterState`）
- Test: 内联 router-builds smoke

**Interfaces:**
- Consumes: `FleetService`、`WorkspaceService`、`CurrentUser`、`ApiResponse`、`AppError`。
- Produces: `pub fn orchestrator_routes(state: OrchestratorRouterState) -> axum::Router`，挂：
  - `GET/POST /api/orchestrator/fleets`、`GET/PUT/DELETE /api/orchestrator/fleets/{id}`
  - `GET/POST /api/orchestrator/workspaces`、`GET/PUT/DELETE /api/orchestrator/workspaces/{id}`
  - handler 用 `Extension(user): Extension<CurrentUser>` 取 `user.id`（注意 §public-route 不要在公开路由 extract CurrentUser——本组路由在 auth 中间件下，可安全 extract）。

参照模板：`crates/backend/nomifun-webhook/src/routes.rs`（handler 形状、JsonRejection→BadRequest、ApiResponse 包装、201 CREATED）。

- [ ] **Step 1: 写 router-builds 测试（失败优先）** — 构造 `OrchestratorRouterState`（两个 service over in-memory repos），`orchestrator_routes(state)` 不 panic；可选 `tower::ServiceExt::oneshot` 打一个 `GET /api/orchestrator/fleets` 断 200。
- [ ] **Step 2: 跑确认失败** — `cargo nextest run -p nomifun-orchestrator routes`。
- [ ] **Step 3: 写 state.rs + routes.rs**（仿 webhook，user.id 传入 service）。
- [ ] **Step 4: lib.rs re-export** `orchestrator_routes`、`OrchestratorRouterState`。
- [ ] **Step 5: 跑确认通过**。
- [ ] **Step 6: 提交** — `git commit -m "feat(orchestrator): CRUD 路由 + RouterState"`

---

## Task 8: app 接线（编译 + HTTP 集成测试）

**Files:**
- Modify: `crates/backend/nomifun-app/src/router/state.rs`（`use nomifun_orchestrator::OrchestratorRouterState;`；`ModuleStates` 加 `pub orchestrator: OrchestratorRouterState,`；`build_orchestrator_state(services) -> OrchestratorRouterState`；`build_module_states` 加 `orchestrator: build_orchestrator_state(services),`）
- Modify: `crates/backend/nomifun-app/src/router/routes.rs`（`use nomifun_orchestrator::orchestrator_routes;`；`create_router_with_states` 加 `let orchestrator_authenticated = orchestrator_routes(states.orchestrator.clone()).route_layer(from_fn_with_state(auth_mw_state.clone(), auth_middleware));` + `.merge(orchestrator_authenticated)`）
- Modify: `crates/backend/nomifun-app/Cargo.toml`（`nomifun-orchestrator.workspace = true`）
- Test: `crates/backend/nomifun-app/tests/`（新 `orchestrator_e2e.rs` 或加入既有集成测试）

**Interfaces:**
- Consumes: `build_orchestrator_state` 内 `services.database.pool().clone()` → `SqliteFleetRepository`/`SqliteOrchWorkspaceRepository`（`Arc<dyn ...>`）→ `FleetService::new` / `WorkspaceService::new` → `OrchestratorRouterState`。P0 **不需** AppServices 单例字段（无 agent 工厂依赖）；全部在 `build_orchestrator_state` 内构造。
- Produces: 装好的 `/api/orchestrator/*` 路由（auth 后）。

参照模板：`crates/backend/nomifun-app/src/router/state.rs` 的 `build_webhook_state`；`routes.rs` 的 webhook merge 行；现有集成测试如何起 router + 注入测试 auth。

- [ ] **Step 1: 写 HTTP 集成测试（失败优先）** — 起测试 router（仿现有 app 集成测试的 helper / `create_router_with_all_state`），带测试用户认证，`POST /api/orchestrator/fleets`（body 含 1 成员）→ 201 + 返回带 id；`GET .../fleets` → 含该 fleet；`PUT` 改名 → 200；`DELETE` → 204/200；再 `GET /{id}` → 404。
- [ ] **Step 2: 跑确认失败** — `cargo nextest run -p nomifun-app orchestrator`。
- [ ] **Step 3: 写 build_orchestrator_state + ModuleStates 字段 + use（state.rs）**。
- [ ] **Step 4: 写 routes.rs merge + use + Cargo.toml dep**。
- [ ] **Step 5: 全量后端编译** — `cargo build -p nomifun-app`（关键闸：app 必须编过）。
- [ ] **Step 6: 跑集成测试确认通过** — `cargo nextest run -p nomifun-app orchestrator`。
- [ ] **Step 7: 提交** — `git commit -m "feat(orchestrator): app 接线 + HTTP 集成测试"`

---

## Task 9: 前端 ipcBridge 客户端 + TS 类型

**Files:**
- Create: `ui/src/common/types/orchestrator/orchestratorTypes.ts`（手写镜像 DTO：`TFleet`/`TFleetMember`/`TCapabilityProfile`/`TMemberConstraints`/`TOrchWorkspace` + Create/Update payload）
- Modify: `ui/src/common/adapter/ipcBridge.ts`（加 `orchestrator = {...}` 块）
- Test: typecheck

**Interfaces:**
- Consumes: 现有 `httpGet/httpPost/httpPut/httpDelete` helper（对照 ipcBridge 既有域块）。
- Produces:
```ts
orchestrator = {
  fleets: {
    list: () => Promise<TFleet[]>,
    get: (id: string) => Promise<TFleet>,
    create: (body: TCreateFleet) => Promise<TFleet>,
    update: (id: string, body: TUpdateFleet) => Promise<TFleet>,
    remove: (id: string) => Promise<void>,
  },
  workspaces: {
    list / get / create / update / remove (同形)
  },
}
```
> 数字字段（max_parallel/sort_order/created_at）确保 TS 为 `number`（DTO 是 i64，但走 JSON 是 number；本任务手写类型直接用 `number`）。

参照模板：ipcBridge.ts 既有 `team = {...}` 或 `cron`/`webhook` 块。

- [ ] **Step 1: 写 TS 类型镜像** `orchestratorTypes.ts`（对齐 Task 4 DTO 字段名，蛇形→保持后端字段名 `snake_case` 与 wire 一致）。
- [ ] **Step 2: 写 ipcBridge.orchestrator 块**（REST 方法）。
- [ ] **Step 3: typecheck** — `cd ui && npm run typecheck` → 0 error。
- [ ] **Step 4: 提交** — `git commit -m "feat(orchestrator): 前端 ipcBridge + 类型"`

---

## Task 10: 侧栏 entry + 路由 + i18n

**Files:**
- Create: `ui/src/renderer/components/layout/Sider/SiderNav/SiderOrchestratorEntry.tsx`
- Modify: `ui/src/renderer/components/layout/Sider/SiderNav/index.ts`
- Modify: `ui/src/renderer/components/layout/Sider/index.tsx`
- Modify: `ui/src/renderer/components/layout/Router.tsx`
- Create: `ui/src/renderer/services/i18n/locales/zh-CN/orchestrator.json` + `en-US/orchestrator.json`
- Modify: `locales/zh-CN/common.json` + `en-US/common.json`（nav label）+ `i18n-keys.d.ts`
- Create（占位）: `ui/src/renderer/pages/orchestrator/index.tsx`（先渲染一个标题占位，Task 11 充实）
- Test: typecheck + 真机（tab 出现并路由）

**Interfaces:**
- Consumes: `SiderModelHubEntry.tsx`（复制模板）、`withRouteFallback`、`React.lazy`、`common.siderNav.orchestrator` i18n key。
- Produces: `/orchestrator` 路由 + 常用组「会话」下方的 entry。

参照模板：`SiderModelHubEntry.tsx`（entry 形状：collapsed icon-only Tooltip + expanded icon+label，active `!bg-primary-1 !text-primary-6`）；`Router.tsx` 的 lazy 路由注册；`Sider/index.tsx` 常用组 JSX。

- [ ] **Step 1: 占位页** `pages/orchestrator/index.tsx`：
```tsx
export default function OrchestratorPage() {
  return <div className="p-24 text-t-primary">智能编排（建设中）</div>;
}
```
- [ ] **Step 2: SiderOrchestratorEntry.tsx** — 复制 `SiderModelHubEntry.tsx`，改 icon（`@icon-park/react` 选一个语义贴切的，如 `Workbench` / `Connection`，**不起别名**），i18n key 换 `common.siderNav.orchestrator`，target `/orchestrator`。
- [ ] **Step 3: 接 SiderNav/index.ts** — `export { default as SiderOrchestratorEntry } from './SiderOrchestratorEntry';`
- [ ] **Step 4: 插入 Sider/index.tsx 常用组** — 在 `SiderConversationEntry` 之后、`SiderNomiEntry` 之前渲染 `<SiderOrchestratorEntry .../>`；加 navTo `/orchestrator` 与 isActive（`pathname.startsWith('/orchestrator')`）。
- [ ] **Step 5: Router.tsx 路由** — `const OrchestratorPage = React.lazy(() => import('@/renderer/pages/orchestrator'));` + 在 ProtectedLayout 下加 `<Route path="/orchestrator" element={withRouteFallback(OrchestratorPage)} />`。
- [ ] **Step 6: i18n** — `common.json` 双语加 `siderNav.orchestrator`（中「智能编排」/英「Orchestration」）；建 `orchestrator.json` 双语（先放页面用到的 key 占位）；`bun run gen:i18n`（仓库根）；同步 `i18n-keys.d.ts`。
- [ ] **Step 7: typecheck** — `cd ui && npm run typecheck` → 0。
- [ ] **Step 8: 提交** — `git commit -m "feat(orchestrator): 侧栏 tab + 路由 + i18n"`

---

## Task 11: 编排页壳（ContentSider 三段）

**Files:**
- Modify: `ui/src/renderer/pages/orchestrator/index.tsx`（充实为 ContentSider + 主区，`?section=` 切 workspace/fleet/run-history）
- Create: `ui/src/renderer/pages/orchestrator/WorkspaceList.tsx`、`RunHistory.tsx`（后者 P0 占位空态）
- Create: `ui/src/renderer/pages/orchestrator/useOrchestratorData.ts`（SWR hooks：`useFleets()`、`useWorkspaces()`）
- Test: typecheck

**Interfaces:**
- Consumes: `ContentSider`、`useResizableSplit`（独立 storageKey `nomifun:orchestrator-sider-width`）、`useLayoutContext`（isMobile→SegmentedTabs）、`useSearchParams`、`ipcBridge.orchestrator`（Task 9）、`useSWR`。
- Produces: 三段导航（工作间 / 编队 / Run 历史），主区按 `?section=` 渲染对应子视图；默认 `fleet`。

参照模板：`ui/src/renderer/pages/modelHub/index.tsx`（ContentSider + `?section=` 模式）；`ContentSider/index.tsx`；`useResizableSplit.ts`。

- [ ] **Step 1: useOrchestratorData.ts** — `useFleets`/`useWorkspaces`（SWR key `'orchestrator.fleets'`/`'orchestrator.workspaces'`，fetcher = ipcBridge）。
- [ ] **Step 2: WorkspaceList.tsx** — 列出工作间（卡/行）+ 「新建工作间」按钮（弹 NomiModal，name + 默认编队选择 → `ipcBridge.orchestrator.workspaces.create` → `mutate`）。
- [ ] **Step 3: RunHistory.tsx** — 空态卡：「Run 执行即将上线（P1）」。
- [ ] **Step 4: index.tsx 页壳** — ContentSider 三段（icon+label，`?section=` 内联态，移动端 SegmentedTabs）；主区 switch；FleetManager 由 Task 12 提供（先 import 占位）。
- [ ] **Step 5: typecheck** → 0。
- [ ] **Step 6: 提交** — `git commit -m "feat(orchestrator): 页壳 ContentSider 三段 + 工作间列表"`

---

## Task 12: 编队管理 UI（卡片网格 + 编辑抽屉）

**Files:**
- Create: `ui/src/renderer/pages/orchestrator/FleetManager.tsx`、`FleetCard.tsx`、`FleetEditDrawer.tsx`、`FleetMemberRow.tsx`
- Modify: `pages/orchestrator/index.tsx`（接入 FleetManager）
- Test: typecheck + 真机（创建/编辑/删除编队）

**Interfaces:**
- Consumes: `useFleets`（Task 11）、`ipcBridge.orchestrator.fleets`、`ipcBridge.acpConversation.getAvailableAgents`（`/api/agents`，选 agent）、模型/provider 列表（对照模型管理页取 providers + models 的现有 hook/ipc）、`NomiModal`/Arco `Drawer`、`AssistantTagFilterBar`（chip 筛选风格）、`useArcoMessage`。
- Produces: 编队卡片网格（每卡显示编队名 + 成员头像/模型 chip + 成员数 + max_parallel）；新建/编辑抽屉（编队名、描述、并发上限 + 成员编辑器：每行 = 选 agent + provider+model + 角色提示 + 强项标签 + 约束）。删除二次确认。

参照模板：`pages/settings/AgentSettings/AgentCard.tsx` / `LocalAgents`（卡片网格 + agent 选择）；`AssistantTagFilterBar.tsx`（chip）；`AssistantEditDrawer`（编辑抽屉范式）；模型选择对照 `NomiModelSelector` / 模型管理页取 providers。

- [ ] **Step 1: FleetMemberRow.tsx** — 单成员编辑行（agent 下拉来自 `/api/agents` 已启用项；provider+model 下拉来自 providers；role_hint 输入；强项标签多选；约束 max_concurrency/cost_tier）。用 `<div onClick>` 不用裸 `<button>`。
- [ ] **Step 2: FleetEditDrawer.tsx** — Arco Drawer：编队名/描述/max_parallel + 成员列表（增删行，复用 FleetMemberRow）；保存 → `create`/`update` → `mutate('orchestrator.fleets')` + `useArcoMessage` 成功提示；校验 name 非空、至少 1 成员。
- [ ] **Step 3: FleetCard.tsx** — 卡片：编队名、成员 avatar+model chip、max_parallel badge；点编辑 → 开抽屉；右上菜单删除（二次确认）。颜色全用 CSS 变量。
- [ ] **Step 4: FleetManager.tsx** — `auto-fill minmax(min(320px,100%),1fr)` 卡片网格（**必用** `min(...)` 防窄面板裁切）+ 「新建编队」入口 + 空态。
- [ ] **Step 5: 接 index.tsx** — `section==='fleet'` 渲染 `<FleetManager/>`。
- [ ] **Step 6: typecheck** → 0。
- [ ] **Step 7: 真机验证** — 起 app（`bun run dev:web`，设 `NOMIFUN_DATA_DIR` 避开桌面锁；或桌面 dev），进「智能编排」→ 编队 → 新建编队（加 2 个不同 agent+model 成员）→ 保存 → 卡片出现 → 编辑改名 → 删除。截图留证。
- [ ] **Step 8: 提交** — `git commit -m "feat(orchestrator): 编队管理 UI(卡片网格+编辑抽屉)"`

---

## Self-Review（对照 spec 的 P0 切片）

**1. Spec 覆盖：**
- §3 概念模型（Fleet/Workspace/Run/RunTask/Dep/Assignment/Member）→ Task 1 全表 + Task 4 DTO（Run/Task 表本期仅建表建模，CRUD 服务/UI 在 P1+）。✔
- §5 数据模型 8 表（注：spec §5 列 7 个 CREATE TABLE：fleets/fleet_members/orch_workspaces/orch_runs/orch_run_tasks/orch_run_task_deps/orch_assignments）→ Task 1 全建。✔
- §9 前端：侧栏 tab（常用组会话下）→ Task 10；ContentSider 三段 → Task 11；编队管理 → Task 12。✔（画布/节点转录 = P2，本期不做）
- §10 接线 6-8 点 → Task 8（P0 无 GatewayDeps，那是 P4）。✔
- §12 测试：Rust 单测（Task 1-8 各自）+ HTTP 集成（Task 8）+ 前端 typecheck（Task 9-12）+ 真机（Task 12 Step 7）。✔

**2. 占位符扫描：** 无 TBD/TODO/「类似 Task N」；每个代码步给出真实签名/SQL/测试；样板步骤指向确切模板文件路径。RunHistory 的「占位空态」是有意的 P0 范围边界（已注明 P1 充实），非计划缺陷。✔

**3. 类型一致性：** `IFleetRepository.replace_members` 在 Task 2 定义、Task 5 FleetService 消费；`generate_prefixed_id` 前缀 `fleet_`/`fmem_`/`ows_` 全程一致；DTO 字段名（Task 4）与 Row（Task 1）与 TS（Task 9）三处对齐（snake_case wire）；`OrchestratorRouterState { fleet, workspace }`（Task 7）与 `build_orchestrator_state`（Task 8）一致。✔

**P0 交付物：** 真实 app 内可创建/编辑/删除编队与工作间；后端全编译 + 集成测试绿 + 前端 typecheck 0 + 真机截图。为 P1（Run 引擎）铺好 schema 与服务壳。

---

## Execution Handoff

P0 计划完成。任务依赖与并行波次：
- **Wave A（并行）**：Task 1（db 迁移/模型）‖ Task 4（DTO）‖ Task 10（侧栏/路由，纯前端）
- **Wave B（并行）**：Task 2 + Task 3（仓库，依赖 1）‖ Task 9（ipcBridge，依赖 4）
- **Wave C（并行）**：Task 5（依赖 2+4）；Task 11（页壳，依赖 10）
- **Wave D**：Task 6（依赖 3）→ Task 7（依赖 5+6）→ Task 8（依赖 7）
- **Wave E**：Task 12（依赖 9+11+8 的 API）

执行用 **subagent-driven-development**：每任务一个 fresh subagent + 两阶段评审（实现评审 + 质量评审），独立文件域的任务并行派遣。
