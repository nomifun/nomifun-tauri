use nomifun_common::{generate_prefixed_id, now_ms};
use sqlx::SqlitePool;

use crate::models::{FleetMemberRow, FleetRow};
use crate::repository::orch_fleet::{
    CreateFleetParams, IFleetRepository, NewFleetMember, UpdateFleetParams,
};

#[derive(Clone, Debug)]
pub struct SqliteFleetRepository {
    pool: SqlitePool,
}

impl SqliteFleetRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IFleetRepository for SqliteFleetRepository {
    async fn create_fleet(&self, p: CreateFleetParams) -> Result<FleetRow, sqlx::Error> {
        let id = generate_prefixed_id("fleet");
        let now = now_ms();
        sqlx::query(
            "INSERT INTO fleets (\
                id, user_id, name, description, max_parallel, created_at, updated_at\
            ) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&p.user_id)
        .bind(&p.name)
        .bind(&p.description)
        .bind(p.max_parallel)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(FleetRow {
            id,
            user_id: p.user_id,
            name: p.name,
            description: p.description,
            max_parallel: p.max_parallel,
            created_at: now,
            updated_at: now,
        })
    }

    async fn list_fleets(&self, user_id: &str) -> Result<Vec<FleetRow>, sqlx::Error> {
        let rows = sqlx::query_as::<_, FleetRow>(
            "SELECT * FROM fleets WHERE user_id = ? ORDER BY created_at DESC",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn get_fleet(&self, id: &str) -> Result<Option<FleetRow>, sqlx::Error> {
        let row = sqlx::query_as::<_, FleetRow>("SELECT * FROM fleets WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    async fn update_fleet(&self, id: &str, p: UpdateFleetParams) -> Result<(), sqlx::Error> {
        // Build the SET clause conservatively: only touch columns the caller
        // actually supplied. `None` = skip, `Some(None)` = set NULL,
        // `Some(Some(v))` = set v. When nothing changes, return early.
        let mut sets: Vec<&str> = Vec::new();
        if p.name.is_some() {
            sets.push("name = ?");
        }
        if p.description.is_some() {
            sets.push("description = ?");
        }
        if p.max_parallel.is_some() {
            sets.push("max_parallel = ?");
        }
        if sets.is_empty() {
            return Ok(());
        }
        sets.push("updated_at = ?");
        let sql = format!("UPDATE fleets SET {} WHERE id = ?", sets.join(", "));

        let mut q = sqlx::query(&sql);
        if let Some(name) = &p.name {
            q = q.bind(name);
        }
        if let Some(description) = &p.description {
            q = q.bind(description);
        }
        if let Some(max_parallel) = &p.max_parallel {
            q = q.bind(max_parallel);
        }
        q = q.bind(now_ms());
        q = q.bind(id);
        q.execute(&self.pool).await?;
        Ok(())
    }

    async fn delete_fleet(&self, id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM fleets WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn list_members(&self, fleet_id: &str) -> Result<Vec<FleetMemberRow>, sqlx::Error> {
        let rows = sqlx::query_as::<_, FleetMemberRow>(
            "SELECT * FROM fleet_members WHERE fleet_id = ? ORDER BY sort_order ASC",
        )
        .bind(fleet_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn replace_members(
        &self,
        fleet_id: &str,
        members: Vec<NewFleetMember>,
    ) -> Result<(), sqlx::Error> {
        let now = now_ms();
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM fleet_members WHERE fleet_id = ?")
            .bind(fleet_id)
            .execute(&mut *tx)
            .await?;
        for m in members {
            let mid = generate_prefixed_id("fmem");
            sqlx::query(
                "INSERT INTO fleet_members (\
                    id, fleet_id, agent_id, provider_id, model, role_hint, \
                    capability_profile, constraints, sort_order, created_at, updated_at\
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&mid)
            .bind(fleet_id)
            .bind(&m.agent_id)
            .bind(&m.provider_id)
            .bind(&m.model)
            .bind(&m.role_hint)
            .bind(&m.capability_profile)
            .bind(&m.constraints)
            .bind(m.sort_order)
            .bind(now)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::init_database_memory;

    #[tokio::test]
    async fn fleet_crud_and_member_replace_roundtrip() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteFleetRepository::new(db.pool().clone());
        let f = repo
            .create_fleet(CreateFleetParams {
                user_id: "u1".into(),
                name: "团队A".into(),
                description: None,
                max_parallel: Some(3),
            })
            .await
            .unwrap();
        assert!(f.id.starts_with("fleet_"));
        repo.replace_members(
            &f.id,
            vec![NewFleetMember {
                agent_id: "agent_builtin_claude".into(),
                provider_id: Some("prov_x".into()),
                model: Some("claude-opus-4-8".into()),
                role_hint: Some("后端".into()),
                capability_profile: Some("{\"strengths\":[\"coding\"]}".into()),
                constraints: None,
                sort_order: 0,
            }],
        )
        .await
        .unwrap();
        let members = repo.list_members(&f.id).await.unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].agent_id, "agent_builtin_claude");
        // 删 fleet → 成员级联删
        repo.delete_fleet(&f.id).await.unwrap();
        assert!(repo.get_fleet(&f.id).await.unwrap().is_none());
        assert_eq!(repo.list_members(&f.id).await.unwrap().len(), 0);
    }
}
