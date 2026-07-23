use std::str::FromStr;
use std::sync::Arc;

use chrono::{TimeZone, Utc};
use cron::Schedule;
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use tokio::task::JoinHandle;

use nomifun_common::{CronJobId, TimestampMs, UserId, now_ms};

use crate::error::CronError;
use crate::types::{CronJob, CronSchedule};

// ---------------------------------------------------------------------------
// Schedule validation
// ---------------------------------------------------------------------------

/// Normalize a cron expression so both 5-field (standard Unix) and 6-field
/// (seconds-prefixed, as required by the `cron` crate) forms are accepted.
/// A 5-field expression is promoted by prepending `0 ` for the seconds field.
pub(crate) fn normalize_cron_expr(expr: &str) -> String {
    let trimmed = expr.trim();
    let field_count = trimmed.split_whitespace().count();
    if field_count == 5 {
        format!("0 {trimmed}")
    } else {
        trimmed.to_owned()
    }
}

pub fn validate_cron_expression(expr: &str) -> Result<Schedule, CronError> {
    let normalized = normalize_cron_expr(expr);
    Schedule::from_str(&normalized).map_err(|e| CronError::InvalidCronExpression(format!("{expr}: {e}")))
}

pub fn validate_timezone(tz: &str) -> Result<chrono_tz::Tz, CronError> {
    tz.parse::<chrono_tz::Tz>()
        .map_err(|_| CronError::InvalidTimezone(tz.to_owned()))
}

// ---------------------------------------------------------------------------
// Next-run computation
// ---------------------------------------------------------------------------

pub fn compute_next_run(schedule: &CronSchedule, now: TimestampMs) -> Option<TimestampMs> {
    match schedule {
        CronSchedule::At { at_ms, .. } => Some(*at_ms),
        CronSchedule::Every { every_ms, .. } => {
            if *every_ms <= 0 {
                return None;
            }
            Some(now + *every_ms)
        }
        CronSchedule::Cron { expr, tz, .. } => compute_cron_next_run(expr, tz.as_deref(), now),
    }
}

fn compute_cron_next_run(expr: &str, tz: Option<&str>, now: TimestampMs) -> Option<TimestampMs> {
    let normalized = normalize_cron_expr(expr);
    let schedule = Schedule::from_str(&normalized).ok()?;

    if let Some(tz_str) = tz {
        let tz_parsed: chrono_tz::Tz = tz_str.parse().ok()?;
        let now_dt = tz_parsed.timestamp_millis_opt(now).single()?;
        let next = schedule.after(&now_dt).next()?;
        Some(next.timestamp_millis())
    } else {
        let now_dt = Utc.timestamp_millis_opt(now).single()?;
        let next = schedule.after(&now_dt).next()?;
        Some(next.timestamp_millis())
    }
}

// ---------------------------------------------------------------------------
// Schedule validation for create/update
// ---------------------------------------------------------------------------

pub fn validate_schedule(schedule: &CronSchedule) -> Result<(), CronError> {
    match schedule {
        CronSchedule::At { .. } => Ok(()),
        CronSchedule::Every { every_ms, .. } => {
            if *every_ms <= 0 {
                return Err(CronError::InvalidSchedule("every_ms must be positive".into()));
            }
            Ok(())
        }
        CronSchedule::Cron { expr, tz, .. } => {
            if expr.trim().is_empty() {
                return Ok(());
            }
            validate_cron_expression(expr)?;
            if let Some(tz_str) = tz {
                validate_timezone(tz_str)?;
            }
            // Guard the silent-failure path: an expression can parse yet have no
            // upcoming occurrence (e.g. an impossible date). Such a job would be
            // created `enabled` with `next_run_at = None` and never scheduled,
            // with no error surfaced. Reject it loudly instead.
            if compute_cron_next_run(expr, tz.as_deref(), now_ms()).is_none() {
                return Err(CronError::InvalidCronExpression(format!(
                    "{expr}: expression has no upcoming run time"
                )));
            }
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// CronScheduler — manages tokio timers for scheduled jobs
// ---------------------------------------------------------------------------

/// Scheduler callbacks carry both the local job id and the owner captured
/// when the timer was installed. `CronService::tick` re-verifies the pair
/// against the current row before execution, closing delete/recreate races.
pub type TickCallback = Arc<dyn Fn(String, String) + Send + Sync>;

struct ScheduledHandle {
    user_id: String,
    task: JoinHandle<()>,
}

pub struct CronScheduler {
    handles: DashMap<String, ScheduledHandle>,
    tick_callback: TickCallback,
}

impl CronScheduler {
    pub fn new(tick_callback: TickCallback) -> Self {
        Self {
            handles: DashMap::new(),
            tick_callback,
        }
    }

    pub fn schedule_job(&self, job: &CronJob) {
        if CronJobId::parse(&job.cron_job_id).is_err()
            || UserId::try_from(job.user_id.as_str()).is_err()
        {
            tracing::error!(job_id = %job.cron_job_id, user_id = %job.user_id, "Refusing to schedule a cron job with invalid durable ids");
            return;
        }
        self.cancel_job(&job.cron_job_id);

        if !job.enabled {
            return;
        }

        let Some(next_run_at) = job.next_run_at else {
            return;
        };

        let job_id = job.cron_job_id.clone();
        let user_id = job.user_id.clone();
        let handle_owner = user_id.clone();
        let schedule = job.schedule.clone();
        let callback = Arc::clone(&self.tick_callback);

        let handle = match &schedule {
            CronSchedule::At { .. } => {
                spawn_at_timer(job_id, user_id, next_run_at, callback)
            }
            CronSchedule::Every { every_ms, .. } => {
                spawn_every_timer(job_id, user_id, next_run_at, *every_ms, callback)
            }
            CronSchedule::Cron { expr, tz, .. } => {
                spawn_cron_timer(
                    job_id,
                    user_id,
                    next_run_at,
                    expr.clone(),
                    tz.clone(),
                    callback,
                )
            }
        };

        self.handles.insert(
            job.cron_job_id.clone(),
            ScheduledHandle {
                user_id: handle_owner,
                task: handle,
            },
        );
    }

    pub fn cancel_job(&self, job_id: &str) {
        if CronJobId::parse(job_id).is_err() {
            return;
        }
        if let Some((_, scheduled)) = self.handles.remove(job_id) {
            scheduled.task.abort();
        }
    }

    /// Cancel only the timer installed for this owner. A callback from an old
    /// deleted job must never abort a newer same-id timer belonging to someone
    /// else merely because its database verification failed.
    pub fn cancel_job_for_owner(&self, job_id: &str, user_id: &str) {
        if CronJobId::parse(job_id).is_err() || UserId::try_from(user_id).is_err() {
            return;
        }
        if let Entry::Occupied(entry) = self.handles.entry(job_id.to_owned())
            && entry.get().user_id == user_id
        {
            entry.remove().task.abort();
        }
    }

    pub fn reschedule_job(&self, job: &CronJob) {
        self.schedule_job(job);
    }

    pub fn cancel_all(&self) {
        for entry in self.handles.iter() {
            entry.value().task.abort();
        }
        self.handles.clear();
    }

    pub fn active_count(&self) -> usize {
        self.handles.len()
    }

    pub fn is_scheduled(&self, job_id: &str) -> bool {
        CronJobId::parse(job_id).is_ok() && self.handles.contains_key(job_id)
    }
}

impl Drop for CronScheduler {
    fn drop(&mut self) {
        self.cancel_all();
    }
}

// ---------------------------------------------------------------------------
// Timer spawn helpers
// ---------------------------------------------------------------------------

fn spawn_at_timer(
    job_id: String,
    user_id: String,
    run_at: TimestampMs,
    callback: TickCallback,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let delay = delay_until(run_at);
        if delay > 0 {
            tokio::time::sleep(tokio::time::Duration::from_millis(delay as u64)).await;
        }
        callback(job_id, user_id);
    })
}

fn spawn_every_timer(
    job_id: String,
    user_id: String,
    first_run_at: TimestampMs,
    every_ms: i64,
    callback: TickCallback,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let initial_delay = delay_until(first_run_at);
        if initial_delay > 0 {
            tokio::time::sleep(tokio::time::Duration::from_millis(initial_delay as u64)).await;
        }
        callback(job_id.clone(), user_id.clone());

        let interval_duration = tokio::time::Duration::from_millis(every_ms as u64);
        let mut interval = tokio::time::interval(interval_duration);
        interval.tick().await; // first tick fires immediately, skip it
        loop {
            interval.tick().await;
            callback(job_id.clone(), user_id.clone());
        }
    })
}

fn spawn_cron_timer(
    job_id: String,
    user_id: String,
    first_run_at: TimestampMs,
    expr: String,
    tz: Option<String>,
    callback: TickCallback,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let initial_delay = delay_until(first_run_at);
        if initial_delay > 0 {
            tokio::time::sleep(tokio::time::Duration::from_millis(initial_delay as u64)).await;
        }
        callback(job_id.clone(), user_id.clone());

        loop {
            let now = now_ms();
            let next = compute_cron_next_run(&expr, tz.as_deref(), now);
            let Some(next_at) = next else {
                break;
            };
            let delay = delay_until(next_at);
            if delay > 0 {
                tokio::time::sleep(tokio::time::Duration::from_millis(delay as u64)).await;
            }
            callback(job_id.clone(), user_id.clone());
        }
    })
}

fn delay_until(target_ms: TimestampMs) -> i64 {
    let now = now_ms();
    (target_ms - now).max(0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const JOB_1: &str = "0190f5fe-7c00-7a00-8abc-012345678901";
    const JOB_2: &str = "0190f5fe-7c00-7a00-8abc-012345678902";
    const JOB_3: &str = "0190f5fe-7c00-7a00-8abc-012345678903";
    const JOB_AT: &str = "0190f5fe-7c00-7a00-8abc-012345678904";
    const JOB_EVERY: &str = "0190f5fe-7c00-7a00-8abc-012345678905";
    const USER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000001";
    const FOREIGN_USER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000002";
    const CONVERSATION_ID: &str = "0190f5fe-7c00-7a00-8000-000000000001";

    // -- compute_next_run ----------------------------------------------------

    #[test]
    fn next_run_at_returns_at_ms() {
        let schedule = CronSchedule::At {
            at_ms: 5000,
            description: None,
        };
        assert_eq!(compute_next_run(&schedule, 1000), Some(5000));
    }

    #[test]
    fn next_run_at_past_still_returns_at_ms() {
        let schedule = CronSchedule::At {
            at_ms: 500,
            description: None,
        };
        assert_eq!(compute_next_run(&schedule, 1000), Some(500));
    }

    #[test]
    fn next_run_every_adds_interval() {
        let schedule = CronSchedule::Every {
            every_ms: 60000,
            description: None,
        };
        assert_eq!(compute_next_run(&schedule, 1000), Some(61000));
    }

    #[test]
    fn next_run_every_zero_returns_none() {
        let schedule = CronSchedule::Every {
            every_ms: 0,
            description: None,
        };
        assert_eq!(compute_next_run(&schedule, 1000), None);
    }

    #[test]
    fn next_run_every_negative_returns_none() {
        let schedule = CronSchedule::Every {
            every_ms: -100,
            description: None,
        };
        assert_eq!(compute_next_run(&schedule, 1000), None);
    }

    #[test]
    fn next_run_cron_returns_future_time() {
        let now = now_ms();
        let schedule = CronSchedule::Cron {
            expr: "0 * * * * *".into(), // every minute
            tz: None,
            description: None,
        };
        let next = compute_next_run(&schedule, now);
        assert!(next.is_some());
        assert!(next.unwrap() > now);
    }

    #[test]
    fn next_run_cron_with_timezone() {
        let now = now_ms();
        let schedule = CronSchedule::Cron {
            expr: "0 * * * * *".into(),
            tz: Some("Asia/Shanghai".into()),
            description: None,
        };
        let next = compute_next_run(&schedule, now);
        assert!(next.is_some());
        assert!(next.unwrap() > now);
    }

    #[test]
    fn next_run_cron_invalid_expr_returns_none() {
        let schedule = CronSchedule::Cron {
            expr: "invalid".into(),
            tz: None,
            description: None,
        };
        assert_eq!(compute_next_run(&schedule, 1000), None);
    }

    #[test]
    fn next_run_cron_invalid_tz_returns_none() {
        let schedule = CronSchedule::Cron {
            expr: "0 * * * * *".into(),
            tz: Some("Mars/Olympus".into()),
            description: None,
        };
        assert_eq!(compute_next_run(&schedule, 1000), None);
    }

    // -- validate_schedule ---------------------------------------------------

    #[test]
    fn validate_at_schedule() {
        let s = CronSchedule::At {
            at_ms: 1000,
            description: None,
        };
        assert!(validate_schedule(&s).is_ok());
    }

    #[test]
    fn validate_every_positive() {
        let s = CronSchedule::Every {
            every_ms: 1000,
            description: None,
        };
        assert!(validate_schedule(&s).is_ok());
    }

    #[test]
    fn validate_every_zero_fails() {
        let s = CronSchedule::Every {
            every_ms: 0,
            description: None,
        };
        assert!(validate_schedule(&s).is_err());
    }

    #[test]
    fn validate_every_negative_fails() {
        let s = CronSchedule::Every {
            every_ms: -1,
            description: None,
        };
        assert!(validate_schedule(&s).is_err());
    }

    #[test]
    fn validate_cron_valid() {
        let s = CronSchedule::Cron {
            expr: "0 */5 * * * *".into(),
            tz: None,
            description: None,
        };
        assert!(validate_schedule(&s).is_ok());
    }

    #[test]
    fn validate_cron_empty_expr_is_manual_only() {
        let s = CronSchedule::Cron {
            expr: String::new(),
            tz: None,
            description: Some("manual".into()),
        };
        assert!(validate_schedule(&s).is_ok());
        assert_eq!(compute_next_run(&s, 1000), None);
    }

    #[test]
    fn validate_cron_with_valid_tz() {
        let s = CronSchedule::Cron {
            expr: "0 0 9 * * *".into(),
            tz: Some("Asia/Shanghai".into()),
            description: None,
        };
        assert!(validate_schedule(&s).is_ok());
    }

    #[test]
    fn validate_cron_invalid_expr() {
        let s = CronSchedule::Cron {
            expr: "invalid".into(),
            tz: None,
            description: None,
        };
        let err = validate_schedule(&s).unwrap_err();
        assert!(matches!(err, CronError::InvalidCronExpression(_)));
    }

    #[test]
    fn validate_cron_invalid_tz() {
        let s = CronSchedule::Cron {
            expr: "0 * * * * *".into(),
            tz: Some("Invalid/TZ".into()),
            description: None,
        };
        let err = validate_schedule(&s).unwrap_err();
        assert!(matches!(err, CronError::InvalidTimezone(_)));
    }

    #[test]
    fn validate_cron_rejects_expr_with_no_upcoming_run() {
        // Feb 30 never occurs: the expression parses but has no next run, which
        // would otherwise be created enabled yet never scheduled, silently.
        let s = CronSchedule::Cron {
            expr: "0 0 0 30 2 ?".into(),
            tz: None,
            description: None,
        };
        assert!(validate_schedule(&s).is_err());
    }

    #[test]
    fn validate_cron_minute_level_is_accepted() {
        for expr in ["* * * * *", "*/1 * * * *", "0 */5 * * * ?", "0 * * * * ?"] {
            let s = CronSchedule::Cron {
                expr: expr.into(),
                tz: Some("Asia/Shanghai".into()),
                description: None,
            };
            assert!(validate_schedule(&s).is_ok(), "expected {expr} to validate");
        }
    }

    // -- validate_cron_expression / validate_timezone -------------------------

    #[test]
    fn validate_cron_expression_valid() {
        assert!(validate_cron_expression("0 */5 * * * *").is_ok());
        assert!(validate_cron_expression("0 0 9 * * *").is_ok());
        assert!(validate_cron_expression("0 0 0 1 1 *").is_ok());
    }

    #[test]
    fn validate_cron_expression_accepts_five_field_unix_form() {
        // Standard 5-field Unix cron (minute hour day month dow) — must be
        // auto-normalized to the 6-field form the `cron` crate requires.
        assert!(validate_cron_expression("0 9 * * *").is_ok());
        assert!(validate_cron_expression("30 14 * * MON-FRI").is_ok());
        assert!(validate_cron_expression("0 10 * * WED").is_ok());
        assert!(validate_cron_expression("0 * * * *").is_ok());
    }

    #[test]
    fn validate_cron_expression_invalid() {
        assert!(validate_cron_expression("not a cron").is_err());
        assert!(validate_cron_expression("").is_err());
    }

    #[test]
    fn normalize_cron_expr_leaves_six_field_alone() {
        assert_eq!(normalize_cron_expr("0 0 9 * * *"), "0 0 9 * * *");
    }

    #[test]
    fn normalize_cron_expr_promotes_five_field() {
        assert_eq!(normalize_cron_expr("0 9 * * *"), "0 0 9 * * *");
        assert_eq!(normalize_cron_expr("  30 14 * * MON-FRI  "), "0 30 14 * * MON-FRI");
    }

    #[test]
    fn validate_timezone_valid() {
        assert!(validate_timezone("UTC").is_ok());
        assert!(validate_timezone("Asia/Shanghai").is_ok());
        assert!(validate_timezone("America/New_York").is_ok());
    }

    #[test]
    fn validate_timezone_invalid() {
        assert!(validate_timezone("Invalid/TZ").is_err());
        assert!(validate_timezone("Mars").is_err());
    }

    // -- CronScheduler -------------------------------------------------------

    #[tokio::test]
    async fn scheduler_schedule_and_cancel() {
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let called_clone = Arc::clone(&called);
        let scheduler = CronScheduler::new(Arc::new(move |_id, _owner_id| {
            called_clone.store(true, std::sync::atomic::Ordering::SeqCst);
        }));

        let job = make_test_job(JOB_1, true, Some(now_ms() + 100_000));
        scheduler.schedule_job(&job);
        assert!(scheduler.is_scheduled(JOB_1));
        assert_eq!(scheduler.active_count(), 1);

        scheduler.cancel_job_for_owner(JOB_1, FOREIGN_USER_ID);
        assert!(
            scheduler.is_scheduled(JOB_1),
            "a stale callback cannot cancel another owner's current timer"
        );

        scheduler.cancel_job_for_owner(JOB_1, USER_ID);
        assert!(!scheduler.is_scheduled(JOB_1));
        assert_eq!(scheduler.active_count(), 0);
    }

    #[tokio::test]
    async fn scheduler_disabled_job_not_scheduled() {
        let scheduler = CronScheduler::new(Arc::new(|_, _| {}));
        let job = make_test_job(JOB_1, false, Some(now_ms() + 100_000));
        scheduler.schedule_job(&job);
        assert!(!scheduler.is_scheduled(JOB_1));
    }

    #[tokio::test]
    async fn scheduler_no_next_run_not_scheduled() {
        let scheduler = CronScheduler::new(Arc::new(|_, _| {}));
        let job = make_test_job(JOB_1, true, None);
        scheduler.schedule_job(&job);
        assert!(!scheduler.is_scheduled(JOB_1));
    }

    #[tokio::test]
    async fn scheduler_cancel_all() {
        let scheduler = CronScheduler::new(Arc::new(|_, _| {}));
        let future = now_ms() + 100_000;
        scheduler.schedule_job(&make_test_job(JOB_1, true, Some(future)));
        scheduler.schedule_job(&make_test_job(JOB_2, true, Some(future)));
        scheduler.schedule_job(&make_test_job(JOB_3, true, Some(future)));
        assert_eq!(scheduler.active_count(), 3);

        scheduler.cancel_all();
        assert_eq!(scheduler.active_count(), 0);
    }

    #[tokio::test]
    async fn scheduler_reschedule_replaces_timer() {
        let scheduler = CronScheduler::new(Arc::new(|_, _| {}));
        let job = make_test_job(JOB_1, true, Some(now_ms() + 100_000));
        scheduler.schedule_job(&job);
        assert!(scheduler.is_scheduled(JOB_1));

        let updated = CronJob {
            next_run_at: Some(now_ms() + 200_000),
            ..job
        };
        scheduler.reschedule_job(&updated);
        assert!(scheduler.is_scheduled(JOB_1));
        assert_eq!(scheduler.active_count(), 1);
    }

    #[tokio::test]
    async fn scheduler_cancel_nonexistent_no_panic() {
        let scheduler = CronScheduler::new(Arc::new(|_, _| {}));
        scheduler.cancel_job("0190f5fe-7c00-7a00-8abc-012345678999");
    }

    #[tokio::test]
    async fn scheduler_at_timer_fires_callback() {
        let (tx, rx) = tokio::sync::oneshot::channel::<(String, String)>();
        let tx = Arc::new(std::sync::Mutex::new(Some(tx)));
        let scheduler = CronScheduler::new(Arc::new(move |id, owner_id| {
            if let Some(sender) = tx.lock().unwrap().take() {
                let _ = sender.send((id, owner_id));
            }
        }));

        let job = CronJob {
            schedule: CronSchedule::At {
                at_ms: now_ms() + 50,
                description: None,
            },
            next_run_at: Some(now_ms() + 50),
            ..make_test_job(JOB_AT, true, Some(now_ms() + 50))
        };
        scheduler.schedule_job(&job);

        let result = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx).await;
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap().unwrap(),
            (JOB_AT.to_owned(), USER_ID.to_owned())
        );
    }

    #[tokio::test]
    async fn scheduler_every_timer_fires_callback() {
        let counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counter_clone = Arc::clone(&counter);
        let scheduler = CronScheduler::new(Arc::new(move |_id, _owner_id| {
            counter_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));

        let job = CronJob {
            schedule: CronSchedule::Every {
                every_ms: 50,
                description: None,
            },
            next_run_at: Some(now_ms() + 50),
            ..make_test_job(JOB_EVERY, true, Some(now_ms() + 50))
        };
        scheduler.schedule_job(&job);

        tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;
        scheduler.cancel_job(JOB_EVERY);

        let count = counter.load(std::sync::atomic::Ordering::SeqCst);
        assert!(count >= 2, "expected at least 2 ticks, got {count}");
    }

    // -- Test helper ----------------------------------------------------------

    fn make_test_job(id: &str, enabled: bool, next_run_at: Option<TimestampMs>) -> CronJob {
        use crate::types::{CreatedBy, ExecutionMode};
        CronJob {
            cron_job_id: id.to_owned(),
            user_id: USER_ID.into(),
            name: "Test".into(),
            enabled,
            schedule: CronSchedule::Every {
                every_ms: 60000,
                description: None,
            },
            message: "test message".into(),
            execution_mode: ExecutionMode::Existing,
            agent_config: None,
            conversation_id: Some(CONVERSATION_ID.into()),
            conversation_title: None,
            agent_type: "acp".into(),
            created_by: CreatedBy::User,
            skill_content: None,
            description: None,
            created_at: 1000,
            updated_at: 1000,
            next_run_at,
            last_run_at: None,
            last_status: None,
            last_error: None,
            run_count: 0,
            retry_count: 0,
            max_retries: 3,
        }
    }
}
