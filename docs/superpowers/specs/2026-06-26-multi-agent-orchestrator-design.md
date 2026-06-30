# 多 Agent 智能编排引擎 · 设计文档

- **状态**：设计已与用户对齐，待用户审阅本文档后转 writing-plans。
- **日期**：2026-06-26
- **用户可见名**：「智能编排」（Smart Orchestration）
- **内部 crate / 标识**：`nomifun-orchestrator`
- **取代对象**：遗留 `nomifun-team` 子系统（交付时移除，见 §11）

---

## 1. 背景与动机

### 1.1 用户目标
用户希望「同时使用多个 agent 与多个模型」，由一个**主管 Agent** 依据**任务特征**与各**模型 & agent 的能力特征**，**自动智能地分配并执行**任务；同时允许用户**预先设定可用的 agent & 模型范围**以控制成本与效果。该功能在侧边栏单开一个 tab，UI 必须美观、有格调，并对「用户配置/编排/生命周期管理」与「逐 agent 信息/生命周期展示」两个视角都清晰、易用、健全。**必须完整可用，交付后做全链路测试**。

### 1.2 现状与「为什么不复用 team」
代码库已存在 `nomifun-team` 子系统（Lead + N Teammates，每个 agent 是一条完整会话），但它被明确视为遗留设计：

- 编排是**纯散文转述**：协调全靠一个 SQLite mailbox + 顾问性质（advisory，状态/owner 是自由文本，不强制）的 TaskBoard + 每 agent 的「唤醒提示」事件循环。没有一等公民的 plan/DAG、没有把任务分派给 agent 的调度器、没有 run 记录。
- **拓扑写死**为单 Lead + 扁平 teammates（星形），Lead 是贯穿各处的特例。
- **紧耦合**：每个 agent 必须是完整会话；编排靠 kill+warmup 进程重建 + 直接插 MessageRow；agent 经一个 per-team TCP+HTTP MCP server 回话，端口/令牌每次重启都变、必须重新注入每条 `conversation.extra` 并重新 warmup（脆弱、重启繁重）。
- **通信是一次性 mailbox 行 + LLM 回合**：agent 间无流式、无结构化结果回流、无中途取消（只有一个可被拒绝的 shutdown_request）。
- **并发封顶 4**，且受 mailbox/notify 竞态制约（双 finalize 去重窗口、wake 锁回退、self-trigger 过滤等多处打补丁）。
- **前端从未成型**：无 `/team` 路由/页面，只有一个 per-conversation 开关 + 只读子 agent 抽屉。
- README、归档的阶段文档、`agents_version='1.0.1'`、过时命令、gateway 显式禁止 `nomi_team_*` 出现在任何 surface——都标志它早于本引擎的意图。

### 1.3 可复用的现有资产（站在巨人肩上）
- **5 个可插拔 agent 引擎**：`AgentType` 枚举（Acp / Nomi / OpenclawGateway / Nanobot / Remote）→ 单一工厂 `build_agent` → `AgentInstance` 枚举。新增引擎 = 新枚举分支 + 新工厂分支。
- **`AgentRegistry`**：`agent_metadata` 表水化成内存目录，是「哪个 agent 能做什么」的现成查询（`team_capable`、`BehaviorPolicy`、handshake `agent_capabilities` 含 image/audio/mcp）。
- **`ProviderWithModel { provider_id, model, use_model }`**：解耦引擎与凭证/模型——任意子 agent 可被赋予任意 provider/model。
- **`AgentStreamEvent` 广播 + `StreamRelay`**：与 agent 类型无关的通用事件扇出（WS + DB 持久化 + 续写/failover）。
- **IDMM**（`nomifun-idmm`）：通用监督层。一个 agent 只要实现 `SessionProbe` 就能获得「规则→模型→halt」自动决策；三个 seam trait（`IdmmHandle` / `ConversationSupervisionHook` / `TerminalSupervisionHook`）展示了单向依赖倒置的接入范式。
- **Gateway 能力内核**（`nomifun-gateway`）：单一能力 Registry（~132 工具，DangerTier×Surface 权限矩阵），`nomi_agent_run`/`nomi_agent_result` **已实现** fire-and-poll 委派（生成一个自主 nomi 子会话、流式回传进度）；`nomi_create_conversation`+`nomi_send_to_conversation`+`nomi_conversation_status` 让「主管」可创建/驱动/观察子会话。Remote 前门 + per-companion 令牌已就位。
- **会话引擎**（`nomifun-conversation`）：`ConversationService`（串行 TurnClaim，每会话至多一个活回合）、`IWorkerTaskManager`（每会话一个 `AgentInstance`，`Arc<OnceCell>` 语义）、steering inbox、协作式取消。
- **DB/接线规范**：仓库模式（`I*Repository`+`Sqlite*Repository`）、迁移 append-only（最新 017）、设备边界 ID 规则、ts-rs `#[ts(type="number")]` 防 bigint、realtime（`EventBroadcaster`+`WebSocketManager`+域 `*EventEmitter`）。模板 = `nomifun-webhook` / `nomifun-cron`。
- **前端**：`ChatLayout`（三栏壳）、`ContentSider`、`MessageList`+流式 store、`useNomiMessage` 订阅、`MessageToolGroupSummary`/`MessageText`/`MermaidBlock`、`NomiModal`、`AssistantTagFilterBar`（chip 筛选）、`ContextUsagePill`、CSS 变量主题、`react-flow`（无限画布调研选型）。

### 1.4 核心缺口（本引擎要新建的东西）
- **没有任何任务路由层**：能力元数据只描述 agent「能做什么」，没有把任务对 agent/模型能力打分并自动选择的机制。当前引擎+backend+模型由人在建会话时手选。
- **没有持久的 plan/run/DAG 模型**、没有调度器、没有结构化结果聚合。
- **没有编排 UI**（无专属路由、无并发 agent 画布、无任务图可视化、无逐 agent 实时转录磁贴、无聚合 run 时间线）。

---

## 2. 目标与非目标

### 2.1 目标
1. 一个**持久工作间**内可反复发起**目标驱动的 Run**；Run 由主管拆成任务 DAG、按能力**自动分派**给一支用户可配置的**编队**、**真并行**执行、动态再规划、聚合汇报。
2. **能力路由**：依任务特征 × agent/模型能力自动分派并给出**理由**；用户可逐任务**改派/锁定**。
3. **成本/效果可控**：编队即「可用范围」控制面；Run 级可选并发上限与（可选）预算上限。
4. **人在环**：Run 级三档自主（自主/守护/协同）；随时 steer、暂停/恢复/取消、编辑/改派任务。
5. **美观且双视角清晰**的前端：DAG 编排画布为 hero，点节点展开该 agent 实时对话；编队管理视图同时服务「用户配置」与「逐 agent 信息/生命周期」。
6. **统一暴露**：经 gateway `caps_orchestrator` 域对内对外（Remote）可调用，并收编遗留 `nomi_agent_run`。
7. **完整、健全、可用**：交付时移除 team，并完成全链路真机测试。

### 2.2 非目标（YAGNI）
- 不做嵌套子编队 / 多级主管（一层主管 + 扁平 worker 池足矣；DAG 提供结构）。本期不支持「worker 再开 worker」的递归编排（主管可再规划替代）。
- 不做跨设备分布式执行（worker 都跑在本机引擎上）。
- 不做计费/账单系统（成本仅展示与软上限，不做硬性结算）。
- 不强行兼容 team 的任何数据/接口（明确弃旧）。

---

## 3. 概念模型（三层 + 执行单元）

| 概念 | 定义 | ID 形态 |
|---|---|---|
| **Fleet 编队** | 可复用的「可用 agent×模型」名单。成员 = agent（`AgentRegistry`）+ `ProviderWithModel` + 角色提示 + 能力画像 + 约束。是「预设可用范围」= 成本/效果控制面。 | `fleet_{uuidv7}`（跨 gateway/Remote 边界 → 字符串） |
| **Workspace 工作间** | 持久容器：默认编队 + 工作目录（复用现有 workspace 机制）+ 上下文/产物 + 历次 Run。 | `ows_{uuidv7}`（经 gateway 暴露 → 字符串） |
| **Run 行动** | 一次目标执行：目标 + 编队快照 + 自主级别 + 计划(DAG) + 状态 + 汇总。可复盘/可 fork。 | `run_{uuidv7}` |
| **RunTask 节点** | DAG 节点：标题、规格、状态机、被分派成员、**worker 会话 conversation_id**、上游输入、产物摘要、重试、成本。 | `rtask_{uuidv7}` |
| **RunTaskDep 边** | 有向依赖 `(blocker, blocked)`，completion 解除阻塞（复用 team 的边表模式）。 | 复合主键 |
| **Assignment 分派** | task→成员、理由、打分、auto/override、locked。 | `asg_{uuidv7}` |
| **FleetMember 成员** | agent_id + provider_id + model + role_hint + capability_profile(JSON) + constraints(JSON)。 | `fmem_{uuidv7}` |

### 3.1 RunTask 状态机
```
pending ──(deps satisfied)──▶ ready ──(scheduler picks)──▶ assigned
   │                                                          │
   │                                              (worker turn starts)
   ▼                                                          ▼
skipped                                                    running
                                                              │
                            ┌──────────────┬──────────────────┤
                            ▼              ▼                   ▼
                       needs_review     failed             done
                       (协同/守护)    (可 retry/改派)    (产物落库, 解除下游)
```

### 3.2 Run 状态机
```
draft ─▶ planning ─▶ awaiting_plan_approval(协同) ─▶ running ⇄ paused
                                                       │
                          ┌────────────────────────────┼───────────────┐
                          ▼                             ▼               ▼
                      completed                     failed          cancelled
```

---

## 4. 架构

### 4.1 分层与定位
新 crate `nomifun-orchestrator` 位于 `nomifun-conversation` / `nomifun-ai-agent` 之上，镜像 `nomifun-requirement`（AutoWork——现存最接近的「多目标循环 runner」）的分层。它**拥有计划 + 调度 + 路由**；**不**重新实现 agent 运行时。

### 4.2 worker 实体：每个 worker 任务 = 一条真实会话（关键抉择）
- 任务就绪时，调度器按分派成员**确保**一条对应 `agent_type + backend + model` 的会话存在（带任务工作目录 + 「worker 简报」系统提示 + 上游产物作为输入），把任务规格作为一个 turn 发出（`ConversationService::send_message`），消费其 `AgentStreamEvent` 流（`StreamRelay`）。
- **白嫖**：流式、工具、IDMM、工作区栏、gateway 工具、steering、取消——全部复用。每个 worker 天然是一条可观察的真实对话，正好喂画布右侧面板。
- worker 会话标记：`conversation.extra` 加 `{ orchestrator_run_id, orchestrator_task_id, worker_brief, session_mode }`；这些 worker 会话**从主侧栏过滤掉**（参照 companion `companionSession` 过滤），只在编排画布内呈现。
- 被否决的方案：B 轻量一次性模型调用（失去工具/流式/IDMM/工作区，否决）；C 复活 team 的 per-team MCP + mailbox 唤醒（脆弱遗留，否决）。

### 4.3 协调方式
orchestrator **直接驱动会话并消费其事件流**，而非 team 的散文转述 mailbox 唤醒。任务间通过**结构化产物传递**（上游 task 的 `output_summary` + 产物文件路径注入下游 task 的输入），而非自由文本广播。

### 4.4 主管 Agent 与执行哲学
主管是一个 **Nomi 引擎 agent**，被授予一套**编排工具集**（gateway 新域 `caps_orchestrator`，见 §8）。回合流程：
1. **规划**：拆目标为 RunTask DAG（结构化计划，工具 `nomi_run_plan` 写入 tasks+deps）。
2. **分派**：每任务由 Router（§6）提名成员；主管可接受或调整。
3. **调度**：调度器把就绪任务**真并行**跑在 worker 上（受编队/Run 并发上限约束）。
4. **再规划**：worker 完成后产物喂下游；主管可动态增删任务（DAG 动态生长）。
5. **汇总**：主管聚合并向用户汇报（写入 Run.summary）。

= **确定性调度器（稳）** ⊕ **智能体再规划（活）**。调度器本身是确定性的（依赖驱动、并发受限、状态机严格）；主管的「规划/再规划/分派」是 LLM 智能体行为，但其产出落库为结构化 plan，由确定性调度器执行——避免 team「散文即真相」的不可靠。

### 4.5 并发模型
- **真并行**是核心价值（team 封顶 4、companion 串行拒绝）。
- 每条 worker 会话仍遵守现有「每会话串行单回合」TurnClaim（不破坏现有不变量）；并行发生在**不同 worker 会话之间**。
- 并发上限：`min(Run.max_parallel ?? Fleet.max_parallel ?? 全局默认, 就绪任务数)`。调度器维护一个就绪队列 + 信号量。
- provider 速率保护：同 provider 的并发可额外设软上限（沿用 team 的 provider-rate 保护精神，但按 provider 维度而非全局 4）。

---

## 5. 数据模型（迁移 018）

> 迁移 append-only：新增 `018_orchestrator.sql`，**不改** 001。设备边界规则：跨 gateway/Remote 暴露的实体用字符串前缀 id；worker `conversation_id` 仍是本机 INTEGER。`PRAGMA foreign_keys=ON`。

```sql
-- 编队
CREATE TABLE fleets (
  id            TEXT PRIMARY KEY,            -- fleet_{uuidv7}
  user_id       TEXT NOT NULL,
  name          TEXT NOT NULL,
  description   TEXT,
  max_parallel  INTEGER,                     -- 编队级并发上限(可空→全局默认)
  created_at    INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL
);

CREATE TABLE fleet_members (
  id                 TEXT PRIMARY KEY,        -- fmem_{uuidv7}
  fleet_id           TEXT NOT NULL REFERENCES fleets(id) ON DELETE CASCADE,
  agent_id           TEXT NOT NULL,           -- AgentRegistry id(软引用，无 FK，因 builtin slug)
  provider_id        TEXT,                    -- providers.id；ACP 引擎可空(模型由 CLI 协商)
  model              TEXT,                    -- 模型 id
  role_hint          TEXT,                    -- 角色/职责提示(注入 worker brief)
  capability_profile TEXT,                    -- JSON: 合成的能力画像(见 §6)
  constraints        TEXT,                    -- JSON: {max_concurrency?, cost_tier?, allowed_task_kinds?}
  sort_order         INTEGER NOT NULL DEFAULT 0,
  created_at         INTEGER NOT NULL,
  updated_at         INTEGER NOT NULL
);

-- 工作间
CREATE TABLE orch_workspaces (
  id                TEXT PRIMARY KEY,         -- ows_{uuidv7}
  user_id           TEXT NOT NULL,
  name              TEXT NOT NULL,
  default_fleet_id  TEXT REFERENCES fleets(id) ON DELETE SET NULL,
  workspace_dir     TEXT,                     -- 工作目录(复用 workspace 机制)
  context           TEXT,                     -- JSON: 持久上下文/备注
  created_at        INTEGER NOT NULL,
  updated_at        INTEGER NOT NULL
);

-- Run
CREATE TABLE orch_runs (
  id              TEXT PRIMARY KEY,           -- run_{uuidv7}
  workspace_id    TEXT NOT NULL REFERENCES orch_workspaces(id) ON DELETE CASCADE,
  user_id         TEXT NOT NULL,
  goal            TEXT NOT NULL,
  fleet_snapshot  TEXT NOT NULL,              -- JSON: 发起时编队成员快照(可复盘)
  autonomy        TEXT NOT NULL,              -- 'autonomous'|'supervised'|'interactive'
  max_parallel    INTEGER,
  lead_conv_id    INTEGER,                    -- 主管会话(conversations.id, 本机)
  status          TEXT NOT NULL,              -- draft|planning|awaiting_plan_approval|running|paused|completed|failed|cancelled
  summary         TEXT,                       -- 主管最终汇总
  total_tokens    INTEGER,
  forked_from     TEXT,                       -- run_{uuidv7}? 复盘来源
  created_at      INTEGER NOT NULL,
  updated_at      INTEGER NOT NULL
);

-- 任务(DAG 节点)
CREATE TABLE orch_run_tasks (
  id              TEXT PRIMARY KEY,           -- rtask_{uuidv7}
  run_id          TEXT NOT NULL REFERENCES orch_runs(id) ON DELETE CASCADE,
  title           TEXT NOT NULL,
  spec            TEXT NOT NULL,              -- 该单元工作的规格描述(发给 worker)
  task_profile    TEXT,                       -- JSON: 推断的任务画像(见 §6)
  status          TEXT NOT NULL,              -- pending|ready|assigned|running|needs_review|done|failed|skipped
  conversation_id INTEGER,                    -- worker 会话(conversations.id, ON DELETE SET NULL 由应用层维护)
  output_summary  TEXT,                       -- 产物摘要(回流下游)
  output_files    TEXT,                       -- JSON: 产物文件路径数组
  attempt         INTEGER NOT NULL DEFAULT 0,
  tokens          INTEGER,
  graph_x         REAL,                       -- 画布坐标(用户可拖)
  graph_y         REAL,
  created_at      INTEGER NOT NULL,
  updated_at      INTEGER NOT NULL
);

-- 依赖边
CREATE TABLE orch_run_task_deps (
  blocker_task_id TEXT NOT NULL REFERENCES orch_run_tasks(id) ON DELETE CASCADE,
  blocked_task_id TEXT NOT NULL REFERENCES orch_run_tasks(id) ON DELETE CASCADE,
  PRIMARY KEY (blocker_task_id, blocked_task_id),
  CHECK (blocker_task_id <> blocked_task_id)
);

-- 分派记录
CREATE TABLE orch_assignments (
  id          TEXT PRIMARY KEY,               -- asg_{uuidv7}
  task_id     TEXT NOT NULL REFERENCES orch_run_tasks(id) ON DELETE CASCADE,
  member_id   TEXT NOT NULL,                  -- fleet_members.id(快照内软引用)
  score       REAL,                           -- Router 打分
  rationale   TEXT,                           -- 自然语言理由(展示给用户)
  source      TEXT NOT NULL,                  -- 'auto'|'override'
  locked      INTEGER NOT NULL DEFAULT 0,     -- 锁定后再规划不动
  created_at  INTEGER NOT NULL
);
```

- 删除 Run 依赖 FK ON DELETE CASCADE 一次清掉 tasks/deps/assignments；删 task 前应用层负责处理其 worker 会话。
- 实时事件：新 `OrchestratorEventEmitter`（`run.planUpdated` / `task.statusChanged` / `task.assigned` / `task.output` / `run.statusChanged` / `run.completed`）走现有 WebSocket，ts-rs 导出（i64 字段 `#[ts(type="number")]`）。
- 仓库：每表 `I*Repository`+`Sqlite*Repository`（模板 `nomifun-webhook`）。

---

## 6. 路由 / 能力匹配（自动分派 + 可覆盖）

### 6.1 成员能力画像 CapabilityProfile（落 `fleet_members.capability_profile`）
合成自：
- `AgentRegistry` handshake：`prompt_capabilities`（image/audio）、`mcp_capabilities`、按 backend 推断的编码倾向。
- provider 模型能力标签：`text|vision|function_calling|reasoning`（来自 `providers.capabilities`）。
- 用户填写：强项标签（如「编码/研究/写作/视觉/长上下文」）、成本档（economy/standard/premium）、速度档。

```jsonc
// CapabilityProfile
{
  "strengths": ["coding", "long_context"],   // 用户/推断标签
  "modalities": ["text", "vision"],
  "tools": true,                              // 支持函数调用/MCP
  "reasoning": "high",                        // low|medium|high
  "cost_tier": "premium",                     // economy|standard|premium
  "speed_tier": "standard"
}
```

### 6.2 任务画像 TaskProfile（落 `orch_run_tasks.task_profile`）
规划阶段由主管为每个任务推断：
```jsonc
{
  "kind": "coding",                           // research|coding|writing|analysis|vision|tool|review|...
  "needs_vision": false,
  "needs_long_context": true,
  "needs_high_reasoning": true,
  "bulk": false                               // 廉价批量(倾向 economy 成员)
}
```

### 6.3 Router
- **确定性预筛 + 打分**：硬约束过滤（如 `needs_vision` → 必须有 vision modality；`tools`→必须支持工具），软评分（kind↔strengths 匹配、reasoning 档匹配、`bulk`↔cost_tier 反向偏好、成员 `allowed_task_kinds` 约束、负载均衡）。产出 `成员×任务` 排名 + 分数。
- **主管 LLM 拍板**：把 Top-K 候选 + 分数 + 任务规格交给主管，由其用判断做最终选择（可偏离打分但需给理由）。
- **理由透出**：写入 `orch_assignments.rationale`，画布节点详情展示。
- **用户覆盖**：用户可对任意任务改派/锁定（`source='override'`, `locked=1`）；锁定项在再规划/重打分时不动。

---

## 7. 自主级别与人在环

| 级别 | 行为 | worker session_mode |
|---|---|---|
| **自主** autonomous | 跑到底；IDMM 自动处理决策/权限/failover；无审批 UI（同 AutoWork）。 | yolo |
| **守护** supervised（默认） | IDMM 自动处理安全项；风险/歧义升级为检查点给用户。 | 守护(IDMM 武装) |
| **协同** interactive | 关键闸口审批：执行前批计划（`awaiting_plan_approval`）、逐任务 `needs_review`。 | 交互(审批闸开) |

- 用户始终可：**steer 主管**（复用 steering inbox）、**暂停/恢复/取消** Run（协作式取消，cancel 信号传播到所有活跃 worker）、**编辑/改派/重试**任务。
- IDMM 接入：每条 worker 会话经现有 `ConversationSupervisionHook` 武装；主管会话同样武装。新增 orchestrator 维度的 IDMM seam（若需要 run 级监督，复用 `IdmmHandle::ensure_supervising((kind=orchestrator_run, target_id=run_id))` 范式）。
- **关键不变量**：取消必须让所有 worker 以 `Finish(Cancelled)` 收尾（非 Error），以免 AutoWork/IDMM 误重启（沿用现有取消语义）。

---

## 8. Gateway / 对外暴露

新增 `caps_orchestrator` 域（`crates/backend/nomifun-gateway/src/caps_orchestrator.rs`，3 步契约：register fn + lib.rs mod + build() 调用）：

- `nomi_fleet_list` / `nomi_fleet_get`（读）
- `nomi_run_create`（写）/ `nomi_run_status`（读）/ `nomi_run_result`（读）
- `nomi_run_plan`（写：主管写入 tasks+deps）/ `nomi_run_add_task` / `nomi_run_assign`（写：分派）
- `nomi_run_cancel`（Destructive→Confirm/Deny 按 surface）

权限：读 Read，规划/分派 Write，取消 Destructive，按 DangerTier×Surface 矩阵自动 gate。命名 `nomi_` 前缀、≤42 字符、wire 名 ≤64。

**收编遗留 `nomi_agent_run`**：把单发委派语义并入 Run 模型（一个单任务 Run 即等价于旧 `nomi_agent_run`），逐步弃用旧工具（与 team 移除同期）。

主管的规划/分派工具即住此域——主管作为一个挂了 `caps_orchestrator` + desktopGateway 的 Nomi 会话，通过这些工具操作 Run 状态，调度器观察状态变化并执行。

---

## 9. 前端（编排画布为核心）

### 9.1 侧栏与路由
- **侧栏新 tab**：放进**常用**分组，**紧随「会话」之下**（顺序：会话 → 智能编排 → 桌面伙伴），高地位。
- 照搬 `SiderModelHubEntry.tsx`（改名 `SiderOrchestratorEntry`、换 icon、换 i18n key `common.siderSection`/新 nav key），在 `SiderNav/index.ts` re-export，插入 `Sider/index.tsx` 常用组，加 navTo + isActive 匹配，i18n label 双语 + `i18n-keys.d.ts`。
- 路由 `/orchestrator`（在 Router.tsx 加 `withRouteFallback` lazy 路由，ProtectedLayout 下）。

### 9.2 页面结构
ContentSider（二级侧栏）+ 主区 hero：

- **二级侧栏**（`ContentSider` + `useResizableSplit`，独立 storageKey）：三段（工作间列表 / 编队管理 / Run 历史），用 `?section=` 内联态（参照 modelHub 模式 A）或嵌套路由。
- **Hero = DAG 编排画布**（`react-flow`）：
  - 任务节点：状态色（CSS 变量）、分派 agent 头像 + 模型 chip、进度、重试/改派动作。
  - 依赖边、顶部主管节点。WS 实时刷新（`task.statusChanged` 等）。
  - 节点可拖（落 `graph_x/graph_y`）。
  - **点节点 → 右侧滑出该 worker 实时对话**：复用 `ChatLayout`/`NomiChat`/`MessageList`，只读 + 可 steer 模式（`hideAdvancedControls`、`hideSendBox` 视模式；参照 SubagentDrawer 但升级为可交互）。
  - 顶栏：目标、自主级别选择、编队选择、Run 控制（暂停/恢复/取消/fork）、聚合进度 + 成本/token 表（复用 `ContextUsagePill` 风格）。
- **编队管理视图**：组建编队（从已配 providers 选 agent+model、填角色/标签/约束），看每成员能力画像。卡片网格 + `AssistantTagFilterBar` 风格。**同时满足**「对用户清晰配置」+「对 agent 清晰展示信息/编排/生命周期」两个视角。

### 9.3 双视角合一
画布 + 节点详情既是用户视角的编排，也是每个 agent 的生命周期卡（状态机 + 重试/改派 + 转录）。无需为「agent 视角」单独造页。

### 9.4 美感（硬门槛）
- 全 CSS 变量主题；`react-flow` 主题化对齐既有视觉语言（节点/边/背景皆用 `var(--*)`）。
- 走 frontend-design 技能打磨画布与编队卡。
- 遵守既有约定：`icon-park` 具名导入不起别名、`useArcoMessage`、无 UnoCSS button reset（用 `<div onClick>`）、Arco popover 清零内边距、`isDesktopShell()` 而非 `isElectronDesktop()`。

---

## 10. 与现有引擎/服务的接线

新域服务 + 路由典型触及 ~6–8 点：
1. 迁移 `018_orchestrator.sql` + `nomifun-db` 内 Row 模型 + `I*Repository`/`Sqlite*Repository`。
2. `nomifun-api-types` DTO。
3. 新 crate `nomifun-orchestrator`（lib/routes/state/service/events，镜像 `nomifun-webhook`+`nomifun-requirement`）。
4. `AppServices::from_config` 加单例（OrchestratorService 需与 agent 工厂/会话服务共享）。
5. `router/state.rs` 加 `ModuleStates` 字段 + `build_orchestrator_state`。
6. `router/routes.rs` merge `orchestrator_routes(state)`（auth 中间件下）。
7. `GatewayDeps` 加 orchestrator_service 字段 + `caps_orchestrator` 注册。
8. 前端：Router 路由 + 侧栏 entry + i18n + protocolBindings。

调度器作为后端持久循环（参照 AutoWork Orchestrator 的 boot-resume + 持久循环 + `(kind, target_id)` 键），重启后对账未完成 Run。

---

## 11. 旧 team 移除（交付时）

分阶段，**先建后拆**：
1. 实现期：搬运可复用零件到新 crate——任务依赖边表模式、崩溃/不活跃检测、agent==会话——但**不**依赖 team crate。
2. 交付期：
   - 新增迁移（如 `0XX_drop_team.sql`，append-only，DROP `teams`/`team_agents`/`mailbox`/`team_tasks`/`team_task_deps`）。
   - 删 `nomifun-team` crate + 工作区 Cargo 成员 + `build_team_state` + 路由挂载。
   - 删前端 `ui/src/.../multiAgent/*`、`team/teamTypes.ts`、`teamMapper.ts`、`ipcBridge.team`、`ChatLayout` 内 `AgentStatusStrip` 挂载。
   - 删 Guide MCP（`nomifun-team/src/guide/*`）与相关 `nomi_create_team`。
   - 全工作区编译 + 测试回归确认无残引用。

---

## 12. 测试（全链路，硬要求）

- **Rust 单测**：Router 打分（硬约束过滤 + 软评分 + 锁定项不动）、调度器 DAG 执行（就绪判定、并发上限、依赖解除、动态加任务）、状态机转移、取消传播。
- **Rust 集成测试**：mock agents 跑一个完整 Run（多任务 DAG、并行、分派、产物回流、完成聚合），模板 = team 的 `scheduler_integration.rs`。
- **Gateway**：`caps_orchestrator` 注册不变量（命名/长度/权限矩阵/计数 floor）。
- **前端**：`npm run typecheck` 归零（本机无 vitest；不新增前端单测文件）。
- **真机全链路**（验收门槛，参照外部能力 Task 8 真机范式）：起 app → 配 provider/编队 → 发起一个真实多 agent 目标 → 观察画布实时更新 + 各 worker 转录 + 自动分派理由 → 验证真并行 + 依赖解除 + 改派/锁定 + 暂停/取消 + 完成聚合。截图留证。

---

## 13. 实施分期（一份 spec，分期落地）

| 期 | 内容 |
|---|---|
| **P0** | 域模型 + crate + 迁移 018 + repos + 编队/工作间 CRUD + 编队管理 UI + 侧栏 tab + 路由 |
| **P1** | Run 生命周期 + 主管规划(`nomi_run_plan`) + DAG 持久化 + 调度器(先串行) + 静态画布(react-flow 渲染 DAG) |
| **P2** | 并行 worker 执行(会话substrate) + 实时流式画布(WS) + 节点转录面板 |
| **P3** | 能力 Router(自动分派 + 理由 + 覆盖/锁定) + 自主三级 + IDMM 接入 + steer/暂停/取消/重试 |
| **P4** | `caps_orchestrator` + Remote 暴露 + 收编 `nomi_agent_run` |
| **P5** | 移除 team + 打磨 + 全链路真机测试 + 截图验收 |

---

## 14. 锁定的关键不变量（实施时勿破坏）

1. **worker = 真实会话**，复用现有 `AgentInstance`/`ConversationService`/`StreamRelay`/IDMM；worker 会话从主侧栏过滤（`extra.orchestrator_run_id` 标记）。
2. **调度器确定性，主管智能体行为落库为结构化 plan**；不退回 team 的「散文即真相」。
3. **真并行发生在 worker 会话之间**；每会话仍串行单回合（不破坏 TurnClaim）。
4. **取消让所有 worker 以 `Finish(Cancelled)` 收尾**（非 Error），防 IDMM/AutoWork 误重启。
5. **依赖边表 + completion 解除阻塞**（复用 team 唯一做对的部分）。
6. **跨 gateway/Remote 暴露的 id 用字符串前缀；worker conversation_id 本机 INTEGER**。
7. **能力 gate 在 gateway dispatch 集中执行**，handler 不自查权限。
8. **不为兼容 team 打任何补丁**；team 交付时彻底移除。
9. 前端全 CSS 变量主题；`react-flow` 主题化；遵守 icon-park/useArcoMessage/isDesktopShell 等既有约定。
10. 迁移 append-only；ts-rs i64 字段 `#[ts(type="number")]`。

---

## 15. 开放问题
（设计阶段已与用户对齐核心 4 forks + 名称/侧栏位置；其余默认已确认接受。本节留空，待用户审阅本文档后若有补充再记。）
