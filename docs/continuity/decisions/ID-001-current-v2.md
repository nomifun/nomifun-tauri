# ID-001：Legacy Current ID-contract-v2

状态：`SUPERSEDED`

> 本决策仅记录 legacy v2 历史事实。它不定义当前 schema、ID parser、
> backup/restore 或升级行为；当前契约以 ID-002—ID-005 和 v3 architecture
> 文档为准。

实施基线：`2c0f975a`

记录时间：2026-07-21

## 决策内容（历史事实）

当前版本曾采用：

```text
prefix_UUIDv7
TEXT entity PK/FK
禁止 AUTOINCREMENT
严格拒绝旧格式
旧 lineage quarantine + 新库重建
```

## 结论

该设计已经在代码中实施，但不再作为后续架构依据。它将：

- 数据库行主键；
- 业务主键；
- 外部/协议 ID；
- 文件 key；
- 操作 token

错误地压缩成一个全局格式，增加了兼容和维护成本。

## 后续处理

- 不直接在 v2 上加例外；
- 以 `ID-002-layered-id-v3.md` 为新设计入口；
- v2 值只存在于 retired/quarantine 数据集中，不进入 v3 parser；
- v2 数据不迁移、不转换、不导入 v3；
- 代码回滚和数据回滚分开管理。
