# 外呼员工 P0：对外服务安全内核 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** 落地"对外服务档"的**原生工具安全内核**——让一个标记为 `PublicService` 的伙伴会话在引擎层被**硬性收窄**到只剩安全工具（问答 + 知识库检索），并修复使白名单失效的 C3 绕过 bug。

**Architecture:** 新增正交于 `Surface` 的 `ExposureMode` 枚举（`nomifun-api-types`），穿过 `NomiBuildExtra` 进入 nomi 工厂；工厂对 `PublicService` 施加**钳制**（安全白名单 + 关网关/computer/browser/spawn）。引擎侧修复 C3：post-build 注册的 knowledge/memory 工具此前绕过 `retain_named`，改为在全部注册完成后再收口一次。

**Tech Stack:** Rust 2024、`serde`/`schemars`、`nextest`。

## Global Constraints

- 语言/命名：产品面 NomiFun；内部 companion/pet/nomi 不动。
- **默认拒绝**：`ExposureMode` 缺省 = `Private`（今日行为，零回归）；`PublicService` 是唯一收窄档。
- **不信客户端**：exposure 是后端设定字段；HTTP 会话路由既有的 `strip_desktop_gateway_flag` 语义延伸——exposure 亦不可由客户端 extra 抬升（本 P0 只加字段+钳制；入口盖章在 P1）。
- **白名单非空不变量**：`retain_named(&[])` = no-op = **不限制**（`registry.rs:73`）。故 `PublicService` 的安全工具集**必须非空**，否则会意外放开全部工具。测试须钉死此不变量。
- 测试：只跑触碰 crate 的 nextest；typecheck=0；不新增前端单测。

---

### Task 1: ExposureMode 类型 + 安全白名单 + 钳制（纯逻辑）

**Files:**
- Create: `crates/backend/nomifun-api-types/src/exposure.rs`
- Modify: `crates/backend/nomifun-api-types/src/lib.rs`（加 `mod exposure; pub use exposure::*;`）

**Interfaces:**
- Produces:
  - `enum ExposureMode { Private, TrustedRemote, PublicService }`（`Default = Private`；serde `rename_all="snake_case"`）
  - `const SAFE_PUBLIC_SERVICE_TOOLS: &[&str]`（首期 = `["knowledge_search", "knowledge_read"]`）
  - `struct ExposureClamp { allowed_tools: Vec<String>, desktop_gateway: bool, computer_use: bool, browser_use: bool, in_process_spawn: bool }`
  - `fn exposure_clamp(mode: ExposureMode) -> Option<ExposureClamp>`（`None` = 不钳制，用于 Private/TrustedRemote；`Some` = PublicService 的强制值）

- [ ] **Step 1: 写失败测试** — `crates/backend/nomifun-api-types/src/exposure.rs` 末尾 `#[cfg(test)]`：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_private() {
        assert_eq!(ExposureMode::default(), ExposureMode::Private);
    }

    #[test]
    fn public_service_clamp_is_locked_down() {
        let c = exposure_clamp(ExposureMode::PublicService).expect("public service clamps");
        // 白名单非空不变量（空 = retain_named no-op = 放开全部）
        assert!(!c.allowed_tools.is_empty(), "PublicService allowlist MUST be non-empty");
        assert!(c.allowed_tools.iter().all(|t| SAFE_PUBLIC_SERVICE_TOOLS.contains(&t.as_str())));
        assert!(!c.desktop_gateway, "no gateway for strangers");
        assert!(!c.computer_use, "no desktop control for strangers");
        assert!(!c.browser_use, "no browser for strangers");
        assert!(!c.in_process_spawn, "no fan-out for strangers");
    }

    #[test]
    fn private_and_trusted_do_not_clamp() {
        assert!(exposure_clamp(ExposureMode::Private).is_none());
        assert!(exposure_clamp(ExposureMode::TrustedRemote).is_none());
    }

    #[test]
    fn safe_tools_exclude_dangerous_names() {
        for bad in ["Bash", "Write", "Edit", "ExecCommand", "Computer", "Browser", "save_memory", "recall_memories"] {
            assert!(!SAFE_PUBLIC_SERVICE_TOOLS.contains(&bad), "{bad} must NOT be public-safe");
        }
    }
}
```

- [ ] **Step 2: 跑测试确认失败** — `cargo nextest run -p nomifun-api-types exposure`；Expected: 编译失败（类型未定义）。

- [ ] **Step 3: 最小实现** — 在 `exposure.rs` 顶部：

```rust
//! Exposure tier: an orthogonal-to-Surface trust axis attached to a companion/
//! token/channel binding. `Surface` says WHERE a call comes from; `ExposureMode`
//! says HOW MUCH the caller is trusted. `PublicService` is the untrusted-stranger
//! tier: the engine is hard-clamped to a safe allowlist, no gateway, no OS tools.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExposureMode {
    /// Owner-local companion / conversation. Full capabilities (today's behavior).
    #[default]
    Private,
    /// Owner or owner-trusted external agent over the Remote front door.
    /// All-or-nothing trust (existing Remote posture); not clamped here.
    TrustedRemote,
    /// Untrusted strangers on a public channel. Hard-clamped to safe tools only.
    PublicService,
}

/// The ONLY native tools a `PublicService` session may keep. MUST stay non-empty
/// (an empty allowlist is a no-op in `retain_named` = unrestricted — the exact
/// footgun this tier exists to prevent). Grows deliberately (safe web search in P2).
pub const SAFE_PUBLIC_SERVICE_TOOLS: &[&str] = &["knowledge_search", "knowledge_read"];

/// The forced session configuration for a clamped exposure tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExposureClamp {
    pub allowed_tools: Vec<String>,
    pub desktop_gateway: bool,
    pub computer_use: bool,
    pub browser_use: bool,
    pub in_process_spawn: bool,
}

/// `Some(clamp)` = force these values regardless of what the client/host asked
/// for; `None` = leave the session unmodified (Private / TrustedRemote).
pub fn exposure_clamp(mode: ExposureMode) -> Option<ExposureClamp> {
    match mode {
        ExposureMode::Private | ExposureMode::TrustedRemote => None,
        ExposureMode::PublicService => Some(ExposureClamp {
            allowed_tools: SAFE_PUBLIC_SERVICE_TOOLS.iter().map(|s| s.to_string()).collect(),
            desktop_gateway: false,
            computer_use: false,
            browser_use: false,
            in_process_spawn: false,
        }),
    }
}
```

Then in `lib.rs` add `mod exposure;` and `pub use exposure::*;` alongside the existing module exports.

- [ ] **Step 4: 跑测试确认通过** — `cargo nextest run -p nomifun-api-types exposure`；Expected: 4 passed.

- [ ] **Step 5: Commit** — `git add -A && git commit`（feat(exposure): ExposureMode + safe public-service allowlist + clamp）。

---

### Task 2: 修 C3 — post-build 后重新收口 retain_named

**Files:**
- Modify: `crates/backend/nomifun-ai-agent/src/manager/nomi/agent.rs`（post-build 注册块之后，`init_session` 调用之前，约 :447）
- Test: `crates/backend/nomifun-ai-agent/tests/agent_types_integration.rs`（或就近既有集成测试）

**Interfaces:**
- Consumes: `config_extra.allowed_tools: Vec<String>`（Task 3 会由 exposure 填充；此任务只保证"若非空则真生效"）；`engine.registry_mut() -> &mut ToolRegistry`；`ToolRegistry::retain_named(&[String])`（已存在，`nomi-tools/src/registry.rs:73`）。

- [ ] **Step 1: 捕获白名单** — 在 `config` 被 move 进 `AgentBootstrap::new(config, …)`（:330）之前，加一行快照：

```rust
// C3-fix: post-build 工具（memory/knowledge/companion/requirement）注册在
// engine.registry_mut() 上，晚于 bootstrap 内的 retain_named，会绕过 per-session
// 白名单。快照白名单，post-build 后再收口一次。
let native_allowlist = config_extra.allowed_tools.clone();
```

- [ ] **Step 2: post-build 后收口** — 紧接 knowledge_write 注册块结束（:447）之后、`init_session`（:448）之前插入：

```rust
// C3-fix: 全部 post-build 原生工具注册完毕后再执行一次白名单收口，使
// PublicService 等受限会话的 allowed_tools 真正约束 memory/knowledge 工具
// （bootstrap 内的 retain_named 只覆盖 build 之前注册的工具）。空名单 = 不限制。
if !native_allowlist.is_empty() {
    engine.registry_mut().retain_named(&native_allowlist);
    debug!(conversation_id = %conversation_id, allow = ?native_allowlist, "C3: re-applied native allowlist after post-build registration");
}
```

- [ ] **Step 3: 写守护测试** — 断言"收口发生在 post-build 之后"的机制。就近加一个针对 `retain_named` 语义 + post-build 名称集的单测（若 manager build 需要重型 mock，则测试 `ToolRegistry`：注册 `knowledge_search`/`knowledge_read`/`recall_memories`/`save_memory`，`retain_named(&["knowledge_search","knowledge_read"])` 后断言前二保留、后二移除）：

```rust
#[test]
fn public_allowlist_strips_postbuild_memory_tools() {
    use nomi_tools::registry::ToolRegistry;
    let mut reg = ToolRegistry::new();
    for n in ["knowledge_search", "knowledge_read", "recall_memories", "save_memory"] {
        reg.register(make_named_tool(n)); // 复用该测试文件既有的 make_tool 辅助
    }
    reg.retain_named(&["knowledge_search".into(), "knowledge_read".into()]);
    assert!(reg.get("knowledge_search").is_some());
    assert!(reg.get("knowledge_read").is_some());
    assert!(reg.get("recall_memories").is_none(), "save/recall 必须被收窄掉");
    assert!(reg.get("save_memory").is_none());
}
```

- [ ] **Step 4: 跑测试** — `cargo nextest run -p nomifun-ai-agent`（或触碰的测试名）；Expected: PASS + crate 编译绿。

- [ ] **Step 5: Commit** — `git add -A && git commit`（fix(agent): re-apply native allowlist after post-build tool registration (C3)）。

---

### Task 3: exposure 穿过 NomiBuildExtra + 工厂施加钳制

**Files:**
- Modify: `crates/backend/nomifun-api-types/src/agent_build_extra.rs`（`NomiBuildExtra` 加字段）
- Modify: `crates/backend/nomifun-ai-agent/src/factory/nomi.rs`（应用 `exposure_clamp`）
- Test: `crates/backend/nomifun-ai-agent/tests/agent_types_integration.rs`

**Interfaces:**
- Consumes: `ExposureMode` / `exposure_clamp`（Task 1）。
- Produces: `NomiBuildExtra.exposure: ExposureMode`。

- [ ] **Step 1: 加字段** — 在 `NomiBuildExtra`（`agent_build_extra.rs`，`allowed_tools` 字段附近）追加：

```rust
    /// 对外服务信任档（正交于 Surface）。后端设定；`PublicService` 令工厂把会话
    /// 硬钳到安全白名单（关网关/computer/browser/spawn）。缺省 Private = 今日行为。
    #[serde(default, alias = "exposure")]
    pub exposure: nomifun_api_types_self_ref_ExposureMode,
```

> 注：`NomiBuildExtra` 就在 `nomifun-api-types` crate 内，直接用 `crate::ExposureMode`（Task 1 已 `pub use`）。字段写作：

```rust
    #[serde(default)]
    pub exposure: crate::ExposureMode,
```

- [ ] **Step 2: 写失败测试** — `agent_types_integration.rs`：

```rust
#[test]
fn nomi_build_extra_deserializes_public_service_exposure() {
    let extra: nomifun_api_types::NomiBuildExtra =
        serde_json::from_value(serde_json::json!({ "exposure": "public_service" })).unwrap();
    assert_eq!(extra.exposure, nomifun_api_types::ExposureMode::PublicService);
    // 缺省不回归
    let d: nomifun_api_types::NomiBuildExtra = serde_json::from_value(serde_json::json!({})).unwrap();
    assert_eq!(d.exposure, nomifun_api_types::ExposureMode::Private);
}
```

- [ ] **Step 3: 跑测试确认失败** — `cargo nextest run -p nomifun-ai-agent nomi_build_extra_deserializes_public_service_exposure`；Expected: 编译失败（无 exposure 字段）。

- [ ] **Step 4: 工厂施加钳制** — 在 `factory/nomi.rs` 组装最终配置处（`browser_use_enabled`/`computer_use` 解析之后、构造 `config_extra`/`allowed_tools`/`desktop_gateway` 之前），插入钳制：

```rust
// 对外服务钳制：PublicService 硬性覆盖为安全白名单 + 关网关/computer/browser/spawn。
// 覆盖任何 client/host 传入值——这是 execution-time 后端权威闸，不信上游。
let exposure_clamp = nomifun_api_types::exposure_clamp(overrides.exposure);
let (eff_allowed_tools, eff_desktop_gateway, eff_computer_use, eff_browser_use, eff_in_process_spawn) =
    match &exposure_clamp {
        Some(c) => (
            c.allowed_tools.clone(),
            c.desktop_gateway,
            c.computer_use,
            c.browser_use,
            c.in_process_spawn,
        ),
        None => (
            overrides.allowed_tools.clone(),
            overrides.desktop_gateway,
            computer_use_enabled,   // 既有解析值
            browser_use_enabled,    // 既有解析值（:346）
            engine_spawn_enabled(overrides.desktop_gateway, overrides.channel_platform.as_deref()),
        ),
    };
```

然后把下游对 `overrides.allowed_tools` / `overrides.desktop_gateway` / `computer_use` / `browser_use` / `in_process_spawn` 的取值全部改用 `eff_*`（`allowed_tools: eff_allowed_tools`（:440）、gateway 注入判定（:60）、`config.tools.computer.enabled`/`browser.enabled`、`in_process_spawn`）。

> 校验点：`desktop_gateway` 目前在 :60 与 :436/:1110/:1279 多处读取；确保对外钳制路径统一读 `eff_desktop_gateway`（PublicService 恒 false → 不注入网关 MCP、不进 lead）。

- [ ] **Step 5: 工厂钳制单测** — 抽一个纯函数便于测（推荐）：`fn resolve_exposure_effective(overrides, computer_use_enabled, browser_use_enabled) -> Effective`，对其单测：

```rust
#[test]
fn factory_clamps_public_service_session() {
    let mut ov = nomifun_api_types::NomiBuildExtra::default();
    ov.exposure = nomifun_api_types::ExposureMode::PublicService;
    ov.desktop_gateway = true;              // 上游即便要网关…
    ov.allowed_tools = vec!["Bash".into()]; // …或塞危险工具
    let eff = resolve_exposure_effective(&ov, /*computer*/ true, /*browser*/ true);
    assert!(!eff.desktop_gateway);
    assert!(!eff.computer_use);
    assert!(!eff.browser_use);
    assert!(!eff.in_process_spawn);
    assert_eq!(eff.allowed_tools, vec!["knowledge_search".to_string(), "knowledge_read".to_string()]);
}
```

- [ ] **Step 6: 跑测试** — `cargo nextest run -p nomifun-ai-agent`；Expected: PASS。

- [ ] **Step 7: 全触碰 crate build** — `cargo check -p nomifun-api-types -p nomifun-ai-agent`；Expected: 绿。

- [ ] **Step 8: Commit** — `git add -A && git commit`（feat(factory): clamp PublicService exposure sessions to safe tools）。

---

## 后续计划（独立子系统，各自成 plan）

- **P1 数据模型 + 入口盖章**：`CompanionProfileConfig` 加对外服务字段（迁移）+ 专门"对外伙伴"隔离单元（独立记忆/只绑公开库/独立数据目录）；渠道公开模式与 Remote 令牌把 `ExposureMode::PublicService` 盖进 `NomiBuildExtra`/`CallerCtx`；`recall_memories` 对 PublicService 关闭或改公司自身作用域。
- **P1-hardening 网关 C7**：把域/档白名单从"仅广告"提升为 `dispatch_opt` 权威强制（防知道名字即越权调用）。
- **P2 安全网搜**：出口防火墙 fetch 工具 + 加入 `SAFE_PUBLIC_SERVICE_TOOLS`。
- **P3 沙箱编程**：独立安全域（容器/WSL2/独立用户 + 资源/出口管控）。
- **UI**："外呼员工"顶级 Tab + 审计日志。

## Self-Review

- **Spec 覆盖**：本 P0 覆盖 spec §2.1 强制点①（原生白名单）+ 必修 C3；§2.1 ②(网关权威)③(KB 烘死)④(隔离伙伴)⑤(入口盖章) 明确落到 P1。
- **占位符扫描**：无 TODO/TBD；每步含实际代码或命令。
- **类型一致**：`ExposureMode`/`exposure_clamp`/`SAFE_PUBLIC_SERVICE_TOOLS`/`ExposureClamp` 命名在三任务间一致；字段名 `exposure`、`allowed_tools`、`desktop_gateway` 与既有代码一致。
