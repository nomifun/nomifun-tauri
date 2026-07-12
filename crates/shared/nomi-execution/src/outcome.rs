use std::{
    fmt,
    str::FromStr,
    time::{Duration, Instant},
};

use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionId(Uuid);

impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    pub const fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for SessionId {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(value).map(Self)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OutputCursor(u64);

impl OutputCursor {
    pub const START: Self = Self(0);

    pub const fn new(offset: u64) -> Self {
        Self(offset)
    }

    pub const fn offset(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OutputStream {
    Stdout,
    Stderr,
    Pty,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputChunk {
    pub seq: u64,
    pub start: u64,
    pub stream: OutputStream,
    pub bytes: Vec<u8>,
    pub text: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EncodingMetadata {
    pub source_encoding: String,
    pub decode_errors: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OutputSnapshot {
    pub chunks: Vec<OutputChunk>,
    pub next_cursor: OutputCursor,
    pub retained_bytes: usize,
    pub dropped_bytes: u64,
    pub encoding: EncodingMetadata,
}

impl OutputSnapshot {
    pub fn text(&self) -> String {
        self.chunks.iter().map(|chunk| chunk.text.as_str()).collect()
    }

    pub fn raw_bytes(&self) -> Vec<u8> {
        self.chunks
            .iter()
            .flat_map(|chunk| chunk.bytes.iter().copied())
            .collect()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CleanupReport {
    pub interrupt_attempted: bool,
    pub terminate_attempted: bool,
    pub force_kill_attempted: bool,
    pub reaped: bool,
    pub elapsed: Duration,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnFailure {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProcessState {
    Starting,
    Running,
    Exited,
    Cancelling,
    Cancelled,
    TimedOut,
    Lost,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessSnapshot {
    pub pid: u32,
    pub state: ProcessState,
    pub started_at: Instant,
    pub last_activity_at: Instant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecutionOutcome {
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
        output: OutputSnapshot,
        cleanup: CleanupReport,
    },
    SpawnFailed(SpawnFailure),
    Cancelled {
        output: OutputSnapshot,
        cleanup: CleanupReport,
    },
    TimedOut {
        output: OutputSnapshot,
        cleanup: CleanupReport,
    },
    Lost {
        last_known: ProcessSnapshot,
        output: OutputSnapshot,
        cleanup: CleanupReport,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecutionEvent {
    Output {
        seq: u64,
        stream: OutputStream,
        bytes: Vec<u8>,
        text: String,
        encoding: EncodingMetadata,
    },
    StateChanged {
        seq: u64,
        state: ProcessState,
    },
    OutputDropped {
        seq: u64,
        bytes: u64,
    },
}
