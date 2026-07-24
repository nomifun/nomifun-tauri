use std::time::{SystemTime, UNIX_EPOCH};

use nomifun_cron::skill_file::{
    ParsedSkillContent, build_skill_content, content_hash, cron_skill_dir, cron_skill_file_path, parse_skill_content,
    read_skill_content, validate_skill_content, write_raw_skill_file, write_skill_file,
};

const JOB_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678901";
const JOB_ID_2: &str = "0190f5fe-7c00-7a00-8abc-012345678902";

fn unique_temp_dir(label: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("nomifun-cron-{label}-{}-{nanos}", std::process::id()))
}

#[test]
fn build_skill_content_matches_frontend_shape() {
    let content = build_skill_content(
        "Daily Report",
        "Line 1\nLine 2\r\nLine 3",
        "Run report",
        Some("Every day at 9am"),
    );

    assert!(content.contains("name: Daily Report"));
    assert!(content.contains("description: Line 1 Line 2 Line 3"));
    assert!(content.contains("This is a scheduled task: **Daily Report**"));
    assert!(content.contains("Schedule: Every day at 9am"));
    assert!(content.contains("## Instructions"));
    assert!(content.ends_with("Run report"));
}

#[test]
fn parse_skill_content_roundtrips_built_files() {
    let built = build_skill_content("My Job", "My Description", "First\n\nSecond", None);
    let parsed = parse_skill_content(&built).unwrap();
    assert_eq!(
        parsed,
        ParsedSkillContent {
            name: "My Job".into(),
            description: "My Description".into(),
            body: "First\n\nSecond".into(),
        }
    );
}

#[test]
fn parse_skill_content_skips_blank_lines_after_frontmatter() {
    let parsed = parse_skill_content("---\nname: Test\ndescription: Desc\n---\n\n\nPrompt").unwrap();
    assert_eq!(parsed.body, "Prompt");
}

#[test]
fn parse_skill_content_handles_empty_body() {
    let parsed = parse_skill_content("---\nname: Test\ndescription: Desc\n---\n\n").unwrap();
    assert_eq!(parsed.body, "");
}

#[test]
fn validate_skill_content_rejects_placeholders() {
    let err =
        validate_skill_content("---\nname: skill-name\ndescription: Real description\n---\n\nReal body").unwrap_err();
    assert!(err.to_string().contains("template placeholder"));
}

#[test]
fn content_hash_normalizes_line_endings_and_edges() {
    let a = content_hash("---\nname: Test\ndescription: Desc\n---\n\nBody\n");
    let b = content_hash("---\r\nname: Test\r\ndescription: Desc\r\n---\r\n\r\nBody");
    let c = content_hash("  ---\nname: Test\ndescription: Desc\n---\n\nBody  ");
    assert_eq!(a, b);
    assert_eq!(a, c);
}

#[tokio::test]
async fn write_read_and_resolve_skill_file_paths() {
    let base = unique_temp_dir("write-read");
    std::fs::create_dir_all(&base).unwrap();

    let file_path = write_skill_file(
        &base,
        JOB_ID,
        "Daily Report",
        "Generate daily report",
        "Run report",
        Some("Every day at 9am"),
    )
    .await
    .unwrap();

    assert_eq!(
        cron_skill_dir(&base, JOB_ID).unwrap(),
        base.join("cron").join("skills").join(JOB_ID)
    );
    assert_eq!(file_path, cron_skill_file_path(&base, JOB_ID).unwrap());

    let raw = read_skill_content(&base, JOB_ID).await.unwrap().unwrap();
    let parsed = parse_skill_content(&raw).unwrap();
    assert_eq!(parsed.name, "Daily Report");
    assert_eq!(parsed.description, "Generate daily report");
    assert_eq!(parsed.body, "Run report");

    std::fs::remove_dir_all(&base).unwrap();
}

#[tokio::test]
async fn write_raw_skill_file_validates_before_writing() {
    let base = unique_temp_dir("write-raw");
    std::fs::create_dir_all(&base).unwrap();

    let err = write_raw_skill_file(&base, JOB_ID_2, "not valid").await.unwrap_err();
    assert!(err.to_string().contains("skill file must start with YAML frontmatter"));
    assert!(read_skill_content(&base, JOB_ID_2).await.unwrap().is_none());

    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn cron_skill_paths_reject_noncanonical_job_ids() {
    let base = unique_temp_dir("invalid-id");
    assert!(cron_skill_dir(&base, "1").is_err());
    assert!(
        cron_skill_file_path(&base, "cron-0190f5fe-7c00-7a00-8abc-012345678901").is_err()
    );
}
