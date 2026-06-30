# 多 Agent 智能编排引擎 · P3b 实施计划（自主级别 + 控制 + IDMM 武装）

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development. Steps `- [ ]`.

**Goal:** 实现 spec §7 的人在环与「可控」：Run 三档**自主级别**生效（autonomous=直跑;supervised=IDMM 自动值守 worker;interactive=计划执行前审批闸）；运行中 **pause/resume**（暂停=停止派新 worker、在飞跑完;恢复=继续）；**steer**（向某个在飞 worker 注入消息）；**IDMM 武装 worker 会话**（worker 遇决策/开放问题自动作答,不卡住——「自动智能执行」的兜底）。取消已在 P2 交付。

**Architecture:** RunEngine + RunService 加 autonomy 行为 + paused 状态 + steer 转发;engine 的独立 conv_service 挂 IDMM 监督 hook（or 每 worker ensure_supervising）;新增 run 生命周期端点（approve_plan/pause/resume/steer）+前端控件。

**Spec：** §7（自主三级 + steer/暂停/取消 + IDMM 接入）。承接 P1a 的 Run.autonomy 字段 + P2 的并行引擎/取消。

## Global Constraints
- 三档:**autonomous**=plan 后直接 running 跑到底(worker yolo);**supervised**(默认)=worker yolo + IDMM 武装(自动值守);**interactive**=plan 后 status='awaiting_plan_approval',用户 approve→running(worker 仍 yolo,但计划经人确认)。worker 始终 desktopGateway+yolo(无逐 worker 审批 UI;interactive 的人在环在计划闸,不在每 worker)。
- **pause**:run='paused',engine run_loop 停止 fill(不派新 worker),在飞继续至完成;**resume**:run='running',继续 fill。pause 不取消在飞(取消是 cancel)。无 busy-spin(paused 且无在飞→idle await,不空转)。
- **steer**:向指定在飞 worker 的 conversation 注入(`ConversationService::steer_message`),不改 run 状态。
- **IDMM 武装**:engine 的 conv_service `with_supervision_hook(idmm_manager)`(或每 worker `ensure_supervising`);worker 遇 decision/permission 自动按 IDMM 规则处理。**若 IDMM 接线过于纠缠（拿不到 IdmmManager / 造环），报 BLOCKED,我延后此项**（不阻塞 autonomy/pause/steer）。
- 取消语义不破坏(P2);并行不破坏(P2);确定性调度不破坏。
- 后端禁 cargo fmt;只跑触碰 crate;app 必编过。前端 typecheck0+build;禁 any/ts-ignore;theme vars;useArcoMessage。**禁合并 main**。

## File Structure
- 修改 `nomifun-orchestrator/src/run_service.rs`(approve_plan/pause/resume/steer 方法 + autonomy 在 plan 后的状态决策)、`engine.rs`(paused 状态读取 + run_loop 尊重 paused;steer 转发若在引擎)。
- 修改 `nomifun-orchestrator/src/routes.rs`(POST .../runs/{id}/approve|pause|resume + POST .../runs/{id}/tasks/{task}/steer)。
- 修改 `api-types`(SteerRequest 等若需)。
- 修改 app `build_orchestrator_state`(IDMM hook 注入 conv_service;steer 需 conv_service 已有)。
- 前端:RunDetailHeader 加 approve/pause/resume 按钮 + 节点 steer 输入;ipcBridge.runs.{approve,pause,resume,steer};i18n。

---

## Task 1: 自主级别 + pause/resume + steer（后端 + 端点）

**Files:** Modify `run_service.rs`、`engine.rs`、`routes.rs`、`api-types/orchestrator.rs`;Test 内联。

**Interfaces:**
```rust
impl RunService {
  // plan() 末尾按 autonomy 决定下一状态: interactive → 'awaiting_plan_approval'(不 emit running); 否则 'running'。
  pub async fn approve_plan(&self, run_id:&str) -> Result<(), AppError>; // awaiting_plan_approval → running + emit (调用方随后 engine.start)
  pub async fn pause(&self, run_id:&str) -> Result<(), AppError>;   // running → paused + emit
  pub async fn resume(&self, run_id:&str) -> Result<(), AppError>;  // paused → running + emit (engine.start 若未运行)
  pub async fn steer_task(&self, run_id:&str, task_id:&str, text:&str) -> Result<(), AppError>; // 该 task 的 worker conv → ConversationService::steer_message; 无 conv/未运行 → BadRequest
}
```
- **autonomy gate**:`nomi_run_create`/POST runs 现做 create→plan→engine.start。改为:create→plan;若 run.autonomy=='interactive' 且 plan 成功 → 状态='awaiting_plan_approval',**不** engine.start(等 approve);否则 status='running' + engine.start。`approve_plan`:awaiting_plan_approval→running + engine.start。
- **pause/resume**:RunEngine run_loop 每轮(fill 前)读 run.status;若 'paused' → 不 fill(跳过派新),若有在飞则 await 在飞完成处理 outcome,若无在飞则 idle-await(sleep/notify)直到 resume 或 cancel(不 busy-spin、不判 completed)。resume 设 running(engine 若已停则重 start;若 loop 仍在则它下轮恢复 fill)。**注意**:engine 持久 loop 仍在跑(只是 paused 时不 fill),所以 pause 不需停 loop;但若 loop 在 paused+无在飞 idle,要能被 resume 唤醒(用 run 的 wake notify 或周期 re-check status)。
- **steer**:steer_task 找 task.conversation_id → `conv_service.steer_message(SYSTEM_USER_ID, conv_id.to_string(), SendMessageRequest{...text...}, &task_manager)`（读 steer_message 签名;它中途注入运行中回合,Nomi-only;失败→fallback/BadRequest）。RunService 需持 conv_service + task_manager（engine 已持;steer 可走 engine 或 service——放能拿到 conv_service 的层）。
- 路由:POST `/api/orchestrator/runs/{id}/approve`、`/pause`、`/resume`;POST `/api/orchestrator/runs/{id}/tasks/{task_id}/steer`(body {text})。薄 handler。

参照模板：`ConversationService::steer_message`/`cancel`(service.rs);P2 engine run_loop(paused 检查插在 fill 前);P0 routes handler。

- [ ] **Step 1: 测试(失败优先)** — (a) interactive run: create→plan 后 status='awaiting_plan_approval'(engine 未跑);approve→running;(b) pause: 运行中 pause→status=paused,engine 不派新 worker(mock worker 计数不增),在飞跑完;resume→继续完成;(c) steer_task: 对有 conv 的 running task 调用 → conv_service.steer 被调(mock);无 conv→BadRequest。用 mock worker(delay)。
- [ ] **Step 2: RED** `cargo nextest run -p nomifun-orchestrator`(engine/run_service)。
- [ ] **Step 3: 实现** autonomy gate(plan 状态决策)+ approve/pause/resume + steer_task + engine paused 尊重(无 busy-spin)+ 路由 + DTO。
- [ ] **Step 4: GREEN** + `cargo build -p nomifun-orchestrator`。
- [ ] **Step 5: 提交** `git commit -m "feat(orchestrator): 自主级别闸 + pause/resume + steer-worker"`

---

## Task 2: IDMM 武装 worker 会话（后端接线）

**Files:** Modify app `build_orchestrator_state`(state.rs)（给 engine 的 conv_service 挂 IDMM 监督 hook）;可能 `engine.rs`(若用 ensure_supervising 每 worker);Test/build。

**设计:**
- 目标:worker(nomi yolo)会话遇 decision/open-question 时由 IDMM 自动作答,不卡到 timeout。机制二选一:
  - (A) `conv_service.with_supervision_hook(idmm_manager)`:engine 的独立 conv_service 挂上 IdmmManager（它实现 ConversationSupervisionHook，on_turn_start 武装）。需把 idmm_manager 传进 build_orchestrator_state（build_idmm_state 产出 idmm_state.service.manager()）。
  - (B) engine 每 worker 起 turn 后 `idmm_handle.ensure_supervising((kind, conv_id))`（AutoWork 范式）。
  - **优先 (A)**（一行 with_supervision_hook，最小侵入），需 build_module_states 把 idmm manager 传给 build_orchestrator_state。
- **autonomy 联动**:supervised/autonomous → 武装;interactive 也可武装(计划已审,执行自动)。简单起见:全部武装（worker 总是 yolo 自动执行;IDMM 兜底决策）。
- **若拿不到 IdmmManager 或造环**（orchestrator 依赖 idmm? idmm 依赖 conversation?）：报 BLOCKED + 说明,我延后 IDMM 武装（autonomy/pause/steer 已交付即满足主要「可控」需求）。

参照模板：`build_requirement_state`/`build_idmm_state`(state.rs)如何拿 idmm manager + 注册 hook;`with_supervision_hook`(ConversationService)。

- [ ] **Step 1: 探可行性** — 读 build_idmm_state + build_orchestrator_state + ConversationService::with_supervision_hook;确认能把 idmm manager 传进 orchestrator builder 且无依赖环。若不可行 → 报 BLOCKED,本任务标记延后。
- [ ] **Step 2: 接线** conv_service.with_supervision_hook(idmm_manager) in build_orchestrator_state(+ build_module_states 传参)。
- [ ] **Step 3: `cargo build -p nomifun-app`(关键闸)** + 跑 orchestrator/app 触碰测试确认无回归(IDMM 武装难以纯单测;以「app 编译 + 既有 e2e 不破 + hook 注册存在」为验证;真 IDMM 自动作答需 provider 真跑,留用户)。
- [ ] **Step 4: 提交** `git commit -m "feat(orchestrator): IDMM 武装 worker 会话(自动值守)"`（或若 BLOCKED:记账延后,跳到 Task 3）。

---

## Task 3: 前端 — Run 控制（approve/pause/resume/steer）

**Files:** Modify `ipcBridge.ts`(runs.{approve,pause,resume,steer})、`RunDetailHeader.tsx`(按钮)、`WorkerTranscriptPanel.tsx`/节点(steer 输入)、i18n;Test typecheck+build。

**行为:**
- ipcBridge.orchestrator.runs 加 approve/pause/resume(POST,无 body→void)+ steer(POST {run_id,task_id,updates:{text}}→void)。
- RunDetailHeader 按 run.status 显:awaiting_plan_approval→「批准计划」按钮(approve→refetch);running→「暂停」(pause);paused→「继续」(resume) + 已有「终止 Run」。状态徽标加 paused/awaiting_plan_approval。
- 节点详情(WorkerTranscriptPanel):对 running 且有 conv 的 worker,加一个 steer 输入框 + 发送 → runs.steer → 提示。
- i18n orchestrator.run.{approvePlan,pause,resume,steer,steerPlaceholder,steerSent,status.paused,status.awaiting_plan_approval} 双语 + gen:i18n。
- 操作后 useRunLive refetch 刷新。

参照模板：P1b RunDetailHeader(cancel 按钮);WorkerTranscriptPanel;ipcBridge runs。

- [ ] **Step 1: ipcBridge.runs.{approve,pause,resume,steer} + i18n + gen:i18n**。
- [ ] **Step 2: RunDetailHeader 状态化控制按钮(approve/pause/resume) + 状态徽标扩展**。
- [ ] **Step 3: 节点 steer 输入(running+有 conv)→ runs.steer → refetch**。
- [ ] **Step 4: typecheck0 + `bun run build` 绿**。
- [ ] **Step 5: 提交** `git commit -m "feat(orchestrator): Run 控制 UI(批准/暂停/恢复/steer)"`

---

## Self-Review（spec §7）
**覆盖：** 三档自主(interactive 计划闸)→Task1;pause/resume→Task1;steer→Task1+Task3;IDMM 武装→Task2;取消已 P2;UI 控制→Task3。**不含**:逐 worker 审批 UI(interactive 人在环在计划闸,worker 仍 yolo——简化决策);per-task needs_review 闸(可后续)。
**占位符:** 无 TBD;IDMM 武装允许 BLOCKED 延后(有明确判据);「worker 始终 yolo,人在环在计划闸」是有意简化。
**类型一致：** approve/pause/resume/steer 后端↔ipcBridge↔UI 一致;run 状态新增 awaiting_plan_approval/paused 三层(DTO status:String 已容纳)。

## Execution Handoff
波次:Task1(后端控制)→Task2(IDMM 接线,可 BLOCKED 延后)→Task3(前端)。SDD 每任务两阶评审+fix+记账。autonomy/IDMM 真效果验收需 provider 留用户。
