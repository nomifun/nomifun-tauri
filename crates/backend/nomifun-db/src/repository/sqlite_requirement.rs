use nomifun_common::{ConversationId, RequirementId, TerminalId, TimestampMs, now_ms};
use sqlx::{Sqlite, SqlitePool, Transaction};

use crate::error::DbError;
use crate::models::{NewRequirementRow, RequirementRow, RequirementRowUpdate, RequirementTagRow};
use crate::repository::bind::{BindValue, bind_value, bind_value_as, bind_value_scalar};
use crate::repository::requirement::{
    IRequirementRepository, ListRequirementsParams, RequirementClaim,
    RequirementClaimResolution,
};

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

fn validate_claim_token(claim_token: &str) -> Result<(), DbError> {
    if claim_token.len() != 64
        || !claim_token
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(DbError::Conflict(
            "Requirement claim capability is invalid".into(),
        ));
    }
    Ok(())
}

fn validate_claim_request(
    owner_conversation_id: Option<&str>,
    owner_terminal_id: Option<&str>,
    lease_ms: i64,
) -> Result<(), DbError> {
    if lease_ms <= 0 {
        return Err(DbError::Conflict(
            "Requirement claim lease must be positive".into(),
        ));
    }
    if owner_conversation_id.is_some() == owner_terminal_id.is_some() {
        return Err(DbError::Conflict(
            "Requirement claim requires exactly one typed owner".into(),
        ));
    }
    Ok(())
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

async fn recover_active_claim_in_transaction(
    transaction: &mut Transaction<'_, Sqlite>,
    tag: &str,
    owner_conversation_id: Option<&str>,
    owner_terminal_id: Option<&str>,
    lease_ms: i64,
    now: TimestampMs,
) -> Result<Option<RequirementRow>, DbError> {
    validate_claim_request(owner_conversation_id, owner_terminal_id, lease_ms)?;
    // A process restart re-enters the exact durable claim even when its
    // wall-clock lease has expired. Lease expiry proves only that the owner
    // stopped renewing; it cannot prove that model/tool/PTY side effects never
    // began.
    Ok(sqlx::query_as::<_, RequirementRow>(
        "UPDATE requirements \
         SET lease_expires_at=?1 + ?2, updated_at=?1 \
         WHERE id = ( \
             SELECT id FROM requirements \
             WHERE tag = ?3 AND status = 'in_progress' \
               AND claim_generation > 0 AND claim_token IS NOT NULL \
               AND owner_conversation_id IS ?4 \
               AND owner_terminal_id IS ?5 \
             ORDER BY active_turn_started_at ASC, id ASC \
             LIMIT 1 \
         ) \
         RETURNING *",
    )
    .bind(now)
    .bind(lease_ms)
    .bind(tag)
    .bind(owner_conversation_id)
    .bind(owner_terminal_id)
    .fetch_optional(&mut **transaction)
    .await?)
}

async fn claim_pending_in_transaction(
    transaction: &mut Transaction<'_, Sqlite>,
    tag: &str,
    owner_conversation_id: Option<&str>,
    owner_terminal_id: Option<&str>,
    lease_ms: i64,
    now: TimestampMs,
) -> Result<Option<RequirementRow>, DbError> {
    validate_claim_request(owner_conversation_id, owner_terminal_id, lease_ms)?;
    Ok(sqlx::query_as::<_, RequirementRow>(
        "UPDATE requirements \
         SET status='in_progress', \
             owner_conversation_id=?1, owner_terminal_id=?2, \
             active_turn_started_at=?3, started_at=COALESCE(started_at, ?3), \
             lease_expires_at=?3 + ?4, \
             attempt_count=attempt_count + 1, \
             claim_generation=claim_generation + 1, \
             claim_token=lower(hex(randomblob(32))), \
             updated_at=?3 \
         WHERE id = ( \
             SELECT id FROM requirements \
             WHERE tag = ?5 AND status = 'pending' \
               AND NOT EXISTS ( \
                   SELECT 1 FROM requirements active \
                   WHERE active.status = 'in_progress' \
                     AND active.owner_conversation_id IS ?1 \
                     AND active.owner_terminal_id IS ?2 \
               ) \
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
    .fetch_optional(&mut **transaction)
    .await?)
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
        if params.status.is_some()
            || params.owner_conversation_id.is_some()
            || params.owner_terminal_id.is_some()
            || params.active_turn_started_at.is_some()
            || params.lease_expires_at.is_some()
            || params.started_at.is_some()
            || params.completed_at.is_some()
            || params.attempt_count.is_some()
        {
            return Err(DbError::Conflict(
                "generic Requirement update cannot mutate execution authority fields; use an exact repository transition"
                    .into(),
            ));
        }
        let mut transaction = self.pool.begin().await?;

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

    async fn touch_updated_at(
        &self,
        requirement_id: &str,
        now: TimestampMs,
    ) -> Result<RequirementRow, DbError> {
        let requirement_id = parse_requirement_id(requirement_id)?;
        sqlx::query_as::<_, RequirementRow>(
            "UPDATE requirements SET updated_at=?1 \
             WHERE requirement_id=?2 RETURNING *",
        )
        .bind(now)
        .bind(requirement_id.as_str())
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| DbError::NotFound(format!("requirement '{requirement_id}'")))
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
        let status: String =
            sqlx::query_scalar("SELECT status FROM requirements WHERE requirement_id = ?")
                .bind(requirement_id.as_str())
                .fetch_one(&mut *transaction)
                .await?;
        if status == "in_progress" {
            sqlx::query(
                "UPDATE requirements \
                 SET status='needs_review', lease_expires_at=NULL, \
                     completion_note=COALESCE( \
                         completion_note, \
                         'Deletion was requested while this Requirement could still be executing; it was parked for review and not deleted.' \
                     ), updated_at=?1 \
                 WHERE requirement_id=?2 AND status='in_progress'",
            )
            .bind(now_ms())
            .bind(requirement_id.as_str())
            .execute(&mut *transaction)
            .await?;
            transaction.commit().await?;
            return Err(DbError::Conflict(format!(
                "requirement '{requirement_id}' was active and was parked for review instead of deleted"
            )));
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

    #[cfg(test)]
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
        let row = claim_pending_in_transaction(
            &mut transaction,
            tag,
            owner_conversation_id,
            owner_terminal_id,
            lease_ms,
            now,
        )
        .await?;
        transaction.commit().await?;
        Ok(row)
    }

    async fn claim_next_for_runner(
        &self,
        tag: &str,
        owner_conversation_id: Option<&str>,
        owner_terminal_id: Option<&str>,
        lease_ms: i64,
        now: TimestampMs,
    ) -> Result<Option<RequirementClaim>, DbError> {
        let mut transaction = self.pool.begin().await?;
        lock_requirement_owners(
            &mut transaction,
            owner_conversation_id,
            owner_terminal_id,
        )
        .await?;
        let existing = recover_active_claim_in_transaction(
            &mut transaction,
            tag,
            owner_conversation_id,
            owner_terminal_id,
            lease_ms,
            now,
        )
        .await?;
        if let Some(existing) = existing {
            transaction.commit().await?;
            return Ok(Some(RequirementClaim {
                row: existing,
                recovered_active: true,
            }));
        }
        let row = claim_pending_in_transaction(
            &mut transaction,
            tag,
            owner_conversation_id,
            owner_terminal_id,
            lease_ms,
            now,
        )
        .await?;
        transaction.commit().await?;
        Ok(row.map(|row| RequirementClaim {
            row,
            recovered_active: false,
        }))
    }

    async fn recover_active_claim_for_runner(
        &self,
        tag: &str,
        owner_conversation_id: Option<&str>,
        owner_terminal_id: Option<&str>,
        lease_ms: i64,
        now: TimestampMs,
    ) -> Result<Option<RequirementClaim>, DbError> {
        let mut transaction = self.pool.begin().await?;
        lock_requirement_owners(
            &mut transaction,
            owner_conversation_id,
            owner_terminal_id,
        )
        .await?;
        let row = recover_active_claim_in_transaction(
            &mut transaction,
            tag,
            owner_conversation_id,
            owner_terminal_id,
            lease_ms,
            now,
        )
        .await?;
        transaction.commit().await?;
        Ok(row.map(|row| RequirementClaim {
            row,
            recovered_active: true,
        }))
    }

    async fn renew_lease(
        &self,
        requirement_id: &str,
        owner_conversation_id: Option<&str>,
        owner_terminal_id: Option<&str>,
        expected_generation: i64,
        expected_claim_token: &str,
        lease_ms: i64,
        now: TimestampMs,
    ) -> Result<bool, DbError> {
        let requirement_id = parse_requirement_id(requirement_id)?;
        validate_claim_token(expected_claim_token)?;
        validate_claim_request(owner_conversation_id, owner_terminal_id, lease_ms)?;
        let result = sqlx::query(
            "UPDATE requirements SET lease_expires_at = ?1 + ?2, updated_at = ?1 \
             WHERE requirement_id = ?3 AND owner_conversation_id IS ?4 \
               AND owner_terminal_id IS ?5 AND status = 'in_progress' \
               AND claim_generation = ?6 AND claim_token = ?7",
        )
        .bind(now)
        .bind(lease_ms)
        .bind(requirement_id.as_str())
        .bind(owner_conversation_id)
        .bind(owner_terminal_id)
        .bind(expected_generation)
        .bind(expected_claim_token)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn resolve_claim_exact(
        &self,
        requirement_id: &str,
        expected_generation: i64,
        expected_claim_token: &str,
        owner_conversation_id: Option<&str>,
        owner_terminal_id: Option<&str>,
        resolution: &RequirementClaimResolution,
        now: TimestampMs,
    ) -> Result<Option<RequirementRow>, DbError> {
        let requirement_id = parse_requirement_id(requirement_id)?;
        if expected_generation <= 0 {
            return Err(DbError::Conflict(format!(
                "requirement claim generation must be positive, got {expected_generation}"
            )));
        }
        validate_claim_token(expected_claim_token)?;
        match (owner_conversation_id, owner_terminal_id) {
            (Some(owner), None) => {
                ConversationId::parse(owner).map_err(|error| {
                    DbError::Conflict(format!(
                        "requirement claim has invalid conversation owner: {error}"
                    ))
                })?;
            }
            (None, Some(owner)) => {
                TerminalId::parse(owner).map_err(|error| {
                    DbError::Conflict(format!(
                        "requirement claim has invalid terminal owner: {error}"
                    ))
                })?;
            }
            _ => {
                return Err(DbError::Conflict(
                    "exact requirement claim resolution requires exactly one typed owner".into(),
                ));
            }
        }

        // Every branch retains the same authority predicate.  In particular,
        // an already parked `needs_review` row and a newer claim generation are
        // both absorbing: neither can be reopened by a late runner result.
        let updated = match resolution {
            RequirementClaimResolution::Done { completion_note } => {
                sqlx::query_as::<_, RequirementRow>(
                    "UPDATE requirements \
                     SET status='done', completion_note=?1, completed_at=?2, updated_at=?2 \
                     WHERE requirement_id=?3 AND status='in_progress' \
                       AND claim_generation=?4 \
                       AND owner_conversation_id IS ?5 AND owner_terminal_id IS ?6 \
                       AND claim_token=?7 \
                     RETURNING *",
                )
                .bind(completion_note)
                .bind(now)
                .bind(requirement_id.as_str())
                .bind(expected_generation)
                .bind(owner_conversation_id)
                .bind(owner_terminal_id)
                .bind(expected_claim_token)
                .fetch_optional(&self.pool)
                .await?
            }
            RequirementClaimResolution::NeedsReview { completion_note } => {
                sqlx::query_as::<_, RequirementRow>(
                    "UPDATE requirements \
                     SET status='needs_review', completion_note=?1, \
                         lease_expires_at=NULL, updated_at=?2 \
                     WHERE requirement_id=?3 AND status='in_progress' \
                       AND claim_generation=?4 \
                       AND owner_conversation_id IS ?5 AND owner_terminal_id IS ?6 \
                       AND claim_token=?7 \
                     RETURNING *",
                )
                .bind(completion_note)
                .bind(now)
                .bind(requirement_id.as_str())
                .bind(expected_generation)
                .bind(owner_conversation_id)
                .bind(owner_terminal_id)
                .bind(expected_claim_token)
                .fetch_optional(&self.pool)
                .await?
            }
            RequirementClaimResolution::Failed { completion_note } => {
                sqlx::query_as::<_, RequirementRow>(
                    "UPDATE requirements \
                     SET status='failed', completion_note=?1, updated_at=?2 \
                     WHERE requirement_id=?3 AND status='in_progress' \
                       AND claim_generation=?4 \
                       AND owner_conversation_id IS ?5 AND owner_terminal_id IS ?6 \
                       AND claim_token=?7 \
                     RETURNING *",
                )
                .bind(completion_note)
                .bind(now)
                .bind(requirement_id.as_str())
                .bind(expected_generation)
                .bind(owner_conversation_id)
                .bind(owner_terminal_id)
                .bind(expected_claim_token)
                .fetch_optional(&self.pool)
                .await?
            }
            RequirementClaimResolution::Cancelled { completion_note } => {
                sqlx::query_as::<_, RequirementRow>(
                    "UPDATE requirements \
                     SET status='cancelled', completion_note=?1, updated_at=?2 \
                     WHERE requirement_id=?3 AND status='in_progress' \
                       AND claim_generation=?4 \
                       AND owner_conversation_id IS ?5 AND owner_terminal_id IS ?6 \
                       AND claim_token=?7 \
                     RETURNING *",
                )
                .bind(completion_note)
                .bind(now)
                .bind(requirement_id.as_str())
                .bind(expected_generation)
                .bind(owner_conversation_id)
                .bind(owner_terminal_id)
                .bind(expected_claim_token)
                .fetch_optional(&self.pool)
                .await?
            }
        };
        Ok(updated)
    }

    async fn transition_status_if_current(
        &self,
        requirement_id: &str,
        expected_status: &str,
        next_status: &str,
        write_completion_note: bool,
        completion_note: Option<&str>,
        initialize_started_at: bool,
        set_completed_at: bool,
        now: TimestampMs,
    ) -> Result<Option<RequirementRow>, DbError> {
        let requirement_id = parse_requirement_id(requirement_id)?;
        let valid_status = |status: &str| {
            matches!(
                status,
                "pending"
                    | "in_progress"
                    | "done"
                    | "failed"
                    | "cancelled"
                    | "needs_review"
            )
        };
        if !valid_status(expected_status) || !valid_status(next_status) {
            return Err(DbError::Conflict(
                "requirement status transition contains an invalid status".into(),
            ));
        }
        if expected_status == "in_progress"
            || next_status == "in_progress"
            || next_status == "pending"
        {
            return Err(DbError::Conflict(
                "generic Requirement status transitions cannot enter/leave active execution or requeue work"
                    .into(),
            ));
        }
        Ok(sqlx::query_as::<_, RequirementRow>(
            "UPDATE requirements \
             SET status=?1, \
                 completion_note=CASE WHEN ?2 THEN ?3 ELSE completion_note END, \
                 started_at=CASE WHEN ?4 THEN COALESCE(started_at, ?6) ELSE started_at END, \
                 completed_at=CASE WHEN ?5 THEN ?6 ELSE completed_at END, \
                 updated_at=?6 \
             WHERE requirement_id=?7 AND status=?8 \
               AND status NOT IN ('done', 'failed', 'cancelled') \
             RETURNING *",
        )
        .bind(next_status)
        .bind(write_completion_note)
        .bind(completion_note)
        .bind(initialize_started_at)
        .bind(set_completed_at)
        .bind(now)
        .bind(requirement_id.as_str())
        .bind(expected_status)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn requeue_for_resume_exact(
        &self,
        requirement_id: &str,
        expected_status: &str,
        expected_generation: i64,
        reset_attempts: bool,
        now: TimestampMs,
    ) -> Result<Option<RequirementRow>, DbError> {
        let requirement_id = parse_requirement_id(requirement_id)?;
        if !matches!(expected_status, "failed" | "needs_review") || expected_generation < 0 {
            return Err(DbError::Conflict(
                "invalid exact Requirement resume state".into(),
            ));
        }
        Ok(sqlx::query_as::<_, RequirementRow>(
            "UPDATE requirements \
             SET status='pending', completion_note=NULL, \
                 owner_conversation_id=NULL, owner_terminal_id=NULL, \
                 active_turn_started_at=NULL, lease_expires_at=NULL, \
                 claim_token=NULL, \
                 attempt_count=CASE WHEN ?1 THEN 0 ELSE attempt_count END, \
                 updated_at=?2 \
             WHERE requirement_id=?3 AND status=?4 AND claim_generation=?5 \
             RETURNING *",
        )
        .bind(reset_attempts)
        .bind(now)
        .bind(requirement_id.as_str())
        .bind(expected_status)
        .bind(expected_generation)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn detach_owner_exact(
        &self,
        requirement_id: &str,
        expected_status: &str,
        expected_generation: i64,
        expected_claim_token: Option<&str>,
        owner_conversation_id: Option<&str>,
        owner_terminal_id: Option<&str>,
        review_note: Option<&str>,
        now: TimestampMs,
    ) -> Result<Option<RequirementRow>, DbError> {
        let requirement_id = parse_requirement_id(requirement_id)?;
        if matches!(expected_status, "in_progress" | "needs_review") {
            let token = expected_claim_token.ok_or_else(|| {
                DbError::Conflict(
                    "active or parked Requirement owner detach requires the exact claim capability"
                        .into(),
                )
            })?;
            validate_claim_token(token)?;
        }
        match (owner_conversation_id, owner_terminal_id) {
            (Some(owner), None) => {
                ConversationId::parse(owner).map_err(|error| {
                    DbError::Conflict(format!("invalid conversation owner detach: {error}"))
                })?;
            }
            (None, Some(owner)) => {
                TerminalId::parse(owner).map_err(|error| {
                    DbError::Conflict(format!("invalid terminal owner detach: {error}"))
                })?;
            }
            _ => {
                return Err(DbError::Conflict(
                    "owner detach requires exactly one typed owner".into(),
                ));
            }
        }
        Ok(sqlx::query_as::<_, RequirementRow>(
            "UPDATE requirements \
             SET status=CASE WHEN status='in_progress' THEN 'needs_review' ELSE status END, \
                 completion_note=CASE WHEN status='in_progress' \
                     THEN COALESCE(completion_note, ?1) ELSE completion_note END, \
                 lease_expires_at=CASE WHEN status='in_progress' \
                     THEN NULL ELSE lease_expires_at END, \
                 owner_conversation_id=CASE \
                     WHEN status IN ('in_progress', 'needs_review') \
                     THEN owner_conversation_id ELSE NULL END, \
                 owner_terminal_id=CASE \
                     WHEN status IN ('in_progress', 'needs_review') \
                     THEN owner_terminal_id ELSE NULL END, \
                 updated_at=?2 \
             WHERE requirement_id=?3 AND status=?4 AND claim_generation=?5 \
               AND claim_token IS ?6 \
               AND owner_conversation_id IS ?7 AND owner_terminal_id IS ?8 \
             RETURNING *",
        )
        .bind(review_note)
        .bind(now)
        .bind(requirement_id.as_str())
        .bind(expected_status)
        .bind(expected_generation)
        .bind(expected_claim_token)
        .bind(owner_conversation_id)
        .bind(owner_terminal_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn detach_owner_for_session(
        &self,
        owner_conversation_id: Option<&str>,
        owner_terminal_id: Option<&str>,
        review_note: &str,
        now: TimestampMs,
    ) -> Result<Vec<RequirementRow>, DbError> {
        match (owner_conversation_id, owner_terminal_id) {
            (Some(owner), None) => {
                ConversationId::parse(owner).map_err(|error| {
                    DbError::Conflict(format!("invalid conversation owner detach: {error}"))
                })?;
            }
            (None, Some(owner)) => {
                TerminalId::parse(owner).map_err(|error| {
                    DbError::Conflict(format!("invalid terminal owner detach: {error}"))
                })?;
            }
            _ => {
                return Err(DbError::Conflict(
                    "session owner detach requires exactly one typed owner".into(),
                ));
            }
        }
        Ok(sqlx::query_as::<_, RequirementRow>(
            "UPDATE requirements \
             SET status=CASE WHEN status='in_progress' THEN 'needs_review' ELSE status END, \
                 completion_note=CASE WHEN status='in_progress' \
                     THEN COALESCE(completion_note, ?1) ELSE completion_note END, \
                 lease_expires_at=CASE WHEN status='in_progress' \
                     THEN NULL ELSE lease_expires_at END, \
                 owner_conversation_id=CASE \
                     WHEN status IN ('in_progress', 'needs_review') \
                     THEN owner_conversation_id ELSE NULL END, \
                 owner_terminal_id=CASE \
                     WHEN status IN ('in_progress', 'needs_review') \
                     THEN owner_terminal_id ELSE NULL END, \
                 updated_at=?2 \
             WHERE owner_conversation_id IS ?3 AND owner_terminal_id IS ?4 \
             RETURNING *",
        )
        .bind(review_note)
        .bind(now)
        .bind(owner_conversation_id)
        .bind(owner_terminal_id)
        .fetch_all(&self.pool)
        .await?)
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
             SET status='needs_review', lease_expires_at=NULL, \
                 completion_note=COALESCE(completion_note, \
                     'AutoWork claim lease expired while execution state was ambiguous; it was not executed again.'), \
                 updated_at=? \
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

    async fn resume_tag_with_requeues(
        &self,
        tag: &str,
        requirement_ids: &[String],
        now: TimestampMs,
    ) -> Result<Vec<RequirementRow>, DbError> {
        let ids = requirement_ids
            .iter()
            .map(|id| parse_requirement_id(id))
            .collect::<Result<Vec<_>, _>>()?;
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO requirement_tags(tag, paused) VALUES(?1, 0) \
             ON CONFLICT(tag) DO UPDATE SET paused=paused",
        )
        .bind(tag)
        .execute(&mut *tx)
        .await?;
        let mut updated = Vec::new();
        for id in ids {
            if let Some(row) = sqlx::query_as::<_, RequirementRow>(
                "UPDATE requirements \
                 SET status='pending', completion_note=NULL, \
                 owner_conversation_id=NULL, owner_terminal_id=NULL, \
                 active_turn_started_at=NULL, lease_expires_at=NULL, \
                 claim_token=NULL, \
                     attempt_count=0, updated_at=?1 \
                 WHERE requirement_id=?2 AND tag=?3 \
                   AND status IN ('failed', 'needs_review') \
                 RETURNING *",
            )
            .bind(now)
            .bind(id.as_str())
            .bind(tag)
            .fetch_optional(&mut *tx)
            .await?
            {
                updated.push(row);
            }
        }
        sqlx::query("UPDATE requirement_tags SET paused=0 WHERE tag=?1")
            .bind(tag)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(updated)
    }

    async fn resume_tag_for_enable_atomic(
        &self,
        tag: &str,
        review_note: &str,
        now: TimestampMs,
    ) -> Result<Vec<RequirementRow>, DbError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO requirement_tags(tag, paused) VALUES(?1, 0) \
             ON CONFLICT(tag) DO UPDATE SET paused=paused",
        )
        .bind(tag)
        .execute(&mut *tx)
        .await?;
        let paused: i64 =
            sqlx::query_scalar("SELECT paused FROM requirement_tags WHERE tag=?1")
                .bind(tag)
                .fetch_one(&mut *tx)
                .await?;
        if paused == 0 {
            tx.commit().await?;
            return Ok(Vec::new());
        }
        let updated = sqlx::query_as::<_, RequirementRow>(
            "UPDATE requirements \
             SET status=CASE WHEN status='in_progress' THEN 'needs_review' ELSE 'pending' END, \
                 completion_note=CASE WHEN status='in_progress' \
                     THEN COALESCE(completion_note, ?1) ELSE NULL END, \
                 owner_conversation_id=CASE WHEN status='in_progress' \
                     THEN owner_conversation_id ELSE NULL END, \
                 owner_terminal_id=CASE WHEN status='in_progress' \
                     THEN owner_terminal_id ELSE NULL END, \
                 active_turn_started_at=CASE WHEN status='in_progress' \
                     THEN active_turn_started_at ELSE NULL END, \
                 lease_expires_at=NULL, \
                 claim_token=CASE WHEN status='in_progress' \
                     THEN claim_token ELSE NULL END, \
                 attempt_count=CASE WHEN status='in_progress' \
                     THEN attempt_count ELSE 0 END, \
                 updated_at=?2 \
             WHERE tag=?3 AND status IN ('failed', 'pending', 'in_progress') \
             RETURNING *",
        )
        .bind(review_note)
        .bind(now)
        .bind(tag)
        .fetch_all(&mut *tx)
        .await?;
        sqlx::query("UPDATE requirement_tags SET paused=0 WHERE tag=?1")
            .bind(tag)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(updated)
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

    async fn abandon_claim_before_admission_exact(
        &self,
        requirement_id: &str,
        owner_conversation_id: Option<&str>,
        owner_terminal_id: Option<&str>,
        expected_generation: i64,
        expected_claim_token: &str,
        now: TimestampMs,
    ) -> Result<Option<RequirementRow>, DbError> {
        let requirement_id = parse_requirement_id(requirement_id)?;
        if expected_generation <= 0 {
            return Err(DbError::Conflict(
                "requirement pre-effect abandon generation must be positive".into(),
            ));
        }
        validate_claim_token(expected_claim_token)?;
        match (owner_conversation_id, owner_terminal_id) {
            (Some(owner), None) => {
                ConversationId::parse(owner).map_err(|error| {
                    DbError::Conflict(format!(
                        "requirement abandon has invalid conversation owner: {error}"
                    ))
                })?;
            }
            (None, Some(owner)) => {
                TerminalId::parse(owner).map_err(|error| {
                    DbError::Conflict(format!(
                        "requirement abandon has invalid terminal owner: {error}"
                    ))
                })?;
            }
            _ => {
                return Err(DbError::Conflict(
                    "requirement pre-effect abandon requires exactly one typed owner".into(),
                ));
            }
        }

        // The INSERT is itself the one-transaction command. Migration 010's
        // insert/update/consume trigger chain independently repeats every
        // predicate, performs the active->pending transition, and deletes the
        // command row before this statement can return.
        let mut transaction = self.pool.begin().await?;
        let inserted = sqlx::query(
            "INSERT INTO requirement_pre_effect_abandon_guards (\
                 requirement_id, claim_generation, claim_token, \
                 owner_conversation_id, owner_terminal_id, created_at\
             ) \
             SELECT requirement.requirement_id, requirement.claim_generation, \
                    requirement.claim_token, requirement.owner_conversation_id, \
                    requirement.owner_terminal_id, ?1 \
               FROM requirements AS requirement \
              WHERE requirement.requirement_id = ?2 \
                AND requirement.status = 'in_progress' \
                AND requirement.claim_generation = ?3 \
                AND requirement.claim_token = ?4 \
                AND requirement.owner_conversation_id IS ?5 \
                AND requirement.owner_terminal_id IS ?6 \
                AND NOT EXISTS (\
                    SELECT 1 FROM conversation_delivery_receipts AS receipt \
                     WHERE json_extract(\
                               receipt.request_payload, \
                               '$.autowork_authority.requirement_id'\
                           ) = requirement.requirement_id \
                       AND json_extract(\
                               receipt.request_payload, \
                               '$.autowork_authority.claim_generation'\
                           ) = requirement.claim_generation\
                ) \
                AND NOT EXISTS (\
                    SELECT 1 FROM conversations AS conversation \
                     WHERE conversation.conversation_id = \
                           requirement.owner_conversation_id \
                       AND (\
                           conversation.status = 'running' \
                           OR conversation.active_turn_operation_id IS NOT NULL\
                       )\
                ) \
                AND NOT EXISTS (\
                    SELECT 1 FROM terminal_turn_admissions AS admission \
                     WHERE admission.requirement_id = requirement.requirement_id \
                       AND admission.claim_generation = requirement.claim_generation\
                )",
        )
        .bind(now)
        .bind(requirement_id.as_str())
        .bind(expected_generation)
        .bind(expected_claim_token)
        .bind(owner_conversation_id)
        .bind(owner_terminal_id)
        .execute(&mut *transaction)
        .await?;
        if inserted.rows_affected() == 0 {
            transaction.commit().await?;
            return Ok(None);
        }

        let updated = sqlx::query_as::<_, RequirementRow>(
            "SELECT * FROM requirements \
              WHERE requirement_id = ?1 AND status = 'pending' \
                AND claim_generation = ?2 AND claim_token IS NULL \
                AND completion_note IS NULL \
                AND owner_conversation_id IS NULL AND owner_terminal_id IS NULL \
                AND active_turn_started_at IS NULL AND lease_expires_at IS NULL",
        )
        .bind(requirement_id.as_str())
        .bind(expected_generation)
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(updated) = updated else {
            transaction.rollback().await?;
            return Err(DbError::Conflict(
                "pre-effect abandon command returned without its exact pending transition".into(),
            ));
        };
        let guard_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM requirement_pre_effect_abandon_guards \
              WHERE requirement_id = ?1",
        )
        .bind(requirement_id.as_str())
        .fetch_one(&mut *transaction)
        .await?;
        if guard_count != 0 {
            transaction.rollback().await?;
            return Err(DbError::Conflict(
                "pre-effect abandon command capability was not consumed".into(),
            ));
        }
        transaction.commit().await?;
        Ok(Some(updated))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        IConversationRepository, RequirementConversationTurnAuthority,
        SqliteConversationRepository, init_database_memory,
    };
    use nomifun_common::{ConversationId, MessageId, TerminalId};
    use sha2::{Digest, Sha256};
    use std::sync::Arc;
    use tokio::sync::Barrier;

    async fn setup_database(
        db: crate::Database,
    ) -> (
        SqliteRequirementRepository,
        crate::Database,
        String,
        String,
    ) {
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

    async fn setup() -> (
        SqliteRequirementRepository,
        crate::Database,
        String,
        String,
    ) {
        setup_database(init_database_memory().await.expect("init db")).await
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

    async fn claim_for_conversation(
        repo: &SqliteRequirementRepository,
        tag: &str,
        conversation_id: &str,
        now: i64,
    ) -> RequirementRow {
        repo.claim_next_for_runner(tag, Some(conversation_id), None, 60_000, now)
            .await
            .unwrap()
            .expect("pending Requirement must be claimable")
            .row
    }

    async fn claim_for_terminal(
        repo: &SqliteRequirementRepository,
        tag: &str,
        terminal_id: &str,
        now: i64,
    ) -> RequirementRow {
        repo.claim_next_for_runner(tag, None, Some(terminal_id), 60_000, now)
            .await
            .unwrap()
            .expect("pending Requirement must be claimable")
            .row
    }

    async fn insert_autowork_receipt(
        db: &crate::Database,
        installation_owner: &str,
        conversation_id: &str,
        requirement: &RequirementRow,
        status: &str,
    ) {
        let operation_id = format!(
            "autowork-test-{}-{}",
            requirement.requirement_id, status
        );
        let message_id = MessageId::new().into_string();
        let request_payload = serde_json::json!({
            "autowork_authority": {
                "requirement_id": requirement.requirement_id,
                "claim_generation": requirement.claim_generation,
                "claim_token_sha256": "receiver-evidence-does-not-expose-the-capability"
            }
        })
        .to_string();
        let (result_ok, completed_at) = if status == "completed" {
            (Some(1_i64), Some(101_i64))
        } else {
            (None, None)
        };
        sqlx::query(
            "INSERT INTO conversation_delivery_receipts (\
                 operation_id, message_id, conversation_id, user_id, kind, \
                 request_payload, status, result_ok, created_at, updated_at, completed_at\
             ) VALUES (?1, ?2, ?3, ?4, 'turn', ?5, ?6, ?7, 100, 101, ?8)",
        )
        .bind(operation_id)
        .bind(message_id)
        .bind(conversation_id)
        .bind(installation_owner)
        .bind(request_payload)
        .bind(status)
        .bind(result_ok)
        .bind(completed_at)
        .execute(db.pool())
        .await
        .unwrap();
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
                ..Default::default()
            },
        )
        .await
        .unwrap();
        repo.transition_status_if_current(
            &requirement_id,
            "pending",
            "done",
            false,
            None,
            false,
            true,
            now_ms(),
        )
        .await
        .unwrap()
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
                    conversation_claim.claim_generation,
                    conversation_claim.claim_token.as_deref().unwrap(),
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
                conversation_claim.claim_generation,
                conversation_claim.claim_token.as_deref().unwrap(),
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
            repo.abandon_claim_before_admission_exact(
                    &terminal_req.requirement_id,
                    Some(&conversation_id),
                    None,
                    terminal_claim.claim_generation,
                    terminal_claim.claim_token.as_deref().unwrap(),
                    now_ms(),
                )
                .await
                .unwrap()
                .is_none(),
            "a conversation identity must not unclaim terminal-owned work"
        );
        assert!(
            repo.abandon_claim_before_admission_exact(
                &terminal_req.requirement_id,
                None,
                Some(&terminal_id),
                terminal_claim.claim_generation,
                terminal_claim.claim_token.as_deref().unwrap(),
                now_ms(),
            )
            .await
            .unwrap()
            .is_some()
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
    async fn one_typed_owner_cannot_hold_two_active_requirements_across_tags() {
        let (repo, db, conversation_id, _terminal_id) = setup().await;
        repo.insert(&make_row("owner-a", "1")).await.unwrap();
        repo.insert(&make_row("owner-b", "1")).await.unwrap();
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(3));
        let first_repo = repo.clone();
        let second_repo = repo.clone();
        let first_owner = conversation_id.clone();
        let second_owner = conversation_id.clone();
        let first_barrier = barrier.clone();
        let second_barrier = barrier.clone();

        let first = tokio::spawn(async move {
            first_barrier.wait().await;
            first_repo
                .claim_next_for_runner("owner-a", Some(&first_owner), None, 60_000, 100)
                .await
        });
        let second = tokio::spawn(async move {
            second_barrier.wait().await;
            second_repo
                .claim_next_for_runner("owner-b", Some(&second_owner), None, 60_000, 100)
                .await
        });
        barrier.wait().await;
        let first = first.await.unwrap().unwrap();
        let second = second.await.unwrap().unwrap();
        assert_ne!(
            first.is_some(),
            second.is_some(),
            "exactly one tag may allocate execution authority to one typed owner"
        );

        let active: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM requirements \
             WHERE status='in_progress' AND owner_conversation_id=?",
        )
        .bind(&conversation_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
        let pending: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM requirements WHERE status='pending'")
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(active, 1);
        assert_eq!(pending, 1);
    }

    #[tokio::test]
    async fn exact_claim_resolution_cannot_reopen_review_or_touch_a_new_generation() {
        let (repo, _db, conversation_id, _terminal_id) = setup().await;
        let requirement = repo.insert(&make_row("exact-finalize", "1")).await.unwrap();
        let first = repo
            .claim_next_for_runner(
                "exact-finalize",
                Some(&conversation_id),
                None,
                60_000,
                100,
            )
            .await
            .unwrap()
            .unwrap()
            .row;

        let parked = repo
            .resolve_claim_exact(
                &requirement.requirement_id,
                first.claim_generation,
                first.claim_token.as_deref().unwrap(),
                Some(&conversation_id),
                None,
                &RequirementClaimResolution::NeedsReview {
                    completion_note: Some("teardown made delivery ambiguous".into()),
                },
                101,
            )
            .await
            .unwrap()
            .expect("current generation should park");
        assert_eq!(parked.status, "needs_review");

        assert!(
            repo.abandon_claim_before_admission_exact(
                    &requirement.requirement_id,
                    Some(&conversation_id),
                    None,
                    first.claim_generation,
                    first.claim_token.as_deref().unwrap(),
                    102,
                )
                .await
                .unwrap()
                .is_none(),
            "a late error must not reopen an absorbing needs_review row"
        );
        let still_parked = repo
            .get_by_requirement_id(&requirement.requirement_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(still_parked.status, "needs_review");
        assert_eq!(still_parked.claim_generation, first.claim_generation);
        assert_eq!(still_parked.claim_token, first.claim_token);
        assert_eq!(
            still_parked.owner_conversation_id.as_deref(),
            Some(conversation_id.as_str())
        );
        assert_eq!(still_parked.owner_terminal_id, None);
        assert_eq!(still_parked.attempt_count, first.attempt_count);

        // Model an explicit human requeue. The next claim receives a fresh
        // generation, which the stale first runner must not be able to settle.
        repo.requeue_for_resume_exact(
            &requirement.requirement_id,
            "needs_review",
            first.claim_generation,
            false,
            150,
        )
        .await
        .unwrap()
        .unwrap();
        let second = repo
            .claim_next_for_runner(
                "exact-finalize",
                Some(&conversation_id),
                None,
                60_000,
                200,
            )
            .await
            .unwrap()
            .unwrap()
            .row;
        assert!(second.claim_generation > first.claim_generation);
        assert!(
            repo.resolve_claim_exact(
                &requirement.requirement_id,
                first.claim_generation,
                first.claim_token.as_deref().unwrap(),
                Some(&conversation_id),
                None,
                &RequirementClaimResolution::Done {
                    completion_note: Some("late old result".into()),
                },
                201,
            )
            .await
            .unwrap()
            .is_none()
        );
        let authoritative = repo
            .get_by_requirement_id(&requirement.requirement_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(authoritative.status, "in_progress");
        assert_eq!(authoritative.claim_generation, second.claim_generation);
    }

    #[tokio::test]
    async fn stale_nonterminal_status_transition_cannot_overwrite_done() {
        let (repo, _db, _conversation_id, _terminal_id) = setup().await;
        let requirement = repo.insert(&make_row("status-cas", "1")).await.unwrap();
        let observed_status = requirement.status.clone();

        let done = repo
            .transition_status_if_current(
                &requirement.requirement_id,
                &observed_status,
                "done",
                true,
                Some("winner"),
                false,
                true,
                100,
            )
            .await
            .unwrap()
            .expect("first verdict wins");
        assert_eq!(done.status, "done");
        let touched = repo
            .touch_updated_at(&requirement.requirement_id, 1000)
            .await
            .unwrap();
        assert_eq!(
            touched.status, "done",
            "attachment-only touch must not replay an observed status"
        );
        assert_eq!(touched.completion_note.as_deref(), Some("winner"));

        assert!(
            repo.transition_status_if_current(
                &requirement.requirement_id,
                &observed_status,
                "needs_review",
                true,
                Some("stale loser"),
                false,
                false,
                101,
            )
            .await
            .unwrap()
            .is_none()
        );
        let authoritative = repo
            .get_by_requirement_id(&requirement.requirement_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(authoritative.status, "done");
        assert_eq!(authoritative.completion_note.as_deref(), Some("winner"));
    }

    #[tokio::test]
    async fn expired_same_owner_runner_recovery_preserves_generation_and_attempt() {
        let (repo, _db, conversation_id, _terminal_id) = setup().await;
        let requirement = repo
            .insert(&make_row("restart-safe", "1"))
            .await
            .unwrap();
        let first_claim_at = now_ms() - 10_000;

        assert!(
            repo.recover_active_claim_for_runner(
                "restart-safe",
                Some(&conversation_id),
                None,
                60_000,
                first_claim_at,
            )
            .await
            .unwrap()
            .is_none(),
            "recovery-only admission must never allocate a pending row"
        );
        let first = repo
            .claim_next_for_runner(
                "restart-safe",
                Some(&conversation_id),
                None,
                1,
                first_claim_at,
            )
            .await
            .unwrap()
            .unwrap();
        assert!(!first.recovered_active);
        assert_eq!(first.row.claim_generation, 1);
        assert_eq!(first.row.attempt_count, 1);
        assert!(
            repo.claim_next(
                "restart-safe",
                Some(&conversation_id),
                None,
                60_000,
                first_claim_at + 2,
            )
            .await
            .unwrap()
            .is_none(),
            "the public pending-only claim API must not replay an active row"
        );

        let recovered_at = first_claim_at + 2;
        let recovered = repo
            .recover_active_claim_for_runner(
                "restart-safe",
                Some(&conversation_id),
                None,
                60_000,
                recovered_at,
            )
            .await
            .unwrap()
            .unwrap();
        assert!(recovered.recovered_active);
        assert_eq!(
            recovered.row.requirement_id, requirement.requirement_id,
            "restart must recover the same durable work identity"
        );
        assert_eq!(recovered.row.claim_generation, first.row.claim_generation);
        assert_eq!(recovered.row.attempt_count, first.row.attempt_count);
        assert_eq!(recovered.row.lease_expires_at, Some(recovered_at + 60_000));
    }

    #[tokio::test]
    async fn expired_lease_sweep_parks_ambiguity_and_respects_owner_domains() {
        let (repo, db, conversation_id, terminal_id) = setup().await;
        let protected_terminal_id = TerminalId::new().into_string();
        let installation_owner = crate::installation_owner_id(db.pool()).await.unwrap();
        sqlx::query(
            "INSERT INTO terminal_sessions \
                (terminal_id, name, cwd, command, args, created_at, updated_at, user_id) \
             VALUES (?1, 'protected-terminal', '/tmp', '$SHELL', '[]', 0, 0, ?2)",
        )
        .bind(&protected_terminal_id)
        .bind(&installation_owner)
        .execute(db.pool())
        .await
        .unwrap();
        let ambiguous_req_id = repo
            .insert(&make_row("t", "1"))
            .await
            .unwrap()
            .requirement_id;
        let protected_req_id = repo
            .insert(&make_row("protected", "2"))
            .await
            .unwrap()
            .requirement_id;
        let expired_at = now_ms() - 10_000;
        let ambiguous_claim = repo
            .claim_next("t", None, Some(&terminal_id), 1, expired_at)
            .await
            .unwrap()
            .unwrap();
        let protected_claim = repo
            .claim_next(
                "protected",
                None,
                Some(&protected_terminal_id),
                1,
                expired_at,
            )
            .await
            .unwrap()
            .unwrap();

        let parked = repo
            .sweep_expired_leases(
                std::slice::from_ref(&conversation_id),
                std::slice::from_ref(&protected_terminal_id),
                expired_at + 10,
            )
            .await
            .unwrap();
        assert_eq!(
            parked, 1,
            "an active conversation cannot protect another terminal's claim"
        );
        let row = repo
            .get_by_requirement_id(&ambiguous_req_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, "needs_review");
        assert!(row.owner_conversation_id.is_none());
        assert_eq!(row.owner_terminal_id.as_deref(), Some(terminal_id.as_str()));
        assert_eq!(row.claim_generation, ambiguous_claim.claim_generation);
        assert_eq!(row.attempt_count, ambiguous_claim.attempt_count);
        assert!(
            row.completion_note
                .as_deref()
                .is_some_and(|note| note.contains("not executed again"))
        );

        let retained = repo
            .sweep_expired_leases(
                &[],
                std::slice::from_ref(&protected_terminal_id),
                expired_at + 10,
            )
            .await
            .unwrap();
        assert_eq!(
            retained, 0,
            "the matching active terminal retains its durable claim"
        );
        let protected = repo
            .get_by_requirement_id(&protected_req_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(protected.status, "in_progress");
        assert_eq!(
            protected.claim_generation,
            protected_claim.claim_generation
        );
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
    async fn repeated_enable_is_a_transactional_noop_after_a_new_claim_starts() {
        let (repo, _db, conversation_id, _terminal_id) = setup().await;
        repo.insert(&make_row("enable-race", "1")).await.unwrap();
        repo.pause_tag("enable-race", "manual", None, 10)
            .await
            .unwrap();
        assert!(repo.is_tag_paused("enable-race").await.unwrap());

        let first_enable = repo
            .resume_tag_for_enable_atomic("enable-race", "ambiguous", 20)
            .await
            .unwrap();
        assert_eq!(first_enable.len(), 1);
        let claim = repo
            .claim_next_for_runner(
                "enable-race",
                Some(&conversation_id),
                None,
                60_000,
                30,
            )
            .await
            .unwrap()
            .unwrap();

        // Model a second enable caller that observed `paused=true` before the
        // first transaction committed but entered its writer transaction only
        // after the runner claimed. The in-transaction paused read must make it
        // a no-op rather than parking the newly-created claim.
        let second_enable = repo
            .resume_tag_for_enable_atomic("enable-race", "stale second enable", 40)
            .await
            .unwrap();
        assert!(second_enable.is_empty());
        let authoritative = repo
            .get_by_requirement_id(&claim.row.requirement_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(authoritative.status, "in_progress");
        assert_eq!(
            authoritative.claim_token,
            claim.row.claim_token,
            "stale enable must not revoke the new claim capability"
        );
    }

    #[tokio::test]
    async fn deleting_an_active_requirement_parks_and_preserves_execution_identity() {
        let (repo, _db, conversation_id, _terminal_id) = setup().await;
        let requirement = repo.insert(&make_row("active-delete", "1")).await.unwrap();
        let claim = repo
            .claim_next_for_runner(
                "active-delete",
                Some(&conversation_id),
                None,
                60_000,
                100,
            )
            .await
            .unwrap()
            .unwrap()
            .row;
        assert!(matches!(
            repo.delete(&requirement.requirement_id).await.unwrap_err(),
            DbError::Conflict(_)
        ));
        let parked = repo
            .get_by_requirement_id(&requirement.requirement_id)
            .await
            .unwrap()
            .expect("active row must not be deleted");
        assert_eq!(parked.status, "needs_review");
        assert_eq!(parked.claim_generation, claim.claim_generation);
        assert_eq!(
            parked.owner_conversation_id.as_deref(),
            Some(conversation_id.as_str())
        );
        assert_eq!(parked.active_turn_started_at, claim.active_turn_started_at);
        assert!(parked.lease_expires_at.is_none());
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

    #[tokio::test]
    async fn pre_effect_abandon_is_atomic_refunds_attempt_clears_terminal_text_and_consumes_guard()
    {
        let (repo, db, conversation_id, _terminal_id) = setup().await;
        let requirement = repo
            .insert(&make_row("pre-effect-command", "1"))
            .await
            .unwrap();
        let claim_at = now_ms() + 10_000;
        let claimed =
            claim_for_conversation(&repo, "pre-effect-command", &conversation_id, claim_at).await;
        let claim_token = claimed.claim_token.clone().unwrap();

        let raw_transition = sqlx::query(
            "UPDATE requirements \
                SET status = 'pending', completion_note = NULL, \
                    owner_conversation_id = NULL, owner_terminal_id = NULL, \
                    active_turn_started_at = NULL, lease_expires_at = NULL, \
                    attempt_count = MAX(attempt_count - 1, 0), claim_token = NULL \
              WHERE requirement_id = ?1",
        )
        .bind(&requirement.requirement_id)
        .execute(db.pool())
        .await;
        assert!(
            raw_transition.is_err(),
            "raw active->pending must be physically rejected without the exact command"
        );

        sqlx::query(
            "UPDATE requirements SET completion_note = 'stale terminal-looking text' \
              WHERE requirement_id = ?1",
        )
        .bind(&requirement.requirement_id)
        .execute(db.pool())
        .await
        .unwrap();

        let before = repo
            .get_by_requirement_id(&requirement.requirement_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(before.status, "in_progress");
        assert_eq!(before.updated_at, claimed.updated_at);
        let abandoned = repo
            .abandon_claim_before_admission_exact(
                &requirement.requirement_id,
                Some(&conversation_id),
                None,
                claimed.claim_generation,
                &claim_token,
                claim_at - 5_000,
            )
            .await
            .unwrap()
            .expect("exact claim with no receiver evidence may be abandoned");

        assert_eq!(abandoned.status, "pending");
        assert_eq!(abandoned.claim_generation, claimed.claim_generation);
        assert_eq!(abandoned.attempt_count, 0);
        assert_eq!(abandoned.claim_token, None);
        assert_eq!(abandoned.completion_note, None);
        assert_eq!(abandoned.owner_conversation_id, None);
        assert_eq!(abandoned.owner_terminal_id, None);
        assert_eq!(abandoned.active_turn_started_at, None);
        assert_eq!(abandoned.lease_expires_at, None);
        assert_eq!(abandoned.started_at, claimed.started_at);
        assert_eq!(
            abandoned.updated_at, claimed.updated_at,
            "an old custodian timestamp must not move updated_at backwards"
        );
        let guard_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM requirement_pre_effect_abandon_guards")
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(
            guard_count, 0,
            "the INSERT command must consume its capability before commit"
        );
    }

    #[tokio::test]
    async fn every_conversation_receipt_state_and_live_aggregate_authority_block_abandon() {
        for (status, suffix) in [("accepted", "accepted"), ("completed", "completed")] {
            let (repo, db, conversation_id, _terminal_id) = setup().await;
            let installation_owner = crate::installation_owner_id(db.pool()).await.unwrap();
            let tag = format!("receipt-{suffix}");
            let requirement = repo.insert(&make_row(&tag, "1")).await.unwrap();
            let claimed = claim_for_conversation(&repo, &tag, &conversation_id, 100).await;
            let claim_token = claimed.claim_token.clone().unwrap();
            insert_autowork_receipt(
                &db,
                &installation_owner,
                &conversation_id,
                &claimed,
                status,
            )
            .await;

            assert!(
                repo.abandon_claim_before_admission_exact(
                    &requirement.requirement_id,
                    Some(&conversation_id),
                    None,
                    claimed.claim_generation,
                    &claim_token,
                    200,
                )
                .await
                .unwrap()
                .is_none(),
                "{status} is permanent execution evidence"
            );
            let retained = repo
                .get_by_requirement_id(&requirement.requirement_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(retained.status, "in_progress");
            assert_eq!(retained.claim_generation, claimed.claim_generation);
            assert_eq!(retained.claim_token.as_deref(), Some(claim_token.as_str()));
            assert_eq!(
                retained.owner_conversation_id.as_deref(),
                Some(conversation_id.as_str())
            );
        }

        let (repo, db, conversation_id, _terminal_id) = setup().await;
        let installation_owner = crate::installation_owner_id(db.pool()).await.unwrap();
        let requirement = repo
            .insert(&make_row("aggregate-authority", "1"))
            .await
            .unwrap();
        let claimed =
            claim_for_conversation(&repo, "aggregate-authority", &conversation_id, 100).await;
        let claim_token = claimed.claim_token.clone().unwrap();
        let operation_id = "ordinary-live-turn";
        sqlx::query(
            "INSERT INTO conversation_delivery_receipts (\
                 operation_id, message_id, conversation_id, user_id, kind, \
                 request_payload, status, created_at, updated_at\
             ) VALUES (?1, ?2, ?3, ?4, 'turn', '{}', 'accepted', 100, 100)",
        )
        .bind(operation_id)
        .bind(MessageId::new().into_string())
        .bind(&conversation_id)
        .bind(&installation_owner)
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "UPDATE conversations \
                SET status = 'running', active_turn_operation_id = ?1, \
                    admission_epoch = admission_epoch + 1 \
              WHERE conversation_id = ?2",
        )
        .bind(operation_id)
        .bind(&conversation_id)
        .execute(db.pool())
        .await
        .unwrap();
        let claim_receipts: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM conversation_delivery_receipts \
              WHERE json_extract(request_payload, '$.autowork_authority.requirement_id') = ?1 \
                AND json_extract(request_payload, '$.autowork_authority.claim_generation') = ?2",
        )
        .bind(&requirement.requirement_id)
        .bind(claimed.claim_generation)
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(claim_receipts, 0, "this case exercises aggregate authority");
        assert!(
            repo.abandon_claim_before_admission_exact(
                &requirement.requirement_id,
                Some(&conversation_id),
                None,
                claimed.claim_generation,
                &claim_token,
                200,
            )
            .await
            .unwrap()
            .is_none(),
            "a live receiver aggregate must fail closed even if its receipt is not AutoWork-shaped"
        );
        let retained = repo
            .get_by_requirement_id(&requirement.requirement_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(retained.status, "in_progress");
        assert_eq!(retained.claim_token.as_deref(), Some(claim_token.as_str()));
    }

    #[tokio::test]
    async fn terminal_admission_is_absorbing_even_with_wrong_capability_and_after_settlement() {
        let (repo, db, _conversation_id, terminal_id) = setup().await;
        let requirement = repo
            .insert(&make_row("terminal-evidence", "1"))
            .await
            .unwrap();
        let claimed = claim_for_terminal(&repo, "terminal-evidence", &terminal_id, 100).await;
        let claim_token = claimed.claim_token.clone().unwrap();
        let wrong_token = if claim_token == "f".repeat(64) {
            "e".repeat(64)
        } else {
            "f".repeat(64)
        };
        let turn_token = MessageId::new().into_string();
        sqlx::query(
            "INSERT INTO terminal_turn_admissions (\
                 turn_token, terminal_id, pty_epoch, requirement_id, \
                 claim_generation, claim_token, phase, admitted_at\
             ) VALUES (?1, ?2, 7, ?3, ?4, ?5, 'admitted', 100)",
        )
        .bind(&turn_token)
        .bind(&terminal_id)
        .bind(&requirement.requirement_id)
        .bind(claimed.claim_generation)
        .bind(&wrong_token)
        .execute(db.pool())
        .await
        .unwrap();

        for expected_phase in ["admitted", "settled"] {
            assert!(
                repo.abandon_claim_before_admission_exact(
                    &requirement.requirement_id,
                    None,
                    Some(&terminal_id),
                    claimed.claim_generation,
                    &claim_token,
                    200,
                )
                .await
                .unwrap()
                .is_none(),
                "{expected_phase} Terminal evidence must be absorbing regardless of token"
            );
            let retained = repo
                .get_by_requirement_id(&requirement.requirement_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(retained.status, "in_progress");
            assert_eq!(retained.claim_generation, claimed.claim_generation);
            assert_eq!(retained.claim_token.as_deref(), Some(claim_token.as_str()));

            if expected_phase == "admitted" {
                sqlx::query(
                    "UPDATE terminal_turn_admissions \
                        SET phase = 'settled', outcome = 'needs_review', \
                            detail = 'corrupt-token evidence retained', settled_at = 200 \
                      WHERE turn_token = ?1",
                )
                .bind(&turn_token)
                .execute(db.pool())
                .await
                .unwrap();
            }
        }
    }

    #[tokio::test]
    async fn stale_or_wrong_abandon_commands_cannot_touch_or_authorize_a_new_generation() {
        let (repo, db, conversation_id, terminal_id) = setup().await;
        let requirement = repo
            .insert(&make_row("stale-abandon", "1"))
            .await
            .unwrap();
        let first = claim_for_conversation(&repo, "stale-abandon", &conversation_id, 100).await;
        let first_token = first.claim_token.clone().unwrap();
        let wrong_owner = ConversationId::new().into_string();
        let wrong_token = if first_token == "f".repeat(64) {
            "e".repeat(64)
        } else {
            "f".repeat(64)
        };

        for result in [
            repo.abandon_claim_before_admission_exact(
                &requirement.requirement_id,
                Some(&wrong_owner),
                None,
                first.claim_generation,
                &first_token,
                110,
            )
            .await
            .unwrap(),
            repo.abandon_claim_before_admission_exact(
                &requirement.requirement_id,
                Some(&conversation_id),
                None,
                first.claim_generation,
                &wrong_token,
                111,
            )
            .await
            .unwrap(),
            repo.abandon_claim_before_admission_exact(
                &requirement.requirement_id,
                Some(&conversation_id),
                None,
                first.claim_generation + 1,
                &first_token,
                112,
            )
            .await
            .unwrap(),
            repo.abandon_claim_before_admission_exact(
                &requirement.requirement_id,
                None,
                Some(&terminal_id),
                first.claim_generation,
                &first_token,
                113,
            )
            .await
            .unwrap(),
        ] {
            assert!(result.is_none());
        }
        let still_first = repo
            .get_by_requirement_id(&requirement.requirement_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(still_first.status, "in_progress");
        assert_eq!(still_first.claim_token.as_deref(), Some(first_token.as_str()));

        repo.abandon_claim_before_admission_exact(
            &requirement.requirement_id,
            Some(&conversation_id),
            None,
            first.claim_generation,
            &first_token,
            120,
        )
        .await
        .unwrap()
        .expect("the exact command may abandon generation one");
        let second = claim_for_conversation(&repo, "stale-abandon", &conversation_id, 130).await;
        assert_eq!(second.claim_generation, first.claim_generation + 1);
        assert_ne!(second.claim_token.as_deref(), Some(first_token.as_str()));

        let stale_guard = sqlx::query(
            "INSERT INTO requirement_pre_effect_abandon_guards (\
                 requirement_id, claim_generation, claim_token, \
                 owner_conversation_id, owner_terminal_id, created_at\
             ) VALUES (?1, ?2, ?3, ?4, NULL, 140)",
        )
        .bind(&requirement.requirement_id)
        .bind(first.claim_generation)
        .bind(&first_token)
        .bind(&conversation_id)
        .execute(db.pool())
        .await;
        assert!(
            stale_guard.is_err(),
            "a consumed generation-one capability cannot become a permit for generation two"
        );
        let authoritative = repo
            .get_by_requirement_id(&requirement.requirement_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(authoritative.status, "in_progress");
        assert_eq!(authoritative.claim_generation, second.claim_generation);
        assert_eq!(authoritative.claim_token, second.claim_token);
        let guard_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM requirement_pre_effect_abandon_guards")
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(guard_count, 0);
    }

    #[tokio::test]
    async fn conversation_admission_and_pre_effect_abandon_have_one_cross_connection_winner() {
        let database_root = tempfile::tempdir().unwrap();
        let database_path = database_root.path().join("admission-abandon-race.db");
        let database = crate::init_database(&database_path).await.unwrap();
        let (repo, db, conversation_id, _terminal_id) = setup_database(database).await;
        let installation_owner = crate::installation_owner_id(db.pool()).await.unwrap();
        let mut new_requirement = make_row("admission-abandon-race", "1");
        new_requirement.created_by = installation_owner.clone();
        let requirement = repo.insert(&new_requirement).await.unwrap();
        let claimed =
            claim_for_conversation(&repo, "admission-abandon-race", &conversation_id, 100).await;
        let claim_token = claimed.claim_token.clone().unwrap();
        let authority = RequirementConversationTurnAuthority {
            requirement_id: requirement.requirement_id.clone(),
            claim_generation: claimed.claim_generation,
            claim_token: claim_token.clone(),
        };
        let claim_token_sha256 = format!("{:x}", Sha256::digest(claim_token.as_bytes()));
        let request_payload = serde_json::json!({
            "autowork_authority": {
                "requirement_id": requirement.requirement_id,
                "claim_generation": claimed.claim_generation,
                "claim_token_sha256": claim_token_sha256,
            }
        })
        .to_string();
        let operation_id = "autowork-admission-abandon-race".to_owned();
        let candidate_message_id = MessageId::new().into_string();
        let barrier = Arc::new(Barrier::new(3));

        let admission_repo = SqliteConversationRepository::new(db.pool().clone());
        let admission_barrier = barrier.clone();
        let admission_user_id = installation_owner.clone();
        let admission_conversation_id = conversation_id.clone();
        let admission_operation_id = operation_id.clone();
        let admission_message_id = candidate_message_id.clone();
        let admission_payload = request_payload.clone();
        let admission_task = tokio::spawn(async move {
            admission_barrier.wait().await;
            admission_repo
                .claim_autowork_turn_delivery_receipt_and_admit_with_candidate(
                    &admission_user_id,
                    &admission_conversation_id,
                    &admission_operation_id,
                    &admission_message_id,
                    &admission_payload,
                    &authority,
                    0,
                    200,
                )
                .await
        });

        let abandon_repo = SqliteRequirementRepository::new(db.pool().clone());
        let abandon_barrier = barrier.clone();
        let abandon_requirement_id = requirement.requirement_id.clone();
        let abandon_conversation_id = conversation_id.clone();
        let abandon_claim_token = claim_token.clone();
        let abandon_task = tokio::spawn(async move {
            abandon_barrier.wait().await;
            abandon_repo
                .abandon_claim_before_admission_exact(
                    &abandon_requirement_id,
                    Some(&abandon_conversation_id),
                    None,
                    claimed.claim_generation,
                    &abandon_claim_token,
                    201,
                )
                .await
        });

        barrier.wait().await;
        let admission_result = admission_task.await.unwrap();
        let abandon_result = abandon_task.await.unwrap().unwrap();
        let admission_won = admission_result.is_ok();
        let abandon_won = abandon_result.is_some();
        assert_ne!(
            admission_won, abandon_won,
            "serialized SQLite writers must allow exactly one authority transition"
        );

        let authoritative = repo
            .get_by_requirement_id(&requirement.requirement_id)
            .await
            .unwrap()
            .unwrap();
        let receipt_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM conversation_delivery_receipts WHERE operation_id = ?1",
        )
        .bind(&operation_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
        let (conversation_status, active_operation): (String, Option<String>) = sqlx::query_as(
            "SELECT status, active_turn_operation_id FROM conversations \
              WHERE conversation_id = ?1",
        )
        .bind(&conversation_id)
        .fetch_one(db.pool())
        .await
        .unwrap();

        if admission_won {
            assert_eq!(authoritative.status, "in_progress");
            assert_eq!(authoritative.claim_token.as_deref(), Some(claim_token.as_str()));
            assert_eq!(receipt_count, 1);
            assert_eq!(conversation_status, "running");
            assert_eq!(active_operation.as_deref(), Some(operation_id.as_str()));
        } else {
            assert!(matches!(admission_result, Err(DbError::Conflict(_))));
            assert_eq!(authoritative.status, "pending");
            assert_eq!(authoritative.claim_token, None);
            assert_eq!(receipt_count, 0);
            assert_eq!(conversation_status, "pending");
            assert_eq!(active_operation, None);
        }
        let guard_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM requirement_pre_effect_abandon_guards")
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(guard_count, 0);
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
