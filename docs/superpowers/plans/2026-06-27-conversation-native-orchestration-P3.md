# P3 — 模型描述 + 三态择优实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development。Steps `- [ ]`。

**Goal:** 给注册模型加用户自填**描述**字段，并让编排规划**按描述在模型范围内择优**：主管(LLM 规划)读到每个候选模型的描述，其 description-informed 选择在通过硬过滤时被采纳。

**Architecture:** `providers.model_descriptions`(JSON model_id→描述)端到端落地；模型中心每模型行加描述编辑；`LlmPlanProducer`(唯一持 provider_repo 的装配路径组件)在 `produce` 查每个成员 `(provider_id,model)` 的描述并写入规划提示；`plan()` 的采纳规则放宽——规划给的 `member_index` 只要**可行(通过 Router 硬过滤)**即采纳(不再限 top-2),Router 排序仅作回退与硬过滤。

**Spec:** §6（模型描述 + 范围内择优）、§12。

## Global Constraints
- 选择权变更（LLM-primary + Router-veto）是有意设计：裸模型成员 capability_profile=None→Router 中性打分无区分力，描述驱动的 LLM 选择才是用户要的"按描述选用"。Router 仍**硬过滤**(vision/tools)+**回退排序**。
- 后端禁 cargo fmt；只跑触碰 crate；app 必编过。前端 typecheck0+build；禁 any/ts-ignore；icon-park 无别名；`<div role=button>`；Arco useArcoMessage。
- 迁移 append-only，编号 **021**（020=work_dir）。
- 不破坏既有 orchestrator_run_e2e（4/4）+ provider 既有测试。**禁合并 main**。UI 必须漂亮。

## File Structure（已勘察 file:line）
- 迁移 `migrations/021_model_descriptions.sql`
- DB：`models/provider.rs`（+字段）、`repository/provider.rs`（CreateProviderParams/UpdateProviderParams +字段）、`repository/sqlite_provider.rs`（INSERT :46-68 / UPDATE :106-128 / create 返回 :78-95 / merge_update :155-186）、测试 fixture `nomifun-gateway/src/tools_provider.rs:317-333`
- 映射：`nomifun-system/src/provider.rs`（create :33-64 / update :67-100 / row_to_response :118-149，用 serialize_opt/deserialize_opt）
- api-types：`nomifun-api-types/src/provider.rs`（ProviderResponse :123-148 / Create :151-181 / Update :190-205）
- 前端类型：`ui/src/common/config/storage.ts:492-552`（IProvider）+ `ui/src/common/types/provider/providerApi.ts`（Create :18-39 / Update :45-59）
- 模型中心 UI：`ui/src/renderer/components/settings/SettingsModal/contents/ModelModalContent.tsx`（每模型行 :492-614 + 删除清理 :580-600）
- 规划：`nomifun-orchestrator/src/plan.rs`（produce :96-121 / build_plan_user_prompt :138-159）、`run_service.rs`（plan 采纳 :319-345）、常量 `PLANNER_HONOR_TOP_K`（:53）

---

## Task 1: model_descriptions 端到端持久化（DB + 映射 + api-types + 前端类型）

**Files:** Create `migrations/021_model_descriptions.sql`；Modify `models/provider.rs`、`repository/provider.rs`、`repository/sqlite_provider.rs`、`nomifun-system/src/provider.rs`、`nomifun-api-types/src/provider.rs`、`ui/.../storage.ts`、`ui/.../providerApi.ts`、测试 fixture `tools_provider.rs:317-333`。

**迁移：** `ALTER TABLE providers ADD COLUMN model_descriptions TEXT NOT NULL DEFAULT '{}';`（仿 models/capabilities 的非空 JSON-TEXT）。

**改动（仿 `model_protocols` 全链路）：**
- `Provider` struct 加 `model_descriptions: Option<String>`（与 `model_protocols` 同型；FromRow 顺序匹配新列）。
- `CreateProviderParams`/`UpdateProviderParams` 加 `model_descriptions`（Create: `Option<&str>`；Update: `Option<Option<&str>>` 仿 model_protocols）。
- `sqlite_provider.rs`：INSERT 列+VALUES+bind；UPDATE SET+bind；create 手构 `Provider{...}` 返回补字段；merge_update 补字段。
- `ProviderService`：create/update 用 `serialize_opt` 序列化 `req.model_descriptions`→params；`row_to_response` 用 `deserialize_opt(&row.model_descriptions,"model_descriptions")?` →`ProviderResponse`。
- api-types：三个 DTO 加 `model_descriptions: Option<HashMap<String,String>>`（仿 model_protocols）。
- 前端：`IProvider.model_descriptions?: Record<string,string>`；providerApi.ts 的 Create/Update 同加。
- fixture `tools_provider.rs:317-333` 的 `Provider{...}` 字面量补字段。

- [ ] **Step 1: 测试（失败优先）** — provider 仓库/服务往返测试：create 带 `model_descriptions={"m1":"擅长前端"}` → row_to_response 解回相同 map；update 改描述 → 持久化。
- [ ] **Step 2: RED** `cargo nextest run -p nomifun-db -p nomifun-system -p nomifun-api-types`。
- [ ] **Step 3: 实现** 全链路。
- [ ] **Step 4: GREEN** 上述 nextest + `cargo build -p nomifun-app`；前端 `cd ui && npm run typecheck`(0)。
- [ ] **Step 5: 提交** `git commit -m "feat(orchestrator): 迁移021 providers.model_descriptions 端到端持久化"`

---

## Task 2: 模型中心 UI — 每模型描述编辑

**Files:** Modify `ModelModalContent.tsx`（每模型行 :492-614 + 删除清理 :580-600）。

**改动：** 在每模型行加描述编辑（推荐方案：模型名下方加一行可编辑次级文本，或行右操作组加 `Write` 图标按钮弹出小输入框）。展示/编辑 `platform.model_descriptions?.[model]`；保存仿 protocol 的 mutate 模式：
```tsx
updatePlatform({ ...platform, model_descriptions: { ...platform.model_descriptions, [model]: text } }, () => {});
```
删除模型处理器（:580-600）补清理 `model_descriptions`（删模型时移除其描述键）。整套 `IProvider`（已含 model_descriptions）经 `persistPlatform`→IPC 自动透传，无需新 IPC。视觉对齐既有行内控件（Arco/CSS 变量/icon-park outline 无别名）。描述输入空态友好（placeholder "描述该模型擅长什么，用于自动编排选用"）。

- [ ] **Step 1: 实现** 每模型描述编辑 + 删除清理。
- [ ] **Step 2: typecheck** `cd ui && npm run typecheck` → 0。
- [ ] **Step 3: build** `cd ui && bun run build` 绿。
- [ ] **Step 4: 提交** `git commit -m "feat(orchestrator): 模型中心每模型描述编辑(驱动自动编排选用)"`

---

## Task 3: 规划读描述 + 选择权放宽（LLM-primary + Router-veto）

**Files:** Modify `plan.rs`（produce :96-121 / build_plan_user_prompt :138-159）、`run_service.rs`（plan 采纳 :319-345）；测试内联。

**改动：**
1. `LlmPlanProducer::produce`（持 `provider_repo`）：构造提示前，对每个 `member`（有 `provider_id`+`model`）经 `provider_repo` 查该 provider 的 `model_descriptions[model]`，得 `Option<String>`。把描述传入 `build_plan_user_prompt`。
   - 实现：批量 `provider_repo.list()` 或按 provider_id `find_by_id`，建 `(provider_id,model)→description` map（注意 provider.model_descriptions 是 JSON TEXT,需解码）。
2. `build_plan_user_prompt`：成员行加 `desc=` 列：`{i}. {agent_id} | role={role} | strengths={strengths} | desc={description}`（无描述→`-`）。提示语补一句："优先依据 desc 把任务分给最合适的模型，设置 member_index。"
3. `run_service.rs` plan 采纳规则（:319-345）放宽：
   ```rust
   // 旧:.take(PLANNER_HONOR_TOP_K)  仅 top-2 采纳
   let planner_choice = planned.member_index.and_then(|mi|
       ranked.iter().find(|c| c.member_index == mi));   // 任意可行(在 ranked 中=通过硬过滤)即采纳
   let chosen = planner_choice.unwrap_or(&ranked[0]);    // 规划弃权/被硬过滤→Router 首位回退
   ```
   保留 `ranked.is_empty()`（全被硬过滤）分支不变。`PLANNER_HONOR_TOP_K` 删除或标注废弃。
   **理由注释**：裸模型成员 capability_profile=None→Router 中性无区分；描述驱动的 LLM 选择应被采纳,Router 只硬过滤+回退。

- [ ] **Step 1: 测试（失败优先）** — (a) `build_plan_user_prompt` 含 `desc=`(给成员带描述); (b) plan 采纳:6 个 None-profile 成员,planner 选 member_index=4 → 断言 assignment 选中 index 4 的成员(旧 top-2 规则会误选 ranked[0];新规则采纳 4); (c) planner 选了被硬过滤(如 needs_vision 但成员无 vision)的 index → 回退 ranked[0]; (d) planner 无 member_index → ranked[0]。produce 读描述用 mock provider_repo。
- [ ] **Step 2: RED** `cargo nextest run -p nomifun-orchestrator`。
- [ ] **Step 3: 实现** produce 读描述 + 提示 + 采纳放宽。
- [ ] **Step 4: GREEN** `cargo nextest run -p nomifun-orchestrator` + `cargo build -p nomifun-orchestrator -p nomifun-app` + `cargo nextest run -p nomifun-app -E 'binary(orchestrator_run_e2e)'`(4/4 不破)。
- [ ] **Step 5: 提交** `git commit -m "feat(orchestrator): 规划喂模型描述 + 采纳描述驱动选择(LLM-primary+Router-veto)"`

---

## Task 4: 集成 + 真机冒烟

- [ ] **Step 1:** `cargo build --workspace` 绿 + `cargo nextest run -p nomifun-orchestrator -p nomifun-db -p nomifun-system -p nomifun-api-types -p nomifun-gateway -p nomifun-app` 全绿（orchestrator_run_e2e 4/4）；前端 typecheck0+build。
- [ ] **Step 2: 真机冒烟（controller）** — `nomifun-web --dist --insecure-no-auth`（temp NOMIFUN_DATA_DIR target/_p3_smoke）。打开模型中心 → 给某模型填描述 → 保存 → 重开确认描述持久化 → 零 console error → UI 漂亮。截图 target/_p3_smoke。真·描述驱动选模型需 provider 真跑,留用户（mock 单测已证 seam）。
- [ ] **Step 3: 记账 + 提交**（若有微调）；账本追加 P3 完成行。

## Self-Review（spec §6）
**覆盖：** 模型描述字段端到端→T1；UI 编辑→T2；规划读描述 + 范围内择优(LLM-primary)→T3；集成→T4。
**风险：** 采纳规则放宽是 keystone 行为变更→T3 强测(4 case)+评审;描述读取的 provider_repo 解码(JSON TEXT)→T3;删模型清理描述→T2。
**类型一致：** model_descriptions 后端 HashMap↔前端 Record↔DB JSON-TEXT。

## Execution Handoff
波次：T1(持久化全链路,opus——跨6文件)→T2(UI,sonnet)→T3(规划+选择,opus——keystone 行为变更)→T4(集成冒烟,opus controller)。每任务两阶评审+fix+记账。禁合并 main。
