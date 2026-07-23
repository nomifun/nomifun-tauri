use nomifun_api_types::Requirement;
use nomifun_common::AgentType;

use crate::attachments::PromptAttachment;

/// Whether a chat-style engine has the native `requirement_complete` /
/// `requirement_update_status` tools registered into its tool bus at session
/// build time.
///
/// This must mirror the runtime registration logic: only the Nomi factory
/// (`crates/backend/nomifun-ai-agent/src/factory/nomi.rs`) consumes the
/// `requirement_sink`, and only `NomiAgentManager` registers
/// `RequirementCompleteTool` / `RequirementUpdateStatusTool` on the engine.
/// Every other engine (ACP, Openclaw, Nanobot, Remote) ships without
/// them *in-process* — though ACP gains an equivalent declaration channel via
/// the injected requirement MCP server (see [`session_has_requirement_tools`]).
///
/// Keep this in lock-step with the registration site if engines ever change.
pub fn has_native_requirement_tools(agent_type: AgentType) -> bool {
    matches!(agent_type, AgentType::Nomi)
}

/// Whether *this session* exposes the requirement declaration tools
/// (`requirement_complete` / `requirement_update_status`) — and therefore the
/// platform should EXPECT an explicit verdict rather than assuming a clean turn
/// means success.
///
/// True when either:
/// - the engine registers them natively in-process (Nomi), or
/// - the requirement MCP server is injected for this ACP session
///   (`requirement_mcp_enabled`), giving claude/codex/gemini the same tools over
///   the stdio bridge.
///
/// `requirement_mcp_enabled` is a bootstrap-level flag (the requirement MCP
/// server started and its config was plumbed into the agent factory). Gating on
/// it — rather than on `agent_type` alone — guarantees the prompt only tells an
/// ACP agent to call `requirement_complete` when that tool actually exists in
/// the session. Otherwise the agent would try to call a missing tool and break
/// the turn, the exact failure the tool-free prompt was written to avoid.
pub fn session_has_requirement_tools(agent_type: AgentType, requirement_mcp_enabled: bool) -> bool {
    has_native_requirement_tools(agent_type) || (requirement_mcp_enabled && matches!(agent_type, AgentType::Acp))
}

/// Whether a terminal AutoWork turn should expect a structured verdict from the
/// agent (via the injected `requirement_complete` / `requirement_update_status`
/// MCP tools). True when the requirement MCP is enabled (Task 2 always injects
/// it into agent terminals), so a clean turn where the agent did NOT call those
/// tools → `needs_review` (not silently done).
///
/// Used by the AutoWork runner's terminal branch to set `expects_verdict = true`
/// when finalizing a terminal turn.
pub fn terminal_expects_verdict(requirement_mcp_enabled: bool) -> bool {
    requirement_mcp_enabled
}

/// Render the attachments section appended to every requirement prompt
/// variant. Empty input renders nothing. The model is explicitly told to view
/// the images with its file-reading tool BEFORE starting — this is the
/// path-plus-guidance contract (same pattern as the knowledge context builder).
fn render_attachments_section(attachments: &[PromptAttachment]) -> String {
    if attachments.is_empty() {
        return String::new();
    }
    let mut s = format!("\n## Requirement attachments ({} images)\n", attachments.len());
    for a in attachments {
        if a.missing {
            s.push_str(&format!(
                "- {} — (missing: the original file could not be found)\n",
                a.file_name
            ));
        } else {
            s.push_str(&format!("- {} — {}\n", a.file_name, a.path));
        }
    }
    s.push_str(
        "Before starting the work, view each attached image above with your file-reading tool — \
         they are part of the requirement description.\n",
    );
    s
}

/// Build the message injected into the agent for a claimed requirement.
/// Tells the agent exactly what to do and how to report completion. The agent
/// must NOT pick the next requirement — the platform hands it the next one.
///
/// The instruction text is session-aware: only sessions that actually expose
/// the `requirement_complete` / `requirement_update_status` tools (Nomi
/// natively, or ACP with the requirement MCP injected) are told to call those
/// tools. Every other session is given a tool-free contract — it just does the
/// work and ends the turn, and the platform records completion automatically
/// via `RequirementService::finalize_if_needed` on a clean Finish.
pub fn build_requirement_prompt(
    tag: &str,
    req: &Requirement,
    agent_type: AgentType,
    requirement_mcp_enabled: bool,
    attachments: &[PromptAttachment],
) -> String {
    if session_has_requirement_tools(agent_type, requirement_mcp_enabled) {
        build_requirement_prompt_with_native_tools(tag, req, attachments)
    } else {
        build_requirement_prompt_no_native_tools(tag, req, attachments)
    }
}

/// Native-tool variant: the engine has `requirement_complete` /
/// `requirement_update_status` registered, so we tell the model to call them.
fn build_requirement_prompt_with_native_tools(tag: &str, req: &Requirement, attachments: &[PromptAttachment]) -> String {
    format!(
        "[AutoWork] You are working through requirements in tag \"{tag}\".\n\n\
         ## Current requirement\n\
         id: {id}\n\
         title: {title}\n\
         order: {order}\n\n\
         {content}\n\
         {attachments_section}\n\
         ## When finished\n\
         - Call the `requirement_complete` tool with this requirement's id (\"{id}\") and a concise \
         completion note describing what you did.\n\
         - If you cannot complete it, call `requirement_update_status` with id \"{id}\", \
         status=\"failed\", and a reason.\n\
         Do not pick the next requirement yourself — the platform will hand you the next one.",
        tag = tag,
        id = req.requirement_id,
        title = req.title,
        order = req.order_key,
        content = req.content,
        attachments_section = render_attachments_section(attachments),
    )
}

/// Tool-free variant: the engine does NOT have the native requirement tools
/// registered. The model must NOT try to call `requirement_complete` —
/// invoking a tool the session does not expose just produces an apologetic
/// "我无法调用 requirement_complete" message and breaks the turn.
///
/// Instead the contract is simple: do the work, then end the turn with a
/// brief completion note in plain text. The platform records `done`
/// automatically when the turn finishes cleanly (see
/// `RequirementService::finalize_if_needed`); if the turn errors out the
/// platform retries / marks it failed on its own. To clearly signal an
/// inability to complete, the model is asked to surface the failure plainly
/// in its final message — humans reading the conversation see a real reason,
/// and downstream automation has unambiguous text to grep.
fn build_requirement_prompt_no_native_tools(tag: &str, req: &Requirement, attachments: &[PromptAttachment]) -> String {
    format!(
        "[AutoWork] You are working through requirements in tag \"{tag}\".\n\n\
         ## Current requirement\n\
         id: {id}\n\
         title: {title}\n\
         order: {order}\n\n\
         {content}\n\
         {attachments_section}\n\
         ## When finished\n\
         - Do the work, then end your turn with a brief plain-text completion note describing what \
         you did. This session has no requirement-management tools registered, so do NOT attempt \
         any tool call to record completion — the platform records it automatically when your turn \
         ends cleanly.\n\
         - If you cannot complete this requirement, end your turn with a plain-text message that \
         clearly states the failure and the reason (for example, start the final line with \
         \"Requirement failed:\" followed by the reason). Do not retry silently.\n\
         Do not pick the next requirement yourself — the platform will hand you the next one.",
        tag = tag,
        id = req.requirement_id,
        title = req.title,
        order = req.order_key,
        content = req.content,
        attachments_section = render_attachments_section(attachments),
    )
}

/// Build the message injected into a terminal CLI (claude/codex over a PTY)
/// for a claimed requirement.
///
/// The agent is instructed to declare completion via the `requirement_complete`
/// / `requirement_update_status` MCP tools (injected by Task 2 into every
/// AutoWork-enabled agent terminal). This mirrors the ACP `requirement_mcp_enabled`
/// branch of `build_requirement_prompt`: the tools ARE present, so the agent
/// SHOULD call them. A clean turn-end where the agent did NOT call them → the
/// platform parks the requirement as `needs_review` (not silently done).
pub fn build_terminal_requirement_prompt(
    tag: &str,
    req: &Requirement,
    attachments: &[PromptAttachment],
) -> String {
    format!(
        "[AutoWork] You are working through requirements in tag \"{tag}\". Complete ONLY the \
         requirement below.\n\n\
         ## Current requirement\n\
         id: {id}\n\
         title: {title}\n\
         order: {order}\n\n\
         {content}\n\
         {attachments_section}\n\
         ## When finished\n\
         - Call the `requirement_complete` tool with this requirement's id (\"{id}\") and a concise \
         completion note describing what you did.\n\
         - If you cannot complete it, call `requirement_update_status` with id \"{id}\", \
         status=\"failed\", and a reason.\n\
         Do not pick the next requirement yourself — the platform will hand you the next one.",
        tag = tag,
        id = req.requirement_id,
        title = req.title,
        order = req.order_key,
        content = req.content,
        attachments_section = render_attachments_section(attachments),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_api_types::RequirementStatus;

    const ATTACHMENT_REQ_ID: &str = "0190f5fe-7c00-7a00-8000-000000000001";

    fn req() -> Requirement {
        Requirement {
            requirement_id: ATTACHMENT_REQ_ID.into(),
            display_no: 1,
            title: "Do X".into(),
            content: "Detailed body".into(),
            tag: "t".into(),
            order_key: "1.2".into(),
            status: RequirementStatus::InProgress,
            completion_note: None,
            owner_conversation_id: None,
            owner_terminal_id: None,
            started_at: None,
            completed_at: None,
            attempt_count: 1,
            created_by: "user".into(),
            created_at: 0,
            updated_at: 0,
            attachments: vec![],
        }
    }

    fn atts() -> Vec<crate::attachments::PromptAttachment> {
        vec![
            crate::attachments::PromptAttachment {
                file_name: "设计稿.png".into(),
                path: format!(
                    "./.nomi/requirement-attachments/{ATTACHMENT_REQ_ID}/设计稿.png"
                ),
                missing: false,
            },
            crate::attachments::PromptAttachment {
                file_name: "gone.png".into(),
                path: String::new(),
                missing: true,
            },
        ]
    }

    #[test]
    fn attachments_section_lists_paths_and_missing_marker() {
        for at in [AgentType::Nomi, AgentType::Acp] {
            let p = build_requirement_prompt("t", &req(), at, false, &atts());
            assert!(p.contains("Requirement attachments"));
            assert!(p.contains(&format!(
                "./.nomi/requirement-attachments/{ATTACHMENT_REQ_ID}/设计稿.png"
            )));
            assert!(p.contains("设计稿.png"));
            assert!(p.contains("missing"), "vanished originals are flagged, not silently dropped");
            assert!(p.contains("view each attached image"), "must instruct the model to read the images");
        }
        let p = build_terminal_requirement_prompt("t", &req(), &atts());
        assert!(p.contains("Requirement attachments"));
        assert!(p.contains("设计稿.png"));
    }

    #[test]
    fn no_attachments_means_no_section() {
        let p = build_requirement_prompt("t", &req(), AgentType::Nomi, false, &[]);
        assert!(!p.contains("Requirement attachments"));
        let p = build_terminal_requirement_prompt("t", &req(), &[]);
        assert!(!p.contains("Requirement attachments"));
    }

    #[test]
    fn nomi_prompt_contains_id_and_native_tool_instructions() {
        let p = build_requirement_prompt("t", &req(), AgentType::Nomi, false, &[]);
        assert!(p.contains(&format!("id: {ATTACHMENT_REQ_ID}")));
        assert!(p.contains("Detailed body"));
        assert!(
            p.contains("requirement_complete"),
            "Nomi prompt MUST instruct calling requirement_complete (tool is registered for Nomi)"
        );
        assert!(
            p.contains("requirement_update_status"),
            "Nomi prompt MUST instruct calling requirement_update_status on failure"
        );
    }

    #[test]
    fn non_native_prompt_does_not_mention_requirement_complete_tool() {
        // Every non-Nomi engine WITHOUT the requirement MCP injected: no tool bus
        // entry for the native requirement tools, so the prompt must NOT tell the
        // model to call them.
        for at in [
            AgentType::Acp,
            AgentType::OpenclawGateway,
            AgentType::Nanobot,
            AgentType::Remote,
        ] {
            let p = build_requirement_prompt("t", &req(), at, false, &[]);
            assert!(
                p.contains(&format!("id: {ATTACHMENT_REQ_ID}")),
                "{at:?}: must still carry the requirement UUIDv7"
            );
            assert!(p.contains("Detailed body"), "{at:?}: must still carry the body");
            assert!(
                !p.contains("requirement_complete"),
                "{at:?}: prompt MUST NOT name the requirement_complete tool — it isn't registered for this engine"
            );
            assert!(
                !p.contains("requirement_update_status"),
                "{at:?}: prompt MUST NOT name the requirement_update_status tool — it isn't registered for this engine"
            );
            // It SHOULD describe the tool-free contract: end the turn with a note,
            // platform records completion automatically; failures are stated in plain text.
            assert!(
                p.contains("automatically") || p.contains("turn ends"),
                "{at:?}: prompt should describe the auto-finalize-on-clean-finish contract"
            );
            assert!(
                p.contains("Requirement failed:"),
                "{at:?}: prompt should tell the model how to surface a failure in plain text"
            );
        }
    }

    #[test]
    fn acp_with_requirement_mcp_uses_native_prompt() {
        // Once the requirement MCP is injected, an ACP session DOES expose the
        // declaration tools, so it must be told to call them (same contract as
        // Nomi). This is the soft-failure fix for ACP backends.
        let p = build_requirement_prompt("t", &req(), AgentType::Acp, true, &[]);
        assert!(p.contains(&format!("id: {ATTACHMENT_REQ_ID}")));
        assert!(
            p.contains("requirement_complete"),
            "ACP + requirement MCP MUST instruct calling requirement_complete"
        );
        assert!(
            p.contains("requirement_update_status"),
            "ACP + requirement MCP MUST instruct calling requirement_update_status on failure"
        );
    }

    #[test]
    fn acp_without_requirement_mcp_stays_tool_free() {
        let p = build_requirement_prompt("t", &req(), AgentType::Acp, false, &[]);
        assert!(
            !p.contains("requirement_complete"),
            "ACP without the requirement MCP must NOT be told to call a tool it does not have"
        );
    }

    #[test]
    fn session_has_requirement_tools_reflects_mcp_for_acp() {
        // Nomi always has them in-process, regardless of the MCP flag.
        assert!(session_has_requirement_tools(AgentType::Nomi, false));
        assert!(session_has_requirement_tools(AgentType::Nomi, true));
        // ACP only when the requirement MCP is enabled.
        assert!(!session_has_requirement_tools(AgentType::Acp, false));
        assert!(session_has_requirement_tools(AgentType::Acp, true));
        // Other engines never have them, even with the flag set.
        for at in [
            AgentType::OpenclawGateway,
            AgentType::Nanobot,
            AgentType::Remote,
        ] {
            assert!(!session_has_requirement_tools(at, true), "{at:?}: no requirement tools");
        }
    }

    #[test]
    fn has_native_requirement_tools_only_for_nomi() {
        assert!(has_native_requirement_tools(AgentType::Nomi));
        for at in [
            AgentType::Acp,
            AgentType::OpenclawGateway,
            AgentType::Nanobot,
            AgentType::Remote,
        ] {
            assert!(
                !has_native_requirement_tools(at),
                "{at:?}: the native requirement tools are NOT registered for this engine"
            );
        }
    }

    #[test]
    fn terminal_prompt_instructs_requirement_complete_and_has_no_knowledge_hint() {
        let p = build_terminal_requirement_prompt("t", &req(), &[]);
        assert!(p.contains(&format!("id: {ATTACHMENT_REQ_ID}")));
        assert!(p.contains("Detailed body"));
        // Must instruct the agent to call the requirement completion tools
        // (they are injected via the requirement MCP server — Task 2).
        assert!(
            p.contains("requirement_complete"),
            "terminal prompt MUST instruct calling requirement_complete"
        );
        assert!(
            p.contains("requirement_update_status"),
            "terminal prompt MUST instruct calling requirement_update_status on failure"
        );
        // The old knowledge hint is gone (knowledge is now a real MCP tool).
        assert!(
            !p.contains("knowledge"),
            "terminal prompt must NOT contain the old TERMINAL_KNOWLEDGE_HINT"
        );
        // The old printed-marker protocol is gone.
        assert!(!p.contains("NOMI_AUTOWORK_END"), "terminal prompt must not ask for a marker");
    }

    #[test]
    fn terminal_expects_verdict_mirrors_mcp_enabled_flag() {
        use crate::prompt::terminal_expects_verdict;
        assert!(terminal_expects_verdict(true));
        assert!(!terminal_expects_verdict(false));
    }
}
