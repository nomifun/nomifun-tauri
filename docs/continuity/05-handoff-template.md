# ID / Schema 专项交接模板

复制本文件到 `05-handoffs/YYYY-MM-DD-<topic>.md` 后填写。

```yaml
status: PROPOSED
date_utc:
owner:
account_or_platform:
repository_root: ${REPO_ROOT}
data_dir: ${DATA_DIR}
work_dir: ${WORK_DIR}
branch:
commit:
git_status:
```

## 本次范围

- workstream:
- owned_files:
- non_goals:
- related_decision:

## 已完成

- （填写）

## 证据

- command:
  result:
- test:
  result:
- data_fixture:
  result:

## 未决问题

- （填写）

## 明确禁止

- 不修改其他 worker 的写入范围；
- 不对真实用户目录执行 reset、quarantine 或 GC；
- 不删除旧 DB/WAL/SHM/side-store；
- 不向 v3 schema 增加物理外键、`REFERENCES` 或 `*_row_id`；
- 不创建缺少 `id INTEGER PRIMARY KEY AUTOINCREMENT` 的产品持久表；
- 不把失败改成静默 fallback；
- 不把 `cargo test` 当作数据闭包证明。

## 下一步

1. （填写）
2. （填写）
3. （填写）

## 回滚

- code_baseline:
- data_snapshot:
- side_store_snapshot:
- reset_receipt/report:

## 交接检查

```bash
git status --short
git rev-parse HEAD
git diff --check
sed -n '1,240p' docs/continuity/00-current-state.md
```

不得写入 token、API key、私钥、个人路径、账号密码或真实用户内容。
