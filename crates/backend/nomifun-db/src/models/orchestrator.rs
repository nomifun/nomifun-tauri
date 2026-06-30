use nomifun_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Row in the `fleets` table — a named group of agents available for orchestration.
#[derive(Debug, Clone, sqlx::FromRow, Serialize, Deserialize)]
pub struct FleetRow {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub description: Option<String>,
    pub max_parallel: Option<i64>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// Row in the `fleet_members` table — one agent enrolled in a fleet.
#[derive(Debug, Clone, sqlx::FromRow, Serialize, Deserialize)]
pub struct FleetMemberRow {
    pub id: String,
    pub fleet_id: String,
    pub agent_id: String,
    pub provider_id: Option<String>,
    pub model: Option<String>,
    pub role_hint: Option<String>,
    pub capability_profile: Option<String>, // JSON
    pub constraints: Option<String>,        // JSON
    pub sort_order: i64,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// Row in the `orch_workspaces` table — a user workspace scoping orchestration runs.
#[derive(Debug, Clone, sqlx::FromRow, Serialize, Deserialize)]
pub struct OrchWorkspaceRow {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub default_fleet_id: Option<String>,
    pub workspace_dir: Option<String>,
    pub context: Option<String>, // JSON
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// Row in the `orch_runs` table — a single orchestration run (goal decomposition + execution).
#[derive(Debug, Clone, sqlx::FromRow, Serialize, Deserialize)]
pub struct OrchRunRow {
    pub id: String,
    /// Owning workspace, or `None` for an ad-hoc run created straight from a
    /// conversation (such a run carries its own `work_dir` instead).
    pub workspace_id: Option<String>,
    pub user_id: String,
    pub goal: String,
    pub fleet_snapshot: String, // JSON
    pub autonomy: String,
    pub max_parallel: Option<i64>,
    /// Lead/coordinator worker conversation — local `conversations.id` INTEGER.
    pub lead_conv_id: Option<i64>,
    pub status: String,
    pub summary: Option<String>,
    pub total_tokens: Option<i64>,
    pub forked_from: Option<String>,
    /// Working directory for an ad-hoc (workspace-less) run.
    pub work_dir: Option<String>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// Row in the `orch_run_tasks` table — one decomposed task within a run.
#[derive(Debug, Clone, sqlx::FromRow, Serialize, Deserialize)]
pub struct OrchRunTaskRow {
    pub id: String,
    pub run_id: String,
    pub title: String,
    pub spec: String,
    pub task_profile: Option<String>, // JSON
    pub status: String,
    /// Worker conversation — local `conversations.id` INTEGER.
    pub conversation_id: Option<i64>,
    pub output_summary: Option<String>, // JSON
    pub output_files: Option<String>,   // JSON
    pub attempt: i64,
    pub tokens: Option<i64>,
    pub graph_x: Option<f64>,
    pub graph_y: Option<f64>,
    /// Short Chinese role the planner named for this task (P5 沉淀捕获, 迁移 022).
    /// Nullable: tasks planned before this column exist read back as `None`.
    pub role: Option<String>,
    /// Task mode (ultracode 模式增强, 迁移 023). `'agent'`(默认)= 现状单 agent;
    /// `'synthesis'` = 综合上游产出为最终结果。`NOT NULL DEFAULT 'agent'` →
    /// 旧行读回 `"agent"`。
    pub kind: String,
    /// Optional per-kind config JSON (迁移 023, nullable), e.g. fan-out 兄弟任务
    /// 共享的分组标签 `{"group":"<label>"}`。未用时 `None`。
    pub pattern_config: Option<String>, // JSON
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// Row in the `orch_run_task_deps` table — a blocker→blocked edge in the task DAG.
#[derive(Debug, Clone, sqlx::FromRow, Serialize, Deserialize)]
pub struct OrchRunTaskDepRow {
    pub blocker_task_id: String,
    pub blocked_task_id: String,
}

/// Row in the `orch_assignments` table — a member assigned to a task (auto-scored or locked).
#[derive(Debug, Clone, sqlx::FromRow, Serialize, Deserialize)]
pub struct OrchAssignmentRow {
    pub id: String,
    pub task_id: String,
    pub member_id: String,
    pub score: Option<f64>,
    pub rationale: Option<String>,
    pub source: String,
    pub locked: i64,
    pub created_at: TimestampMs,
}

#[cfg(test)]
mod tests {
    use crate::database::init_database_memory;

    #[tokio::test]
    async fn migration_018_creates_orchestrator_tables() {
        let db = init_database_memory()
            .await
            .expect("db init runs all migrations");
        let pool = db.pool();
        // 断言 7 张表存在
        for t in [
            "fleets",
            "fleet_members",
            "orch_workspaces",
            "orch_runs",
            "orch_run_tasks",
            "orch_run_task_deps",
            "orch_assignments",
        ] {
            let row: (i64,) = sqlx::query_as(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?",
            )
            .bind(t)
            .fetch_one(pool)
            .await
            .unwrap();
            assert_eq!(row.0, 1, "table {t} should exist");
        }
    }

    /// 迁移 020：orch_runs 加 `work_dir` 列；`workspace_id` 改为可空。
    /// 既有行（workspace_id 非空）经表重建后必须保留。
    #[tokio::test]
    async fn migration_020_adds_work_dir_and_nullable_workspace_id() {
        let db = init_database_memory()
            .await
            .expect("db init runs all migrations");
        let pool = db.pool();

        // PRAGMA table_info(orch_runs): work_dir 存在 + workspace_id notnull==0。
        let cols: Vec<(String, i64)> =
            sqlx::query_as("SELECT name, \"notnull\" FROM pragma_table_info('orch_runs')")
                .fetch_all(pool)
                .await
                .unwrap();

        let work_dir = cols.iter().find(|(n, _)| n == "work_dir");
        assert!(work_dir.is_some(), "orch_runs should have a work_dir column");

        let ws = cols
            .iter()
            .find(|(n, _)| n == "workspace_id")
            .expect("orch_runs should have a workspace_id column");
        assert_eq!(ws.1, 0, "workspace_id should be nullable (notnull==0)");

        // 表重建保留既有行：种入一行 workspace_id 非空，应原样读回。
        let now = nomifun_common::now_ms();
        // workspace FK 行（init 已建 system user，但 workspace 需先存在）。
        sqlx::query(
            "INSERT INTO orch_workspaces (id, user_id, name, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind("ws_keep")
        .bind("system_default_user")
        .bind("保留区")
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO orch_runs \
             (id, workspace_id, user_id, goal, fleet_snapshot, autonomy, status, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind("run_keep")
        .bind("ws_keep")
        .bind("system_default_user")
        .bind("保留这一行")
        .bind("{}")
        .bind("auto")
        .bind("planning")
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .unwrap();

        let (ws_id, work_dir): (Option<String>, Option<String>) =
            sqlx::query_as("SELECT workspace_id, work_dir FROM orch_runs WHERE id = ?")
                .bind("run_keep")
                .fetch_one(pool)
                .await
                .unwrap();
        assert_eq!(ws_id.as_deref(), Some("ws_keep"), "existing workspace_id preserved");
        assert!(work_dir.is_none(), "new work_dir defaults to NULL");
    }

    /// 迁移 022：orch_run_tasks 加 `role` 列(可空)。沉淀捕获用：规划为每个任务
    /// 命名的角色名持久于此。
    #[tokio::test]
    async fn migration_022_adds_role_to_orch_run_tasks() {
        let db = init_database_memory()
            .await
            .expect("db init runs all migrations");
        let pool = db.pool();

        let cols: Vec<(String, i64)> =
            sqlx::query_as("SELECT name, \"notnull\" FROM pragma_table_info('orch_run_tasks')")
                .fetch_all(pool)
                .await
                .unwrap();

        let role = cols
            .iter()
            .find(|(n, _)| n == "role")
            .expect("orch_run_tasks should have a role column");
        assert_eq!(role.1, 0, "role should be nullable (notnull==0)");
    }

    /// 迁移 023:orch_run_tasks 加 `kind`(NOT NULL DEFAULT 'agent')+
    /// `pattern_config`(可空 JSON)。ultracode 模式增强地基:旧行 kind 默认
    /// 'agent'(零回归),pattern_config 为 NULL。
    #[tokio::test]
    async fn migration_023_adds_kind_and_pattern_config_to_orch_run_tasks() {
        let db = init_database_memory()
            .await
            .expect("db init runs all migrations");
        let pool = db.pool();

        let cols: Vec<(String, i64, Option<String>)> = sqlx::query_as(
            "SELECT name, \"notnull\", dflt_value FROM pragma_table_info('orch_run_tasks')",
        )
        .fetch_all(pool)
        .await
        .unwrap();

        let kind = cols
            .iter()
            .find(|(n, _, _)| n == "kind")
            .expect("orch_run_tasks should have a kind column");
        assert_eq!(kind.1, 1, "kind should be NOT NULL (notnull==1)");
        assert!(
            kind.2.as_deref().unwrap_or_default().contains("agent"),
            "kind default should be 'agent', got {:?}",
            kind.2
        );

        let pattern_config = cols
            .iter()
            .find(|(n, _, _)| n == "pattern_config")
            .expect("orch_run_tasks should have a pattern_config column");
        assert_eq!(pattern_config.1, 0, "pattern_config should be nullable (notnull==0)");
    }
}
