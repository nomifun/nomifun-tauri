# P6 — 编排可见性与控制（验收期优化）实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development。Steps `- [ ]`。

**Goal:** 解决"开了自动编排却看不到 DAG / 不知道用了哪些 agent / 不知道每个 agent 配置 / 管理弱"。让编排**全程可见且可控**：多 agent 默认 `interactive`(主管出团队+DAG→用户审批→才跑);会话内常驻**编排状态条**(各状态可见+一键开 DAG);Run 起即**自动展开**右栏 DAG;**每 agent 可查配置**(角色/模型/人设/技能/状态)+管理(改派/锁定/steer/批准)。

**Spec:** `docs/superpowers/specs/2026-06-27-conversation-native-orchestration-redesign.md` §7(自主级别/审批)、§9(DAG 右栏)。用户定案:默认 interactive 审批。

## Global Constraints
- 复用既有:interactive 审批流(P3b:awaiting_plan_approval + RunDetailHeader 批准钮)、DagCanvas/DagRailTab/WorkerTranscriptPanel/useRunLive、workspace toggle 事件(workspaceEvents.ts)。
- 后端禁 cargo fmt;只跑触碰 crate;app 必编过。前端 typecheck0+build;禁 any/ts-ignore;icon-park 无别名;`<div role=button>`;Arco useArcoMessage;CSS 主题变量;**UI 必须漂亮**(硬门槛,用户已多次把关)。
- 不破坏 P1-P5/orchestrator_run_e2e/run 生命周期/IDMM。**禁合并 main**(已 merge main 进本分支=反向同步,OK)。

## File Structure（已勘察）
- 后端:`nomifun-gateway/src/caps_orchestrator.rs`(nomi_run_create 默认 autonomy=interactive) 或 `nomifun-orchestrator/src/run_service.rs`(create_adhoc 默认);可能 `routes.rs`(approve 已存在)。
- 前端状态条:新 `ui/src/renderer/pages/orchestrator/RunDetail/OrchestrationStatusStrip.tsx` + 挂载于会话(ChatLayout header `headerExtra`/`headerLeading` 或 NomiConversationPanel 顶部);新 hook 派生 lead 状态。
- 前端自动展开:`OrchestrationStatusStrip`/ChatSlider 在 run 出现时 `dispatchWorkspaceToggleEvent`/展开(workspaceEvents.ts)。
- 前端 inspector:`WorkerTranscriptPanel.tsx`(加「配置」段:role/model/persona/skills/status)。
- 审批可见:RunDetailHeader 批准钮已存在;状态条 awaiting 态也给「批准」入口。

---

## Task 1: 多 agent 默认 interactive(审批闸)

**Files:** Modify `nomifun-gateway/src/caps_orchestrator.rs`(create handler);测试内联。

**改动:** `nomi_run_create` 构造 `CreateAdhocRunRequest` 时,`autonomy` 默认 `"interactive"`(不再走 create_adhoc 的 supervised 默认)。这样:用户提交→主管调 nomi_run_create→run `plan()`→状态 `awaiting_plan_approval`(不自动 engine.start)→等用户批准。nomi_run_create 工具返回应包含 `status: "awaiting_plan_approval"` + 提示主管告知用户"已拟定 N 个子任务的团队,待你在编排面板批准"。
- 核对 P3b 的 plan→awaiting_plan_approval 分支对 create_adhoc 路径生效(plan() 按 run.autonomy 决定是否 awaiting)。若 create_adhoc 的 run 也走同一 plan(),则只需传 interactive。
- approve 路由已存在(routes.rs approve_run→approve_plan+engine.start)。确认 adhoc run(workspace_id NULL)approve 不依赖 workspace。

- [ ] **Step 1: 测试(失败优先)** — nomi_run_create(mock conv extra lead+range)→create_adhoc 收到 autonomy="interactive";run 经 plan 后状态=awaiting_plan_approval(非 running);approve 后 running。
- [ ] **Step 2: RED** `cargo nextest run -p nomifun-gateway -p nomifun-orchestrator`。
- [ ] **Step 3: 实现** interactive 默认 + 工具返回措辞 + 核对 approve 对 adhoc 生效。
- [ ] **Step 4: GREEN** + `cargo build -p nomifun-app` + e2e 4/4(注意:既有 e2e 若假设 supervised 直跑,可能需显式传 autonomy 保持;**勿改 e2e 语义**,e2e 自带 autonomy 即可)。
- [ ] **Step 5: 提交** `git commit -m "feat(orchestrator): 多 agent 默认 interactive(主管出计划待用户审批)"`

---

## Task 2: 会话内编排状态条 + 自动展开 DAG（keystone）

**Files:** Create `OrchestrationStatusStrip.tsx` + 一个状态 hook;Modify 会话挂载处(ChatConversation/NomiConversationPanel 或 ChatLayout header)、ChatSlider/DagRailTab(自动展开);i18n。

**状态条行为(常驻于 lead 会话顶部/header):** 派生并显示 lead 编排状态,各态都可见:
- lead 会话首回合处理中且无 run → `主管规划中…`
- run 存在且 `awaiting_plan_approval` → `已拟定 N 个 agent 团队 · 待批准` + **「查看并批准」**按钮(开 DAG + 触发 approve 可达)
- run `running` → `N 个 agent 协作中 · X/Y 完成`(实时)
- run `completed` → `编排完成 · Y 个任务`
- run `failed`/`cancelled` → 对应态
- lead 首回合结束、无 run → `主管直接作答(未拆分)`
- lead 首回合出错/无可用模型 → `未配置可用模型 / 主管未能运行`(给"去配置模型"链接 → /settings/model)
点状态条 → 展开右栏「编排」DAG tab。

**数据来源:** lead 状态 = (a) `conversation.extra.orchestrator_run_id` 有→`useRunLive(runId)` 取 run.status + tasks done/total;(b) 无 run_id→看 lead 会话回合状态(is_processing / last turn 是否出错——勘察会话回合状态 hook,如 useConversation/turn 事件;若取不到细粒度,降级:无 run_id 且非处理中→"未拆分/待提交")。**实现时探会话回合状态 API**;取不到的态可合并降级,但 awaiting/running/completed/no-run 必须区分。

**自动展开:** run 首次出现(orchestrator_run_id 从无到有,或状态进入 awaiting/running)时,`dispatchWorkspaceToggleEvent()`/展开事件把右栏打开到 DAG tab(避免折叠隐藏)。仅自动展开一次(尊重用户后续手动折叠偏好,沿用 useWorkspaceCollapse 偏好键)。

**视觉:** 紧凑横条,主题变量,状态点/图标(icon-park outline 无别名),awaiting 态主色强调 + 批准 CTA。对齐既有 header/控件视觉。漂亮。

- [ ] **Step 1: 实现** 状态 hook + 状态条组件 + 会话挂载 + 自动展开 + i18n + gen:i18n。
- [ ] **Step 2: typecheck** `cd ui && npm run typecheck` → 0。
- [ ] **Step 3: build** `cd ui && bun run build` 绿。
- [ ] **Step 4: 提交** `git commit -m "feat(orchestrator): 会话编排状态条(各态可见+一键开DAG)+Run起自动展开右栏"`

---

## Task 3: 每 agent 配置 inspector + 审批/管理可见

**Files:** Modify `WorkerTranscriptPanel.tsx`(加「配置」段)、RunDetailHeader/DagCanvas(awaiting 态批准 CTA 已存在,确认在 rail embedded 下可用);i18n。

**改动:**
- WorkerTranscriptPanel(点 DAG 节点打开)顶部加**「配置」段**:该 agent 的 角色(role_hint/role)、模型(provider/model)、人设摘要(member.system_prompt 截断)、技能(member.enabled_skills)、状态(task.status)、改派/锁定控件(已存在,理顺展示)。其下保留转录 + steer。让"这个 agent 是谁、用什么、在干嘛"一目了然。
- 确认 awaiting_plan_approval 态下,DAG(rail embedded)能看到拟定的全部节点 + RunDetailHeader 的「批准计划」钮可点(embedded 模式 P2 隐了返回钮但保留 run 控件——确认批准钮在 embedded 保留)。
- 数据来自 OpenTaskPayload(task+assignment+fleetMembers,已含富化字段 description/system_prompt/enabled_skills,P4)。

- [ ] **Step 1: 实现** inspector 配置段 + 审批可见性核对 + i18n + gen:i18n。
- [ ] **Step 2: typecheck0 + build 绿。**
- [ ] **Step 3: 提交** `git commit -m "feat(orchestrator): agent 配置 inspector(角色/模型/人设/技能/状态)+审批可见"`

---

## Task 4: 集成 + 真机冒烟（截图自检）

- [ ] **Step 1:** `cargo build --workspace` 绿 + `cargo nextest run -p nomifun-orchestrator -p nomifun-gateway -p nomifun-db -p nomifun-api-types -p nomifun-app`(e2e 4/4);前端 typecheck0+build。
- [ ] **Step 2: 真机冒烟(controller,截图)** — `nomifun-web --dist --insecure-no-auth`(temp target/_p6_smoke,debug 二进制)。种一个 lead 会话 + awaiting_plan_approval 的 run(带 role 任务/成员快照)+ 一个 running run。验证:①状态条在会话顶部显示对应态(awaiting/running)②点状态条/Run 起 → 右栏自动展开 DAG③点节点 → inspector 显配置段(角色/模型/人设/技能)④awaiting 态可见「批准」⑤零 console error⑥UI 漂亮。**controller 亲自 Read 截图判定美观**(用户对 UI 严格,勿仅靠子 agent)。截图 target/_p6_smoke。
- [ ] **Step 3: 记账 + 总结**(交付 + 用户验收[配 provider 跑真 interactive run])。

## Self-Review（用户 4 点）
**覆盖:** 看不到 DAG→T2(状态条+自动展开);不知哪些 agent→T2(状态条 N agents)+T3(inspector);不知配置→T3(配置段);管理弱→T1(审批闸)+T3(改派/锁定/steer/批准可见)。
**风险:** lead 无 run 的"思考中/未配置模型"态需会话回合状态(T2 探 API,取不到则降级合并);interactive 默认勿破 e2e(T1 e2e 自带 autonomy);自动展开只一次尊重偏好(T2)。
**provider 前提:** 无配置 provider 则 lead 跑不起来——T2 的"未配置模型"态使其可见;真 interactive run 验收需 provider(留用户)。

## Execution Handoff
波次:T1(后端 interactive,sonnet)→T2(状态条+自动展开,opus——keystone+探回合状态)→T3(inspector,sonnet)→T4(集成+冒烟+controller 截图判定,opus)。每任务两阶评审+fix+记账。禁合并 main。
