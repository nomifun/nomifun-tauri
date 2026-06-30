# P1 — 会话式 Run 创建（体验主干）实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development。Steps `- [ ]`。

**Goal:** 用户在会话入口选「自动/范围」模型、写需求、提交 → 落地一个**主管会话**（nomi + 主管系统提示 + caps_orchestrator 工具 + model_range），主管可 `nomi_run_create` 从 model_range 直接拆出多 agent run，**无需预建 workspace/fleet**。引擎整套复用。

**Architecture:** 新增 `RunService::create_adhoc`（从 model_range 就地构造成员快照），caps_orchestrator 的 `nomi_run_create` 改为从调用会话上下文取参；后端给 nomi 加 `orchestrator_role` 识别并注入主管提示；前端模型选择器三态 + useGuidSend 注入 lead 标记。迁移 020 给 `orch_runs` 加 `work_dir` 并使 `workspace_id` 可空，引擎取目录优先 `work_dir`。

**Spec:** `docs/superpowers/specs/2026-06-27-conversation-native-orchestration-redesign.md` §3、§4、§9.1、§11、§12。

## Global Constraints
- 引擎复用契约（§11/§12 不变量）：`RunEngine`/`ConversationWorkerRunner`/`LlmPlanProducer`/`Router`/run 生命周期/IDMM/worker 主侧栏过滤 **零改或仅最小适配**；唯一引擎适配点=`run_loop` 取工作目录处。
- 迁移 append-only，编号 **020**（019=drop_team）。实施前 Read 018+019 确认表重建/FK/事务约定。
- 后端禁 `cargo fmt`；只跑触碰 crate 的 nextest；`nomifun-app` 必须编过。
- 前端 typecheck=0（`cd ui && npm run typecheck`，**不是** `npx tsc`）；`bun run build` 绿；禁 `any`/`ts-ignore`；Arco 弹窗用 `useArcoMessage`；icon-park 具名导入禁别名；`<div onClick>` 不用 `<button>`。
- **禁合并 main**；分支 `feat/multi-agent-orchestrator`。
- 品牌 NomiFun；新增/改 UI 必须漂亮、对齐既有视觉语言。
- 设备边界 id：跨设备字符串前缀；本机 `conversation_id` 用 INTEGER；ts-rs i64 用 `#[ts(type="number")]`。

## File Structure
- 迁移：`crates/backend/nomifun-db/migrations/020_orch_run_work_dir.sql`
- DB：`nomifun-db/src/models/orchestrator.rs`（`OrchRunRow.workspace_id` → `Option<String>` + `work_dir: Option<String>`）、`repository/orch_run.rs`（`CreateRunParams` 加字段）、`repository/sqlite_orch_run.rs`（INSERT/SELECT/map）
- api-types：`nomifun-api-types/src/orchestrator.rs`（`ModelRange`、`CreateAdhocRunRequest`、`Run.workspace_id` → `Option<String>`）
- orchestrator：`run_service.rs`（`create_adhoc`）、`engine.rs`（work_dir 适配）
- gateway：`nomifun-gateway/src/caps_orchestrator.rs`（`nomi_run_create` 改签名）
- ai-agent：`nomifun-api-types/src/agent_build_extra.rs`（`NomiBuildExtra.orchestrator_role`）、`nomifun-ai-agent/src/factory/nomi.rs`（lead 主管提示注入）
- 前端：`ui/src/renderer/pages/guid/hooks/useGuidModelSelection.ts`、`components/GuidModelSelector.tsx`、`GuidPage.tsx`、`hooks/useGuidSend.ts`、`ui/src/common/adapter/ipcBridge.ts`（`ICreateConversationParams.extra` 加字段）

---

## Task 1: 迁移 020 + DB 层（orch_runs.work_dir + workspace_id 可空）

**Files:** Create `migrations/020_orch_run_work_dir.sql`；Modify `models/orchestrator.rs`、`repository/orch_run.rs`、`repository/sqlite_orch_run.rs`；测试 `nomifun-db`。

**已知 018 DDL（实施前再 Read 核对）：** `orch_runs(id TEXT PK, workspace_id TEXT NOT NULL REFERENCES orch_workspaces(id) ON DELETE CASCADE, user_id, goal, fleet_snapshot, autonomy, max_parallel, lead_conv_id INTEGER, status, summary, total_tokens, forked_from, created_at, updated_at)` + `CREATE INDEX idx_orch_runs_workspace ON orch_runs(workspace_id)`。

**迁移内容（表重建使 workspace_id 可空 + 加 work_dir）：**
```sql
-- 020_orch_run_work_dir.sql
-- 让 orch_runs 脱离强制 workspace：workspace_id 可空 + 新增 work_dir(运行工作目录)。
-- SQLite 不能 drop NOT NULL，走标准表重建。参考 019 的事务/FK 约定。
PRAGMA foreign_keys=OFF;
CREATE TABLE orch_runs_new (
  id              TEXT PRIMARY KEY,
  workspace_id    TEXT REFERENCES orch_workspaces(id) ON DELETE CASCADE,  -- 可空
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
  work_dir        TEXT,
  created_at      INTEGER NOT NULL,
  updated_at      INTEGER NOT NULL
);
INSERT INTO orch_runs_new
  (id, workspace_id, user_id, goal, fleet_snapshot, autonomy, max_parallel,
   lead_conv_id, status, summary, total_tokens, forked_from, work_dir, created_at, updated_at)
SELECT
   id, workspace_id, user_id, goal, fleet_snapshot, autonomy, max_parallel,
   lead_conv_id, status, summary, total_tokens, forked_from, NULL, created_at, updated_at
FROM orch_runs;
DROP TABLE orch_runs;
ALTER TABLE orch_runs_new RENAME TO orch_runs;
CREATE INDEX idx_orch_runs_workspace ON orch_runs(workspace_id);
PRAGMA foreign_keys=ON;
```
> 若 019/迁移运行器已在关闭 FK 的事务里跑，去掉 PRAGMA 行（按既有约定）。实施时核对。

**Struct/repo 改动：**
- `OrchRunRow`：`workspace_id: Option<String>`、新增 `work_dir: Option<String>`。
- `CreateRunParams`：`workspace_id: Option<String>`、新增 `work_dir: Option<String>`、新增 `lead_conv_id: Option<i64>`（今天 INSERT 恒 NULL，create_adhoc 需要写）。
- `sqlite_orch_run.rs::create_run`：INSERT 列加 `work_dir`，`lead_conv_id` 改为绑 `p.lead_conv_id`（不再硬编 NULL）；所有 `SELECT` orch_runs 的列表加 `work_dir`；row 映射补 `work_dir`、`workspace_id` 按可空读取。

- [ ] **Step 1: 写迁移测试（失败优先）** — `nomifun-db` 迁移测试：跑全部迁移后，`PRAGMA table_info(orch_runs)` 含 `work_dir` 且 `workspace_id` 的 `notnull==0`；旧行（若测试种入一行 workspace_id 非空）保留。
- [ ] **Step 2: RED** `cargo nextest run -p nomifun-db`（新测试失败/编译失败）。
- [ ] **Step 3: 实现** 迁移 + struct/repo 改动；全 crate 编过。
- [ ] **Step 4: GREEN** `cargo nextest run -p nomifun-db` + `cargo build -p nomifun-db`。
- [ ] **Step 5: 提交** `git commit -m "feat(orchestrator): 迁移020 orch_runs.work_dir + workspace_id 可空 + repo 适配"`

---

## Task 2: api-types DTO + RunService::create_adhoc + 引擎 work_dir 适配

**Files:** Modify `nomifun-api-types/src/orchestrator.rs`、`nomifun-orchestrator/src/run_service.rs`、`engine.rs`；测试内联。

**DTO（api-types）：**
```rust
// orchestrator.rs
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ModelRange {
    Single { model: ModelRef },
    Auto,
    Range { models: Vec<ModelRef> },
}
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct ModelRef { pub provider_id: String, pub model: String }

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct CreateAdhocRunRequest {
    pub goal: String,
    pub work_dir: Option<String>,
    pub model_range: ModelRange,
    #[serde(default)] pub pinned_roles: Vec<String>,   // P4 用；P1 解析但允许空
    #[serde(default)] pub autonomy: Option<String>,
    #[serde(default)] pub max_parallel: Option<i64>,
    #[serde(default)] pub lead_conv_id: Option<i64>,    // #[ts(type="number")]
}
```
- `Run` DTO 的 `workspace_id` 改 `Option<String>`（核对前端消费处，P1 前端不读它）。

**RunService::create_adhoc（参照现有 `create`，run_service.rs:84-125）：**
```rust
pub async fn create_adhoc(&self, user_id: &str, req: CreateAdhocRunRequest) -> Result<Run, AppError> {
    if req.goal.trim().is_empty() { return Err(OrchestratorError::BadRequest("goal must not be empty".into()).into()); }
    let members = build_members_from_range(&req.model_range, /*pinned_roles 解析留 P4，P1 忽略*/);
    if members.is_empty() { return Err(OrchestratorError::BadRequest("model_range 为空：无可用模型".into()).into()); }
    let fleet_snapshot = serde_json::to_string(&members).unwrap_or_else(|_| "[]".into());
    let autonomy = req.autonomy.filter(|a|!a.trim().is_empty()).unwrap_or_else(|| DEFAULT_AUTONOMY.to_string());
    let row = self.run_repo.create_run(CreateRunParams {
        workspace_id: None, work_dir: req.work_dir, lead_conv_id: req.lead_conv_id,
        user_id: user_id.to_string(), goal: req.goal, fleet_snapshot, autonomy, max_parallel: req.max_parallel,
    }).await.map_err(OrchestratorError::from)?;
    let run = run_row_to_dto(row);
    self.emitter.emit_run_status(&run.id, &run.status);
    Ok(run)
}
```
`build_members_from_range`：
- `Auto` → 展开所有启用模型为成员（需 provider_repo 列举启用 provider×model；本 P1 可先把 `Auto` 当作"调用方传空范围时回退到 single 主模型"或最简实现：`Auto` 时由调用方在 caps 层解析为 range，service 只认 Single/Range）。**决策：P1 service 只处理 Single/Range；Auto 的展开放在 caps_orchestrator 层（它有 provider 访问）或先标 TODO 由 caller 传 Range。** 见 Task 3。
- 每个 `ModelRef{provider_id, model}` → `FleetMember{ id: generate_prefixed_id("rmbr"), agent_id: String::new(), provider_id: Some(provider_id), model: Some(model), role_hint: None, capability_profile: None, constraints: None, sort_order: i }`。
- worker 硬要求 provider+model 均 Some（worker.rs:116-120）——满足。

**引擎 work_dir 适配（engine.rs:408-416）：** 取 `workspace_dir` 处改为：
```rust
let workspace_dir = if let Some(wd) = run.work_dir.as_deref().map(str::trim).filter(|s|!s.is_empty()) {
    Some(wd.to_string())
} else if let Some(ws_id) = run.workspace_id.as_deref() {
    deps.ws_repo.get(ws_id).await.ok().flatten().and_then(|w| w.workspace_dir)
} else { None };
```
（`Run.work_dir` 字段需经 run_row_to_dto 透传——核对 Run DTO 加 `work_dir: Option<String>`。）

- [ ] **Step 1: 测试（失败优先）** — (a) `create_adhoc` Range 两模型 → run 落库、`fleet_snapshot` 反序列化得 2 个 member（provider+model 均 Some）、`work_dir`/`lead_conv_id` 落库；(b) 空 range → BadRequest；(c) `build_members_from_range` 单测。
- [ ] **Step 2: RED** `cargo nextest run -p nomifun-orchestrator -p nomifun-api-types`。
- [ ] **Step 3: 实现** DTO + create_adhoc + build_members + 引擎 work_dir 适配（ts-rs 导出：`bun run` 端的 ts 类型由 build 生成，后端加 `#[ts]` + i64 `#[ts(type="number")]`）。
- [ ] **Step 4: GREEN** 上述 nextest + `cargo build -p nomifun-orchestrator` + `cargo build -p nomifun-app`（引擎改动不破 e2e：跑 `cargo nextest run -p nomifun-app orchestrator_run_e2e`）。
- [ ] **Step 5: 提交** `git commit -m "feat(orchestrator): create_adhoc 从 model_range 构造快照 + 引擎 work_dir 适配"`

---

## Task 3: caps_orchestrator.nomi_run_create 改签名（从调用会话上下文取参）

**Files:** Modify `nomifun-gateway/src/caps_orchestrator.rs`；测试内联（mock conversation_service.get 返回带 extra 的会话）。

**改动（caps_orchestrator.rs:34-96）：**
- `RunCreateParams` 精简为 `{ goal: String, #[serde(default)] autonomy: Option<String> }`（删 `workspace_id`/`fleet_id`）。
- `create` handler：用 `ctx.conversation_id`/`ctx.user_id`，读调用会话 extra：
```rust
let conv = deps.conversation_service.get(&ctx.user_id, &ctx.conversation_id).await
    .map_err(|e| json!({"error": e.to_string()}))?;  // 注意 handler 返回 Value，用 match 处理
let work_dir = conv.extra.get("workspace").and_then(|v| v.as_str()).map(str::to_string);
let model_range: ModelRange = conv.extra.get("model_range")
    .and_then(|v| serde_json::from_value(v.clone()).ok())
    .ok_or_else(|| ...)?;  // 无 model_range → 该会话不是 lead，报错提示
let lead_conv_id = ctx.conversation_id.parse::<i64>().ok();
let req = CreateAdhocRunRequest { goal: p.goal, work_dir, model_range, pinned_roles: vec![], autonomy: p.autonomy, max_parallel: None, lead_conv_id };
let run = deps.orchestrator_run_service.create_adhoc(user, req).await ...;
// 回写 lead 会话 extra.orchestrator_run_id（供前端 DAG 定位，P2 用）：
//   conversation_service.update extra 合并 {orchestrator_run_id: run.id}（读 update 签名；若无合并 API 则 get→改→set）
// 之后 plan() + engine.start()（与今日一致）
```
- **Auto 展开**：若 `ModelRange::Auto`，在此层用 `deps` 可达的 provider 列举展开为 `Range`（列举启用 provider×可用 model）。若 deps 无 provider_repo，则把 Auto 透传给 service，由 service 用其 provider 访问展开（二选一，实施时按 deps 可达性定；**优先在 caps 层展开**，因为 service 的 provider 访问用于 plan 而非成员构造）。
- `nomi_run_status`/`nomi_run_result` 不变。

- [ ] **Step 1: 测试（失败优先）** — mock `conversation_service.get` 返回 `extra={workspace:"/x", model_range:{mode:"range",models:[...]}}`：`create` → `create_adhoc` 被调且 work_dir/model_range 正确；无 model_range 的会话 → 错误 json。
- [ ] **Step 2: RED** `cargo nextest run -p nomifun-gateway`。
- [ ] **Step 3: 实现** 改签名 + 读 extra + 回写 orchestrator_run_id。
- [ ] **Step 4: GREEN** nextest + `cargo build -p nomifun-gateway -p nomifun-app`。
- [ ] **Step 5: 提交** `git commit -m "feat(orchestrator): nomi_run_create 从调用会话上下文取 work_dir/model_range"`

---

## Task 4: 后端 lead 主管武装（orchestrator_role + 主管系统提示注入）

**Files:** Modify `nomifun-api-types/src/agent_build_extra.rs`（`NomiBuildExtra.orchestrator_role`）、`nomifun-ai-agent/src/factory/nomi.rs`（lead 时注入主管提示）；测试内联。

**改动：**
- `agent_build_extra.rs:219-305`：`NomiBuildExtra` 加 `#[serde(default)] pub orchestrator_role: Option<String>`。
- `factory/nomi.rs`：在组装 `system_prompt`（:291 附近，preset_rules 合并之后）时，若 `overrides.orchestrator_role.as_deref()==Some("lead")`，把**主管系统提示**前置/合并进 `system_prompt`。主管提示常量（中文，server-authored）：
  > 「你是 NomiFun 的编排主管。用户已在本会话限定可用模型范围（见运行上下文）。对简单或单步需求：直接作答。对复杂、可拆分为多个并行/有依赖子任务的需求：调用工具 `nomi_run_create(goal)` 把需求拆成任务 DAG 并行执行（模型范围与工作目录会自动取用），随后用 `nomi_run_status`/`nomi_run_result` 跟进并向用户汇报进展与产出。不要询问 workspace 或 fleet——它们已不存在。」
- 工具可达性：lead 会话在受信桌面会话上已默认获得 desktopGateway（含 caps_orchestrator），无需新增 gateway 代码（见 spec §3.3 勘察）。**不**从客户端标记强制开 gateway（避免非受信会话自授权）；WebUI/非受信编排门控列 carry-forward。

- [ ] **Step 1: 测试（失败优先）** — `NomiBuildExtra` 反序列化 `{"orchestrator_role":"lead"}` → 字段为 `Some("lead")`；factory 单测（若有可测的 system_prompt 组装函数）断言 lead 时 system_prompt 含主管提示片段；非 lead 不含。
- [ ] **Step 2: RED** `cargo nextest run -p nomifun-api-types -p nomifun-ai-agent`。
- [ ] **Step 3: 实现** 字段 + 注入。
- [ ] **Step 4: GREEN** nextest + `cargo build -p nomifun-ai-agent -p nomifun-app`。
- [ ] **Step 5: 提交** `git commit -m "feat(orchestrator): lead 会话注入编排主管系统提示(orchestrator_role)"`

---

## Task 5: 前端模型选择器三态 + ipcBridge extra 字段

**Files:** Modify `ipcBridge.ts`（`ICreateConversationParams.extra`）、`useGuidModelSelection.ts`、`GuidModelSelector.tsx`、`GuidPage.tsx`。

**改动：**
- `ipcBridge.ts:1659-1713` `ICreateConversationParams['extra']` 加：`orchestrator_role?: 'lead'`、`model_range?: { mode:'single'; model:{provider_id:string;model:string} } | { mode:'auto' } | { mode:'range'; models:Array<{provider_id:string;model:string}> }`、`desktopGateway?: boolean`。
- `useGuidModelSelection.ts`：加 `const [selectionMode, setSelectionMode] = useState<'single'|'auto'|'range'>('single')` + `const [selectedRange, setSelectedRange] = useState<TProviderWithModel[]>([])`；加进 `GuidModelSelectionResult` 返回。持久化（可选）到 config。
- `GuidModelSelector.tsx`（isGeminiMode 分支 91-182）：droplist 顶部加 `Radio.Group type='button' size='small'`（单一/自动/范围，对齐 `pages/nomi/index.tsx:167` 模式）；`range` 态把单选 `Menu.Item` 换为多选（`Checkbox.Group` 或 `NomiSelect mode="multiple"` + OptGroup 复用 provider 分组）；`auto` 态隐藏列表+提示"全部启用模型"。触发按钮 label 反映当前态（单一→模型名；自动→"自动编排"；范围→"N 个模型"）。视觉对齐既有 round/small 按钮 + icon-park outline。
- `GuidPage.tsx`：把 `selectionMode`/`selectedRange` 经 props 透传给 `GuidModelSelector`（514-522）与 `useGuidSend`（155-180）。

- [ ] **Step 1: 实现** ipcBridge 类型 + hook 状态 + 选择器三态 UI + GuidPage 透传。
- [ ] **Step 2: typecheck** `cd ui && npm run typecheck` → 0。
- [ ] **Step 3: build** `cd ui && bun run build` 绿。
- [ ] **Step 4: 提交** `git commit -m "feat(orchestrator): 会话模型选择器三态(单一/自动/范围)"`

---

## Task 6: 前端 useGuidSend 注入 lead 标记

**Files:** Modify `useGuidSend.ts`（nomi 分支 269-320 + `GuidSendDeps` 21-76 + 解构 88-123 + deps 数组 403-427）。

**改动：** nomi 分支构造 `extra` 时，若 `selectionMode ∈ {auto, range}`：
- 注入 `orchestrator_role: 'lead'`；
- 注入 `model_range`：`single`→`{mode:'single',model:{provider_id:current_model.id,model:current_model.use_model}}`；`auto`→`{mode:'auto'}`；`range`→`{mode:'range',models:selectedRange.map(m=>({provider_id:m.id,model:m.use_model}))}`；
- `session_mode` 覆盖为 `'yolo'`（替代 `selectedMode`）；
- 仍需有效 `current_model`（range 态用其作主模型/lead 模型；range 为空时 `useArcoMessage` 警告并 return）。
- `single` 态保持今日行为（不注入 lead 标记）。
- 把 `selectionMode`/`selectedRange` 加入 `GuidSendDeps` 与 `handleSend` 依赖数组。

- [ ] **Step 1: 实现** extra 注入 + deps 接线。
- [ ] **Step 2: typecheck** `cd ui && npm run typecheck` → 0。
- [ ] **Step 3: build** `cd ui && bun run build` 绿。
- [ ] **Step 4: 提交** `git commit -m "feat(orchestrator): 自动/范围提交时标记 lead 会话 + model_range"`

---

## Task 7: 集成验证（seam e2e + app 编译 + 真机冒烟）

**Files:** 可能 Modify app `build_orchestrator_state`/gateway deps 接线（确认 `create_adhoc` 经 `orchestrator_run_service` 可达——它是同一 `RunService` 实例，无需新接线，仅确认）。新增/补 e2e。

- [ ] **Step 1:** `cargo build --workspace` 绿；`cargo nextest run -p nomifun-orchestrator -p nomifun-gateway -p nomifun-db -p nomifun-api-types -p nomifun-app` 全绿（既有 orchestrator_run_e2e 3/3 不破）。
- [ ] **Step 2:** 补一条 app/gateway e2e：构造一个带 `extra.model_range`(range,2模型) 的会话 → 调 `nomi_run_create` 工具（或直接 `create_adhoc`）→ 断言 run 创建、fleet_snapshot 含 2 member、status 进入 planning/running（mock planner/worker，沿用既有 e2e 模式）。
- [ ] **Step 3:** 前端 `cd ui && npm run typecheck`(0) + `bun run build`(绿)。
- [ ] **Step 4:** 真机冒烟（controller）：`nomifun-web --dist --insecure-no-auth`，开会话入口 → 模型选择器三态可切换、范围多选可用、UI 漂亮、零 console error（无头截图 target/_p1_smoke）。真·主管 LLM 行为需 provider，留用户。
- [ ] **Step 5: 记账 + 提交**（若有接线改动）`git commit -m "test(orchestrator): P1 seam e2e + 集成验证"`；账本追加 P1 完成行。

---

## Self-Review（spec §3/§4/§9.1/§11/§12）
**覆盖：** 入口三态+lead 标记→T5/T6；create_adhoc 从 model_range→T2；caps 改签名取上下文→T3；主管武装→T4；迁移 work_dir/workspace_id 可空+引擎适配→T1/T2；集成→T7。
**不变量守护：** 引擎唯一适配点=work_dir（T2）；worker provider+model 硬要求由合成成员满足；不破 e2e（T2/T7 跑 orchestrator_run_e2e）。
**占位/风险：** `Auto` 展开位置（caps 层 vs service 层）按 deps 可达性二选一，T3 决策；表重建 FK/事务约定按 019 核对（T1）；`Run.workspace_id`→Option 的前端消费面 P1 不读（核对无破）。
**类型一致：** ModelRange/CreateAdhocRunRequest 后端↔ipcBridge extra 的 model_range 结构一致（single/auto/range 三 variant）。

## Execution Handoff
波次：T1(迁移+DB,sonnet)→T2(DTO+create_adhoc+引擎,opus——keystone)→T3(caps 改签名,opus——上下文读取)→T4(主管武装,sonnet)→T5(前端三态,sonnet)→T6(useGuidSend,sonnet)→T7(集成,opus——controller 验证)。每任务两阶评审+fix+记账。禁合并 main。
