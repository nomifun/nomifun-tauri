# Phase 0 — Subagent 标配化 + 运行控制修复 + 协作模型选择器 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 移除「智能编排」独立入口、把 subagent 能力做成所有桌面会话标配（常驻轻量提示）；修好 DAG 画布运行控制在各状态下的可见性与反馈；在会话 composer 内新增「协作模型」选择器并打通对活跃会话 `extra.orchestrator_model_range` 的更新。

**Architecture:** 纯剪除 + 复用现有原语，零新引擎、零迁移。后端仅动 `factory/nomi.rs`（把编排 lead 提示词从「按开关/角色注入」改为「常驻轻量注入」）与新增一个 FE 更新路径调用（复用已有 `PATCH /api/conversations/{id}`）。FE 删除首页编排入口 + 全局设置开关，重构 `RunControls` 使主控在所有 run 状态下都可见可用，把 `GuidCollaboratorSelector` 复用进 `NomiSendBox` 会话工具条。

**Tech Stack:** Rust（`nomifun-ai-agent`、`nomifun-orchestrator`、`nomifun-conversation`，cargo-nextest 测试）；TypeScript + React + Arco Design + @icon-park/react + @xyflow/react（前端，`bun run typecheck` 为唯一门槛，无可跑单测）。

## Global Constraints

以下为项目级铁律，**每个任务隐含包含**：

- **前端 typecheck**：cwd `ui/`，命令 `bun run typecheck`，必须 exit 0 / 零新错。**禁用 `npx tsc`（误报 0）**。（[[frontend-test-harness-reality]] [[typecheck-zero-and-arco-conventions]]）
- **前端无可跑单测**：不新增前端 vitest；FE 任务以 typecheck + 视觉/行为人工验收为准。
- **改 locale 后必须重新生成类型**：在**仓库根**运行 `bun run gen:i18n`（= `bun scripts/generate-i18n-types.mjs`），否则 `typecheck`/`check:i18n` 漂移。
- **Rust 测试**：只跑触碰的 crate：`cargo nextest run -p <crate>`；避免 `| tail` 掩盖退出码；本期**无迁移**（不需 bump `db_lifecycle`）。
- **@icon-park/react 具名导入禁起别名**（Babel 会生成非法代码运行时崩，typecheck 也漏）。（[[icon-park-imports-no-alias]]）
- **Arco 弹窗必经 `useArcoMessage`**，勿裸 `Message.useMessage`。（[[typecheck-zero-and-arco-conventions]]）
- **无 button reset**：新增可点元素沿用既有约定（`role='button'` 的 `div`，或 Arco `Button`），避免真 `<button>` 的 WebView2 黑框。（[[no-unocss-button-reset]]）
- **Git**：先在新分支工作（勿在 `main` 直接提交）；每次提交前 `git pull --rebase`。（[[pull-before-commit]]）分支名：`feat/phase0-subagent-standardization`。
- **品牌**：一律 NomiFun；用户可见文案用「桌面伙伴」，内部 `companion`/`nomi` 标识不动。（[[brand-naming]]）
- **保留不动**（这是被保留的 subagent 能力本体，任何任务都不得删）：`caps_orchestrator.rs` 网关工具、`nomifun-orchestrator` crate、迁移 018、`link_orchestrator_run`/`extra.orchestrator_run_id`、会话原生画布组件、`engine_spawn_enabled`、`read_conversation_model_range`。

---

## 任务总览

- **Task 1**（后端 TDD）：`factory/nomi.rs` 把编排提示从「按 `autoOrchestration` 开关 / `orchestrator_role` 角色」注入改为**常驻轻量 subagent 提示**（非伙伴、非渠道/远程会话）。
- **Task 2**（FE）：删除**首页**「智能编排」入口（`ComposerEntryStrip` 按钮 + `GuidPage` 状态 + `GuidActionRow` 分支 + `useGuidSend` 编排分支）+ 相关 i18n。
- **Task 3**（FE）：删除**全局设置**「智能编排（普通会话）」开关 + `configKeys` + 相关 i18n。
- **Task 4**（FE）：重构 `RunControls`/`OrchestrationTopPanel`，使主控在**所有 run 状态**下可见可用（含 planning/终态/加载态），暂停显示「进行中 N · 排空中」，折叠态保留迷你主控。
- **Task 5**（FE）：给 `GuidCollaboratorSelector` 增加 `className` 可选 prop（为在会话工具条中做视觉对齐做准备）。
- **Task 6**（FE）：在 `NomiSendBox` 会话工具条内挂载「协作模型」选择器，从 `conversation.extra.orchestrator_model_range` 水合，变更经 `conversation.update` 写回，主模型切换时同步重写 range。

> **顺序**：Task 1 独立；Task 2、3 独立（可并行）；Task 4 独立；Task 5 → Task 6（5 是 6 的前置）。建议按 1→2→3→4→5→6 顺序执行并逐任务提交。

---

### Task 1: 常驻轻量 subagent 提示（去除智能编排 lead 门控）

**Files:**
- Modify: `crates/backend/nomifun-ai-agent/src/factory/nomi.rs`（常量 757-763、765-771 保留、852-862、864-884、177-197、605-608）
- Test: 同文件 `#[cfg(test)] mod tests`（新增纯函数单测）

**Interfaces:**
- Produces:
  - `SUBAGENT_STANDARD_HINT: &str` — 常驻轻量提示常量。
  - `should_inject_subagent_hint(is_companion: bool, is_channel: bool) -> bool` — 纯策略函数（`!is_companion && !is_channel`）。
  - `compose_subagent_hint(base: Option<String>, inject: bool) -> Option<String>` — 纯组合函数：`inject` 为真时把 hint 追加到 `base` 之后（空行分隔），否则原样返回 `base`。
- 移除：`PREF_AUTO_ORCHESTRATION`、`is_orchestration_lead`、`compose_lead_prompt`、`LEAD_ORCHESTRATOR_PROMPT`。
- 保留：`engine_spawn_enabled`（与本任务无关，是标配能力的一部分）。
- `orchestrator_role` 字段（`agent_build_extra.rs`）**保留不删**（`#[serde(default)]`，旧会话 extra 仍可反序列化；不再被读取即可，零风险）。

- [ ] **Step 1: 写失败测试**（新增到 `factory/nomi.rs` 的 `mod tests`）

```rust
    #[test]
    fn subagent_hint_injects_for_plain_desktop_session() {
        // 普通桌面会话（非伙伴、非渠道）→ 追加 subagent 提示
        assert!(super::should_inject_subagent_hint(false, false));
        let out = super::compose_subagent_hint(Some("基础提示".to_string()), true);
        let s = out.unwrap();
        assert!(s.starts_with("基础提示"));
        assert!(s.contains("nomi_spawn"));
        assert!(s.contains("nomi_run_create"));
    }

    #[test]
    fn subagent_hint_skips_companion_and_channel() {
        // 伙伴有自己的 smart_orchestration；渠道/远程网关拒 Remote，注入是死路
        assert!(!super::should_inject_subagent_hint(true, false));  // companion
        assert!(!super::should_inject_subagent_hint(false, true));  // channel/remote
        // inject=false 时原样返回，不追加
        let base = Some("仅基础".to_string());
        assert_eq!(super::compose_subagent_hint(base.clone(), false), base);
    }

    #[test]
    fn subagent_hint_handles_empty_base() {
        let out = super::compose_subagent_hint(None, true);
        assert_eq!(out, Some(super::SUBAGENT_STANDARD_HINT.to_string()));
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo nextest run -p nomifun-ai-agent subagent_hint`
Expected: FAIL（`should_inject_subagent_hint` / `compose_subagent_hint` / `SUBAGENT_STANDARD_HINT` 未定义，编译错误）。

- [ ] **Step 3: 实现常量 + 两个纯函数**

在 `factory/nomi.rs`，删除 `LEAD_ORCHESTRATOR_PROMPT`（757-763）、`is_orchestration_lead`（852-862）、`compose_lead_prompt`（864-884）、`PREF_AUTO_ORCHESTRATION`（605-608），新增：

```rust
/// 常驻轻量 subagent 使用提示。追加到每个普通桌面 nomi 会话的附加系统提示末尾，
/// 让模型在合适场景自发用编排工具把活拆给子 agent 并在画布可视化。伙伴走各自的
/// smart_orchestration 人格提示、渠道/远程会话网关拒 Remote，故不注入。取代原
/// 「智能编排」lead 提示（不再需要 autoOrchestration 开关或 orchestrator_role 角色）。
pub(crate) const SUBAGENT_STANDARD_HINT: &str = "遇到可并行的独立子任务，或需要成体系拆解的复杂多步任务时，可用 `nomi_spawn(tasks)` 立即并行派发子 agent（每个子任务在右侧编排画布实时可见状态与转录），或用 `nomi_run_create(goal)` 让规划器把目标拆成有依赖关系的任务 DAG（可用模型范围与工作目录自动取用、随即开跑）。派发后拿到 run_id，直接告诉用户已在后台执行、进度可在右侧编排画布查看，然后结束本轮——不要自己轮询等待，也不要重复创建：子任务全部完成或失败时系统会自动把结果回执给你，届时再向用户汇总。简单或单步问题直接作答，无需派发。";

/// 是否给本会话追加常驻 subagent 提示（纯策略，可单测）。伙伴与渠道/远程会话除外。
pub(crate) fn should_inject_subagent_hint(is_companion: bool, is_channel: bool) -> bool {
    !is_companion && !is_channel
}

/// 把 subagent 提示组合到已有的附加系统提示之后（组合而非替换，保留 preset/人格/知识
/// 内容）。`inject` 为假时原样返回 `base`。纯函数，便于隔离测试。
pub(crate) fn compose_subagent_hint(base: Option<String>, inject: bool) -> Option<String> {
    if !inject {
        return base;
    }
    Some(match base {
        Some(existing) if !existing.is_empty() => format!("{existing}\n\n{SUBAGENT_STANDARD_HINT}"),
        _ => SUBAGENT_STANDARD_HINT.to_owned(),
    })
}
```

- [ ] **Step 4: 替换装配处的门控块**

把 `factory/nomi.rs` 的 lead 注入块（当前 177-197：`let auto_orchestration = …; let lead = is_orchestration_lead(…); overrides.system_prompt = compose_lead_prompt(…);`）替换为：

```rust
    // 常驻 subagent 提示：让普通桌面会话默认懂得在合适场景用 nomi_spawn / nomi_run_create
    // 把活拆给子 agent 并在画布可视化。工具本就随桌面网关标配，这里只塑形提示（不授予能力、
    // 不改审批模式）。伙伴走各自 smart_orchestration；渠道/远程会话不注入（网关拒 Remote）。
    let inject_subagent_hint = should_inject_subagent_hint(
        overrides.companion,
        overrides.channel_platform.is_some(),
    );
    overrides.system_prompt =
        compose_subagent_hint(overrides.system_prompt.take(), inject_subagent_hint);
```

> 位置：保持在知识库上下文追加（当前 170-175）之后、回复语言指令（当前 ~204-214）之前——即原 lead 块所在的装配次序。
> **公开伙伴（Public）**：若 `overrides` 暴露公开/exposure 标记（见 87-90 行公开人格预置块所读的同一标记），一并纳入排除条件（`should_inject_subagent_hint` 追加一个 `&& !is_public` 参数与实参）。若该标记在此不可得，则维持伙伴/渠道两项排除即可（公开伙伴属独立领域，后续 Phase 单独处理）。

- [ ] **Step 5: 运行单测 + 全 crate check**

Run: `cargo nextest run -p nomifun-ai-agent`
Expected: PASS（新增 3 个测试通过；原 `nomi_build_extra_deserializes_orchestrator_role_lead` 等在 `nomifun-api-types` 不受影响，保留）。
Run: `cargo check -p nomifun-ai-agent`
Expected: 无错误、无「未使用」告警（确认 `LEAD_ORCHESTRATOR_PROMPT`/`is_orchestration_lead`/`compose_lead_prompt`/`PREF_AUTO_ORCHESTRATION` 及其唯一读者已一并删除，`read_bool_pref` 若仅此一处用到需一并核查是否变死代码）。

- [ ] **Step 6: 提交**

```bash
git add crates/backend/nomifun-ai-agent/src/factory/nomi.rs
git commit -m "feat(nomi): 常驻 subagent 提示取代智能编排 lead 门控"
```

---

### Task 2: 删除首页「智能编排」入口

**Files:**
- Modify: `ui/src/renderer/pages/guid/components/ComposerEntryStrip.tsx`（导入 8；props 26-27/47-48；按钮 233-244；docstring 35-36）
- Modify: `ui/src/renderer/pages/guid/GuidPage.tsx`（状态 76-80；collaborators 167-168 + import 35；useGuidSend 线程 214-217；重置 331/349/423；`collaboratorSelectorNode` 564-578 + import 21；GuidActionRow props 615/635-643；ComposerEntryStrip props 726-745；SummonDrawer 778）
- Modify: `ui/src/renderer/pages/guid/components/GuidActionRow.tsx`（导入 19；props 31-33/80/70-73/102；`configOptionCount` 110；render 271；tooltip/icon 301-331）
- Modify: `ui/src/renderer/pages/guid/hooks/useGuidSend.ts`（类型导入 24；deps 87/94 + 解构 154-155 + dep 数组 550；编排分支 171-233）
- Modify: `ui/src/renderer/services/i18n/locales/{zh-CN,en-US}/guid.json`（`entry.orchestrate`）
- Modify: `ui/src/renderer/services/i18n/locales/{zh-CN,en-US}/conversation.json`（`orchestration.startTitle` 仅此一键）

- [ ] **Step 1: `ComposerEntryStrip.tsx`**
  - 导入行 8：`import { Lightning, Robot, Workbench } from '@icon-park/react';` → 删 `Workbench`。
  - 删 props：`onOrchestrate`（26）、`isOrchestrationMode?`（27）及解构（47-48）。
  - 删按钮块（236-244，含注释 + 整个 orchestrate `<button>`），保留 `<div className={styles.entryStrip}>` 与其后「召唤助手」按钮。
  - docstring 35-36 去掉 `[智能编排]`。

- [ ] **Step 2: `GuidActionRow.tsx`**
  - 导入 19：删 `Workbench`。
  - 删 `collaboratorSelectorNode` prop（31-33 声明 + 80 解构）与其 render（271 `{collaboratorSelectorNode}`）。
  - 删 `orchestrationMode` prop（70-73 声明 + 102 解构）。
  - `configOptionCount`（110）改为：`const configOptionCount = (modelSelectorNode ? 1 : 0) + (showModeSwitch ? 1 : 0);`
  - tooltip/icon（301-331）折叠去掉 `orchestrationMode` 分支：`content` → `t('requirements.autowork.startSession')`；`disabled` → `disabled={!autoWorkMode}`；`icon` 去掉 `Workbench` 三元臂，仅留 `autoWorkMode ? <Robot…/> : <ArrowUp…/>`。

- [ ] **Step 3: `useGuidSend.ts`**
  - 类型导入 24：删 `import type { TModelRange, TModelRef } …`（仅编排分支用）。
  - 删 deps `orchestrationMode`（87 + docstring 79-87）、`collaborators`（94 + docstring 89-94）、解构（154-155）、dep 数组项（550）。
  - 删整个 `if (orchestrationMode) { … }` 编排分支（171-233）。删后首页 send 直接落入既有 nomi 常规分支（385-441，不动）。

- [ ] **Step 4: `GuidPage.tsx`**
  - 删状态 `orchestrationMode`（76-80）。
  - 删 `useGuidCollaborators` 调用（167-168）与 import（35）。
  - 删传入 `useGuidSend` 的 `orchestrationMode` / `collaborators`（214-217）。
  - 删三处 `setOrchestrationMode(false);`（331、349、423）。
  - 删 `mainModelRef` + `collaboratorSelectorNode` 块（564-578）与 `GuidCollaboratorSelector` import（21）。
  - `GuidActionRow`：删 `collaboratorSelectorNode`（615）、`orchestrationMode`（637）prop；`autoWorkMode` 与 `isButtonDisabled`（635-643）简化为不含 `orchestrationMode` 的版本：
    ```tsx
    autoWorkMode={isAutoWorkMode}
    isButtonDisabled={
      isAutoWorkMode
        ? autoWorkStartDisabled(guidInput.loading, advancedConfig.autoWork)
        : send.isButtonDisabled
    }
    ```
  - `ComposerEntryStrip`（726-745）：删 `onOrchestrate={…}`（733-741）与 `isOrchestrationMode={orchestrationMode}`（742）；`onSummon`（730）、`onFree`（732）去掉 `setOrchestrationMode(false); `。
  - `SummonDrawer` `onFree`（778）去掉 `setOrchestrationMode(false); `。

- [ ] **Step 5: i18n 键删除 + 重新生成类型**
  - `guid.json`（zh-CN + en-US）删 `entry.orchestrate` 一行（zh `"orchestrate": "智能编排",`；en `"orchestrate": "Orchestrate",`）。
  - `conversation.json`（zh-CN + en-US）在 `orchestration` 块内**仅**删 `"startTitle"` 一行（保留 `panelTitle`/`planning`/`collapseCanvas`/`expandCanvas`/`resizeCanvas` —— 会话内画布仍在用）。
  - Run（仓库根）：`bun run gen:i18n`
  - Expected: 更新 `ui/src/renderer/services/i18n/i18n-keys.d.ts`，无残留键引用。

- [ ] **Step 6: typecheck**

Run（cwd `ui/`）：`bun run typecheck`
Expected: exit 0，零新错（确认 `guid.entry.orchestrate` / `conversation.orchestration.startTitle` 已无引用；`TModelRange`/`TModelRef`/`useGuidCollaborators`/`GuidCollaboratorSelector` 在 guid 目录已无残留引用——注意 `GuidCollaboratorSelector` 组件文件本身**保留**，Task 5/6 会复用它）。

- [ ] **Step 7: 提交**

```bash
git add ui/src/renderer/pages/guid ui/src/renderer/services/i18n/locales
git commit -m "feat(guid): 移除首页智能编排入口"
```

---

### Task 3: 删除全局「智能编排（普通会话）」设置开关

**Files:**
- Modify: `ui/src/renderer/components/settings/SettingsModal/contents/SystemModalContent/index.tsx`（state 59；init 92；handler 191-197；preferenceItem 296-301）
- Modify: `ui/src/common/config/configKeys.ts`（`'nomi.autoOrchestration'` 46-49）
- Modify: `ui/src/renderer/services/i18n/locales/{zh-CN,en-US}/settings.json`（`autoOrchestration` + `autoOrchestrationDesc` 222-223）

- [ ] **Step 1: `SystemModalContent/index.tsx`**
  - 删 state（59 `const [autoOrchestration, setAutoOrchestration] = useState(false);`）。
  - 删 init 读（92 `setAutoOrchestration(configService.get('nomi.autoOrchestration') ?? false);`）。
  - 删 handler `handleAutoOrchestrationChange`（191-197）。
  - 删 `preferenceItems` 中 `key: 'autoOrchestration'` 项（296-301）。

- [ ] **Step 2: `configKeys.ts`**
  - 删 `'nomi.autoOrchestration': boolean | undefined;`（46-49，含上方注释）。
  - **核查** `'nomi.orchestrationCollaborators'`（43-45）：Task 2 已删 `useGuidCollaborators`（其唯一读者）。但 Task 6 的会话协作选择器改用 `conversation.extra`（不复用此全局键）。故此键成为死键——**保留声明不删**（配置袋里可能有历史值，删类型无收益且 configService 泛型需保持宽松）。若 typecheck 报未使用可加注释说明，勿强删。

- [ ] **Step 3: i18n + 重新生成**
  - `settings.json`（zh-CN + en-US）删 `autoOrchestration` 与 `autoOrchestrationDesc` 两行。
  - Run（根）：`bun run gen:i18n`

- [ ] **Step 4: typecheck**

Run（cwd `ui/`）：`bun run typecheck`
Expected: exit 0，零新错（`settings.autoOrchestration*` 已无引用）。

- [ ] **Step 5: 提交**

```bash
git add ui/src/renderer/components/settings ui/src/common/config/configKeys.ts ui/src/renderer/services/i18n/locales
git commit -m "feat(settings): 移除全局智能编排开关"
```

---

### Task 4: 运行控制在所有状态下可见可用

**Files:**
- Modify: `ui/src/renderer/pages/orchestrator/RunDetail/RunControls.tsx`（`RunControls` 组件）
- Modify: `ui/src/renderer/pages/conversation/orchestration/OrchestrationTopPanel.tsx`（折叠态迷你控件 199-222；`detail.tasks` 已在 `detail` 内可用）
- Modify: `ui/src/renderer/services/i18n/locales/{zh-CN,en-US}/orchestrator.json`（`run.detail` 下新增 `planningHint` / `draining` 键）

**Interfaces:**
- Consumes: `ipcBridge.orchestrator.runs.{approve,pause,resume,cancel}`（已存在）；`ipcBridge.orchestrator.runs.get` 返回的 `detail.tasks[].status`（用于进行中计数）；`detail.run.status`。
- Produces: `RunControls` 新增 prop `inFlightCount?: number`（进行中 worker 数，由 `OrchestrationTopPanel` 从 `detail.tasks` 计算传入）。

- [ ] **Step 1: `RunControls` 状态自适应 + 进行中计数**

改造 `RunControls`：主控在**每种状态**都渲染一个明确主操作，`replan` 始终可用，`cancel` 非终态可用。新增 `inFlightCount` prop，`running` 时在暂停旁显示「进行中 N · 排空中」。逐项：
  - 组件 props 增加 `inFlightCount?: number;`。
  - `awaiting_plan_approval` → 保留 `批准计划`（primary）。
  - `running` → 保留 `暂停`；其后追加一个只读计数徽标（非按钮）：当 `inFlightCount && inFlightCount > 0` 显示 `t('orchestrator.run.detail.draining', { count: inFlightCount })`。
  - `paused` → 保留 `继续`；追加只读徽标：`inFlightCount > 0` 时显示 `draining`（暂停后进行中的仍在排空）。
  - `planning` → 新增一个 `busy`（禁用）主控显示 `t('orchestrator.run.detail.planningHint')`（如「规划中…」），配 `Loading` 旋转图标；`replan` 仍在旁可用。
  - 终态（`completed`/`failed`/`cancelled`）→ 无 approve/pause/resume；`replan`（`重新规划`）此时即主「再来一次」操作，保持可见（当前实现已始终渲染 replan，确认不被 `isTerminal` 隐藏）。
  - `status === ''`（detail 未加载）→ 渲染一个 `busy` 占位主控（`t('orchestrator.run.detail.planningHint')` 或加载文案），**不要**出现「一个按钮都没有」。

  计数徽标用与状态药丸一致的只读样式（非 `HeaderControl`，避免误点）：
  ```tsx
  {status === 'running' && typeof inFlightCount === 'number' && inFlightCount > 0 && (
    <span className='inline-flex items-center gap-4px rd-8px px-8px h-30px text-11px font-500 text-t-secondary border border-b-base'>
      <Loading theme='outline' size='12' strokeWidth={3} className='animate-spin line-height-0' />
      {t('orchestrator.run.detail.draining', { count: inFlightCount })}
    </span>
  )}
  ```
  （从 `@icon-park/react` 具名导入 `Loading`，勿别名。）

- [ ] **Step 2: `OrchestrationTopPanel` 传入进行中计数 + 折叠态迷你控件**
  - 计算：`const inFlightCount = detail?.tasks.filter((tk) => tk.status === 'running').length ?? 0;`（`detail.tasks` 来自 `runs.get`，已在轮询）。
  - 传入：`<RunControls runId={runId} status={status} inFlightCount={inFlightCount} refetch={refetch} onReplan={openReplan} />`。
  - 折叠态（199-222）：当前折叠条只有「展开」。在其内追加一个基于 `status` 的迷你状态点已存在（`collapsedDot`）——补一个「运行中/待批准时可一键展开去操作」的可点性即可（保持展开为主操作，不在折叠条内塞完整控件，避免拥挤）。**验收重点**：折叠态不再让用户以为「控件消失」——`collapsedLabel` 追加状态文案（复用 `statusLabel`），使折叠条自解释。

- [ ] **Step 3: i18n 新键 + 重新生成**
  - `orchestrator.json`（zh-CN + en-US）在 `run.detail` 对象内新增：
    - zh-CN：`"planningHint": "规划中…"`，`"draining": "进行中 {{count}} · 排空中"`
    - en-US：`"planningHint": "Planning…"`，`"draining": "{{count}} in flight · draining"`
  - Run（根）：`bun run gen:i18n`

- [ ] **Step 4: typecheck**

Run（cwd `ui/`）：`bun run typecheck`
Expected: exit 0。

- [ ] **Step 5: 视觉/行为验收**（人工，无单测）
  - 构造/打开一个已链接 run 的会话：分别在 `running`（暂停出现 + 进行中计数）、`paused`（继续出现）、`planning`（禁用「规划中…」+ 重新规划可用）、终态（重新规划为主操作）、加载中（占位而非空）状态下确认头部**总有**明确主控。
  - 暂停一个「所有节点都在飞」的 run：确认出现「进行中 N · 排空中」而非「点了没反应」。

- [ ] **Step 6: 提交**

```bash
git add ui/src/renderer/pages/orchestrator/RunDetail/RunControls.tsx ui/src/renderer/pages/conversation/orchestration/OrchestrationTopPanel.tsx ui/src/renderer/services/i18n/locales
git commit -m "feat(orch): 运行控制在所有状态下可见可用 + 暂停排空反馈"
```

> 说明：本期 W2 为**纯前端**，复用既有 `approve/pause/resume/cancel/replan` 端点。run 级「重跑同一计划」（区别于会重规划的 replan）与「重启已死循环的 running run」涉及后端新端点，**故意留到 Phase 1**（与可靠性/部分交付一并做），避免 Phase 0 触后端 run 生命周期。

---

### Task 5: `GuidCollaboratorSelector` 增加 `className` prop（复用前置）

**Files:**
- Modify: `ui/src/renderer/pages/guid/components/GuidCollaboratorSelector.tsx`

- [ ] **Step 1: 增加可选 `className`**
  - `GuidCollaboratorSelectorProps` 增加 `className?: string;`。
  - 解构增加 `className`。
  - 触发按钮的 `className` 由硬编码 `'sendbox-model-btn guid-config-btn'` 改为可拼接：
    ```tsx
    import classNames from 'classnames';
    // …
    className={classNames('sendbox-model-btn guid-config-btn', className)}
    ```
  - 其余不动（首页调用方不传 `className`，行为不变）。

- [ ] **Step 2: typecheck**

Run（cwd `ui/`）：`bun run typecheck`
Expected: exit 0。

- [ ] **Step 3: 提交**

```bash
git add ui/src/renderer/pages/guid/components/GuidCollaboratorSelector.tsx
git commit -m "refactor(guid): GuidCollaboratorSelector 支持 className 以便复用"
```

---

### Task 6: 会话 composer 内「协作模型」选择器 + 活跃会话 range 更新

**Files:**
- Modify: `ui/src/renderer/pages/conversation/platforms/nomi/NomiSendBox.tsx`（props + `rightTools` 778-797）
- Modify: `ui/src/renderer/pages/conversation/components/ChatConversation.tsx`（`NomiConversationPanel` 149-164 附近 + 挂载 195-230）

**Interfaces:**
- Consumes: `GuidCollaboratorSelector`（Task 5 后带 `className`）；`ipcBridge.conversation.update`（`PATCH /api/conversations/{id}`，extra 顶层 merge）；`useModelRange` 的 `TModelRef`/`TModelRange`；`conversation.extra.orchestrator_model_range`；`conversation.model`（主模型）。
- Produces: `NomiSendBox` 新增 prop `collaboratorSelectorNode?: React.ReactNode`（父组件构造，`hideModeSelector` 时不传/不显示）。
- 约定：`orchestrator_model_range = { mode: 'range', models: [mainRef, ...collaborators] }`，`models[0]` = 主模型 = lead/planner（与 `useGuidSend`、后端 `read_conversation_model_range` 一致）。

- [ ] **Step 1: `NomiSendBox` 接受并渲染协作选择器节点**
  - 组件 props 增加 `collaboratorSelectorNode?: React.ReactNode;`（放在 `hideModeSelector?` 旁，含 docstring：会话内「协作模型」选择器，锁定伙伴等表面不传）。
  - 解构增加 `collaboratorSelectorNode`。
  - `rightTools`（778-797）在 `<NomiModelSelector … />` 之后插入：
    ```tsx
    <NomiModelSelector selection={modelSelection} className='nomi-sendbox-model-btn' />
    {collaboratorSelectorNode}
    <AgentModeSelector … />
    ```
  - （`hideModeSelector` 为真时整个 `rightTools` 已是 `undefined`，故锁定表面天然不显示；父组件对非锁定表面才构造该节点。）

- [ ] **Step 2: `ChatConversation` 的 `NomiConversationPanel` 构造协作节点 + 更新逻辑**
  - 该面板已有 `conversation`（含 `conversation.id` number、`conversation.extra`、`conversation.model`）与 `modelSelection`（`current_model`）、`onSelectModel`。
  - 计算主模型引用与初始协作池（从 extra 水合）：
    ```tsx
    const mainModelRef = useMemo(
      () => (modelSelection.current_model
        ? { provider_id: modelSelection.current_model.id, model: modelSelection.current_model.use_model }
        : null),
      [modelSelection.current_model?.id, modelSelection.current_model?.use_model]
    );
    const [collaborators, setCollaboratorsState] = useState<TModelRef[]>(() => {
      const range = conversation.extra?.orchestrator_model_range;
      // models[0] = 主模型，其余为协作池
      return range?.mode === 'range' ? range.models.slice(1) : [];
    });
    ```
  - 写回函数（构造 range → `conversation.update`，dedup 与 `useGuidSend` 一致的空格分隔键）：
    ```tsx
    const persistModelRange = useCallback(
      async (mainRef: TModelRef | null, collabs: TModelRef[]) => {
        if (!mainRef) return;
        const seen = new Set<string>();
        const models = [mainRef, ...collabs].filter((r) => {
          if (!r?.provider_id || !r.model) return false;
          const key = `${r.provider_id} ${r.model}`;
          if (seen.has(key)) return false;
          seen.add(key);
          return true;
        });
        const orchestrator_model_range: TModelRange = { mode: 'range', models };
        await ipcBridge.conversation.update.invoke({
          id: conversation.id,
          updates: { extra: { orchestrator_model_range } as TChatConversation['extra'] },
        });
      },
      [conversation.id]
    );
    const onCollaboratorsChange = useCallback(
      (next: TModelRef[]) => { setCollaboratorsState(next); void persistModelRange(mainModelRef, next); },
      [mainModelRef, persistModelRange]
    );
    ```
  - 主模型切换时同步重写 range：在既有 `onSelectModel`（149-164）成功分支（`if (ok)` 后）追加 `void persistModelRange({ provider_id: _provider.id, model: modelName }, collaborators);`。
  - 构造节点（仅非锁定表面；companion 锁定表面走 `hideModeSelector` 分支，不构造）：
    ```tsx
    const collaboratorSelectorNode = (
      <GuidCollaboratorSelector
        value={collaborators}
        onChange={onCollaboratorsChange}
        mainModel={mainModelRef}
        className='nomi-sendbox-model-btn'
      />
    );
    ```
  - 传入 `<NomiSendBox … collaboratorSelectorNode={collaboratorSelectorNode} />`。
  - 补齐 imports：`GuidCollaboratorSelector`、`TModelRef`/`TModelRange`（`@/common/types/orchestrator/orchestratorTypes`）、`ipcBridge`、`useState`/`useMemo`/`useCallback`、`TChatConversation`（若类型转换需要）。

- [ ] **Step 3: typecheck**

Run（cwd `ui/`）：`bun run typecheck`
Expected: exit 0（注意 `conversation.extra?.orchestrator_model_range` 的类型在 `storage.ts:478-487` 已声明，`models.slice(1)` 类型可用）。

- [ ] **Step 4: 行为验收**（人工）
  - 打开一个普通桌面 nomi 会话：主模型选择器旁出现「协作模型」pill；主模型以 `· 主` 钉选、不可从协作列表移除。
  - 选几个协作模型 → 关闭 → 重开会话：选择被 `conversation.extra.orchestrator_model_range` 记住（水合正确）。
  - 切换主模型 → 确认 `models[0]` 随之更新（用 devtools 看 PATCH body 或后续 `nomi_run_create` 落库的 fleet）。
  - 桌面伙伴锁定聊天表面（`hideModeSelector`）：**不出现**协作选择器。

- [ ] **Step 5: 提交**

```bash
git add ui/src/renderer/pages/conversation/platforms/nomi/NomiSendBox.tsx ui/src/renderer/pages/conversation/components/ChatConversation.tsx
git commit -m "feat(conversation): 会话内协作模型选择器 + 活跃会话 range 更新"
```

---

## 自审（Self-Review）

**1. Spec 覆盖（对照 spec §2/§3/§6.1）：**
- §2 移除智能编排入口 + 常驻提示 → Task 1（后端提示）+ Task 2（首页入口）+ Task 3（全局开关）。✅ 保留 `read_conversation_model_range`、`caps_orchestrator`、画布（Global Constraints 明列）。✅ `orchestrator_role` 保留不读（Task 1 Interfaces）。✅ 孤儿 `/runs/adhoc` 前门本期不动（spec §2.2 默认保留）。
- §3 运行控制修复 → Task 4（状态自适应主控 + 暂停排空反馈 + 折叠自解释）。⚠️ run 级「重跑同计划」与「重启死循环 running」显式**延到 Phase 1**（Task 4 末尾说明），因涉后端 run 生命周期；Phase 0 用既有 replan 兜「终态再来一次」。
- §6.1 协作模型选择器进会话 + 更新路径 → Task 5（组件复用前置）+ Task 6（挂载 + `conversation.update` 写回 + 主模型同步）。✅ 复用后端只读侧（无后端改动）。
- §6.2 自动模型路由（激活确定性 Router）→ **不在 Phase 0**（属 Phase 2），本计划不含，符合分期。

**2. Placeholder 扫描：** 无 TBD/TODO；每个改动含确切文件:行 + 逐行动作 + 真实代码；每个验证含确切命令 + 期望。唯一软引用：Task 1 Step 4 的「公开伙伴」排除依赖装配处已有的 public 标记——给出了确切定位（87-90 行公开人格块所读同一标记）与回退策略（取不到则维持两项排除），非空泛占位。

**3. 类型一致性：** `orchestrator_model_range` 结构在 `useGuidSend`（删除前）、Task 6（新增）、后端 `read_conversation_model_range`、`storage.ts:478-487` 四处一致（`{mode:'range', models:[mainRef, ...]}`，`models[0]`=主）。`TModelRef`/`TModelRange` 来自同一 `orchestratorTypes.ts`。dedup 键统一用空格分隔（与 `useGuidSend`、`encodePair` 一致）。`RunControls` 新 prop `inFlightCount` 在 Task 4 Step 1 定义、Step 2 传入，名字一致。`compose_subagent_hint`/`should_inject_subagent_hint`/`SUBAGENT_STANDARD_HINT` 在 Task 1 各步命名一致。
