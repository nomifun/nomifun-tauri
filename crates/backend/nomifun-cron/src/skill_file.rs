use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::fs;
use nomifun_common::CronJobId;

use crate::error::CronError;

pub const CRON_SKILLS_REL_DIR: &str = "cron/skills";
pub const CRON_SKILL_DIR_PREFIX: &str = "";
pub const SKILL_FILE_NAME: &str = "SKILL.md";

const PLACEHOLDER_PATTERNS: &[&str] = &[
    "skill-name",
    "one-line description",
    "your-skill-name",
    "your skill name",
    "description of",
];
const PLACEHOLDER_BODY_PATTERNS: &[&str] = &[
    "(full skill.md body",
    "full skill.md body",
    "(clear instructions for executing this task",
    "<full instructions: output format, tone, sources to check",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSkillContent {
    pub name: String,
    pub description: String,
    pub body: String,
}

pub fn cron_skill_name(job_id: &str) -> Result<String, CronError> {
    validate_job_id(job_id)?;
    Ok(job_id.to_owned())
}

pub fn cron_skill_dir(data_dir: &Path, job_id: &str) -> Result<PathBuf, CronError> {
    Ok(data_dir.join(CRON_SKILLS_REL_DIR).join(cron_skill_name(job_id)?))
}

pub fn cron_skill_file_path(data_dir: &Path, job_id: &str) -> Result<PathBuf, CronError> {
    Ok(cron_skill_dir(data_dir, job_id)?.join(SKILL_FILE_NAME))
}

pub fn build_skill_content(name: &str, description: &str, prompt: &str, schedule_description: Option<&str>) -> String {
    let sanitized_desc = description
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");

    let mut lines = vec![
        "---".to_owned(),
        format!("name: {name}"),
        format!("description: {sanitized_desc}"),
        "---".to_owned(),
        String::new(),
        format!("This is a scheduled task: **{name}**"),
    ];

    if let Some(schedule_description) = schedule_description {
        lines.push(format!("Schedule: {schedule_description}"));
    }

    lines.extend([
        String::new(),
        "## Instructions".to_owned(),
        String::new(),
        "You are executing a scheduled task. Follow the instructions below directly.".to_owned(),
        "Do NOT ask clarifying questions — just execute the task and produce the result.".to_owned(),
        String::new(),
        prompt.to_owned(),
    ]);

    lines.join("\n")
}

pub fn parse_skill_content(content: &str) -> Result<ParsedSkillContent, CronError> {
    let (name, description, body) = parse_frontmatter(content)?;
    let prompt = extract_prompt_from_body(&body);
    Ok(ParsedSkillContent {
        name,
        description,
        body: prompt,
    })
}

pub fn validate_skill_content(content: &str) -> Result<ParsedSkillContent, CronError> {
    let (name, description, body) = parse_frontmatter(content)?;
    let trimmed_body = body.trim();
    if trimmed_body.is_empty() {
        return Err(CronError::InvalidSkillContent("skill file body cannot be empty".into()));
    }
    if is_placeholder(&name, PLACEHOLDER_PATTERNS) {
        return Err(CronError::InvalidSkillContent(
            "skill name looks like a template placeholder".into(),
        ));
    }
    if is_placeholder(&description, PLACEHOLDER_PATTERNS) {
        return Err(CronError::InvalidSkillContent(
            "skill description looks like a template placeholder".into(),
        ));
    }
    if is_placeholder(trimmed_body, PLACEHOLDER_BODY_PATTERNS) {
        return Err(CronError::InvalidSkillContent(
            "skill body looks like a template placeholder".into(),
        ));
    }

    Ok(ParsedSkillContent {
        name,
        description,
        body: trimmed_body.to_owned(),
    })
}

pub fn content_hash(content: &str) -> String {
    let normalized = normalize_for_hash(content);
    let mut hasher = DefaultHasher::new();
    normalized.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

pub async fn write_skill_file(
    data_dir: &Path,
    job_id: &str,
    name: &str,
    description: &str,
    prompt: &str,
    schedule_description: Option<&str>,
) -> Result<PathBuf, CronError> {
    let content = build_skill_content(name, description, prompt, schedule_description);
    write_raw_skill_file(data_dir, job_id, &content).await
}

pub async fn write_raw_skill_file(
    data_dir: &Path,
    job_id: &str,
    raw_content: &str,
) -> Result<PathBuf, CronError> {
    validate_skill_content(raw_content)?;

    let dir = cron_skill_dir(data_dir, job_id)?;
    let file_path = dir.join(SKILL_FILE_NAME);
    fs::create_dir_all(&dir)
        .await
        .map_err(|err| CronError::InvalidSkillContent(err.to_string()))?;

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp_path = dir.join(format!("{SKILL_FILE_NAME}.tmp-{}-{nonce}", std::process::id()));

    fs::write(&temp_path, raw_content)
        .await
        .map_err(|err| CronError::InvalidSkillContent(err.to_string()))?;
    fs::rename(&temp_path, &file_path)
        .await
        .map_err(|err| CronError::InvalidSkillContent(err.to_string()))?;

    Ok(file_path)
}

pub async fn read_skill_content(data_dir: &Path, job_id: &str) -> Result<Option<String>, CronError> {
    let file_path = cron_skill_file_path(data_dir, job_id)?;
    match fs::read_to_string(file_path).await {
        Ok(content) => Ok(Some(content)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(CronError::InvalidSkillContent(err.to_string())),
    }
}

pub async fn has_skill_file(data_dir: &Path, job_id: &str) -> Result<bool, CronError> {
    let file_path = cron_skill_file_path(data_dir, job_id)?;
    match fs::metadata(file_path).await {
        Ok(metadata) => Ok(metadata.is_file()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(CronError::InvalidSkillContent(err.to_string())),
    }
}

pub async fn delete_skill_file(data_dir: &Path, job_id: &str) -> Result<(), CronError> {
    let dir = cron_skill_dir(data_dir, job_id)?;
    match fs::remove_dir_all(dir).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(CronError::InvalidSkillContent(err.to_string())),
    }
}

fn validate_job_id(job_id: &str) -> Result<(), CronError> {
    CronJobId::parse(job_id)
        .map(|_| ())
        .map_err(|error| CronError::InvalidSkillContent(format!("invalid cron job id: {error}")))
}

fn parse_frontmatter(content: &str) -> Result<(String, String, String), CronError> {
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines = normalized.lines();
    if lines.next() != Some("---") {
        return Err(CronError::InvalidSkillContent(
            "skill file must start with YAML frontmatter".into(),
        ));
    }

    let mut frontmatter = Vec::new();
    let mut found_end = false;
    for line in &mut lines {
        if line == "---" {
            found_end = true;
            break;
        }
        frontmatter.push(line);
    }
    if !found_end {
        return Err(CronError::InvalidSkillContent(
            "skill file is missing the closing frontmatter delimiter".into(),
        ));
    }

    let name = frontmatter
        .iter()
        .find_map(|line| line.strip_prefix("name:"))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| CronError::InvalidSkillContent("missing skill name".into()))?
        .to_owned();
    let description = frontmatter
        .iter()
        .find_map(|line| line.strip_prefix("description:"))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| CronError::InvalidSkillContent("missing skill description".into()))?
        .to_owned();

    let mut body_lines: Vec<&str> = lines.collect();
    while matches!(body_lines.first(), Some(line) if line.is_empty()) {
        body_lines.remove(0);
    }
    let body = body_lines.join("\n");
    Ok((name, description, body))
}

fn extract_prompt_from_body(body: &str) -> String {
    let instructions_idx = match body.find("## Instructions") {
        Some(idx) => idx,
        None => return body.trim_end().to_owned(),
    };

    let after_heading = &body[instructions_idx..];
    let lines: Vec<&str> = after_heading.split('\n').collect();
    let mut start_idx = lines.len();
    for (idx, line) in lines.iter().enumerate().skip(1) {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("You are executing") || trimmed.starts_with("Do NOT ask") {
            continue;
        }
        start_idx = idx;
        break;
    }

    if start_idx >= lines.len() {
        return String::new();
    }

    lines[start_idx..].join("\n").trim_end().to_owned()
}

fn is_placeholder(value: &str, patterns: &[&str]) -> bool {
    let normalized = value.trim().to_ascii_lowercase();
    patterns.iter().any(|pattern| normalized.starts_with(pattern))
}

fn normalize_for_hash(content: &str) -> String {
    content.replace("\r\n", "\n").replace('\r', "\n").trim().to_owned()
}
