use std::collections::HashMap;

use nomifun_common::TimestampMs;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// A. Schedule — tagged union with three variants
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind")]
pub enum CronScheduleDto {
    #[serde(rename = "at")]
    At {
        at_ms: TimestampMs,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
    #[serde(rename = "every")]
    Every {
        every_ms: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
    #[serde(rename = "cron")]
    Cron {
        expr: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tz: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
}

// ---------------------------------------------------------------------------
// B. Agent configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CronAgentConfigDto {
    /// ACP/agent backend only. Nomi provider selection is carried by
    /// `provider_id`; this field must be absent for Nomi jobs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_agent_id: Option<String>,
    /// Reusable configuration preset selected for this scheduled task. This is
    /// deliberately independent from `custom_agent_id`, which identifies an
    /// executable custom ACP/OpenClaw/Nanobot agent rather than a preset.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::serde_util::deserialize_optional_preset_id"
    )]
    pub preset_id: Option<String>,
    /// Frozen preset revision used when the task was saved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset_revision: Option<i64>,
    /// Frozen resolved preset payload. Scheduled execution must not silently
    /// drift when a reusable preset is edited later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset_snapshot: Option<crate::ResolvedPresetSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Natural model key. This is not an entity ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Logical reference to `providers.provider_id`, used only by Nomi jobs.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::serde_util::deserialize_optional_provider_id"
    )]
    pub provider_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_options: Option<HashMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// Clear the agent context before each scheduled run (only meaningful when
    /// the job reuses an existing conversation). Defaults to `false`.
    #[serde(default)]
    pub clear_context_each_run: bool,
}

// ---------------------------------------------------------------------------
// C. CronJob response — one Agent execution contract
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CronJobMetadataDto {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::serde_util::deserialize_optional_conversation_id"
    )]
    pub conversation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_title: Option<String>,
    pub agent_type: String,
    pub created_by: String,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_config: Option<CronAgentConfigDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CronJobStateDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_run_at_ms: Option<TimestampMs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_at_ms: Option<TimestampMs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub run_count: i64,
    pub retry_count: i64,
    pub max_retries: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CronJobResponse {
    #[serde(deserialize_with = "crate::serde_util::deserialize_cron_job_id")]
    pub cron_job_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub enabled: bool,
    pub schedule: CronScheduleDto,
    pub message: String,
    pub execution_mode: String,
    pub metadata: CronJobMetadataDto,
    pub state: CronJobStateDto,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CronJobRunResponse {
    #[serde(deserialize_with = "crate::serde_util::deserialize_cron_job_run_id")]
    pub cron_job_run_id: String,
    #[serde(deserialize_with = "crate::serde_util::deserialize_cron_job_id")]
    pub cron_job_id: String,
    pub executed_at_ms: TimestampMs,
    pub status: String,
}

// ---------------------------------------------------------------------------
// D. Create / Update request DTOs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateCronJobRequest {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub schedule: CronScheduleDto,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::serde_util::deserialize_optional_conversation_id"
    )]
    pub conversation_id: Option<String>,
    #[serde(default)]
    pub conversation_title: Option<String>,
    pub agent_type: String,
    pub created_by: String,
    #[serde(default)]
    pub execution_mode: Option<String>,
    #[serde(default)]
    pub agent_config: Option<CronAgentConfigDto>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateCronJobRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub schedule: Option<CronScheduleDto>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub agent_config: Option<CronAgentConfigDto>,
    #[serde(default)]
    pub conversation_title: Option<String>,
    #[serde(default)]
    pub max_retries: Option<i64>,
}

// ---------------------------------------------------------------------------
// E. Query parameters
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ListCronJobsQuery {
    #[serde(
        default,
        deserialize_with = "crate::serde_util::deserialize_optional_conversation_id"
    )]
    pub conversation_id: Option<String>,
}

// ---------------------------------------------------------------------------
// F. Other responses
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunNowResponse {
    #[serde(deserialize_with = "crate::serde_util::deserialize_conversation_id")]
    pub conversation_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaveCronSkillRequest {
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HasSkillResponse {
    pub has_skill: bool,
}

// ---------------------------------------------------------------------------
// G. Event payloads
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CronJobExecutedEvent {
    #[serde(deserialize_with = "crate::serde_util::deserialize_cron_job_id")]
    pub cron_job_id: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CronJobRemovedPayload {
    #[serde(deserialize_with = "crate::serde_util::deserialize_cron_job_id")]
    pub cron_job_id: String,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const JOB_ID: &str = "0190f5fe-7c00-7a00-8abc-012345678941";
    const JOB_ID_2: &str = "0190f5fe-7c00-7a00-8abc-012345678942";

    // -- A. CronScheduleDto ---------------------------------------------------

    #[test]
    fn schedule_at_serialize() {
        let s = CronScheduleDto::At {
            at_ms: 1700000000000,
            description: Some("Run at specific time".into()),
        };
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["kind"], "at");
        assert_eq!(json["at_ms"], 1700000000000_i64);
        assert_eq!(json["description"], "Run at specific time");
    }

    #[test]
    fn schedule_at_deserialize() {
        let raw = json!({"kind": "at", "at_ms": 1700000000000_i64, "description": "once"});
        let s: CronScheduleDto = serde_json::from_value(raw).unwrap();
        assert_eq!(
            s,
            CronScheduleDto::At {
                at_ms: 1700000000000,
                description: Some("once".into()),
            }
        );
    }

    #[test]
    fn schedule_at_without_description() {
        let raw = json!({"kind": "at", "at_ms": 1000});
        let s: CronScheduleDto = serde_json::from_value(raw).unwrap();
        assert_eq!(
            s,
            CronScheduleDto::At {
                at_ms: 1000,
                description: None,
            }
        );
        let json = serde_json::to_value(&s).unwrap();
        assert!(json.get("description").is_none());
    }

    #[test]
    fn schedule_every_serialize() {
        let s = CronScheduleDto::Every {
            every_ms: 60000,
            description: Some("Every minute".into()),
        };
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["kind"], "every");
        assert_eq!(json["every_ms"], 60000);
        assert_eq!(json["description"], "Every minute");
    }

    #[test]
    fn schedule_every_deserialize() {
        let raw = json!({"kind": "every", "every_ms": 300000});
        let s: CronScheduleDto = serde_json::from_value(raw).unwrap();
        assert_eq!(
            s,
            CronScheduleDto::Every {
                every_ms: 300000,
                description: None,
            }
        );
    }

    #[test]
    fn schedule_cron_serialize() {
        let s = CronScheduleDto::Cron {
            expr: "0 0 9 * * *".into(),
            tz: Some("Asia/Shanghai".into()),
            description: Some("Daily at 9am".into()),
        };
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["kind"], "cron");
        assert_eq!(json["expr"], "0 0 9 * * *");
        assert_eq!(json["tz"], "Asia/Shanghai");
        assert_eq!(json["description"], "Daily at 9am");
    }

    #[test]
    fn schedule_cron_without_tz() {
        let raw = json!({"kind": "cron", "expr": "0 */5 * * * *"});
        let s: CronScheduleDto = serde_json::from_value(raw).unwrap();
        assert_eq!(
            s,
            CronScheduleDto::Cron {
                expr: "0 */5 * * * *".into(),
                tz: None,
                description: None,
            }
        );
    }

    #[test]
    fn schedule_invalid_kind() {
        let raw = json!({"kind": "unknown", "value": 123});
        let result = serde_json::from_value::<CronScheduleDto>(raw);
        assert!(result.is_err());
    }

    #[test]
    fn schedule_at_missing_at_ms() {
        let raw = json!({"kind": "at"});
        let result = serde_json::from_value::<CronScheduleDto>(raw);
        assert!(result.is_err());
    }

    #[test]
    fn schedule_every_missing_every_ms() {
        let raw = json!({"kind": "every"});
        let result = serde_json::from_value::<CronScheduleDto>(raw);
        assert!(result.is_err());
    }

    #[test]
    fn schedule_cron_missing_expr() {
        let raw = json!({"kind": "cron"});
        let result = serde_json::from_value::<CronScheduleDto>(raw);
        assert!(result.is_err());
    }

    #[test]
    fn schedule_roundtrip_all_variants() {
        let variants = vec![
            CronScheduleDto::At {
                at_ms: 999,
                description: Some("once".into()),
            },
            CronScheduleDto::Every {
                every_ms: 5000,
                description: None,
            },
            CronScheduleDto::Cron {
                expr: "* * * * *".into(),
                tz: Some("UTC".into()),
                description: Some("every minute".into()),
            },
        ];
        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            let parsed: CronScheduleDto = serde_json::from_str(&json).unwrap();
            assert_eq!(&parsed, v);
        }
    }

    // -- B. CronAgentConfigDto ------------------------------------------------

    #[test]
    fn agent_config_full() {
        let raw = json!({
            "backend": "acp",
            "name": "Claude Agent",
            "cli_path": "/usr/bin/claude",
            "custom_agent_id": "agent-1",
            "preset_id": "0190f5fe-7c00-7a00-8000-000000000001",
            "preset_revision": 3,
            "mode": "auto",
            "model": "claude-sonnet-4-6",
            "config_options": {"key": "value"},
            "workspace": "/tmp/ws"
        });
        let c: CronAgentConfigDto = serde_json::from_value(raw).unwrap();
        assert_eq!(c.backend.as_deref(), Some("acp"));
        assert_eq!(c.name, "Claude Agent");
        assert_eq!(c.cli_path.as_deref(), Some("/usr/bin/claude"));
        assert_eq!(c.custom_agent_id.as_deref(), Some("agent-1"));
        assert_eq!(
            c.preset_id.as_deref(),
            Some("0190f5fe-7c00-7a00-8000-000000000001")
        );
        assert_eq!(c.preset_revision, Some(3));
        assert_eq!(c.model.as_deref(), Some("claude-sonnet-4-6"));
        assert!(c.provider_id.is_none());
        assert_eq!(c.config_options.as_ref().unwrap()["key"], "value");
    }

    #[test]
    fn preset_id_rejects_catalog_keys_and_prefixed_ids() {
        for preset_id in [
            "word-creator",
            "preset_7",
            "preset_0190f5fe-7c00-7a00-8abc-012345678901",
            " word-creator ",
        ] {
            assert!(
                serde_json::from_value::<CronAgentConfigDto>(json!({
                    "backend": "acp",
                    "name": "Claude",
                    "preset_id": preset_id
                }))
                .is_err()
            );
        }
    }

    #[test]
    fn agent_config_minimal() {
        let raw = json!({"backend": "openai", "name": "GPT"});
        let c: CronAgentConfigDto = serde_json::from_value(raw).unwrap();
        assert_eq!(c.backend.as_deref(), Some("openai"));
        assert_eq!(c.name, "GPT");
        assert!(c.cli_path.is_none());
        assert!(c.preset_id.is_none());
        assert!(c.config_options.is_none());
    }

    #[test]
    fn agent_config_serialize_omits_none() {
        let c = CronAgentConfigDto {
            backend: Some("acp".into()),
            name: "Test".into(),
            cli_path: None,
            custom_agent_id: None,
            preset_id: None,
            preset_revision: None,
            preset_snapshot: None,
            mode: None,
            model: None,
            provider_id: None,
            config_options: None,
            workspace: None,
            clear_context_each_run: false,
        };
        let json = serde_json::to_value(&c).unwrap();
        assert!(json.get("cli_path").is_none());
        assert!(json.get("preset_id").is_none());
        assert!(json.get("config_options").is_none());
    }

    #[test]
    fn agent_config_roundtrip() {
        let c = CronAgentConfigDto {
            backend: Some("acp".into()),
            name: "Agent".into(),
            cli_path: Some("/bin/x".into()),
            custom_agent_id: Some("c1".into()),
            preset_id: Some("0190f5fe-7c00-7a00-8000-000000000001".into()),
            preset_revision: Some(7),
            preset_snapshot: None,
            mode: Some("plan".into()),
            model: Some("m1".into()),
            provider_id: None,
            config_options: Some(HashMap::from([("a".into(), "b".into())])),
            workspace: Some("/ws".into()),
            clear_context_each_run: false,
        };
        let json = serde_json::to_string(&c).unwrap();
        let parsed: CronAgentConfigDto = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, c);
    }

    // -- C. CronJobResponse ---------------------------------------------------

    fn sample_cron_job_response() -> CronJobResponse {
        CronJobResponse {
            cron_job_id: JOB_ID.into(),
            name: "Daily report".into(),
            description: Some("Daily report description".into()),
            enabled: true,
            schedule: CronScheduleDto::Cron {
                expr: "0 0 9 * * *".into(),
                tz: Some("Asia/Shanghai".into()),
                description: Some("Daily at 9am".into()),
            },
            message: "Generate report".into(),
            execution_mode: "new_conversation".into(),
            metadata: CronJobMetadataDto {
                conversation_id: Some("0190f5fe-7c00-7a00-8abc-012345678901".into()),
                conversation_title: Some("Reports".into()),
                agent_type: "acp".into(),
                created_by: "user".into(),
                created_at: 1700000000000,
                updated_at: 1700001000000,
                agent_config: Some(CronAgentConfigDto {
                    backend: Some("acp".into()),
                    name: "Claude".into(),
                    cli_path: None,
                    custom_agent_id: None,
                    preset_id: None,
                    preset_revision: None,
                    preset_snapshot: None,
                    mode: None,
                    model: None,
                    provider_id: None,
                    config_options: None,
                    workspace: None,
                    clear_context_each_run: false,
                }),
            },
            state: CronJobStateDto {
                next_run_at_ms: Some(1700100000000),
                last_run_at_ms: Some(1700000000000),
                last_status: Some("ok".into()),
                last_error: None,
                run_count: 5,
                retry_count: 0,
                max_retries: 3,
            },
        }
    }

    #[test]
    fn cron_job_response_serialize_snake_case() {
        let resp = sample_cron_job_response();
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["cron_job_id"], JOB_ID);
        assert!(json.get("id").is_none());
        assert_eq!(json["name"], "Daily report");
        assert_eq!(json["enabled"], true);
        assert_eq!(json["schedule"]["kind"], "cron");
        assert_eq!(json["schedule"]["expr"], "0 0 9 * * *");
        assert_eq!(json["message"], "Generate report");
        assert_eq!(json["execution_mode"], "new_conversation");
        assert_eq!(json["metadata"]["conversation_id"], "0190f5fe-7c00-7a00-8abc-012345678901");
        assert_eq!(json["metadata"]["agent_type"], "acp");
        assert_eq!(json["metadata"]["created_by"], "user");
        assert_eq!(json["metadata"]["created_at"], 1700000000000_i64);
        assert_eq!(json["state"]["next_run_at_ms"], 1700100000000_i64);
        assert_eq!(json["state"]["last_status"], "ok");
        assert_eq!(json["state"]["run_count"], 5);
        assert_eq!(json["state"]["retry_count"], 0);
        assert_eq!(json["state"]["max_retries"], 3);
        assert!(json["state"].get("last_error").is_none());
    }

    #[test]
    fn cron_job_response_roundtrip() {
        let resp = sample_cron_job_response();
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: CronJobResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, resp);
    }

    #[test]
    fn cron_job_response_rejects_legacy_id_field() {
        let mut json = serde_json::to_value(sample_cron_job_response()).unwrap();
        let object = json.as_object_mut().unwrap();
        let cron_job_id = object.remove("cron_job_id").unwrap();
        object.insert("id".into(), cron_job_id);
        assert!(serde_json::from_value::<CronJobResponse>(json).is_err());
    }

    #[test]
    fn cron_job_response_minimal_state() {
        let resp = CronJobResponse {
            cron_job_id: JOB_ID_2.into(),
            name: "Ping".into(),
            description: None,
            enabled: false,
            schedule: CronScheduleDto::Every {
                every_ms: 60000,
                description: None,
            },
            message: "ping".into(),
            execution_mode: "existing".into(),
            metadata: CronJobMetadataDto {
                conversation_id: Some("0190f5fe-7c00-7a00-8abc-012345678902".into()),
                conversation_title: None,
                agent_type: "gemini".into(),
                created_by: "agent".into(),
                created_at: 1000,
                updated_at: 1000,
                agent_config: None,
            },
            state: CronJobStateDto {
                next_run_at_ms: None,
                last_run_at_ms: None,
                last_status: None,
                last_error: None,
                run_count: 0,
                retry_count: 0,
                max_retries: 3,
            },
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["execution_mode"], "existing");
        assert!(json["metadata"].get("conversation_title").is_none());
        assert!(json["metadata"].get("agent_config").is_none());
        assert!(json["state"].get("next_run_at_ms").is_none());
        assert!(json["state"].get("last_status").is_none());
    }

    // -- D. CreateCronJobRequest ----------------------------------------------

    #[test]
    fn create_request_full() {
        let raw = json!({
            "name": "Daily task",
            "schedule": {"kind": "cron", "expr": "0 0 9 * * *", "tz": "UTC"},
            "message": "Do the thing",
            "conversation_id": "0190f5fe-7c00-7a00-8abc-012345678901",
            "conversation_title": "Tasks",
            "agent_type": "acp",
            "created_by": "user",
            "execution_mode": "new_conversation",
            "agent_config": {"backend": "acp", "name": "Claude"}
        });
        let req: CreateCronJobRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.name, "Daily task");
        assert_eq!(req.message.as_deref(), Some("Do the thing"));
        assert_eq!(req.conversation_id.as_deref(), Some("0190f5fe-7c00-7a00-8abc-012345678901"));
        assert_eq!(req.agent_type, "acp");
        assert_eq!(req.created_by, "user");
        assert_eq!(req.execution_mode.as_deref(), Some("new_conversation"));
        assert!(req.agent_config.is_some());
    }

    #[test]
    fn create_request_minimal() {
        let raw = json!({
            "name": "Ping",
            "schedule": {"kind": "every", "every_ms": 60000},
            "conversation_id": "0190f5fe-7c00-7a00-8abc-012345678901",
            "agent_type": "acp",
            "created_by": "agent"
        });
        let req: CreateCronJobRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.name, "Ping");
        assert!(req.message.is_none());
        assert!(req.prompt.is_none());
        assert!(req.execution_mode.is_none());
        assert!(req.agent_config.is_none());
    }

    #[test]
    fn create_request_with_prompt() {
        let raw = json!({
            "name": "Task",
            "schedule": {"kind": "at", "at_ms": 1000},
            "prompt": "Do something",
            "conversation_id": "0190f5fe-7c00-7a00-8abc-012345678901",
            "agent_type": "gemini",
            "created_by": "user"
        });
        let req: CreateCronJobRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.prompt.as_deref(), Some("Do something"));
        assert!(req.message.is_none());
    }

    #[test]
    fn create_request_missing_name() {
        let raw = json!({
            "schedule": {"kind": "every", "every_ms": 1000},
            "conversation_id": "0190f5fe-7c00-7a00-8abc-012345678901",
            "agent_type": "acp",
            "created_by": "user"
        });
        assert!(serde_json::from_value::<CreateCronJobRequest>(raw).is_err());
    }

    #[test]
    fn create_request_missing_schedule() {
        let raw = json!({
            "name": "X",
            "conversation_id": "0190f5fe-7c00-7a00-8abc-012345678901",
            "agent_type": "acp",
            "created_by": "user"
        });
        assert!(serde_json::from_value::<CreateCronJobRequest>(raw).is_err());
    }

    #[test]
    fn create_request_missing_conversation_id() {
        let raw = json!({
            "name": "X",
            "schedule": {"kind": "every", "every_ms": 1000},
            "agent_type": "acp",
            "created_by": "user"
        });
        let req = serde_json::from_value::<CreateCronJobRequest>(raw).unwrap();
        assert!(req.conversation_id.is_none());
    }

    #[test]
    fn create_request_missing_agent_type() {
        let raw = json!({
            "name": "X",
            "schedule": {"kind": "every", "every_ms": 1000},
            "conversation_id": "0190f5fe-7c00-7a00-8abc-012345678901",
            "created_by": "user"
        });
        assert!(serde_json::from_value::<CreateCronJobRequest>(raw).is_err());
    }

    #[test]
    fn create_request_missing_created_by() {
        let raw = json!({
            "name": "X",
            "schedule": {"kind": "every", "every_ms": 1000},
            "conversation_id": "0190f5fe-7c00-7a00-8abc-012345678901",
            "agent_type": "acp"
        });
        assert!(serde_json::from_value::<CreateCronJobRequest>(raw).is_err());
    }

    #[test]
    fn create_request_rejects_removed_target_discriminator() {
        let raw = json!({
            "name": "legacy terminal task",
            "schedule": {"kind": "every", "every_ms": 1000},
            "conversation_id": "0190f5fe-7c00-7a00-8abc-012345678901",
            "agent_type": "nomi",
            "created_by": "user",
            "target_kind": "terminal"
        });
        assert!(serde_json::from_value::<CreateCronJobRequest>(raw).is_err());
    }

    #[test]
    fn create_request_rejects_noncanonical_conversation_ids() {
        let base = json!({
            "name": "invalid",
            "schedule": {"kind": "every", "every_ms": 1000},
            "conversation_id": "conv_7",
            "agent_type": "acp",
            "created_by": "user"
        });
        assert!(serde_json::from_value::<CreateCronJobRequest>(base).is_err());
    }

    // -- E. UpdateCronJobRequest ----------------------------------------------

    #[test]
    fn update_request_partial() {
        let raw = json!({"name": "New name", "enabled": false});
        let req: UpdateCronJobRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.name.as_deref(), Some("New name"));
        assert_eq!(req.enabled, Some(false));
        assert!(req.schedule.is_none());
        assert!(req.message.is_none());
        assert!(req.max_retries.is_none());
    }

    #[test]
    fn update_request_schedule_change() {
        let raw = json!({
            "schedule": {"kind": "cron", "expr": "0 */10 * * * *"}
        });
        let req: UpdateCronJobRequest = serde_json::from_value(raw).unwrap();
        assert!(req.schedule.is_some());
        assert!(req.name.is_none());
    }

    #[test]
    fn update_request_empty() {
        let raw = json!({});
        let req: UpdateCronJobRequest = serde_json::from_value(raw).unwrap();
        assert!(req.name.is_none());
        assert!(req.enabled.is_none());
        assert!(req.schedule.is_none());
        assert!(req.message.is_none());
        assert!(req.max_retries.is_none());
    }

    #[test]
    fn update_request_rejects_execution_mode_changes() {
        assert!(
            serde_json::from_value::<UpdateCronJobRequest>(json!({
                "execution_mode": "new_conversation"
            }))
            .is_err()
        );
    }

    #[test]
    fn update_request_with_max_retries() {
        let raw = json!({"max_retries": 5});
        let req: UpdateCronJobRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.max_retries, Some(5));
    }

    #[test]
    fn update_request_rejects_removed_target_discriminator() {
        assert!(
            serde_json::from_value::<UpdateCronJobRequest>(json!({
                "target_kind": "terminal"
            }))
            .is_err()
        );
    }

    // -- F. ListCronJobsQuery -------------------------------------------------

    #[test]
    fn list_query_with_conversation_id() {
        let raw = json!({"conversation_id": "0190f5fe-7c00-7a00-8abc-012345678901"});
        let q: ListCronJobsQuery = serde_json::from_value(raw).unwrap();
        assert_eq!(q.conversation_id.as_deref(), Some("0190f5fe-7c00-7a00-8abc-012345678901"));
    }

    #[test]
    fn list_query_empty() {
        let raw = json!({});
        let q: ListCronJobsQuery = serde_json::from_value(raw).unwrap();
        assert!(q.conversation_id.is_none());
    }

    #[test]
    fn list_query_rejects_numeric_and_malformed_conversation_ids() {
        assert!(
            serde_json::from_value::<ListCronJobsQuery>(json!({"conversation_id": 7}))
                .is_err()
        );
        assert!(
            serde_json::from_value::<ListCronJobsQuery>(json!({"conversation_id": "conv_7"}))
                .is_err()
        );
    }

    // -- G. RunNowResponse / HasSkillResponse / SaveCronSkillRequest ----------

    #[test]
    fn run_now_response_serialize() {
        let r = RunNowResponse {
            conversation_id: "0190f5fe-7c00-7a00-8abc-012345678901".into(),
        };
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["conversation_id"], "0190f5fe-7c00-7a00-8abc-012345678901");
    }

    #[test]
    fn run_now_response_roundtrip() {
        let r = RunNowResponse {
            conversation_id: "0190f5fe-7c00-7a00-8abc-012345678901".into(),
        };
        let json = serde_json::to_string(&r).unwrap();
        let parsed: RunNowResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, r);
    }

    #[test]
    fn has_skill_response_true() {
        let r = HasSkillResponse { has_skill: true };
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["has_skill"], true);
    }

    #[test]
    fn has_skill_response_false() {
        let r = HasSkillResponse { has_skill: false };
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["has_skill"], false);
    }

    #[test]
    fn has_skill_response_roundtrip() {
        let r = HasSkillResponse { has_skill: true };
        let json = serde_json::to_string(&r).unwrap();
        let parsed: HasSkillResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, r);
    }

    #[test]
    fn save_skill_request_deserialize() {
        let raw = json!({"content": "---\nname: test\n---\nDo something"});
        let req: SaveCronSkillRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.content, "---\nname: test\n---\nDo something");
    }

    #[test]
    fn save_skill_request_missing_content() {
        let raw = json!({});
        assert!(serde_json::from_value::<SaveCronSkillRequest>(raw).is_err());
    }

    // -- H. Event payloads ----------------------------------------------------

    #[test]
    fn executed_event_serialize() {
        let e = CronJobExecutedEvent {
            cron_job_id: JOB_ID.into(),
            status: "ok".into(),
            error: None,
        };
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(json["cron_job_id"], JOB_ID);
        assert!(json.get("job_id").is_none());
        assert_eq!(json["status"], "ok");
        assert!(json.get("error").is_none());
    }

    #[test]
    fn executed_event_with_error() {
        let e = CronJobExecutedEvent {
            cron_job_id: JOB_ID_2.into(),
            status: "error".into(),
            error: Some("timeout".into()),
        };
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(json["status"], "error");
        assert_eq!(json["error"], "timeout");
    }

    #[test]
    fn executed_event_roundtrip() {
        let e = CronJobExecutedEvent {
            cron_job_id: JOB_ID.into(),
            status: "skipped".into(),
            error: Some("busy".into()),
        };
        let json = serde_json::to_string(&e).unwrap();
        let parsed: CronJobExecutedEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, e);
    }

    #[test]
    fn removed_payload_serialize() {
        let p = CronJobRemovedPayload {
            cron_job_id: JOB_ID.into(),
        };
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["cron_job_id"], JOB_ID);
        assert!(json.get("job_id").is_none());
    }

    #[test]
    fn removed_payload_roundtrip() {
        let p = CronJobRemovedPayload {
            cron_job_id: JOB_ID_2.into(),
        };
        let json = serde_json::to_string(&p).unwrap();
        let parsed: CronJobRemovedPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, p);
    }

    #[test]
    fn cron_event_payloads_reject_legacy_job_id_field() {
        assert!(
            serde_json::from_value::<CronJobExecutedEvent>(json!({
                "job_id": JOB_ID,
                "status": "ok"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<CronJobRemovedPayload>(json!({
                "job_id": JOB_ID
            }))
            .is_err()
        );
    }

    #[test]
    fn cron_job_run_response_rejects_legacy_id_fields() {
        let response = CronJobRunResponse {
            cron_job_run_id: JOB_ID_2.into(),
            cron_job_id: JOB_ID.into(),
            executed_at_ms: 1_700_000_000_000,
            status: "ok".into(),
        };
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["cron_job_run_id"], JOB_ID_2);
        assert_eq!(json["cron_job_id"], JOB_ID);
        assert!(json.get("id").is_none());
        assert!(json.get("job_id").is_none());

        assert!(
            serde_json::from_value::<CronJobRunResponse>(json!({
                "id": JOB_ID_2,
                "job_id": JOB_ID,
                "executed_at_ms": 1_700_000_000_000_i64,
                "status": "ok"
            }))
            .is_err()
        );
    }
}
