# ID-004：v3 全数据集硬重置

状态：`ACCEPTED`

提出时间：2026-07-21

前置决策：`ID-002-layered-id-v3.md`、`ID-003-bare-uuidv7.md`

后续 schema 约束：`ID-005-logical-references-autoincrement.md`

## 决策

v3 使用新的数据库和数据存储 lineage：

- 不兼容历史数据库 schema；
- 不迁移历史业务数据；
- 不导入历史 ID；
- 不保留旧 DTO、旧路由、旧缓存 key 或 alias；
- 首次启动检测到非 v3 dataset 时自动强制把旧 managed dataset 整体
  退出 active 状态，不提供迁移、跳过或继续使用旧数据的选项；
- 初始化完全干净的 v3 数据集。

这项决策以更低开发成本、更少长期兼容分支和确定的新 schema 为目标。

## 为什么不能只清空 SQLite

实体引用分布于：

```text
SQLite + WAL/SHM/journal
storage generation
companion DB/JSON/JSONL/profiles/figures/workspaces
public-agents
attachments
managed knowledge
auto-memory projects
cron generated skills
workshop docs/assets/thumbnails
managed skills/drafts
preview/browser state/profiles/login identity
managed conversation workspaces
browser local/session storage
```

仅删除主数据库会让旧文件和旧缓存与新 ID 重新绑定，产生比历史迁移更难
定位的异常。因此 reset 单位必须是完整 managed dataset。

## Reset 语义

1. 获取排他锁并停止所有写入；
2. 判断 active dataset contract；
3. 已是 v3 时幂等退出；
4. 关闭数据库并闭合文件族；
5. 将旧 managed dataset 移到版本化 retired/quarantine 目录；
6. 不解析、转换或导入其中的业务数据；
7. 在 staging 创建新 v3 数据集；
8. 生成新的 storage generation；
9. 初始化 v3 schema、默认 owner 和内置配置；
10. 验证后原子发布；
11. 保存 reset receipt；
12. 旧数据只允许后续显式 GC，不被 v3 加载。

## 删除与保留

- 产品运行语义上：旧数据被清空，不再可见；
- 文件安全语义上：首次 reset 优先 quarantine，而不是直接不可逆删除；
- quarantine 只服务于原子切换、崩溃恢复和防误删，不构成兼容层；
- resolved `<work_dir>/conversations` 即使位于 `data_dir` 外部，也属于唯一
  的外部产品受管根，整体进入该 work root 下的
  `.nomifun-retired-datasets`；
- 其他任意外部用户 workspace、项目目录和自定义路径不扫描、不删除；
- dataset-owned API key/credential 不迁移到新数据集，用户重新配置；
- 平台级 `auth.json`、`config.toml` 与 workspace/user instruction
  `AGENTS.md` 属于用户控制面，不是 v3 dataset，不随 reset 删除；
- platform user-global `commands` root 已停止读取，不以兼容名义加入
  managed roots；项目自有 `.nomi/commands` 仅作为显式 workspace 输入；
- 浏览器缓存：通过新 generation/namespace 失效；
- 旧 backup：不作为 v3 restore 输入。

如产品最终要求物理永久删除，必须使用独立 GC 操作，并在发布说明中
明确数据不可恢复。

## 已实施的 managed-root 边界

canonical registry 统一驱动 factory reset 和 backup coverage。当前已确认：

- `<app_data>/skills` 是 user-skill 权威根；
- `<app_data>/projects` 保存 auto-memory，并作为 portable root；
- `browser-state`、`browser-profiles`、`login-profile`、
  `browser-secrets` portable include；
- `browser-data` 仍由 reset 管理，但以 runtime/cache 原因从 portable
  backup 排除；
- `<work_dir>/conversations` 通过独立 work namespace 捕获；当
  `work_dir == data_dir` 时不重复捕获；
- backup manifest 必须精确覆盖 registry 中的每个 include/exclude root
  及稳定排除理由，不能静默遗漏未知 root。

`AGENTS.md`、`auth.json`、`config.toml` 和任意未登记外部 workspace 不在
这个 registry 中。这是产品数据与用户控制面的刻意边界，不是 backup
遗漏。

## 完成条件

- [x] 当前已知产品 managed roots 已登记，skills、auto-memory projects、
  browser roots 和 external work conversations 已收编；
- [ ] DB/WAL/SHM 和 side-store 没有跨代残留；
- [x] 已有定向测试覆盖 reset plan、receipt 和部分中断恢复；
- [x] restore 写入新 generation 与匹配 receipt，避免恢复后再次清空；
- [x] arbitrary external workspace 不被扫描或删除；仅
  `<work_dir>/conversations` 是明确受管根；
- [ ] v3 UI 在 reset 完成前不读取业务数据；
- [x] v3 backup/restore 与 Companion import 使用严格 v3 contract；
- [ ] 发布说明明确升级会重置本地产品数据。

以上定向证据不代表真实平台矩阵或整体发布门禁已经完成。
