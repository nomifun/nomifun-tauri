# 会话原生编排（Conversation-Native Orchestration）重设计

> 取代 `2026-06-26-multi-agent-orchestrator-design.md` 中"工作空间→编队→Run"三步设置的入口模型。**后端编排引擎整套保留**，本 spec 只重构入口/触发/资产/可视化层，并补齐"模型描述驱动选用"与"角色资产沉淀"。

**状态**：设计已与用户对齐（三处分叉已定 + 助手统一已拍板）。用户授权自主实施，留在分支 `feat/multi-agent-orchestrator`，**禁合并 main**。

---

## 1. 动机与核心重构

### 1.1 旧设计的问题（用户原话）
- 使用路径太长：用户要独立完成"工作空间 → 编队 → Run 历史"创建才能开始。
- 优秀产品应像"会话"：选工作路径、输入需求、提交即开始。
- 模型编排应自动：用户未限定范围时按"录入模型时写的描述"自动选用；限定范围后在范围内挑选。不应要求用户预先做"编队"这种高成本操作。
- DAG 应是会话右栏的一个面板（点节点看该节点会话历史 + 工作状态）。
- 工作角色（规划/前端/后端/测试/设计…）应沉淀为可复用资产，可在选用范围里指定、模型也能自动复用。
- 旧的独立"智能编排"tab 可考虑与"会话"合并。

### 1.2 一句话重构
**一次 Run 就是一个"会话"——当主管判断需求值得拆分时，这个会话在右栏长出一张 DAG，每个节点是一个 worker 子会话。** 用户的操作面与今天的"会话"完全一致：选路径、写需求、提交。没有任何前置设置。

### 1.3 为什么代价可控（关键技术事实，来自代码勘察）
- **一次 Run 在运行期从不引用 Fleet 实体**：创建时编队成员被快照成 JSON 存入 `orch_runs.fleet_snapshot`；引擎/规划/路由/worker 此后只读这份快照（一个 `Vec<FleetMember>`）。`orch_assignments.member_id` 是无 FK 的 TEXT，指向快照而非活表。
- "工作空间→编队→Run 必须预建"的耦合**几乎全部集中在 `RunService::create` 一个函数**：它做两件事——查 fleet → 生成快照；查 workspace → 取 `workspace_dir`。
- 因此"从 {工作路径, 需求文本, 模型范围} 直接建 Run"**不需要改引擎**：只需新增一条 create 路径，就地把模型范围构造成成员快照；把工作路径作为 dir 传入。
- `fleet_members.agent_id` 当前是**死字符串**（无 FK、无读取路径），可被重新赋予"指向助手"的语义而不破坏任何现存逻辑。
- 注册模型**目前没有用户自填描述字段**（只有自动推断的能力标签 + 上下文窗口），"按描述自动选用"需要新增该字段。
- 会话模型选择器目前是**单选**（`current_model`），"范围"需要扩成多选/自动。

---

## 2. 核心概念（重定义）

| 概念 | 定义 | 实现承载 |
|---|---|---|
| **Run** | 一次有目标的多 agent 执行，宿主在一个**主管会话**里 | `orch_runs`（复用）；新增 `lead_conv_id` 关联 + `work_dir` |
| **主管会话（Lead）** | 用户直接对话的 Nomi 会话，武装了 `caps_orchestrator` 工具 + 主管提示词；它创建/规划/驱动 Run，可被用户 steer | 一个 `type='nomi'` 会话，`extra` 带 `orchestrator_role='lead'` + `model_range` + （创建 Run 后）`orchestrator_run_id` |
| **Worker（节点）** | DAG 的一个任务节点 = 一个真实子会话（nomi yolo + desktopGateway） | `orch_run_tasks`（复用）；worker conversation `extra.orchestrator_run_id/task_id`（复用，已从主侧栏过滤） |
| **角色资产 = 助手** | 可复用的命名角色（规划/前端/后端/测试/设计…）= 一个 `assistants` 记录：name + description + 系统提示 + 技能 + 标签 + 偏好模型 | `assistants`（复用 + 补字段）；管理面复用助手页 |
| **模型范围** | 本次 Run 允许使用的模型集合：`单一` / `自动`（全部启用） / `范围`（勾选若干） | 会话 `extra.model_range`；可存为可选预设 |
| **模型描述** | 用户注册模型时自填的自由文本，描述该模型擅长什么 | **新增** `providers.model_descriptions`（按 model id 的 JSON map）|
| **编排预设（可选）** | 命名的"模型范围 + 钉选角色"组合，便于复用；**非关键路径** | 复用 `fleets`/`fleet_members` 重新诠释 |

**触发规则**：会话创建时模型选择 = `单一模型` → 普通会话（不武装主管，今日行为）；= `自动` 或 `范围` → 武装为主管会话。主管仍按需求复杂度决定是否真的 `nomi_run_create` 拆 DAG——简单需求即使在自动档也只在主管会话里直接作答。

---

## 3. 入口与触发（点 #1、#2、#3）

### 3.1 复用会话入口
复用 `ui/src/renderer/pages/guid/GuidPage.tsx` 全套：`GuidWorkspaceFootnote`（工作路径）+ `GuidInputCard`（需求文本 + 附件）+ 模型选择器。提交链路 `useGuidSend.handleSend()` → `ipcBridge.conversation.create.invoke({type:'nomi', ...})` → 跳 `/conversation/{id}`。**不新增入口页**。

### 3.2 模型选择器三态（替换单选）
`GuidModelSelector` / `useGuidModelSelection`（nomi 路径）从"单选 `current_model`"扩展为三态：
- **单一模型**（默认，今日行为）：选定一个模型，普通单 agent 会话。
- **自动**：不指定具体模型，允许编排；范围 = 当前所有启用模型。
- **范围**：多选若干模型，允许编排；范围 = 勾选集合。

选择结果写入会话创建 `extra.model_range`：
```ts
// extra.model_range
| { mode: 'single', model: { provider_id: string; model: string } }
| { mode: 'auto' }
| { mode: 'range', models: Array<{ provider_id: string; model: string }> }
```
`mode==='single'` → 普通会话；`auto`/`range` → 主管会话（见 §4.1）。

UI：选择器加一个分段控件（单一 / 自动 / 范围）。`范围`态下模型项变多选 checkbox。视觉对齐既有 Arco 下拉风格（点 #UI 硬底线）。

### 3.3 主管会话的武装
当 `model_range.mode ∈ {auto, range}`，`useGuidSend` 在创建 nomi 会话时于 `extra` 注入：
- `orchestrator_role: 'lead'`
- `model_range`（上面的结构）
- `session_mode: 'yolo'`、`desktopGateway: true`（主管需要工具权限）

后端 `ConversationService::create` / 会话引擎在装配该会话时，识别 `orchestrator_role==='lead'`，为其挂载 `caps_orchestrator` 工具集 + 主管系统提示词（说明：你可用 `nomi_run_create` 把复杂需求拆成 DAG，简单需求直接作答；模型范围已由用户限定在 `extra.model_range`）。

---

## 4. 会话原生 Run 创建（点 #1 核心 + 引擎复用契约）

### 4.1 主管驱动（Model α，选定方案）
主管会话收到用户需求后：
1. 简单需求 → 直接在主管会话里作答，不建 Run。
2. 复杂需求 → 调用 `nomi_run_create`（改造后签名见 §4.3）创建 Run，引擎并行跑 worker；主管在会话里同步进展、可被用户 steer；DAG 在右栏可视化。

主管会话即"会话原生"的体现：用户始终在一个会话里，编排是这个会话的能力，而非另一个页面。

### 4.2 `RunService` 新增 fleet-less / workspace-less 创建路径
新增 `RunService::create_adhoc`（与既有 `create` 并存；旧 `create` 可保留给历史/外部）：
```rust
pub struct CreateAdhocRunRequest {
    pub goal: String,                 // = 需求文本
    pub work_dir: Option<String>,     // = 工作路径（可空 = 临时）
    pub model_range: ModelRange,      // single/auto/range，见 §3.2 的 Rust 镜像
    pub pinned_roles: Vec<String>,    // 可选：用户钉选的助手 id（角色资产）
    pub autonomy: Option<String>,     // 缺省 supervised
    pub max_parallel: Option<i64>,
    pub lead_conv_id: Option<i64>,    // 主管会话 id（用于回写 run_id + DAG 绑定）
}
```
实现：
- **成员快照**：把 `model_range` 展开 + `pinned_roles` 解析为 `Vec<FleetMember>`：
  - `pinned_roles` 中每个助手 → 一个 `FleetMember`：`agent_id=助手id`、`provider_id/model`=助手偏好模型（在范围内，否则取范围首个）、`role_hint=助手名`、`capability_profile`=由助手描述/标签派生（见 §5.4）。
  - `model_range` 展开为"裸模型成员"：每个 `(provider_id, model)` → 一个 `FleetMember`，`agent_id=""`、`role_hint=""`（角色由规划逐任务赋予）、`capability_profile`=由模型描述派生（见 §6.3）。`mode==='auto'` 时后端展开为全部启用模型。
  - 合并去重后序列化进 `orch_runs.fleet_snapshot`（与今日格式一致，引擎无感）。
- **工作目录**：写入新列 `orch_runs.work_dir`（见 §9.1）；`workspace_id` 置 NULL。
- **回写**：`orch_runs.lead_conv_id = lead_conv_id`；并把 `run_id` 写回主管会话 `extra.orchestrator_run_id`（供 DAG 右栏定位，§7.3）。
- 之后 `plan()` → 引擎执行，与今日完全一致。

### 4.3 改造 `caps_orchestrator`
`nomi_run_create` 工具签名从 `{workspace_id, goal, fleet_id, autonomy?}` 改为 `{goal, autonomy?}`（精简），其余参数由主管会话上下文推导：
- `work_dir` ← 主管会话的工作路径（`extra.workspace`）。
- `model_range` ← 主管会话 `extra.model_range`。
- `pinned_roles` ← 主管会话 `extra.pinned_roles`（若用户钉选）。
- `lead_conv_id` ← 当前会话 id。
工具内部调用 `RunService::create_adhoc`。`nomi_run_status`/`nomi_run_result` 不变。

### 4.4 引擎复用契约（不变量，禁破坏）
以下保持零改动或仅最小适配：`RunEngine`/`RunEngineDeps`/`run_loop`、`ConversationWorkerRunner`、`LlmPlanProducer`+`pick_lead`、`Router`、`orch_runs/orch_run_tasks/orch_run_task_deps/orch_assignments` 执行表、全部 run 生命周期（plan/approve/pause/resume/cancel/reassign/steer）、IDMM 武装、worker conversation 从主侧栏过滤。唯一适配点：`run_loop` 取 `workspace_dir` 处改为"优先 `run.work_dir`，回退 `workspace_id` 查表"。

---

## 5. 角色资产 = 助手（点 #5 + 用户追问的拍板）

### 5.1 判断结论
"工作角色"与"助手"是同一概念实现了两遍。**统一为 `assistants`**：编排角色就是助手，管理面直接复用助手页（卡片网格 + 标签筛选 + 编辑抽屉现成）。`fleet_members.agent_id` 当前是死字符串，恰好可承载"指向助手"的语义。

### 5.2 助手已具备的（无需新建）
`assistants` 表已有：`name`、`description`、`avatar`、系统提示（规则文件，经 `readAssistantRule`）、`enabled_skills`/`disabled_builtin_skills`、`audience_tags`/`scenario_tags`，以及一个**已存在但 UI 未开放**的 `models` JSON 字段（`models[0]` 在会话创建时已被尊重）。

### 5.3 为编排补的三处缺口
1. **开放 `models` 字段编辑**：助手编辑抽屉 `AssistantEditDrawer` 新增"偏好模型/小范围"控件，写入 `assistants.models`。语义=该角色擅长用哪些模型（编排挑模型时的内层倾向）。
2. **建 `agent_id → assistants` 读取链路**：worker 执行时，若 `member.agent_id` 能解析到助手，则 worker brief 继承该助手的系统提示 + 应用其 `enabled_skills`/`disabled_builtin_skills` + 偏好模型（在范围内求交）。今天 worker 只注入 orchestrator 拼装的 brief（`ROLE: {role_hint}`），需扩展 `compose_brief` / worker 装配读取助手。
3. **规划读助手描述**：`LlmPlanProducer` 的成员清单（今天只喂 `agent_id/role_hint/strengths`）改为附带助手 `description`，让主管/规划"按描述匹配角色"，实现自动复用。

### 5.4 capability_profile 的来源
确定性 `Router` 仍按 `capability_profile` 打分。新路径下：
- 助手成员：`capability_profile` 由助手的标签 + 描述派生（轻量启发式：从描述/标签关键词映射 strengths/modalities/reasoning/cost_tier；无则取基线）。
- 主路径以 **LLM 规划（读描述）** 为主、确定性 Router 为兜底/打分辅助——这正合"按描述自动选用"。`needs_long_context` 等未用字段保持现状（carry-forward）。

---

## 6. 模型描述 + 范围内择优（点 #2 真正缺的）

### 6.1 新增模型描述字段
`providers` 表新增 `model_descriptions TEXT DEFAULT '{}'`（按 model id 的 JSON map：`{ "<model_id>": "<用户自填描述>" }`）。api-types `ProviderResponse`/`Create`/`Update` 加该字段；前端 `IProvider` 同步。

### 6.2 注册 UI
模型中心（`AddPlatformModal`/`AddModelModal`/`EditModeModal` 或每模型行）为每个模型提供一个"描述/备注"文本框，写入 `model_descriptions[model_id]`。视觉对齐既有表单。

### 6.3 择优算法
- **范围是外层边界**：`model_range`（single/auto/range）限定可用模型集合。
- **角色偏好是内层倾向**：助手 `models` 表达该角色偏好的模型。
- **描述驱动**：规划/挑选时，把"模型描述 + 任务画像 + 角色偏好"一并提供给主管（LLM）；在范围内择优。裸模型成员的 `capability_profile` 由模型描述 + 能力标签派生供 Router 打分兜底。
- 求交规则：`选中模型 = (角色偏好 ∩ 范围)` 优先；为空则"范围内按描述/画像择优"。

---

## 7. DAG 右栏（点 #4）

### 7.1 复用右栏抽象
复用 `WorkspaceSource.extraTabs`（Nomi 已用它挂"会话指标"tab）。在主管会话的右栏 `ChatSlider` 中新增一个 **"编排/DAG" extraTab**（仅当会话 `extra.orchestrator_role==='lead'` 且已有 `orchestrator_run_id` 时出现）。

### 7.2 面板内容
DAG tab 渲染该 Run 的 react-flow 节点图（复用既有 `DagCanvas`/`TaskNode`/`layoutDag`）：节点 = 任务，边 = 依赖，颜色 = 状态。点节点 → 复用 `ReadOnlyConversationView`（已修好 PreviewProvider）展示该节点的会话历史 + 工作状态 + （运行中）steer 输入。WS 实时更新复用 `useRunLive`/`orchestratorEvents`。

### 7.3 绑定
DAG tab 通过主管会话 `extra.orchestrator_run_id` 定位 Run（无则查 `orch_runs WHERE lead_conv_id = 当前会话`）。右栏 collapse/toggle 机制现成（按 `conversation_id` 键）。PreviewProvider 由 `ChatLayout` 提供（per-surface，不变量）。

---

## 8. 沉淀（点 #5 的"建议保存 + 自动复用"）

### 8.1 蒸馏候选
Run 结束（或主管收尾）时，对本次出现的**临时角色**（即 `agent_id` 为空、由规划赋予 `role_hint` 的成员/任务）蒸馏为助手候选：`name=role_hint`、`description`=主管对该角色职责的一句话总结、`enabled_skills`=该 worker 实际启用的技能、`models`=该任务实际用的模型。

### 8.2 一键采纳
在 DAG 右栏/Run 总结处展示候选卡片（"把这些角色存为可复用助手？"），用户一键采纳 → 写入 `assistants`（source=user）。**不自动写入**（避免一次性角色污染，符合用户选定的"建议保存"档）。

### 8.3 自动复用
下次规划，`LlmPlanProducer` 的成员/角色候选包含现有助手（附描述，§5.3），主管按描述优先复用已有助手而非每次新造。

---

## 9. 数据模型变更

### 9.1 迁移 `020_conversation_native_orchestration.sql`
已核对 018 实际 DDL：`orch_runs` **已有** `lead_conv_id INTEGER`（无需新增）；`workspace_id TEXT NOT NULL REFERENCES orch_workspaces(id) ON DELETE CASCADE` + 索引 `idx_orch_runs_workspace`。因此 020 需：
1. **重建 `orch_runs` 让 `workspace_id` 可空** + 新增 `work_dir TEXT`。SQLite 不能直接 drop NOT NULL，走标准表重建：
   - `PRAGMA foreign_keys=OFF;`（核对迁移运行器是否已在事务/关闭 FK 环境——参考 019 的写法）
   - 建 `orch_runs_new`（同列，但 `workspace_id TEXT`（可空、保留 `REFERENCES orch_workspaces(id) ON DELETE CASCADE`，FK 仅在非空时校验）+ 末尾加 `work_dir TEXT`）
   - `INSERT INTO orch_runs_new (<018 列...>) SELECT <018 列...> FROM orch_runs;`（`work_dir` 留空）
   - `DROP TABLE orch_runs;` → `ALTER TABLE orch_runs_new RENAME TO orch_runs;`
   - 重建索引 `idx_orch_runs_workspace`。
2. `ALTER TABLE providers ADD COLUMN model_descriptions TEXT NOT NULL DEFAULT '{}';`

> 迁移 append-only；编号 020（019 是 drop_team）。实施任务须先 Read 018 + 019 确认重建写法与 FK/事务约定一致；迁移测试若断言旧约束需同步更新。

### 9.2 api-types / 前端类型
- `ProviderResponse`/`CreateProviderRequest`/`UpdateProviderRequest` + 前端 `IProvider`：加 `model_descriptions`。
- 新增 `ModelRange` DTO（single/auto/range）+ `CreateAdhocRunRequest`。
- 会话 `extra` 约定字段：`orchestrator_role`、`model_range`、`pinned_roles`、`orchestrator_run_id`（lead）。

### 9.3 助手
`assistants.models` 已存在，仅前端编辑器开放。无需迁移。

---

## 10. 旧设计退役（点 #1、#6）

- **删除创建类页面**：工作空间创建、编队创建/编辑、Run 历史中的"新建 Run"流程——从关键路径移除。
- **独立 tab 重做为"资产库"**（非关键路径）：助手（角色资产）+ 编排预设（可选的模型范围 + 钉选角色，复用 `fleets`/`fleet_members` 重新诠释）+ Run 历史（只读复盘）。或按实施评估，把这些并入会话页内的分组/弹层（用户首选"并入会话 + 保留资产库"）。
- `orch_workspaces` 表保留（back-compat），新流程不再创建；`fleets`/`fleet_members` 重新诠释为可选预设。
- 侧栏"智能编排"入口：保留为"资产库"入口，或并入会话（实施期定，倾向保留一个轻量"编排资产"入口）。

---

## 11. 复用契约（实施期禁破坏）

**零改/微改复用**：`RunEngine`/`run_loop`/`ConversationWorkerRunner`/`LlmPlanProducer`/`pick_lead`/`Router`/全部 run 生命周期 + 执行表 + IDMM 武装 + worker 主侧栏过滤 + react-flow 画布组件 + `ReadOnlyConversationView`（已修）+ `useRunLive`/`orchestratorEvents`。

**唯一引擎适配点**：`run_loop` 取工作目录处 → 优先 `run.work_dir`。

---

## 12. 不变量（实施期硬约束）

1. worker = 真实子会话（nomi yolo + desktopGateway + `orchestrator_run_id/task_id` 标记 + 从主侧栏过滤）。
2. Run = 分裂过的主管会话；主管会话 = 普通 nomi 会话 + caps_orchestrator + 主管提示词，由用户对话驱动。
3. 引擎只吃 `fleet_snapshot`（`Vec<FleetMember>`），不依赖 Fleet/Workspace 实体。
4. 角色资产 = 助手（统一，不再造平行概念）；`fleet_members.agent_id` 承载助手引用。
5. 模型选择三态：单一=普通会话；自动/范围=允许编排，主管按复杂度决定是否真拆。
6. 沉淀=建议保存（不自动写库）。
7. 品牌字样 NomiFun；新增/改动 UI 必须漂亮（验收门槛），走既有视觉语言。
8. 禁破坏 pause/resume/steer/cancel/IDMM 语义。
9. `ReadOnlyConversationView` 保留自挂 PreviewProvider（per-surface 不变量）。
10. 禁合并 main；禁 cargo fmt；提交前 pull --rebase。

---

## 13. 分期（每期独立可验收）

- **P1 — 会话式 Run 创建（体验主干）**：`RunService::create_adhoc`（成员快照从 model_range 构造）+ `orch_runs.work_dir` 迁移 + `run_loop` 适配 + `caps_orchestrator.nomi_run_create` 改签名 + 会话 `extra` 约定 + 主管会话武装（caps_orchestrator + 主管提示词）。前端：模型选择器三态 + `useGuidSend` 注入 `model_range`/主管标记。**验收：选自动/范围、写需求、提交 → 主管会话拆出多 agent run（用现有助手/模型，mock e2e 证 seam）。**
- **P2 — DAG 右栏**：`WorkspaceSource.extraTabs` 加"编排"tab（lead 会话）+ 复用画布 + 点节点看转录 + `useRunLive` 接线。**验收：主管会话右栏出现 DAG，点节点看到 worker 转录，零 console error。**
- **P3 — 模型描述 + 三态择优**：`providers.model_descriptions` 迁移 + api-types/前端类型 + 模型中心 UI 描述框 + 裸模型成员 capability 由描述派生 + 规划喂描述。**验收：注册模型填描述；自动档下规划按描述择优（mock 证 seam）。**
- **P4 — 助手↔角色统一**：`AssistantEditDrawer` 开放 `models` + `agent_id→assistants` 读取链路（worker brief 继承助手 prompt/skills/模型）+ 规划成员清单附助手描述 + capability 由助手标签/描述派生。**验收：钉选助手作角色 → worker 继承其 persona/skills/模型。**
- **P5 — 沉淀 + 退役 + 资产库**：Run 结束蒸馏角色候选 + 一键采纳写助手 + 自动复用规划 + 删除创建类页面 + tab 改资产库（助手 + 预设 + Run 历史）。**验收：跑完建议保存角色，采纳后下次可复用；旧创建流程已无；资产库可用且漂亮。**

---

## 14. 测试策略

- 后端：`RunService::create_adhoc` 单测（model_range→快照展开、pinned_roles 解析、work_dir 落库）；`caps_orchestrator` 改签名后 e2e；引擎契约回归（既有 orchestrator_run_e2e 3/3 不破）；助手→worker brief 继承单测（mock 助手）。
- 前端：typecheck 0 + build 绿；模型选择器三态、DAG tab 渲染、模型描述表单的真机冒烟（nomifun-web --dist --insecure-no-auth + 无头）。
- 真效果（自动挑模型/角色复用/主管决策）需配 provider+API key 真跑，留用户验收；CI 用 mock planner/worker 证 seam（沿用既有模式）。

---

## 15. Carry-forward（非阻塞）

- 对外（gateway/Remote）暴露前补 run get/update/delete 的 `user_id` 归属校验（沿用旧 carry-forward）。
- `needs_long_context` 路由未计分；`team_capable` 端点 vestigial 可清。
- `orch_workspaces` 表 back-compat 保留，后续若确认无历史数据可单独 drop。
- 编排预设（复用 fleets）若评估为低价值，可在 P5 降级为"仅 Run 历史 + 助手"两类资产，预设留后续。
