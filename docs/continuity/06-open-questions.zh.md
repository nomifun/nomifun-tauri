# v3 实施审计与剩余发布门禁

状态：`IMPLEMENTATION COMPLETE FOR CORE CONTRACT / RELEASE GATES OPEN`

本文件不再把已经落地的事项保留为抽象“开放问题”，而是记录截至
2026-07-22 的实施边界、定向证据和仍未通过的发布门禁。这里的完成标记
不等价于 workspace 全量测试通过，也不代表 v3 整体已经完成。

## A. 已实施的数据库与数据集契约

- [x] v3 使用新 lineage，不迁移、回填、导入或兼容读取历史业务数据；
- [x] 64 张产品持久表统一使用
  `id INTEGER PRIMARY KEY AUTOINCREMENT`；
- [x] S 类业务键使用无前缀、固定 36 字符的小写标准 UUIDv7；
- [x] schema 已移除物理 `FOREIGN KEY`、`REFERENCES`、数据库 cascade 和
  `*_row_id` 双轨字段；
- [x] logical-reference registry、关联索引和数据库 orphan audit 已建立；
- [x] hard reset 使用严格 request、durable plan、receipt/finalize，并在
  检测到非 v3 dataset 时强制执行；
- [x] restore 会写入新的 storage generation 和匹配 receipt，避免恢复后
  再次被 hard reset；
- [x] backup format v2 严格绑定 `id-contract-v3`，manifest 必须完整列出
  canonical managed-root registry 中的 include/exclude root 及排除理由。

## B. Managed roots 实施审计

### B.1 浏览器持久根

- [x] hosted agent/browser 通过 `NOMIFUN_DATA_DIR` 使用有效 application
  data root，不再把产品浏览器数据写到另一个平台配置根；
- [x] `browser-state`、`browser-profiles`、`login-profile` 和
  `browser-secrets` 属于可移植 managed dataset；
- [x] `browser-data` 只保存浏览器引擎、缓存、下载和 ephemeral profile，
  factory reset 会管理它，但 portable backup 明确以 runtime/cache 理由排除；
- [x] site memory 位于 `browser-state/site-memory`；
- [x] `work_dir == data_dir` 时，`conversations` 只通过 work namespace
  捕获一次，不在 backup 中重复。

### B.2 外部 work root 与 workspace 边界

- [x] 即使 `work_dir != data_dir`，唯一的外部产品受管根仍是
  `<work_dir>/conversations`；
- [x] hard reset 将该根整体移到
  `<work_dir>/.nomifun-retired-datasets/id-reference-v3-<generation>/conversations`；
- [x] backup 将它放在 `work/conversations` namespace，restore 统一恢复到
  目标数据目录的 `conversations`；
- [x] 任意其他自定义 workspace、用户项目目录和外部路径不被扫描、删除
  或打包；它们只作为用户拥有的外部输入。

### B.3 skills、auto-memory 与用户控制面

- [x] user skills 已收编到 `<app_data>/skills`，与后台 skill service 使用同一
  reset/backup-managed root；
- [x] auto-memory 已收编到
  `<app_data>/projects/<sanitized-project>/memory`，`projects` 已加入 registry
  并作为 portable root；
- [x] platform user-global `commands` root 已停止读取；不会为了兼容旧命令
  根而把它纳入 v3 dataset；
- [x] 项目自有的 `.nomi/commands` 仍可作为显式 workspace 输入，这不表示
  恢复 platform user commands root；
- [x] `AGENTS.md`/`AGENTS.override.md`、平台级 `auth.json` 和
  `config.toml` 属于用户控制面或 workspace instruction，不属于 v3 product
  dataset，不随 factory reset 删除，也不作为 v3 backup root；
- [x] dataset-owned provider secret、browser secret 和 encryption key 仍按
  managed-root policy 处理，不能与上述用户控制面文件混为一谈。

## C. Companion exact-v3 实施审计

- [x] Companion export manifest 的版本必须精确为 `3`；
- [x] manifest、state 和 knowledge references 使用严格 schema，拒绝未知
  字段、缺失版本、`0`、低版本和 future version；
- [x] import 在写入前完整验证，并在 SQLite transaction 中 staging；
- [x] event 文件无覆盖发布；同名冲突必须满足 SHA-256 与内容完全一致；
- [x] same-ID 数据必须完整一致，重复 zip entry、zip-slip、损坏 JSON
  均拒绝；
- [x] 导入失败会回滚数据库和本次已发布文件；
- [x] 不存在旧 Companion export 到 v3 的迁移或宽松兼容路径。

## D. Logical delete policy 实施审计

| 父对象 | 已实施策略 |
| --- | --- |
| Requirement | `requirement_tags.paused_requirement_id` 执行 `SET_NULL`；attachment 行逻辑级联；文件先 rename 到临时删除态，DB 失败时恢复 |
| Knowledge base | preset 引用执行 `RESTRICT`；删除时清理 `knowledge_binding_bases` |
| Webhook | `tag_settings.webhook_id` 执行 `SET_NULL` |
| Provider | 新写入经权威 repository 验证父记录；execution participant 使用 `KEEP_HISTORY`，仍有历史引用时限制删除；清理 provider preference JSON |
| Terminal | 清理 scrollback、knowledge binding 和 junction；`delete_all()` 复用同一逻辑清理语义 |
| MCP server | soft delete 与清理 `conversation_mcp_servers` 在同一事务完成 |

这些策略由 repository/service 承担，不通过 trigger 或重新引入物理 FK
实现。raw SQL 可以绕过应用契约；其用途应限于测试 fixture、诊断和受控
维护，产生的 orphan 必须由 audit 检出，而不是期待 SQLite 模拟物理 FK。

## E. 已通过的定向证据

当前记录包括 factory reset、backup、schema、MCP repository、
Requirement attachments、provider invariants、webhook、Companion /
Public Agent、Channel、Requirement/AutoWork、Agent Execution、
Provider deletion 定向测试，以及 workspace all-targets check、UI
`check` 和 production build。完整命令和范围见
`04-verification-gates.zh.md`。

主要 DTO/wire hard cut 也已完成：

- Conversation、Execution、Knowledge Base、Workshop Canvas/Asset 等稳定
  对象使用具名 UUIDv7 字段，不再依赖通用 `id` alias；
- Creation Task 使用具名裸 UUIDv7 `creation_task_id`；数据库技术主键仍
  固定为 `creation_tasks.id`，且不进入 API/Gateway/UI；
- Canvas 文档内部 node/edge `id` 保持文档内身份，不与数据库技术主键
  混淆。
- `message_correlations.turn_message_id` 已明确为 wire-scoped UUIDv7
  protocol owner token，不再伪装成 `messages.message_id` 父引用；
  reserve-before-project、continuation call-ID reuse 和普通消息删除均有
  回归测试。

## F. 仍未关闭的发布门禁

- [x] `cargo check --workspace --all-targets` 已通过；
- [ ] 未记录 workspace 全量 `cargo test` 通过；
- [x] UI typecheck 已通过；本轮补充的 preset/provider/MCP UI key
  定向测试 11 个全部通过；
- [x] `bun run check` 和最终 UI production build 已通过；build 仅报告
  既有 dynamic-import/chunk-size 告警；
- [ ] 完整 UI ID 测试矩阵仍未全部执行；
- [x] 最终禁止项静态审计已完成；允许命中仅为 schema 拒绝测试、
  logical-reference registry、局部本地 row 变量、普通英文误命中，以及
  明确标注的 Legacy/禁止项文档；
- [ ] 仍需完成真实操作系统/文件系统上的 reset 中断、锁、权限、磁盘不足
  和产品交互矩阵；
- [ ] retired dataset 的保留天数、磁盘上限和显式 GC 入口仍需发布决策；
- [ ] 发布说明必须明确升级会重置本地 product dataset，同时不会删除
  `AGENTS.md`、`auth.json`、`config.toml` 或任意外部用户 workspace；
- [ ] 只有上述发布门禁和最终集成审计关闭后，才能宣称 v3 整体完成。

`ID-002`、`ID-003`、`ID-004`、`ID-005` 仍是已接受的产品架构决策；本文件
只说明当前实施进度，不降低这些决策的约束。
