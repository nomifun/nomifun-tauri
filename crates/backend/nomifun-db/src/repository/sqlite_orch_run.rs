use nomifun_common::{generate_prefixed_id, now_ms};
use sqlx::SqlitePool;

use crate::models::{OrchAssignmentRow, OrchRunRow, OrchRunTaskDepRow, OrchRunTaskRow};
use crate::repository::orch_run::{
    CreateAssignmentParams, CreateRunParams, CreateTaskParams, IRunRepository, ReconcileDepRef,
    ReconcilePlan, UpdateRunParams, UpdateTaskParams,
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
        if p.work_dir.is_some() {
            sets.push("work_dir = ?");
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
        if let Some(work_dir) = &p.work_dir {
            // `work_dir: &Option<String>` — sqlx binds `None` as NULL, `Some(v)` as v
            // (skip/NULL/set, matching the `summary` column above).
            q = q.bind(work_dir);
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
                kind, pattern_config, on_fail, created_at, updated_at\
            ) VALUES (?, ?, ?, ?, ?, ?, NULL, NULL, NULL, 0, NULL, ?, ?, ?, ?, ?, ?, ?, ?)",
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
        .bind(&p.kind)
        .bind(&p.pattern_config)
        .bind(&p.on_fail)
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
            kind: p.kind,
            pattern_config: p.pattern_config,
            next_retry_at: None,
            override_provider_id: None,
            override_model: None,
            preset_prompt: None,
            last_error: None,
            on_fail: p.on_fail,
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
        if p.spec.is_some() {
            sets.push("spec = ?");
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
        if p.pattern_config.is_some() {
            sets.push("pattern_config = ?");
        }
        if p.next_retry_at.is_some() {
            sets.push("next_retry_at = ?");
        }
        if p.last_error.is_some() {
            sets.push("last_error = ?");
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
        if let Some(spec) = &p.spec {
            q = q.bind(spec);
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
        if let Some(pattern_config) = &p.pattern_config {
            q = q.bind(pattern_config);
        }
        if let Some(next_retry_at) = &p.next_retry_at {
            q = q.bind(next_retry_at);
        }
        if let Some(last_error) = &p.last_error {
            q = q.bind(last_error);
        }
        q = q.bind(now_ms());
        q = q.bind(id);
        q.execute(&self.pool).await?;
        Ok(())
    }

    async fn set_task_overrides(
        &self,
        id: &str,
        override_provider_id: Option<String>,
        override_model: Option<String>,
        preset_prompt: Option<String>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE orch_run_tasks SET override_provider_id = ?, override_model = ?, \
             preset_prompt = ?, updated_at = ? WHERE id = ?",
        )
        .bind(&override_provider_id)
        .bind(&override_model)
        .bind(&preset_prompt)
        .bind(now_ms())
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn delete_task(&self, id: &str) -> Result<(), sqlx::Error> {
        // One statement: deleting the task fires the task-keyed `ON DELETE CASCADE`
        // FKs (migration 018), sweeping out its dep edges (as blocker OR blocked)
        // and its assignment. The run row + its OTHER tasks are untouched. Requires
        // PRAGMA foreign_keys=ON on the connection (project default).
        sqlx::query("DELETE FROM orch_run_tasks WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
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

    async fn reset_orphaned_running_tasks(
        &self,
        run_id: Option<&str>,
    ) -> Result<u64, sqlx::Error> {
        // Mirror `RunService::reset_task` in one bulk statement: status→pending,
        // clear conversation_id/output_summary/output_files/next_retry_at, bump
        // attempt, and — kind-aware — clear pattern_config ONLY for `agent` tasks
        // (a verify/judge/loop node's pattern_config is its POLICY; wiping it would
        // silently revert to defaults, so the CASE preserves it). The `WHERE status
        // = 'running'` scopes the reset to orphaned in-flight rows; an optional
        // `run_id` narrows it to one run (pause) vs. all runs (boot).
        let base = "UPDATE orch_run_tasks SET \
             status = 'pending', \
             conversation_id = NULL, \
             output_summary = NULL, \
             output_files = NULL, \
             next_retry_at = NULL, \
             attempt = attempt + 1, \
             pattern_config = CASE WHEN kind = 'agent' THEN NULL ELSE pattern_config END, \
             updated_at = ? \
             WHERE status = 'running'";
        let result = match run_id {
            Some(rid) => {
                let sql = format!("{base} AND run_id = ?");
                sqlx::query(&sql)
                    .bind(now_ms())
                    .bind(rid)
                    .execute(&self.pool)
                    .await?
            }
            None => sqlx::query(base).bind(now_ms()).execute(&self.pool).await?,
        };
        Ok(result.rows_affected())
    }

    async fn mark_run_running_tasks_cancelled(&self, run_id: &str) -> Result<(), sqlx::Error> {
        // Status + updated_at only — preserve the interrupted node's partial
        // conversation_id / output so a cancelled run stays inspectable.
        sqlx::query(
            "UPDATE orch_run_tasks SET status = 'cancelled', updated_at = ? \
             WHERE run_id = ? AND status = 'running'",
        )
        .bind(now_ms())
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

    async fn clear_run_deps(&self, run_id: &str) -> Result<(), sqlx::Error> {
        // Delete only this run's edges (join the blocked task back to its run);
        // the tasks themselves are untouched. Used by conversational reconcile to
        // rebuild the DAG while preserving KEPT task rows + their output.
        sqlx::query(
            "DELETE FROM orch_run_task_deps WHERE blocked_task_id IN (\
                 SELECT id FROM orch_run_tasks WHERE run_id = ?\
             )",
        )
        .bind(run_id)
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

    async fn reconcile_run_plan(
        &self,
        run_id: &str,
        plan: ReconcilePlan,
    ) -> Result<(), sqlx::Error> {
        // ONE transaction wraps the WHOLE reconcile (UC-3a 评审 Important-A): clear
        // deps → delete un-kept → insert new (+ assignment) → rebuild deps. Any
        // error short-circuits via `?`, the `tx` Drop rolls everything back (we
        // only `commit()` at the very end), so a mid-way DB failure leaves the run
        // unchanged — no durable half-reconciled state. Mirrors `set_assignment`'s
        // coarse-transaction pattern; no sqlx type escapes this method.
        let now = now_ms();
        let mut tx = self.pool.begin().await?;

        // (1) Clear the run's dep edges (kept tasks lose their wiring but survive).
        sqlx::query(
            "DELETE FROM orch_run_task_deps WHERE blocked_task_id IN (\
                 SELECT id FROM orch_run_tasks WHERE run_id = ?\
             )",
        )
        .bind(run_id)
        .execute(&mut *tx)
        .await?;

        // (2) Delete every un-kept task. The task-keyed ON DELETE CASCADE FKs sweep
        //     out each task's (already-cleared) deps + its assignment.
        for task_id in &plan.delete_task_ids {
            sqlx::query("DELETE FROM orch_run_tasks WHERE id = ?")
                .bind(task_id)
                .execute(&mut *tx)
                .await?;
        }

        // (3) Insert each NEW task (pending) + its pre-computed assignment. Mint the
        //     ids HERE (inside the tx) and remember them in plan order so (4) can
        //     resolve `NewIndex(i)` dep refs.
        let mut new_ids: Vec<String> = Vec::with_capacity(plan.new_tasks.len());
        for new_task in &plan.new_tasks {
            let task_id = generate_prefixed_id("rtask");
            let t = &new_task.task;
            sqlx::query(
                "INSERT INTO orch_run_tasks (\
                    id, run_id, title, spec, task_profile, status, conversation_id, \
                    output_summary, output_files, attempt, tokens, graph_x, graph_y, role, \
                    kind, pattern_config, on_fail, created_at, updated_at\
                ) VALUES (?, ?, ?, ?, ?, ?, NULL, NULL, NULL, 0, NULL, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&task_id)
            .bind(run_id)
            .bind(&t.title)
            .bind(&t.spec)
            .bind(&t.task_profile)
            .bind(&t.status)
            .bind(t.graph_x)
            .bind(t.graph_y)
            .bind(&t.role)
            .bind(&t.kind)
            .bind(&t.pattern_config)
            .bind(&t.on_fail)
            .bind(now)
            .bind(now)
            .execute(&mut *tx)
            .await?;

            if let Some(asg) = &new_task.assignment {
                let asg_id = generate_prefixed_id("asg");
                let locked: i64 = if asg.locked { 1 } else { 0 };
                sqlx::query(
                    "INSERT INTO orch_assignments (\
                        id, task_id, member_id, score, rationale, source, locked, created_at\
                    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(&asg_id)
                // The minted task id wins over whatever placeholder the service set.
                .bind(&task_id)
                .bind(&asg.member_id)
                .bind(asg.score)
                .bind(&asg.rationale)
                .bind(&asg.source)
                .bind(locked)
                .bind(now)
                .execute(&mut *tx)
                .await?;
            }

            new_ids.push(task_id);
        }

        // (4) Rebuild the dep edges from the plan: each new task's blockers resolve
        //     to a kept id (verbatim) or a freshly minted new id (by index). A
        //     self-edge is skipped (the table CHECKs blocker <> blocked); an
        //     out-of-range NewIndex cannot occur (the service validated ranges).
        for (idx, new_task) in plan.new_tasks.iter().enumerate() {
            let blocked_id = &new_ids[idx];
            for dep_ref in &new_task.depends_on {
                let blocker_id = match dep_ref {
                    ReconcileDepRef::Kept(id) => id.clone(),
                    ReconcileDepRef::NewIndex(i) => match new_ids.get(*i) {
                        Some(id) => id.clone(),
                        None => continue,
                    },
                };
                if &blocker_id == blocked_id {
                    continue;
                }
                sqlx::query(
                    "INSERT INTO orch_run_task_deps (blocker_task_id, blocked_task_id) VALUES (?, ?)",
                )
                .bind(&blocker_id)
                .bind(blocked_id)
                .execute(&mut *tx)
                .await?;
            }
        }

        tx.commit().await?;
        Ok(())
    }

    async fn list_ready_tasks(&self, run_id: &str) -> Result<Vec<OrchRunTaskRow>, sqlx::Error> {
        // Ready = pending AND no incomplete blocker AND not currently backing off.
        // A task with zero dep rows trivially satisfies the NOT EXISTS (the subquery
        // is empty); a task with multiple blockers is ready only when EVERY blocker
        // is 'done' (any single non-'done' blocker makes the subquery non-empty).
        // The `next_retry_at` gate (迁移 024) excludes a task still in transient-error
        // backoff: `NULL` (the common case — never failed retryably) is always ready;
        // a future timestamp is held out until it elapses. This keeps the read model
        // the single authority on readiness, so the engine loop never busy-spins on a
        // task that is "pending" but not yet due.
        let rows = sqlx::query_as::<_, OrchRunTaskRow>(
            "SELECT t.* FROM orch_run_tasks t \
             WHERE t.run_id = ? AND t.status = 'pending' \
             AND (t.next_retry_at IS NULL OR t.next_retry_at <= ?) AND NOT EXISTS (\
                 SELECT 1 FROM orch_run_task_deps d \
                 JOIN orch_run_tasks bt ON bt.id = d.blocker_task_id \
                 WHERE d.blocked_task_id = t.id AND bt.status != 'done'\
             ) ORDER BY t.created_at ASC",
        )
        .bind(run_id)
        .bind(now_ms())
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
    use crate::repository::orch_run::ReconcileNewTask;
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
                last_error: None,
                status: Some("done".into()),
                spec: None,
                conversation_id: None,
                output_summary: None,
                output_files: None,
                attempt: None,
                tokens: None,
                graph_x: None,
                graph_y: None,
                pattern_config: None,
                next_retry_at: None,
            },
        )
        .await
        .unwrap();
    }

    // 迁移 024: a `pending` task gated by a future `next_retry_at` (transient-error
    // backoff) is held OUT of the ready set until the timestamp elapses; a NULL gate
    // (the common case) is always ready, and a past gate is ready again.
    #[tokio::test]
    async fn list_ready_tasks_respects_retry_backoff_gate() {
        let db = init_database_memory().await.unwrap();
        let pool = db.pool().clone();
        let ws_id = make_workspace(&pool).await;
        let repo = SqliteRunRepository::new(pool);
        let run = repo
            .create_run(CreateRunParams {
                workspace_id: Some(ws_id),
                user_id: "u1".into(),
                goal: "retry gate".into(),
                fleet_snapshot: "{}".into(),
                autonomy: "auto".into(),
                max_parallel: Some(1),
                lead_conv_id: None,
                work_dir: None,
            })
            .await
            .unwrap();
        let t = repo
            .create_task(CreateTaskParams {
                run_id: run.id.clone(),
                title: "solo".into(),
                spec: "s".into(),
                task_profile: None,
                status: "pending".into(),
                graph_x: None,
                graph_y: None,
                role: None,
                kind: "agent".into(),
                pattern_config: None,
                on_fail: None,
            })
            .await
            .unwrap();

        // NULL gate → ready (no regression for normal pending tasks).
        let ready = repo.list_ready_tasks(&run.id).await.unwrap();
        assert_eq!(ready.iter().map(|t| t.id.clone()).collect::<Vec<_>>(), vec![t.id.clone()], "NULL gate is ready");

        let set_gate = |at: Option<i64>| UpdateTaskParams {
            next_retry_at: Some(at),
            ..Default::default()
        };

        // Future gate → held out of the ready set.
        repo.update_task(&t.id, set_gate(Some(now_ms() + 60_000))).await.unwrap();
        assert!(repo.list_ready_tasks(&run.id).await.unwrap().is_empty(), "future gate holds the task out");

        // Past gate → ready again.
        repo.update_task(&t.id, set_gate(Some(now_ms() - 1))).await.unwrap();
        assert_eq!(
            repo.list_ready_tasks(&run.id).await.unwrap().into_iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![t.id.clone()],
            "past gate is ready"
        );

        // Cleared gate → ready.
        repo.update_task(&t.id, set_gate(None)).await.unwrap();
        assert_eq!(
            repo.list_ready_tasks(&run.id).await.unwrap().into_iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![t.id],
            "cleared gate is ready"
        );
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
            kind: "agent".into(),
            pattern_config: None,
            on_fail: None,
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
                work_dir: None,
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
            kind: "agent".into(),
            pattern_config: None,
            on_fail: None,
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
            kind: "agent".into(),
            pattern_config: None,
            on_fail: None,
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
            kind: "agent".into(),
            pattern_config: None,
            on_fail: None,
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
                work_dir: None,
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
            kind: "agent".into(),
            pattern_config: None,
            on_fail: None,
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

    // UC-3a: delete_task removes ONE task (cascading its dep edges — as blocker
    // OR blocked — and its assignment) while the run + its OTHER tasks survive
    // intact. This is the conversational-reconcile "drop an unkept task" step.
    #[tokio::test]
    async fn delete_task_removes_one_task_and_cascades_its_edges_and_assignment() {
        let db = init_database_memory().await.unwrap();
        let pool = db.pool().clone();
        let ws_id = make_workspace(&pool).await;
        let repo = SqliteRunRepository::new(pool);
        let run = repo
            .create_run(CreateRunParams {
                workspace_id: Some(ws_id),
                user_id: "u1".into(),
                goal: "reconcile".into(),
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
            kind: "agent".into(),
            pattern_config: None,
            on_fail: None,
        };
        // A→B→C chain; delete B (the middle task) — both its edges (A→B, B→C) and
        // its assignment must cascade; A and C (and their kept edges/assignments)
        // survive.
        let a = repo.create_task(mk("A")).await.unwrap();
        let b = repo.create_task(mk("B")).await.unwrap();
        let c = repo.create_task(mk("C")).await.unwrap();
        repo.add_dep(&a.id, &b.id).await.unwrap();
        repo.add_dep(&b.id, &c.id).await.unwrap();
        for t in [&a, &b, &c] {
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
        assert_eq!(repo.list_tasks(&run.id).await.unwrap().len(), 3);
        assert_eq!(repo.list_deps(&run.id).await.unwrap().len(), 2);
        assert_eq!(repo.list_assignments(&run.id).await.unwrap().len(), 3);

        repo.delete_task(&b.id).await.unwrap();

        // B is gone; A and C survive.
        assert!(repo.get_task(&b.id).await.unwrap().is_none(), "B deleted");
        assert!(repo.get_task(&a.id).await.unwrap().is_some(), "A kept");
        assert!(repo.get_task(&c.id).await.unwrap().is_some(), "C kept");
        // Both of B's edges cascaded → zero edges remain.
        assert!(repo.list_deps(&run.id).await.unwrap().is_empty(), "B's edges cascaded");
        // B's assignment cascaded; A's and C's survive.
        assert_eq!(repo.list_assignments(&run.id).await.unwrap().len(), 2, "only B's assignment gone");
        assert!(repo.get_assignment_for_task(&b.id).await.unwrap().is_none(), "B's assignment gone");
        assert!(repo.get_assignment_for_task(&a.id).await.unwrap().is_some(), "A's assignment kept");
        // The run row survives.
        assert!(repo.get_run(&run.id).await.unwrap().is_some(), "run kept");
    }

    // UC-3a: clear_run_deps removes ALL of a run's dep edges while leaving the
    // tasks (and their assignments) intact — the reconcile "rebuild the DAG" step.
    // A second run's edges are untouched (scoped to the target run).
    #[tokio::test]
    async fn clear_run_deps_clears_edges_keeps_tasks() {
        let db = init_database_memory().await.unwrap();
        let pool = db.pool().clone();
        let ws_id = make_workspace(&pool).await;
        let repo = SqliteRunRepository::new(pool);
        let run = repo
            .create_run(CreateRunParams {
                workspace_id: Some(ws_id.clone()),
                user_id: "u1".into(),
                goal: "rewire".into(),
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
            kind: "agent".into(),
            pattern_config: None,
            on_fail: None,
        };
        let a = repo.create_task(mk(run.id.clone(), "A")).await.unwrap();
        let b = repo.create_task(mk(run.id.clone(), "B")).await.unwrap();
        repo.add_dep(&a.id, &b.id).await.unwrap();

        // A second run with its own edge that must survive.
        let other = repo
            .create_run(CreateRunParams {
                workspace_id: Some(ws_id),
                user_id: "u1".into(),
                goal: "other".into(),
                fleet_snapshot: "[]".into(),
                autonomy: "supervised".into(),
                max_parallel: None,
                lead_conv_id: None,
                work_dir: None,
            })
            .await
            .unwrap();
        let oa = repo.create_task(mk(other.id.clone(), "OA")).await.unwrap();
        let ob = repo.create_task(mk(other.id.clone(), "OB")).await.unwrap();
        repo.add_dep(&oa.id, &ob.id).await.unwrap();

        assert_eq!(repo.list_deps(&run.id).await.unwrap().len(), 1);

        repo.clear_run_deps(&run.id).await.unwrap();

        // The target run's edges are gone but its tasks survive.
        assert!(repo.list_deps(&run.id).await.unwrap().is_empty(), "target run edges cleared");
        assert!(repo.get_task(&a.id).await.unwrap().is_some(), "task A kept");
        assert!(repo.get_task(&b.id).await.unwrap().is_some(), "task B kept");
        // The OTHER run's edge is untouched.
        assert_eq!(repo.list_deps(&other.id).await.unwrap().len(), 1, "other run's edge survives");
    }

    // UC-3a 评审 Important-A: reconcile_run_plan applies the WHOLE reconcile in ONE
    // transaction. A successful call clears the run's deps, deletes the un-kept
    // tasks (cascading their assignment), inserts the new tasks + their
    // pre-computed assignment, and rebuilds the deps — all committed atomically.
    #[tokio::test]
    async fn reconcile_run_plan_applies_whole_reconcile_atomically() {
        let db = init_database_memory().await.unwrap();
        let pool = db.pool().clone();
        let ws_id = make_workspace(&pool).await;
        let repo = SqliteRunRepository::new(pool);
        let run = repo
            .create_run(CreateRunParams {
                workspace_id: Some(ws_id),
                user_id: "u1".into(),
                goal: "reconcile-atomic".into(),
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
            kind: "agent".into(),
            pattern_config: None,
            on_fail: None,
        };
        // Seed: keep + drop tasks, an edge keep→drop, and an assignment on each.
        let keep = repo.create_task(mk("keep")).await.unwrap();
        let drop = repo.create_task(mk("drop")).await.unwrap();
        repo.add_dep(&keep.id, &drop.id).await.unwrap();
        for t in [&keep, &drop] {
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

        // Reconcile: drop the "drop" task; add one NEW task depending on the kept
        // one (kept-id ref) with its own auto assignment.
        let new_task = ReconcileNewTask {
            task: CreateTaskParams {
                run_id: run.id.clone(),
                title: "new".into(),
                spec: "ns".into(),
                task_profile: None,
                status: "pending".into(),
                graph_x: None,
                graph_y: None,
                role: None,
                kind: "agent".into(),
                pattern_config: None,
                on_fail: None,
            },
            assignment: Some(CreateAssignmentParams {
                task_id: String::new(), // overwritten with the minted id
                member_id: "fmem_new".into(),
                score: Some(0.5),
                rationale: Some("auto".into()),
                source: "auto".into(),
                locked: false,
            }),
            depends_on: vec![ReconcileDepRef::Kept(keep.id.clone())],
        };
        repo.reconcile_run_plan(
            &run.id,
            ReconcilePlan {
                delete_task_ids: vec![drop.id.clone()],
                new_tasks: vec![new_task],
            },
        )
        .await
        .expect("reconcile commits");

        // The kept task survives WITH its assignment; the dropped task + assignment
        // are gone; exactly one new task exists, routed; the new edge is wired.
        let tasks = repo.list_tasks(&run.id).await.unwrap();
        assert_eq!(tasks.len(), 2, "kept + new: {tasks:?}");
        assert!(tasks.iter().any(|t| t.id == keep.id), "kept survives");
        assert!(!tasks.iter().any(|t| t.id == drop.id), "dropped gone");
        let new_row = tasks.iter().find(|t| t.title == "new").expect("new task");
        assert_eq!(new_row.status, "pending");
        assert!(repo.get_task(&drop.id).await.unwrap().is_none(), "drop row gone");
        assert!(
            repo.get_assignment_for_task(&drop.id).await.unwrap().is_none(),
            "drop assignment cascaded"
        );
        assert!(
            repo.get_assignment_for_task(&keep.id).await.unwrap().is_some(),
            "kept assignment preserved"
        );
        let new_asg = repo
            .get_assignment_for_task(&new_row.id)
            .await
            .unwrap()
            .expect("new task routed");
        assert_eq!(new_asg.member_id, "fmem_new", "new assignment uses minted task id");
        let deps = repo.list_deps(&run.id).await.unwrap();
        assert_eq!(deps.len(), 1, "exactly the rebuilt edge: {deps:?}");
        assert_eq!(deps[0].blocker_task_id, keep.id);
        assert_eq!(deps[0].blocked_task_id, new_row.id);
    }

    // UC-3a 评审 Important-A (rollback): a mid-transaction error rolls the WHOLE
    // reconcile back — the run is left EXACTLY as it was (no half-state). We force a
    // failure in the dep-rebuild phase (step 4, AFTER deletes + inserts) by wiring a
    // NEW task's dep to a NON-EXISTENT kept id, which violates the
    // orch_run_task_deps FK. The expected post-state: the "drop" task is STILL
    // present (its delete rolled back), NO new task was persisted, and the original
    // edge survives.
    #[tokio::test]
    async fn reconcile_run_plan_rolls_back_on_mid_transaction_error() {
        let db = init_database_memory().await.unwrap();
        let pool = db.pool().clone();
        let ws_id = make_workspace(&pool).await;
        let repo = SqliteRunRepository::new(pool);
        let run = repo
            .create_run(CreateRunParams {
                workspace_id: Some(ws_id),
                user_id: "u1".into(),
                goal: "reconcile-rollback".into(),
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
            kind: "agent".into(),
            pattern_config: None,
            on_fail: None,
        };
        let keep = repo.create_task(mk("keep")).await.unwrap();
        let drop = repo.create_task(mk("drop")).await.unwrap();
        repo.add_dep(&keep.id, &drop.id).await.unwrap();

        // A NEW task whose dep references a NON-EXISTENT kept id → the dep INSERT in
        // step (4) violates the FK and aborts the transaction.
        let bad_new = ReconcileNewTask {
            task: CreateTaskParams {
                run_id: run.id.clone(),
                title: "phantom".into(),
                spec: "ns".into(),
                task_profile: None,
                status: "pending".into(),
                graph_x: None,
                graph_y: None,
                role: None,
                kind: "agent".into(),
                pattern_config: None,
                on_fail: None,
            },
            assignment: None,
            depends_on: vec![ReconcileDepRef::Kept("rtask_does_not_exist".into())],
        };
        let err = repo
            .reconcile_run_plan(
                &run.id,
                ReconcilePlan {
                    delete_task_ids: vec![drop.id.clone()],
                    new_tasks: vec![bad_new],
                },
            )
            .await
            .expect_err("FK violation must error");
        // Any sqlx error is fine; the point is the rollback below.
        let _ = err;

        // ROLLBACK: the run is unchanged. The "drop" task still exists (its delete
        // rolled back), NO new task persisted, and the original edge survives.
        let tasks = repo.list_tasks(&run.id).await.unwrap();
        assert_eq!(tasks.len(), 2, "no task added or removed: {tasks:?}");
        assert!(tasks.iter().any(|t| t.id == keep.id), "keep present");
        assert!(tasks.iter().any(|t| t.id == drop.id), "drop NOT deleted (rollback)");
        assert!(
            !tasks.iter().any(|t| t.title == "phantom"),
            "no new task persisted (rollback)"
        );
        let deps = repo.list_deps(&run.id).await.unwrap();
        assert_eq!(deps.len(), 1, "original edge survives: {deps:?}");
        assert_eq!(deps[0].blocker_task_id, keep.id);
        assert_eq!(deps[0].blocked_task_id, drop.id);
    }

    // Helper: create a task with an explicit kind + status + optional pattern_config.
    async fn mk_task(
        repo: &SqliteRunRepository,
        run_id: &str,
        title: &str,
        kind: &str,
        status: &str,
        pattern_config: Option<&str>,
    ) -> String {
        repo.create_task(CreateTaskParams {
            run_id: run_id.into(),
            title: title.into(),
            spec: "s".into(),
            task_profile: None,
            status: status.into(),
            graph_x: None,
            graph_y: None,
            role: None,
            kind: kind.into(),
            pattern_config: pattern_config.map(str::to_string),
            on_fail: None,
        })
        .await
        .unwrap()
        .id
    }

    async fn mk_run(repo: &SqliteRunRepository, ws_id: Option<String>, goal: &str) -> String {
        repo.create_run(CreateRunParams {
            workspace_id: ws_id,
            user_id: "u1".into(),
            goal: goal.into(),
            fleet_snapshot: "{}".into(),
            autonomy: "auto".into(),
            max_parallel: Some(1),
            lead_conv_id: None,
            work_dir: None,
        })
        .await
        .unwrap()
        .id
    }

    // Fix A CORE: `reset_orphaned_running_tasks(None)` settles EVERY `running` task
    // back to `pending` (clearing conv/output/next_retry_at, bumping attempt) while
    // being KIND-AWARE — an `agent` task's pattern_config (a stale loop-body carry)
    // is cleared, but a `verify`/`judge`/`loop` node's pattern_config (its POLICY)
    // is PRESERVED. Non-running rows (done/pending) are untouched. This is the boot
    // reconciliation that cures the「重启后卡在执行中」orphan.
    #[tokio::test]
    async fn reset_orphaned_running_tasks_settles_all_kind_aware() {
        let db = init_database_memory().await.unwrap();
        let pool = db.pool().clone();
        let ws = make_workspace(&pool).await;
        let repo = SqliteRunRepository::new(pool);
        let run = mk_run(&repo, Some(ws), "boot reconcile").await;

        let agent_run = mk_task(&repo, &run, "agent-run", "agent", "running", Some("{\"group\":\"g\"}")).await;
        let verify_run = mk_task(&repo, &run, "verify-run", "verify", "running", Some("{\"votes\":\"unanimous\"}")).await;
        let done_t = mk_task(&repo, &run, "done", "agent", "done", Some("{\"keep\":1}")).await;
        let pending_t = mk_task(&repo, &run, "pending", "agent", "pending", None).await;

        // Stamp the running tasks with live-worker residue (conv/output/attempt/gate)
        // so we can prove the reset clears it. The `done` task keeps its output.
        for tid in [&agent_run, &verify_run, &done_t] {
            repo.update_task(
                tid,
                UpdateTaskParams {
                    conversation_id: Some(Some(900)),
                    output_summary: Some(Some("[\"partial\"]".into())),
                    output_files: Some(Some("[]".into())),
                    attempt: Some(2),
                    next_retry_at: Some(Some(now_ms() + 60_000)),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }

        let reset = repo.reset_orphaned_running_tasks(None).await.unwrap();
        assert_eq!(reset, 2, "exactly the two running tasks were reset");

        let by_id = |tasks: &[OrchRunTaskRow], id: &str| tasks.iter().find(|t| t.id == id).unwrap().clone();
        let tasks = repo.list_tasks(&run).await.unwrap();

        let a = by_id(&tasks, &agent_run);
        assert_eq!(a.status, "pending", "agent running → pending");
        assert_eq!(a.conversation_id, None, "conversation cleared");
        assert_eq!(a.output_summary, None, "output cleared");
        assert_eq!(a.output_files, None, "output files cleared");
        assert_eq!(a.next_retry_at, None, "retry gate cleared");
        assert_eq!(a.attempt, 3, "attempt bumped (2 → 3)");
        assert_eq!(a.pattern_config, None, "agent pattern_config (loop carry) cleared");

        let v = by_id(&tasks, &verify_run);
        assert_eq!(v.status, "pending", "verify running → pending");
        assert_eq!(v.conversation_id, None, "conversation cleared");
        assert_eq!(v.attempt, 3, "attempt bumped");
        assert_eq!(
            v.pattern_config.as_deref(),
            Some("{\"votes\":\"unanimous\"}"),
            "verify pattern_config (POLICY) PRESERVED"
        );

        let d = by_id(&tasks, &done_t);
        assert_eq!(d.status, "done", "done task untouched");
        assert_eq!(d.attempt, 2, "done attempt unchanged");
        assert_eq!(d.conversation_id, Some(900), "done output preserved");

        let p = by_id(&tasks, &pending_t);
        assert_eq!(p.status, "pending", "pending task still pending");
        assert_eq!(p.attempt, 0, "pending attempt NOT bumped (only running rows reset)");
    }

    // Fix C SCOPING: `reset_orphaned_running_tasks(Some(run))` resets ONLY that run's
    // running tasks — another run's running task is left live (the pause / rerun
    // liveness path must not touch sibling runs).
    #[tokio::test]
    async fn reset_orphaned_running_tasks_scoped_to_one_run() {
        let db = init_database_memory().await.unwrap();
        let pool = db.pool().clone();
        let ws = make_workspace(&pool).await;
        let repo = SqliteRunRepository::new(pool);
        let run_a = mk_run(&repo, Some(ws.clone()), "run A").await;
        let run_b = mk_run(&repo, Some(ws), "run B").await;
        let a_task = mk_task(&repo, &run_a, "a", "agent", "running", None).await;
        let b_task = mk_task(&repo, &run_b, "b", "agent", "running", None).await;

        let n = repo.reset_orphaned_running_tasks(Some(&run_a)).await.unwrap();
        assert_eq!(n, 1, "only run A's running task reset");

        let a = repo.get_task(&a_task).await.unwrap().unwrap();
        assert_eq!(a.status, "pending", "run A's task reset to pending");
        let b = repo.get_task(&b_task).await.unwrap().unwrap();
        assert_eq!(b.status, "running", "run B's task left untouched");
    }

    // Fix B PRIMITIVE: `mark_run_running_tasks_cancelled` marks ONLY the run's
    // `running` tasks `cancelled` (preserving their partial output for inspection),
    // leaving settled/pending tasks and other runs untouched.
    #[tokio::test]
    async fn mark_run_running_tasks_cancelled_settles_only_running() {
        let db = init_database_memory().await.unwrap();
        let pool = db.pool().clone();
        let ws = make_workspace(&pool).await;
        let repo = SqliteRunRepository::new(pool);
        let run = mk_run(&repo, Some(ws.clone()), "cancel run").await;
        let other = mk_run(&repo, Some(ws), "other run").await;
        let running = mk_task(&repo, &run, "running", "agent", "running", None).await;
        let done_t = mk_task(&repo, &run, "done", "agent", "done", None).await;
        let other_running = mk_task(&repo, &other, "other", "agent", "running", None).await;
        // Stamp partial output on the running task — cancel must PRESERVE it.
        repo.update_task(
            &running,
            UpdateTaskParams { conversation_id: Some(Some(901)), ..Default::default() },
        )
        .await
        .unwrap();

        repo.mark_run_running_tasks_cancelled(&run).await.unwrap();

        let r = repo.get_task(&running).await.unwrap().unwrap();
        assert_eq!(r.status, "cancelled", "running → cancelled");
        assert_eq!(r.conversation_id, Some(901), "partial output preserved (still inspectable)");
        let d = repo.get_task(&done_t).await.unwrap().unwrap();
        assert_eq!(d.status, "done", "settled task untouched");
        let o = repo.get_task(&other_running).await.unwrap().unwrap();
        assert_eq!(o.status, "running", "other run's task untouched");
    }
}
