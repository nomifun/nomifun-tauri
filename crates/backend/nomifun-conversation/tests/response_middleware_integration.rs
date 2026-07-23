//! Black-box integration tests for the message middleware.
//!
//! Tests cover the test-plan.md section 6 (消息中间件):
//! - Think tag cleaning (6.1)
//! - Cron command detection (6.2)
//! - MessageMiddleware pipeline end-to-end

use async_trait::async_trait;
use nomifun_conversation::{
    CronCommand, CronCommandResult, CronCreateParams, CronUpdateParams, ICronService, MessageMiddleware,
    detect_cron_commands, has_cron_commands, strip_cron_commands, strip_think_tags,
};

// ===========================================================================
// 6.1 Think tag cleaning
// ===========================================================================

#[test]
fn think_tag_before_and_after_text() {
    let input = "前文<think>内部思考</think>后文";
    assert_eq!(strip_think_tags(input), "前文后文");
}

#[test]
fn thinking_tag_before_answer() {
    let input = "<thinking>深度思考</thinking>回答";
    assert_eq!(strip_think_tags(input), "回答");
}

#[test]
fn nested_think_tags() {
    // Non-greedy: `<think>外<think>内</think>` matches first close,
    // then `外</think>后` remains. The second `</think>` is literal text.
    // Per API spec this is the expected behavior — nested tags are consumed.
    let input = "<think>外<think>内</think>外</think>后";
    let result = strip_think_tags(input);
    // First match: `<think>外<think>内</think>` → removed → "外</think>后"
    assert_eq!(result, "外</think>后");
}

#[test]
fn no_think_tags() {
    let input = "普通文本";
    assert_eq!(strip_think_tags(input), "普通文本");
}

#[test]
fn empty_think_tag() {
    let input = "a<think></think>b";
    assert_eq!(strip_think_tags(input), "ab");
}

#[test]
fn think_tag_with_multiline_content() {
    let input = "Start\n<think>\nLine 1\nLine 2\nLine 3\n</think>\nEnd";
    let result = strip_think_tags(input);
    assert_eq!(result, "Start\n\nEnd");
}

#[test]
fn mixed_think_and_thinking_tags() {
    let input = "<think>a</think>middle<thinking>b</thinking>end";
    assert_eq!(strip_think_tags(input), "middleend");
}

#[test]
fn unclosed_think_tag_preserved() {
    let input = "<think>no closing tag";
    assert_eq!(strip_think_tags(input), "<think>no closing tag");
}

// ===========================================================================
// 6.2 Cron command detection
// ===========================================================================

#[test]
fn detect_cron_create_with_all_fields() {
    let input = "[CRON_CREATE]\nname: 每日代码审查\nschedule: 0 9 * * MON\nschedule_description: 每周一上午 9 点\nmessage: 请审查本周的代码变更\n[/CRON_CREATE]";
    let commands = detect_cron_commands(input);
    assert_eq!(commands.len(), 1);
    match &commands[0] {
        CronCommand::Create(params) => {
            assert_eq!(params.name, "每日代码审查");
            assert_eq!(params.schedule, "0 9 * * MON");
            assert_eq!(params.schedule_description, "每周一上午 9 点");
            assert_eq!(params.message, "请审查本周的代码变更");
        }
        _ => panic!("Expected Create"),
    }
}

#[test]
fn detect_cron_list() {
    let input = "[CRON_LIST]";
    let commands = detect_cron_commands(input);
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0], CronCommand::List);
}

#[test]
fn detect_cron_update_with_all_fields() {
    let input = "[CRON_UPDATE: 0190f5fe-7c00-7a00-8abc-012345678908]\nname: 更新后的任务\nschedule: 0 10 * * MON\nschedule_description: 每周一上午 10 点\nmessage: 请发送更新后的提醒\n[/CRON_UPDATE]";
    let commands = detect_cron_commands(input);
    assert_eq!(commands.len(), 1);
    match &commands[0] {
        CronCommand::Update(params) => {
            assert_eq!(params.job_id, "0190f5fe-7c00-7a00-8abc-012345678908");
            assert_eq!(params.name, "更新后的任务");
            assert_eq!(params.schedule, "0 10 * * MON");
            assert_eq!(params.schedule_description, "每周一上午 10 点");
            assert_eq!(params.message, "请发送更新后的提醒");
        }
        _ => panic!("Expected Update"),
    }
}

#[test]
fn detect_cron_delete_with_id() {
    let input = "[CRON_DELETE: 0190f5fe-7c00-7a00-8abc-012345678907]";
    let commands = detect_cron_commands(input);
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0], CronCommand::Delete("0190f5fe-7c00-7a00-8abc-012345678907".into()));
}

#[test]
fn detect_mixed_content_with_cron() {
    let input = "Here's what I did:\n\n[CRON_CREATE]\nname: cleanup\nschedule: 0 0 * * *\nschedule_description: daily midnight\nmessage: clean old files\n[/CRON_CREATE]\n\nThen updated one:\n[CRON_UPDATE: 0190f5fe-7c00-7a00-8abc-012345678905]\nname: cleanup-v2\nschedule: 0 1 * * *\nschedule_description: daily 1am\nmessage: clean old files carefully\n[/CRON_UPDATE]\n\nAlso check: [CRON_LIST]\n\nAnd remove old one: [CRON_DELETE: 0190f5fe-7c00-7a00-8abc-012345678907]";
    let commands = detect_cron_commands(input);
    assert_eq!(commands.len(), 4);
    assert!(matches!(&commands[0], CronCommand::Create(_)));
    assert!(matches!(&commands[1], CronCommand::Update(_)));
    assert_eq!(commands[2], CronCommand::List);
    assert_eq!(commands[3], CronCommand::Delete("0190f5fe-7c00-7a00-8abc-012345678907".into()));
}

#[test]
fn detect_no_commands_in_normal_text() {
    let commands = detect_cron_commands("普通回复");
    assert!(commands.is_empty());
}

#[test]
fn has_cron_detects_all_types() {
    assert!(has_cron_commands("[CRON_CREATE]\nschedule: *\n[/CRON_CREATE]"));
    assert!(has_cron_commands("[CRON_UPDATE: 0190f5fe-7c00-7a00-8abc-012345678901]\nschedule: *\n[/CRON_UPDATE]"));
    assert!(has_cron_commands("[CRON_LIST]"));
    assert!(has_cron_commands("[CRON_DELETE: 0190f5fe-7c00-7a00-8abc-012345678901]"));
    assert!(!has_cron_commands("nothing here"));
}

#[test]
fn strip_cron_removes_all_types_preserves_text() {
    let input = "Before\n[CRON_CREATE]\nname: t\nschedule: *\n[/CRON_CREATE]\nMiddle [CRON_LIST] After [CRON_DELETE: 0190f5fe-7c00-7a00-8abc-012345678901] Between [CRON_UPDATE: 0190f5fe-7c00-7a00-8abc-012345678904]\nname: t2\nschedule: 0 * * * *\n[/CRON_UPDATE] End";
    let stripped = strip_cron_commands(input);
    assert!(!stripped.contains("[CRON_"));
    assert!(stripped.contains("Before"));
    assert!(stripped.contains("Middle"));
    assert!(stripped.contains("After"));
    assert!(stripped.contains("End"));
}

#[test]
fn cron_create_missing_schedule_not_parsed() {
    let input = "[CRON_CREATE]\nname: broken\nmessage: no schedule\n[/CRON_CREATE]";
    let commands = detect_cron_commands(input);
    assert!(commands.is_empty());
}

#[test]
fn cron_delete_with_whitespace_in_id() {
    let input = "[CRON_DELETE:   0190f5fe-7c00-7a00-8abc-012345678906   ]";
    let commands = detect_cron_commands(input);
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0], CronCommand::Delete("0190f5fe-7c00-7a00-8abc-012345678906".into()));
}

#[test]
fn multiple_cron_creates() {
    let input = "[CRON_CREATE]\nname: first\nschedule: 0 * * * *\n[/CRON_CREATE] text [CRON_CREATE]\nname: second\nschedule: 0 0 * * *\n[/CRON_CREATE]";
    let commands = detect_cron_commands(input);
    assert_eq!(commands.len(), 2);
    match (&commands[0], &commands[1]) {
        (CronCommand::Create(a), CronCommand::Create(b)) => {
            assert_eq!(a.name, "first");
            assert_eq!(b.name, "second");
        }
        _ => panic!("Expected two Create commands"),
    }
}

// ===========================================================================
// MessageMiddleware end-to-end
// ===========================================================================

/// Test cron service that tracks execution.
struct TrackingCronService;

#[async_trait]
impl ICronService for TrackingCronService {
    async fn create_job(&self, _user_id: &str, _conversation_id: &str, params: &CronCreateParams) -> CronCommandResult {
        CronCommandResult {
            success: true,
            message: format!("Job '{}' created with schedule '{}'", params.name, params.schedule),
        }
    }

    async fn update_job(&self, _user_id: &str, conversation_id: &str, params: &CronUpdateParams) -> CronCommandResult {
        CronCommandResult {
            success: true,
            message: format!("Job '{}' updated in conversation '{}'", params.job_id, conversation_id),
        }
    }

    async fn list_jobs(&self, _user_id: &str, conversation_id: &str) -> CronCommandResult {
        CronCommandResult {
            success: true,
            message: format!("Active jobs for '{}': daily-check (0 9 * * *)", conversation_id),
        }
    }

    async fn delete_job(&self, _user_id: &str, job_id: &str) -> CronCommandResult {
        CronCommandResult {
            success: true,
            message: format!("Job '{}' deleted", job_id),
        }
    }
}

/// Failing cron service for error path testing.
struct FailingCronService;

#[async_trait]
impl ICronService for FailingCronService {
    async fn create_job(
        &self,
        _user_id: &str,
        _conversation_id: &str,
        _params: &CronCreateParams,
    ) -> CronCommandResult {
        CronCommandResult {
            success: false,
            message: "Database connection lost".to_string(),
        }
    }

    async fn update_job(
        &self,
        _user_id: &str,
        _conversation_id: &str,
        _params: &CronUpdateParams,
    ) -> CronCommandResult {
        CronCommandResult {
            success: false,
            message: "Update rejected".to_string(),
        }
    }

    async fn list_jobs(&self, _user_id: &str, _conversation_id: &str) -> CronCommandResult {
        CronCommandResult {
            success: false,
            message: "Service unavailable".to_string(),
        }
    }

    async fn delete_job(&self, _user_id: &str, _job_id: &str) -> CronCommandResult {
        CronCommandResult {
            success: false,
            message: "Permission denied".to_string(),
        }
    }
}

#[tokio::test]
async fn middleware_plain_text_passes_through() {
    let mw = MessageMiddleware::new(Some(Box::new(TrackingCronService)));
    let result = mw.process("Hello world!", "u1", "c1").await;
    assert_eq!(result.message, "Hello world!");
    assert!(result.display_message.is_none());
    assert!(result.system_responses.is_empty());
}

#[tokio::test]
async fn middleware_strips_think_and_thinking() {
    let mw = MessageMiddleware::new(None);
    let input = "<think>reasoning about the problem</think>The answer is 42.<thinking>more thought</thinking>";
    let result = mw.process(input, "u1", "c1").await;
    assert_eq!(result.message, "The answer is 42.");
}

#[tokio::test]
async fn middleware_executes_cron_create_successfully() {
    let mw = MessageMiddleware::new(Some(Box::new(TrackingCronService)));
    let input = "Done! I've set up the job.\n[CRON_CREATE]\nname: daily-review\nschedule: 0 9 * * *\nschedule_description: Daily at 9am\nmessage: Review PRs\n[/CRON_CREATE]";
    let result = mw.process(input, "u1", "c1").await;

    assert!(!result.message.contains("[CRON_CREATE]"));
    assert!(result.message.contains("Done!"));
    assert!(result.display_message.is_some());
    assert_eq!(result.system_responses.len(), 1);
    assert!(result.system_responses[0].contains("daily-review"));
}

#[tokio::test]
async fn middleware_executes_cron_list() {
    let mw = MessageMiddleware::new(Some(Box::new(TrackingCronService)));
    let input = "Here are your jobs: [CRON_LIST]";
    let result = mw.process(input, "u1", "c1").await;

    assert!(!result.message.contains("[CRON_LIST]"));
    assert_eq!(result.system_responses.len(), 1);
    assert!(result.system_responses[0].contains("Active jobs for 'c1'"));
}

#[tokio::test]
async fn middleware_executes_cron_update() {
    let mw = MessageMiddleware::new(Some(Box::new(TrackingCronService)));
    let input = "Updating it now. [CRON_UPDATE: 0190f5fe-7c00-7a00-8abc-012345678906]\nname: renamed\nschedule: 0 8 * * *\nschedule_description: Daily at 8am\nmessage: New prompt\n[/CRON_UPDATE]";
    let result = mw.process(input, "u1", "c1").await;

    assert!(!result.message.contains("[CRON_UPDATE"));
    assert_eq!(result.system_responses.len(), 1);
    assert!(result.system_responses[0].contains("42"));
    assert!(result.system_responses[0].contains("c1"));
}

#[tokio::test]
async fn middleware_executes_cron_delete() {
    let mw = MessageMiddleware::new(Some(Box::new(TrackingCronService)));
    let input = "Removing it now. [CRON_DELETE: 0190f5fe-7c00-7a00-8abc-012345678906]";
    let result = mw.process(input, "u1", "c1").await;

    assert!(!result.message.contains("[CRON_DELETE"));
    assert_eq!(result.system_responses.len(), 1);
    assert!(result.system_responses[0].contains("42"));
}

#[tokio::test]
async fn middleware_handles_cron_failure() {
    let mw = MessageMiddleware::new(Some(Box::new(FailingCronService)));
    let input = "[CRON_UPDATE: 0190f5fe-7c00-7a00-8abc-012345678901]\nname: renamed\nschedule: 0 8 * * *\nschedule_description: Daily at 8am\nmessage: New prompt\n[/CRON_UPDATE]";
    let result = mw.process(input, "u1", "c1").await;

    assert_eq!(result.system_responses.len(), 1);
    assert!(result.system_responses[0].contains("System Error"));
    assert!(result.system_responses[0].contains("Update rejected"));
}

#[tokio::test]
async fn middleware_no_cron_service_returns_unavailable() {
    let mw = MessageMiddleware::new(None);
    let input = "Listing jobs [CRON_LIST]";
    let result = mw.process(input, "u1", "c1").await;

    assert_eq!(result.system_responses.len(), 1);
    assert!(result.system_responses[0].contains("not available"));
}

#[tokio::test]
async fn middleware_combined_think_tags_and_cron_commands() {
    let mw = MessageMiddleware::new(Some(Box::new(TrackingCronService)));
    let input = "<thinking>Let me think about this...</thinking>Sure, I'll set that up for you.\n[CRON_CREATE]\nname: weekly\nschedule: 0 0 * * SUN\nschedule_description: Every Sunday\nmessage: Weekly report\n[/CRON_CREATE]";
    let result = mw.process(input, "u1", "c1").await;

    // Think tags stripped
    assert!(!result.message.contains("<thinking>"));
    // Cron commands stripped
    assert!(!result.message.contains("[CRON_CREATE]"));
    // Text preserved
    assert!(result.message.contains("Sure, I'll set that up for you."));
    // Cron executed
    assert_eq!(result.system_responses.len(), 1);
    assert!(result.system_responses[0].contains("weekly"));
}

#[tokio::test]
async fn middleware_multiple_cron_commands_all_executed() {
    let mw = MessageMiddleware::new(Some(Box::new(TrackingCronService)));
    let input = "[CRON_CREATE]\nname: job1\nschedule: 0 * * * *\n[/CRON_CREATE] and [CRON_UPDATE: 0190f5fe-7c00-7a00-8abc-012345678901]\nname: job2\nschedule: 0 1 * * *\nschedule_description: daily 1am\nmessage: New prompt\n[/CRON_UPDATE] and [CRON_LIST] and [CRON_DELETE: 0190f5fe-7c00-7a00-8abc-012345678901]";
    let result = mw.process(input, "u1", "c1").await;

    assert_eq!(result.system_responses.len(), 4);
}
