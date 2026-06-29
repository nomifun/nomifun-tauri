//! Native tools that give a companion-companion agent access to its memory store
//! through a `CompanionMemorySink` trait object. The backend (nomifun-companion)
//! injects a concrete sink; other hosts pass `None` and these are not
//! registered. Mirrors `requirement_tools.rs`.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use nomi_protocol::events::ToolCategory;
use nomi_tools::Tool;
use nomi_types::tool::{JsonSchema, ToolResult};

/// Memory kinds shared with the companion store taxonomy.
pub const COMPANION_MEMORY_KINDS: [&str; 6] = ["profile", "preference", "knowledge", "episode", "task", "affective"];

/// Backend seam for the companion's long-term memory + activity feed. Implemented
/// by `nomifun-companion` over its `CompanionStore`; `nomi-agent` only depends on this.
#[async_trait]
pub trait CompanionMemorySink: Send + Sync {
    /// Search memories by keyword (optionally by kind / incl. archived).
    /// `conversation_id` scopes the search to the owning companion (shared +
    /// its own private memories), so one companion never recalls another's
    /// private memories. Returns a human-readable digest the model can quote.
    async fn recall(&self, conversation_id: &str, query: &str, kind: Option<&str>, include_archived: bool) -> Result<String, String>;

    /// Persist one memory; implementations dedup. Returns a confirmation line.
    /// `conversation_id` identifies the session the save came from, so the
    /// backend can attribute per-companion rewards (XP) to the owning companion.
    async fn save(&self, conversation_id: &str, kind: &str, content: &str, tags: &[String]) -> Result<String, String>;

    /// Newest collected work events (already sanitized), newest-last digest.
    async fn recent_events(&self, limit: usize) -> Result<String, String>;
}

/// `recall_memories` — search the companion's full memory store.
pub struct RecallMemoriesTool {
    sink: Arc<dyn CompanionMemorySink>,
    /// The conversation this tool instance serves — passed to the sink so the
    /// backend can scope recall to the owning companion (shared + own private).
    conversation_id: String,
}

impl RecallMemoriesTool {
    pub fn new(sink: Arc<dyn CompanionMemorySink>, conversation_id: impl Into<String>) -> Self {
        Self {
            sink,
            conversation_id: conversation_id.into(),
        }
    }
}

#[async_trait]
impl Tool for RecallMemoriesTool {
    fn name(&self) -> &str {
        "recall_memories"
    }

    fn description(&self) -> &str {
        "搜索你对主人的全部长期记忆（包含未注入上下文的与已归档的）。当主人问起过去的事、\
         或你需要确认自己是否记得某事时使用。返回匹配的记忆列表。"
    }

    fn input_schema(&self) -> JsonSchema {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "关键词（按内容模糊匹配）"},
                "kind": {"type": "string", "enum": COMPANION_MEMORY_KINDS, "description": "可选：限定记忆类型"},
                "include_archived": {"type": "boolean", "description": "是否包含已归档记忆，默认 false"}
            },
            "required": ["query"]
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let query = input.get("query").and_then(|v| v.as_str()).unwrap_or("").trim();
        if query.is_empty() {
            return ToolResult {
                content: "query 不能为空".into(),
                is_error: true,
                images: Vec::new(),
            };
        }
        let kind = input
            .get("kind")
            .and_then(|v| v.as_str())
            .filter(|k| COMPANION_MEMORY_KINDS.contains(k));
        let include_archived = input.get("include_archived").and_then(|v| v.as_bool()).unwrap_or(false);
        match self.sink.recall(&self.conversation_id, query, kind, include_archived).await {
            Ok(out) => ToolResult {
                content: out,
                is_error: false,
                images: Vec::new(),
            },
            Err(e) => ToolResult {
                content: e,
                is_error: true,
                images: Vec::new(),
            },
        }
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Info
    }
}

/// `save_memory` — persist a long-term memory about the user.
pub struct SaveMemoryTool {
    sink: Arc<dyn CompanionMemorySink>,
    /// The conversation this tool instance serves — passed to the sink so the
    /// backend can attribute the save to the owning companion.
    conversation_id: String,
}

impl SaveMemoryTool {
    pub fn new(sink: Arc<dyn CompanionMemorySink>, conversation_id: impl Into<String>) -> Self {
        Self {
            sink,
            conversation_id: conversation_id.into(),
        }
    }
}

#[async_trait]
impl Tool for SaveMemoryTool {
    fn name(&self) -> &str {
        "save_memory"
    }

    fn description(&self) -> &str {
        "立即保存一条关于主人的长期记忆。当主人告诉你值得记住的事（偏好、约定、计划、\
         纠正你的认知）时使用；一句话自包含，宁缺毋滥。kind 取值：profile(稳定画像)/\
         preference(偏好)/knowledge(可复用结论)/episode(带时间的经历)/task(待办线索)/affective(情感)。"
    }

    fn input_schema(&self) -> JsonSchema {
        json!({
            "type": "object",
            "properties": {
                "kind": {"type": "string", "enum": COMPANION_MEMORY_KINDS},
                "content": {"type": "string", "description": "一句话记忆内容（中文，自包含）"},
                "tags": {"type": "array", "items": {"type": "string"}}
            },
            "required": ["kind", "content"]
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        false
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let kind = input.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        let content = input.get("content").and_then(|v| v.as_str()).unwrap_or("").trim();
        if !COMPANION_MEMORY_KINDS.contains(&kind) {
            return ToolResult {
                content: format!("kind 必须是 {COMPANION_MEMORY_KINDS:?} 之一"),
                is_error: true,
                images: Vec::new(),
            };
        }
        if content.is_empty() {
            return ToolResult {
                content: "content 不能为空".into(),
                is_error: true,
                images: Vec::new(),
            };
        }
        let tags: Vec<String> = input
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|t| t.as_str().map(str::to_owned)).collect())
            .unwrap_or_default();
        match self.sink.save(&self.conversation_id, kind, content, &tags).await {
            Ok(out) => ToolResult {
                content: out,
                is_error: false,
                images: Vec::new(),
            },
            Err(e) => ToolResult {
                content: e,
                is_error: true,
                images: Vec::new(),
            },
        }
    }

    fn category(&self) -> ToolCategory {
        // Writes only to the companion's own memory.db (never user files) — treat
        // as Info so default session mode doesn't gate it behind approval.
        ToolCategory::Info
    }
}

/// `list_recent_events` — peek at the user's recent collected work activity.
pub struct ListRecentEventsTool {
    sink: Arc<dyn CompanionMemorySink>,
}

impl ListRecentEventsTool {
    pub fn new(sink: Arc<dyn CompanionMemorySink>) -> Self {
        Self { sink }
    }
}

#[async_trait]
impl Tool for ListRecentEventsTool {
    fn name(&self) -> &str {
        "list_recent_events"
    }

    fn description(&self) -> &str {
        "查看最近采集到的主人工作事件（已脱敏摘要）。当主人问「我今天/最近都干了啥」\
         或你想结合实际活动给建议时使用。"
    }

    fn input_schema(&self) -> JsonSchema {
        json!({
            "type": "object",
            "properties": {
                "limit": {"type": "integer", "description": "最多返回多少条，默认 20，上限 50"}
            }
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let limit = input
            .get("limit")
            .and_then(|v| v.as_i64())
            .unwrap_or(20)
            .clamp(1, 50) as usize;
        match self.sink.recent_events(limit).await {
            Ok(out) => ToolResult {
                content: out,
                is_error: false,
                images: Vec::new(),
            },
            Err(e) => ToolResult {
                content: e,
                is_error: true,
                images: Vec::new(),
            },
        }
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Info
    }
}

// ---------------------------------------------------------------------------
// 自进化技能：自调用（design §7）
// ---------------------------------------------------------------------------

/// 一个可调用技能的精简描述，用于每轮的 when_to_use 索引注入。
#[derive(Debug, Clone)]
pub struct SkillListing {
    pub name: String,
    pub when_to_use: String,
}

/// Backend seam for the companion's self-evolved skills. Implemented by
/// `nomifun-companion` over its store + `skill_service`; `nomi-agent` only depends
/// on this (engine stays host-agnostic).
#[async_trait]
pub trait CompanionSkillSink: Send + Sync {
    /// This companion's currently-active skills (for per-turn `when_to_use` injection).
    /// Must be cheap — called once per turn.
    async fn active_skills(&self) -> Vec<SkillListing>;
    /// The SKILL.md body of a named active skill, or `None` if unknown.
    async fn load_skill_body(&self, name: &str) -> Option<String>;
}

/// `companion_skill` — invoke a learned skill by name to fetch its playbook.
pub struct CompanionSkillTool {
    sink: Arc<dyn CompanionSkillSink>,
}

impl CompanionSkillTool {
    pub fn new(sink: Arc<dyn CompanionSkillSink>) -> Self {
        Self { sink }
    }
}

#[async_trait]
impl Tool for CompanionSkillTool {
    fn name(&self) -> &str {
        "companion_skill"
    }

    fn description(&self) -> &str {
        "调用你已学会的某个技能，获取它的操作手册（步骤），然后照着执行。\
         当当前任务匹配系统提示里列出的某个技能的适用场景时使用。"
    }

    fn input_schema(&self) -> JsonSchema {
        json!({
            "type": "object",
            "properties": {
                "skill": {"type": "string", "description": "技能名（见系统提示里列出的可用技能）"}
            },
            "required": ["skill"]
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let name = input.get("skill").and_then(|v| v.as_str()).unwrap_or("").trim();
        if name.is_empty() {
            return ToolResult {
                content: "skill 不能为空".into(),
                is_error: true,
                images: Vec::new(),
            };
        }
        match self.sink.load_skill_body(name).await {
            Some(body) => ToolResult {
                content: body,
                is_error: false,
                images: Vec::new(),
            },
            None => ToolResult {
                content: format!("未找到技能：{name}"),
                is_error: true,
                images: Vec::new(),
            },
        }
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Info
    }
}

/// 每轮把该伙伴 active 技能的 `when_to_use` 索引注入系统提示（design §7）。
/// 空技能集 → `None`（no-op 快路，引擎据此每轮零成本跳过）。
pub struct CompanionSkillContributor {
    sink: Arc<dyn CompanionSkillSink>,
}

impl CompanionSkillContributor {
    pub fn new(sink: Arc<dyn CompanionSkillSink>) -> Self {
        Self { sink }
    }
}

#[async_trait]
impl crate::context_contributor::ContextContributor for CompanionSkillContributor {
    async fn pre_turn_context(&self) -> Option<String> {
        let skills = self.sink.active_skills().await;
        if skills.is_empty() {
            return None;
        }
        let mut s = String::from(
            "<system-reminder>\n你已经学会以下技能。遇到匹配场景时，用 companion_skill 工具按名调用以获取操作手册并照做：\n",
        );
        for sk in &skills {
            s.push_str(&format!("- {}: {}\n", sk.name, sk.when_to_use));
        }
        s.push_str("</system-reminder>");
        Some(s)
    }

    fn label(&self) -> &str {
        "companion_skills"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct RecordingSink {
        saved: Mutex<Vec<(String, String, String)>>,
    }

    #[async_trait]
    impl CompanionMemorySink for RecordingSink {
        async fn recall(&self, _conversation_id: &str, query: &str, kind: Option<&str>, _archived: bool) -> Result<String, String> {
            Ok(format!("hits for {query} kind={kind:?}"))
        }
        async fn save(&self, conversation_id: &str, kind: &str, content: &str, _tags: &[String]) -> Result<String, String> {
            self.saved
                .lock()
                .unwrap()
                .push((conversation_id.into(), kind.into(), content.into()));
            Ok("saved".into())
        }
        async fn recent_events(&self, limit: usize) -> Result<String, String> {
            Ok(format!("{limit} events"))
        }
    }

    fn sink() -> Arc<RecordingSink> {
        Arc::new(RecordingSink {
            saved: Mutex::new(vec![]),
        })
    }

    #[tokio::test]
    async fn recall_requires_query_and_filters_kind() {
        let tool = RecallMemoriesTool::new(sink(), "conv_t");
        let bad = tool.execute(json!({})).await;
        assert!(bad.is_error);
        let ok = tool.execute(json!({"query": "结论", "kind": "preference"})).await;
        assert!(!ok.is_error);
        assert!(ok.content.contains("preference"));
        // Invalid kind is dropped, not an error.
        let loose = tool.execute(json!({"query": "x", "kind": "bogus"})).await;
        assert!(!loose.is_error);
        assert!(loose.content.contains("None"));
    }

    #[tokio::test]
    async fn save_validates_kind_and_content() {
        let s = sink();
        let tool = SaveMemoryTool::new(s.clone(), "conv_t");
        assert!(tool.execute(json!({"kind": "bogus", "content": "x"})).await.is_error);
        assert!(tool.execute(json!({"kind": "task", "content": "  "})).await.is_error);
        let ok = tool.execute(json!({"kind": "task", "content": "明天修 bug"})).await;
        assert!(!ok.is_error);
        let saved = s.saved.lock().unwrap();
        assert_eq!(saved.len(), 1);
        // The tool stamps the conversation it serves into every save.
        assert_eq!(saved[0].0, "conv_t");
    }

    #[tokio::test]
    async fn recent_events_clamps_limit() {
        let tool = ListRecentEventsTool::new(sink());
        let out = tool.execute(json!({"limit": 9999})).await;
        assert_eq!(out.content, "50 events");
        let out = tool.execute(json!({})).await;
        assert_eq!(out.content, "20 events");
    }

    use crate::context_contributor::ContextContributor;

    struct FakeSkillSink {
        skills: Vec<SkillListing>,
    }
    #[async_trait]
    impl CompanionSkillSink for FakeSkillSink {
        async fn active_skills(&self) -> Vec<SkillListing> {
            self.skills.clone()
        }
        async fn load_skill_body(&self, name: &str) -> Option<String> {
            self.skills.iter().find(|s| s.name == name).map(|s| format!("# {}\nbody", s.name))
        }
    }

    #[tokio::test]
    async fn skill_contributor_is_noop_when_empty() {
        let sink = Arc::new(FakeSkillSink { skills: vec![] });
        let c = CompanionSkillContributor::new(sink);
        assert!(c.pre_turn_context().await.is_none());
    }

    #[tokio::test]
    async fn skill_contributor_lists_when_to_use() {
        let sink = Arc::new(FakeSkillSink {
            skills: vec![SkillListing { name: "weekly-report".into(), when_to_use: "周五出周报".into() }],
        });
        let c = CompanionSkillContributor::new(sink);
        let out = c.pre_turn_context().await.unwrap();
        assert!(out.contains("weekly-report"));
        assert!(out.contains("周五出周报"));
        assert!(out.contains("companion_skill"));
    }

    #[tokio::test]
    async fn skill_tool_returns_body_or_error() {
        let sink = Arc::new(FakeSkillSink {
            skills: vec![SkillListing { name: "fmt".into(), when_to_use: "x".into() }],
        });
        let tool = CompanionSkillTool::new(sink);
        assert!(tool.execute(json!({})).await.is_error);
        let ok = tool.execute(json!({"skill": "fmt"})).await;
        assert!(!ok.is_error);
        assert!(ok.content.contains("fmt"));
        assert!(tool.execute(json!({"skill": "nope"})).await.is_error);
    }
}
