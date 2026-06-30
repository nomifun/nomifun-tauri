use nomifun_common::{generate_prefixed_id, now_ms};
use sqlx::SqlitePool;

use crate::models::OrchWorkspaceRow;
use crate::repository::orch_workspace::{
    CreateOrchWorkspaceParams, IOrchWorkspaceRepository, UpdateOrchWorkspaceParams,
};

#[derive(Clone, Debug)]
pub struct SqliteOrchWorkspaceRepository {
    pool: SqlitePool,
}

impl SqliteOrchWorkspaceRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IOrchWorkspaceRepository for SqliteOrchWorkspaceRepository {
    async fn create(&self, p: CreateOrchWorkspaceParams) -> Result<OrchWorkspaceRow, sqlx::Error> {
        let id = generate_prefixed_id("ows");
        let now = now_ms();
        sqlx::query(
            "INSERT INTO orch_workspaces (\
                id, user_id, name, default_fleet_id, workspace_dir, context, created_at, updated_at\
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&p.user_id)
        .bind(&p.name)
        .bind(&p.default_fleet_id)
        .bind(&p.workspace_dir)
        .bind(&p.context)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(OrchWorkspaceRow {
            id,
            user_id: p.user_id,
            name: p.name,
            default_fleet_id: p.default_fleet_id,
            workspace_dir: p.workspace_dir,
            context: p.context,
            created_at: now,
            updated_at: now,
        })
    }

    async fn list(&self, user_id: &str) -> Result<Vec<OrchWorkspaceRow>, sqlx::Error> {
        let rows = sqlx::query_as::<_, OrchWorkspaceRow>(
            "SELECT * FROM orch_workspaces WHERE user_id = ? ORDER BY created_at DESC",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn get(&self, id: &str) -> Result<Option<OrchWorkspaceRow>, sqlx::Error> {
        let row = sqlx::query_as::<_, OrchWorkspaceRow>("SELECT * FROM orch_workspaces WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    async fn update(&self, id: &str, p: UpdateOrchWorkspaceParams) -> Result<(), sqlx::Error> {
        // Build the SET clause conservatively: only touch columns the caller
        // actually supplied. `None` = skip, `Some(None)` = set NULL,
        // `Some(Some(v))` = set v. When nothing changes, return early.
        let mut sets: Vec<&str> = Vec::new();
        if p.name.is_some() {
            sets.push("name = ?");
        }
        if p.default_fleet_id.is_some() {
            sets.push("default_fleet_id = ?");
        }
        if p.workspace_dir.is_some() {
            sets.push("workspace_dir = ?");
        }
        if p.context.is_some() {
            sets.push("context = ?");
        }
        if sets.is_empty() {
            return Ok(());
        }
        sets.push("updated_at = ?");
        let sql = format!("UPDATE orch_workspaces SET {} WHERE id = ?", sets.join(", "));

        let mut q = sqlx::query(&sql);
        if let Some(name) = &p.name {
            q = q.bind(name);
        }
        if let Some(default_fleet_id) = &p.default_fleet_id {
            q = q.bind(default_fleet_id);
        }
        if let Some(workspace_dir) = &p.workspace_dir {
            q = q.bind(workspace_dir);
        }
        if let Some(context) = &p.context {
            q = q.bind(context);
        }
        q = q.bind(now_ms());
        q = q.bind(id);
        q.execute(&self.pool).await?;
        Ok(())
    }

    async fn delete(&self, id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM orch_workspaces WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::init_database_memory;
    use crate::repository::orch_fleet::{CreateFleetParams, IFleetRepository};
    use crate::repository::SqliteFleetRepository;

    #[tokio::test]
    async fn orch_workspace_crud_roundtrip() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteOrchWorkspaceRepository::new(db.pool().clone());

        // 先建一个真实编队，供 default_fleet_id 外键引用
        let fleet_repo = SqliteFleetRepository::new(db.pool().clone());
        let fleet = fleet_repo
            .create_fleet(CreateFleetParams {
                user_id: "u1".into(),
                name: "编队X".into(),
                description: None,
                max_parallel: None,
            })
            .await
            .unwrap();

        // create
        let w = repo
            .create(CreateOrchWorkspaceParams {
                user_id: "u1".into(),
                name: "工作区A".into(),
                default_fleet_id: None,
                workspace_dir: Some("/tmp/ws".into()),
                context: None,
            })
            .await
            .unwrap();
        assert!(w.id.starts_with("ows_"));
        assert_eq!(w.name, "工作区A");
        assert_eq!(w.workspace_dir.as_deref(), Some("/tmp/ws"));
        assert_eq!(w.default_fleet_id, None);

        // list 含新建
        let all = repo.list("u1").await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, w.id);

        // get 命中
        let got = repo.get(&w.id).await.unwrap().unwrap();
        assert_eq!(got.id, w.id);
        assert_eq!(got.name, "工作区A");

        // update：改名 + 换默认编队
        repo.update(
            &w.id,
            UpdateOrchWorkspaceParams {
                name: Some("工作区B".into()),
                default_fleet_id: Some(Some(fleet.id.clone())),
                workspace_dir: None,
                context: None,
            },
        )
        .await
        .unwrap();
        let after = repo.get(&w.id).await.unwrap().unwrap();
        assert_eq!(after.name, "工作区B");
        assert_eq!(after.default_fleet_id.as_deref(), Some(fleet.id.as_str()));
        // 未触碰列保持原值
        assert_eq!(after.workspace_dir.as_deref(), Some("/tmp/ws"));
        assert!(after.updated_at >= w.updated_at);

        // delete → get 为 None
        repo.delete(&w.id).await.unwrap();
        assert!(repo.get(&w.id).await.unwrap().is_none());
        assert_eq!(repo.list("u1").await.unwrap().len(), 0);
    }
}
