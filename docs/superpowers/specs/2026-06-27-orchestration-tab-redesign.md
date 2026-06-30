# 智能编排 Tab 重设计（从会话融合回归专用 Tab）

> 取代「会话原生编排」方向（lead 会话 / 状态条 / 右栏 DAG / 会话模型选择器三态）——该方向 bug 重灾。回归一个**专用「智能编排」Tab**：结构化表单快速发起 + 可视化审批/管控。**保留已验证的引擎与可复用 UI**，只移除会话融合胶水。在当前分支 `feat/multi-agent-orchestrator` 就地清理（用户定案）。

**状态**：设计已与用户对齐——重建策略=就地清理(保留引擎)；主管交互=结构化表单+可视化审批(无主管聊天)。

---

## 1. 动机与边界

- 把编排塞进「会话」（lead 会话、会话顶部状态条、右栏 DAG tab、会话模型选择器三态/主管模型、useGuidSend lead 标记）引入了过多 UX 复杂度与边缘 bug。
- 决策：**回归专用「智能编排」Tab**。用户进 Tab 做多 agent 工作，全程不离开 Tab。「会话」回到纯单 agent，**无任何编排痕迹**。
- **引擎不是 bug 源**：`nomifun-orchestrator`（RunEngine/Router/LlmPlanProducer/worker/run_service）、迁移 018-022、DAG/inspector/run-control 组件 已过 1880 测试 + e2e + 评审。**全部保留复用**。
- 在当前分支就地清理（已同步 origin/main，HEAD b7b0f96c）。

## 2. 核心概念（沿用引擎，去掉会话融合层）

| 概念 | 说明 | 变化 |
|---|---|---|
| **Run** | 一次多 agent 执行 | 引擎不变 |
| **Worker** | 每任务 = 一个真实会话(nomi yolo+desktopGateway)，从主侧栏隐藏(`orchestrator_task_id` 过滤) | 不变(仍隐藏;但不再有 lead 会话) |
| **Agent/角色** | = 助手(assistant)，含偏好模型/人设/技能 | 沿用 P4 统一 |
| **模型范围** | 单一/自动/范围 | 沿用,但移到 **Tab 表单**(不在会话) |
| **自主级别** | interactive(默认,审批闸)/supervised | 沿用 |
| **~~lead 会话~~** | ~~会话即主管~~ | **移除**(改 Tab 表单直建 run) |

## 3. 「智能编排」Tab（UX，主线）

**布局：master-detail（在 Tab 内）**
```
┌ 智能编排 Tab ───────────────────────────────────────────┐
│ [左] Run 列表           │ [主] 选中 Run 视图               │
│  ＋ 新建 Run            │  ┌ 头部: 目标·状态·X/Y·控制按钮 ┐│
│  · Run(待批准/跑中/完成)│  │ [批准计划][暂停][恢复][取消] ││
│  · …历史               │  ├ Agent 花名册: 角色·模型·状态 ┤│
│                        │  ├ DAG 画布(节点=任务/agent) ───┤│
│                        │  │  点节点→右侧 inspector       ││
│                        │  └ inspector: 配置/转录/控制 ───┘│
└─────────────────────────────────────────────────────────┘
```

**新建 Run 表单（快速发起，结构化）**：
- 工作路径（文件夹选择，可空=临时）
- 需求（多行文本）
- 模型范围（单一 / 自动 / 范围多选）——模型编排
- 角色（可选：钉选若干助手作角色池；默认自动=引擎从启用助手+范围内挑，沿用 P4）
- 自主级别（默认 **interactive 审批**；可选 supervised 直跑）
- 「开始编排」→ 直建 run → 落到主视图

**Run 主视图**：
- **头部**：目标 + 状态徽标(待批准/规划中/运行中/已完成/失败/取消/暂停) + X/Y 进度 + 控制按钮(按状态:批准计划/暂停/恢复/取消)。
- **Agent 花名册**：横向条，每个 agent 一张小卡(头像/角色 · 模型 · 状态点)，点击=选中该节点。让"用了哪些 agent"一目了然。
- **DAG 画布**：react-flow，节点=任务(角色·模型·状态)，边=依赖。点节点→inspector。
- **inspector（点节点/花名册项）**：配置段(角色/模型/人设/技能/状态 + 匹配理由) + 控制(改派模型·锁定·steer) + 该 agent 只读转录(worker 会话)。
- **沉淀**：Run 完成后 RolePrecipitationPanel(建议把临时角色存为助手)——沿用 P5。
- 实时：`useRunLive` + orchestratorEvents WS。

## 4. 可见性 / 配置 / 控制（用户硬要求逐项落点）

- **每 agent 工作可见性**：DAG 节点状态 + 花名册 + inspector 转录(看该 agent 在干嘛)。
- **配置灵活性**：表单(模型范围/钉选角色) + inspector(逐任务改派模型/锁定)。
- **便于用户编排**：表单发起 + 画布可视化审批/改派 + 控制按钮。
- **便于模型编排**：`caps_orchestrator`(agent 经 MCP 创建 run)沿用、适配为显式参数。
- **状态跟进 + 控制**：实时状态(WS)、X/Y 进度、approve/pause/resume/cancel/steer/reassign 全在 Tab 可达。

## 5. 后端（复用 + 适配）

**复用（不改）**：`RunService::create_adhoc`、`LlmPlanProducer`(描述驱动分解)、`Router`、`RunEngine`(含 interactive→awaiting_plan_approval 闸 + run_loop awaiting gate)、`ConversationWorkerRunner`、pause/resume/cancel/steer/reassign、迁移 018-022、worker 主侧栏隐藏过滤(`orchestrator_task_id`)。

**适配**：
1. **新增 REST 路由 `POST /api/orchestrator/runs/adhoc`** → `RunService::create_adhoc`（Tab 表单经此直建 run）。当前 create_adhoc 只被 caps 调用;需一个 Tab 面向的受保护 REST 端点 + `ipcBridge.orchestrator.runs.createAdhoc`。请求体 = `{ goal, work_dir?, model_range, pinned_roles?, autonomy?, max_parallel? }`(CreateAdhocRunRequest 已存在,workspace_id 已可空)。autonomy 默认 interactive。
2. **`caps_orchestrator.nomi_run_create` 适配回显式参数**：不再读调用会话的 extra(lead-conversation 概念移除);改为工具入参 `{ goal, model_range?/autonomy? }`(model_range 缺省=自动/全部启用)，agent 经 MCP 显式发起。`nomi_run_status`/`nomi_run_result` 不变。
3. **不再写 lead 会话 extra.orchestrator_run_id**（lead 概念移除）。worker 仍写 `orchestrator_run_id`+`orchestrator_task_id`(隐藏过滤靠 task_id,不变)。

## 6. 前端（建 Tab + 拆胶水）

**建/改**：
- `pages/orchestrator/index.tsx`：从"只读 Run 历史库"升级为 master-detail 编排主页(Run 列表 + 新建 Run 表单入口 + Run 主视图)。
- 新增 `NewRunComposer.tsx`(表单)、`AgentRoster.tsx`(花名册条)。复用 `DagCanvas`/`RunDetailHeader`/`WorkerTranscriptPanel`/`RolePrecipitationPanel`/`useRunLive`/`useOrchestratorData`(listMine)/`RunHistory`。
- `ipcBridge.orchestrator.runs.createAdhoc`(POST /api/orchestrator/runs/adhoc)。
- 模型范围控件 + 角色钉选 + 自主级别：在表单内(复用模型列表 hook useModelProviderList / 助手列表 assistants.list)。

**移除/回退会话融合胶水**：
- 删 `OrchestrationStatusStrip.tsx`、`DagRailTab.tsx`、`useOrchestrationStatus.ts`、`GuidOrchestrationMode.tsx`。
- `ChatSlider.tsx`：去掉「编排」extraTab(orchestration-dag)。
- `ChatConversation.tsx`(NomiConversationPanel)：去掉状态条挂载。
- `useGuidSend.ts`：去掉 lead 标记 + model_range 注入(会话创建回到普通 nomi)。
- `GuidModelSelector.tsx` + `useGuidModelSelection.ts`：**回退为单选**(去三态/主管模型/leadLabel/leadHint);`GuidPage.tsx`：去 orchestrationModeNode + 隐藏/主管模型逻辑。
- `workspaceEvents.ts`：去 `WORKSPACE_SELECT_TAB_EVENT`(仅右栏 DAG 用过) + `WorkspaceRailBody` 对应监听。
- i18n：清 guid.orchestration.* / guid.modelSelector.lead* / orchestrator.status.* 等死键 + gen:i18n。
- **保留** worker 隐藏过滤(orchestrator_task_id) —— worker 会话仍不该进主侧栏。

## 7. 不变量（实施勿破）

1. 引擎只吃 `fleet_snapshot`，快照驱动;create_adhoc/plan/Router/engine/worker 不改逻辑。
2. worker=隐藏会话(orchestrator_task_id 过滤保留);**不再有 lead 会话**。
3. 角色=助手(P4);模型描述驱动选择(P3);interactive 默认审批(P6 Task1 引擎闸保留)。
4. 「会话」页回到纯单 agent,无编排控件。
5. 既有 1880 测试 + orchestrator_run_e2e 4/4 不回归。
6. 禁合并 main(已反向同步 OK);禁 cargo fmt;前端 typecheck0+build;UI 必须漂亮。

## 8. 分期（保持构建始终绿：先建后拆）

- **P1 — 后端适配**：REST `POST /runs/adhoc` + ipcBridge.createAdhoc;caps nomi_run_create 改显式参数(去会话 extra 读取);去 lead extra 回写。引擎/测试不回归。
- **P2 — 建 Tab**：index.tsx master-detail + NewRunComposer + AgentRoster + Run 主视图(复用 DagCanvas/RunDetailHeader/inspector/useRunLive)。Tab 端到端可发起→审批→跑→看→控。（此时会话融合胶水仍在,但 Tab 已自洽。）
- **P3 — 拆胶水 + 会话回退单选**：删状态条/右栏 DAG tab/三态选择器/lead 标记;会话模型选择器回单选;清死键。typecheck0 全清悬挂。
- **P4 — 集成 + 真机冒烟 + 全分支评审**：build --workspace + 触碰 crate 测试 + e2e 4/4 + 前端;真机冒烟(Tab 发起→DAG→审批→inspector→控制;会话回到单 agent 无编排);controller 截图判美;最终评审。

## 9. 测试策略

- 后端:create_adhoc REST 路由测试(受保护层,归属);caps 显式参数测试;引擎契约回归(orchestrator_run_e2e 4/4)。
- 前端:typecheck0 + build;真机冒烟(种 awaiting/running run → Tab 主视图渲染 DAG/花名册/inspector/审批;会话页无编排控件)。
- 真·LLM 编排(发起→规划→审批→跑)需 provider 真跑,留用户验收;CI mock 证 seam。

## 10. Carry-forward

- legacy REST create_run/fleet/workspace 路由+service 仍在(P5 起死代码),本次可顺带清或留;
- 对外 Remote 已 `deny_on(Remote)` 编排 caps(保留);user_id 归属(adhoc REST 路由按受保护层带 user)。
