use nomifun_common::now_ms;
use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::{SkillTagRow, UpsertSkillTagParams};
use crate::repository::skill_tag::ISkillTagRepository;

/// SQLite-backed implementation of [`ISkillTagRepository`].
#[derive(Clone, Debug)]
pub struct SqliteSkillTagRepository {
    pool: SqlitePool,
}

impl SqliteSkillTagRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl ISkillTagRepository for SqliteSkillTagRepository {
    async fn get_all(&self) -> Result<Vec<SkillTagRow>, DbError> {
        let rows = sqlx::query_as::<_, SkillTagRow>("SELECT * FROM skill_tags")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    async fn upsert(&self, params: &UpsertSkillTagParams<'_>) -> Result<SkillTagRow, DbError> {
        let now = now_ms();
        sqlx::query(
            "INSERT INTO skill_tags (skill_name, audience_tags, scenario_tags, updated_at) \
             VALUES (?, ?, ?, ?) \
             ON CONFLICT(skill_name) DO UPDATE SET \
                audience_tags = excluded.audience_tags, \
                scenario_tags = excluded.scenario_tags, \
                updated_at = excluded.updated_at",
        )
        .bind(params.skill_name)
        .bind(params.audience_tags)
        .bind(params.scenario_tags)
        .bind(now)
        .execute(&self.pool)
        .await?;
        let id: i64 = sqlx::query_scalar("SELECT id FROM skill_tags WHERE skill_name = ?")
            .bind(params.skill_name)
            .fetch_one(&self.pool)
            .await?;
        Ok(SkillTagRow {
            id,
            skill_name: params.skill_name.to_string(),
            audience_tags: params.audience_tags.map(String::from),
            scenario_tags: params.scenario_tags.map(String::from),
            updated_at: now,
        })
    }

    async fn delete(&self, skill_name: &str) -> Result<bool, DbError> {
        let result = sqlx::query("DELETE FROM skill_tags WHERE skill_name = ?")
            .bind(skill_name)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;

    #[tokio::test]
    async fn skill_tag_upsert_get_delete() {
        let db = init_database_memory().await.unwrap();
        let r = SqliteSkillTagRepository::new(db.pool().clone());
        r.upsert(&UpsertSkillTagParams {
            skill_name: "mermaid",
            audience_tags: Some(r#"["developer"]"#),
            scenario_tags: Some(r#"["dataviz"]"#),
        })
        .await
        .unwrap();
        let all = r.get_all().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].skill_name, "mermaid");
        assert_eq!(all[0].audience_tags.as_deref(), Some(r#"["developer"]"#));
        // upsert overwrites
        r.upsert(&UpsertSkillTagParams {
            skill_name: "mermaid",
            audience_tags: Some(r#"["developer","office"]"#),
            scenario_tags: None,
        })
        .await
        .unwrap();
        let all = r.get_all().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].audience_tags.as_deref(), Some(r#"["developer","office"]"#));
        assert!(all[0].scenario_tags.is_none());
        assert!(r.delete("mermaid").await.unwrap());
        assert!(r.get_all().await.unwrap().is_empty());
        assert!(!r.delete("mermaid").await.unwrap());
    }
}
