# 多 Agent 智能编排引擎 · P3a 实施计划（能力 Router + 分派覆盖）

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development. Steps `- [ ]`.

**Goal:** 实现 spec §6 的「智能分派」核心：一个**确定性能力 Router** 对 `成员×任务` 打分（硬约束过滤 + 软评分）并给出**自然语言理由**；`RunService.plan` 用 Router 选成员（替代盲选 planner 的 member_index，planner 建议作先验）；写入 assignment 带 score+rationale；用户可**改派/锁定**（覆盖，locked 再规划不动）。RunDetail 加 `fleet_members`(快照) 供前端解析 member→agent/model（同时修 P1b 节点 chip 只显 member_id 的遗留）。

**Architecture:** 新 `nomifun-orchestrator/src/router.rs`（纯函数打分）;改 `run_service.rs`（plan 用 Router + reassign）;扩 `RunDetail` DTO（fleet_members）;新 REST `PUT .../runs/{run}/tasks/{task}/assignment`;前端节点详情/run 详情加理由展示 + 改派/锁定控件。

**Spec：** §6（路由/能力匹配）。承接 P0/P1a 的 capability_profile(成员) + task_profile(任务) 形态。

## Global Constraints
- Router 是**纯函数 + 确定性**（同输入同输出，可单测，无 LLM）。硬约束失败 → 候选排除；软评分 → 排名；理由人类可读。
- plan 用 Router 选成员;planner 的 member_index 作**先验/平手打破**（不是唯一依据）。assignment.source='auto'+score+rationale。**locked 的 assignment 再规划/重打分不动**。
- 用户覆盖：reassign 写 source='override', locked=true。
- RunDetail 加 `fleet_members: Vec<FleetMember>`(从 run.fleet_snapshot 解码)——前端据此 member_id→agent_id/model/role 友好显示 + 改派 picker。
- 前端:禁 any/ts-ignore;useArcoMessage;theme vars;icon-park 不别名;npm run typecheck。后端:禁 cargo fmt;只跑触碰 crate;app 必编过。**禁合并 main**。

## File Structure
- 创建 `crates/backend/nomifun-orchestrator/src/router.rs`（score_member/rank_members + ScoredCandidate）。
- 修改 `run_service.rs`（plan 用 Router;reassign 方法）、`src/lib.rs`（re-export router 类型若需）。
- 修改 `crates/backend/nomifun-api-types/src/orchestrator.rs`（RunDetail 加 fleet_members;ReassignRequest）。
- 修改 `nomifun-orchestrator/src/routes.rs`（PUT assignment 路由）。
- 修改 `ui/src/common/adapter/ipcBridge.ts`（runs.reassign）+ `orchestratorTypes.ts`（RunDetail.fleet_members、TReassign）。
- 修改前端：DagCanvas/TaskNode（chip 用 fleet_members 友好标签）+ 节点详情或 run 详情加理由+改派控件。

---

## Task 1: 能力 Router 打分（纯函数）

**Files:** Create `crates/backend/nomifun-orchestrator/src/router.rs`;Modify `src/lib.rs`;Test 内联。

**Interfaces produced:**
```rust
pub struct ScoredCandidate { pub member_index: usize, pub score: f64, pub rationale: String }
/// 对单个成员打分;硬约束失败返 None(排除)。
pub fn score_member(member: &FleetMember, profile: &TaskProfile) -> Option<(f64, String)>;
/// 对全部成员排名(desc);可空(全被硬约束排除→空,调用方回退)。
pub fn rank_members(members: &[FleetMember], profile: &TaskProfile) -> Vec<ScoredCandidate>;
```
- **硬约束**（任一不满足 → None）：`profile.needs_vision` → member.capability_profile.modalities 含 "vision"；`profile.kind=="tool"` 或需要工具 → member.capability_profile.tools==true。（capability_profile 可空 → 视为基础能力:无 vision、tools=false、reasoning="medium"。）
- **软评分**（base 0，累加）：kind ↔ strengths 命中(+2 每命中相关项,如 kind=="coding" 且 strengths 含 "coding")；`needs_high_reasoning` 且 member.reasoning=="high"(+2)/=="medium"(0)/=="low"(-1)；`bulk` 且 cost_tier=="economy"(+1) 或 !bulk 且需高质 cost_tier=="premium"(+0.5)；modalities 覆盖任务所需(+0.5)。**确定性**:同分按 member_index 升序稳定。
- **理由**:拼接命中的因素，如 `"强项匹配[coding]; 高推理; 视觉就绪"`（i18n 不强求——理由是给 LLM/调试 + UI 展示的英文/中文短语，存库;UI 可原样显示。用中文短语，对齐产品语言）。

参照模板：无直接模板（新逻辑）;参照 FleetMember.capability_profile / TaskProfile 字段（api-types orchestrator.rs）。

- [ ] **Step 1: 写打分测试（失败优先）** — (a) needs_vision 任务 + 无 vision 成员 → score_member None(排除);有 vision → Some;(b) kind=coding 任务,成员 strengths 含 coding 比不含的分高;(c) needs_high_reasoning,reasoning=high 比 low 分高;(d) rank_members 排序 desc + 同分稳定;(e) 全排除 → 空 vec;(f) rationale 非空且含命中因素。
- [ ] **Step 2: 跑确认失败** — `cargo nextest run -p nomifun-orchestrator router`。
- [ ] **Step 3: 实现 router.rs + lib re-export**。
- [ ] **Step 4: 跑确认通过** + `cargo build -p nomifun-orchestrator`。
- [ ] **Step 5: 提交** `git commit -m "feat(orchestrator): 能力 Router 打分(纯函数)"`

---

## Task 2: plan 用 Router + reassign + RunDetail.fleet_members

**Files:** Modify `run_service.rs`、`api-types/src/orchestrator.rs`（RunDetail+ReassignRequest）、`nomifun-orchestrator/src/routes.rs`（PUT assignment）;Test 内联。

**Interfaces:**
```rust
// api-types: RunDetail 加字段
pub struct RunDetail { pub run: Run, pub tasks: Vec<RunTask>, pub deps: Vec<RunTaskDep>, pub assignments: Vec<Assignment>, pub fleet_members: Vec<FleetMember> }
pub struct ReassignRequest { pub member_id: String, pub locked: Option<bool> }
// RunService
impl RunService {
  // plan(): 每任务 TaskProfile → rank_members(快照成员) → 取 top(若 planner member_index 在 top-K 内则优先它作平手打破) → create_assignment(member_id, score, rationale, source='auto', locked=false)。已 locked 的不重写。
  pub async fn reassign(&self, run_id:&str, task_id:&str, req: ReassignRequest) -> Result<(), AppError>; // upsert assignment source='override', locked=req.locked.unwrap_or(true)
}
```
- plan 用 Router：对每个 PlannedTask 的 task_profile（无则用一个默认 profile：kind 从 title/spec 粗推或 "general"，全 false）跑 rank_members；取最高分成员；若 planner 给了 member_index 且该成员在候选前列（score 与 top 接近，比如在 top 2），优先采用 planner 的（尊重主管判断）。写 assignment（member_id=该成员 id、score、rationale）。**get_detail 解码 run.fleet_snapshot → fleet_members 填入 RunDetail**。
- reassign：找/建该 task 的 assignment，设 member_id + source='override' + locked。get_assignment_for_task 已存在;可能需 repo 加 update_assignment 或 delete+create（取简单:repo 加 `upsert_assignment` 或先删后建——P0 repo 有 create_assignment;加一个 `set_assignment(task_id, member_id, source, locked, ...)` upsert）。
- 路由：`PUT /api/orchestrator/runs/{run_id}/tasks/{task_id}/assignment`（body ReassignRequest）→ reassign → 200。

参照模板：P1a run_service.plan（现 member_index 直选);P0 orchestrator routes handler 形态。

- [ ] **Step 1: 写测试（失败优先）** — plan 后 assignment 有 rationale + 选了 Router 最高分成员(构造一个明显更匹配的成员,断言被选);reassign 改 member + locked,且再 plan 不动 locked;get_detail 返回 fleet_members(快照成员)。
- [ ] **Step 2: 跑确认失败** — `cargo nextest run -p nomifun-orchestrator`（router/run_service）。
- [ ] **Step 3: 实现** plan 用 Router + reassign + repo upsert_assignment(若需) + RunDetail.fleet_members + 路由 + DTO。
- [ ] **Step 4: 跑确认通过** + `cargo build -p nomifun-orchestrator`。
- [ ] **Step 5: 提交** `git commit -m "feat(orchestrator): plan 用能力 Router + 分派覆盖/锁定 + RunDetail.fleet_members"`

---

## Task 3: 前端 — 分派理由展示 + 改派/锁定 + 节点友好标签

**Files:** Modify `orchestratorTypes.ts`(TRunDetail 加 fleet_members、TReassign)、`ipcBridge.ts`(runs.reassign)、`RunDetail/DagCanvas.tsx` + `nodes/TaskNode.tsx`(chip 用 fleet_members 解析 agent/model)、节点详情或 run 详情加理由+改派控件(可在 WorkerTranscriptPanel 顶部或一个新的 TaskInspectorPanel)。Test: typecheck。

**行为:**
- TRunDetail 加 `fleet_members: TFleetMember[]`;ipcBridge.orchestrator.runs 加 `reassign: httpPut<void,{run_id;task_id;updates:TReassign}>((p)=>\`/api/orchestrator/runs/${p.run_id}/tasks/${p.task_id}/assignment\`,(p)=>p.updates)`（或合适签名,对齐既有 update idiom）。
- TaskNode chip：用 `fleet_members.find(m=>m.id===assignment.member_id)` → 显示 agent_id 友好名(经 resolveAgentLogo/agent 名) + model（替代裸 member_id;修 P1b 遗留）。DagCanvas 把 fleet_members + assignments 传入节点 data。
- 节点详情(点节点时,可扩 WorkerTranscriptPanel 或在 canvas 内一个浮层)：显示 assignment.rationale（「为何分派给它」）+ 一个成员下拉(fleet_members)改派 + 锁定开关 → `runs.reassign` → mutate(useRunLive refetch)。useArcoMessage 提示。
- i18n orchestrator.run.assign.*（rationale 标题/改派/锁定/改派成功）双语 + gen:i18n。

参照模板：P1b WorkerTranscriptPanel/TaskNode;FleetMemberRow(成员下拉);ipcBridge update idiom。

- [ ] **Step 1: 类型 + ipcBridge.reassign + i18n keys + gen:i18n**。
- [ ] **Step 2: TaskNode/DagCanvas chip 用 fleet_members 友好标签**。
- [ ] **Step 3: 节点详情理由展示 + 改派/锁定控件 → reassign → refetch**。
- [ ] **Step 4: typecheck 0**;`cd ui && bun run build` 绿。
- [ ] **Step 5: 提交** `git commit -m "feat(orchestrator): 分派理由展示 + 改派/锁定 UI + 节点友好标签"`

---

## Self-Review（对照 spec §6）
**覆盖：** §6 成员能力画像×任务画像打分 → Task1;确定性预筛+软评分+理由 → Task1;plan 用 Router → Task2;用户改派/锁定 → Task2(后端)+Task3(UI);理由透出 → Task3。**不含**:主管 LLM 二次拍板(P1 planner 已给 member_index 作先验,P3a Router 为主;完整「Router+主管 LLM 协商」可后续);成本/延迟实时计量(用 cost_tier 标签近似)。
**占位符:** 无 TBD;「TaskProfile 无则默认 general」「planner member_index 作先验平手打破」是有意设计。
**类型一致：** ScoredCandidate/score_member(Task1)→plan(Task2);RunDetail.fleet_members(Task2)→前端(Task3);reassign 签名后端↔前端一致。

## Execution Handoff
波次:Task1→Task2→Task3。SDD 每任务两阶评审+fix+记账。Task1 纯函数(sonnet);Task2 后端(sonnet);Task3 前端(sonnet)。真机 Router 效果验收需 provider 留用户。
