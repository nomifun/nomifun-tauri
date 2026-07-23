# NomiFun ID / 数据存储重构专项

状态：`V3 CONTRACT IMPLEMENTED / RELEASE AUDIT PENDING`
（v3 clean baseline、主要 Rust/Gateway/UI ID hard cut 已落地；Rust
all-targets check、UI typecheck/check/production build 已通过，workspace
全量测试和真实桌面 reset/发布矩阵仍未关闭）

本目录是跨账号、跨平台、跨工作区的唯一接力入口。任何接手本专项的
开发者先阅读本文件和 `00-current-state.md`，再阅读当前工作流的 brief。

## 当前基线

- 仓库：`nomifun-tauri`
- 基线提交：`f880ef20ea3ecd1d0a8b2f0ea39e7ac28fe56b2b`
- 基线时间：2026-07-21
- ID v2 实施提交：`2c0f975a`
- 最近正式版本：`v0.2.30`（`02e642a6`）
- 工作树：基线审查时 clean；之后的实施改动以当前工作树和专项审计结果为准

## Legacy v2 背景与 v3 当前契约

> 本节开头的 v2 规则只用于说明被替换的 legacy 实现；从“目标不是”开始
> 的内容和后续 v3 条目才是当前契约。v3 不读取、迁移或兼容 v2 数据。

Legacy 的 ID-contract-v2 曾经是一次全局硬切，但不是 v3 目标架构：

```text
所有持久实体 = prefix_UUIDv7
所有实体 PK/FK = TEXT
全局禁止 AUTOINCREMENT
所有边界严格拒绝旧数字/短 ID
```

这套规则必须被视为待替换的历史实现，不能继续通过增加例外、增加前缀
或增加校验来修补。

目标不是“把所有字段都改成 UUID”，而是恢复正常的工程分层。对于确实
需要分布式业务 ID 的字段，新值统一使用**无前缀的标准 UUIDv7**：

```text
0190f5fe-7c00-7a00-8000-000000000003
```

固定为 36 个字符、小写、带标准连字符。类型由表名、字段名和
Rust/TypeScript 类型表达，不再把 `conv_`、`msg_`、`prov_` 等前缀塞进
值里。这样既比 `prefix_UUIDv7` 短，也不引入自定义紧凑编码。

整体工程分层为：

1. 每张产品持久表的技术主键：字段固定命名为 `id`，统一使用
   `INTEGER PRIMARY KEY AUTOINCREMENT`；
2. 具名业务 ID：只有业务、API、跨存储、导入导出确实需要时才存在，
   字段按业务命名，例如 `conversation_id`、`provider_id`、`figure_id`、
   `mcp_server_id`、`webhook_id`、`credential_id`、`creation_task_id`；
3. 逻辑关联键：不创建 SQLite `FOREIGN KEY`，稳定实体直接引用具名业务
   ID；纯内部行通过 owner UUIDv7、自然键或复合条件定位，不引用其他表的
   技术 `id`；
4. 自然键：名称、URL、平台组合键、配置 key、排序号等；
5. 外部 ID：第三方签发，完全 opaque；
6. 操作/幂等/运行时 token：短生命周期，不得伪装成实体 ID。

跨协议对象不再输出语义不明的通用 `id`。稳定对象使用
`conversation_id`、`execution_id`、`knowledge_base_id`、`canvas_id`、
`asset_id`、`mcp_server_id`、`webhook_id`、`credential_id`、
`creation_task_id`、`conversation_artifact_id`、`intervention_id` 等具名
裸 UUIDv7 字段。SQLite 的 `id` 只属于本表技术主键，不进入产品 wire。
Canvas 文档内部 node/edge 的 `id` 属于文档结构，不是表技术主键，因此
保持不变。

### UUIDv7 固定结构

- 仅需要分布式身份的具名业务字段使用 UUIDv7；
- 新值固定为 36 字符标准 UUIDv7，不增加业务前缀；
- 字段按业务命名，例如 `conversation_id`、`provider_id`、`figure_id`；
- 每张产品持久表主键固定为 `id INTEGER PRIMARY KEY AUTOINCREMENT`；
- SQLite 业务字段使用 `TEXT NOT NULL UNIQUE` 和固定长度/格式约束；
- 不存在 `conversation_row_id` 这类双轨字段；
- 不创建数据库物理外键、`REFERENCES` 或数据库级级联；
- 稳定实体直接使用 `conversation_id`、`provider_id` 等业务键关联；
- 对外或跨模块定位的实体使用 `webhook_id`、`mcp_server_id` 等具名裸
  UUIDv7 逻辑关联；技术 `id` 不作为业务关联值；
- Cron Job/Run 使用裸 UUIDv7 `cron_job_id` / `cron_job_run_id`，表内 `id`
  仅为技术主键；
- Rust 使用领域 newtype 包装 `Uuid`，TypeScript 使用领域 brand；
- 技术 `id`、自然键、外部 ID 不强制 UUIDv7；
- 新 schema 不读取、不导入、不转换历史前缀 UUIDv7、旧短 ID 或数字 ID。

### v3 数据集策略：一次性全数据集硬重置

新版本采用新的数据契约和新的数据库 lineage，不进行历史 ID 或历史业务
数据迁移：

- 首次启动先识别旧数据集；只要不是 v3 contract，就自动强制 reset，
  不要求用户选择迁移或兼容模式；
- 完整关闭数据库和后台写入，处理 DB/WAL/SHM 文件族；
- 将旧数据集整体移动到版本化 quarantine/rescue 目录，默认不再加载；
- 创建全新的数据库、storage generation 和所有关联 side-store；
- 不在新库中导入旧表、旧 JSON、旧 workspace 索引或旧缓存；
- 不提供双读、双写、alias、old-to-new map 或兼容 parser；
- 用户看到的是干净的新版本初始状态。

这里的 quarantine 只用于让文件切换具备原子性、崩溃恢复和防误删能力，
不是历史数据兼容功能。v3 不读取、不展示、不导入 quarantine 内容。

“清空数据库”不能只删除主 SQLite 文件，否则旧 companion、Workshop、
attachments、knowledge、public-agent、skills、浏览器缓存和 workspace
可能继续携带旧 ID。必须按 `decisions/ID-004-v3-hard-reset.md` 对整个
受管数据集执行同一代际重置。

### 关于统一自增 `id`

所有由 NomiFun 定义的产品持久表，包括实体表、关系表、自然键表、单例表、
缓存表，都必须有：

```sql
id INTEGER PRIMARY KEY AUTOINCREMENT
```

这个 `id` 只是本表的技术主键，不自动成为业务 ID，不用于跨库匹配、公开
URL、文件路径或跨设备协议。SQLite 内部表、migration metadata 和 TEMP
表不属于产品 schema。

### 关于逻辑外键

v3 全面移除物理外键。以 conversation/message 为例：

```text
conversations.id                 本表自增技术主键
conversations.conversation_id    稳定业务 ID
messages.id                      本表自增技术主键
messages.conversation_id         指向 conversation_id 的逻辑关联
```

不再同时保存 `messages.conversation_row_id`。文档中的该名称只用于说明
被废弃的旧方案，v3 schema 不得出现它。逻辑引用由
repository/service 在事务中验证、维护删除策略，并通过索引和 orphan
审计保持完整性。

## 文档索引

| 文件 | 内容 | 状态 |
| --- | --- | --- |
| `00-current-state.md` | legacy v2 快照、历史提交、已知风险、v3 审计上下文 | `LEGACY SNAPSHOT` |
| `01-target-id-architecture.zh.md` | 目标 ID / schema / API 架构 | `ACCEPTED` |
| `02-table-classification.zh.md` | v3 baseline 的 64 张表逐表分类与目标键 | `V3 BASELINE CLASSIFICATION` |
| `03-v3-reset-execution-plan.zh.md` | v3 切换、hard reset、并发分工与回滚 | `ACCEPTED PLAN / IMPLEMENTED` |
| `04-verification-gates.zh.md` | 低耗时验证、编译策略、完成门禁 | `ACCEPTED / WORKSPACE CHECK VERIFIED` |
| `05-handoff-template.md` | 跨账号/平台交接模板 | `TEMPLATE` |
| `06-open-questions.zh.md` | 实施证据与发布前检查表 | `IMPLEMENTATION AUDIT` |
| `decisions/ID-001-current-v2.md` | 对旧 UUIDv7 硬契约的决策结论 | `SUPERSEDED` |
| `decisions/ID-002-layered-id-v3.md` | 新分层 ID 架构决策 | `ACCEPTED` |
| `decisions/ID-003-bare-uuidv7.md` | 无前缀、固定结构 UUIDv7 决策 | `ACCEPTED` |
| `decisions/ID-004-v3-hard-reset.md` | 新版本不兼容历史数据的全数据集硬重置 | `ACCEPTED` |
| `decisions/ID-005-logical-references-autoincrement.md` | 全表自增主键与逻辑外键 | `ACCEPTED` |

## 明确禁止的执行方式

- 不在当前启动路径中加入无版本、无快照的全库 ID 扫描和重写；
- 不把所有 `*_id` 列都当成实体 ID；
- 不把 `id`、`business_id`、`display_no`、外部平台 ID 混为一谈；
- 不创建 `FOREIGN KEY`、`REFERENCES`、数据库级 cascade 或 `*_row_id`；
- 不遗漏任何产品持久表的 `id INTEGER PRIMARY KEY AUTOINCREMENT`；
- 不因为一个字段是字符串就强制 UUID；但已分类为分布式业务 ID 的字段
  必须使用无前缀、固定 36 字符的标准 UUIDv7；
- 不因为支持导入导出，就给所有子表增加分布式 ID；
- 不只删除主数据库而保留旧 side-store；
- 不把旧数据导入新 schema；
- 不在新版本保留历史 ID 兼容分支；
- 在没有完整文件族 quarantine 之前不直接永久删除旧数据；
- 不用 `git revert` 假设可以完成数据回滚；
- 不让多个 worker 同时修改同一个 v3 schema baseline 或同一个公共 ID 模块。

## 接手后的第一步

```bash
git status --short
git rev-parse HEAD
sed -n '1,240p' docs/continuity/00-current-state.md
sed -n '1,260p' docs/continuity/01-target-id-architecture.zh.md
```

核心架构决策已经接受。继续实施或宣称完成前，仍必须以只读 inventory、
逐表 owner 审核、reset 锁/恢复/GC 策略和验证门禁作为证据。
