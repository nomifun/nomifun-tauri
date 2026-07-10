# Agent Reliability Wave A — Cross-Platform Execution Kernel Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the single cross-platform process kernel and migrate the existing Bash/exec/write internals onto it so Windows, macOS, and Linux share truthful exit, cancellation, PTY, output, cwd, sandbox, and process-tree semantics without changing the model-visible tool schema yet.

**Architecture:** Add a backend-neutral `crates/shared/nomi-execution` crate with immutable normalized requests, bounded ordered output, execution-scoped process ownership, pipe/PTY transports, UUIDv7 owner-scoped sessions, and a 5-second cancellation state machine. `nomifun-runtime` becomes a thin compatibility facade over shared primitives while retaining Bun-specific resolution; `nomi-tools` keeps its current public schemas during Wave A but delegates every production command path to `ProcessSupervisor`.

**Tech Stack:** Rust 2024, Tokio, tokio-util, async-trait, thiserror, tracing, uuid v7, encoding_rs, libc, portable-pty on Unix only, windows-sys Win32 process/Job/ConPTY APIs; cargo test/nextest; GitHub Actions OS matrix.

## Global Constraints

- `nomi-execution` lives under `crates/shared` and must not depend on `nomifun-*`, `nomi-types`, database, conversation, UI, Bun embedding, or Agent business logic.
- Every non-hand-off process must obtain an execution owner before user code runs. Ownership setup failure is a spawn failure; there is no silent fallback.
- Windows pipe and PTY creation must use `CREATE_SUSPENDED`, assign an execution-scoped `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` Job, then resume. Do not use the current portable-pty Windows spawn, global Job, or `taskkill` in the new kernel.
- Linux uses an independent process group plus a process-level, direct-child pidfd watchdog. Do not use thread-scoped `PR_SET_PDEATHSIG` as a host-liveness condition.
- macOS uses an independent process group plus a process-level, direct-child kqueue watchdog; do not migrate the existing detached/double-fork best-effort watcher.
- Unix process groups contain the owned group, not arbitrary descendants that deliberately escape with `setsid`/`setpgid`. Observed escape or unproven containment is `Lost`; never claim PGID cleanup proves an arbitrary process tree is gone.
- Cancellation SLA is fixed at 5 seconds: interrupt up to 1s, terminate up to 1s, force-kill and wait/reap in the remaining 3s. Unproven cleanup becomes `Lost`, never `Cancelled`.
- `yield` never kills. Child exit must be surfaced within 250ms after the waiter observes it, independent of remaining yield.
- Default output cap is 4 MiB per execution. Memory is bounded while reading, `dropped_bytes` is exact, and decoding errors are explicit.
- Paths/environment stay `PathBuf/OsString` until display. Invalid or unauthorized cwd fails before spawn and never falls back to a home/profile directory.
- Wave A preserves model-visible `Bash`, `exec_command`, and `write_stdin` schemas. Their old OS execution implementations stop serving production calls by the end of this plan.
- Existing Bun cache/extract/resolver stays in `nomifun-runtime`; generic spawn, ownership, cleanup, and PATH merge primitives have one implementation in `nomi-execution`.
- Use TDD for every task: failing test, observed failure, minimal implementation, focused pass, touched-crate regression, commit.
- On this Windows workstation invoke Cargo as `& "$env:USERPROFILE\.cargo\bin\cargo.exe"`; portable plan commands use `cargo` in CI.
- Each task is one reviewable commit. Do not combine later Wave B typed tool protocol, Wave C RunController, or Wave D persistence/UI work into Wave A.

## Program Decomposition

The approved design spans five independent waves. This plan covers Wave A only and leaves the workspace in a working, testable state. Wave B begins only after this plan's final gate and receives a separate implementation plan based on the concrete interfaces delivered here; Waves C, D, and E follow the same rule.

## File Responsibility Map

### New shared crate

- `crates/shared/nomi-execution/Cargo.toml`: dependencies, target-specific Win32/Unix features, test helper binaries.
- `src/lib.rs`: public re-exports only.
- `src/request.rs`: public request DTOs and `normalize_request`.
- `src/capability.rs`: cwd/sandbox/hand-off spawn policy.
- `src/outcome.rs`: session/process state, events, snapshots, exit and cleanup facts.
- `src/io.rs`: bounded ordered byte log and incremental text decoder.
- `src/supervisor.rs`: public lifecycle API and cancellation state machine.
- `src/registry.rs`: owner-scoped UUIDv7 sessions, leases, capacity and reaper.
- `src/platform/mod.rs`: private `ProcessOwner` interface and platform selection.
- `src/platform/unix.rs`: Unix pipe spawn, process group, signals, wait/reap.
- `src/platform/linux_watchdog.rs`: Linux parent-death group watchdog.
- `src/platform/macos_watchdog.rs`: macOS kqueue group watchdog.
- `src/platform/unix_pty.rs`: Unix PTY adapter.
- `src/platform/windows.rs`: Win32 RAII handles, suspended pipe spawn and execution Job.
- `src/platform/windows/conpty.rs`: suspended ConPTY spawn and bounded close.
- `src/bin/execution_test_helper.rs`: deterministic cross-platform child/grandchild/I/O scenarios.
- `src/bin/parent_death_harness.rs`: subprocess used to test host-death cleanup.
- `tests/request_contract.rs`, `io_contract.rs`, `process_contract.rs`, `pty_contract.rs`, `session_registry.rs`, `parent_death.rs`: black-box contracts.

### Existing adapters

- `crates/backend/nomifun-runtime/src/spawn.rs`: legacy `Builder` facade delegating shared primitives; no second Job/group/watchdog implementation.
- `crates/backend/nomifun-runtime/src/job.rs`: removed after call sites/tests move to the shared Windows implementation.
- `crates/backend/nomifun-runtime/src/shell_env.rs`: Bun-bin-aware wrapper over generic shared PATH merge.
- `crates/agent/nomi-tools/src/bash.rs`: legacy schema adapter to pipe execution.
- `crates/agent/nomi-tools/src/exec_command.rs`: legacy schema adapter selecting pipe for `tty=false`, PTY for `tty=true`.
- `crates/agent/nomi-tools/src/write_stdin.rs`: legacy session action adapter.
- `crates/agent/nomi-tools/src/{pty,process_store,persistent_shell}.rs`: cease production use; physical deletion remains Wave E after history compatibility is no longer needed.
- `crates/agent/nomi-agent/src/bootstrap.rs`: create one supervisor per Agent registry and give all command adapters the same instance.

---

### Task 1: Scaffold `nomi-execution` and freeze request/outcome contracts

**Files:**
- Create: `crates/shared/nomi-execution/Cargo.toml`
- Create: `crates/shared/nomi-execution/src/lib.rs`
- Create: `crates/shared/nomi-execution/src/request.rs`
- Create: `crates/shared/nomi-execution/src/capability.rs`
- Create: `crates/shared/nomi-execution/src/outcome.rs`
- Create: `crates/shared/nomi-execution/src/platform/mod.rs`
- Create: `crates/shared/nomi-execution/tests/request_contract.rs`
- Modify: `Cargo.toml`
- Modify: `crates/shared/README.md`

**Interfaces:**
- Produces:
  - `ExecutionOwner::new(run_id: Uuid, call_id: Uuid) -> ExecutionOwner`
  - `ExecutionRequest`, `CommandSpec`, `Transport`, `ShellKind`, `ExecutionPolicy`
  - `CapabilityPolicy::local_owner(root: PathBuf) -> CapabilityPolicy`
  - `normalize_request(request, session_cwd) -> Result<NormalizedExecutionRequest, ExecutionError>`
  - `SessionId::new() -> SessionId`, backed by UUIDv7
  - `ExecutionOutcome`, `ExecutionEvent`, `OutputSnapshot`, `CleanupReport`
- Consumes: only std types plus uuid/thiserror; no Agent/backend DTOs.

- [ ] **Step 1: Add the crate scaffold and failing request-contract tests**

`Cargo.toml` workspace additions:

```toml
nomi-execution = { path = "crates/shared/nomi-execution" }
encoding_rs = "0.8"
```

New crate manifest:

```toml
[package]
name = "nomi-execution"
version.workspace = true
edition.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
async-trait.workspace = true
encoding_rs.workspace = true
serde.workspace = true
thiserror.workspace = true
tokio.workspace = true
tokio-util.workspace = true
tracing.workspace = true
uuid.workspace = true

[dev-dependencies]
tempfile.workspace = true
serial_test.workspace = true

[target.'cfg(unix)'.dependencies]
libc.workspace = true
portable-pty.workspace = true

[target.'cfg(windows)'.dependencies]
windows-sys = { version = "0.61", features = [
  "Win32_Foundation",
  "Win32_Security",
  "Win32_Storage_FileSystem",
  "Win32_System_Console",
  "Win32_System_IO",
  "Win32_System_JobObjects",
  "Win32_System_Pipes",
  "Win32_System_Threading",
] }
```

Create `tests/request_contract.rs`:

```rust
use std::{collections::BTreeMap, ffi::OsString, path::PathBuf, time::Duration};
use nomi_execution::{
    normalize_request, CapabilityPolicy, CommandSpec, ExecutionOwner,
    ExecutionPolicy, ExecutionRequest, SessionId, ShellKind, Transport,
};
use uuid::Uuid;

fn request(cwd: PathBuf) -> ExecutionRequest {
    ExecutionRequest {
        owner: ExecutionOwner::new(Uuid::now_v7(), Uuid::now_v7()),
        command: CommandSpec::Program {
            program: OsString::from("tool"),
            args: vec![OsString::from("--flag")],
        },
        cwd,
        env: BTreeMap::new(),
        transport: Transport::Pipe,
        policy: ExecutionPolicy::default(),
        capability: CapabilityPolicy::local_owner(std::env::temp_dir()),
    }
}

#[test]
fn relative_cwd_is_anchored_and_validated() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("child")).unwrap();
    let mut req = request(PathBuf::from("child"));
    req.capability = CapabilityPolicy::local_owner(root.path().to_path_buf());
    let normalized = normalize_request(req, root.path()).unwrap();
    assert_eq!(normalized.cwd, root.path().join("child"));
}

#[test]
fn missing_cwd_fails_before_spawn() {
    let root = tempfile::tempdir().unwrap();
    let mut req = request(PathBuf::from("missing"));
    req.capability = CapabilityPolicy::local_owner(root.path().to_path_buf());
    let err = normalize_request(req, root.path()).unwrap_err();
    assert_eq!(err.code(), "invalid_working_directory");
}

#[test]
fn cwd_outside_capability_root_is_denied() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let mut req = request(outside.path().to_path_buf());
    req.capability = CapabilityPolicy::local_owner(root.path().to_path_buf());
    let err = normalize_request(req, root.path()).unwrap_err();
    assert_eq!(err.code(), "capability_denied");
}

#[test]
fn cancellation_policy_totals_five_seconds() {
    let p = ExecutionPolicy::default();
    assert_eq!(p.interrupt_grace, Duration::from_secs(1));
    assert_eq!(p.terminate_grace, Duration::from_secs(1));
    assert_eq!(p.reap_grace, Duration::from_secs(3));
}

#[test]
fn session_ids_are_uuid_v7_and_unpredictable() {
    let a = SessionId::new();
    let b = SessionId::new();
    assert_ne!(a, b);
    assert_eq!(a.as_uuid().get_version_num(), 7);
}

#[test]
fn shell_requires_a_script_and_program_preserves_os_strings() {
    let shell = CommandSpec::Shell {
        shell: ShellKind::Posix,
        script: "printf ok".to_owned(),
    };
    assert!(matches!(shell, CommandSpec::Shell { .. }));

    let spec = CommandSpec::Program {
        program: OsString::from("echo"),
        args: vec![OsString::from("ok")],
    };
    assert!(matches!(spec, CommandSpec::Program { .. }));
}
```

- [ ] **Step 2: Run the tests and capture the expected failure**

Run:

```powershell
& "$env:USERPROFILE\.cargo\bin\cargo.exe" test -p nomi-execution --test request_contract -- --test-threads=1
```

Expected: compile failure because the crate modules and public types do not exist.

- [ ] **Step 3: Implement the immutable request and outcome types**

Use these exact public shapes:

```rust
pub struct ExecutionRequest {
    pub owner: ExecutionOwner,
    pub command: CommandSpec,
    pub cwd: PathBuf,
    pub env: BTreeMap<OsString, OsString>,
    pub transport: Transport,
    pub policy: ExecutionPolicy,
    pub capability: CapabilityPolicy,
}

pub enum CommandSpec {
    Program { program: OsString, args: Vec<OsString> },
    Shell { shell: ShellKind, script: String },
}

pub enum Transport {
    Pipe,
    Pty { cols: u16, rows: u16 },
}

pub enum ShellKind { PowerShell, Posix }

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ExecutionOwner { pub run_id: Uuid, pub call_id: Uuid }

#[derive(Clone, Debug)]
pub struct ExecutionPolicy {
    pub output_limit_bytes: usize,
    pub lease: Duration,
    pub deadline: Option<Instant>,
    pub interrupt_grace: Duration,
    pub terminate_grace: Duration,
    pub reap_grace: Duration,
}

impl Default for ExecutionPolicy {
    fn default() -> Self {
        Self {
            output_limit_bytes: 4 * 1024 * 1024,
            lease: Duration::from_secs(15 * 60),
            deadline: None,
            interrupt_grace: Duration::from_secs(1),
            terminate_grace: Duration::from_secs(1),
            reap_grace: Duration::from_secs(3),
        }
    }
}
```

`CapabilityPolicy` and the normalized value use these exact fields:

```rust
pub enum SandboxPolicy {
    UnrestrictedLocalOwner,
    MacSeatbelt { write_roots: Vec<PathBuf> },
    DenyExecution,
}

pub struct CapabilityPolicy {
    pub cwd_roots: Vec<PathBuf>,
    pub sandbox: SandboxPolicy,
    pub allow_hand_off: bool,
}

pub struct NormalizedExecutionRequest {
    pub owner: ExecutionOwner,
    pub command: CommandSpec,
    pub cwd: PathBuf,
    pub env: BTreeMap<OsString, OsString>,
    pub transport: Transport,
    pub policy: ExecutionPolicy,
    pub capability: CapabilityPolicy,
}
```

`normalize_request` must:

1. Join relative cwd to `session_cwd`.
2. Canonicalize the existing directory.
3. Reject non-directory cwd.
4. Canonicalize allowed roots and require the cwd to start with one.
5. Reject `Transport::Pty { cols: 0 | rows: 0 }`.
6. Reject an empty program or empty shell script.
7. Return a separate `NormalizedExecutionRequest`; later code never executes the mutable input.

Define `ExecutionError::code()` with stable strings used above. Define `SessionId(Uuid)` with `new/as_uuid/Display/FromStr`. Define outcome/event enums exactly as in the approved design, including `Lost` and `dropped_bytes`.

`OutputSnapshot` exposes `chunks: Vec<OutputChunk>`, `next_cursor: OutputCursor`, `retained_bytes: usize`, `dropped_bytes: u64`, and `encoding: EncodingMetadata`. `CleanupReport` exposes `interrupt_attempted`, `terminate_attempted`, `force_kill_attempted`, `reaped`, `elapsed`, and `errors: Vec<String>`.

- [ ] **Step 4: Run focused tests and touched-crate formatting**

Run:

```powershell
& "$env:USERPROFILE\.cargo\bin\cargo.exe" fmt --all
& "$env:USERPROFILE\.cargo\bin\cargo.exe" test -p nomi-execution --test request_contract -- --test-threads=1
```

Expected: all request-contract tests pass.

- [ ] **Step 5: Commit**

```powershell
git add Cargo.toml Cargo.lock crates/shared/README.md crates/shared/nomi-execution
git commit -m "feat(execution): define cross-platform process contracts"
```

---

### Task 2: Implement bounded ordered output and incremental decoding

**Files:**
- Create: `crates/shared/nomi-execution/src/io.rs`
- Create: `crates/shared/nomi-execution/tests/io_contract.rs`
- Modify: `crates/shared/nomi-execution/src/lib.rs`
- Modify: `crates/shared/nomi-execution/src/outcome.rs`

**Interfaces:**
- Produces:
  - `OutputBuffer::new(limit_bytes) -> OutputBuffer`
  - `OutputBuffer::push(stream, bytes) -> Vec<ExecutionEvent>`
  - `OutputBuffer::snapshot_from(cursor) -> OutputSnapshot`
  - `OutputCursor`, `OutputStream::{Stdout,Stderr,Pty}`
  - `EncodingMetadata { source_encoding, decode_errors }`
- Consumes: Task 1 outcome types.

- [ ] **Step 1: Write failing byte-order, Unicode, and cap tests**

```rust
use nomi_execution::{OutputBuffer, OutputCursor, OutputStream};

#[test]
fn preserves_cross_stream_observation_order() {
    let out = OutputBuffer::new(1024);
    out.push(OutputStream::Stdout, b"one");
    out.push(OutputStream::Stderr, b"two");
    let s = out.snapshot_from(OutputCursor::START);
    assert_eq!(s.chunks[0].stream, OutputStream::Stdout);
    assert_eq!(s.chunks[1].stream, OutputStream::Stderr);
}

#[test]
fn decodes_utf8_split_across_chunks_without_replacement() {
    let out = OutputBuffer::new(1024);
    let bytes = "中文🙂".as_bytes();
    for byte in bytes { out.push(OutputStream::Stdout, &[*byte]); }
    let s = out.snapshot_from(OutputCursor::START);
    assert_eq!(s.text(), "中文🙂");
    assert_eq!(s.encoding.decode_errors, 0);
}

#[test]
fn bounded_buffer_reports_exact_dropped_bytes() {
    let out = OutputBuffer::new(8);
    out.push(OutputStream::Stdout, b"123456");
    out.push(OutputStream::Stdout, b"7890");
    let s = out.snapshot_from(OutputCursor::START);
    assert!(s.retained_bytes <= 8);
    assert_eq!(s.dropped_bytes, 2);
}

#[test]
fn pty_stream_identity_is_not_fabricated() {
    let out = OutputBuffer::new(1024);
    out.push(OutputStream::Pty, b"merged");
    let s = out.snapshot_from(OutputCursor::START);
    assert_eq!(s.chunks[0].stream, OutputStream::Pty);
}

#[test]
fn invalid_bytes_are_reported_and_raw_bytes_remain_bounded() {
    let out = OutputBuffer::new(1024);
    out.push(OutputStream::Stdout, &[0xff, 0xfe]);
    let s = out.snapshot_from(OutputCursor::START);
    assert!(s.encoding.decode_errors > 0);
    assert_eq!(s.raw_bytes(), &[0xff, 0xfe]);
}
```

- [ ] **Step 2: Run and observe the compile failure**

Run: `cargo test -p nomi-execution --test io_contract`

Expected: compile failure for missing `OutputBuffer`.

- [ ] **Step 3: Implement a sequence-preserving bounded chunk log**

Implementation rules:

- Allocate monotonically increasing byte offsets and event sequence numbers.
- Retain `VecDeque<StoredChunk>`; if total bytes exceed the cap, drain bytes from the oldest chunk, increment `base_offset` and `dropped_bytes`, and emit `OutputDropped`.
- `snapshot_from` starts at `max(cursor, base_offset)`, reports loss when the caller cursor is older, and returns the next absolute cursor.
- Keep one incremental decoder per stream. Strict UTF-8 is first choice; on Windows only, a failed strict decode uses the active-code-page mapping and records the source label. Raw bounded bytes remain available when decoding is not lossless.
- Never call `String::from_utf8_lossy` on individual chunks.

Core storage:

```rust
struct StoredChunk {
    seq: u64,
    start: u64,
    stream: OutputStream,
    bytes: Vec<u8>,
}

pub struct OutputBuffer {
    limit: usize,
    inner: Mutex<OutputState>,
}

struct OutputState {
    next_seq: u64,
    next_offset: u64,
    base_offset: u64,
    retained: usize,
    dropped_bytes: u64,
    chunks: VecDeque<StoredChunk>,
    decoders: StreamDecoders,
}
```

- [ ] **Step 4: Run focused tests and Miri-safe unit checks**

Run:

```powershell
& "$env:USERPROFILE\.cargo\bin\cargo.exe" test -p nomi-execution --test io_contract
& "$env:USERPROFILE\.cargo\bin\cargo.exe" test -p nomi-execution
```

Expected: all pass and no unbounded allocation test exceeds the configured cap.

- [ ] **Step 5: Commit**

```powershell
git add Cargo.toml Cargo.lock crates/shared/nomi-execution
git commit -m "feat(execution): add bounded ordered process output"
```

---

### Task 3: Build the supervisor core and deterministic test helper

**Files:**
- Create: `crates/shared/nomi-execution/src/supervisor.rs`
- Create: `crates/shared/nomi-execution/src/registry.rs`
- Create: `crates/shared/nomi-execution/src/bin/execution_test_helper.rs`
- Create: `crates/shared/nomi-execution/tests/supervisor_contract.rs`
- Modify: `crates/shared/nomi-execution/src/lib.rs`
- Modify: `crates/shared/nomi-execution/src/platform/mod.rs`

**Interfaces:**
- Produces `ProcessSupervisor`, `ExecutionHandle`, `PollResult`, `ProcessOwner`.
- Consumes Task 1 normalized request and Task 2 output buffer.

- [ ] **Step 1: Add the cross-platform helper commands**

The helper binary accepts these exact subcommands:

```text
exit <code>
sleep <milliseconds>
echo-stdin
emit-interleaved
emit-split-utf8
flood <bytes>
spawn-grandchild <pid-marker-path>
ignore-interrupt
write-pid <path>
```

`spawn-grandchild` launches the same executable with `sleep 60000`, writes its PID atomically to the marker, then sleeps. `emit-interleaved` alternates flushed stdout/stderr records. `emit-split-utf8` writes the bytes of `中文🙂` one at a time with flushes.

- [ ] **Step 2: Write failing supervisor state-machine tests with a fake ProcessOwner**

```rust
#[tokio::test]
async fn cancellation_escalates_and_never_claims_success_without_reap() {
    let fake = FakeOwner::ignores_interrupt_and_terminate();
    let supervisor = ProcessSupervisor::for_test(fake.clone());
    let handle = supervisor.start_for_test().await.unwrap();
    let outcome = supervisor.cancel(&handle.owner, &handle.session_id).await.unwrap();
    assert!(matches!(outcome, ExecutionOutcome::Cancelled { .. }));
    assert_eq!(fake.calls(), vec!["interrupt", "terminate", "force_kill", "wait_reaped"]);
}

#[tokio::test]
async fn unproven_reap_returns_lost() {
    let fake = FakeOwner::never_reaps();
    let supervisor = ProcessSupervisor::for_test(fake);
    let handle = supervisor.start_for_test().await.unwrap();
    let outcome = supervisor.cancel(&handle.owner, &handle.session_id).await.unwrap();
    assert!(matches!(outcome, ExecutionOutcome::Lost { .. }));
}

#[tokio::test]
async fn child_exit_wakes_poll_without_waiting_for_yield() {
    tokio::time::pause();
    let fake = FakeOwner::exits_after(Duration::from_millis(20), 0);
    let supervisor = ProcessSupervisor::for_test(fake);
    let handle = supervisor.start_for_test().await.unwrap();
    let poll = supervisor.poll(
        &handle.owner,
        &handle.session_id,
        OutputCursor::START,
        Instant::now() + Duration::from_secs(10),
    ).await.unwrap();
    assert!(matches!(poll, PollResult::Finished(_)));
    assert!(tokio::time::Instant::now() < handle.started_at + Duration::from_millis(300));
}
```

- [ ] **Step 3: Implement the private platform contract and supervisor transitions**

```rust
#[async_trait]
pub(crate) trait ProcessOwner: Send + Sync {
    fn pid(&self) -> u32;
    async fn write(&self, bytes: &[u8]) -> io::Result<()>;
    async fn close_stdin(&self) -> io::Result<()>;
    async fn interrupt(&self) -> io::Result<()>;
    async fn terminate(&self) -> io::Result<()>;
    async fn force_kill(&self) -> io::Result<()>;
    async fn wait_reaped(&self, deadline: Instant) -> io::Result<ExitFact>;
}

impl ProcessSupervisor {
    pub fn new(config: SupervisorConfig) -> Arc<Self>;
    pub async fn start(
        self: &Arc<Self>,
        request: NormalizedExecutionRequest,
    ) -> Result<ExecutionHandle, ExecutionError>;
    pub async fn poll(
        &self,
        owner: &ExecutionOwner,
        session_id: &SessionId,
        cursor: OutputCursor,
        yield_until: Instant,
    ) -> Result<PollResult, ExecutionError>;
    pub async fn write(&self, owner: &ExecutionOwner, session_id: &SessionId, bytes: &[u8])
        -> Result<(), ExecutionError>;
    pub async fn close_stdin(&self, owner: &ExecutionOwner, session_id: &SessionId)
        -> Result<(), ExecutionError>;
    pub async fn interrupt(&self, owner: &ExecutionOwner, session_id: &SessionId)
        -> Result<(), ExecutionError>;
    pub async fn terminate(&self, owner: &ExecutionOwner, session_id: &SessionId)
        -> Result<ExecutionOutcome, ExecutionError>;
    pub async fn cancel(&self, owner: &ExecutionOwner, session_id: &SessionId)
        -> Result<ExecutionOutcome, ExecutionError>;
    pub async fn status(&self, owner: &ExecutionOwner, session_id: &SessionId)
        -> Result<ProcessSnapshot, ExecutionError>;
}
```

The public handles/configuration are:

```rust
#[derive(Clone)]
pub struct ExecutionHandle {
    pub owner: ExecutionOwner,
    pub session_id: SessionId,
    pub started_at: Instant,
}

pub struct SupervisorConfig {
    pub max_sessions: usize,
    pub reaper_interval: Duration,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self { max_sessions: 64, reaper_interval: Duration::from_secs(30) }
    }
}
```

The supervisor registry owns resources; dropping `ExecutionHandle` does not lose the process. A background waiter writes the single exit fact to a watch channel and notifies pollers. On natural exit, allow up to 120ms final output drain, snapshot, then return regardless of PTY EOF.

- [ ] **Step 4: Run supervisor-only tests**

Run: `cargo test -p nomi-execution --test supervisor_contract`

Expected: pass with Tokio time paused; no real OS process required yet.

- [ ] **Step 5: Commit**

```powershell
git add crates/shared/nomi-execution
git commit -m "feat(execution): add supervised process lifecycle"
```

---

### Task 4: Implement Unix pipe ownership and parent-death cleanup

**Files:**
- Modify: `docs/superpowers/specs/2026-07-10-nomifun-agent-command-reliability-design.md`
- Modify: `docs/superpowers/plans/2026-07-10-agent-reliability-wave-a-execution-kernel.md`
- Create: `crates/shared/nomi-execution/src/platform/unix.rs`
- Create: `crates/shared/nomi-execution/src/platform/linux_watchdog.rs`
- Create: `crates/shared/nomi-execution/src/platform/macos_watchdog.rs`
- Create: `crates/shared/nomi-execution/src/bin/parent_death_harness.rs`
- Create: `crates/shared/nomi-execution/tests/process_contract.rs`
- Create: `crates/shared/nomi-execution/tests/parent_death.rs`
- Modify: `crates/shared/nomi-execution/src/platform/mod.rs`

**Interfaces:**
- Produces `platform::spawn_pipe(NormalizedExecutionRequest, OutputBuffer) -> SpawnedPlatformProcess` on Unix.
- Consumes supervisor `ProcessOwner`.

- [ ] **Step 1: Write Unix-only failing real-process tests**

Tests must assert:

- helper `exit 0` and `exit 7` return exact codes;
- stdin closes and `echo-stdin` exits;
- invalid executable is `SpawnFailed`;
- `spawn-grandchild` child PID and grandchild PID are both gone after cancel;
- parent-death harness exits without cleanup code and both descendants disappear;
- natural exit returns within 250ms after exit observation;
- invalid exec, withheld ACK, short registration, wrong nonce and watchdog failure terminate within one shared setup deadline without running a marker;
- leader-first exit cleans a same-group descendant before reap, while a detectable `setsid` escape becomes `Lost` rather than a false whole-tree success;
- rapid spawn/exit and subreaper cases leave no normal-path watchdog zombies;
- after leader reap no negative-PGID signal can occur, including forced PGID-reuse tests.

Use `libc::kill(pid, 0)` only in test helpers and always wait with a bounded condition loop.

- [ ] **Step 2: Run on the current platform or compile-gate**

Run on Linux/macOS: `cargo test -p nomi-execution --test process_contract --test parent_death -- --test-threads=1`

Run on Windows for compile coverage: `cargo test -p nomi-execution --test process_contract --no-run`

Expected on Unix: failures because Unix platform spawn/watchdogs are missing.

- [ ] **Step 3: Move and harden existing Unix primitives**

- Reuse only hardened ideas from the old runtime. Implement a direct-child watchdog with a bounded `BOOT_READY -> child registration/ACK -> COMMIT/ABORT -> COMMITTED` protocol; the watchdog is the only ACK writer, so `Command::spawn` cannot deadlock waiting on its private exec-error pipe.
- Keep a watchdog health/control channel. ACK-before-health failure is spawn failure; ACK/exec-after-health failure is `StartLost` and triggers immediate cleanup.
- Linux watchdog monitors the host and child identity with pidfd. Fallback is allowed only for explicit pidfd unavailability and must verify `/proc` start time/state; identity uncertainty fails closed. macOS watchdog creates and registers its own kqueue before READY/ACK.
- Watchdog fork code is a raw async-signal-safe loop. It moves retained protocol FDs to fixed slots, redirects stdio, and closes all other inherited descriptors. User-child `pre_exec` closes only known protocol FDs and must never `close_range` unknown FDs.
- `interrupt` sends SIGINT to negative pgid; `terminate` sends SIGTERM; `force_kill` sends SIGKILL. A single signal gate makes the state check and syscall atomic with respect to reap.
- On leader exit, watchdog performs the final group kill while leader identity is still anchored, then exits. The sole waiter reaps watchdog, permanently closes the signal gate, reaps leader exactly once, and only probes group absence afterward. It never signals a PGID after leader reap; any remaining/ambiguous group is `Lost`.
- `ProcessSupervisor::start` keeps its existing shape. Add stable `ExecutionError::SpawnFailed { failure }` (`spawn_failed`) and `ExecutionError::StartLost { failure, last_known, cleanup }` (`start_lost`). Invalid executable is `SpawnFailed` only after all pre-exec resources are proven clean.
- All pipe executions are owned regardless of `allow_hand_off`. `DenyExecution` and unsupported/uninstalled sandbox requests fail before spawn; no unrestricted fallback.

- [ ] **Step 4: Run Unix contracts**

Run in CI target:

```bash
cargo test -p nomi-execution --test process_contract --test parent_death -- --test-threads=1
```

Expected: pass on Ubuntu and macOS.

- [ ] **Step 5: Commit**

```bash
git add crates/shared/nomi-execution
git commit -m "feat(execution): own Unix process groups and parent death"
```

---

### Task 5: Implement Windows suspended pipe spawn and execution Job

**Files:**
- Create: `crates/shared/nomi-execution/src/platform/windows.rs`
- Create: `crates/shared/nomi-execution/src/platform/windows/handles.rs`
- Modify: `crates/shared/nomi-execution/src/platform/mod.rs`
- Extend: `crates/shared/nomi-execution/tests/process_contract.rs`
- Extend: `crates/shared/nomi-execution/tests/parent_death.rs`

**Interfaces:**
- Produces the same `spawn_pipe` and `ProcessOwner` contract as Unix.

- [ ] **Step 1: Write Windows-only suspended ownership tests**

Add assertions that:

- Job assignment failure from an injected Win32 facade makes `start` fail and never resumes the child;
- a leader that spawns `spawn-grandchild` is killed together with its grandchild;
- dropping the parent harness closes the Job handle and kills the tree;
- cancel reaches `Cancelled` within 5 seconds;
- no implementation command invokes `taskkill`.

- [ ] **Step 2: Run and verify failure**

Run:

```powershell
& "$env:USERPROFILE\.cargo\bin\cargo.exe" test -p nomi-execution --test process_contract --test parent_death -- --test-threads=1
```

Expected: fail because Windows platform spawn is missing.

- [ ] **Step 3: Implement RAII and the suspended CreateProcess sequence**

Required order:

```text
Create inheritable stdin/stdout/stderr pipes
-> clear inheritance on parent pipe ends
-> CreateJobObjectW
-> SetInformationJobObject(KILL_ON_JOB_CLOSE)
-> CreateProcessW(CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW)
-> AssignProcessToJobObject
-> ResumeThread
-> close child-only pipe/thread handles in parent
-> start bounded reader threads and async waiter
```

On any error before `ResumeThread`, terminate the suspended process if it exists, close every handle, and return `ExecutionError`. `WindowsOwner::force_kill` closes the execution Job; `wait_reaped` waits on the process handle and reads `GetExitCodeProcess`. No global Job and no `taskkill`.

RAII wrappers:

```rust
struct OwnedHandle(HANDLE);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.0); }
        }
    }
}

struct WindowsOwner {
    pid: u32,
    process: Arc<OwnedHandle>,
    job: Mutex<Option<OwnedHandle>>,
    stdin: Mutex<Option<OwnedHandle>>,
}
```

- [ ] **Step 4: Run Windows contracts and leak loop**

Run:

```powershell
& "$env:USERPROFILE\.cargo\bin\cargo.exe" test -p nomi-execution --test process_contract --test parent_death -- --test-threads=1
1..50 | ForEach-Object {
  & "$env:USERPROFILE\.cargo\bin\cargo.exe" test -p nomi-execution --test process_contract natural_exit_returns_promptly -- --exact
  if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}
```

Expected: pass; Task Manager/process probes show no accumulating helper processes or handles.

- [ ] **Step 5: Commit**

```powershell
git add Cargo.toml Cargo.lock crates/shared/nomi-execution
git commit -m "feat(execution): own Windows processes with scoped jobs"
```

---

### Task 6: Add Unix PTY and suspended Windows ConPTY transports

**Files:**
- Create: `crates/shared/nomi-execution/src/platform/unix_pty.rs`
- Create: `crates/shared/nomi-execution/src/platform/windows/conpty.rs`
- Create: `crates/shared/nomi-execution/tests/pty_contract.rs`
- Modify: `crates/shared/nomi-execution/src/platform/mod.rs`
- Modify: `crates/shared/nomi-execution/src/supervisor.rs`

**Interfaces:**
- `Transport::Pty` uses the same supervisor/session/owner/outcome API.
- Adds `ProcessSupervisor::resize(owner, session_id, cols, rows)`.

- [ ] **Step 1: Write failing PTY contracts**

Test:

- `echo-stdin` round trip;
- one-byte-at-a-time UTF-8;
- quick `exit 0` with a 10-second yield returns in under 1 second total on the helper;
- running session supports poll, write, close stdin, interrupt and resize;
- ConPTY child and grandchild belong to the same Job;
- closing a ConPTY is isolated from the Tokio runtime and has a 3-second cleanup deadline;
- Unix PTY kill targets the process group.

- [ ] **Step 2: Run and observe failures**

Run: `cargo test -p nomi-execution --test pty_contract -- --test-threads=1`

Expected: failures because PTY transports are not implemented.

- [ ] **Step 3: Implement transports**

- Unix may wrap `portable-pty`, but the new `ProcessOwner` remains authoritative for group signals and wait/reap.
- Windows must call `CreatePseudoConsole`, configure `STARTUPINFOEXW` with `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE`, and use the same suspended CreateProcess + execution Job sequence as Task 5.
- ConPTY CreateProcess flags are `CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT`; `CREATE_NO_WINDOW` is used only by the non-PTY pipe path.
- ConPTY input/output handles use dedicated blocking reader/writer workers. `ClosePseudoConsole` runs only in a cleanup worker and is bounded; an expired close produces cleanup diagnostics rather than blocking runtime threads.
- Natural child exit is sufficient to complete after the 120ms final drain; never wait for ConPTY EOF.

- [ ] **Step 4: Run all transport tests**

Run:

```powershell
& "$env:USERPROFILE\.cargo\bin\cargo.exe" test -p nomi-execution --test process_contract --test pty_contract -- --test-threads=1
```

Expected: pass on Windows; the same command passes on Unix CI.

- [ ] **Step 5: Commit**

```powershell
git add crates/shared/nomi-execution
git commit -m "feat(execution): add supervised PTY transports"
```

---

### Task 7: Add owner-scoped session leases and reaper

**Files:**
- Modify: `crates/shared/nomi-execution/src/registry.rs`
- Modify: `crates/shared/nomi-execution/src/supervisor.rs`
- Create: `crates/shared/nomi-execution/tests/session_registry.rs`

**Interfaces:**
- Uses the Task 3 `SupervisorConfig { max_sessions: 64, reaper_interval: 30s }`.
- Session actions require the exact `ExecutionOwner`.

- [ ] **Step 1: Write failing session security/resource tests**

Test:

- UUIDv7 uniqueness;
- wrong run or call owner is denied;
- poll/write/status renew lease;
- a live run heartbeat renews owner activity;
- expired session is cancelled and reaped, not merely removed;
- capacity pressure chooses finished sessions first and never silently evicts an active protected owner;
- supervisor shutdown returns `ShutdownReport` listing `Cancelled` or `Lost` for every session.

- [ ] **Step 2: Run and observe failures**

Run: `cargo test -p nomi-execution --test session_registry -- --test-threads=1`

- [ ] **Step 3: Implement registry and reaper**

Use:

```rust
struct SessionEntry {
    id: SessionId,
    owner: ExecutionOwner,
    process: Arc<ManagedProcess>,
    cursor: OutputCursor,
    lease_expires_at: Instant,
    last_used: Instant,
}
```

The reaper obtains victims under the registry lock, removes them, releases the lock, then awaits the 5-second cancellation state machine. Never await OS cleanup while holding the registry mutex.

- [ ] **Step 4: Run all shared-crate tests**

Run: `cargo test -p nomi-execution -- --test-threads=1`

Expected: all pass and test completion leaves no helper process.

- [ ] **Step 5: Commit**

```powershell
git add crates/shared/nomi-execution
git commit -m "feat(execution): manage owner-scoped session leases"
```

---

### Task 8: Make `nomifun-runtime` a thin shared-kernel facade

**Files:**
- Modify: `crates/backend/nomifun-runtime/Cargo.toml`
- Modify: `crates/backend/nomifun-runtime/src/lib.rs`
- Modify: `crates/backend/nomifun-runtime/src/spawn.rs`
- Delete: `crates/backend/nomifun-runtime/src/job.rs`
- Modify: `crates/backend/nomifun-runtime/src/shell_env.rs`
- Modify tests in `crates/backend/nomifun-runtime/src/spawn.rs` and `src/shell_env.rs`

**Interfaces:**
- `nomifun-runtime` keeps `resolve_bun/bun_bin_dir/resolve_command_path`.
- Existing `Builder` remains source-compatible but delegates environment cleaning and platform setup to shared primitives.
- New Agent execution does not use legacy `Builder`; it uses `ProcessSupervisor`.

- [ ] **Step 1: Add architecture tests that fail while duplicate primitives exist**

Add tests asserting `nomifun-runtime/src` no longer contains definitions of `CleanupJob`, `install_pdeathsig`, `install_macos_pdeath_watch`, or a `taskkill` command after migration. Existing Bun resolver tests remain unchanged.

- [ ] **Step 2: Run and verify duplicate-primitive failures**

Run: `cargo test -p nomifun-runtime`

Expected: new architecture assertions fail.

- [ ] **Step 3: Delegate shared primitives without moving Bun behavior**

- Add `nomi-execution.workspace = true`.
- Replace local Job/group/watchdog helpers with shared `CommandBuilder`/platform helper calls.
- Keep `resolve_program` in the facade so `bun/bunx` still use bundled resolution.
- Change `enhance_process_path` to compute `bun_bin_dir()` then call shared `merge_process_path(&[bun_dir])`.
- Delete `job.rs` and its module declaration.

- [ ] **Step 4: Run runtime and dependent compile gates**

```powershell
& "$env:USERPROFILE\.cargo\bin\cargo.exe" test -p nomifun-runtime -- --test-threads=1
& "$env:USERPROFILE\.cargo\bin\cargo.exe" check -p nomifun-ai-agent -p nomifun-mcp -p nomifun-office -p nomi-browser-engine
```

Expected: pass without changing Bun resolution or public runtime callers.

- [ ] **Step 5: Commit**

```powershell
git add Cargo.toml Cargo.lock crates/backend/nomifun-runtime crates/shared/nomi-execution
git commit -m "refactor(runtime): delegate process ownership to shared kernel"
```

---

### Task 9: Migrate `BashTool` and sandbox execution to the supervisor

**Files:**
- Modify: `crates/agent/nomi-tools/Cargo.toml`
- Modify: `crates/agent/nomi-tools/src/bash.rs`
- Modify: `crates/agent/nomi-tools/src/sandbox.rs`
- Modify: `crates/agent/nomi-agent/src/bootstrap.rs`
- Modify: `crates/agent/nomi-agent/src/spawner.rs`
- Test: `crates/agent/nomi-tools/src/bash.rs`
- Test: `crates/agent/nomi-agent/tests/cwd_injection_test.rs`

**Interfaces:**
- `BashTool::new(supervisor: Arc<ProcessSupervisor>, cwd: PathBuf, capability: CapabilityPolicy)`.
- Existing model-visible name, description and input schema remain unchanged in Wave A.

- [ ] **Step 1: Write failures for timeout cleanup, encoding, cwd, and sandbox inheritance**

Add tests:

- helper writes a marker after the configured timeout; after timeout + marker delay the marker must not exist and the helper PID/grandchild must be gone;
- exit 7 is `is_error=true`;
- Windows `中文🙂` output is intact;
- nonexistent cwd fails and no file appears under USERPROFILE;
- macOS sandbox applies to both pipe execution and a process-internal child Agent registry;
- sandbox setup failure returns an error and never falls back unrestricted.

- [ ] **Step 2: Run and confirm current Bash behavior fails**

Run:

```powershell
& "$env:USERPROFILE\.cargo\bin\cargo.exe" test -p nomi-tools bash -- --test-threads=1
& "$env:USERPROFILE\.cargo\bin\cargo.exe" test -p nomi-agent cwd_injection -- --test-threads=1
```

Expected: timeout-cleanup/encoding/sandbox-inheritance tests fail.

- [ ] **Step 3: Replace one-shot and sandbox process execution**

- Build a `CommandSpec::Shell` request with pipe transport and normalized cwd.
- Map legacy `timeout` to a deadline no later than the call's remaining host deadline.
- At timeout invoke supervisor cancellation and wait for `Cancelled/TimedOut/Lost`; never drop a `Command::output` future.
- Render ordered output with stream labels and dropped/decode metadata.
- Make macOS sandbox a `CapabilityPolicy` field consumed by the shared platform spawn boundary.
- Remove production registration of `with_persistent_shell`; retain the old config field as ignored compatibility input with one warning. Unified supervised sessions replace it in Wave B.
- Pass the parent's capability into the child registry; the child receives `parent ∩ role`, never a default unrestricted Bash tool.

- [ ] **Step 4: Run tool and Agent regressions**

```powershell
& "$env:USERPROFILE\.cargo\bin\cargo.exe" test -p nomi-tools -- --test-threads=1
& "$env:USERPROFILE\.cargo\bin\cargo.exe" test -p nomi-agent cwd_injection -- --test-threads=1
```

Expected: pass; no Bash production path contains a `tokio::time::timeout` wrapper around a `.output()` future.

- [ ] **Step 5: Commit**

```powershell
git add Cargo.toml Cargo.lock crates/agent/nomi-tools crates/agent/nomi-agent
git commit -m "refactor(tools): run Bash through supervised execution"
```

---

### Task 10: Migrate legacy `exec_command/write_stdin` internals

**Files:**
- Modify: `crates/agent/nomi-tools/src/exec_command.rs`
- Modify: `crates/agent/nomi-tools/src/write_stdin.rs`
- Modify: `crates/agent/nomi-tools/src/process_store.rs`
- Modify: `crates/agent/nomi-tools/src/pty.rs`
- Modify: `crates/agent/nomi-agent/src/bootstrap.rs`
- Test: modules above

**Interfaces:**
- Schemas remain legacy during Wave A.
- Both tools share the same `Arc<ProcessSupervisor>`.
- Legacy numeric session ids are an adapter map to UUIDv7 `SessionId`.

- [ ] **Step 1: Write the missing behavior tests**

Add exact assertions:

- `tty=false` reports pipe transport and keeps stdout/stderr identities;
- `tty=true` reports PTY transport;
- exit 7 returns `is_error=true`;
- immediate exit with `yield_time_ms=3000` completes in under 1000ms on the deterministic helper;
- missing/empty/non-directory/out-of-root workdir is an error;
- empty poll returns immediately once an already-exited process is observed;
- write, close-stdin, Ctrl-C, terminate and status all reach truthful states;
- output produced between calls is replayed by cursor or reports exact dropped bytes;
- wrong legacy owner/session mapping is rejected;
- expired sessions are reaped.

- [ ] **Step 2: Run focused tests and observe failures**

```powershell
& "$env:USERPROFILE\.cargo\bin\cargo.exe" test -p nomi-tools exec_command -- --test-threads=1
& "$env:USERPROFILE\.cargo\bin\cargo.exe" test -p nomi-tools write_stdin -- --test-threads=1
```

Expected: pipe/PTY, nonzero, latency, workdir and lifecycle assertions fail against the old implementation.

- [ ] **Step 3: Replace the production internals**

- `exec_command` maps `tty=false` to `Transport::Pipe`, `tty=true` to `Transport::Pty`.
- `write_stdin` maps legacy empty chars to poll and nonempty chars to write + poll.
- Store only `legacy_u64 -> (ExecutionOwner, SessionId)`; do not store PTY/process objects in `ProcessStore`.
- When a process exits, derive `ToolResult.is_error` from exit code.
- Return `session_id` only for a live `Running` poll.
- All cleanup and output cursors come from the supervisor.
- Remove `crate::pty::Pty` and old `collect_until_deadline` from production references. Keep modules compiled only for legacy session deserialization tests until Wave E deletion.

- [ ] **Step 4: Run full affected suites**

```powershell
& "$env:USERPROFILE\.cargo\bin\cargo.exe" test -p nomi-execution -- --test-threads=1
& "$env:USERPROFILE\.cargo\bin\cargo.exe" test -p nomi-tools -- --test-threads=1
& "$env:USERPROFILE\.cargo\bin\cargo.exe" test -p nomi-agent -- --test-threads=1
```

Expected: pass; the former Windows immediate-exit test no longer consumes its 3-second yield.

- [ ] **Step 5: Commit**

```powershell
git add crates/agent/nomi-tools crates/agent/nomi-agent
git commit -m "refactor(tools): unify legacy command sessions on supervisor"
```

---

### Task 11: Add three-platform CI and architecture boundary gates

**Files:**
- Create: `.github/workflows/command-reliability.yml`
- Create: `scripts/check-process-runtime-boundary.mjs`
- Modify: `package.json`
- Create: `crates/shared/nomi-execution/tests/architecture_contract.rs`
- Modify: `crates/shared/README.md`

**Interfaces:**
- Produces `bun run check:process-runtime-boundary`.

- [ ] **Step 1: Write a failing boundary scanner**

The scanner checks production Rust files and fails when:

- `portable_pty::native_pty_system` appears outside `nomi-execution/src/platform/unix_pty.rs`;
- `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, negative-PID group kill, PDEATHSIG, kqueue watchdog, or `taskkill /T` appears outside `nomi-execution/src/platform`;
- `nomi-tools` calls `tokio::process::Command::output` or wraps output in `tokio::time::timeout`;
- a production command tool depends on old `Pty/ProcessStore` objects.

Allow explicit hand-off launch code by exact path allowlist; do not allow a wildcard directory exemption.

- [ ] **Step 2: Run and confirm the scanner identifies remaining duplicates**

Run: `bun run check:process-runtime-boundary`

Expected: fail until Tasks 8–10 have removed or isolated every duplicate.

- [ ] **Step 3: Add the OS matrix**

`.github/workflows/command-reliability.yml` matrix:

```yaml
strategy:
  fail-fast: false
  matrix:
    os: [windows-latest, macos-14, ubuntu-24.04]
```

Each job runs:

```text
cargo test -p nomi-execution -- --test-threads=1
cargo test -p nomi-tools -- --test-threads=1
cargo test -p nomifun-runtime -- --test-threads=1
```

Parent-death and process-tree tests use the separate harness binaries and are not mocked.

- [ ] **Step 4: Run Wave A final verification**

```powershell
bun run check:process-runtime-boundary
& "$env:USERPROFILE\.cargo\bin\cargo.exe" fmt --all -- --check
& "$env:USERPROFILE\.cargo\bin\cargo.exe" clippy -p nomi-execution -p nomifun-runtime -p nomi-tools -p nomi-agent --all-targets -- -D warnings
& "$env:USERPROFILE\.cargo\bin\cargo.exe" test -p nomi-execution -- --test-threads=1
& "$env:USERPROFILE\.cargo\bin\cargo.exe" test -p nomifun-runtime -- --test-threads=1
& "$env:USERPROFILE\.cargo\bin\cargo.exe" test -p nomi-tools -- --test-threads=1
& "$env:USERPROFILE\.cargo\bin\cargo.exe" test -p nomi-agent -- --test-threads=1
& "$env:USERPROFILE\.cargo\bin\cargo.exe" check --workspace --all-targets
```

Expected: all exit 0; Windows process probes show no helper/grandchild survivors.

- [ ] **Step 5: Commit**

```powershell
git add .github/workflows/command-reliability.yml scripts/check-process-runtime-boundary.mjs package.json crates/shared/README.md crates/shared/nomi-execution
git commit -m "ci: enforce cross-platform process reliability"
```

## Spec Coverage Check

| Approved design requirement | Wave A task |
|---|---|
| Shared backend-neutral execution crate | Task 1 |
| Immutable cwd/env/capability normalization | Task 1 |
| Bounded ordered output, Unicode and dropped-byte evidence | Task 2 |
| Single lifecycle and truthful cancel/lost state | Task 3 |
| Linux process group, PDEATHSIG and group watchdog | Task 4 |
| macOS process group and kqueue watchdog | Task 4 |
| Windows suspended spawn and execution-scoped Job | Task 5 |
| Real pipe vs PTY and ConPTY quick-exit semantics | Task 6 |
| UUIDv7 owner-scoped sessions, lease and reaper | Task 7 |
| One copy of runtime ownership primitives | Task 8 |
| Bash timeout cleanup, encoding, cwd and sandbox inheritance | Task 9 |
| Legacy exec/write nonzero, pipe/PTY and polling semantics | Task 10 |
| Three-platform CI and architectural regression prevention | Task 11 |

The remaining approved requirements—single model-visible action schema, typed ToolOutcome/ToolExecutionContext, RunController/shared budget, progress/completion audit, durable outbox/recovery, UI timeline, and final old-path deletion—belong to Waves B–E and are intentionally not implemented in this plan.

## Wave A Completion Gate

Wave A is complete only when:

- all eleven task commits exist;
- the three-platform workflow is green;
- Windows pipe and ConPTY use suspended execution-scoped Job ownership;
- Linux/macOS parent-death harnesses prove descendant group cleanup;
- cancel produces `Cancelled` only after reap, otherwise `Lost`;
- quick exit no longer waits remaining yield;
- output is bounded and Unicode/decode loss is explicit;
- invalid cwd never falls back;
- Bash/exec/write production internals all use one `ProcessSupervisor`;
- `nomifun-runtime` has no second Job/group/watchdog implementation;
- the architecture scanner passes.
