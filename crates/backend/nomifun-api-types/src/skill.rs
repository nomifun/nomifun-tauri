use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// A. Skill list & info
// ---------------------------------------------------------------------------

/// Origin of a listed skill — `builtin`, `custom`, or `extension`.
///
/// Matches the renderer contract in
/// `src/common/adapter/ipcBridge.ts::listAvailableSkills`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SkillSourceResponse {
    Builtin,
    Custom,
    Extension,
}

/// Single item in the available skills list (`GET /api/skills`).
///
/// For `source=builtin` entries, `location` is a synthesized absolute path
/// under `{data_dir}/builtin-skills-view/{name}/SKILL.md` (lazily
/// materialized from the embedded corpus so the export-symlink flow can
/// resolve it), and `relative_location` carries the path the frontend
/// passes back into `POST /api/skills/builtin-skill` (e.g.
/// `"auto-inject/cron/SKILL.md"` or `"{name}/SKILL.md"`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SkillListItemResponse {
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub name_i18n: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub description_i18n: HashMap<String, String>,
    pub location: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relative_location: Option<String>,
    pub is_custom: bool,
    pub source: SkillSourceResponse,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audience_tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scenario_tags: Vec<String>,
}

/// Request body for `PUT /api/skills/{name}/tags`.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct SetSkillTagsRequest {
    #[serde(default)]
    pub audience_tags: Vec<String>,
    #[serde(default)]
    pub scenario_tags: Vec<String>,
}

/// An auto-injected built-in skill (`GET /api/skills/builtin-auto`).
///
/// `location` is the relative path the frontend passes back into
/// `POST /api/skills/builtin-skill` (e.g. `"auto-inject/cron/SKILL.md"`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BuiltinAutoSkillResponse {
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub name_i18n: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub description_i18n: HashMap<String, String>,
    pub location: String,
}

/// Request body for `POST /api/skills/info`.
#[derive(Debug, Clone, Deserialize)]
pub struct ReadSkillInfoRequest {
    pub skill_path: String,
}

/// Response for `POST /api/skills/info`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReadSkillInfoResponse {
    pub name: String,
    pub description: String,
}

// ---------------------------------------------------------------------------
// B. Skill import / export / delete
// ---------------------------------------------------------------------------

/// Request body for `POST /api/skills/import` and `POST /api/skills/import-symlink`.
#[derive(Debug, Clone, Deserialize)]
pub struct ImportSkillRequest {
    pub skill_path: String,
}

/// Response for skill import operations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImportSkillResponse {
    pub skill_name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skill_names: Vec<String>,
}

/// Request body for `POST /api/skills/export-symlink`.
#[derive(Debug, Clone, Deserialize)]
pub struct ExportSkillRequest {
    pub skill_path: String,
    pub target_dir: String,
}

/// Request body for `DELETE /api/skills/:name` (path param, but also usable as body).
#[derive(Debug, Clone, Deserialize)]
pub struct DeleteSkillRequest {
    pub skill_name: String,
}

// ---------------------------------------------------------------------------
// C. Skill scanning & discovery
// ---------------------------------------------------------------------------

/// Request body for `POST /api/skills/scan`.
#[derive(Debug, Clone, Deserialize)]
pub struct ScanForSkillsRequest {
    pub folder_path: String,
}

/// A skill discovered by directory scanning.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScannedSkillResponse {
    pub name: String,
    pub description: String,
    pub path: String,
}

/// Response for `POST /api/skills/scan`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScanForSkillsResponse {
    pub skills: Vec<ScannedSkillResponse>,
}

/// An external skill source with count (`GET /api/skills/detect-external`).
///
/// `source` is a stable slug identifying the origin (e.g. `"claude"`,
/// `"gemini"`, `"agents"`, or `"custom-<abs-path>"` for user-added paths).
/// The renderer uses it as a React key and `data-testid` suffix in
/// `SkillsHubSettings.tsx`, so it must be unique across the returned list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExternalSkillSourceResponse {
    pub name: String,
    pub path: String,
    pub source: String,
    pub skill_count: usize,
    pub skills: Vec<ScannedSkillResponse>,
}

/// A named filesystem path (`GET /api/skills/detect-paths`, external paths).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NamedPathResponse {
    pub name: String,
    pub path: String,
}

/// Response for `GET /api/skills/paths`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SkillPathsResponse {
    pub user_skills_dir: String,
    pub builtin_skills_dir: String,
}

// ---------------------------------------------------------------------------
// D. Assistant rules & skills
// ---------------------------------------------------------------------------

/// Request body for `POST /api/skills/assistant-rule/read` and
/// `POST /api/skills/assistant-skill/read`.
#[derive(Debug, Clone, Deserialize)]
pub struct ReadAssistantRuleRequest {
    pub assistant_id: String,
    #[serde(default)]
    pub locale: Option<String>,
}

/// Request body for `POST /api/skills/assistant-rule/write` and
/// `POST /api/skills/assistant-skill/write`.
#[derive(Debug, Clone, Deserialize)]
pub struct WriteAssistantRuleRequest {
    pub assistant_id: String,
    pub content: String,
    #[serde(default)]
    pub locale: Option<String>,
}

/// Request body for `POST /api/skills/builtin-rule` and
/// `POST /api/skills/builtin-skill`.
#[derive(Debug, Clone, Deserialize)]
pub struct ReadBuiltinResourceRequest {
    pub file_name: String,
}

/// Request body for `POST /api/skills/materialize-for-agent`.
///
/// Callers pass the resolved skill snapshot (see
/// `conversation.extra.skills`). For backwards compatibility with pre-
/// snapshot clients that still emit `enabled_skills`, the alias below
/// accepts either spelling; remove the alias after the frontend PR lands.
#[derive(Debug, Clone, Deserialize)]
pub struct MaterializeSkillsRequest {
    pub conversation_id: i64,
    #[serde(default, alias = "enabled_skills")]
    pub skills: Vec<String>,
}

/// One entry in the `MaterializeSkillsResponse::skills` list.
///
/// Each entry tells the frontend the absolute on-disk directory of a
/// resolved skill. The frontend is expected to symlink that directory
/// into the agent CLI's native skills dir — the backend no longer
/// copies files per-conversation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MaterializedSkillRef {
    pub name: String,
    /// Absolute path on disk to the skill's source directory. May live
    /// under `{data_dir}/builtin-skills/` (top-level or `auto-inject/`)
    /// or `{data_dir}/skills/` (user-created skills).
    pub source_path: String,
}

/// Response for `POST /api/skills/materialize-for-agent`.
///
/// Returns a list of resolved skill references rather than a copied
/// directory; the frontend symlinks each `source_path` into the CLI's
/// native skills dir. Unknown names from the request are silently
/// omitted from the list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MaterializeSkillsResponse {
    pub skills: Vec<MaterializedSkillRef>,
}

// ---------------------------------------------------------------------------
// E. External path management
// ---------------------------------------------------------------------------

/// Request body for `POST /api/skills/external-paths`.
#[derive(Debug, Clone, Deserialize)]
pub struct AddExternalPathRequest {
    pub name: String,
    pub path: String,
}

/// Request body for `DELETE /api/skills/external-paths`.
#[derive(Debug, Clone, Deserialize)]
pub struct RemoveExternalPathRequest {
    pub path: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- Skill list --

    #[test]
    fn test_skill_list_item_serde() {
        let item = SkillListItemResponse {
            name: "my-skill".into(),
            description: "Does things".into(),
            location: "/home/user/.nomifun/skills/my-skill".into(),
            relative_location: None,
            is_custom: true,
            source: SkillSourceResponse::Custom,
            audience_tags: vec![],
            scenario_tags: vec![],
        };
        let json = serde_json::to_value(&item).unwrap();
        assert_eq!(json["name"], "my-skill");
        // Project-wide wire contract: field names are snake_case.
        assert_eq!(json["is_custom"], true);
        assert!(json.get("isCustom").is_none());
        assert_eq!(json["source"], "custom");
        // Absent for custom source — Option<String>::None is skipped.
        assert!(json.get("relative_location").is_none());
        assert!(json.get("relativeLocation").is_none());
    }

    #[test]
    fn test_skill_list_item_builtin_with_relative_location() {
        let item = SkillListItemResponse {
            name: "cron".into(),
            description: "Schedule recurring tasks".into(),
            location: "/home/user/.nomifun/builtin-skills-view/cron/SKILL.md".into(),
            relative_location: Some("auto-inject/cron/SKILL.md".into()),
            is_custom: false,
            source: SkillSourceResponse::Builtin,
            audience_tags: vec![],
            scenario_tags: vec![],
        };
        let json = serde_json::to_value(&item).unwrap();
        // Project-wide wire contract: relative_location stays snake_case.
        assert_eq!(json["relative_location"], "auto-inject/cron/SKILL.md");
        assert!(json.get("relativeLocation").is_none());
        assert_eq!(json["source"], "builtin");
    }

    #[test]
    fn test_skill_list_item_deserializes_snake_case() {
        // Frontend wire format → backend deserialization round-trip.
        let raw = json!({
            "name": "cron",
            "description": "Schedule",
            "location": "/tmp/view/cron/SKILL.md",
            "relative_location": "auto-inject/cron/SKILL.md",
            "is_custom": false,
            "source": "builtin",
        });
        let item: SkillListItemResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(item.name, "cron");
        assert!(!item.is_custom);
        assert_eq!(item.relative_location.as_deref(), Some("auto-inject/cron/SKILL.md"));
    }

    #[test]
    fn test_skill_tags_default_and_skip_empty() {
        let item = SkillListItemResponse {
            name: "x".into(),
            description: "d".into(),
            location: "/l".into(),
            relative_location: None,
            is_custom: true,
            source: SkillSourceResponse::Custom,
            audience_tags: vec![],
            scenario_tags: vec!["document".into()],
        };
        let j = serde_json::to_value(&item).unwrap();
        assert!(j.get("audience_tags").is_none()); // empty skipped
        assert_eq!(j["scenario_tags"], serde_json::json!(["document"]));
    }

    #[test]
    fn test_materialize_request_roundtrip() {
        let raw = json!({
            "conversation_id": 42,
            "skills": ["mermaid", "pdf"],
        });
        let req: MaterializeSkillsRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.conversation_id, 42);
        assert_eq!(req.skills, vec!["mermaid", "pdf"]);
    }

    #[test]
    fn test_materialize_request_accepts_legacy_enabled_skills_alias() {
        let raw = json!({
            "conversation_id": 42,
            "enabled_skills": ["pdf"],
        });
        let req: MaterializeSkillsRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.skills, vec!["pdf"]);
    }

    #[test]
    fn test_materialize_request_default_enabled() {
        let raw = json!({"conversation_id": 42});
        let req: MaterializeSkillsRequest = serde_json::from_value(raw).unwrap();
        assert!(req.skills.is_empty());
    }

    #[test]
    fn test_materialize_response_serializes_snake() {
        let resp = MaterializeSkillsResponse {
            skills: vec![
                MaterializedSkillRef {
                    name: "cron".into(),
                    source_path: "/tmp/builtin-skills/auto-inject/cron".into(),
                },
                MaterializedSkillRef {
                    name: "mermaid".into(),
                    source_path: "/tmp/builtin-skills/mermaid".into(),
                },
            ],
        };
        let json = serde_json::to_value(&resp).unwrap();
        let skills = json["skills"].as_array().unwrap();
        assert_eq!(skills.len(), 2);
        // Project-wide wire contract: snake_case fields on the wire.
        assert_eq!(skills[0]["name"], "cron");
        assert_eq!(skills[0]["source_path"], "/tmp/builtin-skills/auto-inject/cron");
        assert!(skills[0].get("sourcePath").is_none());
    }

    #[test]
    fn test_materialize_response_roundtrip() {
        let raw = json!({
            "skills": [
                {"name": "cron", "source_path": "/tmp/builtin-skills/auto-inject/cron"}
            ]
        });
        let resp: MaterializeSkillsResponse = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(resp.skills.len(), 1);
        assert_eq!(resp.skills[0].name, "cron");
        assert_eq!(resp.skills[0].source_path, "/tmp/builtin-skills/auto-inject/cron");
        assert_eq!(serde_json::to_value(&resp).unwrap(), raw);
    }

    #[test]
    fn test_skill_source_serializes_lowercase() {
        assert_eq!(
            serde_json::to_value(SkillSourceResponse::Builtin).unwrap(),
            serde_json::json!("builtin")
        );
        assert_eq!(
            serde_json::to_value(SkillSourceResponse::Custom).unwrap(),
            serde_json::json!("custom")
        );
        assert_eq!(
            serde_json::to_value(SkillSourceResponse::Extension).unwrap(),
            serde_json::json!("extension")
        );
    }

    #[test]
    fn test_read_skill_info_request() {
        // Project-wide wire contract: skill_path on the wire.
        let raw = json!({"skill_path": "/path/to/skill"});
        let req: ReadSkillInfoRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.skill_path, "/path/to/skill");
        // Legacy camelCase must now fail.
        let legacy = json!({"skillPath": "/path/to/skill"});
        assert!(serde_json::from_value::<ReadSkillInfoRequest>(legacy).is_err());
    }

    #[test]
    fn test_read_skill_info_response() {
        let resp = ReadSkillInfoResponse {
            name: "test".into(),
            description: "A test skill".into(),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["name"], "test");
        assert_eq!(json["description"], "A test skill");
    }

    // -- Import / Export --

    #[test]
    fn test_import_skill_request() {
        let raw = json!({"skill_path": "/external/skill"});
        let req: ImportSkillRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.skill_path, "/external/skill");
    }

    #[test]
    fn test_import_skill_response() {
        let resp = ImportSkillResponse {
            skill_name: "imported-skill".into(),
            skill_names: vec!["imported-skill".into(), "second-skill".into()],
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["skill_name"], "imported-skill");
        assert_eq!(json["skill_names"], json!(["imported-skill", "second-skill"]));
        assert!(json.get("skillName").is_none());
    }

    #[test]
    fn test_export_skill_request() {
        let raw = json!({"skill_path": "/user/skill", "target_dir": "/external/dir"});
        let req: ExportSkillRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.skill_path, "/user/skill");
        assert_eq!(req.target_dir, "/external/dir");
    }

    // -- Scanning --

    #[test]
    fn test_scan_for_skills_request() {
        let raw = json!({"folder_path": "/some/dir"});
        let req: ScanForSkillsRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.folder_path, "/some/dir");
    }

    #[test]
    fn test_scanned_skill_response() {
        let skill = ScannedSkillResponse {
            name: "found-skill".into(),
            description: "Found during scan".into(),
            path: "/dir/found-skill".into(),
        };
        let json = serde_json::to_value(&skill).unwrap();
        assert_eq!(json["name"], "found-skill");
        assert_eq!(json["path"], "/dir/found-skill");
    }

    #[test]
    fn test_external_skill_source_response() {
        let source = ExternalSkillSourceResponse {
            name: "Claude Skills".into(),
            path: "/home/user/.claude/skills".into(),
            source: "claude".into(),
            skill_count: 2,
            skills: vec![
                ScannedSkillResponse {
                    name: "s1".into(),
                    description: "d1".into(),
                    path: "/p1".into(),
                },
                ScannedSkillResponse {
                    name: "s2".into(),
                    description: "d2".into(),
                    path: "/p2".into(),
                },
            ],
        };
        let json = serde_json::to_value(&source).unwrap();
        // Project-wide wire contract: skill_count stays snake_case.
        assert_eq!(json["skill_count"], 2);
        assert!(json.get("skillCount").is_none());
        assert_eq!(json["skills"].as_array().unwrap().len(), 2);
        assert_eq!(json["source"], "claude");
    }

    #[test]
    fn test_external_skill_source_response_custom_source() {
        let source = ExternalSkillSourceResponse {
            name: "My Extras".into(),
            path: "/opt/extras".into(),
            source: "custom-/opt/extras".into(),
            skill_count: 0,
            skills: vec![],
        };
        let json = serde_json::to_value(&source).unwrap();
        assert_eq!(json["source"], "custom-/opt/extras");
        assert_eq!(json["name"], "My Extras");
    }

    #[test]
    fn test_external_skill_source_response_roundtrip() {
        let raw = json!({
            "name": "Gemini Skills",
            "path": "/home/user/.gemini/skills",
            "source": "gemini",
            "skill_count": 0,
            "skills": []
        });
        let parsed: ExternalSkillSourceResponse = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(parsed.source, "gemini");
        assert_eq!(parsed.name, "Gemini Skills");
        assert_eq!(parsed.skill_count, 0);
        let round = serde_json::to_value(&parsed).unwrap();
        assert_eq!(round, raw);
    }

    #[test]
    fn test_named_path_response() {
        let path = NamedPathResponse {
            name: "Claude Config".into(),
            path: "/home/user/.claude".into(),
        };
        let json = serde_json::to_value(&path).unwrap();
        assert_eq!(json["name"], "Claude Config");
        assert_eq!(json["path"], "/home/user/.claude");
    }

    #[test]
    fn test_skill_paths_response() {
        let resp = SkillPathsResponse {
            user_skills_dir: "/home/user/.nomifun/skills".into(),
            builtin_skills_dir: "/app/resources/skills".into(),
        };
        let json = serde_json::to_value(&resp).unwrap();
        // Project-wide wire contract: snake_case fields on the wire.
        assert_eq!(json["user_skills_dir"], "/home/user/.nomifun/skills");
        assert_eq!(json["builtin_skills_dir"], "/app/resources/skills");
        assert!(json.get("userSkillsDir").is_none());
        assert!(json.get("builtinSkillsDir").is_none());
    }

    // -- Assistant rules --

    #[test]
    fn test_read_assistant_rule_request_with_locale() {
        let raw = json!({"assistant_id": "abc123", "locale": "zh-CN"});
        let req: ReadAssistantRuleRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.assistant_id, "abc123");
        assert_eq!(req.locale.as_deref(), Some("zh-CN"));
    }

    #[test]
    fn test_read_assistant_rule_request_without_locale() {
        let raw = json!({"assistant_id": "abc123"});
        let req: ReadAssistantRuleRequest = serde_json::from_value(raw).unwrap();
        assert!(req.locale.is_none());
    }

    #[test]
    fn test_write_assistant_rule_request() {
        let raw = json!({
            "assistant_id": "abc123",
            "content": "# Rules\nBe helpful.",
            "locale": "en-US"
        });
        let req: WriteAssistantRuleRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.assistant_id, "abc123");
        assert_eq!(req.content, "# Rules\nBe helpful.");
        assert_eq!(req.locale.as_deref(), Some("en-US"));
    }

    #[test]
    fn test_read_builtin_resource_request() {
        // Project-wide wire contract: the frontend sends `file_name`.
        let raw = json!({"file_name": "code-review.md"});
        let req: ReadBuiltinResourceRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.file_name, "code-review.md");

        // Legacy camelCase now fails — matches project-wide wire contract.
        let legacy = json!({"fileName": "code-review.md"});
        assert!(serde_json::from_value::<ReadBuiltinResourceRequest>(legacy).is_err());
    }

    // -- External paths --

    #[test]
    fn test_add_external_path_request() {
        let raw = json!({"name": "My Skills", "path": "/path/to/skills"});
        let req: AddExternalPathRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.name, "My Skills");
        assert_eq!(req.path, "/path/to/skills");
    }

    #[test]
    fn test_remove_external_path_request() {
        let raw = json!({"path": "/path/to/skills"});
        let req: RemoveExternalPathRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.path, "/path/to/skills");
    }
}
