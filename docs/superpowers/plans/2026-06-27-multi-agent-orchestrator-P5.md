# 多 Agent 智能编排引擎 · P5 实施计划（移除遗留 team + 最终验证）

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development. Steps `- [ ]`.

**Goal:** 兑现「交付时移除遗留 `team` 子系统、无历史债」（用户硬要求）：物理删除 `nomifun-team` crate + 其 app/db/api-types 接线 + 前端 team 代码 + DROP team 表迁移；保持**整个工作区编译 + 测试绿**。然后对整个 orchestrator 功能分支做最终全量验证 + 全分支评审。

**Architecture:** 纯删除 + 解接线。team 已 inert（gateway 禁 nomi_team_*、未surface）；共享 crate（ai-agent permission_router 测试串、cron/requirement 注释）对 team 是**非功能性引用**，删 team 不需重构它们（仅清理注释/测试 fixture 字符串）。新增 append-only 迁移 DROP team 5 表。

**Spec：** §11（旧 team 移除，交付期）。orchestrator(P0-P3b)已全交付,team 与之无功能耦合。

## Global Constraints
- **整个工作区必须编译**（`cargo build --workspace`）+ 触碰 crate 测试绿 + 前端 typecheck0+build。删除遗漏的悬挂引用 = 编译错,必须全清。
- 迁移 append-only：新增 `019_drop_team.sql` DROP teams/team_agents/mailbox/team_tasks/team_task_deps（001 baseline 不改）。
- 共享 crate 非功能引用（permission_router 测试 fixture 用 `mcp__nomifun-team__...` 串、cron/requirement 注释 mirrors nomifun-team）：可保留或轻改（不阻塞;若它们导致编译/测试失败才必改——它们不 import team crate,应不影响）。
- **禁合并 main**;保持 feat/multi-agent-orchestrator。禁 cargo fmt。

## team 引用面（已扫描）
- crate: `crates/backend/nomifun-team/`（整删）。
- 根 `Cargo.toml`: `[workspace.dependencies] nomifun-team` 条目（删;members glob `crates/backend/*` 删 crate 目录即移除）。
- `nomifun-app/Cargo.toml`: `nomifun-team.workspace = true`（删）。
- `nomifun-app/src/router/state.rs`: `use nomifun_team::*`、`ModuleStates.team` 字段、`build_team_state`、build_module_states 的 `team:` 调用、guide_mcp 相关（删）。
- `nomifun-app/src/router/routes.rs`: `team_routes` use + merge（删）。
- `nomifun-app/tests/team_phase1_smoke.rs`（整删）。
- `nomifun-db`: `repository/team.rs` + `models/team.rs` + mod.rs/lib.rs re-export（删;注意 P1a 改名的 `UpdateTeamTaskParams` alias 随之消失,核对无其它引用）。
- `nomifun-api-types/src/team_mcp.rs` + lib.rs re-export（删）。
- 迁移 `019_drop_team.sql`（新增 DROP 5 表）。
- 前端: `ui/src/renderer/pages/conversation/components/multiAgent/*`、`ui/src/common/types/team/teamTypes.ts`、`ui/src/common/adapter/teamMapper.ts`、`ipcBridge.ts` 的 `team={...}` 块 + team WS、`ChatLayout/index.tsx` 的 AgentStatusStrip 挂载、`useDeepLink` 的 /team 白名单（删/清）。
- guide MCP（`nomifun-team/src/guide/*` 随 crate 删;app 若注入 guide_mcp_config 已是 None,核对）。

## Tasks

### Task 1: 后端 team 移除 + DROP 迁移 + 全工作区编译
**Files:** 删 `crates/backend/nomifun-team/`;改根 Cargo.toml、nomifun-app/Cargo.toml、nomifun-app routes.rs/state.rs、删 team_phase1_smoke.rs;删 nomifun-db team repo/models + re-export;删 nomifun-api-types team_mcp + re-export;新增 `nomifun-db/migrations/019_drop_team.sql`;清理 cron/requirement/permission_router 的 team 注释/fixture（仅在导致编译/测试失败时必改）。
- [ ] **Step 1:** 删 nomifun-team crate 目录 + 所有 nomifun-app/db/api-types 的 team 接线 + 根/app Cargo 的 team dep + team_phase1_smoke.rs。
- [ ] **Step 2:** 新增 `019_drop_team.sql`：`DROP TABLE IF EXISTS team_task_deps; team_tasks; mailbox; team_agents; teams;`（FK 顺序:先子后父;或 PRAGMA foreign_keys=OFF 环境已处理——用 IF EXISTS + 子表先）。
- [ ] **Step 3:** `cargo build --workspace`（关键闸）。逐一清除所有悬挂引用（编译错指哪改哪;含 UpdateTeamTaskParams alias 移除后的核对）。反复直到全工作区编译绿。
- [ ] **Step 4:** 触碰 crate 测试：`cargo nextest run -p nomifun-db -p nomifun-api-types -p nomifun-app`（+ orchestrator 不应受影响）。确认无 team 残留致测试失败（migration 测试若断言 team 表存在需改；019 后 team 表应不存在）。
- [ ] **Step 5:** 提交 `git commit -m "refactor(orchestrator): 移除遗留 team 子系统(crate+接线+DROP 迁移019)"`

### Task 2: 前端 team 移除
**Files:** 删 multiAgent/*、teamTypes.ts、teamMapper.ts、ipcBridge team 块+WS、ChatLayout AgentStatusStrip 挂载、useDeepLink /team 白名单;清 i18n team.* 若有 dead key。
- [ ] **Step 1:** 删上述前端 team 文件 + 清 ChatLayout/ipcBridge/useDeepLink 的 team 引用。grep 确认无悬挂 import。
- [ ] **Step 2:** `cd ui && npm run typecheck` → 0（清所有 team 悬挂类型）+ `bun run build` 绿。
- [ ] **Step 3:** i18n：删 team.* dead key（若 multiAgent 用过）+ gen:i18n;check:i18n 干净。
- [ ] **Step 4:** 提交 `git commit -m "refactor(orchestrator): 移除前端 team 多 agent 旧 UI"`

### Task 3: 最终全量验证 + 全分支评审
- [ ] **Step 1:** 全工作区编译 `cargo build --workspace` 绿;触碰 crate 全测试绿(orchestrator/db/api-types/gateway/app);前端 typecheck0+build✓。grep 全仓 `nomifun.team|nomifun_team|multiAgent|teamMapper` 确认仅余非功能注释/无。
- [ ] **Step 2:** 最终全分支评审(opus,见 requesting-code-review)覆盖整个 orchestrator(P0-P5)diff vs main merge-base;triage ledger 的 Minor/carry-forward。
- [ ] **Step 3:** 真机冒烟(controller):起 nomifun-web --dist,确认 orchestrator 页仍渲染、team 入口已无(侧栏无 team)、零 console error。
- [ ] **Step 4:** 记账 + 综合总结给用户(交付清单 + carry-forward + 用户验收清单[配 provider 跑真 run])。

## Self-Review
覆盖 §11 team 移除 + 最终验证。风险=悬挂引用→编译错,由 `cargo build --workspace` 全闸兜底。共享 crate 非功能引用(注释/测试串)不阻塞。DROP 迁移 append-only。

## Execution Handoff
Task1(后端删,opus——宽且须全工作区绿)→Task2(前端删,sonnet)→Task3(controller 全量验证+opus 全分支评审+冒烟+总结)。禁合并 main。
