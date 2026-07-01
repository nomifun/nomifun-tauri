# 伙伴会话归档 + 通用 Subagent 智能编排 · 设计

日期：2026-07-02
状态：草案（自主实施中；关键决策已在文中标注「决策」与「待用户确认」，供醒来复核）
分支：`feature/partner-memory-and-orchestration`

> 本文件是对用户 `/goal` 愿景的架构落地设计。愿景两条：
> 1. **会话（Session）** 用完即删，无长期存储/上下文压力；
> 2. **伙伴（Companion）** 长期使用、期望长期记忆 + 能力自动进化，靠外挂知识库获得超大知识。
>
> 待解决的**首要问题**：伙伴当前的**数据上下文压力**——简单对话成本贵、压缩压力大、无法承担高密度复杂工作。
> 用户方案：① 按「天 + 动态会话窗口（30min 无动态）」归档伙伴聊天，压缩进按天分区的记忆库（支持"去年今日"）；② 伙伴（及普通会话）可驱动 subagent，主 agent 只调度/留总结、保持干净上下文，子 agent 用完即弃；默认用户显式驱动，或开"智能编排"（默认驱动）。

---

## 0. 结论先行（探查后的关键事实与设计定调）

深度探查（5 路并行 + 源码核验）后，现有架构的关键事实：

1. **会话与伙伴是同一套机制**：同一 `conversations`/`messages` 表、同一 `ConversationService`、同一 nomi 引擎。伙伴 = 一条 `type='nomi'` 会话 + `extra.companionSession/companionId` 标记 + `companion_threads` 绑定 + 独立 `companion/shared/memory.db` + factory 注入的人格/记忆 hook。伙伴聊天已并入「会话」侧栏「桌面伙伴」分组（`2026-07-01-companion-chat-into-sessions`）。
2. **伙伴只有恰好一条无限增长的会话线程**（`companion_threads` 唯一索引保证每伙伴仅一条活跃线程，所有 IM 渠道汇入）——**这正是上下文/成本压力的根因**。nomi 引擎从**自身持久化的 `Session`**（引擎 transcript）恢复上下文，`messages` 表只是 UI 可见记录；引擎自带 micro→auto(167k)→emergency(197k) 压缩，但**跨会话无重置**，故长期只增不减。
3. **伙伴记忆系统 B 已存在**（`companion_memories`：6 类 kind + 指数衰减 + active/archived 状态 + scope）；`learner` 60s tick 从**已按天分区**的 `events/YYYYMMDD.jsonl` 蒸馏事实入库；`build_companion_system_prompt` 建会话时注入记忆快照。**但无"按天分区的会话归档"，也无"会话窗口"概念**。
4. **多 Agent 编排底层已成熟**：`nomifun-orchestrator`（lead 无状态一次性 LLM + 隔离 worker 会话，主上下文天然干净）、`nomi_run_create` MCP 工具（仅桌面）、会话原生 UI（右栏 tab + 悬浮画布 + 内容投射）、`RunDecisionFeed`。会话原生编排 v2 已批准并落地。**缺"智能编排"持久化开关 + 伙伴作为 lead 的明确接线。**
5. **`clear_context`（`ConversationService::clear_context`）**：清空引擎历史+压缩状态并**持久化空 session**，**保留 `messages` 表可见记录**。这是归档后"重置实时上下文"的最优低风险机制——保住 conversation_id → IM 路由 / 侧栏 / 单会话不变式全不破。

### 设计定调

- **两个需求的共同本质是「上下文卫生（context hygiene）」**：
  - 需求 2（subagent 编排）保持**单轮上下文**干净——重活外包给一次性 worker，主 agent 只留总结。
  - 需求 1（会话归档）保持**跨会话上下文**有界——按天/空闲滚动窗口 + 压缩成可检索的日记忆。
  - 二者合力才能让"伙伴承担高密度复杂工作"。
- **需求 2 底层已成熟**：本设计只做"开关持久化 + 伙伴接线"，**不重造、不碰引擎/调度/锁不变式**。
- **需求 1 是净新增核心**：自包含在 `nomifun-companion` crate + 伙伴 UI，风险面小；唯一触及引擎的点是复用**已有且经测试**的 `clear_context`。
- **不做补丁式历史债设计**：会话窗口作为一等概念建模（`companion_session_windows`），而非往 `companion_memories` 上打补丁列。
- **零后端破坏性改动优先**：新增表、新增方法、config 门控；既有 learner/记忆/聊天/编排路径不改语义。

---

## 1. 需求一：伙伴会话窗口归档（Companion Session-Window Archiving）

### 1.1 核心概念：会话窗口（Session Window）

把伙伴的会话生命切成**窗口**。一个窗口 = 一段连续的会话（从上次重置到下次归档）。窗口**空闲 ≥ 阈值（默认 30min）即关闭**：其消息被 LLM 压缩成一条**日摘要（digest）**（按窗口起始日分区），随后**重置实时上下文**（`clear_context`）开启新窗口。

- **实时上下文永远只含当前窗口** → 有界、小、便宜；可承担高密度工作。
- **长期连续性靠注入 digest + 长期记忆维持**（语义连续，而非原始 transcript 连续）——这正是用户所说的"通过这个方案**保持伙伴的上下文**"。
- **可见聊天记录（`messages` 表）保留**（像 IM 聊天记录），`clear_context` 不删；引擎不再"看到"旧消息，但用户仍可回滚查看。

> **与"会话用完即删"的统一**：伙伴的每个窗口本质是一段 ephemeral 会话，归档进记忆后即"用完"；伙伴的长期身份活在**记忆库（按天 digest + 蒸馏事实）**里，而非巨型 transcript。这与平台双模型（会话即弃 / 伙伴长记）自洽。

### 1.2 触发规则（天 + 动态窗口）

统一规则（既满足用户原话，又达成上下文削减目标）：

> **一个窗口在空闲 ≥ `idle_minutes`（默认 30）时关闭并归档；digest 按窗口的「起始本地日」分区。**

- ✅「跨天期间还在会话状态中则不归档」：活跃窗口（最近 <30min 有动态）**永不被切**，即使跨越午夜。
- ✅「30min 无动态再启动归档」：空闲触发关闭。
- ✅「跨天超出的部分仍属昨天」：digest 用**起始日**做分区键。
- ✅「理论上每天归档」：典型会话 ≤ 一天，空闲触发的自然粒度≈每日；同一天多段会话（上午/晚上）→ 多条 digest，同 `day`，检索按天聚合。
- 空窗口（无用户消息 / 内容太少）**跳过归档**，只重置 boundary，不烧 LLM。

**时区口径**：与 `collector::day_file_name` 一致（本地时区），`created_at` 存 ms epoch UTC，分区键 = `format_local_day(started_at)`。

### 1.3 数据模型（`companion/shared/memory.db`）

新增一张一等表（迁移：`SCHEMA` const 加表 + `STORE_VERSION` 4→5 + `migrate_v4_to_v5` 补建，遵循既有 `PRAGMA user_version` 阶梯，`BEGIN IMMEDIATE` 原子、preflight 幂等）：

```sql
CREATE TABLE IF NOT EXISTS companion_session_windows (
  id                TEXT PRIMARY KEY,          -- csw_<uuidv7>
  companion_id      TEXT NOT NULL,
  conversation_id   TEXT NOT NULL,             -- 承载该窗口的会话
  session_day       TEXT NOT NULL,             -- YYYYMMDD（本地，起始日；分区键）
  started_at        INTEGER NOT NULL,          -- ms epoch UTC
  last_activity_at  INTEGER NOT NULL,          -- 每条消息更新
  closed_at         INTEGER,                   -- NULL = open
  status            TEXT NOT NULL DEFAULT 'open', -- open | archived | skipped
  message_count     INTEGER NOT NULL DEFAULT 0,
  boundary_ts       INTEGER NOT NULL,          -- 窗口起点：只归纳 created_at>boundary_ts 的消息
  digest            TEXT,                       -- 压缩后的日摘要（markdown）
  highlights        TEXT,                       -- JSON：话题/决策/情绪/待办
  token_estimate    INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_csw_companion_day ON companion_session_windows(companion_id, session_day);
CREATE INDEX IF NOT EXISTS idx_csw_status ON companion_session_windows(companion_id, status, last_activity_at);
```

**决策：新表而非扩 `companion_memories` 列**。理由：window 是生命周期实体（open→archived），digest 是"按天叙事"访问模式（"去年今日"= 同 MM-DD 跨年查询），与 `companion_memories`（原子事实 + 衰减 + 语义 recall）是**互补的两层**，混一张表会概念冲突、破坏既有 injection SQL。

**两层记忆并存、职责分明、互不重复处理**：
- `companion_memories`（既有）：原子事实/偏好，由 **learner 从 events 蒸馏**，语义 recall。**不改。**
- `companion_session_windows`（新增）：按天叙事 digest，由 **archiver 从窗口消息压缩**，按天回看 + "去年今日"。
- 二者数据源不同（events vs 窗口消息）、写入方不同（learner vs archiver），无双重处理。

### 1.4 归档器（Archiver）与调度

新模块 `nomifun-companion/src/archiver.rs`，**复用 `learner` 已有的 LLM seam（`CompanionCompleter` trait）与 60s tick 基础设施**（不新建 provider 抽象）。

**扫描流程（每 tick，config 门控 `archive.enabled`）**：
1. 对每个伙伴，取/建其 `open` 窗口（起点 = 当前活跃线程 + 上次 boundary_ts）。
2. 从 `messages` 表取 `conversation_id = ? AND created_at > boundary_ts` 的消息，更新 `last_activity_at`/`message_count`。
3. 若 `now - last_activity_at >= idle_minutes*60_000`：
   - 内容不足（无用户消息 / < 阈值 tokens）→ status=`skipped`，boundary_ts 前移，开新窗口。
   - 否则 → LLM 压缩成 digest（复用 `prompt.rs` 蒸馏范式，新增 `ARCHIVE_SYSTEM` + `build_archive_prompt`）→ 写 `digest/highlights/token_estimate`，status=`archived`，`closed_at=now`。
   - **重置实时上下文**（见 1.5）。
   - 开新窗口（新 boundary_ts = 关闭时刻）。

**决策：archiver 与 learner 独立**（各自 run_lock），但**共享 tick 循环载体**（可在 learner tick 尾部调用 archiver.sweep，或独立 `spawn`）。倾向**独立 `spawn` 的 sweep 循环**，隔离故障域（archiver LLM 失败不拖累 learner，反之亦然）。

**LLM 失败/无模型**：与 learner 同策略——无模型跳过；provider 失败保留窗口下轮重试；解析失败重试有限次后放弃该窗口（前移 boundary，避免烧 token）。

### 1.5 重置机制（durable context reset）

**决策：复用 `ConversationService::clear_context`（保留 `messages`、清空引擎并持久化空 session）。**

durability 关键：`clear_context` 只对**运行中的 agent**重置活引擎；若 agent 未加载，需先 **warmup**（`initialize_agent` 预热，service.rs:2390 附近已有预热路径）再 `clear_context`，确保引擎持久化 session 被清空、下次重建不 resume 旧历史。

- archiver 需一个到 `ConversationService`（或其 `clear_context` + 预热能力）的 seam（依赖注入 `Arc<dyn ...>`，避免 crate 环——参照 `nomifun-requirement` 的 `IdmmHandle` trait 模式）。
- **不新建会话、不删 `companion_threads` 行、不动 `channel_chat_id`** → IM 路由 / 侧栏「桌面伙伴」/ 单会话不变式全不破。

**待用户确认**：可见聊天记录默认**保留**（IM 式历史，分页加载）。若你更想"每窗口后连可见记录也清空"（更极致的 fresh start），改用 `clear_messages`（会丢 transcript，但 digest 已存）。默认取**保留**。

### 1.6 检索与注入

**注入（`build_companion_system_prompt`）**：现有注入 `memories_for_injection` 快照，追加两段（受同一 `MEMORY_CHAR_BUDGET` 预算）：
- **最近 N 条 digest**（按 day desc，默认 3~5 条）：给新窗口以连续性（"我们昨天聊过…"）。
- **"这一天"回响**（可选、克制）：若存在同 MM-DD 的往年 digest，注入一句"（去年今日你…）"。

**Recall 工具**：给 `CompanionMemorySink` 加 `recall_days(since?, until?, day?)`（查 digest），让用户问"去年今日/上周三我们聊了啥"时伙伴能显式检索。与既有 `recall_memories`（查事实）并列。

**注入时机**：新窗口的系统提示在 `create`（或 warmup 重建）时重算，自然带上最新 digest；无需改 per-turn contributor（如需最鲜可加一个 archive contributor，但 v1 用建会话快照足够）。

### 1.7 config

`SharedLearnConfig`（或新增 `SharedArchiveConfig`）加：
```
archive.enabled: bool = true
archive.idle_minutes: u32 = 30
archive.inject_recent_days: u32 = 3
archive.retention_days: u32 = 0  # 0=永久保留 digest；>0 时清理超期原始 messages（digest 恒留）
```
门控保证：`enabled=false` 时 archiver 完全 no-op，伙伴行为 == 现状。

---

## 2. 需求二：通用 Subagent 智能编排 + 「智能编排」开关

### 2.1 现状复用（不重造）

- 任意 `nomi` 会话可成 run 宿主（lead）：`extra.orchestrator_role=="lead"` → factory 注入 `LEAD_ORCHESTRATOR_PROMPT` + `nomi_run_create/status/result`；`caps_orchestrator`（仅桌面）扇出隔离 worker；lead 无状态、只读压缩投影 → 主上下文干净。
- 伙伴会话是桌面 nomi 会话 + `desktopGateway:true` → **技术上已能调 `nomi_run_create`**。
- 前端会话原生编排 v2 呈现层已就绪（右栏预览 + 悬浮画布 + 内容投射 + `RunDecisionFeed`）。

### 2.2 缺口与设计

**A. 「智能编排」持久化开关（默认驱动）**

用户语义解析：默认 = 用户**显式**要求才驱动 subagent；另可**开启"智能编排"**，开启后 agent **自主判断**复杂任务并扇出（默认驱动）。

**决策：把"智能编排"建模为一个 session capability**（与 `cron / AutoWork / IDMM` 并列，`sessionCapabilityItems.tsx`），即**每会话开关 + 全局默认**：
- 全局默认：新增 config key `nomi.autoOrchestration: boolean`（默认 `false`，opt-in）。
- 每会话覆盖：会话行能力图标里的「智能编排」开关，写 `conversation.extra.autoOrchestration`。
- **开启效果**：该会话被视为 lead-capable → factory 注入 lead 能力（`nomi_run_create` + 精简 lead 指引：复杂任务可扇出、简单任务直接答）。**关闭**：不自动扇出；但用户仍可显式发起（会话内 composer / 直接命令 agent），保留 Path B。

> 与会话原生编排 v2 的 `orchestrator_role=="lead"` 关系：v2 的 lead 标记用于"已有一个 run 挂在该会话"。本开关用于"允许/鼓励该会话主动发起 run"。两者正交但可统一：开启智能编排 → factory 在组装 system prompt 时追加 lead 能力（无论当前是否已有 run）。**不改 v2 的 run↔会话链接、过滤键、WS 镜像等不变式。**

**B. 伙伴作为 lead 驱动 subagent**

- 伙伴 factory（`factory/nomi.rs` companion 分支）在 `autoOrchestration`（伙伴级或全局）开启时，同样追加 lead 能力段落到人格 system prompt。
- 伙伴的"主 agent 干净上下文"诉求与归档天然协同：重活走 worker、主线程只留 `nomi_run_result` 总结 → 窗口 digest 更小更干净。
- **约束**：`caps_orchestrator` Remote 硬拒不变（IM 远程伙伴不扇出）；桌面伙伴可扇出。

**C. 普通会话显式驱动**

- 已有 Path B（会话内 `OrchestratorComposer` 发起）；本设计不改，仅确保"智能编排关"时该入口仍可用（显式驱动）。

### 2.3 风险守护（沿用 v2 不变式）

per-run 锁不跨 LLM await；`link_orchestrator_run` 只 merge extra + 广播；侧栏过滤键 `orchestrator_task_id`（lead 只带 `orchestrator_run_id` 可见）；WS 事件两端手动镜像；无 IR/节点图复活；Remote deny。**本需求只加"是否注入 lead 能力"的条件分支，不碰上述任何一处。**

---

## 3. 前端

1. **伙伴记忆按天回看**（需求 1）：`MemoriesTab` 加"时间线/回看"视图切换，或新增 `archive` tab（`COMPANION_TABS`）。数据源：新增 `ipcBridge.companion.listDayDigests({ companion_id, since?, until?, day? })`（读 `companion_session_windows` archived 行）；按 `session_day` 分桶渲染（复用 `OverviewTab` "伙伴日记" + `utils/chat/timeline.ts` 分组）。"去年今日"= 快捷筛选同 MM-DD。点 digest 可展开 highlights，"查看当天原文"→ 只读打开该窗口 conversation（复用 `ReadOnlyConversationView`）。
2. **智能编排开关**（需求 2）：
   - 全局默认：`configKeys.ts` 加 `nomi.autoOrchestration`；`SystemModalContent` 按 `autoPreviewOfficeFiles` 范本加 `<Switch>`。
   - 每会话：`sessionCapabilityItems.tsx` 加「智能编排」项（图标 + 开关 + 说明），写 `extra.autoOrchestration`。
   - i18n：en/zh `settings.json` + `sessionList.json`（或对应命名空间）加文案 → `bun run gen:i18n` → `check:i18n`。
3. **无新状态库**：沿用 SWR + 自研 `createContext` + WS 事件。

---

## 4. 分阶段实施计划

- **Phase 0 · 设计**（本文档）✅
- **Phase 1 · 归档后端**（净新增，低风险，config 门控）：
  1. 迁移：`SCHEMA` 加 `companion_session_windows` + `STORE_VERSION` 4→5 + `migrate_v4_to_v5`（+ 迁移单测，仿 `migrate_v2_to_v3_adds_scope_columns_idempotent`）。
  2. `store.rs`：window CRUD + 按天/去年今日查询（单测）。
  3. `archiver.rs`：sweep 决策（纯函数 `should_archive(window, now, cfg)` 单测）+ digest 生成（复用 `CompanionCompleter`，canned completer 单测）。
  4. `prompt.rs`：`ARCHIVE_SYSTEM` + `build_archive_prompt` + `parse_archive_output`（单测）。
  5. 注入：`build_companion_system_prompt` 追加 recent digests（单测断言含 digest 段）。
  6. 重置 seam：`clear_context` + warmup 注入（trait，避免 crate 环）。
  7. `SharedArchiveConfig` + service 装配 + spawn sweep。
  8. `recall_days` 工具接入 `CompanionMemorySink`。
- **Phase 2 · 编排开关**：config key + session capability + factory 条件注入 lead 能力（伙伴 & 普通会话）+ 单测（prompt 含/不含 lead 段）。
- **Phase 3 · 前端**：时间线回看 + 智能编排 Switch（全局 + 每会话）+ i18n + typecheck/check:i18n。
- **回归**：见 §5。

每 Phase 结束跑相关 crate `cargo test` + `cargo check --workspace` 保持绿。

## 5. 回归与不可破坏不变式

**必须持续正常**（用户点名）：
- **AutoWork**：`nomifun-requirement` 循环认领需求→注入会话→等回合。归档只作用于伙伴会话的 idle 窗口；AutoWork 目标会话若空闲被归档 `clear_context`，需保证 AutoWork 下一轮注入仍工作（clear_context 幂等、保留会话行）。**验证点**：AutoWork 会话不应被伙伴归档器扫描（归档器只遍历 `companion_threads` 伙伴，AutoWork 普通会话不在其列）。
- **IDMM**：对伙伴/路由会话让路不变（本设计不碰 idmm）。
- **知识库**：绑定/检索/注入不变（不碰 `nomifun-knowledge`）。
- **会话原生编排 v2**：run↔会话链接、过滤键、WS 镜像、per-run 锁不变（只加条件注入）。
- **伙伴既有**：单会话契约、`ensureCompanionSession`、`sync_companion_windows`、IM 折叠、learner/记忆/衰减、桌宠窗口——全不破。

**回归动作**：`cargo test`（companion/conversation/orchestrator/requirement/idmm）；`bun run build` + `typecheck` + `check:i18n`；手动清单（伙伴聊天、AutoWork、idmm 让路、知识检索、编排扇出）。

## 6. 待用户确认 / 假设

1. **窗口关闭后可见聊天记录**：默认**保留**（IM 式）。若要极致 fresh start 用 `clear_messages`（丢 transcript）。
2. **`idle_minutes` 默认 30**（用户原话）；`inject_recent_days` 默认 3。
3. **智能编排全局默认 `false`（opt-in）**：符合"默认显式驱动，开启后自主"。若你想默认就自主，改默认 `true`。
4. **digest 保留策略**：默认永久保留 digest；原始 messages 暂不清理（`retention_days=0`），后续按需加 TTL。
5. **archiver 独立 spawn 循环**（隔离故障域），而非塞进 learner tick。

## 7. 实施状态（2026-07-02 自主实施，分支 `feature/partner-memory-and-orchestration`）

### 已交付并通过测试/构建

**需求 1 · 伙伴会话窗口归档 — 完整闭环**
- 数据模型：`companion_session_windows` 表 + `STORE_VERSION` 4→5 迁移阶梯（迁移单测：新库直生 v5、旧库幂等升级）。
- 存储：窗口 open/touch/close + 按天检索 `digests_in_range` + 去年今日 `digests_on_day_of_year`（`local_day` 本地时区，与 collector 一致）。
- 压缩：`prompt::{ARCHIVE_SYSTEM,build_archive_prompt,parse_archive_output}`（结构化 digest：summary/topics/decisions/todos/mood）。
- 归档器 `archiver.rs`：纯 `decide(idle,user_msgs,chars)` 触发规则 + 全量 sweep（复用 learner `CompanionCompleter` LLM seam；provider 失败重试、parse 失败降级 skip）。
- 重置：`archive_port::ConversationArchivePort`（`list_messages` 取窗口消息 → `warmup`+`clear_context` durable 重置引擎、保留可见记录）。
- 注入：`build_companion_system_prompt` 追加最近 digest（仅本地会话）。
- 配置：`SharedArchiveConfig`（默认 **OFF**、opt-in，与 learn 循环一致；无模型/空窗口跳过）。
- API：`GET /api/companion/companions/{id}/digests`（since/until/on_day/limit）。
- 前端：学习页「会话归档」开关（enabled + idle_minutes）；新增「会话回顾」Tab（按天时间线 + 心情/话题标签 + 「去年今日」筛选）。

**需求 2 · 智能编排 — 伙伴 + 普通会话均完整**
- 伙伴：`SharedCompanionConfig.smart_orchestration`（默认 OFF）：开启后本地伙伴人格提示追加「调度子 agent」能力，复杂大任务用 `nomi_run_create` 派隔离子 agent、伙伴只调度+汇总（工具随既有 desktopGateway；远程 IM 不注入，caps_orchestrator 对 Remote 硬拒）。前端学习页开关。
- 普通会话：全局 `nomi.autoOrchestration`（默认 OFF）→ 工厂纯函数 `is_orchestration_lead` 对非伙伴、非远程的桌面会话注入 `LEAD_ORCHESTRATOR_PROMPT`（仅 prompt；工具本就随后端自动授予的 desktopGateway 提供，不改能力/审批模型）。前端系统设置开关。**零改编排引擎/调度/锁不变式。**

**回归（全绿）**：`nomifun-companion` 206、`nomifun-conversation` 254、`nomifun-requirement`/`nomifun-idmm`/`nomifun-knowledge`/`nomifun-orchestrator` 全过；`cargo check --workspace` 通过；前端 `typecheck`/`check:i18n`/`build:ui` 通过。AutoWork/idmm/知识库/编排未受影响（归档器只遍历伙伴的活跃线程，不碰普通会话/AutoWork 目标；clear_context 幂等、保留会话行）。

### 明确的剩余项 / 后续增强

**普通会话的「智能编排（默认驱动）」自动扇出开关 — 已交付。** 探查修正了此前顾虑：
- `nomi_run_create` 随 desktop gateway 提供，而**后端已给每个本地可信桌面会话自动授予 `desktopGateway`**（`nomifun-conversation/routes.rs` 的 `grant_desktop_gateway`：strip 客户端伪造值后由后端重授，产品目标「任意会话皆可全平台操作」）。故普通桌面会话**本就有** `nomi_run_create`。
- 澄清：yolo 免批准是**所有 nomi 会话的既有产品默认**（`factory/nomi.rs` session_mode 默认 yolo，显式 `session_mode` 仍胜出），与 gateway 无关——故本功能**不触及审批模型、不引入 yolo 回退**。
- 实现：新增全局客户端偏好 `nomi.autoOrchestration`（默认 OFF）。工厂纯函数 `is_orchestration_lead(role, auto_pref, is_companion, is_remote)` = 显式 `orchestrator_role=="lead"` 或（全局开 且 非伙伴 且 非远程）→ 注入 `LEAD_ORCHESTRATOR_PROMPT`（**仅 prompt，不改能力/审批**）。伙伴走各自 `smart_orchestration` 人设提示，远程不参与。工厂单测 `is_orchestration_lead_policy` 覆盖全部分支。
- 前端：系统设置新增「智能编排（普通会话）」开关（`nomi.autoOrchestration`）；zh/en `settings.autoOrchestration(Desc)` 文案。

**可选增强（记录，非阻塞）**：普通会话的**每会话**智能编排开关（当前是全局默认；如需 per-session 覆盖，前端写 `extra.orchestrator_role="lead"` 即可，后端已支持、无需改动）；把编排能力从「全量总管 gateway」解耦出受限档（当前普通会话开启即拥有全平台总管工具，是既有产品默认，非本功能引入）；`recall_days` 工具（伙伴主动检索历史 digest；当前靠注入最近 digest + 回顾 Tab 覆盖）；digest 保留/TTL 清理（当前永久保留）；per-turn digest contributor（当前建会话快照已足够）。

**可选增强（记录，非阻塞）**：`recall_days` 工具（让伙伴主动检索历史 digest；当前靠注入最近 digest + 回顾 Tab 覆盖）；digest 保留/TTL 清理（当前永久保留）；per-turn digest contributor（当前建会话快照已足够）。
