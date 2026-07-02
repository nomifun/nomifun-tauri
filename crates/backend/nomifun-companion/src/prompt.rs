//! Prompt assembly + strict-JSON parsing for learning runs, plus the shared
//! persona flavor text (the companion-chat system prompt lives in
//! `companion::build_companion_system_prompt`).

use serde::Deserialize;

use crate::store::{MEMORY_KINDS, CompanionMemory, CompanionSuggestion};

pub const LEARN_MAX_TOKENS: u32 = 4096;

/// Valid moods the companion can be in (renderer maps each to an animation).
pub const MOODS: [&str; 5] = ["happy", "content", "sleepy", "worried", "excited"];

#[derive(Debug, Deserialize)]
pub struct LearnedMemory {
    pub kind: String,
    pub content: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "default_importance")]
    pub importance: f64,
}

fn default_importance() -> f64 {
    0.5
}

#[derive(Debug, Deserialize)]
pub struct LearnedSuggestion {
    pub kind: String,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub action: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct LearnOutput {
    #[serde(default)]
    pub memories: Vec<LearnedMemory>,
    #[serde(default)]
    pub reinforce_ids: Vec<String>,
    #[serde(default)]
    pub supersede_ids: Vec<String>,
    #[serde(default)]
    pub suggestions: Vec<LearnedSuggestion>,
    #[serde(default)]
    pub mood: Option<String>,
    #[serde(default)]
    pub diary: Option<String>,
}

pub const LEARN_SYSTEM: &str = r#"你是这台电脑上所有电子伙伴共享的记忆中枢管家。你的任务是阅读主人最近的工作事件记录，提炼出帮助伙伴们"更懂主人"的记忆，并产出对主人有实际帮助的建议。

记忆 kind 只能是：profile(画像,稳定事实) / preference(偏好,风格口味) / knowledge(知识,可复用结论) / episode(事件,带时间的经历) / task(任务线索,未完成事项或口头承诺) / affective(情感,情绪轨迹)。
建议 kind 只能是：guess_question(猜你想问) / create_skill(建议固化为技能) / create_cron(建议定时任务) / unfinished_task(未完成提醒) / insight(洞察) / wellness(健康关怀) / risk(风险提醒,如对话中疑似泄露密钥)。

规则：
1. 只提炼有信息量的内容，宁缺毋滥；每条记忆一句话、自包含、用中文。
2. 若新事件印证了"已有记忆"列表中的某条，把它的 id 放进 reinforce_ids，不要重复生成。
3. 若新事件与某条已有记忆矛盾，生成新记忆并把旧 id 放进 supersede_ids。
4. 建议最多 3 条，必须基于事件证据，不要空泛；可在 action 中给出跳转，格式 {"type":"navigate","to":"/路径"}。
5. mood 从 happy/content/sleepy/worried/excited 中选一个，代表伙伴们读完这些事件后的共同心情。
6. diary 是以伙伴们的第一人称写的一句话日记（中文、简短、温暖），措辞不要绑定任何单一角色名，如"今天主人修了一下午 bug，我们记住了他喜欢先看报错"。
7. 事件 data 中 origin 为 companion/cron/autowork/idmm、或 created_by 为 agent 的内容，是 agent 的自动行为而非主人发言：绝不能据此蒸馏出"主人想要/主人计划/主人提出"类记忆或建议。
8. 事件名 companion.user_message 是主人对伙伴说的话（高价值：偏好/意图/情感都值得提炼）；companion.reply 是伙伴自己说的话，只能用作上下文理解，绝不能当作主人的事实、意愿或承诺。
9. 若事件表明某个任务/需求已完成或不再需要，把"已有记忆"中对应的 task 记忆 id 放进 supersede_ids，不要为已完成的事保留或新建 task 记忆。
10. 不要产出与"已有建议"列表语义相同或高度相似的建议。

只输出一个 JSON 对象，不要任何其他文字、不要 markdown 代码围栏：
{"memories":[{"kind":"...","content":"...","tags":["..."],"importance":0.0~1.0}],"reinforce_ids":[],"supersede_ids":[],"suggestions":[{"kind":"...","title":"...","body":"...","action":null}],"mood":"content","diary":"..."}"#;

/// Build the learn user prompt from existing memories, pending (status='new')
/// suggestions and new events. Feeding the pending suggestions back lets the
/// model honor rule 10 (no semantically duplicate suggestions).
pub fn build_learn_prompt(
    memories: &[CompanionMemory],
    pending_suggestions: &[CompanionSuggestion],
    events_json: &[String],
    truncated: bool,
) -> String {
    let mut prompt = String::from("## 已有记忆（id | kind | 内容）\n");
    if memories.is_empty() {
        prompt.push_str("（暂无）\n");
    }
    for m in memories {
        prompt.push_str(&format!("- {} | {} | {}\n", m.id, m.kind, m.content));
    }
    prompt.push_str("\n## 已有建议（kind | 标题 — 不要重复产出语义相同的建议）\n");
    if pending_suggestions.is_empty() {
        prompt.push_str("（暂无）\n");
    }
    for s in pending_suggestions {
        prompt.push_str(&format!("- {} | {}\n", s.kind, s.title));
    }
    prompt.push_str("\n## 新事件记录（JSONL）\n");
    for line in events_json {
        prompt.push_str(line);
        prompt.push('\n');
    }
    if truncated {
        prompt.push_str("\n（注意：本批事件因数量限制被截断，还有更多事件等待下次学习。）\n");
    }
    prompt.push_str("\n请按系统指令输出 JSON。");
    prompt
}

/// Parse the model output into `LearnOutput`, tolerating ```json fences and
/// surrounding prose (extracts the outermost {...} block).
pub fn parse_learn_output(raw: &str) -> Result<LearnOutput, String> {
    let cleaned = extract_json_object(raw).ok_or_else(|| "no JSON object found in model output".to_owned())?;
    let mut output: LearnOutput = serde_json::from_str(cleaned).map_err(|e| format!("invalid learn JSON: {e}"))?;
    output.memories.retain(|m| MEMORY_KINDS.contains(&m.kind.as_str()) && !m.content.trim().is_empty());
    if let Some(mood) = &output.mood {
        if !MOODS.contains(&mood.as_str()) {
            output.mood = None;
        }
    }
    Ok(output)
}

/// Extract the outermost `{...}` from text that may contain fences or prose.
fn extract_json_object(raw: &str) -> Option<&str> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    if end <= start {
        return None;
    }
    Some(&raw[start..=end])
}

// ----- session-window archive digests (伙伴会话窗口归档) -----

pub const ARCHIVE_MAX_TOKENS: u32 = 2048;

/// Structured output of one session-window digest run.
#[derive(Debug, Deserialize)]
pub struct ArchiveOutput {
    /// One short narrative paragraph (中文) of what happened this session.
    pub summary: String,
    #[serde(default)]
    pub topics: Vec<String>,
    #[serde(default)]
    pub decisions: Vec<String>,
    #[serde(default)]
    pub todos: Vec<String>,
    #[serde(default)]
    pub mood: Option<String>,
}

impl ArchiveOutput {
    /// Serialize the structured fields (everything but the free-text summary)
    /// into the `highlights` JSON column. `None` when nothing structured.
    pub fn highlights_json(&self) -> Option<String> {
        if self.topics.is_empty() && self.decisions.is_empty() && self.todos.is_empty() && self.mood.is_none() {
            return None;
        }
        serde_json::to_string(&serde_json::json!({
            "topics": self.topics,
            "decisions": self.decisions,
            "todos": self.todos,
            "mood": self.mood,
        }))
        .ok()
    }
}

pub const ARCHIVE_SYSTEM: &str = r#"你是电子伙伴的"日记归档官"。给你一段伙伴与主人的会话记录（一个会话窗口），请把它压缩成一条按天存档的日记摘要，供伙伴日后回顾（比如主人问"去年今日我们聊了啥"）。

规则：
1. summary：用第一人称、中文、2~5 句，温暖而具体地概括这次会话主人做了什么、聊了什么、结论是什么。像伙伴写给自己的日记，不要流水账、不要逐条复述。
2. topics：3~6 个关键词/短语，便于检索。
3. decisions：本次会话中做出的明确决定或结论（没有就空数组）。
4. todos：本次会话遗留的未完成事项或后续计划（没有就空数组）。
5. mood：从 happy/content/sleepy/worried/excited 中选一个，代表伙伴这次会话的心情。
6. 只根据会话内容归纳，不要编造；主人未说的不要写进 decisions/todos。
7. 会话里 [伙伴] 是伙伴自己说的话，只作上下文；[用户] 是主人说的话，是事实来源。

只输出一个 JSON 对象，不要任何其他文字、不要 markdown 代码围栏：
{"summary":"...","topics":["..."],"decisions":["..."],"todos":["..."],"mood":"content"}"#;

/// Build the archive user prompt from a session window's role-tagged lines
/// (each already formatted as `[用户] ...` / `[伙伴] ...`). `day` is the
/// window's local start day (`YYYY-MM-DD`), surfaced so the model can date its
/// diary voice.
pub fn build_archive_prompt(day: &str, lines: &[String]) -> String {
    let mut prompt = format!("## 会话日期\n{day}\n\n## 会话记录\n");
    if lines.is_empty() {
        prompt.push_str("（空）\n");
    }
    for line in lines {
        prompt.push_str(line);
        prompt.push('\n');
    }
    prompt.push_str("\n请按系统指令输出 JSON 摘要。");
    prompt
}

/// Parse the model output into `ArchiveOutput`, tolerating ```json fences and
/// surrounding prose. Rejects empty summaries and normalizes an invalid mood to
/// `None`.
pub fn parse_archive_output(raw: &str) -> Result<ArchiveOutput, String> {
    let cleaned = extract_json_object(raw).ok_or_else(|| "no JSON object found in archive output".to_owned())?;
    let mut output: ArchiveOutput = serde_json::from_str(cleaned).map_err(|e| format!("invalid archive JSON: {e}"))?;
    if output.summary.trim().is_empty() {
        return Err("archive summary is empty".to_owned());
    }
    if let Some(mood) = &output.mood {
        if !MOODS.contains(&mood.as_str()) {
            output.mood = None;
        }
    }
    Ok(output)
}

pub(crate) fn persona_flavor(preset: &str) -> &'static str {
    match preset {
        "calm" => "你的性格沉稳温柔，像一位安静可靠的伙伴，说话简洁、不用太多语气词。",
        "sassy" => "你的性格机灵带点小毒舌，喜欢俏皮地调侃主人，但内心始终关心主人。",
        _ => "你的性格活泼粘人，喜欢用可爱的语气和颜文字，对主人的事情充满好奇。",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_plain_and_fenced_json() {
        let plain = r#"{"memories":[{"kind":"preference","content":"主人喜欢中文回复"}],"mood":"happy","diary":"今天学到了！"}"#;
        let out = parse_learn_output(plain).unwrap();
        assert_eq!(out.memories.len(), 1);
        assert_eq!(out.mood.as_deref(), Some("happy"));

        let fenced = format!("好的，这是结果：\n```json\n{plain}\n```\n以上。");
        let out = parse_learn_output(&fenced).unwrap();
        assert_eq!(out.memories.len(), 1);
    }

    #[test]
    fn parse_rejects_garbage_and_filters_bad_kinds() {
        assert!(parse_learn_output("我不知道").is_err());
        let bad_kind = r#"{"memories":[{"kind":"nonsense","content":"x"},{"kind":"task","content":"修 bug"}],"mood":"angry"}"#;
        let out = parse_learn_output(bad_kind).unwrap();
        assert_eq!(out.memories.len(), 1);
        assert_eq!(out.memories[0].kind, "task");
        assert!(out.mood.is_none());
    }

    #[test]
    fn learn_prompt_lists_pending_suggestions_and_system_has_loop_guards() {
        let suggestion = CompanionSuggestion {
            id: "sug_1".into(),
            kind: "create_cron".into(),
            title: "建议加个每日备份任务".into(),
            body: "…".into(),
            action: None,
            status: "new".into(),
            created_at: 0,
            decided_at: None,
        };
        let prompt = build_learn_prompt(&[], &[suggestion], &["{\"x\":1}".into()], false);
        assert!(prompt.contains("已有建议"));
        assert!(prompt.contains("create_cron | 建议加个每日备份任务"));
        assert!(prompt.contains("不要重复产出语义相同的建议"));
        // Empty lists render the placeholder.
        let empty = build_learn_prompt(&[], &[], &[], false);
        assert!(empty.contains("（暂无）"));
        // The system prompt carries the anti-loop rules.
        assert!(LEARN_SYSTEM.contains("companion/cron/autowork/idmm"));
        assert!(LEARN_SYSTEM.contains("companion.user_message"));
        assert!(LEARN_SYSTEM.contains("companion.reply"));
        assert!(LEARN_SYSTEM.contains("supersede_ids"));
    }

    #[test]
    fn parse_archive_output_plain_and_fenced() {
        let plain = r#"{"summary":"今天陪主人修了一下午 Rust 编译错误，最后定位到生命周期问题。","topics":["Rust","编译错误","生命周期"],"decisions":["改用 Arc 传递状态"],"todos":["明天补单元测试"],"mood":"content"}"#;
        let out = parse_archive_output(plain).unwrap();
        assert!(out.summary.contains("Rust"));
        assert_eq!(out.topics.len(), 3);
        assert_eq!(out.decisions.len(), 1);
        assert_eq!(out.todos.len(), 1);
        assert_eq!(out.mood.as_deref(), Some("content"));
        let hl = out.highlights_json().unwrap();
        assert!(hl.contains("生命周期"));

        let fenced = format!("好的：\n```json\n{plain}\n```");
        assert!(parse_archive_output(&fenced).is_ok());
    }

    #[test]
    fn parse_archive_output_rejects_empty_and_normalizes_mood() {
        assert!(parse_archive_output("我不会").is_err());
        assert!(parse_archive_output(r#"{"summary":"   "}"#).is_err(), "empty summary rejected");
        let bad_mood = r#"{"summary":"聊了会天","mood":"furious"}"#;
        let out = parse_archive_output(bad_mood).unwrap();
        assert!(out.mood.is_none(), "invalid mood normalized to None");
        // No structured fields → no highlights JSON.
        assert!(out.highlights_json().is_none());
    }

    #[test]
    fn build_archive_prompt_lists_lines_and_day() {
        let prompt = build_archive_prompt("2026-07-02", &["[用户] 帮我看看这个 bug".into(), "[伙伴] 好的~".into()]);
        assert!(prompt.contains("2026-07-02"));
        assert!(prompt.contains("[用户] 帮我看看这个 bug"));
        assert!(prompt.contains("[伙伴] 好的~"));
        assert!(ARCHIVE_SYSTEM.contains("去年今日"));
    }
}
