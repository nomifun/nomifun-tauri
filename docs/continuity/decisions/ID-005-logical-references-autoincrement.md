# ID-005：全表自增主键与逻辑外键

状态：`ACCEPTED`

提出时间：2026-07-21

前置决策：`ID-002-layered-id-v3.md`、`ID-003-bare-uuidv7.md`

## 决策

所有由 NomiFun 定义的产品持久表统一使用：

```sql
id INTEGER PRIMARY KEY AUTOINCREMENT
```

该 `id` 只表示当前数据库中本表的技术行身份：

- 是表主键和索引定位入口；
- 删除后不复用；
- 不自动成为业务 ID；
- 不用于跨数据库、跨设备、文件路径或公开协议匹配；
- 关系表、自然键表、单例表和缓存表也不能遗漏。

SQLite 内部表、migration metadata、TEMP 表不属于产品持久表。

## 全面移除物理外键

v3 产品 schema 不得出现：

```text
FOREIGN KEY
REFERENCES
ON DELETE CASCADE
ON UPDATE CASCADE
*_row_id
```

不再同时保存 `conversation_id` 与 `conversation_row_id`。此前双轨设计希望
把数据库行定位和稳定业务身份分开，但它导致 DTO、repository、查询、
clone 和调试都需要持续转换两套值。v3 选择单一逻辑关联字段。

## 逻辑关联字段

### 稳定实体

父实体具有 UUIDv7 业务键时，子表直接保存同名业务键：

```sql
CREATE TABLE conversations (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    conversation_id TEXT NOT NULL UNIQUE
);

CREATE TABLE messages (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    message_id      TEXT NOT NULL UNIQUE,
    conversation_id TEXT NOT NULL
);

CREATE INDEX idx_messages_conversation_id
    ON messages(conversation_id);
```

`messages.conversation_id` 是指向
`conversations.conversation_id` 的逻辑外键，但数据库不声明
`FOREIGN KEY`。

### 内部技术行

技术 `id` 不作为其他表的逻辑关联目标。需要被其他表、模块、API、事件、
受管文件或 side-store 定位的实体，必须拥有具名业务 UUIDv7；纯内部关系、
依附和事件行则通过 owner 业务 ID + sequence、自然键或复合唯一条件定位：

```sql
CREATE TABLE agent_execution_events (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    execution_id TEXT NOT NULL,
    sequence     INTEGER NOT NULL,
    payload      TEXT NOT NULL,
    UNIQUE (execution_id, sequence)
);

CREATE INDEX idx_agent_execution_events_execution_id
    ON agent_execution_events(execution_id);
```

当前 v3 baseline 不包含 `<parent>_id INTEGER -> parent.id` 形式的跨表关系。
`step_id`、`webhook_id`、`mcp_server_id`、`creation_task_id` 等产品 locator
都是具名裸 UUIDv7；不使用 `step_row_id`、`webhook_row_id` 或任何双轨字段。

## 应用层完整性契约

取消物理外键不等于取消引用完整性。每条逻辑关系必须登记：

- 子表和引用字段；
- 父表和目标字段；
- 字段类型；
- 必需/可空；
- 删除策略：`RESTRICT`、`CASCADE`、`SET_NULL`、`KEEP_HISTORY`；
- clone/restore 重建策略；
- orphan 审计 SQL；
- 必需索引。

写入和删除必须经过 repository/service：

1. 在同一写事务中验证父记录存在；
2. 插入或更新子记录；
3. 删除父记录前执行登记的逻辑删除策略；
4. bulk import/restore 完成后运行全量 orphan audit；
5. 启动和诊断工具可运行只读引用闭包检查；
6. 禁止业务代码绕过 repository 直接写关联列。

repository/service 是逻辑引用的**权威写路径**。v3 不使用 SQLite trigger
重新模拟物理 FK，也不要求任意 raw SQL 在写入不存在的父对象时自动失败。
因此：

- repository 测试必须证明不存在的父对象会在权威写路径被拒绝；
- 删除测试必须证明 `RESTRICT`、`CASCADE`、`SET_NULL` 或
  `KEEP_HISTORY` 在同一事务生效；
- raw SQL 只用于受控 fixture、诊断和维护；
- raw SQL 产生的悬空引用必须能被 orphan audit 检出；
- 不得为了让旧的 raw-SQL 断言通过而新增 trigger、隐藏 cascade 或物理
  关系。

## 单例与关系表

- 单例表仍使用自增 `id`，另设 `singleton_key TEXT UNIQUE NOT NULL`；
- 不依赖 `id = 1`；
- 关系表仍使用自增 `id`；
- 关系唯一性由额外 `UNIQUE (...)` 保证；
- `UNIQUE` 是业务去重约束，不是物理外键。

## 完成条件

- [x] 64 张产品持久表全部有 `id INTEGER PRIMARY KEY AUTOINCREMENT`；
- [x] v3 schema 中不存在 `FOREIGN KEY`、`REFERENCES` 或 `*_row_id`；
- [x] 当前 baseline logical-reference registry 中的引用字段具有必需索引；
- [ ] 每个删除策略都有定向测试；
- [ ] orphan audit 覆盖数据库、JSON 和 side-store；
- [x] API 不以通用 `id` 暴露技术主键；Cron、MCP、Webhook、Creation Task
  等 locator 分别使用裸 UUIDv7 `cron_job_id`、`mcp_server_id`、
  `webhook_id`、`creation_task_id`；
- [ ] restore/clone 不把一个数据集的自增 `id` 当作另一个数据集的稳定身份。

已通过的 schema、provider、webhook、MCP、Requirement attachment 等定向
证据见 `../04-verification-gates.zh.md`；这里不据此宣称全部逻辑关系或
workspace 全量测试已经完成。
