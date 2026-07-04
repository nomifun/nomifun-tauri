# 模型供应商删除保护 + 优雅回退 + 友好文案 — 设计文档

- 日期：2026-07-04
- 状态：待评审
- 关联问题：删除模型供应商后，被删 provider 的悬空引用在会话运行时炸出裸 id 报错
  `Bad request: Provider 'prov_1kwcd9za6cqxe0ww' not found`（截图为首页打开旧会话时 warmup + 自动发送两条 toast）。

## 1. 背景与问题

### 现状事实（已核实）

- **Provider 存储**：单表 `providers`，一行一个 provider（`id = prov_{uuidv7}`，`name` 独立列，`models` 为 JSON 数组）。无独立 models 表。
- **引用是"值对象快照"不是外键**：`{ provider_id, model, use_model }`（`ProviderWithModel`）被快照式嵌入多处，全是普通字符串，无 SQL 外键约束：
  - `conversations.model`（普通会话 / 伙伴绑定会话 / 对外伙伴会话在创建时都会盖一个具体 provider_id）
  - 桌面伙伴 JSON 文件 `companions/{id}/config.json` → `ModelConfig.provider_id`（含 shared learn/evolve）
  - 对外伙伴 JSON 文件 `public-agents/{id}/config.json` → `PublicAgentModel.provider_id`
  - 智能决策 IDMM：`client_preferences: idmm_backup_provider_id` + `conversations.extra.idmm` / `terminal_sessions.idmm` 内各 watch 的 `bypass_model.provider_id`
  - 智能编排：`fleet_members.provider_id`（真实列）
  - 软引用（会自愈）：`nomi.defaultModel`（前端配置）、`knowledge.autogenModel`、`agent.model_failover`、`assistant.{platform}.defaultModel`（渠道默认模型，后端 client_preferences）
- **删除无任何 in-use 检查**：`ProviderService::delete`（`nomifun-system/src/provider.rs:110`）直接 `DELETE FROM providers WHERE id=?`（`sqlite_provider.rs:143`）。唯一守卫是 id 不存在返回 404。
- **报错来源**：字符串 `Provider '{id}' not found` 由 `factory/provider_config.rs:61`（`resolve_provider_fields`）产生 → `AppError::BadRequest`，Display 自动加 `Bad request:` 前缀（`nomifun-common/src/error.rs:14`）。另一处 `services/provider_health.rs:68` 只在设置页手动健康检查触达，非首页来源。
- **两条 toast**：`NomiSendBox` 打开会话时并发触发 (1) mount warmup（`.catch` 弹 toast）+ (2) 自动发送首条消息（`catch` 弹 toast），各 build 一次 Nomi agent → 同一错误 → 弹两次。前端 `getConversationRuntimeWorkspaceErrorMessage`（`conversationCreateError.ts`）把 `backendMessage` 原样返回，无 i18n 映射。
- **无回退**：`resolve_provider_fields` 找不到 provider 直接硬报错，没有 fallback。
- **可复用先例**：
  - `AppError` 已有 `Conflict`（409）+ 每变体 `error_details() -> Option<Value>`（`WorkspacePathEdgeWhitespace` 携带结构化 `details`）；前端 `BackendHttpError` 已解析 `code`/`backendMessage`/`details`。
  - 删除拦截先例：`CompanionService::delete_figure`（`companion/src/service.rs:718`）在 `figure_user_count>0` 时返回 `Conflict`。
  - **`resolve_default_model(&provider_repo)`**（`nomifun-ai-agent` `knowledge_completer.rs:118`，`lib.rs:59` 导出）：取首个已启用 provider+model，无则 `None` / 清晰错误。

### 分层关键约束

- `nomifun-system`（`ProviderService`、provider 路由）是**低层 crate**，仅依赖 auth/common/db/net/api-types，看不到伙伴/IDMM/编排。
- 只有顶层 `nomifun-app` 依赖全部子系统，在 `router/state.rs:299` 构造 `ProviderService`。
- Provider 删除路由定义在 `nomifun-system/src/routes.rs:70`（`delete_provider`）。

## 2. 目标 / 非目标

### 目标

1. **根因**：模型供应商被"硬绑定"功能使用时，禁止删除；提示被哪些功能使用并给跳转入口去解绑/更换（硬阻止，无强制删除逃生通道）。硬绑定范围 = 桌面伙伴、对外伙伴、智能决策(IDMM)、智能编排(fleet)。
2. **普通会话不强制关联**：会话绑定的 provider 没了时优雅回退，不再硬报错；前端自愈把会话模型改为用户默认（`nomi.defaultModel`）或首个可用，并轻提示。
3. **友好文案兜底**：任何残留的"provider 找不到 / 无可用模型"报错，前端按 code→i18n 展示可操作文案，绝不出现裸 `prov_xxx`。
4. **软引用自动清理**：删除（未被硬绑定拦截）后，把死 id 从软引用（`nomi.defaultModel` 由前端清、`knowledge.autogenModel`、`agent.model_failover`）剔除。

### 非目标

- 不改引用的存储模型（仍是值对象快照，不引入 SQL 外键）。
- 不拦截普通会话、渠道默认模型、知识库、Guid 默认、失败回退队列的删除（它们自愈或被组件 B 覆盖）。
- 不提供"强制删除并解绑"高级通道（用户明确选择仅硬阻止）。

## 3. 组件 A — 删除保护（in-use 拦截 + 软引用清理）

### A.1 数据形状（`nomifun-api-types`）

```rust
pub struct ProviderUsage {
    pub feature: ProviderUsageFeature,
    pub label: String,            // 人类可读：伙伴名 / "智能决策·备份模型" / 会话标题 / fleet 名
    pub target_id: Option<String> // 可跳转的目标 id（伙伴 id / fleet id / 会话 id），无则 None
}

pub enum ProviderUsageFeature { DesktopCompanion, PublicCompanion, SmartDecision, Orchestrator }
```

序列化为 camelCase 以匹配前端。

### A.2 各 crate 扫描方法（谁的存储谁扫）

每个子系统在自己 crate 内暴露 `async fn providers_in_use(provider_id: &str) -> Result<Vec<ProviderUsage>, AppError>`：

- `nomifun-companion`：遍历 `companions/*/config.json`，命中 `model.provider_id`（含 shared learn/evolve）→ `DesktopCompanion`，label=伙伴名，target_id=伙伴 id。
- `nomifun-public-agent`：遍历 `public-agents/*/config.json`，命中 `model.provider_id` → `PublicCompanion`，label=对外伙伴名，target_id=agent id。
- `nomifun-idmm`：扫 `client_preferences: idmm_backup_provider_id`（命中→label="智能决策·备份模型"，target_id=None）；扫 `conversations.extra.idmm` / `terminal_sessions.idmm` 内各 watch 的 `bypass_model.provider_id`（命中→label=会话/终端标题，target_id=会话 id）。
- `nomifun-orchestrator`：`SELECT DISTINCT fleet_id, fleet_name FROM fleet_members WHERE provider_id=?` → `Orchestrator`，label=fleet 名，target_id=fleet id。

### A.3 app 层聚合 + 注入

- 在低层（`nomifun-system` 或 `nomifun-api-types`）定义 trait：
  ```rust
  #[async_trait]
  pub trait ProviderDeletionCoordinator: Send + Sync {
      async fn usages(&self, provider_id: &str) -> Result<Vec<ProviderUsage>, AppError>;
      async fn cleanup_soft_refs(&self, provider_id: &str) -> Result<(), AppError>;
  }
  ```
- 具体实现放 `nomifun-app`（唯一能看到全部子系统），聚合四路 `providers_in_use`，并实现软清理（清 `knowledge.autogenModel`、`agent.model_failover` 里的死 id；`nomi.defaultModel` 是前端配置由前端清）。
- 在 `state.rs:299` 把 `Arc<dyn ProviderDeletionCoordinator>` 注入 `ProviderService`（构造签名新增该依赖）。
- `ProviderService::delete` 改为：先 `coordinator.usages(id)`；非空 → 返回 `AppError::ProviderInUse { usages }`；空 → `repo.delete(id)`，成功后 `coordinator.cleanup_soft_refs(id)`（清理失败仅告警不回滚删除）。

> 备选：若注入 trait 改动面偏大，可改为把 DELETE handler 上移到 app 层。默认采用 trait 注入（保持路由不动、尊重分层）。最终以实现计划为准。

### A.4 错误传输（`nomifun-common/src/error.rs`）

仿 `WorkspacePathEdgeWhitespace` 新增变体：

```rust
AppError::ProviderInUse(ProviderInUseDetails)   // { usages: Vec<ProviderUsage> }
```

- `status_code` → `409 CONFLICT`
- `error_code` → `"PROVIDER_IN_USE"`
- `error_details` → `Some(json!({ "usages": [...] }))`
- `Display` → 简短英文（前端按 code i18n，不直接展示）。

`ProviderInUseDetails` / `ProviderUsage` 需在 `nomifun-common` 可见（放 `nomifun-common` 或让 `nomifun-common` 依赖 `nomifun-api-types`；以实现计划确认最省依赖的位置）。

### A.5 前端（`ModelModalContent.tsx` + i18n）

- `removePlatform` catch：`isBackendHttpError(err) && err.code === 'PROVIDER_IN_USE'` → **不弹 generic toast**，打开 Arco `Modal`，按 `feature` 分组渲染 `details.usages`，每项一个「去解绑/更换」按钮跳到对应路由；乐观更新回滚（现已有 `mutate()`）。
- feature→路由映射（计划阶段核实精确路由）：
  - `DesktopCompanion` → `/companion`（CompanionModelControl）
  - `PublicCompanion` → 对外伙伴设置页
  - `SmartDecision` → IDMM 设置入口
  - `Orchestrator` → 编排 fleet 设置
- 新 i18n（`settings.providerInUse.*`）：`title`、`desc`、`gotoUnbind`、`feature.desktopCompanion` 等 feature 名。zh-CN + en-US 双份。

## 4. 组件 B — 普通会话优雅回退

### B.1 后端"永不崩"兜底（`factory/nomi.rs`）

把 `nomi.rs` 约 235 行处直接的
`resolve_provider_fields(&deps.provider_repo, &deps.encryption_key, provider_id, &model_id).await?`
换成 fallback-aware 解析（新增 helper，`resolve_provider_fields` 本身保持严格，供 health/编排/IDMM/knowledge 等显式调用方不变）：

1. 若 `provider_id` 非空且 `provider_repo.find_by_id(provider_id)` 命中 → 正常 `resolve_provider_fields`。
2. 否则（provider_id 为空 **或** 行不存在）→ `resolve_default_model(&deps.provider_repo)`（首个已启用 provider+model）→ 用它 `resolve_provider_fields`；标记 `substituted = true`。
3. 若 `resolve_default_model` 也返回 None（无任何可用模型）→ 返回组件 C 的友好 coded 错误。

用 `find_by_id().is_none()` 判定，不依赖错误字符串。该兜底在**工厂唯一收口处**，覆盖普通会话/伙伴/渠道等所有 Nomi 调用方。工厂 build 结果新增 `Option<ProviderWithModel> substituted_model` 供上层感知（读路径不写库）。

### B.2 前端自愈 + 轻提示（honoring 用户默认）

`nomi.defaultModel` 是前端配置、后端读不到，因此让前端负责"改回用户默认"：

- `useNomiModelSelection` / 会话加载路径：当会话 `conversations.model.provider_id` 不在当前 provider 列表中（选择器已过滤缺失 provider）时：
  1. 计算目标模型 = `nomi.defaultModel` 指向的可用模型；无效则第一个可用模型。
  2. 通过既有会话模型持久化路径（NomiModelSelector `onChange` / updateConversation）写回 `conversations.model`。
  3. 轻提示 toast「已回退到默认模型 {name}」。
- 这样既兑现"默认模型→首个可用"，又清掉悬空、下次不再 stale。后端 B.1 是覆盖前端未走到路径（渠道/伙伴绑定/其他客户端）的安全网。

## 5. 组件 C — 友好文案兜底

- 后端：`resolve_default_model` 无可用模型的终态错误，改为带专用 code（新增 `PROVIDER_UNAVAILABLE`，或复用前端已有的 `USER_LLM_PROVIDER_MODEL_NOT_FOUND` 语义）+ 清晰消息。确保该 code 随 warmup / send 的 `AppError` 传到前端。
- 前端：两条 toast 链路（`conversationCreateError.ts` 的 `getConversationRuntimeWorkspaceErrorMessage`；`NomiSendBox` warmup/send 的 `catch`）按 code→i18n 映射为「当前没有可用的模型供应商，请到『模型&Agent』添加并启用后重试」+ 跳转链接，**不再出现 `prov_xxx`**。
- 兜底：凡仍直接弹 `backendMessage` 的 provider 报错点，加 code→i18n 回退，裸 id 永不触达用户。

## 6. 测试策略

### Rust 单测

- 四路 `providers_in_use`：命中 / 未命中 各一。
- app 层聚合器：多来源合并、去重（同一 provider 多处使用）。
- `AppError::ProviderInUse` 序列化：`status=409`、`code=PROVIDER_IN_USE`、`details.usages` 结构正确（仿 error.rs 现有 into_response 测试）。
- `cleanup_soft_refs`：清 `knowledge.autogenModel`、`agent.model_failover` 中死 id，保留其余。
- B.1 fallback：provider 存在→原样；provider 缺失→回退首个可用并置 `substituted`；无可用→`PROVIDER_UNAVAILABLE`。
- `ProviderService::delete`：有硬绑定→`ProviderInUse` 且未删；无绑定→删除且触发清理。

### 前端

- `removePlatform` 的 `PROVIDER_IN_USE` 分支：渲染 Modal + 跳转项，不弹 generic toast。
- 自愈：stale provider 会话加载→写回默认模型 + 轻提示。
- code→i18n：`PROVIDER_UNAVAILABLE` / `PROVIDER_IN_USE` 映射到 zh-CN / en-US 文案，无裸 id。

### 手动验证（对应截图场景）

删一个被普通会话引用的 provider → 打开该会话：warmup+send 不再弹两条裸 id toast；会话模型自愈为默认并提示。删一个被伙伴/IDMM/编排引用的 provider → 弹 Modal 列出用途 + 跳转，删除被阻止。

## 7. 实现计划待钉点（转 writing-plans 时确认）

- `ProviderDeletionCoordinator` trait 与 `ProviderUsage`/`ProviderInUseDetails` 的最省依赖落位（`nomifun-common` vs `nomifun-api-types`）。
- 是否采用 trait 注入 vs DELETE handler 上移 app 层（默认 trait 注入）。
- 前端各 feature 解绑页精确路由（对外伙伴 / IDMM / 编排）。
- `substituted_model` 从工厂 build 结果到 send 响应的传递是否需要（前端自愈已足够时可省后端透传）。
- 渠道 `assistant.{platform}.defaultModel` 软清理是否纳入（默认纳入 cleanup，不纳入拦截）。
