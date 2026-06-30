# P4 — 助手↔角色统一实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development。Steps `- [ ]`。

**Goal:** 让「助手」可作为编排角色：助手可编辑偏好模型；编排自动把启用助手库作为候选角色纳入 Run（主管按描述复用）；任务被分给助手角色时，worker 继承该助手的**人设(persona)+技能+模型**。`fleet_members.agent_id` 从死字符串变为指向助手。

**Architecture:** **在 caps 层(将获 AssistantService 访问)构建快照时富化 `FleetMember`**——把助手解析为成员并把其 description/persona 文本/skills/模型直接写进成员，使引擎/worker 无需依赖 assistant crate（自包含快照=引擎设计原则）。worker 读富化成员→`extra.preset_rules`(persona,工厂已支持) + 模型(member.model,已支持) + 技能(经查明的 seam)。规划读 `member.description`。

**Spec:** §5（角色资产=助手统一）、§12。

## Global Constraints
- 引擎/worker **不新增** assistant crate 依赖；助手属性在 caps 层解析后塞进 `FleetMember`(自包含快照)。
- `FleetMember` 新增字段全部 `#[serde(default)]`，旧快照向后兼容。
- 后端禁 cargo fmt；只跑触碰 crate；app 必编过。前端 typecheck0+build；禁 any/ts-ignore；icon-park 无别名。
- 不破坏 orchestrator_run_e2e(4/4)/run 生命周期/IDMM/P3 描述择优。**禁合并 main**。UI 必须漂亮。

## File Structure（已勘察）
- 前端：`ui/.../AssistantEditDrawer.tsx` + `useAssistantEditor.ts`（加 models 编辑）、`pages/.../index.tsx`(透传)
- api-types：`nomifun-api-types/src/orchestrator.rs`（FleetMember 富化字段）
- gateway：`nomifun-gateway/src/deps.rs`（GatewayDeps + AssistantService）、`nomifun-app/src/router/routes.rs`（接线）、`caps_orchestrator.rs`（构建助手成员）、可能 `tools_provider.rs`(provider+model 解析复用)
- orchestrator：`worker.rs`（build_worker_extra 读 persona/skills）、`plan.rs`（build_plan_user_prompt 读 member.description）、`router.rs`(可选 capability 用)

---

## Task 1: AssistantEditDrawer 开放偏好模型编辑

**Files:** Modify `ui/.../AssistantEditDrawer.tsx`、`useAssistantEditor.ts`、`pages/nomi/.../index.tsx`(或承载 drawer 的父组件,透传 props)。

**契约（已勘察）：** 助手 DTO 已有 `models: string[]`，后端已持久化往返；**仅 UI 未读写**。Create/Update 请求对象(`useAssistantEditor.ts:281-291`/`:313-326`)未发 models。模型列表用现成 hook `useModelProviderList`（`hooks/agent/useModelProviderList.ts:37-102`，返回 providers/getAvailableModels；同 GuidModelSelector 用法）。

**改动：** `useAssistantEditor` 加 `editModels: string[]` + setter，`handleEdit`/`handleCreate`/`handleDuplicate`/`handleSave` 纳入；create/update 请求加 `models: editModels`。`AssistantEditDrawer` 在"Main Agent" Select(:370-392)后加偏好模型多选（用 useModelProviderList 取模型名列表；助手 models 是扁平模型名 `string[]`，多选模型名即可）。空态友好（"可选：该角色偏好的模型，编排时优先在范围内选用"）。视觉对齐既有表单。

- [ ] **Step 1: 实现** editModels 状态 + drawer 多选 + 请求发送。
- [ ] **Step 2: typecheck** `cd ui && npm run typecheck` → 0。
- [ ] **Step 3: build** `cd ui && bun run build` 绿。
- [ ] **Step 4: 提交** `git commit -m "feat(orchestrator): 助手编辑开放偏好模型(编排角色复用)"`

---

## Task 2: FleetMember 富化 + caps 层构建助手角色成员

**Files:** Modify `nomifun-api-types/src/orchestrator.rs`（FleetMember）、`nomifun-gateway/src/deps.rs`、`nomifun-app/src/router/routes.rs`、`nomifun-gateway/src/caps_orchestrator.rs`；测试内联。

**FleetMember 富化字段（全 serde default，向后兼容）：**
```rust
pub struct FleetMember {
    pub id: String, pub agent_id: String,
    pub provider_id: Option<String>, pub model: Option<String>,
    pub role_hint: Option<String>,
    pub capability_profile: Option<CapabilityProfile>,
    pub constraints: Option<MemberConstraints>,
    pub sort_order: i64,
    #[serde(default)] pub description: Option<String>,          // 角色/模型描述,喂规划
    #[serde(default)] pub system_prompt: Option<String>,        // 助手 persona(rule 文本),worker 用
    #[serde(default)] pub enabled_skills: Vec<String>,          // 助手技能
    #[serde(default)] pub disabled_builtin_skills: Vec<String>, // 助手禁用内置技能
}
```

**GatewayDeps + 接线：** `deps.rs` 加 `pub assistant_service: Arc<AssistantService>`（来自 nomifun-assistant）；`routes.rs` 的 `GatewayDeps{...}` 字面量接 `states.assistant.service.clone()`（O(1) 增长模式）。

**caps_orchestrator 构建助手角色成员（在 create handler，model_range 已展开后）：**
- `deps.assistant_service.list()` → 过滤 `enabled` 的助手。
- 每个启用助手解析为一个 `FleetMember`：
  - 偏好模型：取 assistant.models 中**落在 model_range 内**的首个 `(provider_id, model)`；若助手无偏好或都不在范围内 → 跳过该助手（或退而用范围首个模型，二选一：**优先跳过**，避免硬塞）。
  - `agent_id = 助手id`、`role_hint = 助手.name`、`description = 助手.description`、`system_prompt = assistant_service.read_rule(助手id, None)`(读 persona,fail-soft 空)、`enabled_skills/disabled_builtin_skills = 助手对应字段`、`capability_profile = derive_from_tags(助手.audience_tags+scenario_tags, description)`（见下）、`id = generate_prefixed_id("rmbr")`。
- 助手角色成员 **与** 裸模型成员（build_members_from_range 产出）**合并**进快照。为此 create_adhoc 需能接收预构造成员：**改 create_adhoc 接受可选 `extra_members: Vec<FleetMember>`**（或让 caps 把"裸模型 ModelRef + 助手成员"统一构造后传一个成员列表）。**决策：** caps 层构造助手成员；`CreateAdhocRunRequest` 加 `#[serde(default)] role_members: Vec<FleetMember>`（caps 填充），create_adhoc 把 `build_members_from_range(range)` + `role_members` 合并去重(按 provider+model+agent_id)。
- 裸模型成员也填 `description`=该模型的 model_description（caps 有 provider 访问，可顺带；或留 P3 的 produce 查询，二选一——**优先 caps 填 member.description**统一来源，简化 produce）。

**capability 派生（轻量 helper，可在 api-types 或 orchestrator）：** `derive_capability(tags: &[String], description: Option<&str>) -> CapabilityProfile`：tags/描述关键词→strengths；其余取基线（reasoning="medium",cost/speed="standard",tools=true 若有 skills）。保守即可（规划 LLM 读 description 才是主力）。

- [ ] **Step 1: 测试（失败优先）** — (a) FleetMember 富化字段 serde 往返(旧 JSON 无新字段→默认值); (b) caps 构建:mock assistant_service.list 返 1 启用助手(models 含范围内模型)→快照含该助手成员(agent_id=助手id,role_hint=name,system_prompt=persona,enabled_skills,description); 助手偏好模型不在范围→跳过; (c) derive_capability 基本映射。
- [ ] **Step 2: RED** `cargo nextest run -p nomifun-gateway -p nomifun-api-types -p nomifun-orchestrator`。
- [ ] **Step 3: 实现** 富化字段 + GatewayDeps/接线 + caps 构建助手成员 + create_adhoc 合并 + derive_capability。
- [ ] **Step 4: GREEN** 上述 nextest + `cargo build -p nomifun-gateway -p nomifun-orchestrator -p nomifun-app` + e2e 4/4。
- [ ] **Step 5: 提交** `git commit -m "feat(orchestrator): caps 层把启用助手构建为编排角色成员(富化快照)"`

---

## Task 3: worker 继承助手 persona/技能/模型 + 规划读 member.description

**Files:** Modify `nomifun-orchestrator/src/worker.rs`（build_worker_extra + run 透传成员富化）、`engine.rs`（compose_brief/dispatch 透传）、`plan.rs`（build_plan_user_prompt 读 member.description）；测试内联。

**改动：**
1. **worker persona**：`ConversationWorkerRunner::run` 已持 `member: &FleetMember`。`build_worker_extra` 增参，当 `member.system_prompt` 非空 → 设 `extra.preset_rules = member.system_prompt`（工厂 nomi.rs:29-34 会把 preset_rules 接在 system_prompt 后，得 `brief\n\npersona`）。
2. **worker 模型**：已天然继承（member.model 来自助手偏好，Task2 已解析）。无需改。
3. **worker 技能**：调查最简 seam 并接入：
   - 优先尝试 (c) `send_message` 的 `inject_skills`（worker.rs:163 现传空 `vec![]`）→ 改传 `member.enabled_skills`。**先查 inject_skills 语义**（send_message 参数；若它确实把技能注入该回合则用之）。
   - 若 inject_skills 不适用，查 HTTP create handler 如何从 preset_enabled_skills 算 `extra.skills` 并复用（在 build_worker_extra 直接设算好的 `extra.skills`）。
   - **若两条都纠缠**：报 BLOCKED-on-skills，**仅交付 persona+模型继承**（已是核心价值），技能继承记 carry-forward。disabled_builtin_skills 同理。
4. **规划读描述**：`build_plan_user_prompt` 的 `desc=` 列优先取 `member.description`（Task2 已填充，含助手描述与模型描述）；P3 的 provider_repo 查询可保留作裸模型兜底或简化为只读 member.description（**优先 member.description 统一**，若简化则确保裸模型成员的 description 已由 caps 填充）。

- [ ] **Step 1: 测试（失败优先）** — (a) build_worker_extra:member.system_prompt 非空→extra.preset_rules 设置(含 persona); 空→不设; (b) 若技能 seam 可行:member.enabled_skills 透传到 send_message inject_skills/extra.skills; (c) build_plan_user_prompt 用 member.description。
- [ ] **Step 2: RED** `cargo nextest run -p nomifun-orchestrator`。
- [ ] **Step 3: 实现** persona + (技能 if 可行) + 规划读 description。
- [ ] **Step 4: GREEN** `cargo nextest run -p nomifun-orchestrator` + `cargo build -p nomifun-orchestrator -p nomifun-app` + e2e 4/4。
- [ ] **Step 5: 提交** `git commit -m "feat(orchestrator): worker 继承助手 persona/技能 + 规划读成员描述"`（技能若 BLOCKED 则改 "继承 persona + 规划读描述(技能 carry-forward)"）

---

## Task 4: 集成 + 真机冒烟

- [ ] **Step 1:** `cargo build --workspace` 绿 + `cargo nextest run -p nomifun-orchestrator -p nomifun-gateway -p nomifun-api-types -p nomifun-assistant -p nomifun-app` 全绿（e2e 4/4）；前端 typecheck0+build。
- [ ] **Step 2: 真机冒烟（controller）** — `nomifun-web --dist --insecure-no-auth`（temp target/_p4_smoke）。①助手编辑抽屉的偏好模型多选渲染+保存+持久化；②(seam e2e/mock 已证)启用助手 → 编排候选成员。零 console error，UI 漂亮，截图。真·角色复用跑需 provider,留用户。
- [ ] **Step 3: 记账 + 提交**（若有微调）；账本追加 P4 完成行（注明技能继承交付/延后）。

## Self-Review（spec §5）
**覆盖：** 助手偏好模型可编辑→T1；agent_id→助手 + 自动纳入启用助手为角色→T2；worker 继承 persona/模型(+技能 if 可行)→T3；规划读助手描述→T3；capability 派生→T2。
**风险：** 技能 seam 缺失(nomi 无 extra-key)→T3 调查+允许 BLOCKED 延后(persona+模型仍交付);FleetMember 富化向后兼容→serde default + 旧快照测试;caps 加 assistant 依赖无环(assistant 不依赖 gateway)→T2 build 闸;快照体积(persona 文本)→可接受。
**自包含快照：** 引擎/worker 不依赖 assistant crate,属性在 caps 层富化进成员。

## Execution Handoff
波次：T1(UI,sonnet)→T2(富化+caps 构建,opus——跨 crate 接线+keystone)→T3(worker 继承,opus——技能 seam 调查)→T4(集成冒烟,opus controller)。每任务两阶评审+fix+记账。禁合并 main。
