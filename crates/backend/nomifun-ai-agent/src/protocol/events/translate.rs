use agent_client_protocol::schema::{
    ContentBlock, PermissionOption, PermissionOptionKind as SdkPermissionOptionKind, RequestPermissionRequest,
    SessionNotification, SessionUpdate, ToolCallContent as SdkToolCallContent, ToolCallLocation as SdkToolCallLocation,
    ToolCallStatus as SdkToolCallStatus, ToolCallUpdate as SdkToolCallUpdate, ToolKind as SdkToolKind,
};
use tracing::debug;

use super::permission::{
    AcpPermissionEventData, AcpPermissionOptionData, AcpPermissionOptionKind, AcpPermissionRequestData,
    AcpPermissionToolCall,
};
use super::session_updates::{AvailableCommandsEventData, PlanEventData, ThinkingEventData};
use super::tool_call::{
    AcpToolCallContentItem, AcpToolCallEventData, AcpToolCallKind, AcpToolCallLocationItem,
    AcpToolCallSessionUpdateKind, AcpToolCallStatus, AcpToolCallTextBlock, AcpToolCallTextBlockType,
    AcpToolCallUpdateData,
};
use super::{AgentStreamEvent, TextEventData};

/// Convert an SDK [`SessionNotification`] into zero or more [`AgentStreamEvent`]s.
pub(crate) fn session_notification_to_events(notif: &SessionNotification) -> Vec<AgentStreamEvent> {
    let session_id = notif.session_id.to_string();
    let mut events = Vec::new();

    match &notif.update {
        SessionUpdate::AgentMessageChunk(chunk) => {
            if let ContentBlock::Text(text) = &chunk.content {
                events.push(AgentStreamEvent::Text(TextEventData {
                    content: text.text.clone(),
                }));
            }
        }

        SessionUpdate::AgentThoughtChunk(chunk) => {
            if let ContentBlock::Text(text) = &chunk.content {
                events.push(AgentStreamEvent::Thinking(ThinkingEventData {
                    content: text.text.clone(),
                    subject: None,
                    duration: None,
                    status: Some("in_progress".into()),
                }));
            }
        }

        SessionUpdate::UserMessageChunk(_chunk) => {}

        SessionUpdate::ToolCall(tc) => {
            events.push(AgentStreamEvent::AcpToolCall(AcpToolCallEventData {
                session_id,
                update: AcpToolCallUpdateData {
                    session_update: AcpToolCallSessionUpdateKind::ToolCall,
                    tool_call_id: tc.tool_call_id.to_string(),
                    status: Some(map_sdk_tool_status(&tc.status)),
                    title: Some(tc.title.clone()),
                    kind: Some(map_sdk_tool_kind(&tc.kind)),
                    raw_input: tc.raw_input.clone(),
                    raw_output: None,
                    content: map_tool_call_content(&tc.content),
                    locations: map_tool_call_locations(&tc.locations),
                },
                meta: tc.meta.clone(),
            }));
        }

        SessionUpdate::ToolCallUpdate(tcu) => {
            events.push(AgentStreamEvent::AcpToolCall(AcpToolCallEventData {
                session_id,
                update: AcpToolCallUpdateData {
                    session_update: AcpToolCallSessionUpdateKind::ToolCallUpdate,
                    tool_call_id: tcu.tool_call_id.to_string(),
                    status: tcu.fields.status.as_ref().map(map_sdk_tool_status),
                    title: tcu.fields.title.clone(),
                    kind: tcu.fields.kind.as_ref().map(map_sdk_tool_kind),
                    raw_input: tcu.fields.raw_input.clone(),
                    raw_output: tcu.fields.raw_output.clone(),
                    content: tcu
                        .fields
                        .content
                        .as_ref()
                        .and_then(|content| map_tool_call_content(content)),
                    locations: tcu
                        .fields
                        .locations
                        .as_ref()
                        .and_then(|locations| map_tool_call_locations(locations)),
                },
                meta: tcu.meta.clone(),
            }));
        }

        SessionUpdate::Plan(plan) => {
            let entries: Vec<serde_json::Value> = plan
                .entries
                .iter()
                .map(|e| serde_json::to_value(e).unwrap_or_default())
                .collect();

            events.push(AgentStreamEvent::Plan(PlanEventData {
                session_id: Some(session_id),
                source_call_id: None,
                entries,
            }));
        }

        SessionUpdate::AvailableCommandsUpdate(update) => {
            events.push(AgentStreamEvent::AvailableCommands(AvailableCommandsEventData {
                commands: update.available_commands.clone(),
            }));
        }

        SessionUpdate::CurrentModeUpdate(update) => {
            events.push(AgentStreamEvent::AcpModeInfo(
                serde_json::to_value(update).unwrap_or_default(),
            ));
        }

        SessionUpdate::ConfigOptionUpdate(update) => {
            events.push(AgentStreamEvent::AcpConfigOption(
                serde_json::to_value(update).unwrap_or_default(),
            ));
        }

        SessionUpdate::SessionInfoUpdate(update) => {
            events.push(AgentStreamEvent::AcpSessionInfo(
                serde_json::to_value(update).unwrap_or_default(),
            ));
        }

        SessionUpdate::UsageUpdate(update) => {
            events.push(AgentStreamEvent::AcpContextUsage(
                serde_json::to_value(update).unwrap_or_default(),
            ));
        }
        _ => {
            debug!("Unknown SessionUpdate variant received, skipping");
        }
    }

    events
}

pub(crate) fn permission_request_to_event_data(request: &RequestPermissionRequest) -> AcpPermissionEventData {
    AcpPermissionEventData::Request(AcpPermissionRequestData {
        session_id: request.session_id.to_string(),
        tool_call: map_permission_tool_call(&request.tool_call),
        options: request.options.iter().map(map_permission_option).collect(),
        meta: request.meta.clone(),
    })
}

fn map_sdk_tool_status(sdk: &SdkToolCallStatus) -> AcpToolCallStatus {
    match sdk {
        SdkToolCallStatus::Pending => AcpToolCallStatus::Pending,
        SdkToolCallStatus::InProgress => AcpToolCallStatus::InProgress,
        SdkToolCallStatus::Completed => AcpToolCallStatus::Completed,
        SdkToolCallStatus::Failed => AcpToolCallStatus::Failed,
        _ => AcpToolCallStatus::Pending,
    }
}

fn map_sdk_tool_kind(kind: &SdkToolKind) -> AcpToolCallKind {
    match kind {
        SdkToolKind::Read | SdkToolKind::Search => AcpToolCallKind::Read,
        SdkToolKind::Edit | SdkToolKind::Delete | SdkToolKind::Move => AcpToolCallKind::Edit,
        SdkToolKind::Execute
        | SdkToolKind::Think
        | SdkToolKind::Fetch
        | SdkToolKind::SwitchMode
        | SdkToolKind::Other
        | _ => AcpToolCallKind::Execute,
    }
}

fn map_sdk_permission_option_kind(kind: SdkPermissionOptionKind) -> AcpPermissionOptionKind {
    match kind {
        SdkPermissionOptionKind::AllowOnce => AcpPermissionOptionKind::AllowOnce,
        SdkPermissionOptionKind::AllowAlways => AcpPermissionOptionKind::AllowAlways,
        SdkPermissionOptionKind::RejectOnce => AcpPermissionOptionKind::RejectOnce,
        SdkPermissionOptionKind::RejectAlways => AcpPermissionOptionKind::RejectAlways,
        _ => AcpPermissionOptionKind::RejectOnce,
    }
}

fn map_permission_tool_call(tool_call: &SdkToolCallUpdate) -> AcpPermissionToolCall {
    AcpPermissionToolCall {
        tool_call_id: tool_call.tool_call_id.to_string(),
        status: tool_call.fields.status.as_ref().map(map_sdk_tool_status),
        title: tool_call.fields.title.clone(),
        kind: tool_call.fields.kind.as_ref().map(map_sdk_tool_kind),
        raw_input: tool_call.fields.raw_input.clone(),
        raw_output: tool_call.fields.raw_output.clone(),
        content: tool_call
            .fields
            .content
            .as_ref()
            .and_then(|content| map_tool_call_content(content)),
        locations: tool_call
            .fields
            .locations
            .as_ref()
            .and_then(|locations| map_tool_call_locations(locations)),
        meta: tool_call.meta.clone(),
    }
}

fn map_permission_option(option: &PermissionOption) -> AcpPermissionOptionData {
    AcpPermissionOptionData {
        option_id: option.option_id.to_string(),
        name: option.name.clone(),
        kind: map_sdk_permission_option_kind(option.kind),
        meta: option.meta.clone(),
    }
}

fn map_tool_call_content(content: &[SdkToolCallContent]) -> Option<Vec<AcpToolCallContentItem>> {
    let items: Vec<AcpToolCallContentItem> = content
        .iter()
        .filter_map(|item| match item {
            SdkToolCallContent::Content(content) => match &content.content {
                ContentBlock::Text(text) => Some(AcpToolCallContentItem::Content {
                    content: AcpToolCallTextBlock {
                        block_type: AcpToolCallTextBlockType::Text,
                        text: text.text.clone(),
                    },
                }),
                _ => None,
            },
            SdkToolCallContent::Diff(diff) => Some(AcpToolCallContentItem::Diff {
                path: diff.path.to_string_lossy().into_owned(),
                old_text: diff.old_text.clone(),
                new_text: diff.new_text.clone(),
            }),
            SdkToolCallContent::Terminal(_) => None,
            _ => None,
        })
        .collect();

    if items.is_empty() { None } else { Some(items) }
}

fn map_tool_call_locations(locations: &[SdkToolCallLocation]) -> Option<Vec<AcpToolCallLocationItem>> {
    (!locations.is_empty()).then(|| {
        locations
            .iter()
            .map(|loc| AcpToolCallLocationItem {
                path: loc.path.to_string_lossy().into_owned(),
            })
            .collect()
    })
}
