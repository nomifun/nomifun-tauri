//! AI agent lifecycle, worker task dispatch, and skill management.
pub(crate) mod agent_runtime;
pub mod agent_task;
// P3-K2: rendering page-fetch backend for knowledge URL sources. Gated behind
// `browser-use` — the ONE bridge from the (agent-layer) browser engine into the
// knowledge `PageFetcher` trait, keeping the knowledge crate engine-free (②).
#[cfg(feature = "browser-use")]
pub mod browser_fetcher;
pub mod capability;
pub mod cc_switch;
pub mod factory;
pub(crate) mod idle_scanner;
pub mod knowledge_completer;
pub mod knowledge_retrieval;
pub mod knowledge_writeback;
pub mod manager;
pub(crate) mod persistence;
pub mod protocol;
pub mod registry;
pub mod routes;
pub(crate) mod services;
pub mod shared_kernel;
pub mod task_manager;
pub mod terminal_title_completer;
pub mod types;

// ── Agent-layer re-exports (the seam) ──────────────────────────────────────
// Backend crates reach the agent (nomi-*) layer ONLY through nomifun-ai-agent.
// When the agent layer is later extracted into its own repo, these re-exports
// become the single integration surface (see docs/specs/agent-extraction-checklist.md).
pub use nomi_agent::companion_tools::CompanionMemorySink;
pub use nomi_agent::companion_tools::{CompanionSkillSink, SkillListing};
pub use nomi_agent::cron_tools::{CronJobSummary, CronSink};
pub use nomi_agent::requirement_tools::RequirementSink;
pub use nomi_config;
pub use nomi_types;

pub use agent_runtime::AgentRuntime;
#[cfg(any(test, feature = "test-support"))]
pub use agent_task::IMockAgent;
pub use agent_task::{AgentInstance, IAgentTask};
pub use capability::skill_manager::{
    AcpSkillManager, SkillDefinition, SkillIndex, build_skills_index_text, build_system_instructions,
    build_system_instructions_with_skills_index, detect_skill_load_request, prepare_first_message,
    prepare_first_message_with_skills_index,
};
pub use factory::provider_config::{
    one_shot_completion, resolve_provider_config, streaming_completion, streaming_completion_kinded,
    streaming_completion_text_or_reasoning, user_message, DeltaKind,
};
pub use factory::{AgentFactoryDeps, CompanionPromptProvider, build_agent_factory};
pub use idle_scanner::start_idle_scanner;
#[cfg(feature = "browser-use")]
pub use browser_fetcher::BrowserFetcher;
pub use knowledge_completer::LiveKnowledgeCompleter;
pub use knowledge_retrieval::LiveKnowledgeRetrievalSink;
pub use knowledge_writeback::LiveKnowledgeWritebackSink;
pub use terminal_title_completer::LiveTerminalTitleCompleter;
pub use nomifun_api_types::{
    AcpBuildExtra, AcpModelInfo, NomiBuildExtra, OpenClawBuildExtra, OpenClawGatewayConfig, RemoteBuildExtra,
    SlashCommandItem,
};
pub use persistence::AcpSessionSyncService;
pub use protocol::events::{
    AcpPermissionEventData, AcpPermissionOptionKind, AcpToolCallKind, AgentStreamEvent, FinishEventData, TurnStopReason,
};
pub use protocol::send_error::AgentSendError;
pub use registry::{AgentRegistry, UnavailableReason};
pub use routes::{AgentRouterState, RemoteAgentRouterState, agent_routes, remote_agent_routes};
pub use services::AgentService;
pub use services::RemoteAgentService;
pub use task_manager::{IWorkerTaskManager, WorkerTaskManagerImpl};
