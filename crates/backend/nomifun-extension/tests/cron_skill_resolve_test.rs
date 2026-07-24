use nomifun_common::ConversationId;
use nomifun_extension::{resolve_skill_paths, skill_service};
use std::time::{SystemTime, UNIX_EPOCH};

const CRON_JOB_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678901";

fn unique_temp_dir(label: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("nomifun-extension-{label}-{}-{nanos}", std::process::id()))
}

#[tokio::test]
async fn resolve_skill_paths_includes_cron_skills_dir() {
    let base = unique_temp_dir("cron-paths");
    std::fs::create_dir_all(&base).unwrap();

    let paths = resolve_skill_paths(&base, &base);
    assert_eq!(paths.cron_skills_dir, base.join("cron").join("skills"));

    std::fs::remove_dir_all(&base).unwrap();
}

#[tokio::test]
async fn materialize_resolves_saved_cron_skill() {
    let base = unique_temp_dir("cron-materialize");
    let skill_dir = base.join("cron").join("skills").join(CRON_JOB_ID);
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        format!(
            "---\nname: {CRON_JOB_ID}\ndescription: Saved cron skill\n---\nUse the saved steps."
        ),
    )
    .unwrap();

    let paths = resolve_skill_paths(&base, &base);
    let conversation_id = ConversationId::new().into_string();
    let resolved = skill_service::materialize_skills_for_agent(
        &paths,
        &conversation_id,
        &[CRON_JOB_ID.to_owned()],
    )
    .await
    .unwrap();

    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].name, CRON_JOB_ID);
    assert_eq!(resolved[0].source_path, skill_dir);
    assert!(resolved[0].source_path.join("SKILL.md").exists());

    std::fs::remove_dir_all(&base).unwrap();
}
