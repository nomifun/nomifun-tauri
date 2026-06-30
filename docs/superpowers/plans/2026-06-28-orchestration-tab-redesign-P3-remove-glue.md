# P3 — 智能编排 Tab 重设计：拆会话融合胶水 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development。Steps `- [ ]`。（经 Workflow 执行：understand→implement(原子移除,typecheck 驱动)→verify。）

**Goal:** 移除把编排塞进会话的全部胶水,让「会话」回到纯单 agent;编排只在专用 Tab(P2 已建)。typecheck 0 清除所有悬挂引用。

**Architecture:** 原子移除 + 回退:删 4 个胶水组件;ChatSlider/ChatConversation/useGuidSend 去编排接入;GuidModelSelector/useGuidModelSelection 回退单选;workspaceEvents 去 SELECT_TAB;清死 i18n。**保留** worker 主侧栏隐藏过滤(orchestrator_task_id)。

## Global Constraints
- 前端 typecheck0(`cd ui && npm run typecheck`)+`bun run build` 绿;**typecheck 0 是硬闸**(清掉每个悬挂引用)。禁 any/ts-ignore。
- **保留** worker 会话主侧栏隐藏过滤(orchestrator_task_id)——worker 仍是隐藏会话,不该进主侧栏。
- 不动后端;不动 P2 的 Tab(orchestrator/);不动引擎。
- 「会话」模型选择器回退为**纯单选**(GuidModelSelector/useGuidModelSelection 去三态)。CreateTaskDialog(第2调用点,本就用单选降级)须仍工作。
- **禁合并 main**。分支 feat/multi-agent-orchestrator。UI:会话页恢复原单 agent 观感(无编排控件)。

## File Structure（移除/回退清单，spec §6）
- **删文件**：`ui/src/renderer/pages/orchestrator/RunDetail/OrchestrationStatusStrip.tsx`、`RunDetail/DagRailTab.tsx`、`RunDetail/useOrchestrationStatus.ts`、`pages/guid/components/GuidOrchestrationMode.tsx`。
- **改 `pages/conversation/components/ChatSlider.tsx`**：去掉 orchestration-dag extraTab(+ DagRailTab/leadRunId 引用)。
- **改 `pages/conversation/components/ChatConversation.tsx`**：去掉 OrchestrationStatusStrip 挂载(NomiConversationPanel)。
- **改 `pages/guid/hooks/useGuidSend.ts`**：去 lead 标记(orchestrator_role)+model_range 注入;nomi 会话创建回普通(session_mode 用 selectedMode,不强制 yolo)。
- **改 `pages/guid/components/GuidModelSelector.tsx`** + `hooks/useGuidModelSelection.ts`：回退单选——去 selectionMode/selectedRange/toggleRangeModel/三态 droplist/主管模型(leadLabel/leadHint)/auto/range body,只留单选 Menu。
- **改 `pages/guid/GuidPage.tsx`**：去 orchestrationModeNode + hideModelSelector/主管模型逻辑;modelSelectorNode 恒渲染单选。
- **改 `utils/workspace/workspaceEvents.ts`** + `pages/conversation/Workspace/WorkspaceRailBody.tsx`：去 WORKSPACE_SELECT_TAB_EVENT + 其监听(仅右栏 DAG 用过;grep 确认)。
- **i18n**：清死键 `guid.orchestration.*`、`guid.modelSelector.lead*`(leadLabel/leadHint)、`orchestrator.status.*`(若仅 OrchestrationStatusStrip 用)+ gen:i18n + check:i18n。
- **保留**：`useConversationListSync.ts`(orchestrator_task_id 过滤)、`sqlite_conversation.rs`(后端过滤)——worker 隐藏不变。

---

## Task 1: 原子移除会话融合胶水 + 会话模型选择器回退单选

**Files:** 上述删/改清单全部;无新测试(以 typecheck0 + build + 手动 grep 无悬挂为验证;会话回退由既有前端无单测,sanity 经 P4 冒烟)。

**方法（typecheck 驱动）：**
1. 先删 4 个胶水文件。
2. grep 全 ui/src 每个被删符号(`OrchestrationStatusStrip|DagRailTab|useOrchestrationStatus|GuidOrchestrationMode|WORKSPACE_SELECT_TAB`)+ 三态符号(`selectionMode|selectedRange|toggleRangeModel|orchestration-dag|orchestrator_role|model_range`(在 guid/会话侧))→ 逐个清除引用。
3. GuidModelSelector/useGuidModelSelection 回退单选:对照 git 历史(三态是后加的)——去掉 selectionMode 三态分支,droplist 只留单选 Menu(provider 分组),触发钮显模型名(去主管模型 label)。useGuidModelSelection 去 selectionMode/selectedRange state + 返回。GuidPage 去 orchestrationModeNode/hideModelSelector,modelSelectorNode 恒渲染。
4. useGuidSend nomi 分支:extra 去 orchestrator_role/model_range;session_mode 回 selectedMode。
5. ChatSlider 去 orchestration-dag extraTab(只留 nomi-session-metrics 等原有)。ChatConversation 去状态条。
6. workspaceEvents/WorkspaceRailBody 去 SELECT_TAB(grep 确认仅 DagRailTab/OrchestrationStatusStrip 用过)。
7. 清死 i18n + gen:i18n + check:i18n。
8. `cd ui && npm run typecheck` 反复直到 0(清每个悬挂);`bun run build` 绿。

- [ ] **Step 1:** 删 4 文件 + 全 grep 清引用 + 三态回退 + useGuidSend/ChatSlider/ChatConversation/workspaceEvents 清理 + i18n 清死键。
- [ ] **Step 2:** `cd ui && npm run typecheck` → 0(清所有悬挂)。
- [ ] **Step 3:** `cd ui && bun run build` 绿 + i18n check。
- [ ] **Step 4: grep 确认无悬挂**：`OrchestrationStatusStrip|DagRailTab|useOrchestrationStatus|GuidOrchestrationMode|WORKSPACE_SELECT_TAB|orchestration-dag` 在 ui/src 零命中(orchestrator/ Tab 内的 DagCanvas/WorkerTranscriptPanel 等保留组件不算)。确认 worker 隐藏过滤(orchestrator_task_id)仍在。
- [ ] **Step 5: 提交** `git commit -m "refactor(orchestrator): 拆除会话融合胶水(状态条/右栏DAG/三态选择器/lead标记),会话回归纯单 agent"`

---

## Self-Review（spec §6）
**覆盖:** 删 4 胶水文件 + ChatSlider/ChatConversation/useGuidSend/GuidModelSelector/useGuidModelSelection/GuidPage/workspaceEvents 清理 + i18n。**保留** worker 隐藏过滤。
**风险:** 单选回退须干净(对照 git 历史三态前的形态);CreateTaskDialog 第2调用点(已用单选降级)不破;SELECT_TAB 删前 grep 确认仅 DAG 用;typecheck0 兜底悬挂。
**保留确认:** orchestrator/ Tab(P2)整套 + DagCanvas/WorkerTranscriptPanel/RolePrecipitationPanel/useRunLive + worker 过滤 不动。

## Execution Handoff
Workflow:understand(1:map 所有胶水触点 + 三态回退目标 + SELECT_TAB 用处 + 单选历史形态)→implement(1 原子移除,free-text 报告,typecheck 驱动)→verify(对抗:无悬挂/会话单选正确/worker过滤保留/build绿/scope)。P4 冒烟:会话页无编排控件 + Tab 正常。禁合并 main。
