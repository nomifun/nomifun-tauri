use nomifun_common::{ConversationId, PreviewContentType, PreviewSnapshotId};
use serde::{Deserialize, Serialize};

/// Preview capabilities carry 256 bits of OS entropy and are encoded as
/// canonical lowercase hexadecimal for safe use as one URL path segment.
pub const PREVIEW_CAPABILITY_BYTES: usize = 32;
pub const PREVIEW_CAPABILITY_HEX_LEN: usize = PREVIEW_CAPABILITY_BYTES * 2;

pub fn is_preview_capability(value: &str) -> bool {
    value.len() == PREVIEW_CAPABILITY_HEX_LEN
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

// ---------------------------------------------------------------------------
// A. Preview requests
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartPreviewRequest {
    pub file_path: String,
    #[serde(default)]
    pub workspace: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StopPreviewRequest {
    pub capability: String,
}

// ---------------------------------------------------------------------------
// B. Preview responses & events
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PreviewUrlResponse {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PreviewState {
    Starting,
    Installing,
    Ready,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PreviewStatusEvent {
    pub state: PreviewState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

// ---------------------------------------------------------------------------
// C. Preview history target & snapshot info
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PreviewHistoryTargetDto {
    pub content_type: PreviewContentType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<ConversationId>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PreviewSnapshotInfoDto {
    pub snapshot_id: PreviewSnapshotId,
    pub label: String,
    pub created_at: i64,
    pub size: u64,
    pub content_type: PreviewContentType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
}

// ---------------------------------------------------------------------------
// D. Snapshot requests & responses
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaveSnapshotRequest {
    pub target: PreviewHistoryTargetDto,
    pub content: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListSnapshotsRequest {
    pub target: PreviewHistoryTargetDto,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetSnapshotContentRequest {
    pub target: PreviewHistoryTargetDto,
    pub snapshot_id: PreviewSnapshotId,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SnapshotContentResponse {
    pub snapshot: PreviewSnapshotInfoDto,
    pub content: String,
}

// ---------------------------------------------------------------------------
// E. Star Office detection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct DetectStarOfficeRequest {
    #[serde(default)]
    pub preferred_url: Option<String>,
    #[serde(default)]
    pub force: Option<bool>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StarOfficeDetectResponse {
    pub url: Option<String>,
}

// ---------------------------------------------------------------------------
// F. Document conversion
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConversionTarget {
    #[serde(rename = "markdown")]
    Markdown,
    #[serde(rename = "excel-json")]
    ExcelJson,
    #[serde(rename = "ppt-json")]
    PptJson,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentConversionRequest {
    pub file_path: String,
    pub to: ConversionTarget,
    #[serde(default)]
    pub workspace: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DocumentConversionResponse {
    pub to: String,
    pub result: ConversionResultDto,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConversionResultDto {
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// G. Excel conversion data model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExcelWorkbookData {
    pub sheets: Vec<ExcelSheetData>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExcelSheetData {
    pub name: String,
    pub data: Vec<Vec<serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merges: Option<Vec<CellRange>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<ExcelSheetImage>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CellRange {
    pub s: CellCoord,
    pub e: CellCoord,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CellCoord {
    pub r: usize,
    pub c: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExcelSheetImage {
    pub row: usize,
    pub col: usize,
    pub src: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alt: Option<String>,
}

// ---------------------------------------------------------------------------
// H. PPT conversion data model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PptJsonData {
    pub slides: Vec<PptSlideData>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PptSlideData {
    pub slide_number: usize,
    pub content: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- A. StartPreviewRequest / StopPreviewRequest --------------------------

    #[test]
    fn preview_capability_requires_canonical_256_bit_hex() {
        assert!(is_preview_capability(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        ));
        assert!(!is_preview_capability("43210"));
        assert!(!is_preview_capability(&"A".repeat(PREVIEW_CAPABILITY_HEX_LEN)));
        assert!(!is_preview_capability(&"0".repeat(PREVIEW_CAPABILITY_HEX_LEN - 1)));
    }

    #[test]
    fn start_preview_request_deserialize() {
        let raw = json!({"file_path": "/path/to/doc.docx", "workspace": "/tmp/ws"});
        let req: StartPreviewRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.file_path, "/path/to/doc.docx");
        assert_eq!(req.workspace.as_deref(), Some("/tmp/ws"));
    }

    #[test]
    fn start_preview_request_missing_file_path() {
        let raw = json!({});
        assert!(serde_json::from_value::<StartPreviewRequest>(raw).is_err());
    }

    #[test]
    fn stop_preview_request_deserialize() {
        let raw = json!({
            "capability": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        });
        let req: StopPreviewRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.capability.len(), 64);
    }

    #[test]
    fn stop_preview_request_requires_capability() {
        let raw = json!({});
        assert!(serde_json::from_value::<StopPreviewRequest>(raw).is_err());
    }

    #[test]
    fn start_preview_request_workspace_optional() {
        let raw = json!({"file_path": "/path/to/doc.docx"});
        let req: StartPreviewRequest = serde_json::from_value(raw).unwrap();
        assert!(req.workspace.is_none());
    }

    // -- B. PreviewUrlResponse ------------------------------------------------

    #[test]
    fn preview_url_response_success() {
        let resp = PreviewUrlResponse {
            url: "/api/office-watch-proxy/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef/".into(),
            capability: Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into()),
            error: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert!(json["url"].as_str().unwrap().ends_with('/'));
        assert_eq!(json["capability"].as_str().unwrap().len(), 64);
        assert!(json.get("error").is_none());
    }

    #[test]
    fn preview_url_response_error() {
        let resp = PreviewUrlResponse {
            url: String::new(),
            capability: None,
            error: Some("officecli not found".into()),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["url"], "");
        assert_eq!(json["error"], "officecli not found");
    }

    #[test]
    fn preview_url_response_roundtrip() {
        let resp = PreviewUrlResponse {
            url: "/api/ppt-proxy/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef/".into(),
            capability: Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into()),
            error: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: PreviewUrlResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, resp);
    }

    // -- B2. PreviewState / PreviewStatusEvent --------------------------------

    #[test]
    fn preview_state_serialize_all_variants() {
        let cases = [
            (PreviewState::Starting, "starting"),
            (PreviewState::Installing, "installing"),
            (PreviewState::Ready, "ready"),
            (PreviewState::Error, "error"),
        ];
        for (state, expected) in cases {
            let json = serde_json::to_value(state).unwrap();
            assert_eq!(json, expected);
        }
    }

    #[test]
    fn preview_state_deserialize_all_variants() {
        let cases = [
            ("starting", PreviewState::Starting),
            ("installing", PreviewState::Installing),
            ("ready", PreviewState::Ready),
            ("error", PreviewState::Error),
        ];
        for (input, expected) in cases {
            let parsed: PreviewState = serde_json::from_value(json!(input)).unwrap();
            assert_eq!(parsed, expected);
        }
    }

    #[test]
    fn preview_state_invalid() {
        assert!(serde_json::from_value::<PreviewState>(json!("unknown")).is_err());
    }

    #[test]
    fn preview_status_event_serialize() {
        let event = PreviewStatusEvent {
            state: PreviewState::Ready,
            message: None,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["state"], "ready");
        assert!(json.get("message").is_none());
    }

    #[test]
    fn preview_status_event_with_message() {
        let event = PreviewStatusEvent {
            state: PreviewState::Error,
            message: Some("port timeout".into()),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["state"], "error");
        assert_eq!(json["message"], "port timeout");
    }

    #[test]
    fn preview_status_event_roundtrip() {
        let event = PreviewStatusEvent {
            state: PreviewState::Installing,
            message: Some("installing officecli...".into()),
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: PreviewStatusEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, event);
    }

    // -- C. PreviewHistoryTargetDto -------------------------------------------

    #[test]
    fn target_dto_full_fields() {
        let conversation_id = "0190f5fe-7c00-7a00-8abc-012345678901";
        let raw = json!({
            "content_type": "markdown",
            "file_path": "/a.md",
            "workspace": "/ws",
            "file_name": "a.md",
            "title": "My Doc",
            "language": "rust",
            "conversation_id": conversation_id
        });
        let t: PreviewHistoryTargetDto = serde_json::from_value(raw).unwrap();
        assert_eq!(t.content_type, PreviewContentType::Markdown);
        assert_eq!(t.file_path.as_deref(), Some("/a.md"));
        assert_eq!(t.workspace.as_deref(), Some("/ws"));
        assert_eq!(t.file_name.as_deref(), Some("a.md"));
        assert_eq!(t.title.as_deref(), Some("My Doc"));
        assert_eq!(t.language.as_deref(), Some("rust"));
        assert_eq!(t.conversation_id.as_ref().map(|id| id.as_str()), Some(conversation_id));
    }

    #[test]
    fn target_dto_minimal() {
        let raw = json!({"content_type": "code"});
        let t: PreviewHistoryTargetDto = serde_json::from_value(raw).unwrap();
        assert_eq!(t.content_type, PreviewContentType::Code);
        assert!(t.file_path.is_none());
        assert!(t.workspace.is_none());
        assert!(t.file_name.is_none());
        assert!(t.title.is_none());
        assert!(t.language.is_none());
        assert!(t.conversation_id.is_none());
    }

    #[test]
    fn target_dto_missing_content_type() {
        let raw = json!({"file_path": "/a.md"});
        assert!(serde_json::from_value::<PreviewHistoryTargetDto>(raw).is_err());
    }

    #[test]
    fn target_dto_serialize_omits_none() {
        let t = PreviewHistoryTargetDto {
            content_type: PreviewContentType::Html,
            file_path: Some("/b.html".into()),
            workspace: None,
            file_name: None,
            title: None,
            language: None,
            conversation_id: None,
        };
        let json = serde_json::to_value(&t).unwrap();
        assert_eq!(json["content_type"], "html");
        assert_eq!(json["file_path"], "/b.html");
        assert!(json.get("workspace").is_none());
        assert!(json.get("file_name").is_none());
        assert!(json.get("title").is_none());
        assert!(json.get("language").is_none());
        assert!(json.get("conversation_id").is_none());
    }

    #[test]
    fn target_dto_roundtrip() {
        let conversation_id = ConversationId::try_from("0190f5fe-7c00-7a00-8abc-012345678904").unwrap();
        let t = PreviewHistoryTargetDto {
            content_type: PreviewContentType::Excel,
            file_path: Some("/sheet.xlsx".into()),
            workspace: Some("/ws".into()),
            file_name: Some("sheet.xlsx".into()),
            title: Some("Budget".into()),
            language: None,
            conversation_id: Some(conversation_id),
        };
        let json = serde_json::to_string(&t).unwrap();
        let parsed: PreviewHistoryTargetDto = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, t);
    }

    #[test]
    fn target_dto_all_content_types() {
        let types = [
            ("markdown", PreviewContentType::Markdown),
            ("diff", PreviewContentType::Diff),
            ("code", PreviewContentType::Code),
            ("html", PreviewContentType::Html),
            ("pdf", PreviewContentType::Pdf),
            ("ppt", PreviewContentType::Ppt),
            ("word", PreviewContentType::Word),
            ("excel", PreviewContentType::Excel),
            ("image", PreviewContentType::Image),
            ("url", PreviewContentType::Url),
        ];
        for (name, expected) in types {
            let raw = json!({"content_type": name});
            let t: PreviewHistoryTargetDto = serde_json::from_value(raw).unwrap();
            assert_eq!(t.content_type, expected);
        }
    }

    // -- C2. PreviewSnapshotInfoDto -------------------------------------------

    #[test]
    fn snapshot_info_serialize() {
        let info = PreviewSnapshotInfoDto {
            snapshot_id: PreviewSnapshotId::try_from(
                "0190f5fe-7c00-7a00-8000-000000000001",
            )
            .unwrap(),
            label: "2023-11-14 12:00".into(),
            created_at: 1700000000000,
            size: 1024,
            content_type: PreviewContentType::Markdown,
            file_name: Some("doc.md".into()),
            file_path: Some("/a/doc.md".into()),
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(
            json["snapshot_id"],
            "0190f5fe-7c00-7a00-8000-000000000001"
        );
        assert!(json.get("id").is_none());
        assert_eq!(json["label"], "2023-11-14 12:00");
        assert_eq!(json["created_at"], 1700000000000_i64);
        assert_eq!(json["size"], 1024);
        assert_eq!(json["content_type"], "markdown");
        assert_eq!(json["file_name"], "doc.md");
        assert_eq!(json["file_path"], "/a/doc.md");
    }

    #[test]
    fn snapshot_info_without_file_info() {
        let info = PreviewSnapshotInfoDto {
            snapshot_id: PreviewSnapshotId::try_from(
                "0190f5fe-7c00-7a00-8000-000000000002",
            )
            .unwrap(),
            label: "Snapshot 1".into(),
            created_at: 1000,
            size: 256,
            content_type: PreviewContentType::Code,
            file_name: None,
            file_path: None,
        };
        let json = serde_json::to_value(&info).unwrap();
        assert!(json.get("file_name").is_none());
        assert!(json.get("file_path").is_none());
    }

    #[test]
    fn snapshot_info_roundtrip() {
        let info = PreviewSnapshotInfoDto {
            snapshot_id: PreviewSnapshotId::try_from(
                "0190f5fe-7c00-7a00-8000-000000000003",
            )
            .unwrap(),
            label: "Label".into(),
            created_at: 2000,
            size: 512,
            content_type: PreviewContentType::Ppt,
            file_name: Some("slides.pptx".into()),
            file_path: None,
        };
        let json = serde_json::to_string(&info).unwrap();
        let parsed: PreviewSnapshotInfoDto = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, info);
    }

    #[test]
    fn snapshot_info_rejects_removed_generic_id() {
        let raw = json!({
            "id": "0190f5fe-7c00-7a00-8000-000000000003",
            "label": "Legacy",
            "created_at": 2000,
            "size": 512,
            "content_type": "ppt"
        });
        assert!(serde_json::from_value::<PreviewSnapshotInfoDto>(raw).is_err());
    }

    // -- D. Snapshot requests & responses -------------------------------------

    #[test]
    fn save_snapshot_request_deserialize() {
        let raw = json!({
            "target": {"content_type": "markdown", "file_path": "/a.md"},
            "content": "# Hello World"
        });
        let req: SaveSnapshotRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.target.content_type, PreviewContentType::Markdown);
        assert_eq!(req.content, "# Hello World");
    }

    #[test]
    fn save_snapshot_request_missing_content() {
        let raw = json!({"target": {"content_type": "markdown"}});
        assert!(serde_json::from_value::<SaveSnapshotRequest>(raw).is_err());
    }

    #[test]
    fn save_snapshot_request_missing_target() {
        let raw = json!({"content": "hello"});
        assert!(serde_json::from_value::<SaveSnapshotRequest>(raw).is_err());
    }

    #[test]
    fn list_snapshots_request_deserialize() {
        let raw = json!({"target": {"content_type": "html"}});
        let req: ListSnapshotsRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.target.content_type, PreviewContentType::Html);
    }

    #[test]
    fn get_snapshot_content_request_deserialize() {
        let raw = json!({
            "target": {"content_type": "code", "language": "rust"},
            "snapshot_id": "0190f5fe-7c00-7a00-8000-000000000006"
        });
        let req: GetSnapshotContentRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.target.language.as_deref(), Some("rust"));
        assert_eq!(
            req.snapshot_id.as_str(),
            "0190f5fe-7c00-7a00-8000-000000000006"
        );
    }

    #[test]
    fn get_snapshot_content_request_missing_snapshot_id() {
        let raw = json!({"target": {"content_type": "markdown"}});
        assert!(serde_json::from_value::<GetSnapshotContentRequest>(raw).is_err());
    }

    #[test]
    fn snapshot_content_response_serialize() {
        let resp = SnapshotContentResponse {
            snapshot: PreviewSnapshotInfoDto {
                snapshot_id: PreviewSnapshotId::try_from(
                    "0190f5fe-7c00-7a00-8000-000000000004",
                )
                .unwrap(),
                label: "L".into(),
                created_at: 1000,
                size: 5,
                content_type: PreviewContentType::Markdown,
                file_name: None,
                file_path: None,
            },
            content: "hello".into(),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(
            json["snapshot"]["snapshot_id"],
            "0190f5fe-7c00-7a00-8000-000000000004"
        );
        assert!(json["snapshot"].get("id").is_none());
        assert_eq!(json["content"], "hello");
    }

    #[test]
    fn snapshot_content_response_roundtrip() {
        let resp = SnapshotContentResponse {
            snapshot: PreviewSnapshotInfoDto {
                snapshot_id: PreviewSnapshotId::try_from(
                    "0190f5fe-7c00-7a00-8000-000000000005",
                )
                .unwrap(),
                label: "Lab".into(),
                created_at: 2000,
                size: 10,
                content_type: PreviewContentType::Word,
                file_name: Some("doc.docx".into()),
                file_path: Some("/path/doc.docx".into()),
            },
            content: "content here".into(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: SnapshotContentResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, resp);
    }

    // -- E. Star Office detection ---------------------------------------------

    #[test]
    fn detect_star_office_request_full() {
        let raw = json!({
            "preferred_url": "http://localhost:19000",
            "force": true,
            "timeout_ms": 2000
        });
        let req: DetectStarOfficeRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.preferred_url.as_deref(), Some("http://localhost:19000"));
        assert_eq!(req.force, Some(true));
        assert_eq!(req.timeout_ms, Some(2000));
    }

    #[test]
    fn detect_star_office_request_empty() {
        let raw = json!({});
        let req: DetectStarOfficeRequest = serde_json::from_value(raw).unwrap();
        assert!(req.preferred_url.is_none());
        assert!(req.force.is_none());
        assert!(req.timeout_ms.is_none());
    }

    #[test]
    fn star_office_detect_response_found() {
        let resp = StarOfficeDetectResponse {
            url: Some("http://localhost:19000".into()),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["url"], "http://localhost:19000");
    }

    #[test]
    fn star_office_detect_response_not_found() {
        let resp = StarOfficeDetectResponse { url: None };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["url"], serde_json::Value::Null);
    }

    #[test]
    fn star_office_detect_response_roundtrip() {
        let resp = StarOfficeDetectResponse {
            url: Some("http://localhost:18791".into()),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: StarOfficeDetectResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, resp);
    }

    // -- F. Document conversion -----------------------------------------------

    #[test]
    fn conversion_target_serialize() {
        let cases = [
            (ConversionTarget::Markdown, "markdown"),
            (ConversionTarget::ExcelJson, "excel-json"),
            (ConversionTarget::PptJson, "ppt-json"),
        ];
        for (target, expected) in cases {
            let json = serde_json::to_value(target).unwrap();
            assert_eq!(json, expected);
        }
    }

    #[test]
    fn conversion_target_deserialize() {
        let cases = [
            ("markdown", ConversionTarget::Markdown),
            ("excel-json", ConversionTarget::ExcelJson),
            ("ppt-json", ConversionTarget::PptJson),
        ];
        for (input, expected) in cases {
            let parsed: ConversionTarget = serde_json::from_value(json!(input)).unwrap();
            assert_eq!(parsed, expected);
        }
    }

    #[test]
    fn conversion_target_invalid() {
        assert!(serde_json::from_value::<ConversionTarget>(json!("invalid")).is_err());
    }

    #[test]
    fn document_conversion_request_deserialize() {
        let raw = json!({
            "file_path": "/sheet.xlsx",
            "to": "excel-json",
            "workspace": "/tmp/ws"
        });
        let req: DocumentConversionRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.file_path, "/sheet.xlsx");
        assert_eq!(req.to, ConversionTarget::ExcelJson);
        assert_eq!(req.workspace.as_deref(), Some("/tmp/ws"));
    }

    #[test]
    fn document_conversion_request_missing_to() {
        let raw = json!({"file_path": "/a.docx"});
        assert!(serde_json::from_value::<DocumentConversionRequest>(raw).is_err());
    }

    #[test]
    fn document_conversion_request_invalid_to() {
        let raw = json!({"file_path": "/a.docx", "to": "pdf"});
        assert!(serde_json::from_value::<DocumentConversionRequest>(raw).is_err());
    }

    #[test]
    fn document_conversion_request_workspace_optional() {
        let raw = json!({"file_path": "/sheet.xlsx", "to": "excel-json"});
        let req: DocumentConversionRequest = serde_json::from_value(raw).unwrap();
        assert!(req.workspace.is_none());
    }

    #[test]
    fn document_conversion_response_success() {
        let resp = DocumentConversionResponse {
            to: "excel-json".into(),
            result: ConversionResultDto {
                success: true,
                data: Some(json!({"sheets": []})),
                error: None,
            },
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["to"], "excel-json");
        assert_eq!(json["result"]["success"], true);
        assert!(json["result"].get("error").is_none());
    }

    #[test]
    fn document_conversion_response_failure() {
        let resp = DocumentConversionResponse {
            to: "markdown".into(),
            result: ConversionResultDto {
                success: false,
                data: None,
                error: Some("pandoc not installed".into()),
            },
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["result"]["success"], false);
        assert_eq!(json["result"]["error"], "pandoc not installed");
        assert!(json["result"].get("data").is_none());
    }

    #[test]
    fn document_conversion_response_roundtrip() {
        let resp = DocumentConversionResponse {
            to: "ppt-json".into(),
            result: ConversionResultDto {
                success: true,
                data: Some(json!({"slides": [{"slideNumber": 1, "content": {}}]})),
                error: None,
            },
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: DocumentConversionResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, resp);
    }

    // -- G. Excel data model --------------------------------------------------

    #[test]
    fn excel_workbook_data_serialize() {
        let wb = ExcelWorkbookData {
            sheets: vec![ExcelSheetData {
                name: "Sheet1".into(),
                data: vec![vec![json!("Name"), json!("Age")], vec![json!("Alice"), json!(30)]],
                merges: None,
                images: None,
            }],
        };
        let json = serde_json::to_value(&wb).unwrap();
        assert_eq!(json["sheets"][0]["name"], "Sheet1");
        assert_eq!(json["sheets"][0]["data"][0][0], "Name");
        assert_eq!(json["sheets"][0]["data"][1][1], 30);
        assert!(json["sheets"][0].get("merges").is_none());
        assert!(json["sheets"][0].get("images").is_none());
    }

    #[test]
    fn excel_sheet_with_merges() {
        let sheet = ExcelSheetData {
            name: "Merged".into(),
            data: vec![vec![json!("A")]],
            merges: Some(vec![CellRange {
                s: CellCoord { r: 0, c: 0 },
                e: CellCoord { r: 1, c: 2 },
            }]),
            images: None,
        };
        let json = serde_json::to_value(&sheet).unwrap();
        assert_eq!(json["merges"][0]["s"]["r"], 0);
        assert_eq!(json["merges"][0]["s"]["c"], 0);
        assert_eq!(json["merges"][0]["e"]["r"], 1);
        assert_eq!(json["merges"][0]["e"]["c"], 2);
    }

    #[test]
    fn excel_sheet_with_images() {
        let sheet = ExcelSheetData {
            name: "Images".into(),
            data: vec![],
            merges: None,
            images: Some(vec![ExcelSheetImage {
                row: 0,
                col: 1,
                src: "data:image/png;base64,abc".into(),
                width: Some(200),
                height: Some(100),
                alt: Some("logo".into()),
            }]),
        };
        let json = serde_json::to_value(&sheet).unwrap();
        let img = &json["images"][0];
        assert_eq!(img["row"], 0);
        assert_eq!(img["col"], 1);
        assert_eq!(img["src"], "data:image/png;base64,abc");
        assert_eq!(img["width"], 200);
        assert_eq!(img["height"], 100);
        assert_eq!(img["alt"], "logo");
    }

    #[test]
    fn excel_sheet_image_minimal() {
        let img = ExcelSheetImage {
            row: 5,
            col: 3,
            src: "data:image/jpeg;base64,xyz".into(),
            width: None,
            height: None,
            alt: None,
        };
        let json = serde_json::to_value(&img).unwrap();
        assert_eq!(json["row"], 5);
        assert_eq!(json["col"], 3);
        assert!(json.get("width").is_none());
        assert!(json.get("height").is_none());
        assert!(json.get("alt").is_none());
    }

    #[test]
    fn cell_range_roundtrip() {
        let range = CellRange {
            s: CellCoord { r: 0, c: 0 },
            e: CellCoord { r: 3, c: 5 },
        };
        let json = serde_json::to_string(&range).unwrap();
        let parsed: CellRange = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, range);
    }

    #[test]
    fn excel_workbook_roundtrip() {
        let wb = ExcelWorkbookData {
            sheets: vec![
                ExcelSheetData {
                    name: "S1".into(),
                    data: vec![vec![json!(1), json!(2)]],
                    merges: Some(vec![CellRange {
                        s: CellCoord { r: 0, c: 0 },
                        e: CellCoord { r: 0, c: 1 },
                    }]),
                    images: Some(vec![ExcelSheetImage {
                        row: 0,
                        col: 0,
                        src: "data:x".into(),
                        width: Some(50),
                        height: None,
                        alt: None,
                    }]),
                },
                ExcelSheetData {
                    name: "S2".into(),
                    data: vec![],
                    merges: None,
                    images: None,
                },
            ],
        };
        let json = serde_json::to_string(&wb).unwrap();
        let parsed: ExcelWorkbookData = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, wb);
    }

    // -- H. PPT data model ----------------------------------------------------

    #[test]
    fn ppt_json_data_serialize() {
        let ppt = PptJsonData {
            slides: vec![PptSlideData {
                slide_number: 1,
                content: json!({"title": "Intro"}),
            }],
            raw: None,
        };
        let json = serde_json::to_value(&ppt).unwrap();
        assert_eq!(json["slides"][0]["slide_number"], 1);
        assert_eq!(json["slides"][0]["content"]["title"], "Intro");
        assert!(json.get("raw").is_none());
    }

    #[test]
    fn ppt_json_data_with_raw() {
        let ppt = PptJsonData {
            slides: vec![],
            raw: Some(json!({"format": "pptx", "version": 1})),
        };
        let json = serde_json::to_value(&ppt).unwrap();
        assert_eq!(json["raw"]["format"], "pptx");
    }

    #[test]
    fn ppt_json_data_roundtrip() {
        let ppt = PptJsonData {
            slides: vec![
                PptSlideData {
                    slide_number: 1,
                    content: json!({"title": "A"}),
                },
                PptSlideData {
                    slide_number: 2,
                    content: json!({"body": "text"}),
                },
            ],
            raw: Some(json!({"meta": true})),
        };
        let json = serde_json::to_string(&ppt).unwrap();
        let parsed: PptJsonData = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, ppt);
    }

    #[test]
    fn ppt_slide_data_serialize() {
        let slide = PptSlideData {
            slide_number: 3,
            content: json!({"elements": []}),
        };
        let json = serde_json::to_value(&slide).unwrap();
        assert_eq!(json["slide_number"], 3);
    }
}
