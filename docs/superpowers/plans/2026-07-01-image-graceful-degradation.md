# 图片不支持模型的优雅降级 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 当模型不支持图片输入时，会话不再因 `image_url` 400 中断，而是自动剔除图片、重试并在会话内提示用户。

**Architecture:** 反应式为主 + 会话内记忆兜底。进程级 `VisionUnsupportedRegistry` 记住"某 provider+model 不支持图片"。发送时 `build_messages` 依 `ProviderCompat.supports_image()` 剔除图片；上游返回 image 类 400 时，在会话服务发送环里"记忆 + 插提示 + 同模型剔图重跑"（复用现成 failover 抑制/重建轨道），重跑因命中记忆而剔图成功。全程仅内存，不动 DB。

**Tech Stack:** Rust（workspace crates：nomifun-common、nomi-config、nomi-providers、nomifun-api-types、nomifun-ai-agent、nomifun-conversation）。

## Global Constraints

- 提交作者用 `nomifun`：`git -c user.name=nomifun commit ...`（复用配置邮箱）。**不加** `Co-Authored-By` 或 "Generated with Claude Code"。
- 仅内存：**不新增 DB 迁移、不写回 provider capabilities**。
- 本次剔除逻辑只覆盖 OpenAI 兼容 provider（`nomi-providers/src/openai.rs`，即报错来源）；Anthropic-family（`anthropic_shared::build_messages`）不在范围。
- 占位文案（发给模型）：`[图片已省略：当前模型不支持图片输入]`。
- 会话内提示文案：`当前模型不支持图片输入，已自动移除图片并重试。`
- 每模型每轮**至多一次**兜底重跑，防死循环。
- `AgentErrorCode` 派生 `Copy, PartialEq, Eq, Serialize, Deserialize` + `#[serde(rename_all="SCREAMING_SNAKE_CASE")]`；新增变体不破坏任何现有 `match`（无穷尽匹配点，`is_provider_fault` 用 `matches!` 不要求穷尽）。
- 新错误码 `UserLlmProviderImageUnsupported` **不得**加入两处 `is_provider_fault`（`model_failover.rs` 与 `nomifun-idmm/config.rs`），以免误触发换模型。

---

### Task 1: 进程级 VisionUnsupportedRegistry（内存记忆）

**Files:**
- Create: `crates/backend/nomifun-common/src/vision_registry.rs`
- Modify: `crates/backend/nomifun-common/src/lib.rs`（加 `pub mod` + re-export）
- Test: 同文件 `#[cfg(test)]`

**Interfaces:**
- Produces:
  - `VisionUnsupportedRegistry`（struct）
  - `VisionUnsupportedRegistry::global() -> &'static VisionUnsupportedRegistry`
  - `fn mark_unsupported(&self, provider_id: &str, model: &str)`
  - `fn is_unsupported(&self, provider_id: &str, model: &str) -> bool`
  - `fn new() -> Self`

- [ ] **Step 1: 写失败测试**

创建 `crates/backend/nomifun-common/src/vision_registry.rs`，先只放测试：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_then_is_unsupported_hits() {
        let reg = VisionUnsupportedRegistry::new();
        assert!(!reg.is_unsupported("p1", "gpt-x"));
        reg.mark_unsupported("p1", "gpt-x");
        assert!(reg.is_unsupported("p1", "gpt-x"));
    }

    #[test]
    fn same_model_different_provider_is_isolated() {
        let reg = VisionUnsupportedRegistry::new();
        reg.mark_unsupported("p1", "m");
        assert!(reg.is_unsupported("p1", "m"));
        assert!(!reg.is_unsupported("p2", "m"));
    }

    #[test]
    fn global_is_shared_singleton() {
        let a = VisionUnsupportedRegistry::global() as *const _;
        let b = VisionUnsupportedRegistry::global() as *const _;
        assert_eq!(a, b);
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p nomifun-common vision_registry`
Expected: 编译失败（`VisionUnsupportedRegistry` 未定义）。

- [ ] **Step 3: 写最小实现**

在同文件测试模块**之上**加：

```rust
//! 进程级"模型不支持图片"记忆表(仅内存,随进程退出清空)。
//!
//! key = provider_id + model(同名模型跨 provider 隔离)。发送侧(工厂构建
//! Config 时)读、会话服务错误兜底时写。用全局单例避免把 Arc 穿过整条工厂
//! 依赖链;单测用 `new()` 独立实例。

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

#[derive(Default)]
pub struct VisionUnsupportedRegistry {
    inner: Mutex<HashSet<String>>,
}

impl VisionUnsupportedRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn key(provider_id: &str, model: &str) -> String {
        format!("{provider_id}\u{1f}{model}")
    }

    /// 记住该 (provider_id, model) 不支持图片输入。幂等。
    pub fn mark_unsupported(&self, provider_id: &str, model: &str) {
        if let Ok(mut set) = self.inner.lock() {
            set.insert(Self::key(provider_id, model));
        }
    }

    /// 该 (provider_id, model) 是否已被标记为不支持图片。锁中毒时按 false(fail-open)。
    pub fn is_unsupported(&self, provider_id: &str, model: &str) -> bool {
        self.inner
            .lock()
            .map(|set| set.contains(&Self::key(provider_id, model)))
            .unwrap_or(false)
    }

    /// 进程级共享单例。
    pub fn global() -> &'static VisionUnsupportedRegistry {
        static GLOBAL: OnceLock<VisionUnsupportedRegistry> = OnceLock::new();
        GLOBAL.get_or_init(VisionUnsupportedRegistry::new)
    }
}
```

在 `crates/backend/nomifun-common/src/lib.rs` 按其现有 `pub mod` 风格加：

```rust
pub mod vision_registry;
pub use vision_registry::VisionUnsupportedRegistry;
```

（若 lib.rs 用集中 re-export 块，则把 `pub use` 放进该块，保持一致。）

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p nomifun-common vision_registry`
Expected: 3 个测试 PASS。

- [ ] **Step 5: 提交**

```bash
git add crates/backend/nomifun-common/src/vision_registry.rs crates/backend/nomifun-common/src/lib.rs
git -c user.name=nomifun commit -m "feat(common): 新增进程级 VisionUnsupportedRegistry(内存记忆模型不支持图片)"
```

---

### Task 2: ProviderCompat 增加 supports_image 开关

**Files:**
- Modify: `crates/agent/nomi-config/src/compat.rs`（struct 字段 + merge + 默认 true 的 accessor）
- Test: 同文件 `#[cfg(test)]`

**Interfaces:**
- Consumes: 无
- Produces:
  - `ProviderCompat.supports_image: Option<bool>`（字段）
  - `ProviderCompat::supports_image(&self) -> bool`（**默认 true**，与其它默认 false 的 accessor 不同）

- [ ] **Step 1: 写失败测试**

在 `crates/agent/nomi-config/src/compat.rs` 的 `#[cfg(test)]` 模块内加（若无测试模块则新建）：

```rust
#[test]
fn supports_image_defaults_true_when_unset() {
    let compat = ProviderCompat::default();
    assert!(compat.supports_image());
}

#[test]
fn supports_image_false_when_set_false() {
    let compat = ProviderCompat {
        supports_image: Some(false),
        ..Default::default()
    };
    assert!(!compat.supports_image());
}

#[test]
fn merge_user_supports_image_wins() {
    let defaults = ProviderCompat::default();
    let user = ProviderCompat {
        supports_image: Some(false),
        ..Default::default()
    };
    let merged = ProviderCompat::merge(defaults, user);
    assert!(!merged.supports_image());
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p nomi-config supports_image`
Expected: 编译失败（`supports_image` 字段/方法不存在）。

- [ ] **Step 3: 写最小实现**

在 `ProviderCompat` struct 末尾（`effort_levels` 字段之后，约 line 63）加字段：

```rust
    /// 该模型是否支持图片输入(多模态)。None = 默认支持(true)。
    /// 为 Some(false) 时 OpenAI provider 的 build_messages 会剔除图片、改文字占位。
    /// 由 VisionUnsupportedRegistry 在工厂构建时按 provider+model 注入,不持久化。
    pub supports_image: Option<bool>,
```

在 `merge`（line 108-128）返回的 `Self { ... }` 里，`effort_levels` 那行之后加：

```rust
            supports_image: user.supports_image.or(defaults.supports_image),
```

在 accessor 区（`effort_levels()` 之后，约 line 174）加（**注意默认 true**）：

```rust
    /// 是否支持图片输入。**默认 true**——只有被显式标记不支持时才 false。
    pub fn supports_image(&self) -> bool {
        self.supports_image.unwrap_or(true)
    }
```

各 `*_defaults()` 构造器无需改（未设 → None → true）。

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p nomi-config supports_image`
Expected: 3 个测试 PASS。再 `cargo build -p nomi-config` 确认无破坏。

- [ ] **Step 5: 提交**

```bash
git add crates/agent/nomi-config/src/compat.rs
git -c user.name=nomifun commit -m "feat(config): ProviderCompat 增加 supports_image 开关(默认 true)"
```

---

### Task 3: build_messages 依 supports_image 剔除图片

**Files:**
- Modify: `crates/agent/nomi-providers/src/openai.rs`（`build_messages` 用户图分支 line 102-137；`tool_images_user_message` 签名 line 358-376 及其 2 处调用 line 95、260）
- Test: 同文件 `#[cfg(test)]`

**Interfaces:**
- Consumes: `ProviderCompat::supports_image()`（Task 2）
- Produces: `tool_images_user_message(tool_use_id: &str, images: &[ToolImage], supports_image: bool) -> Option<Value>`（签名新增第 3 参）

- [ ] **Step 1: 写失败测试**

在 `crates/agent/nomi-providers/src/openai.rs` 的 `#[cfg(test)]` 内加。用现有测试的构造习惯（参照现有 `build_messages` 测试；`ProviderCompat::default()` 即 supports_image=true）：

```rust
#[test]
fn strips_user_image_when_supports_image_false() {
    let compat = ProviderCompat {
        supports_image: Some(false),
        ..Default::default()
    };
    let messages = vec![Message::new(
        Role::User,
        vec![
            ContentBlock::Text { text: "看这张图".into() },
            ContentBlock::Image {
                media_type: "image/png".into(),
                data: "AAAA".into(),
            },
        ],
    )];
    let out = OpenAIProvider::build_messages(&messages, "", &compat);
    let s = serde_json::to_string(&out).unwrap();
    assert!(!s.contains("image_url"), "不应出现 image_url: {s}");
    assert!(s.contains("图片已省略"), "应出现占位: {s}");
}

#[test]
fn keeps_user_image_when_supports_image_true() {
    let compat = ProviderCompat::default(); // supports_image() == true
    let messages = vec![Message::new(
        Role::User,
        vec![ContentBlock::Image {
            media_type: "image/png".into(),
            data: "AAAA".into(),
        }],
    )];
    let out = OpenAIProvider::build_messages(&messages, "", &compat);
    let s = serde_json::to_string(&out).unwrap();
    assert!(s.contains("image_url"), "应保留 image_url: {s}");
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p nomi-providers strips_user_image_when_supports_image_false keeps_user_image_when_supports_image_true`
Expected: `strips_...` FAIL（当前无条件产出 image_url）；`keeps_...` PASS。

- [ ] **Step 3: 写最小实现**

改 `build_messages` 里含图片 user 消息分支（line 108-137），把 `ContentBlock::Image` 臂改为按 `compat.supports_image()` 决定，并在剔除时补一次占位。将该分支循环体替换为：

```rust
                        if has_images {
                            // Multimodal user message: build content array with
                            // text and image_url parts.
                            let mut parts: Vec<Value> = Vec::new();
                            let mut stripped_images = 0usize;
                            for block in &msg.content {
                                match block {
                                    ContentBlock::Text { text } => {
                                        let text = strip_patterns_from_text(text, compat);
                                        if !text.is_empty() {
                                            parts.push(json!({
                                                "type": "text",
                                                "text": text
                                            }));
                                        }
                                    }
                                    ContentBlock::Image { media_type, data } => {
                                        if compat.supports_image() {
                                            parts.push(json!({
                                                "type": "image_url",
                                                "image_url": {
                                                    "url": format!("data:{media_type};base64,{data}")
                                                }
                                            }));
                                        } else {
                                            stripped_images += 1;
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            if stripped_images > 0 {
                                parts.push(json!({
                                    "type": "text",
                                    "text": "[图片已省略：当前模型不支持图片输入]"
                                }));
                            }
                            result.push(json!({
                                "role": "user",
                                "content": parts
                            }));
                        } else {
```

改 `tool_images_user_message` 签名与首行（line 358-364）：

```rust
fn tool_images_user_message(
    tool_use_id: &str,
    images: &[nomi_types::tool::ToolImage],
    supports_image: bool,
) -> Option<Value> {
    if images.is_empty() || !supports_image {
        return None;
    }
```

改其 2 处调用（line 95 与 line 260），都传 `compat.supports_image()`：

```rust
                                if let Some(img_msg) =
                                    tool_images_user_message(tool_use_id, images, compat.supports_image())
                                {
                                    result.push(img_msg);
                                }
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p nomi-providers`
Expected: 新增 2 测试 PASS，原有测试全绿（`build_messages` 签名未变，7 处测试调用点无需改）。

- [ ] **Step 5: 提交**

```bash
git add crates/agent/nomi-providers/src/openai.rs
git -c user.name=nomifun commit -m "feat(providers): OpenAI build_messages 依 supports_image 剔除图片(用户图+工具截图)"
```

---

### Task 4: 新增 AgentErrorCode::UserLlmProviderImageUnsupported

**Files:**
- Modify: `crates/backend/nomifun-api-types/src/agent_error.rs`（enum，line 12-51）
- Test: 同文件 `#[cfg(test)]`（serde 往返）

**Interfaces:**
- Produces: `AgentErrorCode::UserLlmProviderImageUnsupported`（序列化为 `"USER_LLM_PROVIDER_IMAGE_UNSUPPORTED"`）

- [ ] **Step 1: 写失败测试**

在 `crates/backend/nomifun-api-types/src/agent_error.rs` 的 `#[cfg(test)]`（若无则新建）加：

```rust
#[test]
fn image_unsupported_serde_roundtrip() {
    let code = AgentErrorCode::UserLlmProviderImageUnsupported;
    let json = serde_json::to_string(&code).unwrap();
    assert_eq!(json, "\"USER_LLM_PROVIDER_IMAGE_UNSUPPORTED\"");
    let back: AgentErrorCode = serde_json::from_str(&json).unwrap();
    assert_eq!(back, code);
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p nomifun-api-types image_unsupported_serde_roundtrip`
Expected: 编译失败（变体不存在）。

- [ ] **Step 3: 写最小实现**

在 `enum AgentErrorCode` 里，`UserLlmProviderInvalidRequest` 之后加一行：

```rust
    /// 模型不支持图片输入(收到 image_url 类 400)。会话服务据此剔图重跑,
    /// 故意 **不** 计入 is_provider_fault(不触发换模型)。
    UserLlmProviderImageUnsupported,
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p nomifun-api-types image_unsupported_serde_roundtrip`
Expected: PASS。再 `cargo build -p nomifun-api-types`。

- [ ] **Step 5: 提交**

```bash
git add crates/backend/nomifun-api-types/src/agent_error.rs
git -c user.name=nomifun commit -m "feat(api-types): AgentErrorCode 新增 UserLlmProviderImageUnsupported"
```

---

### Task 5: classify_provider_api 识别 image 类 400（且非 provider fault）

**Files:**
- Modify: `crates/backend/nomifun-ai-agent/src/protocol/send_error.rs`（在通用 invalid_request 分支 line 620 **之前**插入 image 分支）
- Test: `send_error.rs` 的 `#[cfg(test)]` + `crates/backend/nomifun-conversation/src/model_failover.rs` 的 `#[cfg(test)]`

**Interfaces:**
- Consumes: `AgentErrorCode::UserLlmProviderImageUnsupported`（Task 4）、`provider_error(...)`（现有 helper）
- Produces: 无新符号（行为变更）

- [ ] **Step 1: 写失败测试**

在 `send_error.rs` 的测试模块加（参照现有 `classifies_*` 测试的断言方式，用 `AppError::BadGateway` 触发 `classify_upstream_detail`）：

```rust
#[test]
fn classifies_image_unsupported_from_serde_variant_error() {
    let detail = "Failed to deserialize the JSON body into the target type: \
        messages[6]: unknown variant `image_url`, expected `text` at line 1 column 169755";
    let err = AgentSendError::from_app_error(AppError::BadGateway(detail.into()));
    assert_eq!(err.code(), Some(AgentErrorCode::UserLlmProviderImageUnsupported));
}

#[test]
fn plain_invalid_request_still_classifies_as_invalid_request() {
    let detail = "invalid_request_error: content is required";
    let err = AgentSendError::from_app_error(AppError::BadGateway(detail.into()));
    assert_eq!(err.code(), Some(AgentErrorCode::UserLlmProviderInvalidRequest));
}
```

在 `model_failover.rs` 测试模块加（锁定"新码不触发换模型"的不变量）：

```rust
#[test]
fn image_unsupported_is_not_provider_fault() {
    assert!(!is_provider_fault(AgentErrorCode::UserLlmProviderImageUnsupported));
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p nomifun-ai-agent classifies_image_unsupported_from_serde_variant_error`
Expected: FAIL（当前 image 报错命中通用 invalid_request 分支 → `UserLlmProviderInvalidRequest`）。
Run: `cargo test -p nomifun-conversation image_unsupported_is_not_provider_fault`
Expected: PASS（新码本就不在 `is_provider_fault` 列表；此测试锁定该不变量，**不要**把新码加进去）。

- [ ] **Step 3: 写最小实现**

在 `send_error.rs` 通用 invalid_request 分支（`if contains_any(lower, &["invalid request", ...])`，约 line 620）**之前**插入：

```rust
    // 图片不支持:上游对 image_url 内容反序列化失败 / 明确拒绝图片。必须排在
    // 通用 invalid_request 分支之前(那条会吞掉 "invalid_request_error")。
    // retryable=false + 不在 is_provider_fault → 由会话服务发送环专门"剔图重跑"。
    if contains_any(
        lower,
        &[
            "image_url",
            "unknown variant `image_url`",
            "unknown variant 'image_url'",
            "does not support image",
            "image input",
            "multimodal",
        ],
    ) {
        return Some(provider_error(
            "当前模型不支持图片输入",
            AgentErrorCode::UserLlmProviderImageUnsupported,
            false,
            AgentErrorResolutionKind::SendFeedback,
            Some(AgentErrorResolutionTarget::Feedback),
        ));
    }
```

（若 `lower` 变量名或 `contains_any`/`provider_error`/`AgentErrorResolutionKind`/`AgentErrorResolutionTarget` 与文件内一致即可直接用——它们已在同文件在用。）

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p nomifun-ai-agent classifies_image_unsupported_from_serde_variant_error plain_invalid_request_still_classifies_as_invalid_request`
Expected: 均 PASS。
Run: `cargo test -p nomifun-conversation image_unsupported_is_not_provider_fault`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add crates/backend/nomifun-ai-agent/src/protocol/send_error.rs crates/backend/nomifun-conversation/src/model_failover.rs
git -c user.name=nomifun commit -m "feat(ai-agent): 分类 image 类 400 为 UserLlmProviderImageUnsupported(排除于 provider fault)"
```

---

### Task 6: 把 registry 命中透传为 compat.supports_image（主动剔除）

**Files:**
- Modify: `crates/backend/nomifun-ai-agent/src/types.rs`（`NomiCompatOverrides` line 51-55 加字段）
- Modify: `crates/backend/nomifun-ai-agent/src/factory/provider_config.rs`（`resolve_provider_fields` 查 registry 设 override）
- Modify: `crates/backend/nomifun-ai-agent/src/factory/nomi.rs`（`resolve_nomi_url_and_compat` 构造 `NomiCompatOverrides` 的字面量补 `supports_image`）
- Modify: `crates/backend/nomifun-ai-agent/src/manager/nomi/agent.rs`（line 211-216 应用 override 到 `config.compat`）

**Interfaces:**
- Consumes: `VisionUnsupportedRegistry::global()`（Task 1）、`ProviderCompat.supports_image`（Task 2）
- Produces: `NomiCompatOverrides.supports_image: Option<bool>`

- [ ] **Step 1: 写失败测试（工厂读 registry → override）**

在 `provider_config.rs` 的 `#[cfg(test)]`（若无则新建，参照该 crate 现有工厂测试的 repo mock 方式；若无现成 provider repo mock，则本步改为在 `resolve_provider_fields` 返回后的**纯逻辑**上加测试——见下）。最小可行：抽一个纯函数便于测试：

```rust
/// 依 registry 决定该 provider+model 的图片支持 override。
/// Some(false)=已知不支持;None=未知(默认支持)。
pub(crate) fn image_support_override(provider_id: &str, model: &str) -> Option<bool> {
    if nomifun_common::VisionUnsupportedRegistry::global().is_unsupported(provider_id, model) {
        Some(false)
    } else {
        None
    }
}

#[cfg(test)]
mod image_override_tests {
    use super::*;

    #[test]
    fn override_none_when_not_marked() {
        assert_eq!(image_support_override("unlikely-prov-xyz", "unlikely-model"), None);
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p nomifun-ai-agent image_override_tests`
Expected: 编译失败（`image_support_override` 未定义）。

- [ ] **Step 3: 写最小实现**

`types.rs` 的 `NomiCompatOverrides`（line 51-55）加字段：

```rust
#[derive(Debug, Clone, Default)]
pub struct NomiCompatOverrides {
    pub max_tokens_field: Option<String>,
    pub api_path: Option<String>,
    /// None = 默认支持图片;Some(false) = registry 已标记不支持,发送时剔图。
    pub supports_image: Option<bool>,
}
```

`nomi.rs` 里 `resolve_nomi_url_and_compat` 构造 `NomiCompatOverrides { ... }` 的字面量补一行 `supports_image: None,`（grep `NomiCompatOverrides {` 找全部字面量，逐个补；因 `Default` 已派生，也可改用 `..Default::default()`）。

`provider_config.rs`：加上 Step 1 的 `image_support_override` 函数（放在 `use` 之后、`resolve_provider_fields` 之前），并在 `resolve_provider_fields` 内把 `compat_overrides` 改为可变并注入：

```rust
    let (base_url, mut compat_overrides) =
        resolve_nomi_url_and_compat(&row.platform, &row.base_url, &provider, row.is_full_url);
    compat_overrides.supports_image = image_support_override(provider_id, model);
```

`agent.rs`（line 211-216）在两个既有 `if let Some(...) = config_extra.compat_overrides.*` 之后加：

```rust
        if let Some(supports_image) = config_extra.compat_overrides.supports_image {
            config.compat.supports_image = Some(supports_image);
        }
```

- [ ] **Step 4: 运行测试确认通过 + 全 crate 编译**

Run: `cargo test -p nomifun-ai-agent image_override_tests`
Expected: PASS。
Run: `cargo build -p nomifun-ai-agent`
Expected: 编译通过；若 `NomiCompatOverrides { ... }` 字面量报"missing field supports_image"，逐个补 `supports_image: None`。

- [ ] **Step 5: 提交**

```bash
git add crates/backend/nomifun-ai-agent/src/types.rs crates/backend/nomifun-ai-agent/src/factory/provider_config.rs crates/backend/nomifun-ai-agent/src/factory/nomi.rs crates/backend/nomifun-ai-agent/src/manager/nomi/agent.rs
git -c user.name=nomifun commit -m "feat(ai-agent): 工厂按 VisionUnsupportedRegistry 注入 compat.supports_image(主动剔除)"
```

---

### Task 7: 会话内提示 + 同模型剔图重建 helper

**Files:**
- Modify: `crates/backend/nomifun-conversation/src/message_persistence.rs`（新增 `persist_images_stripped_tip`）
- Modify: `crates/backend/nomifun-conversation/src/failover_seam.rs`（新增 `strip_images_and_rebuild`）
- Test: `message_persistence.rs`（tip content JSON 形状，纯函数化断言）

**Interfaces:**
- Consumes: `VisionUnsupportedRegistry::global()`、`provider_model_from_conversation_row`、`build_task_options`、`kill_and_wait`、`get_or_build_task`
- Produces:
  - `ConversationService::persist_images_stripped_tip(&self, conversation_id: &str) -> Option<MessageRow>`
  - `ConversationService::strip_images_and_rebuild(&self, conversation_id: &str, task_manager: &Arc<dyn IWorkerTaskManager>) -> Option<AgentInstance>`

- [ ] **Step 1: 写失败测试（tip 内容形状）**

在 `message_persistence.rs` 加一个可测的纯构造函数 + 测试：

```rust
/// 构造"图片已移除"提示行的 content JSON 串(与 persist 分离便于测试)。
pub(crate) fn images_stripped_tip_content() -> String {
    serde_json::json!({
        "content": "当前模型不支持图片输入，已自动移除图片并重试。",
        "type": "warning",
        "source": "images_stripped",
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_stripped_tip_has_warning_type_and_source() {
        let s = images_stripped_tip_content();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["type"], "warning");
        assert_eq!(v["source"], "images_stripped");
        assert!(v["content"].as_str().unwrap().contains("图片"));
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p nomifun-conversation images_stripped_tip_has_warning_type_and_source`
Expected: 编译失败（`images_stripped_tip_content` 未定义）。

- [ ] **Step 3: 写最小实现**

`message_persistence.rs`：加上 Step 1 的 `images_stripped_tip_content`，并新增 persist 方法（镜像同文件 `persist_send_failure_tip`，但 type=warning、position=center、status=None）：

```rust
    /// 在会话里插入一条"图片已移除"警告提示(tips)。仅供用户查看,不回传模型。
    pub(crate) async fn persist_images_stripped_tip(&self, conversation_id: &str) -> Option<MessageRow> {
        let Ok(conv_id) = conversation_id.parse::<i64>() else {
            warn!(conversation_id, "persist_images_stripped_tip: non-numeric conversation id; skipping");
            return None;
        };
        let row = MessageRow {
            id: Self::mint_msg_id(),
            conversation_id: conv_id,
            msg_id: None,
            r#type: "tips".into(),
            content: images_stripped_tip_content(),
            position: Some("center".into()),
            status: None,
            hidden: false,
            created_at: now_ms(),
        };
        if let Err(store_err) = self.conversation_repo().insert_message(&row).await {
            warn!(conversation_id, error = %ErrorChain(&store_err), "Failed to persist images-stripped tip");
            return None;
        }
        Some(row)
    }
```

`failover_seam.rs`：在 `impl ConversationService { ... }` 内新增（复用 `parse_conv_id`/`string_to_enum`/`provider_model_from_conversation_row`/`build_task_options`，均已在本文件/crate 可用）：

```rust
    /// 同模型"剔图重建":标记 registry(该 provider+model 不支持图片)→ kill →
    /// 用同一行重建任务。重建时工厂重新读 registry → compat.supports_image=false →
    /// build_messages 剔图。仅 nomi 会话放行;返回新句柄或 None(不可重建)。
    pub(crate) async fn strip_images_and_rebuild(
        &self,
        conversation_id: &str,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Option<AgentInstance> {
        let conv_id = parse_conv_id(conversation_id).ok()?;
        let row = match self.conversation_repo().get(conv_id).await {
            Ok(Some(row)) => row,
            Ok(None) => {
                warn!(conversation_id, "strip_images_and_rebuild skipped: conversation row missing");
                return None;
            }
            Err(e) => {
                warn!(error = %ErrorChain(&e), conversation_id, "strip_images_and_rebuild skipped: load failed");
                return None;
            }
        };
        let agent_type: AgentType = string_to_enum(&row.r#type).ok()?;
        if agent_type != AgentType::Nomi {
            return None;
        }
        let pm = provider_model_from_conversation_row(&row);
        nomifun_common::VisionUnsupportedRegistry::global().mark_unsupported(&pm.provider_id, &pm.model);

        task_manager
            .kill_and_wait(conversation_id, Some(AgentKillReason::AgentErrorRecovery))
            .await;

        let build_opts = match self.build_task_options(&row) {
            Ok(opts) => opts,
            Err(e) => {
                warn!(error = %ErrorChain(&e), conversation_id, "strip_images_and_rebuild aborted: build_task_options failed");
                return None;
            }
        };
        match task_manager.get_or_build_task(conversation_id, build_opts).await {
            Ok(agent) => Some(agent),
            Err(e) => {
                warn!(error = %ErrorChain(&e), conversation_id, "strip_images_and_rebuild aborted: rebuild failed");
                None
            }
        }
    }
```

- [ ] **Step 4: 运行测试确认通过 + 编译**

Run: `cargo test -p nomifun-conversation images_stripped_tip_has_warning_type_and_source`
Expected: PASS。
Run: `cargo build -p nomifun-conversation`
Expected: 通过（若 `MessageRow` 字段名/`status` 类型不符，按该文件 `persist_send_failure_tip` 的字段填写对齐）。

- [ ] **Step 5: 提交**

```bash
git add crates/backend/nomifun-conversation/src/message_persistence.rs crates/backend/nomifun-conversation/src/failover_seam.rs
git -c user.name=nomifun commit -m "feat(conversation): 图片剔除会话提示 + 同模型剔图重建 helper"
```

---

### Task 8: 发送环接线（抑制 image 错误 + 剔图重跑）

**Files:**
- Modify: `crates/backend/nomifun-conversation/src/service.rs`（send-loop：line 1807 附近加计数器；line 1859-1866 泛化抑制器；line 1928 后加 image 重跑分支）
- Test: 编译 + 现有 send-loop/failover 测试回归（本任务以集成态验证，见 Task 9）

**Interfaces:**
- Consumes: `AgentErrorCode::UserLlmProviderImageUnsupported`、`strip_images_and_rebuild`、`persist_images_stripped_tip`（Task 7）、现有 `RelayOutcome`/`with_failover_suppressor`
- Produces: 无新符号

- [ ] **Step 1: 加计数器**

在 `failover_switches_done` 声明（line 1807）之后加：

```rust
            // 本轮已做过的"剔图重跑"次数(bounded=1,防死循环)。
            let mut image_strip_retries_done: u32 = 0;
```

- [ ] **Step 2: 泛化 relay 抑制器**

把 line 1859-1866 的整个 `if let Some(config) = failover_config.as_ref() { ... }` 块替换为：

```rust
                // 为 nomi 轮安装 pre-response 错误抑制器:既隐藏"将被换模型重试"的
                // provider fault(在切换上限内),也隐藏"将被同模型剔图重试"的
                // image-unsupported 400(每轮一次)。被吞的错误进 outcome.suppressed_error,
                // 若两种重试都未触发,则下方原样 re-surface。
                if agent.agent_type() == AgentType::Nomi {
                    let failover_within_bound = failover_config.as_ref().is_some_and(|c| {
                        failover_switches_done < c.max_switches.min(c.queue.len() as u32)
                    });
                    let image_retry_available = image_strip_retries_done == 0;
                    if failover_within_bound || image_retry_available {
                        relay = relay.with_failover_suppressor(Arc::new(move |code| {
                            (failover_within_bound
                                && crate::model_failover::is_provider_fault(code))
                                || (image_retry_available
                                    && code
                                        == nomifun_api_types::AgentErrorCode::UserLlmProviderImageUnsupported)
                        }));
                    }
                }
```

- [ ] **Step 3: 加 image 剔图重跑分支**

在 failover 分支（`if let Some(switch) = service.maybe_failover_in_send_loop(...) { ... continue; }`，止于 line 1928）**之后**、suppressed_error re-surface（line 1935 的 `if let Some(suppressed) = ...`）**之前**插入：

```rust
                // 图片不支持降级:pre-response 的 image-unsupported 400 → 记忆 + 提示 +
                // 同模型剔图重跑(每轮一次)。重跑因命中 registry 而剔图,通常成功。
                // 未触发(已重跑过 / 非 nomi / 有响应 / 码不符 / 重建失败)则落到下方
                // re-surface,把原始错误显示给用户。
                if image_strip_retries_done == 0
                    && agent.agent_type() == AgentType::Nomi
                    && outcome.terminal.is_error()
                    && !outcome.emitted_response
                    && outcome.terminal.code()
                        == Some(nomifun_api_types::AgentErrorCode::UserLlmProviderImageUnsupported)
                {
                    if let Some(rebuilt) = service
                        .strip_images_and_rebuild(&conv_id, &task_manager)
                        .await
                    {
                        service.persist_images_stripped_tip(&conv_id).await;
                        info!(
                            conversation_id = %conv_id,
                            "Model rejected images; stripped images and resending same model"
                        );
                        agent = rebuilt;
                        image_strip_retries_done += 1;
                        let resend_msg_id = Self::mint_msg_id();
                        pending_send = Some((
                            SendMessageData {
                                msg_id: resend_msg_id.clone(),
                                ..resend_payload
                            },
                            resend_msg_id,
                        ));
                        continue;
                    }
                }
```

若 `service.rs` 未 `use nomifun_api_types::AgentErrorCode;`，则用上面写的全路径即可（无需新增 use）。`info!` 已在文件在用。

- [ ] **Step 4: 编译 + 回归**

Run: `cargo build -p nomifun-conversation`
Expected: 通过。
Run: `cargo test -p nomifun-conversation`
Expected: 现有 failover/send-loop 测试全绿（抑制器泛化对非 image 场景等价于原 `is_provider_fault` 行为）。

- [ ] **Step 5: 提交**

```bash
git add crates/backend/nomifun-conversation/src/service.rs
git -c user.name=nomifun commit -m "feat(conversation): 发送环抑制 image 错误并同模型剔图重跑,会话不中断"
```

---

### Task 9: 端到端回归编译 + 全量测试

**Files:**
- 无新增（跨 crate 验证）

- [ ] **Step 1: 全工作区编译**

Run: `cargo build --workspace`
Expected: 通过。若某处 `NomiCompatOverrides` / `ProviderCompat` 字面量报缺字段，补 `supports_image: None` / `..Default::default()`。

- [ ] **Step 2: 相关 crate 测试**

Run: `cargo test -p nomifun-common -p nomi-config -p nomi-providers -p nomifun-api-types -p nomifun-ai-agent -p nomifun-conversation`
Expected: 全绿。

- [ ] **Step 3: 手动验证清单（记录到 PR 描述，不在本 plan 内跑）**

用一个纯文本模型（历史里有图片/工具截图）发消息，确认：
1. 首次：短暂不可见重试后正常出结果，会话内出现一条 warning 提示；无红色错误 tip。
2. 同会话后续再发图：静默剔除、直接成功（无新提示）。
3. 换支持图片的模型：图片正常发送（`image_url` 未被误剔）。

- [ ] **Step 4: 提交（若 Step 1 有字面量补齐等收尾改动）**

```bash
git add -A
git -c user.name=nomifun commit -m "chore: 图片降级端到端编译收尾"
```

---

## 说明与已知边界

- **仅覆盖 OpenAI 兼容 provider 的主动剔除**（报错来源）。若未来 Anthropic-family 也需，同法给 `anthropic_shared::build_messages` 接 `supports_image`。
- **跨会话记忆、单会话提示一次**：registry 是进程级；某模型在 A 会话学到后，B 会话会**静默**主动剔除（不再提示）。这是有意的最小实现；若要每会话首剔也提示，需把"本次剔了图"从 build_messages 经事件回传（后续增强，非本次范围）。
- **不新增 DB 迁移 / 不写回 capabilities**（provider 级能力扁平，写回会误伤同 provider 其它模型）。
- **`is_provider_fault` 两处副本**（`model_failover.rs` + `nomifun-idmm/config.rs`）本次**均不改**——新码不入表，故不触发换模型;Task 5 加了断言锁定该不变量。
