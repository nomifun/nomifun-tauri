// `ExecutionError::StartLost` intentionally carries the complete last-known
// process and cleanup evidence. Keeping that evidence inline makes loss
// reporting truthful and pattern matching stable across the public API; the
// error path is cold and the extra stack space is bounded.
#![allow(clippy::result_large_err)]

mod capability;
mod command_builder;
mod io;
mod outcome;
mod platform;
mod registry;
mod request;
mod supervisor;

pub use capability::{CapabilityPolicy, SandboxPolicy};
pub use command_builder::{CommandBuilder, kill_legacy_process_tree, merge_process_path};
pub use io::OutputBuffer;
pub use outcome::{
    CleanupReport, EncodingMetadata, ExecutionEvent, ExecutionOutcome, OutputChunk, OutputCursor,
    OutputSnapshot, OutputStream, ProcessSnapshot, ProcessState, SessionId, SpawnFailure,
};
pub use request::{
    CommandSpec, ExecutionError, ExecutionOwner, ExecutionPolicy, ExecutionRequest,
    NormalizedExecutionRequest, ShellKind, Transport, normalize_request,
};
pub use supervisor::{
    ExecutionHandle, PollResult, ProcessSupervisor, ShutdownReport, ShutdownSessionReport,
    SupervisorConfig,
};
