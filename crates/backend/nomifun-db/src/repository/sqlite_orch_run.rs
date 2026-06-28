use nomifun_common::{generate_prefixed_id, now_ms};
use sqlx::SqlitePool;

use crate::models::{OrchAssignmentRow, OrchRunRow, OrchRunTaskDepRow, OrchRunTaskRow};
use crate::repository::orch_run::{
    CreateAssignmentParams, CreateRunParams, CreateTaskParams, IRunRepository, UpdateRunParams,
    UpdateTaskParams,
};

#[derive(Clone, Debug)]
pub struct SqliteRunRepository {
    pool: SqlitePool,
}

impl SqliteRunRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IRunRepository for SqliteRunRepository {
    // --- runs ---

    async fn create_run(&self, p: CreateRunParams) -> Result<OrchRunRow, sqlx::Error> {
        let id = generate_prefixed_id("run");
        let now = now_ms();
        let status = "planning".to_string();
        sqlx::query(
            "INSERT INTO orch_runs (\
                id, workspace_id, user_id, goal, fleet_snapshot, autonomy, max_parallel, \
                lead_conv_id, status, summary, total_tokens, forked_from, work_dir, \
                created_at, updated_at\
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, NULL, NULL, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&p.workspace_id)
        .bind(&p.user_id)
        .bind(&p.goal)
        .bind(&p.fleet_snapshot)
        .bind(&p.autonomy)
        .bind(p.max_parallel)
        .bind(p.lead_conv_id)
        .bind(&status)
        .bind(&p.work_dir)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(OrchRunRow {
            id,
            workspace_id: p.workspace_id,
            user_id: p.user_id,
            goal: p.goal,
            fleet_snapshot: p.fleet_snapshot,
            autonomy: p.autonomy,
            max_parallel: p.max_parallel,
            lead_conv_id: p.lead_conv_id,
            status,
            summary: None,
            total_tokens: None,
            forked_from: None,
            work_dir: p.work_dir,
            created_at: now,
            updated_at: now,
        })
    }

    async fn get_run(&self, id: &str) -> Result<Option<OrchRunRow>, sqlx::Error> {
        let row = sqlx::query_as::<_, OrchRunRow>("SELECT * FROM orch_runs WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    async fn list_runs(&self, workspace_id: &str) -> Result<Vec<OrchRunRow>, sqlx::Error> {
        let rows = sqlx::query_as::<_, OrchRunRow>(
            "SELECT * FROM orch_runs WHERE workspace_id = ? ORDER BY created_at DESC",
        )
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn list_runs_by_user(&self, user_id: &str) -> Result<Vec<OrchRunRow>, sqlx::Error> {
        let rows = sqlx::query_as::<_, OrchRunRow>(
            "SELECT * FROM orch_runs WHERE user_id = ? ORDER BY created_at DESC",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn list_runs_by_status(&self, status: &str) -> Result<Vec<OrchRunRow>, sqlx::Error> {
        let rows = sqlx::query_as::<_, OrchRunRow>(
            "SELECT * FROM orch_runs WHERE status = ? ORDER BY created_at ASC",
        )
        .bind(status)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn update_run(&self, id: &str, p: UpdateRunParams) -> Result<(), sqlx::Error> {
        // Build the SET clause conservatively: only touch columns the caller
        // actually supplied. For nullable columns `None` = skip,
        // `Some(None)` = set NULL, `Some(Some(v))` = set v. `status` is a plain
        // non-null column (`None` = skip, `Some(v)` = set v).
        let mut sets: Vec<&str> = Vec::new();
        if p.status.is_some() {
            sets.push("status = ?");
        }
        if p.summary.is_some() {
            sets.push("summary = ?");
        }
        if p.lead_conv_id.is_some() {
            sets.push("lead_conv_id = ?");
        }
        if p.total_tokens.is_some() {
            sets.push("total_tokens = ?");
        }
        if p.goal.is_some() {
            sets.push("goal = ?");
        }
        if p.autonomy.is_some() {
            sets.push("autonomy = ?");
        }
        if p.fleet_snapshot.is_some() {
            sets.push("fleet_snapshot = ?");
        }
        if sets.is_empty() {
            return Ok(());
        }
        sets.push("updated_at = ?");
        let sql = format!("UPDATE orch_runs SET {} WHERE id = ?", sets.join(", "));

        let mut q = sqlx::query(&sql);
        if let Some(status) = &p.status {
            q = q.bind(status);
        }
        if let Some(summary) = &p.summary {
            q = q.bind(summary);
        }
        if let Some(lead_conv_id) = &p.lead_conv_id {
            q = q.bind(lead_conv_id);
        }
        if let Some(total_tokens) = &p.total_tokens {
            q = q.bind(total_tokens);
        }
        if let Some(goal) = &p.goal {
            q = q.bind(goal);
        }
        if let Some(autonomy) = &p.autonomy {
            q = q.bind(autonomy);
        }
        if let Some(fleet_snapshot) = &p.fleet_snapshot {
            q = q.bind(fleet_snapshot);
        }
        q = q.bind(now_ms());
        q = q.bind(id);
        q.execute(&self.pool).await?;
        Ok(())
    }

    async fn delete_run(&self, id: &str) -> Result<(), sqlx::Error> {
        // One statement: the `ON DELETE CASCADE` FKs (migration 018) sweep out
        // the run's tasks → deps + assignments. Requires PRAGMA foreign_keys=ON
        // on the connection (project default).
        sqlx::query("DELETE FROM orch_runs WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // --- tasks ---

    async fn create_task(&self, p: CreateTaskParams) -> Result<OrchRunTaskRow, sqlx::Error> {
        let id = generate_prefixed_id("rtask");
        let now = now_ms();
        sqlx::query(
            "INSERT INTO orch_run_tasks (\
                id, run_id, title, spec, task_profile, status, conversation_id, \
                output_summary, output_files, attempt, tokens, graph_x, graph_y, role, \
                created_at, updated_at\
            ) VALUES (?, ?, ?, ?, ?, ?, NULL, NULL, NULL, 0, NULL, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&p.run_id)
        .bind(&p.title)
        .bind(&p.spec)
        .bind(&p.task_profile)
        .bind(&p.status)
        .bind(p.graph_x)
        .bind(p.graph_y)
        .bind(&p.role)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(OrchRunTaskRow {
            id,
            run_id: p.run_id,
            title: p.title,
            spec: p.spec,
            task_profile: p.task_profile,
            status: p.status,
            conversation_id: None,
            output_summary: None,
            output_files: None,
            attempt: 0,
            tokens: None,
            graph_x: p.graph_x,
            graph_y: p.graph_y,
            role: p.role,
            created_at: now,
            updated_at: now,
        })
    }

    async fn list_tasks(&self, run_id: &str) -> Result<Vec<OrchRunTaskRow>, sqlx::Error> {
        let rows = sqlx::query_as::<_, OrchRunTaskRow>(
            "SELECT * FROM orch_run_tasks WHERE run_id = ? ORDER BY created_at ASC",
        )
        .bind(run_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn get_task(&self, id: &str) -> Result<Option<OrchRunTaskRow>, sqlx::Error> {
        let row = sqlx::query_as::<_, OrchRunTaskRow>("SELECT * FROM orch_run_tasks WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    async fn update_task(&self, id: &str, p: UpdateTaskParams) -> Result<(), sqlx::Error> {
        // Nullable columns use double-`Option` (skip/NULL/set); `status`,
        // `attempt`, `graph_x`, `graph_y` are plain skip-on-`None`.
        let mut sets: Vec<&str> = Vec::new();
        if p.status.is_some() {
            sets.push("status = ?");
        }
        if p.conversation_id.is_some() {
            sets.push("conversation_id = ?");
        }
        if p.output_summary.is_some() {
            sets.push("output_summary = ?");
        }
        if p.output_files.is_some() {
            sets.push("output_files = ?");
        }
        if p.attempt.is_some() {
            sets.push("attempt = ?");
        }
        if p.tokens.is_some() {
            sets.push("tokens = ?");
        }
        if p.graph_x.is_some() {
            sets.push("graph_x = ?");
        }
        if p.graph_y.is_some() {
            sets.push("graph_y = ?");
        }
        if sets.is_empty() {
            return Ok(());
        }
        sets.push("updated_at = ?");
        let sql = format!("UPDATE orch_run_tasks SET {} WHERE id = ?", sets.join(", "));

        let mut q = sqlx::query(&sql);
        if let Some(status) = &p.status {
            q = q.bind(status);
        }
        if let Some(conversation_id) = &p.conversation_id {
            q = q.bind(conversation_id);
        }
        if let Some(output_summary) = &p.output_summary {
            q = q.bind(output_summary);
        }
        if let Some(output_files) = &p.output_files {
            q = q.bind(output_files);
        }
        if let Some(attempt) = &p.attempt {
            q = q.bind(attempt);
        }
        if let Some(tokens) = &p.tokens {
            q = q.bind(tokens);
        }
        if let Some(graph_x) = &p.graph_x {
            q = q.bind(graph_x);
        }
        if let Some(graph_y) = &p.graph_y {
            q = q.bind(graph_y);
        }
        q = q.bind(now_ms());
        q = q.bind(id);
        q.execute(&self.pool).await?;
        Ok(())
    }

    async fn clear_run_tasks(&self, run_id: &str) -> Result<(), sqlx::Error> {
        // One statement: deleting the run's tasks fires the task-keyed
        // `ON DELETE CASCADE` FKs (migration 018), sweeping out that run's deps +
        // assignments. The `orch_runs` row is untouched. Requires PRAGMA
        // foreign_keys=ON on the connection (project default).
        sqlx::query("DELETE FROM orch_run_tasks WHERE run_id = ?")
            .bind(run_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // --- deps ---

    async fn add_dep(&self, blocker: &str, blocked: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO orch_run_task_deps (blocker_task_id, blocked_task_id) VALUES (?, ?)",
        )
        .bind(blocker)
        .bind(blocked)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn list_deps(&self, run_id: &str) -> Result<Vec<OrchRunTaskDepRow>, sqlx::Error> {
        // Scope to a run by joining the blocked task back to its run.
        let rows = sqlx::query_as::<_, OrchRunTaskDepRow>(
            "SELECT d.blocker_task_id, d.blocked_task_id FROM orch_run_task_deps d \
             JOIN orch_run_tasks t ON t.id = d.blocked_task_id \
             WHERE t.run_id = ?",
        )
        .bind(run_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn list_ready_tasks(&self, run_id: &str) -> Result<Vec<OrchRunTaskRow>, sqlx::Error> {
        // Ready = pending AND no incomplete blocker. A task with zero dep rows
        // trivially satisfies the NOT EXISTS (the subquery is empty); a task
        // with multiple blockers is ready only when EVERY blocker is 'done'
        // (any single non-'done' blocker makes the subquery non-empty).
        let rows = sqlx::query_as::<_, OrchRunTaskRow>(
            "SELECT t.* FROM orch_run_tasks t \
             WHERE t.run_id = ? AND t.status = 'pending' AND NOT EXISTS (\
                 SELECT 1 FROM orch_run_task_deps d \
                 JOIN orch_run_tasks bt ON bt.id = d.blocker_task_id \
                 WHERE d.blocked_task_id = t.id AND bt.status != 'done'\
             ) ORDER BY t.created_at ASC",
        )
        .bind(run_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    // --- assignments ---

    async fn create_assignment(
        &self,
        p: CreateAssignmentParams,
    ) -> Result<OrchAssignmentRow, sqlx::Error> {
        let id = generate_prefixed_id("asg");
        let now = now_ms();
        let locked: i64 = if p.locked { 1 } else { 0 };
        sqlx::query(
            "INSERT INTO orch_assignments (\
                id, task_id, member_id, score, rationale, source, locked, created_at\
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&p.task_id)
        .bind(&p.member_id)
        .bind(p.score)
        .bind(&p.rationale)
        .bind(&p.source)
        .bind(locked)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(OrchAssignmentRow {
            id,
            task_id: p.task_id,
            member_id: p.member_id,
            score: p.score,
            rationale: p.rationale,
            source: p.source,
            locked,
            created_at: now,
        })
    }

    async fn set_assignment(
        &self,
        p: CreateAssignmentParams,
    ) -> Result<OrchAssignmentRow, sqlx::Error> {
        // Upsert = delete-by-task + create. A task carries at most one effective
        // assignment; an override cleanly replaces it (rather than stacking rows,
        // which `get_assignment_for_task`'s "latest" semantics would otherwise
        // mask). Done in a transaction so a reader never observes the gap between
        // the delete and the insert.
        let id = generate_prefixed_id("asg");
        let now = now_ms();
        let locked: i64 = if p.locked { 1 } else { 0 };
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM orch_assignments WHERE task_id = ?")
            .bind(&p.task_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO orch_assignments (\
                id, task_id, member_id, score, rationale, source, locked, created_at\
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&p.task_id)
        .bind(&p.member_id)
        .bind(p.score)
        .bind(&p.rationale)
        .bind(&p.source)
        .bind(locked)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(OrchAssignmentRow {
            id,
            task_id: p.task_id,
            member_id: p.member_id,
            score: p.score,
            rationale: p.rationale,
            source: p.source,
            locked,
            created_at: now,
        })
    }

    async fn list_assignments(&self, run_id: &str) -> Result<Vec<OrchAssignmentRow>, sqlx::Error> {
        let rows = sqlx::query_as::<_, OrchAssignmentRow>(
            "SELECT a.* FROM orch_assignments a \
             JOIN orch_run_tasks t ON t.id = a.task_id \
             WHERE t.run_id = ? ORDER BY a.created_at ASC",
        )
        .bind(run_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn get_assignment_for_task(
        &self,
        task_id: &str,
    ) -> Result<Option<OrchAssignmentRow>, sqlx::Error> {
        let row = sqlx::query_as::<_, OrchAssignmentRow>(
            "SELECT * FROM orch_assignments WHERE task_id = ? ORDER BY created_at DESC LIMIT 1",
        )
        .bind(task_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::init_database_memory;
    use crate::repository::{
        CreateOrchWorkspaceParams, IOrchWorkspaceRepository, SqliteOrchWorkspaceRepository,
    };

    /// Helper: insert a workspace so run FK (`orch_runs.workspace_id`) holds.
    async fn make_workspace(pool: &SqlitePool) -> String {
        let ws_repo = SqliteOrchWorkspaceRepository::new(pool.clone());
        ws_repo
            .create(CreateOrchWorkspaceParams {
                user_id: "u1".into(),
                name: "工作区A".into(),
                default_fleet_id: None,
                workspace_dir: None,
                context: None,
            })
            .await
            .unwrap()
            .id
    }

    async fn done(repo: &SqliteRunRepository, task_id: &str) {
        repo.update_task(
            task_id,
            UpdateTaskParams {
                status: Some("done".into()),
                conversation_id: None,
                output_summary: None,
                output_files: None,
                attempt: None,
                tokens: None,
                graph_x: None,
                graph_y: None,
            },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_task_dep_dag_roundtrip() {
        let db = init_database_memory().await.unwrap();
        let pool = db.pool().clone();
        let ws_id = make_workspace(&pool).await;
        let repo = SqliteRunRepository::new(pool);

        // --- run ---
        let run = repo
            .create_run(CreateRunParams {
                workspace_id: Some(ws_id.clone()),
                user_id: "u1".into(),
                goal: "构建编排引擎".into(),
                fleet_snapshot: "{}".into(),
                autonomy: "auto".into(),
                max_parallel: Some(2),
                lead_conv_id: None,
                work_dir: None,
            })
            .await
            .unwrap();
        assert!(run.id.starts_with("run_"), "run id prefix: {}", run.id);
        assert_eq!(run.status, "planning");
        assert!(run.lead_conv_id.is_none());

        // get_run / list_runs roundtrip
        assert_eq!(repo.get_run(&run.id).await.unwrap().unwrap().id, run.id);
        assert_eq!(repo.list_runs(&ws_id).await.unwrap().len(), 1);

        // --- 3 tasks A,B,C ---
        let mk = |title: &str| CreateTaskParams {
            run_id: run.id.clone(),
            title: title.into(),
            spec: format!("spec-{title}"),
            task_profile: None,
            status: "pending".into(),
            graph_x: None,
            graph_y: None,
            role: None,
        };
        let a = repo.create_task(mk("A")).await.unwrap();
        let b = repo.create_task(mk("B")).await.unwrap();
        let c = repo.create_task(mk("C")).await.unwrap();
        assert!(a.id.starts_with("rtask_"), "task id prefix: {}", a.id);
        assert_eq!(a.attempt, 0);
        assert_eq!(repo.list_tasks(&run.id).await.unwrap().len(), 3);
        assert_eq!(repo.get_task(&b.id).await.unwrap().unwrap().title, "B");

        // --- deps: A→B, B→C (A blocks B, B blocks C) ---
        repo.add_dep(&a.id, &b.id).await.unwrap();
        repo.add_dep(&b.id, &c.id).await.unwrap();
        let deps = repo.list_deps(&run.id).await.unwrap();
        assert_eq!(deps.len(), 2);

        // initial: only A is ready (B blocked by A, C blocked by B)
        let ready: Vec<String> = repo
            .list_ready_tasks(&run.id)
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(ready, vec![a.id.clone()], "only A ready initially");

        // A done → B ready (A is no longer an incomplete blocker)
        done(&repo, &a.id).await;
        let ready: Vec<String> = repo
            .list_ready_tasks(&run.id)
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(ready, vec![b.id.clone()], "B ready after A done");

        // B done → C ready
        done(&repo, &b.id).await;
        let ready: Vec<String> = repo
            .list_ready_tasks(&run.id)
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(ready, vec![c.id.clone()], "C ready after B done");

        // --- assignment roundtrip ---
        let asg = repo
            .create_assignment(CreateAssignmentParams {
                task_id: c.id.clone(),
                member_id: "fmem_x".into(),
                score: Some(0.87),
                rationale: Some("最强后端".into()),
                source: "auto".into(),
                locked: true,
            })
            .await
            .unwrap();
        assert!(asg.id.starts_with("asg_"), "asg id prefix: {}", asg.id);
        assert_eq!(asg.locked, 1);
        let got = repo
            .get_assignment_for_task(&c.id)
            .await
            .unwrap()
            .expect("assignment exists");
        assert_eq!(got.id, asg.id);
        assert_eq!(got.member_id, "fmem_x");
        assert_eq!(repo.list_assignments(&run.id).await.unwrap().len(), 1);

        // --- update_run + list_runs_by_status('running') ---
        repo.update_run(
            &run.id,
            UpdateRunParams {
                status: Some("running".into()),
                summary: Some(Some("进行中".into())),
                lead_conv_id: Some(Some(42)),
                total_tokens: None,
                goal: None,
                autonomy: None,
                fleet_snapshot: None,
            },
        )
        .await
        .unwrap();
        let running = repo.list_runs_by_status("running").await.unwrap();
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].id, run.id);
        let refreshed = repo.get_run(&run.id).await.unwrap().unwrap();
        assert_eq!(refreshed.status, "running");
        assert_eq!(refreshed.summary.as_deref(), Some("进行中"));
        assert_eq!(refreshed.lead_conv_id, Some(42));
        assert!(refreshed.total_tokens.is_none(), "total_tokens skipped");
    }

    #[tokio::test]
    async fn no_dep_task_is_ready_and_multi_blocker_gating() {
        let db = init_database_memory().await.unwrap();
        let pool = db.pool().clone();
        let ws_id = make_workspace(&pool).await;
        let repo = SqliteRunRepository::new(pool);
        let run = repo
            .create_run(CreateRunParams {
                workspace_id: Some(ws_id),
                user_id: "u1".into(),
                goal: "g".into(),
                fleet_snapshot: "{}".into(),
                autonomy: "auto".into(),
                max_parallel: None,
                lead_conv_id: None,
                work_dir: None,
            })
            .await
            .unwrap();
        let mk = |title: &str| CreateTaskParams {
            run_id: run.id.clone(),
            title: title.into(),
            spec: "s".into(),
            task_profile: None,
            status: "pending".into(),
            graph_x: None,
            graph_y: None,
            role: None,
        };
        // p1, p2 are independent blockers of c; standalone has no deps.
        let p1 = repo.create_task(mk("p1")).await.unwrap();
        let p2 = repo.create_task(mk("p2")).await.unwrap();
        let c = repo.create_task(mk("c")).await.unwrap();
        let standalone = repo.create_task(mk("standalone")).await.unwrap();
        repo.add_dep(&p1.id, &c.id).await.unwrap();
        repo.add_dep(&p2.id, &c.id).await.unwrap();

        // standalone (zero deps) + both blockers are ready; c is gated.
        let ready: Vec<String> = repo
            .list_ready_tasks(&run.id)
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(ready, vec![p1.id.clone(), p2.id.clone(), standalone.id.clone()]);

        // Only one blocker done → c still gated by the other.
        done(&repo, &p1.id).await;
        let ready: Vec<String> = repo
            .list_ready_tasks(&run.id)
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert!(!ready.contains(&c.id), "c gated while p2 incomplete");
        assert!(ready.contains(&p2.id));

        // Both blockers done → c ready.
        done(&repo, &p2.id).await;
        let ready: Vec<String> = repo
            .list_ready_tasks(&run.id)
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert!(ready.contains(&c.id), "c ready after both blockers done");
    }

    // P5 Task 2: list_runs_by_user returns every run owned by the given user
    // (workspace-backed AND ad-hoc workspace_id=NULL), newest first, and excludes
    // other users' runs. This is the read path the repurposed orchestrator tab (a
    // read-only Run-history library) uses — adhoc runs created from conversations
    // carry workspace_id=NULL and so never surface under the workspace-scoped
    // `list_runs`; they must surface here.
    #[tokio::test]
    async fn list_runs_by_user_includes_adhoc_and_excludes_other_users() {
        let db = init_database_memory().await.unwrap();
        let pool = db.pool().clone();
        // A workspace owned by user A (FK target for A's workspace-backed run).
        let ws_repo = SqliteOrchWorkspaceRepository::new(pool.clone());
        let ws_a = ws_repo
            .create(CreateOrchWorkspaceParams {
                user_id: "user_a".into(),
                name: "A 的工作区".into(),
                default_fleet_id: None,
                workspace_dir: None,
                context: None,
            })
            .await
            .unwrap()
            .id;
        let repo = SqliteRunRepository::new(pool);

        // User A: one workspace-backed run, one ad-hoc run (workspace_id=NULL).
        let a_ws_run = repo
            .create_run(CreateRunParams {
                workspace_id: Some(ws_a.clone()),
                user_id: "user_a".into(),
                goal: "A 工作区 run".into(),
                fleet_snapshot: "[]".into(),
                autonomy: "supervised".into(),
                max_parallel: None,
                lead_conv_id: None,
                work_dir: None,
            })
            .await
            .unwrap();
        let a_adhoc_run = repo
            .create_run(CreateRunParams {
                workspace_id: None, // ad-hoc: created straight from a conversation
                user_id: "user_a".into(),
                goal: "A 临时 run".into(),
                fleet_snapshot: "[]".into(),
                autonomy: "supervised".into(),
                max_parallel: None,
                lead_conv_id: Some(7),
                work_dir: Some("/tmp/a".into()),
            })
            .await
            .unwrap();
        assert!(a_adhoc_run.workspace_id.is_none(), "adhoc run has no workspace");

        // User B: one run that must NOT appear in A's listing.
        let b_run = repo
            .create_run(CreateRunParams {
                workspace_id: None,
                user_id: "user_b".into(),
                goal: "B 的 run".into(),
                fleet_snapshot: "[]".into(),
                autonomy: "supervised".into(),
                max_parallel: None,
                lead_conv_id: None,
                work_dir: None,
            })
            .await
            .unwrap();

        let a_runs = repo.list_runs_by_user("user_a").await.unwrap();
        let a_ids: Vec<&str> = a_runs.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(a_runs.len(), 2, "A owns exactly two runs (workspace + adhoc)");
        assert!(a_ids.contains(&a_ws_run.id.as_str()), "workspace-backed run present");
        assert!(a_ids.contains(&a_adhoc_run.id.as_str()), "adhoc (NULL ws) run present");
        assert!(!a_ids.contains(&b_run.id.as_str()), "B's run excluded");

        // Newest first: created_at DESC. (created_at is now_ms(); they may share a
        // millisecond, so assert the ordering is non-increasing rather than a
        // strict adhoc-then-ws order.)
        for w in a_runs.windows(2) {
            assert!(
                w[0].created_at >= w[1].created_at,
                "runs ordered by created_at DESC: {} >= {}",
                w[0].created_at,
                w[1].created_at
            );
        }

        // B sees only its own run.
        let b_runs = repo.list_runs_by_user("user_b").await.unwrap();
        assert_eq!(b_runs.len(), 1);
        assert_eq!(b_runs[0].id, b_run.id);
    }

    // P1 Task 1: delete_run removes the run AND cascades (FK ON DELETE CASCADE)
    // to its tasks → deps + assignments. We seed a full aggregate (run + 2 tasks
    // + 1 dep edge + 2 assignments), assert it all exists, delete the run, then
    // assert the run, its tasks, its dep edges, and its assignments are ALL gone.
    // (This proves PRAGMA foreign_keys=ON is active on the connection — without it
    // the children would orphan rather than cascade.) A second, untouched run's
    // rows must survive (delete is scoped to the target run).
    #[tokio::test]
    async fn delete_run_cascades_tasks_deps_assignments() {
        let db = init_database_memory().await.unwrap();
        let pool = db.pool().clone();
        let ws_id = make_workspace(&pool).await;
        let repo = SqliteRunRepository::new(pool);

        // Target run with a 2-task chain (A→B), each carrying an assignment.
        let run = repo
            .create_run(CreateRunParams {
                workspace_id: Some(ws_id.clone()),
                user_id: "u1".into(),
                goal: "to be deleted".into(),
                fleet_snapshot: "[]".into(),
                autonomy: "supervised".into(),
                max_parallel: None,
                lead_conv_id: None,
                work_dir: None,
            })
            .await
            .unwrap();
        let mk = |title: &str| CreateTaskParams {
            run_id: run.id.clone(),
            title: title.into(),
            spec: "s".into(),
            task_profile: None,
            status: "pending".into(),
            graph_x: None,
            graph_y: None,
            role: None,
        };
        let a = repo.create_task(mk("A")).await.unwrap();
        let b = repo.create_task(mk("B")).await.unwrap();
        repo.add_dep(&a.id, &b.id).await.unwrap();
        for t in [&a, &b] {
            repo.create_assignment(CreateAssignmentParams {
                task_id: t.id.clone(),
                member_id: "fmem_x".into(),
                score: None,
                rationale: None,
                source: "auto".into(),
                locked: false,
            })
            .await
            .unwrap();
        }

        // A second run that must NOT be touched by deleting the first.
        let survivor = repo
            .create_run(CreateRunParams {
                workspace_id: Some(ws_id),
                user_id: "u1".into(),
                goal: "survivor".into(),
                fleet_snapshot: "[]".into(),
                autonomy: "supervised".into(),
                max_parallel: None,
                lead_conv_id: None,
                work_dir: None,
            })
            .await
            .unwrap();
        let s_task = repo
            .create_task(CreateTaskParams {
                run_id: survivor.id.clone(),
                title: "S".into(),
                spec: "s".into(),
                task_profile: None,
                status: "pending".into(),
                graph_x: None,
                graph_y: None,
                role: None,
            })
            .await
            .unwrap();

        // Pre-condition: the full aggregate is present.
        assert!(repo.get_run(&run.id).await.unwrap().is_some());
        assert_eq!(repo.list_tasks(&run.id).await.unwrap().len(), 2);
        assert_eq!(repo.list_deps(&run.id).await.unwrap().len(), 1);
        assert_eq!(repo.list_assignments(&run.id).await.unwrap().len(), 2);

        // Delete the run → one statement, FK cascade does the rest.
        repo.delete_run(&run.id).await.unwrap();

        // The run and EVERY descendant row are gone.
        assert!(repo.get_run(&run.id).await.unwrap().is_none(), "run row deleted");
        assert!(repo.list_tasks(&run.id).await.unwrap().is_empty(), "tasks cascaded");
        assert!(repo.list_deps(&run.id).await.unwrap().is_empty(), "deps cascaded");
        assert!(
            repo.list_assignments(&run.id).await.unwrap().is_empty(),
            "assignments cascaded"
        );
        // The dep + assignment rows are gone at the row level too (the task FK
        // cascade reached them, not just the run-scoped list query).
        assert!(repo.get_task(&a.id).await.unwrap().is_none(), "task A row gone");
        assert!(repo.get_assignment_for_task(&a.id).await.unwrap().is_none(), "assignment A gone");

        // The untouched run survived intact.
        assert!(repo.get_run(&survivor.id).await.unwrap().is_some(), "survivor run kept");
        assert!(repo.get_task(&s_task.id).await.unwrap().is_some(), "survivor task kept");
    }

    // P1 Task 1: update_run with `goal: Some(v)` rewrites the run goal (rename),
    // leaving the other columns untouched (goal=None skips the column).
    #[tokio::test]
    async fn update_run_sets_goal() {
        let db = init_database_memory().await.unwrap();
        let pool = db.pool().clone();
        let ws_id = make_workspace(&pool).await;
        let repo = SqliteRunRepository::new(pool);
        let run = repo
            .create_run(CreateRunParams {
                workspace_id: Some(ws_id),
                user_id: "u1".into(),
                goal: "old goal".into(),
                fleet_snapshot: "[]".into(),
                autonomy: "supervised".into(),
                max_parallel: None,
                lead_conv_id: None,
                work_dir: None,
            })
            .await
            .unwrap();

        repo.update_run(
            &run.id,
            UpdateRunParams {
                status: None,
                summary: None,
                lead_conv_id: None,
                total_tokens: None,
                goal: Some("new goal".into()),
                autonomy: None,
                fleet_snapshot: None,
            },
        )
        .await
        .unwrap();

        let refreshed = repo.get_run(&run.id).await.unwrap().unwrap();
        assert_eq!(refreshed.goal, "new goal", "goal rewritten");
        assert_eq!(refreshed.status, "planning", "status untouched (goal-only update)");
    }

    // P1 Task 2: clear_run_tasks removes a run's tasks (and cascades their deps +
    // assignments via the task-keyed FKs) while LEAVING the run row intact — this
    // is the replan "clear old plan" step. We seed a full aggregate (run + 2 tasks
    // + 1 dep + 2 assignments), clear the tasks, then assert the tasks/deps/
    // assignments are gone but the run survives. A second, untouched run's rows
    // must survive (clear is scoped to the target run_id).
    #[tokio::test]
    async fn clear_run_tasks_removes_tasks_deps_assignments_keeps_run() {
        let db = init_database_memory().await.unwrap();
        let pool = db.pool().clone();
        let ws_id = make_workspace(&pool).await;
        let repo = SqliteRunRepository::new(pool);

        // Target run with a 2-task chain (A→B), each carrying an assignment.
        let run = repo
            .create_run(CreateRunParams {
                workspace_id: Some(ws_id.clone()),
                user_id: "u1".into(),
                goal: "to be replanned".into(),
                fleet_snapshot: "[]".into(),
                autonomy: "supervised".into(),
                max_parallel: None,
                lead_conv_id: None,
                work_dir: None,
            })
            .await
            .unwrap();
        let mk = |run_id: String, title: &str| CreateTaskParams {
            run_id,
            title: title.into(),
            spec: "s".into(),
            task_profile: None,
            status: "pending".into(),
            graph_x: None,
            graph_y: None,
            role: None,
        };
        let a = repo.create_task(mk(run.id.clone(), "A")).await.unwrap();
        let b = repo.create_task(mk(run.id.clone(), "B")).await.unwrap();
        repo.add_dep(&a.id, &b.id).await.unwrap();
        for t in [&a, &b] {
            repo.create_assignment(CreateAssignmentParams {
                task_id: t.id.clone(),
                member_id: "fmem_x".into(),
                score: None,
                rationale: None,
                source: "auto".into(),
                locked: false,
            })
            .await
            .unwrap();
        }

        // A second run that must NOT be touched by clearing the first's tasks.
        let survivor = repo
            .create_run(CreateRunParams {
                workspace_id: Some(ws_id),
                user_id: "u1".into(),
                goal: "survivor".into(),
                fleet_snapshot: "[]".into(),
                autonomy: "supervised".into(),
                max_parallel: None,
                lead_conv_id: None,
                work_dir: None,
            })
            .await
            .unwrap();
        let s_task = repo.create_task(mk(survivor.id.clone(), "S")).await.unwrap();

        // Pre-condition: the full aggregate is present.
        assert_eq!(repo.list_tasks(&run.id).await.unwrap().len(), 2);
        assert_eq!(repo.list_deps(&run.id).await.unwrap().len(), 1);
        assert_eq!(repo.list_assignments(&run.id).await.unwrap().len(), 2);

        // Clear the run's tasks → the task-keyed FK cascade sweeps deps + assignments.
        repo.clear_run_tasks(&run.id).await.unwrap();

        // The run row survives; its tasks + deps + assignments are all gone.
        assert!(repo.get_run(&run.id).await.unwrap().is_some(), "run row kept");
        assert!(repo.list_tasks(&run.id).await.unwrap().is_empty(), "tasks cleared");
        assert!(repo.list_deps(&run.id).await.unwrap().is_empty(), "deps cascaded");
        assert!(
            repo.list_assignments(&run.id).await.unwrap().is_empty(),
            "assignments cascaded"
        );
        // Row-level too: the task A row + its assignment are gone (FK reached them).
        assert!(repo.get_task(&a.id).await.unwrap().is_none(), "task A row gone");
        assert!(
            repo.get_assignment_for_task(&a.id).await.unwrap().is_none(),
            "assignment A gone"
        );

        // The untouched run's task survived (clear is scoped to the target run).
        assert!(repo.get_task(&s_task.id).await.unwrap().is_some(), "survivor task kept");
    }
}
