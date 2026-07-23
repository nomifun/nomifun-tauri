# 分层 ID / Schema 目标架构

状态：`ACCEPTED`

## 1. 目标

目标是让数据库结构固定、查询直接、跨账号和跨平台的数据边界清晰：

```text
表技术主键 ≠ 业务 ID ≠ 自然键 ≠ 外部 ID ≠ 操作 token
```

只有确实需要跨库、跨文件、公开 URL、导入导出或分布式写入的字段使用
无前缀、标准小写连字符 UUIDv7：

```text
0190f5fe-7c00-7a00-8000-000000000003
```

其结构固定为 36 字符、标准 `8-4-4-4-12` 连字符形式。类型由固定字段名、
表语义和 Rust/TypeScript 领域类型表达，不从 UUID 内容推断实体类型。

## 2. 所有持久表的固定主键

所有由 NomiFun 定义的产品持久表，必须包含且只使用本表的技术主键：

```sql
id INTEGER PRIMARY KEY AUTOINCREMENT
```

这条规则覆盖：

- S：具有稳定业务 ID 的实体表；
- L：只有本地技术身份的表；
- N：自然键/复合唯一约束表；
- D：依附表、单例表、缓存和运行时状态表；
- 关系表和值对象表。

SQLite 内部表、migration metadata、TEMP 表不属于产品持久 schema。

`id` 的语义是：

- 当前数据库内、本表内的技术行身份；
- 表主键和索引定位入口；
- 不自动成为业务 ID；
- 不用于跨数据库、跨设备、文件路径或公开协议匹配；
- 不作为表间逻辑关联目标；
- 不出现在产品 API、事件、受管文件或其他 wire contract。

统一使用 `AUTOINCREMENT` 是为了让所有产品表的主键结构和“不复用”
语义固定。它不是分布式 ID，也不代表业务层可以依赖整数连续性。
SQLite 的 `INTEGER PRIMARY KEY` 本身已经是 rowid alias；`AUTOINCREMENT`
主要增加“不复用已用过的正整数”保证和少量 `sqlite_sequence` 成本，
不能宣称它比普通整数主键更快。

## 3. ID 与逻辑关联字段

### 3.1 具名业务 ID

只有业务需要独立于本地数据库行的稳定身份时，才增加具名字段：

```text
conversation_id
message_id
provider_id
knowledge_base_id
figure_id
```

字段通常为：

```sql
<business_name>_id TEXT NOT NULL UNIQUE
```

并使用 SQLite `CHECK` 与应用领域类型共同验证：

```sql
CHECK (
    length(<business_name>_id) = 36
    AND lower(<business_name>_id) = <business_name>_id
    AND <business_name>_id
        GLOB '????????-????-7???-[89ab]???-????????????'
    AND replace(<business_name>_id, '-', '')
        NOT GLOB '*[^0-9a-f]*'
)
```

### 3.2 逻辑外键：不再保存 `*_row_id`

v3 全面移除 SQLite 物理外键。产品 schema 不得出现：

```text
FOREIGN KEY
REFERENCES
ON DELETE CASCADE
ON UPDATE CASCADE
*_row_id
```

不再同时保存 `conversation_id` 和 `conversation_row_id`。这两个字段表达
的是同一条关系的两种定位方式，会造成 DTO、repository、查询、clone 和
调试中的重复转换。

逻辑关联字段只有一份，并按目标语义选择：

| 目标类型 | 子表字段 | 字段类型 | 例子 |
| --- | --- | --- | --- |
| 产品内可定位实体 | 父表具名业务字段名 | `TEXT` UUIDv7 | `messages.conversation_id` |
| 纯内部依附/关系/事件行 | 不引用其技术 `id`；由 owner UUIDv7 + sequence/自然键/复合唯一条件定位 | 按字段语义 | `execution_id + sequence` |
| 外部系统对象 | 外部协议字段名 | opaque | `acp_session_id` |

`cron_job_runs.cron_job_id` 是 `TEXT` UUIDv7，指向
`cron_jobs.cron_job_id`；`conversation_id` 指向
`conversations.conversation_id`。这些都不是物理 FK。

当前 v3 baseline 不包含指向其他表技术 `id` 的 `INTEGER` 逻辑关联。技术
`id` 只服务于本表存储和索引；凡是需要跨表、跨模块或跨 store 定位的实体，
都使用具名裸 UUIDv7、自然键或明确的外部 ID。

每个逻辑关联字段都必须建立索引：

```sql
CREATE INDEX idx_messages_conversation_id
    ON messages(conversation_id);
```

字段命名禁止 `conversation_row_id`、`job_row_id` 等双轨命名。父表的
业务 ID 和本地 `id` 不应在同一个关系中同时保存。

### 3.3 应用层完整性

取消物理 FK 不等于取消数据完整性。所有逻辑关联必须登记在
logical-reference registry 中，至少包含：

- 子表和字段；
- 父表和目标业务/自然字段（不得是技术 `id`）；
- 字段类型、是否必填、作用域；
- 必需索引；
- 删除策略：`RESTRICT`、`CASCADE`、`SET_NULL` 或 `KEEP_HISTORY`；
- clone/restore 的重建策略；
- orphan 审计查询。

repository/service 负责：

1. 在同一写事务中检查父记录存在；
2. 写入或更新子记录；
3. 删除父记录前执行登记的逻辑删除策略；
4. bulk import/restore 后运行完整 orphan audit；
5. 对 JSON、side-store 和数据库字段一起做引用闭包检查。

业务代码不得绕过 repository 直接写入逻辑关联字段。离线修复和诊断工具
可以直接读取，但必须输出 orphan、dangling reference 和修复结果。

## 4. 五类标识

### 4.1 表技术主键

每张产品持久表固定为：

```sql
id INTEGER PRIMARY KEY AUTOINCREMENT
```

关系表、自然键表、单例表和缓存表也必须保留 `id`，但不因此获得独立
业务 ID。关系表的业务唯一性由 `UNIQUE` 或复合 `UNIQUE` 表达。

### 4.2 具名业务 ID

真正需要分布式身份的实体，使用无前缀 UUIDv7。具体领域通过字段名和
newtype 区分，不通过值前缀区分。

### 4.3 自然键

名称、URL、配置 key、tag、model、locale、排序号和复合业务条件保持自然
键。自然键冲突必须显式报告，不能用随机后缀掩盖。

### 4.4 外部 ID

第三方签发的值原样保存：

```text
acp_session_id
platform_user_id
platform_chat_id
remote_task_id
provider_request_id
client_message_id
```

外部 ID 只按来源协议校验，不套 UUIDv7，不作为本系统业务 ID，也不作为
数据库物理外键。

### 4.5 操作/幂等/运行时 token

操作 token、幂等键、审批 token、effect token 和临时 workspace token
可以短、可以过期，但不能升级为实体主键或逻辑业务 ID。

## 5. 推荐 schema

稳定实体和逻辑引用：

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

CREATE TABLE messages (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    message_id      TEXT NOT NULL UNIQUE
                    CHECK (
                        length(message_id) = 36
                        AND lower(message_id) = message_id
                        AND message_id
                            GLOB '????????-????-7???-[89ab]???-????????????'
                        AND replace(message_id, '-', '')
                            NOT GLOB '*[^0-9a-f]*'
                    ),
    conversation_id TEXT NOT NULL
);

CREATE INDEX idx_messages_conversation_id
    ON messages(conversation_id);
```

稳定 Cron 实体和逻辑引用：

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

关系表和值对象：

```sql
CREATE TABLE preset_localizations (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    preset_id     TEXT NOT NULL,
    locale        TEXT NOT NULL,
    UNIQUE (preset_id, locale)
);
```

以上三个例子都没有 `FOREIGN KEY` 或 `REFERENCES`。

## 6. Rust / TypeScript 边界

### Rust

拆分为：

- `TechnicalRowId(i64)`：仅对应当前 repository 内部的表 `id`，不进入产品
  DTO 或逻辑关联；
- `ConversationId(Uuid)`、`MessageId(Uuid)` 等具名业务类型；
- `ExternalId(String)` / `ProviderRequestId(String)`；
- `OperationId(String)`；
- logical-reference registry 与 orphan audit。

业务 UUID 类型只验证标准小写 UUIDv7，不验证业务前缀。外部 ID 和
operation token 使用各自 parser，不经过 UUIDv7 parser。

Legacy `generate_prefixed_id()`、全局 prefix registry、物理 FK contract
和其 parser 在 v3 切换时直接删除，不进入新版本兼容层。

### TypeScript

```text
wire value
    -> field-specific parser
    -> typed NamedBusinessId / ExternalId / OperationId
```

规则：

- 业务 ID 只接受固定结构的小写 UUIDv7 字符串；
- 产品 wire 不接受或输出数据库技术 `id`；
- 逻辑关联字段按其父表 registry 条目解析，不猜测类型；
- 外部 ID 不做业务前缀推断；
- 历史记录不得进入 v3 mapper，dataset gate 必须先完成 hard reset。

## 7. 生成器策略

| 生成器 | 用途 |
| --- | --- |
| SQLite `AUTOINCREMENT` | 每张产品表的技术主键 |
| `new_business_id()` | 真正需要跨边界的标准 UUIDv7 业务键 |
| `new_operation_token()` | 幂等/操作/短生命周期 |
| `new_external_placeholder()` | 协议明确要求时生成外部关联值 |

## 8. 排序规则

ID 不承担业务排序责任：

- 时间使用 `created_at/updated_at`；
- 看板使用 `display_no/order_key/sort_seq`；
- 本地列表 tie-break 可以使用表自身 `id`；
- 跨库稳定排序使用单独的 `sort_key`；
- 不用字符串 `"1","2","10"` 排序。

## 9. v3 导入导出规则

本节只适用于 v3 数据集之间的 backup、restore、merge 和 clone，不接受
任何 v2 或更旧格式。

### Restore / Merge

- 有业务键的实体保留原业务键；
- 同一业务键、内容相同：幂等跳过；
- 同一业务键、内容不同：报告冲突，不静默覆盖；
- 所有目标表的 `id` 由目标 SQLite 重新自增分配；
- 逻辑关联按 registry 重新绑定，不复制源库整数 `id`；
- 自然键按表定义处理；
- 外部 ID 按字段语义保留。

### Clone

- 有业务身份的实体生成新的业务键；
- 所有表的本地 `id` 重新自增；
- 逻辑关联根据父实体映射重建；
- 文件路径使用专门的 safe file key；
- typed map 必须包含领域和字段，不能只按裸值映射。

## 10. v3 代际边界

v3 不兼容旧数据格式：

- 新 schema 只接受本文件定义的字段和 ID 类型；
- 分布式业务 ID 只接受无前缀标准 UUIDv7；
- 每张产品表的技术主键只接受整数 `id`；
- 不读取历史 `prefix_UUIDv7`、旧短 ID 或旧数字业务 ID（这些值仅属于
  retired/quarantine 的 legacy 数据集）；
- 不提供旧字段 alias、旧 DTO、旧路由或旧缓存 key 的 dual-read；
- 不创建 old-to-new 业务数据映射；
- 检测到旧数据集时执行全数据集 hard reset，而不是尝试修复。

硬重置仍必须版本化、幂等和可崩溃恢复，目的是避免新旧代际混用。
