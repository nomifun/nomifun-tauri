# Phase 2 — 激活确定性视觉能力路由 (W6b) 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让「需要图片理解的节点自动路由到视觉模型、排除非视觉模型」真正生效——目前 Router 的 `needs_vision` 硬过滤是**死的**（`CapabilityProfile.modalities` 永远为空 → `has_vision` 恒 false → needs_vision 任务把所有成员都过滤掉 → 退回 LLM 规划器）。修法=把前端已有的「模型名→能力」启发式移植到 Rust，用它在成员构建处填充 `modalities`（含 `"vision"`）。

**Architecture:** 零迁移、零新结构体字段。视觉能力是**逐模型**信号，而系统里唯一的逐模型来源是**模型名**（provider `capabilities` 是 provider 级、非逐模型；hub 无逐模型能力字段）。移植 `ui/src/common/utils/modelCapabilities.ts` 的视觉判定到 `nomifun-api-types` 的一个纯函数 `infer_model_modalities(model) -> Vec<String>`，然后在三个成员构建点用它填 `CapabilityProfile.modalities`：`infer_model_capability`（run_service，裸合成成员）、`derive_role_member`（caps_orchestrator，助手成员，derive_capability 后合并）、`build_assistant_members` 的裸装饰成员（caps_orchestrator，这些在 `merge_members` 里 dedup 胜出=权威）。成本/推理档已由 `infer_model_capability` 的 STRONG/LIGHT 生效、Router 已 soft-score，不动。描述驱动的 LLM 规划路由（plan.rs `build_description_map`/`desc=` 列）已工作，不破坏。

**范围收窄（本相有意不做）：** 长上下文路由（`needs_long_context`）——它需要**新 `CapabilityProfile` 上下文字段 + 新 Router 过滤臂 + `ProviderSummary` 线程 `model_context_limits`**（跨 3 crate + api-types 字段 serde 兼容），是独立较重块、价值低于视觉，延后（Phase 2b/未来）。可编辑逐模型能力字段的 hub UI 亦延后（名字+描述启发式已够）。

**Tech Stack:** Rust（`nomifun-api-types` 新纯函数模块；`nomifun-orchestrator` run_service；`nomifun-gateway` caps_orchestrator），cargo-nextest。无 FE、无迁移。

## Global Constraints

- **Rust 测试**：`cargo nextest run -p nomifun-api-types -p nomifun-orchestrator -p nomifun-gateway`。无 `| tail`；`cargo check` 不编译 test（用 `--tests` 或直接 nextest）。
- **跨 crate 编译（Phase 1 教训）**：本相**不加** `CapabilityProfile`/`FleetMember` 字段（只填既有 `modalities` 字段），故跨 crate 构造点**不破**。但改动涉及 api-types + orchestrator + gateway 三 crate，收尾必跑 `cargo check -p nomifun-app`（下游消费者）。若最终仍决定加字段，则 api-types/orchestrator/gateway 所有 `CapabilityProfile{...}`/`FleetMember{...}` 字面量构造点都要补（勘察已列全 ~14 处）——**本计划刻意避免加字段以免这个面**。
- **无新依赖**：`infer_model_modalities` 用**纯 substring 匹配**（不引 regex 到 api-types），忠实覆盖 FE 的视觉模型族。
- **不破坏既有**：描述驱动规划（plan.rs）、成本/推理 soft-score、`derive_capability` 的 strengths 逻辑都保持；只**新增填 modalities**。`derive_capability` 签名不变（它无模型名）——视觉 modality 在调用点（`derive_role_member`，有模型名）合并。
- **Git**：分支 `feat/phase1-reliability-shared-context`（续用，叠栈）。逐任务提交。提交前 `git pull --rebase`。

## 术语 / 约定

- 新纯函数 `nomifun_api_types::infer_model_modalities(model: &str) -> Vec<String>`：返回该模型名蕴含的 modality 列表（本相只判 `"vision"`，可扩展）。视觉族（base 名 contains 任一）：`"4o"`、`"claude-3"`、`"gpt-4"`（4/4o/4.1 皆视觉）、`"gemini"`（且非 embed/纯文本变体）、`"qwen-vl"`、`"llava"`、`"vision"`、`"pixtral"`、`"grok-vision"`、`"internvl"`、`"minicpm-v"`；**排除**（先判，命中则非视觉）：base 名 contains 任一 `"embed"`、`"rerank"`、`"dall-e"`、`"flux"`、`"stable-diffusion"`、`"whisper"`、`"tts"`。base 名 = FE `getBaseModelName` 等价：lowercase、`[^a-z0-9./-]`→`-`、collapse `-`、trim。
- 填充点权威序：`build_assistant_members` 裸装饰成员（caps_orchestrator，dedup 胜出）> `infer_model_capability`（run_service 裸合成）；助手成员经 `derive_role_member` 合并。

---

### Task 1: 移植「模型名→modalities」启发式到 nomifun-api-types

**Files:**
- Create: `crates/backend/nomifun-api-types/src/model_capability.rs`
- Modify: `crates/backend/nomifun-api-types/src/lib.rs`（`mod model_capability;` + re-export）
- Test: 同文件 `#[cfg(test)] mod tests`

**Interfaces:**
- Produces: `pub fn infer_model_modalities(model: &str) -> Vec<String>`（纯，无依赖，substring 匹配）；`pub fn base_model_name(model: &str) -> String`（可 pub(crate)）。
- Consumes: 无（纯函数）。
- 语义：排除族先判→命中返回空；否则视觉族命中→`vec!["vision"]`；都不中→空 `vec![]`。

- [ ] **Step 1: 写失败测试**（`model_capability.rs` mod tests）
```rust
    #[test]
    fn vision_models_infer_vision_modality() {
        for m in ["gpt-4o", "gpt-4o-mini", "claude-3-5-sonnet", "gemini-1.5-pro",
                  "qwen-vl-max", "llava-1.6", "pixtral-12b", "some-vision-model"] {
            assert!(super::infer_model_modalities(m).contains(&"vision".to_string()),
                "{m} should infer vision");
        }
    }
    #[test]
    fn non_vision_and_excluded_models_infer_no_vision() {
        for m in ["text-embedding-3-large", "bge-reranker", "dall-e-3",
                  "flux-schnell", "whisper-1", "deepseek-chat" /* 纯文本无视觉族 */] {
            assert!(!super::infer_model_modalities(m).contains(&"vision".to_string()),
                "{m} should NOT infer vision");
        }
    }
    #[test]
    fn base_model_name_normalizes() {
        assert_eq!(super::base_model_name("GPT-4o (Preview)!"), "gpt-4o-preview");
    }
```
> 注：`deepseek-chat` 不含任何视觉族子串→空；若实现里 `gpt-4` 判视觉，`gpt-4o` 亦然（正确）。`gemini` 需排除纯文本变体——若测里 `gemini-embedding` 出现应被排除族 `embed` 拦掉。

- [ ] **Step 2: 运行确认失败**

Run: `cargo nextest run -p nomifun-api-types infer_model_modalities`
Expected: FAIL（模块/函数未定义）。

- [ ] **Step 3: 实现纯函数**（`model_capability.rs`）
```rust
/// Per-model capability inference from the model NAME — the only per-model
/// signal available (provider `capabilities` is provider-level; there is no
/// user-authored per-model capability field). Ported from the frontend
/// `ui/src/common/utils/modelCapabilities.ts`. Dep-free substring matching.

/// Normalize a model id for name matching (mirrors FE `getBaseModelName`):
/// lowercase, non-[a-z0-9./-] → '-', collapse runs, trim leading/trailing '-'.
pub fn base_model_name(model: &str) -> String {
    let lowered = model.to_lowercase();
    let mut s = String::with_capacity(lowered.len());
    for ch in lowered.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '/' | '-') {
            s.push(ch);
        } else {
            s.push('-');
        }
    }
    // collapse runs of '-' and trim.
    let mut out = String::with_capacity(s.len());
    let mut prev_dash = false;
    for ch in s.chars() {
        if ch == '-' {
            if !prev_dash { out.push('-'); }
            prev_dash = true;
        } else {
            out.push(ch);
            prev_dash = false;
        }
    }
    out.trim_matches('-').to_string()
}

/// Model families that DISQUALIFY vision (checked first).
const VISION_EXCLUDE: &[&str] =
    &["embed", "rerank", "dall-e", "flux", "stable-diffusion", "whisper", "tts"];
/// Model families that IMPLY vision.
const VISION_INCLUDE: &[&str] = &[
    "4o", "claude-3", "gpt-4", "gemini", "qwen-vl", "llava", "vision",
    "pixtral", "grok-vision", "internvl", "minicpm-v",
];

/// Infer per-model modalities from the model name. Currently only `"vision"`.
pub fn infer_model_modalities(model: &str) -> Vec<String> {
    let base = base_model_name(model);
    let mut out = Vec::new();
    let excluded = VISION_EXCLUDE.iter().any(|k| base.contains(k));
    if !excluded && VISION_INCLUDE.iter().any(|k| base.contains(k)) {
        out.push("vision".to_string());
    }
    out
}
```
`lib.rs`：加 `pub mod model_capability;`（并按该 crate 惯例 re-export，如 `pub use model_capability::infer_model_modalities;`——照 lib.rs 现有 re-export 风格）。

- [ ] **Step 4: 运行确认通过**

Run: `cargo nextest run -p nomifun-api-types`
Expected: PASS（3 新测试绿 + 既有不回归）。

- [ ] **Step 5: 提交**
```bash
git add crates/backend/nomifun-api-types/src/model_capability.rs crates/backend/nomifun-api-types/src/lib.rs
git commit -m "feat(api-types): 移植模型名→modalities 视觉能力启发式"
```

---

### Task 2: 用启发式填充 modalities 激活视觉路由 + 端到端测试

**Files:**
- Modify: `crates/backend/nomifun-orchestrator/src/run_service.rs`（`infer_model_capability` 填 modalities）
- Modify: `crates/backend/nomifun-api-types/src/orchestrator.rs`（**不改** `derive_capability` 签名；见下——modality 在调用点合并）
- Modify: `crates/backend/nomifun-gateway/src/caps_orchestrator.rs`（`derive_role_member` 合并 modality；`build_assistant_members` 裸装饰成员填 profile）
- Test: `run_service.rs` mod tests（端到端视觉路由/veto）+ 各填充点单测

**Interfaces:**
- Consumes: `nomifun_api_types::infer_model_modalities`（Task 1）。
- 三填充点：
  1. `run_service.rs::infer_model_capability(model)`：现返回 `None` 当无 STRONG/LIGHT；改为**先算 modalities**，若 modalities 非空 **或** 有 tier 信号则返回 `Some(profile)`（modalities 填入），都无则仍 `None`（保「None→baseline」）。
  2. `caps_orchestrator.rs::derive_role_member`：`derive_capability(...)` 后，把 `infer_model_modalities(&model)` 合并进 `profile.modalities`（去重）。
  3. `caps_orchestrator.rs::build_assistant_members` 裸装饰成员（现 `capability_profile: None`）：改为 `Some(CapabilityProfile { modalities: infer_model_modalities(model), ..baseline })`——用一个小 helper 构 baseline+modalities（复用 orchestrator baseline 语义：strengths 空、tools false、reasoning "medium"、cost/speed "standard"）。**注意**：这些成员在 `merge_members` dedup 胜出，是 range 成员的权威视觉信号。
- 不改 `derive_capability` 签名（无模型名）；不加任何结构体字段。

- [ ] **Step 1: 写失败测试**（`run_service.rs` mod tests，仿既有 `plan_vetoes_planner_pick_that_was_hard_filtered`）
```rust
    #[test]
    fn infer_model_capability_sets_vision_modality() {
        let cap = super::infer_model_capability("gpt-4o").expect("gpt-4o has a profile");
        assert!(cap.modalities.iter().any(|m| m == "vision"));
        // 纯文本便宜模型:有 tier 无 vision
        let mini = super::infer_model_capability("some-mini").expect("light tier");
        assert!(!mini.modalities.iter().any(|m| m == "vision"));
    }

    #[tokio::test]
    async fn needs_vision_task_routes_to_vision_model_not_text_model() {
        // range = [文本模型, 视觉模型];needs_vision 任务应被路由到视觉模型,
        // 文本模型被硬过滤;规划器若误指文本模型会被 veto 到视觉模型。
        // 复用既有 adhoc_service/veto 测试骨架:构 range 含一个视觉名(gpt-4o)+
        // 一个文本名(deepseek-chat),task_profile.needs_vision=true,
        // 断言 assign 落到 gpt-4o 成员。
        // (按既有 plan_vetoes_planner_pick_that_was_hard_filtered 的构造方式改造)
    }
```
> 第二个测试按 `run_service.rs:2934` 的 veto 测试骨架改造：把成员的模型名设为真实视觉/文本名（而非手工塞 modalities），验证「填充生效后」端到端路由正确。实现 Step 时对齐既有 harness（`adhoc_service`/`model_ref`/assign 断言）。

- [ ] **Step 2: 运行确认失败**

Run: `cargo nextest run -p nomifun-orchestrator infer_model_capability_sets_vision_modality`
Expected: FAIL（`infer_model_capability` 现 modalities 空 → gpt-4o profile 有但无 vision，第一个断言失败）。

- [ ] **Step 3: 填充 `infer_model_capability`**（`run_service.rs`）
```rust
fn infer_model_capability(model: &str) -> Option<CapabilityProfile> {
    let m = model.to_lowercase();
    const STRONG: &[&str] = &[/* 原样 */];
    const LIGHT: &[&str] = &[/* 原样 */];
    let strong = STRONG.iter().any(|k| m.contains(k));
    let light = LIGHT.iter().any(|k| m.contains(k));
    let modalities = nomifun_api_types::infer_model_modalities(model);
    let (reasoning, cost_tier, speed_tier) = if strong && !light {
        ("high", "premium", "medium")
    } else if light && !strong {
        ("low", "economy", "fast")
    } else if modalities.is_empty() {
        // 无 tier 信号且无 modality → 保「None → baseline」语义。
        return None;
    } else {
        // 无 tier 但有 modality(如纯视觉名):给 baseline tier + modalities。
        ("medium", "standard", "standard")
    };
    Some(CapabilityProfile {
        strengths: Vec::new(),
        modalities,
        tools: false,
        reasoning: reasoning.to_string(),
        cost_tier: cost_tier.to_string(),
        speed_tier: speed_tier.to_string(),
    })
}
```

- [ ] **Step 4: 填充 `derive_role_member` + `build_assistant_members`**（`caps_orchestrator.rs`）

`derive_role_member`：`derive_capability(...)` 结果绑成 `let mut prof = derive_capability(...)`，随后：
```rust
    for md in nomifun_api_types::infer_model_modalities(&model) {
        if !prof.modalities.contains(&md) { prof.modalities.push(md); }
    }
```
再 `capability_profile: Some(prof)`。

`build_assistant_members` 裸装饰成员（现 `capability_profile: None`）：改为
```rust
    capability_profile: Some(CapabilityProfile {
        strengths: Vec::new(),
        modalities: nomifun_api_types::infer_model_modalities(model),
        tools: false,
        reasoning: "medium".to_string(),
        cost_tier: "standard".to_string(),
        speed_tier: "standard".to_string(),
    }),
```
（`CapabilityProfile` 已在 caps_orchestrator import；若未 import 则加 `use nomifun_api_types::CapabilityProfile;`。）

- [ ] **Step 5: 运行测试 + 跨 crate 校验**

Run: `cargo nextest run -p nomifun-orchestrator -p nomifun-gateway -p nomifun-api-types`
Expected: PASS——新测试绿；既有 Router/assign/derive_capability 测试不回归（注意 `derive_capability_maps_keywords_and_baseline` 断言 `modalities.is_empty()`——**它测 `derive_capability` 本身、不经 `derive_role_member`**，故 derive_capability 仍返回空 modalities、该测试保持绿；视觉 modality 只在 `derive_role_member` 合并，不影响该单测）。
Run: `cargo check -p nomifun-app`
Expected: clean（无结构体字段变化，下游不破）。
No `| tail`.

- [ ] **Step 6: 提交**
```bash
git add crates/backend/nomifun-orchestrator/src/run_service.rs crates/backend/nomifun-gateway/src/caps_orchestrator.rs
git commit -m "feat(orch): 填充 modalities 激活确定性视觉路由(needs_vision 硬过滤生效)"
```

---

## 自审（Self-Review）

**1. Spec 覆盖（对照 spec §6.2）：**
- §6.2① 线程能力元数据激活 Router 硬过滤 → Task 1（移植启发式）+ Task 2（填 modalities，激活 `needs_vision` 硬过滤）。✅ 视觉路由。
- §6.2 长上下文（`model_context_limits` 线程 + Router 臂）→ **延后**（需新字段+新臂，独立较重块，Task 结构说明）。⚠️ 记录，非缺口。
- §6.2 成本/推理路由 → **已存在**（`infer_model_capability` tier + Router soft-score），本相不动、回归验证不破。✅
- §6.2③ 用户权威逐模型能力标签字段 + hub 编辑器 → **延后**（名字+描述启发式已够）。记录。
- 描述驱动 LLM 路由（plan.rs）→ 不破坏（本相不碰 plan.rs）。✅

**2. Placeholder 扫描：** 无 TBD。Task2 第二个端到端测试标注「按既有 veto 测试骨架改造，对齐 harness」——是具体复用指令（既有 `plan_vetoes_planner_pick_that_was_hard_filtered` @ run_service.rs:2934 + `adhoc_service`/`model_ref`），非空泛占位。启发式实现给了完整 dep-free 代码。

**3. 类型一致性：** `infer_model_modalities(&str)->Vec<String>` 在 Task1 定义、Task2 三处调用一致。填充只写既有 `CapabilityProfile.modalities: Vec<String>` 字段——**不加任何字段**，故无跨 crate 构造点破坏（Phase 1 教训规避）。`derive_capability` 签名不变、其单测 `modalities.is_empty()` 保持绿（视觉合并在 `derive_role_member` 而非 `derive_capability`）。

**4. 跨 crate：** 涉 api-types + orchestrator + gateway；因不加字段，构造点不破；收尾 `cargo check -p nomifun-app` 兜底。测试跑 `-p nomifun-api-types -p nomifun-orchestrator -p nomifun-gateway`。
