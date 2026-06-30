-- 让 orch_runs 脱离强制 workspace：workspace_id 可空 + 新增 work_dir(运行工作目录)。
-- 这样一个 Run 可直接由会话创建、自带工作目录，而不必先建 workspace 实体。
--
-- SQLite 不能 drop NOT NULL，走标准表重建(CREATE new + INSERT…SELECT + DROP old
-- + ALTER…RENAME)。FK 不在此文件内管理：迁移运行器(database.rs::run_migrations)
-- 已在每次迁移外层、事务之外设了 `PRAGMA foreign_keys = OFF` +
-- `PRAGMA legacy_alter_table = ON`，所以这里既不能也不该写 PRAGMA(sqlx 把每个迁移
-- 包进事务，事务内 PRAGMA foreign_keys 是 no-op；legacy_alter_table=ON 已保证
-- RENAME 不会改写 orch_run_tasks 等表对 orch_runs(id) 的 FK 引用)。与 019 约定一致。
CREATE TABLE orch_runs_new (
  id              TEXT PRIMARY KEY,
  workspace_id    TEXT REFERENCES orch_workspaces(id) ON DELETE CASCADE,  -- 可空
  user_id         TEXT NOT NULL,
  goal            TEXT NOT NULL,
  fleet_snapshot  TEXT NOT NULL,
  autonomy        TEXT NOT NULL,
  max_parallel    INTEGER,
  lead_conv_id    INTEGER,
  status          TEXT NOT NULL,
  summary         TEXT,
  total_tokens    INTEGER,
  forked_from     TEXT,
  work_dir        TEXT,
  created_at      INTEGER NOT NULL,
  updated_at      INTEGER NOT NULL
);
INSERT INTO orch_runs_new
  (id, workspace_id, user_id, goal, fleet_snapshot, autonomy, max_parallel,
   lead_conv_id, status, summary, total_tokens, forked_from, work_dir, created_at, updated_at)
SELECT
   id, workspace_id, user_id, goal, fleet_snapshot, autonomy, max_parallel,
   lead_conv_id, status, summary, total_tokens, forked_from, NULL, created_at, updated_at
FROM orch_runs;
DROP TABLE orch_runs;
ALTER TABLE orch_runs_new RENAME TO orch_runs;
CREATE INDEX idx_orch_runs_workspace ON orch_runs(workspace_id);
