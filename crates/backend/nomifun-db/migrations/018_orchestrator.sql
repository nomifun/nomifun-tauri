-- 018 智能编排引擎(取代遗留 team)。append-only；PRAGMA foreign_keys=ON 已由连接设定。
-- ID 规则: 跨 gateway/Remote 暴露的实体用 TEXT 前缀 id(应用层 generate_prefixed_id 生成);
-- worker conversation_id 是本机 conversations.id INTEGER。

CREATE TABLE fleets (
  id            TEXT PRIMARY KEY,
  user_id       TEXT NOT NULL,
  name          TEXT NOT NULL,
  description   TEXT,
  max_parallel  INTEGER,
  created_at    INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL
);

CREATE TABLE fleet_members (
  id                 TEXT PRIMARY KEY,
  fleet_id           TEXT NOT NULL REFERENCES fleets(id) ON DELETE CASCADE,
  agent_id           TEXT NOT NULL,
  provider_id        TEXT,
  model              TEXT,
  role_hint          TEXT,
  capability_profile TEXT,
  constraints        TEXT,
  sort_order         INTEGER NOT NULL DEFAULT 0,
  created_at         INTEGER NOT NULL,
  updated_at         INTEGER NOT NULL
);
CREATE INDEX idx_fleet_members_fleet ON fleet_members(fleet_id);

CREATE TABLE orch_workspaces (
  id                TEXT PRIMARY KEY,
  user_id           TEXT NOT NULL,
  name              TEXT NOT NULL,
  default_fleet_id  TEXT REFERENCES fleets(id) ON DELETE SET NULL,
  workspace_dir     TEXT,
  context           TEXT,
  created_at        INTEGER NOT NULL,
  updated_at        INTEGER NOT NULL
);

CREATE TABLE orch_runs (
  id              TEXT PRIMARY KEY,
  workspace_id    TEXT NOT NULL REFERENCES orch_workspaces(id) ON DELETE CASCADE,
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
  created_at      INTEGER NOT NULL,
  updated_at      INTEGER NOT NULL
);
CREATE INDEX idx_orch_runs_workspace ON orch_runs(workspace_id);

CREATE TABLE orch_run_tasks (
  id              TEXT PRIMARY KEY,
  run_id          TEXT NOT NULL REFERENCES orch_runs(id) ON DELETE CASCADE,
  title           TEXT NOT NULL,
  spec            TEXT NOT NULL,
  task_profile    TEXT,
  status          TEXT NOT NULL,
  conversation_id INTEGER,
  output_summary  TEXT,
  output_files    TEXT,
  attempt         INTEGER NOT NULL DEFAULT 0,
  tokens          INTEGER,
  graph_x         REAL,
  graph_y         REAL,
  created_at      INTEGER NOT NULL,
  updated_at      INTEGER NOT NULL
);
CREATE INDEX idx_orch_run_tasks_run ON orch_run_tasks(run_id);

CREATE TABLE orch_run_task_deps (
  blocker_task_id TEXT NOT NULL REFERENCES orch_run_tasks(id) ON DELETE CASCADE,
  blocked_task_id TEXT NOT NULL REFERENCES orch_run_tasks(id) ON DELETE CASCADE,
  PRIMARY KEY (blocker_task_id, blocked_task_id),
  CHECK (blocker_task_id <> blocked_task_id)
);

CREATE TABLE orch_assignments (
  id          TEXT PRIMARY KEY,
  task_id     TEXT NOT NULL REFERENCES orch_run_tasks(id) ON DELETE CASCADE,
  member_id   TEXT NOT NULL,
  score       REAL,
  rationale   TEXT,
  source      TEXT NOT NULL,
  locked      INTEGER NOT NULL DEFAULT 0,
  created_at  INTEGER NOT NULL
);
CREATE INDEX idx_orch_assignments_task ON orch_assignments(task_id);
