# 数据与标识符规范

本文是仓库级数据库 schema、ID、逻辑关联、数据集重置、备份恢复以及协议边界
规范。新增代码和修改已有代码都必须遵守。

权威顺序如下：

1. [`001_v3_baseline.sql`](../../crates/backend/nomifun-db/migrations/001_v3_baseline.sql)
   与
   [`id_schema_contract.rs`](../../crates/backend/nomifun-db/src/id_schema_contract.rs)
   —— 可执行 schema 和运行时 registry；
2. [`architecture/id-system.zh.md`](../architecture/id-system.zh.md) 与
   [`architecture/data-and-storage.zh.md`](../architecture/data-and-storage.zh.md)
   —— 架构契约与存储行为；
3. 本文 —— 贡献者执行流程和 review 清单；
4. `docs/continuity/` —— 历史背景、决策、交接与审计证据，不能覆盖当前
   架构契约。

如果实现和文档不一致，不得自行增加兼容例外。应同步修正实现与权威文档，
或者暂停并请求架构决策。

## 1. 产品表固定结构

每张由 NomiFun 定义的产品持久表都必须从同一个技术主键开始：

```sql
id INTEGER PRIMARY KEY AUTOINCREMENT
```

这适用于实体表、关系表、值对象表、单例表、缓存表、事件/outbox 表和依附表。
SQLite 内部表、migration metadata 和临时表不属于产品表。

技术 `id` 只属于当前数据集中的本表行。它是：

- 内部存储键和主索引入口；
- repository/storage 代码内部的 `i64`；
- 允许有间隔，绝不能假设连续；
- restore、clone 或 hard reset 时重新分配；
- 禁止作为产品 API、事件、文件、manifest、backup graph 或跨数据集 locator。

不要把该列改名为 `row_id`、`table_id` 或其他变体，也不要把某张表的技术
`id` 复制到另一张表。

当前 v3 是干净的新 baseline，不承诺保留所有历史 migration 路径。任何 schema
变更都必须先判断应当更新 v3 baseline、建立新 lineage，还是采用经过明确批准的
发布迁移。不得为了省事擅自增加兼容 migration、legacy mapper 或
dual-read/dual-write。

## 2. 稳定业务 ID

只有实体需要脱离本地数据库行，在数据库、设备、受管文件、API、事件、backup
graph 或 side-store 之间稳定定位时，才增加业务 ID。

使用一个按业务命名的字段，例如：

```text
user_id
conversation_id
message_id
provider_id
execution_id
knowledge_base_id
webhook_id
credential_id
```

值必须是裸的规范 UUIDv7：

```text
0190f5fe-7c00-7a00-8000-000000000003
```

固定契约如下：

- 标准 `8-4-4-4-12` 结构，共 36 个字符；
- 小写十六进制和标准连字符；
- UUID version 7 与 RFC UUID variant；
- 保留完整 128 bit，不截断；
- SQLite 使用 `TEXT`，JSON 使用字符串；
- 禁止 `prefix_UUIDv7`、后缀、花括号、紧凑形式、空白和自定义分隔符。

实体类型由字段名、表语义和 Rust/TypeScript 领域类型表达，UUID 文本本身不
编码类型前缀。已有具名业务字段时，不要再引入语义不清的通用 wire `id`。

不要为了形式统一给每一行都生成 UUID。只在内部使用的关系、单例、缓存和事件
行，除非确实需要产品或跨 store locator，否则只保留强制的技术 `id`。

## 3. 区分 ID 类别

字段名以 `_id` 结尾不代表它一定是业务 ID。新增或校验字段前必须分类：

| 类别 | 规则 |
| --- | --- |
| 技术行 ID | 固定的本地 `id`；不能越过 repository 或产品边界。 |
| 稳定业务 ID | 产品可定位实体使用的具名裸 UUIDv7。 |
| 自然键 | 名称、slug、URL、locale、singleton key 等领域值；保持原格式。 |
| 外部 ID | Provider 或协议签发的不透明值，如平台 user/chat ID、ACP session ID、remote task ID。 |
| 操作 token | request ID、幂等键、nonce、workspace token、receipt token；有明确用途，不是实体身份。 |
| 文档身份 | Canvas node/edge 等文档内部身份；不是数据库主键。 |

协议专用的 UUIDv7 也必须明确分类。例如
`message_correlations.turn_message_id` 是在投影前使用的、wire scope 的协议
owner token，不是指向 `messages.message_id` 的父引用。相反，像
`messages.msg_id` 这样的字段，如果连接另一条消息，就必须服从逻辑关联
registry。不能只根据字段后缀推断语义。

## 4. 用逻辑关联替代物理外键

产品 DDL 禁止出现：

```text
FOREIGN KEY
REFERENCES
CREATE TRIGGER
ON DELETE CASCADE
ON UPDATE CASCADE
*_row_id
```

一条关系只保存一个逻辑引用：

- 父实体可被产品定位时，引用父实体的具名业务 ID；
- 仅内部的行通过 owner 业务 ID + sequence、自然键或复合条件定位；
- 外部系统拥有该值时，保留用途明确的不透明外部 ID；
- 禁止同时保存 `conversation_id` 和 `conversation_row_id`，也禁止任何等价
  的业务 ID/行 ID 双轨字段。

逻辑关联不是随意约定。每条关联都必须在 `id_schema_contract.rs` 登记，包含
JSON/side-store 关联在内，至少登记：

- 子表/列、父表/目标列；
- kind、值契约、可空性和 scope/predicate；
- 必需索引；
- 删除策略：`RESTRICT`、应用层 `CASCADE`、`SET_NULL` 或
  `KEEP_HISTORY`；
- restore/clone 重建策略；
- orphan audit 策略/查询。

Repository/Service 是正常写入的唯一边界。它们必须校验父项、执行聚合所有权
检查，并在显式事务中执行登记的删除策略。这里的应用层 `CASCADE` 是
Service/Repository 行为，不是 SQLite cascade 或 trigger。裸 SQL 只允许用于
受控 fixture、诊断和维护。

restore/import 后必须对数据库和受管 side-store 执行完整 orphan audit。没有
物理 FK 不代表可以接受未经检查的孤儿行。

## 5. 数据集 lineage 与 hard reset

v3 与历史产品数据集有意不兼容。启动时必须在打开产品数据库之前识别数据集
lineage/generation。不存在数据集时初始化 v3；历史或不兼容的受管数据集整体
退役/quarantine，并创建全新的空 v3 数据集。

禁止以下兼容行为：

- 逐表转换或历史业务数据迁移；
- legacy ID 规范化或 old-to-new map；
- 兼容读取、alias、dual-read 或 dual-write；
- 选择性复制旧 JSON、缓存、workspace 索引或 side-store 行；
- reset 不完整时继续启动。

重置范围包括 SQLite 数据库及其 WAL/SHM sidecar，以及全部受管 side-store。
必须在对外服务前写入并完成 generation/reset receipt。reset 失败必须
fail-closed。用户自有的外部 workspace 不删除，但其历史数据库引用不导入 v3。

## 6. Backup、restore 与 clone

备份和恢复把 v3 受管数据集作为一个整体处理：

- 只接受 v3 manifest 和 lineage；
- 保留稳定业务 UUIDv7；
- 在目标数据集中重新分配技术 `id`；
- 从 registry 登记的业务 ID、自然键、外部 ID、JSON 和 side-store 引用重建
  逻辑关联；
- 拒绝业务 ID 冲突和部分安装；
- 安装后运行完整 orphan audit。

源数据集技术行 ID 不是可移植 graph identity。Clone 保留输入中的业务 ID，不
静默 mint 或改写。

## 7. 边界与类型规则

`nomifun-common` 负责裸 UUIDv7 的生成和严格校验。稳定业务 ID 在适合的边界
使用小型 Rust domain newtype 与 TypeScript branded type。

在 HTTP、WebSocket、MCP、Gateway、事件、缓存、文件系统和 backup 边界：

- 使用领域模型中的具名业务或外部字段类型；
- 技术 `id` 只留在 repository/storage 实现内部；
- 非法值直接拒绝，不得转成 `0`、空字符串或另一种 ID；
- 不要把通用数字 `id` 暴露为可移植产品 locator。

## 8. 必须的测试与验证

数据库或 ID 改动必须为受影响的契约增加或更新聚焦测试，至少覆盖适用项：

- 每张产品表都有 `id INTEGER PRIMARY KEY AUTOINCREMENT`；
- 产品 DDL 没有物理 FK、`REFERENCES`、trigger、数据库级联或 `*_row_id`；
- 接受规范 UUIDv7，拒绝旧格式、带前缀格式和短 ID；
- logical-reference registry、索引、scope 检查和删除策略；
- repository 父项校验与事务行为；
- orphan audit，包括 JSON/side-store 关联；
- dataset lineage 检测、hard reset、generation 和 reset receipt；
- backup/restore/clone 的业务 ID 保留与技术 ID 重建。

删除真实过时或误导性的测试，不要靠伪覆盖维持表面通过。优先运行最小定向
测试，昂贵的 workspace 全量门禁留到最终集成阶段。

纯文档改动至少运行：

```bash
git diff --check
```

## 9. 新表或新 ID 清单

提交 review 前：

1. 阅读本规范和架构 ID 契约。
2. 每张新产品表加入固定 `id` 列。
3. 判断是否确实需要具名稳定 UUIDv7。
4. 将每个 `_id` 字段分类为业务、自然、外部、token、文档身份或逻辑关联。
5. 删除技术行 ID 传播、物理 FK 和 trigger 设计。
6. 登记每条逻辑关系、索引、生命周期策略、重建策略和 orphan audit。
7. 技术 ID 不得进入任何 wire、事件、文件和 backup 契约。
8. 增加 schema、repository、边界、reset 或 restore 聚焦测试。
9. 审查完整受管数据集的重置影响。
10. 保持中英文文档同步。

## 10. 仓库卫生

不得提交本地数据、secret、生成的构建结果、依赖目录或中间产物。特别是以下
内容必须留在 Git 之外：

```text
.tmp*/
.tmp***
target/
build.noindex/
node_modules/
dist/
coverage/
*.o
*.rmeta
*.rlib
```

如果工具生成了非标准临时目录或依赖产物，应先加入本地 ignore 策略再继续。
不要为了“清理”其他开发者生成的文件而把它们加入提交。
