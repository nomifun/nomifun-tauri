//! Runtime capability modules shared across agent managers.
//!
//! These modules provide reusable primitives (CLI process supervision,
//! skill indexing, backend output/protocol sinks, and first-message injection)
//! that any agent implementation can compose.

pub(crate) mod backend_output_sink;
pub(crate) mod backend_protocol_sink;
pub(crate) mod cli_process;
pub(crate) mod first_message_injector;
pub mod model_identity_reminder;
pub mod prompt_pipeline;
pub(crate) mod skill_manager;

pub use prompt_pipeline::{PreSendHook, PromptCtx, PromptPipeline};
