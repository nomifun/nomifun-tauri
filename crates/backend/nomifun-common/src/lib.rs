//! Shared primitives: error types, enums, ID generation, crypto, timestamps, and pagination.
pub mod channel;
pub mod constants;

mod case_convert;
mod crypto;
pub mod dir_config;
mod enums;
mod error;
pub mod factory_reset;
mod fsname;
mod hooks;
mod id;
mod pagination;
pub mod provider_usage;
mod timestamp;
mod types;
pub mod vision_registry;

pub use case_convert::{camel_to_snake, normalize_keys_to_snake_case};
pub use crypto::{decrypt_string, encrypt_string};
pub use enums::{
    AgentKillReason, AgentType, ConversationSource, ConversationStatus, FileChangeOperation, McpServerStatus,
    McpSource, MessagePosition, MessageStatus, MessageType, PreviewContentType, ProtocolType, RemoteAgentAuthType,
    RemoteAgentProtocol, RemoteAgentStatus,
};
pub use error::{AppError, ErrorChain, workspace_path_has_edge_whitespace_segment};
pub use fsname::sanitize_dir_segment;
pub use hooks::{OnConversationDelete, OnTerminalDelete, RequirementCreator};
pub use id::{generate_id, generate_prefixed_id};
pub use pagination::PaginatedResult;
pub use provider_usage::{ProviderInUseDetails, ProviderUsage, ProviderUsageFeature};
pub use timestamp::{TimestampMs, now_ms};
pub use types::{CommandSpec, Confirmation, ConfirmationOption, EnvVar, ProviderWithModel};
pub use vision_registry::VisionUnsupportedRegistry;
