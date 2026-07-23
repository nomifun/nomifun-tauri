use nomifun_common::{ConversationId, RequirementId, TerminalId, TimestampMs, now_ms};
use sqlx::{Sqlite, SqlitePool, Transaction};

use crate::error::DbError;
use crate::models::{NewRequirementRow, RequirementRow, RequirementRowUpdate, RequirementTagRow};
use crate::repository::bind::{BindValue, bind_value, bind_value_as, bind_value_scalar};
use crate::repository::requirement::{IRequirementRepository, ListRequirementsParams};

#[derive(Clone, Debug)]
pub struct SqliteRequirementRepository {
    pool: SqlitePool,
}

impl SqliteRequirementRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

fn parse_requirement_id(requirement_id: &str) -> Result<RequirementId, DbError> {
    RequirementId::parse(requirement_id).map_err(|error| {
        DbError::Conflict(format!(
            "requirement id '{requirement_id}' is not a canonical UUIDv7: {error}"
        ))
    })
}

async fn lock_requirement_owners(
    tx: &mut Transaction<'_, Sqlite>,
    owner_conversation_id: Option<&str>,
    owner_terminal_id: Option<&str>,
) -> Result<(), DbError> {
    if owner_conversation_id.is_some() && owner_terminal_id.is_some() {
        return Err(DbError::Conflict(
            "a requirement cannot have both conversation and terminal owners".into(),
        ));
    }
    if let Some(owner) = owner_conversation_id {
        let owner = ConversationId::parse(owner).map_err(|error| {
            DbError::Conflict(format!(
                "requirement conversation owner '{owner}' is not a canonical UUIDv7: {error}"
            ))
        })?;
        let parent = sqlx::query(
            "UPDATE conversations SET updated_at = updated_at WHERE conversation_id = ?",
        )
        .bind(owner.as_str())
        .execute(&mut **tx)
        .await?;
        if parent.rows_affected() == 0 {
            return Err(DbError::Conflict(format!(
                "requirement conversation owner '{}' does not exist",
                owner
            )));
        }
    }
    if let Some(owner) = owner_terminal_id {
        let owner = TerminalId::parse(owner).map_err(|error| {
            DbError::Conflict(format!(
                "requirement terminal owner '{owner}' is not a canonical UUIDv7: {error}"
            ))
        })?;
        let parent = sqlx::query(
            "UPDATE terminal_sessions SET updated_at = updated_at WHERE terminal_id = ?",
        )
        .bind(owner.as_str())
        .execute(&mut **tx)
        .await?;
        if parent.rows_affected() == 0 {
            return Err(DbError::Conflict(format!(
                "requirement terminal owner '{}' does not exist",
                owner
            )));
        }
    }
    Ok(())
}

/// Build a safe `ORDER BY` clause for the requirements list.
///
/// `order_by` is matched against a hard-coded whitelist of columns — user input
/// only *selects* a fixed column name and is NEVER interpolated into SQL, so a
/// value like `"title; DROP TABLE …"` simply misses the whitelist and falls
/// back to the default queue order. `order` is constrained to `ASC|DESC`
/// (default `DESC` for an explicit sort). For non-unique sort columns the
/// local technical `id <dir>` tiebreaker is appended internally so pagination
/// is deterministic; it is never accepted as a public sort key.
fn build_order_clause(order_by: Option<&str>, order: Option<&str>) -> String {
    const DEFAULT: &str = "ORDER BY sort_seq ASC, priority DESC, created_at ASC";
    let col = match order_by {
        Some("display_no") => "display_no",
        Some("requirement_id") => "requirement_id",
        Some("created_at") => "created_at",
        Some("updated_at") => "updated_at",
        Some("status") => "status",
        // Unknown column or no explicit sort → default queue order.
        _ => return DEFAULT.to_string(),
    };
    let dir = match order.map(str::to_ascii_lowercase).as_deref() {
        Some("asc") => "ASC",
        _ => "DESC",
    };
    if col == "requirement_id" {
        format!("ORDER BY requirement_id {dir}")
    } else {
        format!("ORDER BY {col} {dir}, id {dir}")
    }
}

#[async_trait::async_trait]
impl IRequirementRepository for SqliteRequirementRepository {
    async fn insert(&self, row: &NewRequirementRow) -> Result<RequirementRow, DbError> {
        let mut transaction = self.pool.begin().await?;
        let requirement_id = RequirementId::new();
        lock_requirement_owners(
            &mut transaction,
            row.owner_conversation_id.as_deref(),
            row.owner_terminal_id.as_deref(),
        )
        .await?;
        let display_no: i64 = sqlx::query_scalar(
            "UPDATE requirement_display_sequence \
             SET last_no = last_no + 1 \
             WHERE singleton_key = 'requirements' \
             RETURNING last_no",
        )
        .fetch_one(&mut *transaction)
        .await?;

        let inserted = sqlx::query_as::<_, RequirementRow>(
            "INSERT INTO requirements (\
                requirement_id, display_no, title, content, tag, order_key, sort_seq, status, priority, \
                completion_note, owner_conversation_id, owner_terminal_id, active_turn_started_at, lease_expires_at, \
                started_at, completed_at, attempt_count, created_by, extra, created_at, updated_at\
            ) VALUES (\
                ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?\
            ) RETURNING *",
        )
        .bind(requirement_id.as_str())
        .bind(display_no)
        .bind(&row.title)
        .bind(&row.content)
        .bind(&row.tag)
        .bind(&row.order_key)
        .bind(&row.sort_seq)
        .bind(&row.status)
        .bind(row.priority)
        .bind(&row.completion_note)
        .bind(&row.owner_conversation_id)
        .bind(&row.owner_terminal_id)
        .bind(row.active_turn_started_at)
        .bind(row.lease_expires_at)
        .bind(row.started_at)
        .bind(row.completed_at)
        .bind(row.attempt_count)
        .bind(&row.created_by)
        .bind(&row.extra)
        .bind(row.created_at)
        .bind(row.updated_at)
        .fetch_one(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(inserted)
    }

    async fn update(
        &self,
        requirement_id: &str,
        params: &RequirementRowUpdate,
    ) -> Result<(), DbError> {
        let requirement_id = parse_requirement_id(requirement_id)?;
        let mut transaction = self.pool.begin().await?;
        let current: RequirementRow =
            sqlx::query_as("SELECT * FROM requirements WHERE requirement_id = ?")
                .bind(requirement_id.as_str())
                .fetch_optional(&mut *transaction)
                .await?
                .ok_or_else(|| DbError::NotFound(format!("requirement '{requirement_id}'")))?;
        let final_conversation = params
            .owner_conversation_id
            .as_ref()
            .map_or(current.owner_conversation_id.as_deref(), |value| value.as_deref());
        let final_terminal = params
            .owner_terminal_id
            .as_ref()
            .map_or(current.owner_terminal_id.as_deref(), |value| value.as_deref());
        lock_requirement_owners(&mut transaction, final_conversation, final_terminal).await?;

        let mut set_parts: Vec<String> = Vec::new();
        let mut binds: Vec<BindValue> = Vec::new();

        macro_rules! push_str {
            ($field:ident) => {
                if let Some(ref v) = params.$field {
                    set_parts.push(concat!(stringify!($field), " = ?").to_string());
                    binds.push(BindValue::Str(v.clone()));
                }
            };
        }
        macro_rules! push_opt_str {
            ($field:ident) => {
                if let Some(ref v) = params.$field {
                    set_parts.push(concat!(stringify!($field), " = ?").to_string());
                    binds.push(BindValue::OptStr(v.clone()));
                }
            };
        }
        macro_rules! push_opt_i64 {
            ($field:ident) => {
                if let Some(ref v) = params.$field {
                    set_parts.push(concat!(stringify!($field), " = ?").to_string());
                    binds.push(BindValue::OptI64(*v));
                }
            };
        }
        macro_rules! push_i64 {
            ($field:ident) => {
                if let Some(v) = params.$field {
                    set_parts.push(concat!(stringify!($field), " = ?").to_string());
                    binds.push(BindValue::I64(v));
                }
            };
        }

        push_str!(title);
        push_str!(content);
        push_str!(tag);
        push_str!(order_key);
        push_str!(sort_seq);
        push_str!(status);
        push_i64!(priority);
        push_opt_str!(completion_note);
        push_opt_str!(owner_conversation_id);
        push_opt_str!(owner_terminal_id);
        push_opt_i64!(active_turn_started_at);
        push_opt_i64!(lease_expires_at);
        push_opt_i64!(started_at);
        push_opt_i64!(completed_at);
        push_i64!(attempt_count);
        push_str!(extra);

        if set_parts.is_empty() {
            return Ok(());
        }

        set_parts.push("updated_at = ?".to_string());
        binds.push(BindValue::I64(now_ms()));

        let sql = format!(
            "UPDATE requirements SET {} WHERE requirement_id = ?",
            set_parts.join(", ")
        );
        let mut query = sqlx::query(&sql);
        for bind in &binds {
            query = bind_value(query, bind);
        }
        query = query.bind(requirement_id.as_str());

        let result = query.execute(&mut *transaction).await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("requirement '{requirement_id}'")));
        }
        transaction.commit().await?;
        Ok(())
    }

    async fn delete(&self, requirement_id: &str) -> Result<(), DbError> {
        let requirement_id = parse_requirement_id(requirement_id)?;
        let mut transaction = self.pool.begin().await?;

        // Acquire SQLite's writer lock before applying application-owned
        // logical-reference policies. No physical FK/trigger participates.
        let locked = sqlx::query(
            "UPDATE requirements SET updated_at = updated_at WHERE requirement_id = ?",
        )
            .bind(requirement_id.as_str())
            .execute(&mut *transaction)
            .await?;
        if locked.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("requirement '{requirement_id}'")));
        }

        // SET_NULL: a paused tag may retain its pause reason/state after the
        // triggering requirement is removed, but it must not retain a dangling
        // stable business ID.
        sqlx::query(
            "UPDATE requirement_tags \
             SET paused_requirement_id = NULL \
             WHERE paused_requirement_id = ?",
        )
        .bind(requirement_id.as_str())
        .execute(&mut *transaction)
        .await?;

        // CASCADE: attachment rows are part of the requirement aggregate.
        // AttachmentStore stages the files before calling this repository so a
        // failed transaction can restore the filesystem side.
        sqlx::query("DELETE FROM attachments WHERE requirement_id = ?")
            .bind(requirement_id.as_str())
            .execute(&mut *transaction)
            .await?;

        sqlx::query("DELETE FROM requirements WHERE requirement_id = ?")
            .bind(requirement_id.as_str())
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(())
    }

    async fn get_by_requirement_id(
        &self,
        requirement_id: &str,
    ) -> Result<Option<RequirementRow>, DbError> {
        let requirement_id = parse_requirement_id(requirement_id)?;
        let row = sqlx::query_as::<_, RequirementRow>(
            "SELECT * FROM requirements WHERE requirement_id = ?",
        )
            .bind(requirement_id.as_str())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    async fn list(&self, params: &ListRequirementsParams) -> Result<(Vec<RequirementRow>, u64), DbError> {
        let mut where_parts: Vec<String> = Vec::new();
        let mut binds: Vec<BindValue> = Vec::new();

        if let Some(tag) = &params.tag {
            where_parts.push("tag = ?".to_string());
            binds.push(BindValue::Str(tag.clone()));
        }
        if let Some(status) = &params.status {
            where_parts.push("status = ?".to_string());
            binds.push(BindValue::Str(status.clone()));
        }
        if let Some(owner) = &params.owner_conversation_id {
            where_parts.push("owner_conversation_id = ?".to_string());
            binds.push(BindValue::Str(owner.clone()));
        }
        if let Some(owner) = &params.owner_terminal_id {
            where_parts.push("owner_terminal_id = ?".to_string());
            binds.push(BindValue::Str(owner.clone()));
        }
        if let Some(q) = &params.q
            && !q.trim().is_empty()
        {
            let q = q.trim();
            if let Some(number) = q.strip_prefix('#').and_then(|value| value.parse::<i64>().ok()) {
                where_parts.push("display_no = ?".to_string());
                binds.push(BindValue::I64(number));
            } else {
                // Escape LIKE metacharacters so a user typing `%` or `_` searches
                // literally rather than as wildcards. `\` is the ESCAPE char.
                let escaped = q.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
                let like = format!("%{escaped}%");
                where_parts
                    .push("(title LIKE ? ESCAPE '\\' OR content LIKE ? ESCAPE '\\')".to_string());
                binds.push(BindValue::Str(like.clone()));
                binds.push(BindValue::Str(like));
            }
        }

        let where_clause = if where_parts.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", where_parts.join(" AND "))
        };

        // total count
        let count_sql = format!("SELECT COUNT(*) FROM requirements{where_clause}");
        let mut count_query = sqlx::query_scalar::<_, i64>(&count_sql);
        for bind in &binds {
            count_query = bind_value_scalar(count_query, bind);
        }
        let total: i64 = count_query.fetch_one(&self.pool).await?;

        // page
        let page = params.page.unwrap_or(1).max(1);
        let page_size = params.page_size.unwrap_or(20).clamp(1, 200);
        let offset = (page - 1) * page_size;

        let page_sql = format!(
            "SELECT * FROM requirements{where_clause} {order_clause} LIMIT ? OFFSET ?",
            order_clause = build_order_clause(params.order_by.as_deref(), params.order.as_deref())
        );
        let mut page_query = sqlx::query_as::<_, RequirementRow>(&page_sql);
        for bind in &binds {
            page_query = bind_value_as(page_query, bind);
        }
        page_query = page_query.bind(page_size as i64).bind(offset as i64);
        let rows = page_query.fetch_all(&self.pool).await?;

        Ok((rows, total as u64))
    }

    async fn list_by_tag(&self, tag: &str) -> Result<Vec<RequirementRow>, DbError> {
        let rows = sqlx::query_as::<_, RequirementRow>(
            "SELECT * FROM requirements WHERE tag = ? \
             ORDER BY sort_seq ASC, priority DESC, created_at ASC",
        )
        .bind(tag)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn tag_status_counts(&self) -> Result<Vec<(String, String, i64)>, DbError> {
        let rows = sqlx::query_as::<_, (String, String, i64)>(
            "SELECT tag, status, COUNT(*) as cnt FROM requirements GROUP BY tag, status ORDER BY tag ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn claim_next(
        &self,
        tag: &str,
        owner_conversation_id: Option<&str>,
        owner_terminal_id: Option<&str>,
        lease_ms: i64,
        now: TimestampMs,
    ) -> Result<Option<RequirementRow>, DbError> {
        let mut transaction = self.pool.begin().await?;
        lock_requirement_owners(
            &mut transaction,
            owner_conversation_id,
            owner_terminal_id,
        )
        .await?;
        let row = sqlx::query_as::<_, RequirementRow>(
            "UPDATE requirements \
             SET status='in_progress', \
                 owner_conversation_id=?1, owner_terminal_id=?2, \
                 active_turn_started_at=?3, started_at=COALESCE(started_at, ?3), \
                 lease_expires_at=?3 + ?4, \
                 attempt_count=attempt_count + 1, \
                 updated_at=?3 \
             WHERE id = ( \
                 SELECT id FROM requirements \
                 WHERE tag = ?5 AND status = 'pending' \
                   AND NOT EXISTS ( \
                       SELECT 1 FROM requirement_tags t WHERE t.tag = ?5 AND t.paused = 1 \
                   ) \
                 ORDER BY sort_seq ASC, priority DESC, created_at ASC \
                 LIMIT 1 \
             ) \
             RETURNING *",
        )
        .bind(owner_conversation_id)
        .bind(owner_terminal_id)
        .bind(now)
        .bind(lease_ms)
        .bind(tag)
        .fetch_optional(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(row)
    }

    async fn renew_lease(
        &self,
        requirement_id: &str,
        owner_conversation_id: Option<&str>,
        owner_terminal_id: Option<&str>,
        lease_ms: i64,
        now: TimestampMs,
    ) -> Result<bool, DbError> {
        let requirement_id = parse_requirement_id(requirement_id)?;
        let result = sqlx::query(
            "UPDATE requirements SET lease_expires_at = ?1 + ?2, updated_at = ?1 \
             WHERE requirement_id = ?3 AND owner_conversation_id IS ?4 \
               AND owner_terminal_id IS ?5 AND status = 'in_progress'",
        )
        .bind(now)
        .bind(lease_ms)
        .bind(requirement_id.as_str())
        .bind(owner_conversation_id)
        .bind(owner_terminal_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn sweep_expired_leases(
        &self,
        active_conversation_ids: &[String],
        active_terminal_ids: &[String],
        now: TimestampMs,
    ) -> Result<u64, DbError> {
        // Exclude active sessions independently by their typed canonical owner columns. A conversation ID can never protect a terminal-owned claim.
        let mut active_terms = Vec::new();
        if !active_conversation_ids.is_empty() {
            let placeholders = std::iter::repeat_n("?", active_conversation_ids.len())
                .collect::<Vec<_>>()
                .join(", ");
            active_terms.push(format!(
                "(owner_conversation_id IS NULL OR owner_conversation_id NOT IN ({placeholders}))"
            ));
        }
        if !active_terminal_ids.is_empty() {
            let placeholders = std::iter::repeat_n("?", active_terminal_ids.len())
                .collect::<Vec<_>>()
                .join(", ");
            active_terms.push(format!(
                "(owner_terminal_id IS NULL OR owner_terminal_id NOT IN ({placeholders}))"
            ));
        }
        let active_clause = (!active_terms.is_empty())
            .then(|| format!(" AND {}", active_terms.join(" AND ")))
            .unwrap_or_default();

        let sql = format!(
            "UPDATE requirements \
             SET status='pending', owner_conversation_id=NULL, owner_terminal_id=NULL, \
                 active_turn_started_at=NULL, lease_expires_at=NULL, updated_at=? \
             WHERE status='in_progress' \
               AND lease_expires_at IS NOT NULL \
               AND lease_expires_at < ?{active_clause}"
        );

        let mut query = sqlx::query(&sql).bind(now).bind(now);
        for id in active_conversation_ids {
            query = query.bind(id);
        }
        for id in active_terminal_ids {
            query = query.bind(id);
        }
        let result = query.execute(&self.pool).await?;
        Ok(result.rows_affected())
    }

    // ── AutoWork tag-level pause (Step 1) ───────────────────────────────

    async fn pause_tag(
        &self,
        tag: &str,
        reason: &str,
        requirement_id: Option<&str>,
        now: TimestampMs,
    ) -> Result<(), DbError> {
        let requirement_id = requirement_id
            .map(parse_requirement_id)
            .transpose()?;
        sqlx::query(
            "INSERT INTO requirement_tags (tag, paused, paused_reason, paused_requirement_id, paused_at) \
             VALUES (?1, 1, ?2, ?3, ?4) \
             ON CONFLICT(tag) DO UPDATE SET \
                 paused = 1, paused_reason = ?2, paused_requirement_id = ?3, paused_at = ?4",
        )
        .bind(tag)
        .bind(reason)
        .bind(requirement_id.as_ref().map(RequirementId::as_str))
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn resume_tag(&self, tag: &str) -> Result<(), DbError> {
        sqlx::query("UPDATE requirement_tags SET paused = 0 WHERE tag = ?1")
            .bind(tag)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn is_tag_paused(&self, tag: &str) -> Result<bool, DbError> {
        let paused: Option<i64> = sqlx::query_scalar("SELECT paused FROM requirement_tags WHERE tag = ?1")
            .bind(tag)
            .fetch_optional(&self.pool)
            .await?;
        Ok(paused.unwrap_or(0) != 0)
    }

    async fn get_tag_state(&self, tag: &str) -> Result<Option<RequirementTagRow>, DbError> {
        let row = sqlx::query_as::<_, RequirementTagRow>("SELECT * FROM requirement_tags WHERE tag = ?1")
            .bind(tag)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    async fn unclaim(
        &self,
        requirement_id: &str,
        owner_conversation_id: Option<&str>,
        owner_terminal_id: Option<&str>,
    ) -> Result<bool, DbError> {
        let requirement_id = parse_requirement_id(requirement_id)?;
        let result = sqlx::query(
            "UPDATE requirements \
             SET status='pending', owner_conversation_id=NULL, owner_terminal_id=NULL, \
                 active_turn_started_at=NULL, lease_expires_at=NULL, \
                 attempt_count = MAX(attempt_count - 1, 0), \
                 updated_at=?1 \
             WHERE requirement_id=?2 AND status='in_progress' \
               AND owner_conversation_id IS ?3 AND owner_terminal_id IS ?4",
        )
        .bind(now_ms())
        .bind(requirement_id.as_str())
        .bind(owner_conversation_id)
        .bind(owner_terminal_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;
    use nomifun_common::{ConversationId, TerminalId};

    async fn setup() -> (
        SqliteRequirementRepository,
        crate::Database,
        String,
        String,
    ) {
        let db = init_database_memory().await.expect("init db");
        let installation_owner = crate::installation_owner_id(db.pool()).await.unwrap();
        let repo = SqliteRequirementRepository::new(db.pool().clone());
        let conversation_id = ConversationId::new().into_string();
        let terminal_id = TerminalId::new().into_string();

        sqlx::query(
            "INSERT INTO conversations \
                (conversation_id, user_id, name, type, created_at, updated_at) \
             VALUES (?1, ?2, 'requirement-owner-conversation', 'nomi', 0, 0)",
        )
        .bind(&conversation_id)
        .bind(&installation_owner)
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO terminal_sessions \
                (terminal_id, name, cwd, command, args, created_at, updated_at, user_id) \
             VALUES (?1, 'requirement-owner-terminal', '/tmp', '$SHELL', '[]', 0, 0, ?2)",
        )
        .bind(&terminal_id)
        .bind(&installation_owner)
        .execute(db.pool())
        .await
        .unwrap();

        (repo, db, conversation_id, terminal_id)
    }

    fn make_row(tag: &str, sort_seq: &str) -> NewRequirementRow {
        let now = now_ms();
        NewRequirementRow {
            title: format!("Req {tag}/{sort_seq}"),
            content: "do the thing".into(),
            tag: tag.into(),
            order_key: sort_seq.into(),
            sort_seq: sort_seq.into(),
            status: "pending".into(),
            priority: 0,
            completion_note: None,
            owner_conversation_id: None,
            owner_terminal_id: None,
            active_turn_started_at: None,
            lease_expires_at: None,
            started_at: None,
            completed_at: None,
            attempt_count: 0,
            created_by: "user".into(),
            extra: "{}".into(),
            created_at: now,
            updated_at: now,
        }
    }

    #[tokio::test]
    async fn insert_update_get_delete_use_business_ids() {
        let (repo, _db, _conversation_id, _terminal_id) = setup().await;
        let row = make_row("t", "00000001");
        let inserted = repo.insert(&row).await.unwrap();
        assert!(inserted.id > 0, "every row keeps an internal AUTOINCREMENT id");
        assert!(RequirementId::parse(&inserted.requirement_id).is_ok());
        assert_eq!(inserted.display_no, 1);
        let requirement_id = inserted.requirement_id.clone();

        repo.update(
            &requirement_id,
            &RequirementRowUpdate {
                title: Some("updated".into()),
                status: Some("done".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let found = repo
            .get_by_requirement_id(&requirement_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.title, "updated");
        assert_eq!(found.status, "done");

        repo.delete(&requirement_id).await.unwrap();
        assert!(
            repo.get_by_requirement_id(&requirement_id)
                .await
                .unwrap()
                .is_none()
        );

        let missing = RequirementId::new().into_string();
        assert!(matches!(
            repo.update(
                &missing,
                &RequirementRowUpdate {
                    title: Some("missing".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err(),
            DbError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn list_filters_paginates_and_sorts_without_interpolating_input() {
        let (repo, _db, _conversation_id, _terminal_id) = setup().await;
        let low = repo.insert(&make_row("alpha", "00000001")).await.unwrap();
        let high = repo.insert(&make_row("alpha", "00000002")).await.unwrap();
        repo.insert(&make_row("beta", "00000001")).await.unwrap();

        let (rows, total) = repo
            .list(&ListRequirementsParams {
                tag: Some("alpha".into()),
                page_size: Some(1),
                ..Default::default()
            })
            .await
        .unwrap();
        assert_eq!(total, 2);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].requirement_id, low.requirement_id);

        let (rows, total) = repo
            .list(&ListRequirementsParams {
                order_by: Some("title; DROP TABLE requirements".into()),
                order: Some("asc".into()),
                page_size: Some(100),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(total, 3);
        assert_eq!(rows.len(), 3);
        assert!(
            repo.get_by_requirement_id(&low.requirement_id)
                .await
                .unwrap()
                .is_some()
        );

        let (rows, total) = repo
            .list(&ListRequirementsParams {
                q: Some(format!("#{}", high.display_no)),
                ..Default::default()
            })
            .await
        .unwrap();
        assert_eq!(total, 1);
        assert_eq!(rows[0].requirement_id, high.requirement_id);
    }

    #[tokio::test]
    async fn display_numbers_are_monotonic_non_reusable_and_not_exposed_as_update_fields() {
        let (repo, _db, _conversation_id, _terminal_id) = setup().await;
        let first = repo.insert(&make_row("alpha", "1")).await.unwrap();
        let second = repo.insert(&make_row("alpha", "2")).await.unwrap();
        assert_eq!((first.display_no, second.display_no), (1, 2));
        assert!(second.id > first.id, "technical row ids remain monotonic");

        repo.delete(&second.requirement_id).await.unwrap();
        let third = repo.insert(&make_row("alpha", "3")).await.unwrap();
        assert_eq!(third.display_no, 3, "deleted display numbers must never be reused");

        repo.update(
            &first.requirement_id,
            &RequirementRowUpdate {
                title: Some("still #1".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let refreshed = repo
            .get_by_requirement_id(&first.requirement_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(refreshed.display_no, 1);
    }

    #[tokio::test]
    async fn claim_and_lease_guards_are_domain_typed() {
        let (repo, _db, conversation_id, terminal_id) = setup().await;
        let conversation_req = repo.insert(&make_row("conv", "1")).await.unwrap();
        let terminal_req = repo.insert(&make_row("term", "1")).await.unwrap();

        let conversation_claim = repo
            .claim_next("conv", Some(&conversation_id), None, 60_000, now_ms())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            conversation_claim.requirement_id,
            conversation_req.requirement_id
        );
        assert_eq!(
            conversation_claim.owner_conversation_id.as_deref(),
            Some(conversation_id.as_str())
        );
        assert!(conversation_claim.owner_terminal_id.is_none());
        assert_eq!(conversation_claim.attempt_count, 1);

        assert!(
            !repo
                .renew_lease(
                    &conversation_req.requirement_id,
                    None,
                    Some(&terminal_id),
                    60_000,
                    now_ms(),
                )
                .await
                .unwrap(),
            "a terminal identity must not renew a conversation-owned claim"
        );
        assert!(
            repo.renew_lease(
                &conversation_req.requirement_id,
                Some(&conversation_id),
                None,
                60_000,
                now_ms(),
            )
            .await
            .unwrap()
        );

        let terminal_claim = repo
            .claim_next("term", None, Some(&terminal_id), 60_000, now_ms())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(terminal_claim.requirement_id, terminal_req.requirement_id);
        assert!(terminal_claim.owner_conversation_id.is_none());
        assert_eq!(
            terminal_claim.owner_terminal_id.as_deref(),
            Some(terminal_id.as_str())
        );

        assert!(
            !repo
                .unclaim(
                    &terminal_req.requirement_id,
                    Some(&conversation_id),
                    None,
                )
                .await
                .unwrap(),
            "a conversation identity must not unclaim terminal-owned work"
        );
        assert!(
            repo.unclaim(&terminal_req.requirement_id, None, Some(&terminal_id))
                .await
                .unwrap()
        );
        let row = repo
            .get_by_requirement_id(&terminal_req.requirement_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, "pending");
        assert_eq!(row.attempt_count, 0);
        assert!(row.owner_conversation_id.is_none());
        assert!(row.owner_terminal_id.is_none());
    }

    #[tokio::test]
    async fn expired_lease_sweep_respects_separate_owner_domains() {
        let (repo, _db, conversation_id, terminal_id) = setup().await;
        let req_id = repo
            .insert(&make_row("t", "1"))
            .await
            .unwrap()
            .requirement_id;
        let expired_at = now_ms() - 10_000;
        repo.claim_next(
            "t",
            None,
            Some(&terminal_id),
            1,
            expired_at,
        )
        .await
        .unwrap()
        .unwrap();

        let reset = repo
            .sweep_expired_leases(
                std::slice::from_ref(&conversation_id),
                &[],
                expired_at + 10,
            )
            .await
            .unwrap();
        assert_eq!(reset, 1, "an active conversation cannot protect a terminal lease");
        let row = repo
            .get_by_requirement_id(&req_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, "pending");
        assert!(row.owner_conversation_id.is_none());
        assert!(row.owner_terminal_id.is_none());

        repo.claim_next(
            "t",
            None,
            Some(&terminal_id),
            1,
            expired_at,
        )
        .await
        .unwrap()
        .unwrap();
        let reset = repo
            .sweep_expired_leases(
                &[],
                std::slice::from_ref(&terminal_id),
                expired_at + 10,
            )
            .await
            .unwrap();
        assert_eq!(reset, 0, "the matching active terminal retains its lease");
    }

    #[tokio::test]
    async fn pause_resume_blocks_and_restores_claiming() {
        let (repo, _db, conversation_id, _terminal_id) = setup().await;
        let req_id = repo
            .insert(&make_row("paused", "1"))
            .await
            .unwrap()
            .requirement_id;
        repo.pause_tag(
            "paused",
            "requirement_failed",
            Some(&req_id),
            now_ms(),
        )
        .await
        .unwrap();
        assert!(repo.is_tag_paused("paused").await.unwrap());
        assert!(
            repo.claim_next("paused", Some(&conversation_id), None, 60_000, now_ms())
                .await
                .unwrap()
                .is_none()
        );
        let state = repo.get_tag_state("paused").await.unwrap().unwrap();
        assert_eq!(state.paused_requirement_id, Some(req_id));

        repo.resume_tag("paused").await.unwrap();
        assert!(!repo.is_tag_paused("paused").await.unwrap());
        assert!(
            repo.claim_next("paused", Some(&conversation_id), None, 60_000, now_ms())
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn delete_sets_paused_requirement_reference_null_and_cascades_attachments() {
        let (repo, db, _conversation_id, _terminal_id) = setup().await;
        let requirement = repo.insert(&make_row("paused-delete", "1")).await.unwrap();
        repo.pause_tag(
            "paused-delete",
            "requirement_failed",
            Some(&requirement.requirement_id),
            now_ms(),
        )
        .await
        .unwrap();
        let attachment_id = nomifun_common::AttachmentId::new().into_string();
        sqlx::query(
            "INSERT INTO attachments \
             (attachment_id, requirement_id, file_name, rel_path, mime, size_bytes, created_at) \
             VALUES (?, ?, 'proof.png', ?, 'image/png', 1, 1)",
        )
        .bind(&attachment_id)
        .bind(&requirement.requirement_id)
        .bind(format!(
            "attachments/{}/{}.png",
            requirement.requirement_id, attachment_id
        ))
        .execute(db.pool())
        .await
        .unwrap();

        repo.delete(&requirement.requirement_id).await.unwrap();

        let state = repo
            .get_tag_state("paused-delete")
            .await
            .unwrap()
            .unwrap();
        assert!(state.is_paused(), "pause state survives parent deletion");
        assert_eq!(state.paused_requirement_id, None);
        let attachment_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM attachments WHERE requirement_id = ?")
                .bind(&requirement.requirement_id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(attachment_count, 0);
    }

    #[test]
    fn order_clause_is_whitelisted() {
        assert_eq!(
            build_order_clause(Some("display_no"), Some("asc")),
            "ORDER BY display_no ASC, id ASC"
        );
        assert_eq!(
            build_order_clause(Some("requirement_id"), Some("asc")),
            "ORDER BY requirement_id ASC"
        );
        assert_eq!(
            build_order_clause(Some("status"), Some("desc")),
            "ORDER BY status DESC, id DESC"
        );
        assert_eq!(
            build_order_clause(Some("title; DROP TABLE requirements"), Some("asc")),
            "ORDER BY sort_seq ASC, priority DESC, created_at ASC"
        );
    }
}
