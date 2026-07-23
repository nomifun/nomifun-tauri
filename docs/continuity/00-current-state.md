# Legacy v2 Evidence / v3 Audit Context

状态：`LEGACY SNAPSHOT / V3 AUDIT CONTEXT`

> 本文件第 1—5 节记录的是 v2 实施快照和当时发现的问题，所有其中的
> `TEXT` 主键、前缀 UUIDv7、禁止 `AUTOINCREMENT`、物理 FK 和旧 migration
> 描述均为 **legacy**，不是 v3 当前规范，也不是 v3 兼容要求。v3 当前规范以
> `docs/architecture/id-system*.md`、`docs/architecture/data-and-storage*.md`
> 以及 `ID-002`—`ID-005` 为准。

## 1. 事实基线

截至 2026-07-21，当前提交为：

```text
f880ef20ea3ecd1d0a8b2f0ea39e7ac28fe56b2b
```

ID v2 的核心实施提交为 `2c0f975a`：

- 838 个文件发生变化；
- 约 3.3 万行新增、约 3.1 万行删除；
- 原有多条数据库 migration lineage 被压成
  `001_id_contract_v2.sql`；
- 主数据库的实体 PK/FK 统一为 `TEXT`；
- `nomifun-common/src/id.rs` 建立了强制前缀 UUIDv7 newtype；
- UI、Gateway、serde、备份恢复均加入了严格格式检查。

这不是一次局部修 bug，而是数据库 lineage、协议和前端边界同时变化。
因此“回退代码”不能等价于“回退数据”。

## 2. 当前设计的主要证据

| 位置 | 事实 |
| --- | --- |
| `crates/backend/nomifun-db/migrations/001_id_contract_v2.sql:1-17` | 全库实体使用 `prefix_UUIDv7`，PK/FK 使用 TEXT |
| `crates/backend/nomifun-db/src/id_schema_contract.rs:17-180` | 注册所有实体 PK/FK，禁止 AUTOINCREMENT，仅允许 `system_settings.id` 为整数 |
| `crates/backend/nomifun-db/src/id_schema_contract.rs:184-327` | 备份/恢复扫描并验证值是否为前缀 UUIDv7 |
| `crates/backend/nomifun-common/src/id.rs:103-207` | 生成和解析强制前缀 UUIDv7 |
| `ui/src/common/types/ids.ts:173-205` | UI 拒绝数字、旧短 ID、错误前缀和非 UUIDv7 |
| `crates/backend/nomifun-db/src/database.rs:199-216` | 旧 lineage 会被 quarantine 后重建空库 |
| `crates/backend/nomifun-db/src/backup_bundle.rs` | backup/restore/clone 绑定 ID-contract-v2 |

## 3. 已确认的业务问题

### 3.1 行主键和业务主键没有分层

当前绝大多数表只有一个字符串 `id`，它同时承担：

- SQLite 索引目标；
- 外键目标；
- API locator；
- 导入导出匹配键；
- 文件路径/文件名的一部分；
- UI 展示和排序 tie-break；
- 运行时关联；
- 有时还承担外部协议或客户端 correlation。

这使一个“格式变更”变成全系统重构，也使简单本地表承担了不必要的
分布式唯一性成本。

### 3.2 外部 ID 被误判为实体 ID

已确认的高风险字段包括：

- `messages.msg_id`：客户端/协议消息关联，不是本地 message 实体 ID；
- ACP `session_id`；
- 平台 user/chat/bot ID；
- `remote_task_id`；
- provider/tool/call correlation；
- `channel_users.session_id`；
- IDMM 的多态 `target_id`；
- preset/client preference JSON 中的引用和自然 key。

这些字段只能按各自协议校验，不能统一套前缀 UUIDv7。

### 3.3 UI 和协议历史兼容已经被严格解析器阻断

当前 `ui/src/common/types/ids.ts`、session route、Gateway schema、
serde boundary 会拒绝旧数字 ID、数字字符串和旧短 ID。一个历史值可能
导致整条列表、WS 事件或路由失败。

最新目标决策不再为这些历史值建立兼容 parser。v3 必须在 UI、Gateway
和业务 mapper 启动前完成 dataset gate；旧值随整个旧数据集退出 active
状态，不进入新协议。

### 3.4 Legacy v2 的旧库处理是硬切，不是迁移

旧数据库可能被改名为：

```text
*.pre-id-v2.bak*
```

然后创建空库。它保留了原始文件，但没有 row-by-row 数据迁移。v3 已
明确接受不保留历史业务数据，因此问题不再是“缺少逐行迁移”，而是当前
实现尚未证明 DB 文件族和所有 side-store 都被同一代际完整隔离。

### 3.5 Side-store 不是完整备份闭包

现有 backup 不能证明覆盖所有权威数据。审查发现风险包括：

- attachments；
- Workshop 文档、资产和缩略图；
- managed knowledge 文件；
- public-agent；
- companion 的完整数据库/JSON/JSONL/skills；
- preset/user avatar；
- browser/preview/外部 workspace 引用。

主库恢复成功不等于业务数据恢复成功。

### 3.6 Legacy migrations 曾出现物理 FK 失效造成的补救

`003_remove_local_model_support.sql` 在 FK 未生效的情况下删除 provider，
之后需要 `004_repair_disabled_fk_cascades.sql` 修复部分悬空引用。这也是
v3 决定移除物理 FK 的背景之一：引用完整性不能依赖容易被关闭、绕过或在
表重建中失效的 pragma，而应由固定 logical-reference registry、事务化
repository、删除策略和 orphan audit 显式维护。

## 4. 现状中可以保留的部分

以下不是问题本身，不应为了“反 UUIDv7”而全部删除：

- 跨数据库/跨文件/公开 URL 所需的稳定业务键；
- 消息、会话、用户、终端、Workshop 文件的稳定引用；
- restore/merge 的冲突检测；
- clone 的 old-to-new 显式映射；
- 复合自然键和显式引用闭包检查；
- JSON 中实体 ID 始终使用字符串的原则（对业务字符串成立）；
- 操作 token 与实体 ID 分离的原则。

需要删除的是“所有实体都必须同一种格式”和“所有表都必须使用分布式
字符串主键”，不是删除稳定身份本身。

## 5. 不能据此得出的结论

- 不能因为旧库是数字 ID，就把所有数字 ID 迁成 UUID；
- 不能因为某字段名含 `_id`，就证明它是实体 ID；
- 不能因为某表会被 backup，就证明其每一行需要全局业务 ID；
- 不能因为一个 ID 需要 JSON 传输，就证明数据库 PK 也必须是 TEXT；
- 不能因为表有自增 `id`，就把它自动当成跨数据集业务 ID；
- 不能认为“重置数据库”已经完成了数据迁移。

## 6. 已接受的目标方向

后续 `ID-002`、`ID-003`、`ID-004`、`ID-005` 已确定：

- 每张产品持久表固定为
  `id INTEGER PRIMARY KEY AUTOINCREMENT`；
- 只有需要分布式业务身份的具名字段使用无前缀标准 UUIDv7；
- 全面移除 SQLite 物理外键和 `*_row_id` 双轨关联，使用逻辑外键；
- v3 不迁移历史业务数据；
- 首次升级重置完整 managed dataset，而不是只删除主 SQLite。

本文件其余“兼容/迁移”措辞只描述 legacy 实现和当时风险，不代表 v3
仍要建设历史兼容层。不要把本文件的旧代码路径当作当前 schema 证据。

## 7. 现状结论

旧 v2 是需要被替换的实现基线，目标 v3 应以字段语义和生命周期为
单位重构，而不是在 v2 上继续追加例外。
