# 验证门禁与低耗时执行策略

状态：`ACCEPTED VERIFICATION CONTRACT`

目标是在不牺牲质量的前提下，减少 Rust workspace 的重复编译和全量
测试。验证按“静态 -> SQLite -> 定向 crate -> 跨边界 -> 全量”逐层升级。

## 1. Gate 0：禁止性静态检查（不编译）

检查目标是禁止旧的全局 ID 设计和物理外键设计回流：

```bash
rg -n "validate_prefixed_id|generate_prefixed_id|ENTITY_PRIMARY_KEYS|FOREIGN KEY|REFERENCES|_row_id" \
  crates ui apps docs/architecture
```

文档审计另外执行：

```bash
rg -n -i \
  "prefix[_ -]?uuidv7|generate_prefixed_id|禁止 AUTOINCREMENT|FOREIGN KEY|REFERENCES|_row_id|历史迁移|legacy migration" \
  docs/architecture docs/continuity
```

该命令允许在“禁止项示例”和明确标注为 `Legacy`/`legacy` 的历史章节中
命中；任何未标注为历史、且把旧规则写成当前 v3 要求的命中都必须修复。

结果必须逐项归类：

- 必须删除的旧代际读取代码；
- 特殊协议代码；
- 尚未切换的业务路径；
- 仅文档历史；
- 误用。

新增代码不得出现：

- 未说明字段语义的通用 `id` parser；
- 为表达业务类型而重新增加 UUID 前缀；
- 把所有 `*_id` 注册为同一 ID registry；
- 用字符串 ID 实现排序；
- 把外部 ID 作为内部逻辑关联；
- 缺少 logical-reference registry 的关联字段。

## 2. Gate 1：SQLite schema smoke（不编译）

用 `sqlite3`/Python 在临时数据库中：

- 应用全新的 v3 baseline；
- 验证表、列、PK、INDEX、UNIQUE；
- 验证 64 张产品持久表全部使用
  `id INTEGER PRIMARY KEY AUTOINCREMENT`；
- 验证每个 S 表业务键唯一；
- 验证每张产品持久表自身 `id` 自动分配且删除后不复用；
- 验证 N/D 表使用自然/owner 唯一约束，且不产生随机 UUID；
- 验证 schema SQL 不含 `FOREIGN KEY`、`REFERENCES` 或 `*_row_id`；
- 验证每个逻辑关联字段都有索引；
- 运行 logical-reference registry 的 orphan audit；
- `PRAGMA quick_check`。

注意：应用层逻辑外键也必须在独立 SQLite harness 中可验证，不能只靠某个
Rust 启动路径碰巧执行清理。

## 3. Gate 2：v3 字段契约测试

v3 parser 不兼容历史业务 ID。至少覆盖：

| 输入 | 预期 |
| --- | --- |
| 当前 `prefix_UUIDv7` | v3 业务字段拒绝 |
| 新业务 UUIDv7 | 无前缀、36 字符、小写、标准连字符、version 7 |
| 旧短 ID | v3 业务字段拒绝 |
| UUIDv4/ULID/雪花字符串 | v3 UUIDv7 业务字段拒绝 |
| 安全数字 | 仅 repository 内部技术 `id` 场景接受，不得成为产品 locator |
| 数字字符串 | UUIDv7 业务字段与产品 locator 一律拒绝 |
| 空串/首尾空白/控制字符 | 拒绝 |
| 小数、超安全范围数字、对象/数组 | 拒绝 |
| 外部平台 ID | 仅外部 parser 处理 |

旧数据不应进入 v3 mapper；dataset detector 必须在应用读取业务数据前
完成 hard reset。

## 4. Gate 3：定向 Rust 检查

每个 worker 完成后只运行与写入范围对应的命令，优先共享 target/build
缓存：

```bash
cargo fmt --check
cargo check -p nomifun-common -p nomifun-db
cargo test -p nomifun-common
cargo test -p nomifun-db --test id_schema_contract
cargo test -p nomifun-db --test db_lifecycle
```

`id_schema_contract` 测试本身需要随 v3 重写：检查每张产品表的
`id INTEGER PRIMARY KEY AUTOINCREMENT`、逻辑引用 registry、关联索引和
禁止物理 FK，而不是检查旧的字符串实体 PK/FK registry。

API/业务 worker 只在公共 contract 稳定后运行：

```bash
cargo check -p nomifun-api-types -p nomifun-gateway
cargo test -p <changed-crate> --lib
```

不要每个 worker 都运行 workspace `cargo test`。公共 crate 变更后才
重新编译依赖它的 wave。

### 截至 2026-07-22 的已通过证据

以下是当前 v3 实施工作树已经取得的证据。除明确标记的 workspace
all-targets check 外，其余 Rust 测试仍是定向范围，不等价于 workspace
全量 `cargo test`：

| 命令 | 结果 | 主要覆盖 |
| --- | ---: | --- |
| `cargo test -p nomifun-common factory_reset --lib` | 14 passed | hard reset plan、quarantine、崩溃恢复、外部 work root |
| `cargo test -p nomifun-db --test backup_bundle` | 23 passed | format v2、managed-root manifest coverage、全 root restore |
| `cargo test -p nomifun-db --test db_lifecycle` | 13 passed | v3 lineage、checksum、receipt、reset/reopen 生命周期 |
| `cargo test -p nomifun-db --test id_schema_contract` | 22 passed | 64 表自增主键、无物理 FK、UUIDv7 与 logical-reference contract |
| `cargo test -p nomifun-db sqlite_creation_task --lib` | 6 passed | Creation Task 自增 `id` + `creation_task_id`、逻辑引用结构校验、状态 CAS 与并发取消 |
| `cargo test -p nomifun-creation --lib` | 76 passed | Creation Task `creation_task_id` wire、生成状态机、artifact 完整性与 HTTP adapter |
| `cargo test -p nomifun-db --test channel_repository` | 7 passed | Channel repository 裸 UUIDv7 与逻辑关联 |
| `cargo test -p nomifun-channel --lib` | 315 passed | Channel 领域、会话、消息与 relay |
| `cargo test -p nomifun-db --test mcp_server_repository` | 33 passed | MCP repository 与 conversation junction 逻辑删除 |
| `cargo test -p nomifun-companion export::tests --lib` | 10 passed | exact-v3 export/import、staging、冲突与回滚 |
| `cargo test -p nomifun-requirement attachments::tests --lib` | 6 passed | attachment ingest、失败清理、删除与 workspace staging |
| `cargo test -p nomifun-db --test provider_binding_invariants` | 2 passed | provider 权威写路径、删除原子性、JSON 引用清理 |
| `cargo test -p nomifun-db --test webhook_repo` | 4 passed | webhook CRUD 与 `tag_settings.webhook_id` 的 `SET_NULL` |
| `cargo test -p nomifun-companion -p nomifun-public-agent --lib --no-fail-fast` | 198 passed | Companion exact-v3 side-store、Provider 生命周期、逻辑引用和 Public Agent |
| `cargo test -p nomifun-channel --test session_action_integration --test stream_relay_test --test message_loop_test --test message_service_integration --test manager_integration --test pairing_integration` | 100 passed | Channel Plugin/User/Session 裸 UUIDv7、配对、消息循环、relay |
| `cargo test -p nomifun-app --test channel_e2e` | 23 passed | Channel HTTP 业务 ID、配对、用户撤销、启停 |
| `cargo test -p nomifun-app --test agent_execution_e2e` | 3 passed | Execution/Template 稳定业务 ID 与权威自然键 |
| `cargo test -p nomifun-app --test requirements_e2e` | 10 passed | Requirement API 使用 `requirement_id`，技术 `id` 不暴露 |
| `cargo test -p nomifun-app --test autowork_idmm_cooperation_e2e` | 2 passed | AutoWork/IDMM Requirement UUIDv7 协作 |
| `cargo test -p nomifun-app provider_deletion::tests --lib` | 8 passed | Provider 生命周期、Execution/Template 逻辑引用、soft cleanup |
| `cargo test -p nomifun-webhook --test notifier` | 4 passed | Requirement 稳定业务 ID 的完成通知 |
| `bun run --filter=./ui typecheck` | passed | UI v3 ID/DTO/type boundary |
| `bun test ui/src/common/types/agent/presetTypes.test.ts ui/src/common/types/provider/providerApi.test.ts ui/src/renderer/hooks/mcp/extensionCatalog.test.ts` | 11 passed | preset/provider wire contract、MCP UUIDv7 与 extension UI key 隔离 |
| `npx --yes bun@1.3.14 run check` | passed | UI typecheck、i18n、theme、icons、runtime boundary、agent vocabulary、help contract |
| `npx --yes bun@1.3.14 run build:ui` | passed | Vite production build；仅有既有 chunk/dynamic-import 告警 |
| `cargo check --workspace --all-targets` | passed | Rust workspace 全 target 编译闭包 |
| `cargo fmt --all -- --check` | passed | Rust 格式门禁 |
| `git diff --check` | passed | whitespace/error marker 门禁 |

2026-07-22 的最后一轮 DTO/wire 收口还通过：

```bash
cargo check \
  -p nomifun-api-types \
  -p nomifun-conversation \
  -p nomifun-cron \
  -p nomifun-companion \
  -p nomifun-agent-execution \
  -p nomifun-knowledge \
  -p nomifun-workshop \
  -p nomifun-terminal \
  -p nomifun-channel \
  -p nomifun-gateway \
  -p nomifun-app \
  --all-targets

cargo check -p nomifun-creation -p nomifun-gateway --all-targets
npx --yes bun@1.3.14 run --filter=./ui typecheck
cargo fmt --all -- --check
git diff --check
```

本轮还将 Creation Task 收口为具名 UUIDv7 wire
`creation_task_id`。数据库同时保留统一技术列 `creation_tasks.id`，但 API、
Gateway 和 UI 不暴露该技术值，也不提供旧 `task_id` 或通用 `id` alias。

与上述实施波相关的编译检查也已通过：

```bash
cargo check -p nomifun-common --all-targets
cargo check -p nomifun-db --all-targets
cargo check -p nomifun-requirement -p nomifun-terminal --all-targets
cargo check -p nomifun-companion --all-targets
cargo check -p nomi-config -p nomi-browser -p nomifun-gateway \
  -p nomifun-app -p nomifun-common --all-targets
cargo check --workspace --all-targets
```

### 2026-07-23 correlation 与最终静态审计

续接流式消息复用 provider call ID 的真实路径暴露出一处旧语义：
`message_correlations.turn_message_id` 曾被 registry 误写为
`messages.message_id` 的父引用。当前已改为具名、固定结构的 wire-scoped
UUIDv7 protocol owner token；它不要求存在同值 message 行，也不会阻止
删除普通消息。`message_correlations.message_id` 若已投影为 message，则
仍必须位于同一 Conversation。

本轮通过：

```text
cargo test -p nomifun-db --test id_schema_contract
22 passed

cargo test -p nomifun-db --test conversation_repository
35 passed

cargo test -p nomifun-db --lib id_schema_contract::tests:: --no-fail-fast
7 passed

cargo test -p nomifun-conversation --lib stream_relay::tests:: --no-fail-fast
76 passed

cargo test -p nomifun-conversation --test acp_artifact_turn_history
3 passed

cargo check --workspace --all-targets
passed

cargo fmt --all -- --check
git diff --check
node ui/node_modules/typescript/bin/tsc --noEmit -p ui/tsconfig.json --pretty false
passed
```

独立 Python/SQLite smoke 也确认 baseline 恰有 64 张产品表、64 个
`id INTEGER PRIMARY KEY AUTOINCREMENT`，且没有物理 FK、trigger 或
`*_row_id`。

最终禁止项静态审计也已执行，并排除了生成目录 `ui/dist`。允许命中仅包括：

- schema contract 测试中用于断言拒绝 `FOREIGN KEY`、`REFERENCES` 和
  `*_row_id` 的字符串；
- logical-reference registry 自身的名称；
- MCP 等 L 类实体在局部实现中的 `selected_row_ids` 变量；
- `Preferences` 等与 SQL `REFERENCES` 无关的普通英文单词；
- continuity 文档中明确的禁止项示例或 Legacy v2 记录。

这些记录仍**不宣称** workspace 全量 `cargo test`、完整 UI ID 测试矩阵
或真实桌面平台矩阵已经通过，也不据此宣称 v3 整体实施完成。

## 5. Gate 4：UI 定向检查

```bash
bun run --filter=./ui typecheck
bun run --filter=./ui test -- <id-related-test-files>
```

至少覆盖：

- ids / normalizer；
- session route；
- storage key；
- provider/MCP mapper；
- message/conversation mapper；
- sorting/pagination；
- v3 import/export/clone。

UI 测试必须覆盖 `1, 2, 10` 的数字排序，并验证旧短 ID、
`prefix_UUIDv7`、UUIDv4、ULID 和雪花字符串在 UUIDv7 业务字段中被拒绝；
外部 ID 则只走外部字段 parser。

## 6. Gate 5：数据集代际闭包验证

hard reset 不验证业务数据映射，而验证代际隔离：

- 旧 DB/WAL/SHM/side-store 全部位于 retired dataset；
- active v3 数据树不存在旧业务文件和旧 generation；
- v3 数据库 quick_check 和全量 orphan audit 通过；
- v3 默认数据满足固定 schema；
- 浏览器缓存 namespace 已旋转；
- 外部 workspace 文件未被删除；
- reset receipt 能在每个中断点恢复；
- 第二次启动不会再次清空已经是 v3 的数据。

任何“跳过、删除、无法恢复”都必须出现在报告，不得只返回成功。

## 7. Gate 6：一次最终全量门禁

只有以下条件同时满足才运行昂贵命令：

1. schema smoke 通过；
2. common/db/API 定向 check 通过；
3. UI typecheck 通过；
4. staging dry-run 和闭包验证通过；
5. 所有 worker 的变更已经合并；
6. `git diff --check` 通过。

最终命令：

```bash
cargo test
bun run check
bun run build:ui
```

按 CI 资源可并行，但只执行一次，不作为每个中间步骤的反馈循环。

## 8. 并发和缓存规则

- DB baseline、logical-reference contract、Common ID、API serde 是串行公共波；
- repository、业务 crate、UI 领域可并行；
- 每个 worker 拥有 disjoint write set；
- 不重复运行相同的全量编译；
- 失败先复现到最小定向命令，不立即跑全量；
- 记录命令、耗时、缓存命中和失败原因到 handoff；
- 不为了“绿测试”删掉旧格式拒绝测试和数据集代际闭包检查。

## 9. 完成定义

本专项不能以“代码能编译”完成。必须证明：

- 没有“所有实体都使用 prefix/UUIDv7”的全局业务契约；
- S 类新业务 ID 使用无前缀、固定结构的标准 UUIDv7；
- 64 张产品持久表全部使用 `id INTEGER PRIMARY KEY AUTOINCREMENT`；
- schema 不含物理外键、数据库级 cascade 或 `*_row_id`；
- 逻辑关联索引、删除策略和 orphan audit 完整；
- S/L/N/D/X 分类在 schema、Rust、协议、UI、side-store 一致；
- 当前 v2 和更旧数据统一进入 hard reset，不进入 v3 业务表；
- v3 restore/clone 只处理 v3 contract；
- backup 覆盖范围真实可证明；
- 失败可回滚，且代码回滚与数据回滚已分开说明。
