use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

const MAX_CRON_COMMANDS_PER_RESPONSE: usize = 16;
const CRON_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(not(test))]
const CRON_PIPELINE_TIMEOUT: Duration = Duration::from_secs(15);
#[cfg(test)]
const CRON_PIPELINE_TIMEOUT: Duration = Duration::from_millis(120);

// ---------------------------------------------------------------------------
// Think-tag cleaning
// ---------------------------------------------------------------------------

/// Regex for `<think>...</think>` and `<thinking>...</thinking>` tags.
///
/// Uses `(?s)` (dot-all) so `.` matches newlines within the tag body.
static THINK_TAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<think(?:ing)?>.*?</think(?:ing)?>").expect("valid think-tag regex"));

/// Remove `<think>...</think>` and `<thinking>...</thinking>` tags from text.
pub fn strip_think_tags(text: &str) -> String {
    THINK_TAG_RE.replace_all(text, "").into_owned()
}

// ---------------------------------------------------------------------------
// Cron command detection
// ---------------------------------------------------------------------------

/// Regex for `[CRON_CREATE]...[/CRON_CREATE]` blocks (dot-all).
static CRON_CREATE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)\[CRON_CREATE\]\s*(.*?)\s*\[/CRON_CREATE\]").expect("valid cron-create regex"));

/// Regex for `[CRON_UPDATE: <id>]...[/CRON_UPDATE]` blocks (dot-all).
static CRON_UPDATE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)\[CRON_UPDATE:\s*([^\]]+)\]\s*(.*?)\s*\[/CRON_UPDATE\]").expect("valid cron-update regex")
});

/// Regex for `[CRON_LIST]`.
static CRON_LIST_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[CRON_LIST\]").expect("valid cron-list regex"));

/// Regex for `[CRON_DELETE: <id>]`.
static CRON_DELETE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[CRON_DELETE:\s*([^\]]+)\]").expect("valid cron-delete regex"));

/// A parsed cron command extracted from agent text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CronCommand {
    Create(CronCreateParams),
    Update(CronUpdateParams),
    List,
    Delete(String),
}

/// Parameters for a cron-create command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronCreateParams {
    pub name: String,
    pub schedule: String,
    pub schedule_description: String,
    pub message: String,
}

/// Parameters for a cron-update command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronUpdateParams {
    pub job_id: String,
    pub name: String,
    pub schedule: String,
    pub schedule_description: String,
    pub message: String,
}

/// Detect all cron commands embedded in the text.
pub fn detect_cron_commands(text: &str) -> Vec<CronCommand> {
    detect_cron_commands_bounded(text, usize::MAX)
}

/// Detect at most `limit` commands. The middleware asks for `MAX + 1` so it
/// can report overflow without first allocating/scanning an attacker-sized
/// command vector in a single non-preemptible poll.
fn detect_cron_commands_bounded(text: &str, limit: usize) -> Vec<CronCommand> {
    let mut commands = Vec::new();

    for cap in CRON_CREATE_RE.captures_iter(text) {
        if let Some(body) = cap.get(1)
            && let Some(params) = parse_cron_create_body(body.as_str())
        {
            commands.push(CronCommand::Create(params));
            if commands.len() >= limit {
                return commands;
            }
        }
    }

    for cap in CRON_UPDATE_RE.captures_iter(text) {
        if let (Some(job_id_match), Some(body)) = (cap.get(1), cap.get(2))
            && let Some(params) = parse_cron_update_body(job_id_match.as_str().trim(), body.as_str())
        {
            commands.push(CronCommand::Update(params));
            if commands.len() >= limit {
                return commands;
            }
        }
    }

    if CRON_LIST_RE.is_match(text) {
        commands.push(CronCommand::List);
        if commands.len() >= limit {
            return commands;
        }
    }

    for cap in CRON_DELETE_RE.captures_iter(text) {
        if let Some(id_match) = cap.get(1) {
            let id = id_match.as_str().trim().to_string();
            if !id.is_empty() {
                commands.push(CronCommand::Delete(id));
                if commands.len() >= limit {
                    return commands;
                }
            }
        }
    }

    commands
}

/// Quick check: does the text contain any cron commands?
pub fn has_cron_commands(text: &str) -> bool {
    CRON_CREATE_RE.is_match(text)
        || CRON_UPDATE_RE.is_match(text)
        || CRON_LIST_RE.is_match(text)
        || CRON_DELETE_RE.is_match(text)
}

/// Strip all cron command tags from text, returning cleaned content.
pub fn strip_cron_commands(text: &str) -> String {
    let result = CRON_CREATE_RE.replace_all(text, "");
    let result = CRON_UPDATE_RE.replace_all(&result, "");
    let result = CRON_LIST_RE.replace_all(&result, "");
    let result = CRON_DELETE_RE.replace_all(&result, "");
    result.into_owned()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CronCommandFields {
    name: Option<String>,
    schedule: Option<String>,
    schedule_description: Option<String>,
    message: Option<String>,
}

/// Parse the body of a `[CRON_CREATE]...[/CRON_CREATE]` block.
///
/// Expected key-value format (one per line):
/// ```text
/// name: <value>
/// schedule: <cron expression>
/// schedule_description: <human-readable>
/// message: <prompt text>
/// ```
fn parse_cron_command_body(body: &str) -> Option<CronCommandFields> {
    let mut name = None;
    let mut schedule = None;
    let mut schedule_description = None;
    let mut message = None;

    for line in body.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("name:") {
            name = Some(val.trim().to_string());
        } else if let Some(val) = line.strip_prefix("schedule_description:") {
            schedule_description = Some(val.trim().to_string());
        } else if let Some(val) = line.strip_prefix("schedule:") {
            schedule = Some(val.trim().to_string());
        } else if let Some(val) = line.strip_prefix("message:") {
            message = Some(val.trim().to_string());
        }
    }

    Some(CronCommandFields {
        name,
        schedule,
        schedule_description,
        message,
    })
}

fn parse_cron_create_body(body: &str) -> Option<CronCreateParams> {
    let fields = parse_cron_command_body(body)?;
    Some(CronCreateParams {
        name: fields.name.unwrap_or_default(),
        schedule: fields.schedule?,
        schedule_description: fields.schedule_description.unwrap_or_default(),
        message: fields.message.unwrap_or_default(),
    })
}

fn parse_cron_update_body(job_id: &str, body: &str) -> Option<CronUpdateParams> {
    if job_id.is_empty() {
        return None;
    }

    let fields = parse_cron_command_body(body)?;
    Some(CronUpdateParams {
        job_id: job_id.to_string(),
        name: fields.name?,
        schedule: fields.schedule?,
        schedule_description: fields.schedule_description?,
        message: fields.message?,
    })
}

// ---------------------------------------------------------------------------
// ICronService trait (implemented in Phase 12)
// ---------------------------------------------------------------------------

/// Result of a cron command execution.
#[derive(Debug, Clone)]
pub struct CronCommandResult {
    pub success: bool,
    pub message: String,
}

/// Abstract cron service for executing cron commands.
///
/// This trait will be implemented in Phase 12 (cron module).
/// The middleware uses it via dependency injection.
#[async_trait]
pub trait ICronService: Send + Sync {
    /// Create a cron job. Returns the created job ID on success.
    async fn create_job(&self, user_id: &str, conversation_id: &str, params: &CronCreateParams) -> CronCommandResult;

    /// Update an existing cron job.
    async fn update_job(&self, user_id: &str, conversation_id: &str, params: &CronUpdateParams) -> CronCommandResult;

    /// List cron jobs for the current conversation scope.
    /// Returns a formatted text response.
    async fn list_jobs(&self, user_id: &str, conversation_id: &str) -> CronCommandResult;

    /// Delete a cron job by ID.
    async fn delete_job(&self, user_id: &str, job_id: &str) -> CronCommandResult;
}

// ---------------------------------------------------------------------------
// MessageMiddleware
// ---------------------------------------------------------------------------

/// Result of message middleware processing.
#[derive(Debug, Clone)]
pub struct MiddlewareResult {
    /// The processed message content (think tags stripped, cron commands stripped).
    pub message: String,
    /// Optional display-only message (same as `message` if no cron commands).
    pub display_message: Option<String>,
    /// System responses generated by cron command execution.
    pub system_responses: Vec<String>,
}

/// Post-processing pipeline for completed agent messages.
///
/// Runs on each finished agent response to:
/// 1. Strip think/thinking tags
/// 2. Detect and execute embedded cron commands
/// 3. Return cleaned message + any system responses
pub struct MessageMiddleware {
    cron_service: Option<Box<dyn ICronService>>,
}

impl MessageMiddleware {
    /// Create middleware with optional cron service.
    ///
    /// When `cron_service` is `None`, cron commands are still detected and
    /// stripped, but not executed (system responses will indicate unavailability).
    pub fn new(cron_service: Option<Box<dyn ICronService>>) -> Self {
        Self { cron_service }
    }

    /// Process a completed agent message through the middleware pipeline.
    pub async fn process(&self, message: &str, user_id: &str, conversation_id: &str) -> MiddlewareResult {
        self.process_with_cancellation(message, user_id, conversation_id, None)
            .await
    }

    pub async fn process_with_cancellation(
        &self,
        message: &str,
        user_id: &str,
        conversation_id: &str,
        cancellation: Option<&CancellationToken>,
    ) -> MiddlewareResult {
        // Step 1: Strip think tags
        let cleaned = strip_think_tags(message);

        // Step 2: Detect cron commands
        if !has_cron_commands(&cleaned) {
            return MiddlewareResult {
                message: cleaned,
                display_message: None,
                system_responses: Vec::new(),
            };
        }

        let commands = detect_cron_commands_bounded(
            &cleaned,
            MAX_CRON_COMMANDS_PER_RESPONSE.saturating_add(1),
        );
        let display_message = strip_cron_commands(&cleaned);

        // Step 3: Execute cron commands
        let system_responses = self
            .execute_cron_commands(user_id, conversation_id, &commands, cancellation)
            .await;

        MiddlewareResult {
            message: display_message.clone(),
            display_message: Some(display_message),
            system_responses,
        }
    }

    /// Execute a list of cron commands via the injected service.
    async fn execute_cron_commands(
        &self,
        user_id: &str,
        conversation_id: &str,
        commands: &[CronCommand],
        cancellation: Option<&CancellationToken>,
    ) -> Vec<String> {
        let Some(cron_service) = &self.cron_service else {
            debug!("Cron commands detected but no cron service configured");
            return vec!["[System: Cron service is not available]".to_string()];
        };

        let mut responses = Vec::new();
        let deadline = tokio::time::Instant::now() + CRON_PIPELINE_TIMEOUT;

        for command in commands.iter().take(MAX_CRON_COMMANDS_PER_RESPONSE) {
            if cancellation.is_some_and(CancellationToken::is_cancelled) {
                break;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                responses.push(
                    "[System Error: Cron command pipeline exceeded its total time budget]"
                        .to_owned(),
                );
                break;
            }
            let operation = async {
                match command {
                    CronCommand::Create(params) => {
                        cron_service.create_job(user_id, conversation_id, params).await
                    }
                    CronCommand::Update(params) => {
                        cron_service.update_job(user_id, conversation_id, params).await
                    }
                    CronCommand::List => cron_service.list_jobs(user_id, conversation_id).await,
                    CronCommand::Delete(id) => cron_service.delete_job(user_id, id).await,
                }
            };
            let result = if let Some(cancellation) = cancellation {
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => break,
                    result = tokio::time::timeout(CRON_COMMAND_TIMEOUT.min(remaining), operation) => {
                        match result {
                            Ok(result) => result,
                            Err(_) => CronCommandResult {
                                success: false,
                                message: "Cron command timed out".to_owned(),
                            },
                        }
                    }
                }
            } else {
                match tokio::time::timeout(CRON_COMMAND_TIMEOUT.min(remaining), operation).await {
                    Ok(result) => result,
                    Err(_) => CronCommandResult {
                        success: false,
                        message: "Cron command timed out".to_owned(),
                    },
                }
            };

            if cancellation.is_some_and(CancellationToken::is_cancelled) {
                break;
            }

            if result.success {
                responses.push(format!("[System: {}]", result.message));
            } else {
                warn!(
                    command = ?command,
                    error = %result.message,
                    "Cron command execution failed"
                );
                responses.push(format!("[System Error: {}]", result.message));
            }
        }

        if commands.len() > MAX_CRON_COMMANDS_PER_RESPONSE {
            responses.push(format!(
                "[System Error: Cron command limit exceeded; at most {MAX_CRON_COMMANDS_PER_RESPONSE} commands are allowed per response]"
            ));
        } else if tokio::time::Instant::now() >= deadline
            && !responses.iter().any(|response| response.contains("total time budget"))
        {
            responses.push(
                "[System Error: Cron command pipeline exceeded its total time budget]".to_owned(),
            );
        }

        responses
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    // -----------------------------------------------------------------------
    // Think tag stripping
    // -----------------------------------------------------------------------

    #[test]
    fn strip_think_tags_basic() {
        let input = "Before<think>internal thought</think>After";
        assert_eq!(strip_think_tags(input), "BeforeAfter");
    }

    #[test]
    fn strip_thinking_tags_basic() {
        let input = "<thinking>deep reasoning</thinking>Answer here";
        assert_eq!(strip_think_tags(input), "Answer here");
    }

    #[test]
    fn strip_think_tags_multiline() {
        let input = "Start\n<think>\nline 1\nline 2\n</think>\nEnd";
        assert_eq!(strip_think_tags(input), "Start\n\nEnd");
    }

    #[test]
    fn strip_think_tags_multiple() {
        let input = "<think>a</think>middle<thinking>b</thinking>end";
        assert_eq!(strip_think_tags(input), "middleend");
    }

    #[test]
    fn strip_think_tags_none() {
        let input = "No tags here at all.";
        assert_eq!(strip_think_tags(input), "No tags here at all.");
    }

    #[test]
    fn strip_think_tags_empty() {
        assert_eq!(strip_think_tags(""), "");
    }

    #[test]
    fn strip_think_tags_nested_content() {
        // Non-greedy match consumes the inner content correctly
        let input = "<think>outer <b>bold</b> text</think>after";
        assert_eq!(strip_think_tags(input), "after");
    }

    #[test]
    fn strip_think_tags_with_code_blocks() {
        let input = "```rust\nfn main() {}\n```\n<think>private</think>\nResult";
        assert_eq!(strip_think_tags(input), "```rust\nfn main() {}\n```\n\nResult");
    }

    // -----------------------------------------------------------------------
    // Cron command detection
    // -----------------------------------------------------------------------

    #[test]
    fn detect_cron_create_command() {
        let input = "[CRON_CREATE]\nname: Daily review\nschedule: 0 9 * * MON\nschedule_description: Every Monday 9am\nmessage: Review code\n[/CRON_CREATE]";
        let commands = detect_cron_commands(input);
        assert_eq!(commands.len(), 1);
        match &commands[0] {
            CronCommand::Create(params) => {
                assert_eq!(params.name, "Daily review");
                assert_eq!(params.schedule, "0 9 * * MON");
                assert_eq!(params.schedule_description, "Every Monday 9am");
                assert_eq!(params.message, "Review code");
            }
            _ => panic!("Expected CronCommand::Create"),
        }
    }

    #[test]
    fn detect_cron_list_command() {
        let input = "Here are the jobs: [CRON_LIST]";
        let commands = detect_cron_commands(input);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0], CronCommand::List);
    }

    #[test]
    fn detect_cron_update_command() {
        let input = "[CRON_UPDATE: job-123]\nname: Updated Job\nschedule: 0 10 * * *\nschedule_description: Daily at 10am\nmessage: Updated instructions\n[/CRON_UPDATE]";
        let commands = detect_cron_commands(input);
        assert_eq!(commands.len(), 1);
        match &commands[0] {
            CronCommand::Update(params) => {
                assert_eq!(params.job_id, "job-123");
                assert_eq!(params.name, "Updated Job");
                assert_eq!(params.schedule, "0 10 * * *");
                assert_eq!(params.schedule_description, "Daily at 10am");
                assert_eq!(params.message, "Updated instructions");
            }
            _ => panic!("Expected CronCommand::Update"),
        }
    }

    #[test]
    fn detect_cron_delete_command() {
        let input = "[CRON_DELETE: job-123]";
        let commands = detect_cron_commands(input);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0], CronCommand::Delete("job-123".to_string()));
    }

    #[test]
    fn detect_mixed_cron_commands() {
        let input = "I'll manage your crons.\n[CRON_CREATE]\nname: test\nschedule: * * * * *\nschedule_description: every minute\nmessage: ping\n[/CRON_CREATE]\nUpdate too: [CRON_UPDATE: existing-job]\nname: updated\nschedule: 0 * * * *\nschedule_description: hourly\nmessage: pong\n[/CRON_UPDATE]\nAlso listing: [CRON_LIST]\nAnd deleting: [CRON_DELETE: old-job]";
        let commands = detect_cron_commands(input);
        assert_eq!(commands.len(), 4);
        assert!(matches!(&commands[0], CronCommand::Create(_)));
        assert!(matches!(&commands[1], CronCommand::Update(_)));
        assert_eq!(commands[2], CronCommand::List);
        assert_eq!(commands[3], CronCommand::Delete("old-job".to_string()));
    }

    #[test]
    fn detect_no_cron_commands() {
        let commands = detect_cron_commands("Just a normal reply.");
        assert!(commands.is_empty());
    }

    #[test]
    fn detect_cron_create_missing_schedule() {
        let input = "[CRON_CREATE]\nname: broken\nmessage: oops\n[/CRON_CREATE]";
        let commands = detect_cron_commands(input);
        // Missing required `schedule` field → not parsed
        assert!(commands.is_empty());
    }

    // -----------------------------------------------------------------------
    // has_cron_commands
    // -----------------------------------------------------------------------

    #[test]
    fn has_cron_commands_true() {
        assert!(has_cron_commands("[CRON_LIST]"));
        assert!(has_cron_commands("[CRON_DELETE: x]"));
        assert!(has_cron_commands("[CRON_UPDATE: x]\nschedule: *\n[/CRON_UPDATE]"));
        assert!(has_cron_commands("[CRON_CREATE]\nschedule: *\n[/CRON_CREATE]"));
    }

    #[test]
    fn has_cron_commands_false() {
        assert!(!has_cron_commands("No cron here"));
        assert!(!has_cron_commands("[CRON_SOMETHING_ELSE]"));
    }

    // -----------------------------------------------------------------------
    // strip_cron_commands
    // -----------------------------------------------------------------------

    #[test]
    fn strip_cron_commands_all_types() {
        let input = "Before [CRON_LIST] middle [CRON_DELETE: abc] after [CRON_CREATE]\nname: t\nschedule: *\n[/CRON_CREATE] and [CRON_UPDATE: abc]\nname: t2\nschedule: 0 * * * *\n[/CRON_UPDATE] end";
        let stripped = strip_cron_commands(input);
        assert!(!stripped.contains("[CRON_"));
        assert!(stripped.contains("Before"));
        assert!(stripped.contains("end"));
    }

    #[test]
    fn strip_cron_commands_no_commands() {
        let input = "Nothing to strip.";
        assert_eq!(strip_cron_commands(input), "Nothing to strip.");
    }

    // -----------------------------------------------------------------------
    // parse_cron_create_body
    // -----------------------------------------------------------------------

    #[test]
    fn parse_cron_create_body_full() {
        let body = "name: My Job\nschedule: 0 9 * * *\nschedule_description: Daily 9am\nmessage: Run tests";
        let params = parse_cron_create_body(body).unwrap();
        assert_eq!(params.name, "My Job");
        assert_eq!(params.schedule, "0 9 * * *");
        assert_eq!(params.schedule_description, "Daily 9am");
        assert_eq!(params.message, "Run tests");
    }

    #[test]
    fn parse_cron_create_body_minimal() {
        let body = "schedule: */5 * * * *";
        let params = parse_cron_create_body(body).unwrap();
        assert!(params.name.is_empty());
        assert_eq!(params.schedule, "*/5 * * * *");
        assert!(params.schedule_description.is_empty());
        assert!(params.message.is_empty());
    }

    #[test]
    fn parse_cron_create_body_no_schedule() {
        let body = "name: broken";
        assert!(parse_cron_create_body(body).is_none());
    }

    #[test]
    fn parse_cron_update_body_full() {
        let body = "name: Updated\nschedule: 0 9 * * *\nschedule_description: Daily 9am\nmessage: Run tests";
        let params = parse_cron_update_body("job-7", body).unwrap();
        assert_eq!(params.job_id, "job-7");
        assert_eq!(params.name, "Updated");
        assert_eq!(params.schedule, "0 9 * * *");
        assert_eq!(params.schedule_description, "Daily 9am");
        assert_eq!(params.message, "Run tests");
    }

    #[test]
    fn parse_cron_update_body_requires_job_id() {
        let body = "name: Updated\nschedule: 0 9 * * *";
        assert!(parse_cron_update_body("", body).is_none());
    }

    #[test]
    fn parse_cron_update_body_requires_all_fields() {
        let body = "name: Updated\nschedule: 0 9 * * *\nmessage: Run tests";
        assert!(parse_cron_update_body("job-7", body).is_none());
    }

    #[test]
    fn parse_cron_create_body_schedule_description_before_schedule() {
        // schedule_description should match before schedule due to prefix ordering
        let body = "schedule_description: desc first\nschedule: 0 * * * *\nname: test";
        let params = parse_cron_create_body(body).unwrap();
        assert_eq!(params.schedule, "0 * * * *");
        assert_eq!(params.schedule_description, "desc first");
    }

    // -----------------------------------------------------------------------
    // MessageMiddleware
    // -----------------------------------------------------------------------

    struct MockCronService;

    #[async_trait]
    impl ICronService for MockCronService {
        async fn create_job(
            &self,
            _user_id: &str,
            _conversation_id: &str,
            params: &CronCreateParams,
        ) -> CronCommandResult {
            CronCommandResult {
                success: true,
                message: format!("Created cron job '{}'", params.name),
            }
        }

        async fn update_job(
            &self,
            _user_id: &str,
            _conversation_id: &str,
            params: &CronUpdateParams,
        ) -> CronCommandResult {
            CronCommandResult {
                success: true,
                message: format!("Updated cron job '{}'", params.job_id),
            }
        }

        async fn list_jobs(&self, _user_id: &str, _conversation_id: &str) -> CronCommandResult {
            CronCommandResult {
                success: true,
                message: "No cron jobs found".to_string(),
            }
        }

        async fn delete_job(&self, _user_id: &str, job_id: &str) -> CronCommandResult {
            CronCommandResult {
                success: true,
                message: format!("Deleted cron job '{}'", job_id),
            }
        }
    }

    struct CountingCronService {
        calls: Arc<AtomicUsize>,
        delay: Duration,
    }

    #[async_trait]
    impl ICronService for CountingCronService {
        async fn create_job(
            &self,
            _user_id: &str,
            _conversation_id: &str,
            _params: &CronCreateParams,
        ) -> CronCommandResult {
            unreachable!("test only sends deletes")
        }

        async fn update_job(
            &self,
            _user_id: &str,
            _conversation_id: &str,
            _params: &CronUpdateParams,
        ) -> CronCommandResult {
            unreachable!("test only sends deletes")
        }

        async fn list_jobs(&self, _user_id: &str, _conversation_id: &str) -> CronCommandResult {
            unreachable!("test only sends deletes")
        }

        async fn delete_job(&self, _user_id: &str, _job_id: &str) -> CronCommandResult {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            CronCommandResult {
                success: true,
                message: "deleted".to_owned(),
            }
        }
    }

    #[tokio::test]
    async fn middleware_strips_think_tags() {
        let mw = MessageMiddleware::new(None);
        let result = mw
            .process("<think>reasoning</think>The answer is 42.", "user1", "conv1")
            .await;
        assert_eq!(result.message, "The answer is 42.");
        assert!(result.display_message.is_none());
        assert!(result.system_responses.is_empty());
    }

    #[tokio::test]
    async fn middleware_processes_cron_commands() {
        let mw = MessageMiddleware::new(Some(Box::new(MockCronService)));
        let input = "Done! [CRON_CREATE]\nname: daily\nschedule: 0 9 * * *\nschedule_description: Daily\nmessage: run\n[/CRON_CREATE]";
        let result = mw.process(input, "user1", "conv1").await;
        assert!(!result.message.contains("[CRON_CREATE]"));
        assert!(result.display_message.is_some());
        assert_eq!(result.system_responses.len(), 1);
        assert!(result.system_responses[0].contains("Created cron job"));
    }

    #[tokio::test]
    async fn middleware_processes_cron_update() {
        let mw = MessageMiddleware::new(Some(Box::new(MockCronService)));
        let input = "Updated it. [CRON_UPDATE: job-99]\nname: daily\nschedule: 0 10 * * *\nschedule_description: Daily\nmessage: run\n[/CRON_UPDATE]";
        let result = mw.process(input, "user1", "conv1").await;
        assert!(!result.message.contains("[CRON_UPDATE"));
        assert!(result.display_message.is_some());
        assert_eq!(result.system_responses.len(), 1);
        assert!(result.system_responses[0].contains("Updated cron job 'job-99'"));
    }

    #[tokio::test]
    async fn middleware_no_cron_service_reports_unavailable() {
        let mw = MessageMiddleware::new(None);
        let input = "Check [CRON_LIST] please";
        let result = mw.process(input, "user1", "conv1").await;
        assert_eq!(result.system_responses.len(), 1);
        assert!(result.system_responses[0].contains("not available"));
    }

    #[tokio::test]
    async fn middleware_combined_think_and_cron() {
        let mw = MessageMiddleware::new(Some(Box::new(MockCronService)));
        let input = "<thinking>let me plan</thinking>I'll delete that. [CRON_DELETE: job-99]";
        let result = mw.process(input, "user1", "conv1").await;
        assert!(!result.message.contains("<thinking>"));
        assert!(!result.message.contains("[CRON_DELETE"));
        assert_eq!(result.system_responses.len(), 1);
        assert!(result.system_responses[0].contains("Deleted cron job"));
    }

    #[tokio::test]
    async fn middleware_plain_text_passthrough() {
        let mw = MessageMiddleware::new(Some(Box::new(MockCronService)));
        let result = mw.process("Just a normal response.", "user1", "conv1").await;
        assert_eq!(result.message, "Just a normal response.");
        assert!(result.display_message.is_none());
        assert!(result.system_responses.is_empty());
    }

    #[tokio::test]
    async fn middleware_caps_command_detection_and_execution_at_admission() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mw = MessageMiddleware::new(Some(Box::new(CountingCronService {
            calls: Arc::clone(&calls),
            delay: Duration::ZERO,
        })));
        let input = (0..1_000)
            .map(|index| format!("[CRON_DELETE: job-{index}]"))
            .collect::<Vec<_>>()
            .join("\n");

        let result = mw.process(&input, "user1", "conv1").await;

        assert_eq!(calls.load(Ordering::SeqCst), MAX_CRON_COMMANDS_PER_RESPONSE);
        assert!(
            result
                .system_responses
                .iter()
                .any(|response| response.contains("command limit exceeded"))
        );
    }

    #[tokio::test]
    async fn middleware_uses_one_total_deadline_for_slow_commands() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mw = MessageMiddleware::new(Some(Box::new(CountingCronService {
            calls: Arc::clone(&calls),
            delay: Duration::from_millis(50),
        })));
        let input = (0..10)
            .map(|index| format!("[CRON_DELETE: slow-{index}]"))
            .collect::<Vec<_>>()
            .join("\n");
        let started = tokio::time::Instant::now();

        let result = mw.process(&input, "user1", "conv1").await;

        assert!(started.elapsed() < Duration::from_millis(400));
        assert!(calls.load(Ordering::SeqCst) < 10);
        assert!(
            result
                .system_responses
                .iter()
                .any(|response| response.contains("time budget") || response.contains("timed out"))
        );
    }
}
