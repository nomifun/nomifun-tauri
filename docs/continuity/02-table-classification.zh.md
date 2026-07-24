# 逐表 ID 分类与逻辑关联设计

状态：`V3 BASELINE CLASSIFICATION`

本表覆盖 v3 baseline registry 当前列出的 64 张最终产品持久表，不含 legacy
migration 过程表、TEMP 表、SQLite 内部表和 migration metadata。文中提到的
v2 只表示历史来源，不表示 v3 仍兼容或迁移 v2 数据。

## 所有表共同结构

以下结构不得遗漏：

```sql
id INTEGER PRIMARY KEY AUTOINCREMENT
```

`id` 是本表技术主键，不自动成为业务 ID。S/L/N/D 分类只决定额外业务键
和逻辑关联方式，不改变统一的自增主键。

v3 产品 schema 不创建物理外键：

```text
禁止 FOREIGN KEY
禁止 REFERENCES
禁止数据库级 ON DELETE / ON UPDATE cascade
禁止 *_row_id
```

逻辑关联字段规则：

- 父表有具名稳定业务 ID：子表保存同名业务 ID；
- 纯内部关系、依附和事件行不把技术 `id` 传播到其他表；需要定位时使用
  owner UUIDv7 + sequence、自然键或复合唯一条件；
- 当前 v3 baseline 不存在指向其他表技术 `id` 的 `INTEGER` 逻辑关联；
- 每个逻辑关联字段必须有索引、registry 条目、删除策略和 orphan audit；
- 同一关系不同时保存业务 ID 与本地自增 ID。

逻辑关联的权威写路径是对应领域的 repository/service。它必须在同一事务
中检查父记录、写入关联并执行登记的删除策略。v3 不增加 trigger 来模拟
物理 FK，因此 raw SQL 可以绕过该契约；raw SQL 只应用于受控 fixture、
诊断或维护，且必须由 orphan audit 检查结果兜底。测试不能把“任意 raw
SQL 插入应由 SQLite 拒绝”当作逻辑外键目标。

## 分类说明

- **S（Stable）**：自增 `id` + 具名 `*_id TEXT UNIQUE NOT NULL`。具名
  业务 ID 使用无前缀标准 UUIDv7。
- **L（Local/Internal）**：只有自增 `id`，但该 `id` 仅为本表技术主键；
  行通过 owner 业务 ID + sequence/自然键/复合条件定位，不产生整数业务 ID。
- **N（Natural/Composite）**：有自增 `id`，业务唯一性由自然字段的
  `UNIQUE` 或复合 `UNIQUE` 表达。
- **D（Dependent）**：有自增 `id`，通过 owner 逻辑关联、singleton key
  或一对一 `UNIQUE` 表达依附关系。
- **X（External field）**：第三方或协议值，不套本系统业务 ID 规则。

## S：需要稳定业务身份的表

| 表 | 固定键结构 | 主要逻辑关联/原因 |
| --- | --- | --- |
| `users` | `id` + `user_id` | 认证主体、JWT owner、跨 store 身份 |
| `conversations` | `id` + `conversation_id` | API、消息、workspace、导入导出 |
| `messages` | `id` + `message_id` | 使用 `conversation_id` 逻辑关联 conversation |
| `terminal_sessions` | `id` + `terminal_id` | PTY、WS、knowledge、runtime |
| `providers` | `id` + `provider_id` | 配置快照、模型偏好、creation |
| `agent_metadata`（custom） | `id` + `agent_id` | custom agent 稳定身份；builtin 使用 catalog natural key |
| `agent_execution_templates` | `id` + `execution_template_id` | 可复用配置、API、workspace |
| `agent_executions` | `id` + `execution_id` | 执行聚合根、事件、conversation link |
| `agent_execution_participants` | `id` + `participant_id` | 执行参与者快照、step assignment、attempt history |
| `agent_execution_steps` | `id` + `step_id` | 执行图节点、dependency、attempt、conversation link |
| `agent_execution_attempts` | `id` + `attempt_id` | durable attempt、事件和运行时恢复 |
| `agent_execution_template_participants` | `id` + `template_participant_id` | 模板参与者快照与模板实例化 |
| `knowledge_bases` | `id` + `knowledge_base_id` | 文件根目录、binding、preset、导入导出 |
| `attachments` | `id` + `attachment_id` | 文件实体、下载、backup、requirement |
| `remote_agents` | `id` + `remote_agent_id` | 远程设备、协议连接和密钥生命周期 |
| `presets`（user） | `id` + `preset_id` | 用户配置、API、快照、导入 |
| `workshop_canvases` | `id` + `canvas_id` | 文件夹、canvas.json、缩略图、clone |
| `workshop_assets` | `id` + `asset_id` | 文件、缩略图、capability URL、导入导出 |
| `requirements` | `id` + `requirement_id` + `display_no` | API/MCP/Agent/attachment 使用稳定 ID；`display_no` 只供人类展示 |
| `channel_plugins` | `id` + `channel_plugin_id` | 渠道配置、运行时注册、user/session 关联 |
| `channel_users` | `id` + `channel_user_id` | 授权主体；平台 user ID 仍是外部值 |
| `channel_sessions` | `id` + `channel_session_id` | 平台会话与 conversation/workspace 关联 |
| `conversation_artifacts` | `id` + `conversation_artifact_id` | Conversation/Cron 产物、API 与事件定位 |
| `cron_jobs` | `id` + `cron_job_id` | 调度配置、API、运行恢复 |
| `cron_job_runs` | `id` + `cron_job_run_id` | 运行记录；使用 `cron_job_id` 逻辑关联 Cron Job |
| `mcp_servers` | `id` + `mcp_server_id` | 配置、Conversation junction、Gateway/runtime 定位 |
| `webhooks` | `id` + `webhook_id` | 出站配置与 Tag Setting 逻辑关联 |
| `connector_credentials` | `id` + `credential_id` | 凭据槽、API 与跨模块引用 |
| `creation_tasks` | `id` + `creation_task_id` | API/Gateway/UI/Workshop Asset 使用稳定任务定位；`remote_task_id` 是外部字段 |
| `idmm_interventions` | `id` + `intervention_id` | 审计记录与 API/wire 定位 |
| `knowledge_bindings` | `id` + `knowledge_binding_id` | 绑定配置、side-store 与管理接口定位 |
| `preset_tags` | `id` + `preset_tag_id` | 用户标签与 Preset 关系定位；`key` 保留自然语义 |

以上每个 `id` 都是 `INTEGER PRIMARY KEY AUTOINCREMENT`；表中的具名业务
ID 才是跨数据集身份。v3 不导入旧 `prefix_UUIDv7`、短 ID 或数字业务 ID。

## L：只有内部技术身份的表

以下表仍统一使用自增 `id`，但不增加 UUIDv7 业务 ID：

| 表 | 逻辑关联和说明 |
| --- | --- |
| `agent_execution_events` | 使用 `execution_id`，业务定位为 execution + sequence |
| `conversation_execution_links` | 使用 `conversation_id` + `execution_id` |

L 表不会把本表 `id` 作为跨表或 wire locator。v3 restore/clone 在目标库
重新分配技术 `id`，同时保留或重建 owner UUIDv7、sequence、自然键和复合
唯一条件。

## N：自然键/复合唯一约束表

以下表都保留 `id INTEGER PRIMARY KEY AUTOINCREMENT`。表中所列字段建立
`UNIQUE`，但不建立物理外键：

| 表 | 业务唯一约束 |
| --- | --- |
| `agent_execution_step_dependencies` | `(execution_id, blocker_step_id, blocked_step_id, introduced_in_revision)` |
| `channel_pairing_codes` | `code` |
| `client_preferences` | `key` |
| `conversation_creation_keys` | `creation_key` |
| `conversation_delivery_receipts` | `operation_id`；`message_id` 是逻辑关联 |
| `conversation_mcp_servers` | `(conversation_id, mcp_server_id)` |
| `knowledge_binding_bases` | `(knowledge_binding_id, knowledge_base_id)` |
| `knowledge_tags` | `key` |
| `message_correlations` | `(conversation_id, turn_message_id, message_type, correlation_key)`；`turn_message_id` 是 wire-scoped UUIDv7 owner token，不是 `messages.message_id` 父引用 |
| `model_profiles` | `(provider_id, model)` |
| `oauth_tokens` | `server_url` |
| `preset_agent_preferences` | `(preset_id, agent_id)` |
| `preset_examples` | `(preset_id, locale, sort_order)` |
| `preset_knowledge_bases` | `(preset_id, knowledge_base_id)` |
| `preset_localizations` | `(preset_id, locale)` |
| `preset_model_preferences` | `(preset_id, rank)` |
| `preset_skill_bindings` | `(preset_id, skill_name, binding)` |
| `preset_tag_bindings` | `(preset_id, preset_tag_id, dimension)` |
| `preset_targets` | `(preset_id, target_kind)` |
| `requirement_tags` | `tag`；`paused_requirement_id` 是可空的 stable Requirement 逻辑引用 |
| `skill_tags` | `skill_name` |
| `tag_settings` | `tag`；`webhook_id` 是可空的 UUIDv7 逻辑引用 |

表中 `execution_id`、`conversation_id`、`message_id`、`provider_id`、
`preset_id`、`knowledge_base_id` 等指向 S 表的业务 ID；`step_id`、
`participant_id`、`attempt_id`、`requirement_id` 等也都是指向 S 表的
UUIDv7 业务 ID；`cron_job_id` 也是指向 Cron Job 稳定业务 ID 的 `TEXT`；
`mcp_server_id`、`knowledge_binding_id`、`webhook_id` 同样是指向 S 表
具名 UUIDv7 业务字段的 `TEXT`。这些均为逻辑关联，不指向任何表的技术
`id`。

`message_correlations.turn_message_id` 是例外的协议 owner token：它使用
固定的裸 UUIDv7 结构并建立索引，但不要求 `messages` 中存在同值行。流式
处理采用 reserve-then-project，续接请求可能先预留 correlation，随后才
投影普通消息；因此它不属于 `messages.message_id` 的父子关系，也不应被
orphan audit 当作缺失父行。

自然键冲突必须显式报告。导入器不能为了成功自动给 URL、model、skill
或 tag 增加随机后缀。

## D：依附表、单例和缓存

以下表都保留 `id INTEGER PRIMARY KEY AUTOINCREMENT`：

| 表 | 依附约束 |
| --- | --- |
| `acp_session` | `conversation_id UNIQUE`；`acp_session_id` 是外部字段 |
| `companion_access_token` | `companion_id UNIQUE` 作为跨 store owner |
| `installation_identity` | `singleton_key UNIQUE`；`owner_user_id` 逻辑关联 user |
| `preset_knowledge_policy` | `preset_id UNIQUE` |
| `preset_user_state` | `preset_id UNIQUE`，本地状态 |
| `requirement_display_sequence` | `singleton_key UNIQUE`，不依赖 `id = 1` |
| `system_settings` | `singleton_key UNIQUE`，不依赖 `id = 1` |
| `terminal_scrollback` | `terminal_id UNIQUE`，缓存/blob |

单例表也必须由 SQLite 自增生成 `id`，通过固定 `singleton_key` 限制只有
一个逻辑实例，不硬编码主键值。

## X：外部字段审计

以下字段从旧实体 ID registry 移出：

```text
messages.msg_id
message_correlations.correlation_key
acp_session.acp_session_id
channel_users.platform_user_id
channel_sessions.chat_id
creation_tasks.remote_task_id
idmm_interventions.target_id（按 target_kind 的多态逻辑契约）
provider request/call IDs
client preference JSON 中的第三方 locator
```

它们可以是数字、短字符串、带前缀字符串或平台自定义格式，只按各字段
协议校验。

## v3 baseline 不变量

1. 64 张产品持久表全部有
   `id INTEGER PRIMARY KEY AUTOINCREMENT`；
2. S 表的具名业务 ID 为 `UNIQUE NOT NULL` 的标准无前缀 UUIDv7；
3. schema 中不存在 `FOREIGN KEY`、`REFERENCES`、数据库级 cascade 或
   `*_row_id`；
4. 每个逻辑关联字段都有索引、registry 条目、删除策略和 orphan audit；
5. 同一关系不得同时保存父表业务 ID 与父表技术 `id`；
6. N 表使用额外 `UNIQUE` 表达业务唯一性；
7. 单例表使用 `singleton_key UNIQUE`，不依赖 `id = 1`；
8. X 字段不进入业务 UUIDv7 parser；
9. 文件路径通过 safe file key 生成，不直接拼接任意 ID；
10. restore/clone 不复制源数据集的自增 `id`，也不把技术 `id` 当作逻辑
    引用；关系通过业务 UUIDv7、自然键或外部 ID 重建。
