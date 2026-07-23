# ID 体系

本文是 NomiFun v3 ID 体系的架构权威契约，适用于产品数据库表、Rust 领域模型、
HTTP/WebSocket/MCP 协议、运行时注册表、受管文件、备份与导入。面向贡献者的
强制执行规范见
[数据与标识符规范](../contributing/data-and-identifier-standards.zh.md)。
`continuity/` 文档只提供历史/审计背景，不能覆盖本文契约。

## 核心规则

v3 明确区分五类概念：

```text
表技术主键
稳定业务 ID
内部技术行
自然键/外部 ID
操作 token
```

它们不得混用。

1. 每张由 NomiFun 定义的产品持久表，都必须有统一的技术主键：

   ```sql
   id INTEGER PRIMARY KEY AUTOINCREMENT
   ```

2. 需要跨数据库、跨设备、跨文件、API、事件或受管 store 稳定定位的实体，
   增加 `user_id`、`conversation_id`、`message_id`、`mcp_server_id`、
   `webhook_id`、`credential_id`、`creation_task_id` 等具名裸 UUIDv7
   业务字段。
3. 从不离开所属持久化子系统的关系、单例、缓存和事件行只使用整数 `id`
   作为内部技术身份，不为形式统一而额外生成 UUID，也不把该值升级为产品
   wire locator。
4. 表间关系由 Repository/Service 维护为逻辑外键；产品 schema 不包含物理
   外键、`REFERENCES`、trigger 或数据库级联。
5. v3 是新的数据集代际。历史数据集整体重置，不迁移旧行和旧 ID 格式。

## 技术主键

所有产品表，包括关系表、值对象表、单例表、缓存表和依附表，都必须声明：

```sql
id INTEGER PRIMARY KEY AUTOINCREMENT
```

SQLite 内部表、migration metadata 和临时表不属于产品持久表。

技术 `id` 的语义是：

- 当前 active dataset 内、本表内的行身份；
- 表主键和索引定位入口；
- Rust 中使用 `i64`；
- 绝不以技术行主键的身份跨越 API、事件、受管文件名/manifest 或数据集边界；
- 不是跨数据集身份、公开 locator、文件名契约或分布式 ID，也不写入稳定
  事件或文件引用；
- 不得依赖整数连续。

`AUTOINCREMENT` 用于固定所有产品表的结构，并防止已经签发过的正整数行 ID
被复用；它不会让该整数自动成为稳定业务 ID。

## 稳定业务 ID

只有确实需要脱离本地数据库行仍保持身份的实体，才增加具名业务字段：

```text
user_id
conversation_id
message_id
provider_id
execution_id
knowledge_base_id
```

业务 ID 使用裸、规范的 UUIDv7：

```text
0190f5fe-7c00-7a00-8000-000000000003
```

固定契约如下：

- 标准 `8-4-4-4-12` 结构，共 36 字符；
- 小写十六进制；
- RFC UUID variant；
- UUID version 7；
- 无前缀、无后缀、无花括号、无紧凑格式、无空白、无替代分隔符；
- JSON 中为字符串，SQLite 中为 `TEXT`；
- 保留完整 128 bit UUID，不截断。

实体类型由字段名、表语义和 Rust/TypeScript 领域类型表达，不从 UUID 字符串
内容推断。

稳定实体表的典型结构：

```sql
CREATE TABLE conversations (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    conversation_id TEXT NOT NULL UNIQUE
                    CHECK (
                        length(conversation_id) = 36
                        AND lower(conversation_id) = conversation_id
                        AND conversation_id
                            GLOB '????????-????-7???-[89ab]???-????????????'
                        AND replace(conversation_id, '-', '')
                            NOT GLOB '*[^0-9a-f]*'
                    )
);
```

v3 中的稳定实体包括用户、会话、消息、终端会话、Provider、Requirement、
Agent Execution 及模板、Agent Execution
Participant/Step/Attempt/Template Participant、知识库、附件、Remote Agent、
用户 Preset、Workshop 画布/资产，以及 Channel Plugin/User/Session。
这些实体分别使用具名业务字段；其中 Requirement 使用 `requirement_id`，
并另有只供人类展示的 `display_no`，Agent Execution 和 Channel 子实体使用
`participant_id`、`step_id`、`attempt_id`、`template_participant_id`、
`channel_plugin_id`、`channel_user_id`、`channel_session_id`。

## 内部技术行

部分关系表、单例表、缓存表和事件表不需要独立业务身份，仍必须拥有统一的
表 `id`，但该值只在当前 SQLite 数据集内部有效。它不是产品 locator、数字
字符串、公开 API 字段、事件身份、受管文件名或可移植 backup 身份。

凡是由产品 API、运行时注册表、受管文件、备份或其他 side-store 定位的实体，
即使生命周期只在本安装内，也使用具名裸 UUIDv7 业务字段。当前 v3 baseline
包括 `mcp_server_id`、`webhook_id`、`credential_id`、`creation_task_id`、
`conversation_artifact_id` 和 `intervention_id`；表内 `id` 仍然只是技术主键。

当前产品 wire 契约不引入整数业务 ID，也不引入通用 `id` alias。未来若内部
子系统确实需要整数 handle，必须限制在该子系统内部，不能越过产品边界。

## 逻辑外键

v3 全面移除产品 schema 中的物理外键。DDL 不得出现：

```text
FOREIGN KEY
REFERENCES
CREATE TRIGGER
ON DELETE CASCADE
ON UPDATE CASCADE
*_row_id
```

一条关系只保存一个引用字段：

- 父实体有稳定业务 ID：保存同名 UUIDv7 业务字段；
- 内部关系、依附和事件行通过父实体业务 ID + sequence、自然键或复合条件
  确定作用域，不把技术 `id` 传播到其他表；
- 值由外部系统签发：保存能说明来源和用途的不透明外部 ID。

当前 v3 baseline 不存在指向其他表技术 `id` 的 `INTEGER` 关系。

稳定父实体示例：

```sql
CREATE TABLE messages (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    message_id      TEXT NOT NULL UNIQUE,
    conversation_id TEXT NOT NULL
);

CREATE INDEX idx_messages_conversation_id
    ON messages(conversation_id);
```

稳定 Cron 父实体示例：

```sql
CREATE TABLE cron_jobs (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    cron_job_id TEXT NOT NULL UNIQUE
);

CREATE TABLE cron_job_runs (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    cron_job_run_id TEXT NOT NULL UNIQUE,
    cron_job_id     TEXT NOT NULL
);

CREATE INDEX idx_cron_job_runs_cron_job_id
    ON cron_job_runs(cron_job_id);
```

`messages.conversation_id` 逻辑指向 `conversations.conversation_id`；
`cron_job_runs.cron_job_id` 逻辑指向 `cron_jobs.cron_job_id`。两者都不向
SQLite 声明物理关系。

同一关系禁止同时保存 `conversation_id` 与 `conversation_row_id`，也禁止任何
等价的业务 ID/行 ID 双轨字段。

### 应用层完整性

移除物理外键不等于取消完整性。每个逻辑关联都必须登记：

- 子表和字段；
- 父表和目标字段；
- 类型、可空性和作用域；
- 必需索引；
- 删除策略：`RESTRICT`、应用层 `CASCADE`、`SET_NULL` 或
  `KEEP_HISTORY`；
- restore/clone 重建策略；
- orphan audit 查询。

Repository/Service 必须在显式事务中校验父记录、写入关联数据并执行删除策略。
这里的 `CASCADE` 只表示应用层事务策略，不是 SQLite cascade 或 trigger。
批量 restore/import 后必须运行完整 orphan audit。业务代码不得绕过这一边界
直接写逻辑关联列。

## 自然键、外部 ID 与 Token

Skill 名、Extension slug、模型名、URL、locale、tag 和 singleton key 等自然键
保留各自领域格式。关系表和单例表仍有自增 `id`，业务唯一性由额外 `UNIQUE`
约束表达。

`acp_session_id`、`platform_user_id`、`platform_chat_id`、
`remote_task_id` 和 Provider request ID 等外部标识保持不透明，只按来源协议
校验。

请求 ID、幂等键、capability nonce、workspace token 等短生命周期操作值不是
实体 ID。它们必须使用用途明确的字段名，不能意外升级为表主键或逻辑业务 ID。

## Rust 与协议边界

`nomifun-common` 负责裸 UUIDv7 的生成和严格校验。稳定业务 ID 使用包裹同一
规范字符串的小型领域 newtype；技术行 ID 只允许在 repository/storage
实现内部使用 `i64`，不是领域或 wire 标识。

所有边界统一遵循：

- 稳定业务 ID 是规范 UUIDv7 字符串；
- 技术行 ID 留在 repository/storage 实现内部；
- 外部 ID 是显式类型的不透明值；
- 非法值直接失败，不得变成 `0`、空字符串或另一类 ID；
- Route、DTO、缓存、事件和文件 manifest 与领域模型使用相同的业务或外部
  字段类型；技术 `id` 绝不是这些边界上的可移植值。

## v3 hard reset、备份与恢复

v3 hard reset 不迁移历史数据集。

启动时必须在打开产品数据库之前：

1. 获取 dataset/reset lock；
2. 检测数据集契约和 generation；
3. 已经是 v3 时正常继续；
4. 历史或不兼容数据集整体移动到 retired/quarantine 位置；
5. 创建全新的空 v3 数据集和 baseline schema；
6. 写入并完成 reset receipt 后，才允许对外服务。

禁止逐表转换、dual-read、alias 列、legacy-ID 映射或选择性复制业务数据。
Reset 失败必须终止启动，不能在新旧状态混合时继续运行。用户自有的外部
workspace 不删除，但 v3 不延续其历史数据库引用。

v3 restore 只接受 v3 backup manifest。Restore 保留全部稳定业务 UUIDv7 并
重建技术 `id`；逻辑关系从业务 UUIDv7、自然键、外部 ID 和登记的 JSON/
side-store 引用重建，不读取源库行号。Clone 同样保留输入中的业务 UUIDv7，
不会 mint 或隐式重写业务 UUID；如果与目标已有业务 UUID 冲突，必须
fail-closed，不能留下部分写入。技术 `id` 只属于当前数据集，绝不能作为
portable graph identity。

## 新增带 ID 的实体

新增表或实体时：

1. 产品表必须增加 `id INTEGER PRIMARY KEY AUTOINCREMENT`；
2. 判断该实体是否确实需要跨数据集身份；
3. 需要时增加一个具名、唯一的裸 UUIDv7 业务字段和领域 newtype；
4. 不需要时让 `id` 保持内部；必要定位使用 owner + sequence、自然键或复合条件；
5. 将每条关系分类为业务、自然或外部逻辑关联，禁止指向其他表技术 `id`；
6. 在 registry 中登记索引、删除/重建策略和 orphan audit；
7. 为所选表示增加 schema 与协议边界测试。
