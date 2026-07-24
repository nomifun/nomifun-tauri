# v3 切换、硬重置与并发执行计划

状态：`ACCEPTED / IMPLEMENTED; RELEASE DRILL PENDING`
原则：按已接受的 v3 contract 分波实施；任何 destructive 操作必须在
staging 完成并通过验证后才允许对真实数据集切换。

## 1. 总体路线

```text
W0 冻结与数据根盘点
  -> W1 目标契约和全新 schema
  -> W2 删除旧代际代码
  -> W3 全数据集 hard-reset 状态机
  -> W4 Rust/协议/业务调用链
  -> W5 UI 与本地存储
  -> W6 side-store / factory reset / backup
  -> W7 旧数据集硬重置演练
  -> W8 发布和观察
```

W3-W6 可以并行，但每个 worker 必须有不重叠的写入范围。数据库基线、
公共 ID 类型和最终集成由单一 owner 负责。

## 2. W0：冻结和只读盘点

不编译、不运行全量 test，只执行：

```bash
git status --short
git rev-parse HEAD
git diff --stat 2c0f975a^ 2c0f975a
rg -n "UUIDv7|AUTOINCREMENT|generate_prefixed_id|validate_prefixed_id" \
  crates ui apps docs/architecture
```

对用户数据目录生成只读 inventory：

- DB 文件、WAL、SHM、journal；
- `_sqlx_migrations`；
- 每张表行数、主键、索引、逻辑关联字段、ID 值样本和类型分布；
- companion `memory.db` 文件族；
- public-agent、figures、workshop、attachments、knowledge、skills、
  preview/browser、workspace；
- 所有配置中的 ID 引用；
- 文件索引与实际文件的闭包。

此阶段不得调用会生成新 ID、旋转 generation 或触发旧库处理的启动函数。

## 3. W1：固化已接受的目标决策

在写代码前完成：

1. 以 `ID-002`、`ID-003`、`ID-004`、`ID-005` 为不可回退的 v3 设计基线；
2. 每张表确认 S/L/N/D/X；
3. API 不保留旧字段别名；
4. 历史业务 ID 不进入 v3；
5. 历史业务数据不迁移，只创建全新 baseline；
6. 确认 64 张产品持久表统一使用
   `id INTEGER PRIMARY KEY AUTOINCREMENT`；
7. 确认 schema 没有物理 FK、`REFERENCES` 或 `*_row_id`；
8. 确认 logical-reference registry、索引和删除策略完整；
9. 确认所有 S 类新业务 ID 使用无前缀标准 UUIDv7；
10. 确认 v3 backup 的 side-store 范围。

没有这些决策，不得让 worker 修改 schema。

## 4. W2：删除旧代际代码

新版本不保留历史读取路径：

- Rust/TS parser 只接受 v3 固定结构；
- 删除数字业务 ID、旧 short ID 和 `prefix_UUIDv7` 的 normalize；
- 删除旧路由、旧 DTO、旧 storage key dual-read；
- 删除旧数据库 lineage 的逐行转换/import；
- MCP session-only 外部 ID 仍按外部协议处理，这属于当前协议而非旧代际支持；
- 不生成 alias 表、legacy_id 列或 old-to-new map。

## 5. W3：全数据集 hard-reset 状态机

hard reset 的对象是整个 managed dataset，不是单个数据库文件：

1. 获取位于数据目录外部或父目录的独占 reset lock；
2. 停止 HTTP、cron、collector、agent、terminal、companion 和文件写入；
3. 识别当前 dataset contract/generation；
4. 如果已经是 v3，幂等退出；
5. 关闭 SQLite pool，处理主 DB、WAL、SHM、journal 文件族；
6. 将旧受管数据根整体移动到
   `retired-datasets/<timestamp>-<old-generation>/`；
7. 如果无法整体移动，则逐根 move-or-fail，任何失败都不创建新代际；
8. 在 sibling staging 创建全新 v3 数据树、数据库和 generation；
9. 初始化默认用户/配置和必要内置数据，不复制用户业务数据；
10. 验证新数据树后原子发布 active dataset；
11. 保留 reset receipt，确保崩溃重启可以继续或安全回滚；
12. 观察期后再由显式 GC 删除 retired dataset。

必须纳入 reset 的受管内容至少包括：

```text
main DB + WAL/SHM/journal
storage generation
companion DB/profiles/events/figures/workspaces
public-agents
attachments
managed knowledge
cron generated skills
workshop documents/assets/thumbnails
managed skills/drafts
preview/browser state
managed conversation workspaces
browser/local/session storage generation namespace
```

外部用户 workspace 不删除，只清除本产品对它们的旧引用。

## 6. W4：Rust 与协议工作流

按不重叠目录分工：

| Worker | 写入范围 | 责任 |
| --- | --- | --- |
| DB | `nomifun-db` | 全新 baseline、models、repository、logical-reference registry |
| Common | `nomifun-common` | 业务 ID/ExternalId/OperationToken、reset root registry；技术行 `id` 留在 repository 内部 |
| API | `nomifun-api-types`, `nomifun-gateway` | serde、Gateway schema、MCP/HTTP/WS boundary |
| Core domains | conversation/requirement/system/cron/knowledge/workshop | 按字段语义改调用链 |
| Runtime | agent-execution/ai-agent/channel/companion | runtime 和 cross-store 引用 |
| Side-store | companion/public-agent/workshop file stores | v3 初始化、目录、JSON、JSONL、文件 key |
| UI | `ui/src` | v3 parser、mapper、route、cache、reset 提示 |

任何 worker 不得修改其他 worker 的公共契约；发现契约问题只写
`open-questions.md`，由 Common/API owner 合并。

## 7. W5：UI 和协议切换

重点不是把所有 ID 显示得更短，而是避免类型错配：

- 每个字段按 DTO 固定声明 `number` 或 `string`，不做历史类型猜测；
- brand 只表示领域，不表示格式；
- 业务 ID 与技术 `id` 使用不同 parser；
- `1, 2, 10` 的排序使用数字字段或本地 `id`，不使用字符串比较；
- ID 显示使用 label/display_no，不能把长 ID 当 UI 主文案；
- browser storage 只读取当前 generation namespace，不读取旧 key；
- 不导入历史 provider/MCP 配置；首次启动使用新默认状态。

## 8. W6：side-store 和 backup

建立 side-root registry，至少明确：

```text
database
companion
public-agents
figures
attachments
knowledge
cron skills
workshop
skills
preview/browser
managed workspaces
external workspace references
```

每个 root 定义：

- 权威性；
- owner 字段；
- ID 引用路径；
- backup 是否包含；
- restore/merge/clone 行为；
- 文件名安全编码；
- orphan 检测和修复；
- 回滚方式。

当前 backup v1 不作为 v3 导入源。可继续保留为旧版本人工取证文件，但
v3 restore 只接受 v3 backup manifest。

## 9. W7：真实数据演练

至少演练四类 fixture：

1. 全新 v3 数据库；
2. 当前 v2 数据库及完整 side-store；
3. v2 以前的数字 + short text 混合数据树；
4. 含 companion、Workshop、attachments、knowledge、public-agent
   的完整数据树。

每个 fixture 做：

- reset preflight；
- 旧数据树完整 quarantine；
- v3 空数据集初始化；
- reset 中断后的重启恢复；
- 第二次启动幂等；
- v3 backup/restore；
- 失败注入后不出现新旧混合数据。

## 10. W8：发布策略

作为明确的数据契约断代发布：

- 发布说明清楚写明“升级会重置本地产品数据”；
- 首次启动检测到非 v3 dataset 时自动强制执行，不提供迁移、跳过或继续
  使用旧数据的选项；
- reset 前创建 retired dataset receipt；
- 新版本不提供旧数据导入按钮；
- 观察窗口结束前不 GC retired dataset；
- 如果产品决定永久删除旧数据，必须是后续独立、可见的 GC 操作。

## 11. 回滚

切换前失败：删除 staging，源数据零变化。

切换后但未开放写入：切回旧数据树和旧 generation。

开放新写入后：不能直接 git rollback；必须从新格式 backup 做 forward
restore，或使用变更日志合并新写入。

每次切换必须保存：

- 文件族 checksum；
- 每表 row count；
- 被 quarantine 的旧数据根 manifest；
- v3 新数据集 quick/logical-reference/side-root report；
- old/new generation；
- 安装阶段日志。
