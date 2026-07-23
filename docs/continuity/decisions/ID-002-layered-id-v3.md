# ID-002：Layered ID / Schema v3

状态：`ACCEPTED`

提出时间：2026-07-21

前置决策：`ID-001-current-v2.md`

补充约束：`ID-005-logical-references-autoincrement.md`

## 决策目标

取消全库统一 prefix_UUIDv7 和“禁止 AUTOINCREMENT”的硬约束，改为
逐表、逐字段、逐边界设计。需要分布式业务身份的字段统一使用无前缀、
固定 36 字符的标准 UUIDv7：

- 所有产品持久表统一使用 SQLite 自增技术主键；
- 有跨库/文件/公开 URL/远程协议需求的表增加具名业务 ID；
- 自然键保持自然键；
- 外部 ID 保持 opaque；
- 操作 token 不进入实体关系；
- UUIDv7 固定结构只约束分布式业务 ID，不扩散到技术 `id`、自然键、
  外部 ID 和 operation token。
- 物理外键和 `*_row_id` 双轨关联由 `ID-005` 全面取消。

## 已接受的内容

1. 使用 S/L/N/D/X 分层，不再使用全局实体 ID 格式；
2. 每张产品持久表自身主键固定为
   `id INTEGER PRIMARY KEY AUTOINCREMENT`；
3. S 类增加具名 `*_id TEXT UNIQUE NOT NULL`；
4. S 类业务 ID 使用无前缀标准 UUIDv7；
5. 所有产品持久表统一使用 `AUTOINCREMENT`；
6. v3 不保留旧 API 字段、旧 ID parser 或双读路径；
7. v3 对旧 managed dataset 执行完整代际 hard reset；
8. v3 不创建 SQLite 物理外键，关联统一使用逻辑外键。

## 仍需在完成审计中确认

- `02-table-classification.zh.md` 的逐表领域 owner 审核；
- v3 backup 的 side-store 覆盖闭包；
- reset 锁、崩溃恢复、retired dataset 保留周期和 GC；
- 发布提示和 reset 进度/失败状态的产品交互。

不把所有 ID 改成数字，也不把所有字段改成 UUID；仅 S 类具名业务 ID
使用无前缀 UUIDv7。hard reset 的完整语义由 `ID-004-v3-hard-reset.md`
定义。

## 实施与发布证据

继续生产集成并宣称完成前必须具备：

- 真实数据只读 inventory；
- 每张表的键语义和引用闭包；
- 至少一套标注为 legacy、仅用于 reset 隔离验证的 v2/旧数字/短 ID 混合
  side-store fixture；不得把 fixture 行映射或导入 v3；
- staging hard-reset 演练报告；
- 逻辑引用、JSON 和文件引用闭包验证；
- 失败注入和回滚演练；
- 明确的发布和观察计划。
