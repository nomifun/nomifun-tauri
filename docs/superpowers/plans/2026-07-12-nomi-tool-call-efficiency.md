# Nomi Agent Tool-Call Efficiency Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reduce avoidable Nomi tool calls with supervised script batching, cache-aware multi-file reads, milestone-level plan updates, and structured per-run efficiency telemetry.

**Architecture:** Extend the existing `exec_command` and `Read` schemas so the long-term single-command-tool design remains intact. Keep all execution, including Python interpreter validation and runtime deadlines, on `ProcessSupervisor`; keep all file observations in `FileStateCache`; and make optimization choices model-visible through the existing system-prompt guidance. Legacy `cmd` keeps its numeric `ProcessStore`/`write_stdin` adapter; script mode is a bounded one-shot path that always reaches a terminal result and never retains or returns a session.

**Tech Stack:** Rust 2024, Tokio, serde_json, nomi-execution, tracing, Cargo test/clippy/fmt.

## Global Constraints

- Do not add a new model-visible command tool.
- Preserve all existing `cmd` and single `file_path` inputs.
- Script mode must use the existing supervisor, owner, working-directory normalization, capability, output, and cleanup/cancellation path, but must never insert a process binding into `ProcessStore` or return a `session_id`.
- Script mode accepts exactly `{ script, language, timeout, workdir? }`, always uses pipe transport, rejects `tty` and `yield_time_ms`, and enforces a hard 1–600,000 ms deadline.
- Python script source must be passed directly as one argv value to a host-provided Python 3 interpreter; never interpolate it into a shell command.
- Python is host-provided; never download or silently substitute a runtime. Validate every candidate under the same supervisor, capability, working-directory, and total script deadline before starting user source.
- Batch Read must populate the same file cache as single Read.
- Batch Read must run blocking filesystem work off Tokio workers, render members incrementally, and skip image encoding once its aggregate byte/slot budget is exhausted.
- Do not weaken the existing prior-error execution barrier.
- Do not add database, protocol, or UI schema changes.
- Do not commit or push unless the user explicitly asks.

---

### Task 0: Repair the macOS Zombie-Only Process-Group Terminal Path

This is a prerequisite discovered by the first script-mode execution tests: short-lived supervised commands could stall on macOS before script batching itself could be verified.

**Files:**
- Modify: `crates/shared/nomi-execution/src/platform/unix.rs`
- Modify: `crates/shared/nomi-execution/src/platform/macos_watchdog.rs`
- Modify: `crates/shared/nomi-execution/src/platform/unix_pty.rs`
- Test: `crates/shared/nomi-execution/tests/process_contract.rs`
- Verify: `crates/shared/nomi-execution/tests/pty_contract.rs`

**Interfaces:**
- Produces: host-side `seal_process_group_anchored_by(pgid, anchor_pid) -> io::Result<bool>` handling for the narrowly proven Darwin zombie-only state, including a legacy watchdog anchor after leader reap
- Produces: watchdog-side final group sealing guarded by kqueue leader-exit evidence
- Preserves: exact child reap, post-reap process-group absence proof, and ordinary `EPERM` failure behavior

- [x] **Step 1: Reproduce the baseline stall**

Run short-lived zero/non-zero pipe and PTY commands through `ProcessSupervisor`. Confirm that Darwin returns `EPERM` for `kill(-pgid, SIGKILL)` when the anchored group contains only zombie members and that the cleanup relay otherwise retries until timeout.

- [x] **Step 2: Add the narrow process-group proof**

Add a macOS-only fixed-stack proof that takes two complete `proc_listpgrppids` snapshots, rejects PID 0/1, duplicates, capacity saturation, membership changes, or a missing anchor, and queries `PROC_PIDTBSDINFO` with zombie lookup enabled for every member. On the host path, accept `EPERM` only when `waitid(..., WNOWAIT | WNOHANG)` proves the selected exact direct-child anchor exited both before and after the stable all-`SZOMB` proof. Propagate that same proved PID into the seal call so a legacy cleanup relay may safely use its joined watchdog after the group leader was already reaped. Carry watchdog group membership explicitly through spawn/lifecycle/cleanup state: set it only after the protocol proves a non-external join (validated host registration in the modern path, child ACK ordering in the legacy path), and never let an external-session PTY watchdog authorize a negative-PGID signal.

- [x] **Step 3: Apply the same rule to the watchdog**

Track `leader_exit_observed` from kqueue and accept watchdog-side `EPERM` only when that exit evidence, the stable all-zombie proof, and the original-host parent lease hold before and after inspection. Do not globally swallow permission failures.

- [x] **Step 4: Register the PTY reader before child start**

Duplicate the PTY master and a temporary slave guard, register the async reader before the watchdog/user fork, and wait for its first readiness poll within the existing setup deadline. Transfer that prepared reader into the process owner, abort it on setup failure, and cover two back-to-back fast terminal sessions so the second session cannot lose output to a pre-reader Darwin EIO.

- [x] **Step 5: Verify the lifecycle contracts**

Run the natural-exit unit tests plus `process_contract` and `pty_contract` quick-exit cases. Keep the Unix quick-exit bound at 250 ms and use a macOS-only 1-second bound to account for debug-helper startup while remaining far below a 30-second yield.

---

### Task 1: Cache-Aware Batched Read

**Files:**
- Modify: `crates/agent/nomi-tools/src/read.rs`
- Test: `crates/agent/nomi-tools/src/read.rs`

**Interfaces:**
- Consumes: legacy `{ file_path, offset?, limit? }` or new `{ file_paths, offset?, limit? }`
- Produces: `ReadTool::read_one(raw_path, offset, limit) -> ToolResult`
- Produces: batch output with one header per requested path and aggregated images/errors

- [x] **Step 1: Write failing batch tests**

Add tests that call `ReadTool::execute` with two files, assert both header/content blocks are present, and assert both normalized paths exist in the shared `FileStateCache`. Add tests for empty batches, more than 32 paths, duplicate de-duplication, mixed `file_path` + `file_paths`, and one missing member producing `is_error=true` while preserving successful content.

- [x] **Step 2: Run tests and verify RED**

Run: `cargo test -p nomi-tools read::tests --lib`

Expected: new tests fail because `file_paths` is not accepted and only one path is read.

- [x] **Step 3: Implement single/batch dispatch**

Refactor the existing body into:

```rust
const MAX_BATCH_FILES: usize = 32;

fn read_one(&self, raw_path: &str, offset: Option<usize>, limit: Option<usize>) -> ToolResult;
fn requested_paths(input: &Value) -> Result<Vec<String>, String>;
fn render_batch(entries: Vec<(String, ToolResult)>) -> ToolResult;
```

`requested_paths` accepts exactly one schema, rejects empty/oversized arrays, and de-duplicates exact path strings in first-seen order. `read_one` retains the existing resolver, cache, mtime, binary, image, and line-number behavior. Run the whole read on `spawn_blocking`; render members incrementally instead of collecting 32 complete results. Share the 100,000-byte result budget fairly across member bodies with UTF-8-safe middle truncation, cap aggregate base64 image data at one maximum single-image payload and 20 attachments, skip base64 allocation after either bound is exhausted, report omissions inline, and mark the result erroneous if any member failed.

- [x] **Step 4: Update schema and description**

Add `file_paths` with `minItems=1`, `maxItems=32`, and a schema `oneOf` requiring exactly one of `file_path`/`file_paths`. Tell the model to use batch mode when paths are already known and the same slice is needed.

- [x] **Step 5: Verify GREEN**

Run: `cargo test -p nomi-tools read::tests --lib`

Expected: all Read unit tests pass, including existing single-file behavior.

### Task 2: Supervised Script Mode on `exec_command`

**Files:**
- Modify: `crates/agent/nomi-tools/src/exec_command.rs`
- Modify: `crates/agent/nomi-tools/Cargo.toml`
- Modify: `Cargo.lock`
- Test: `crates/agent/nomi-tools/src/exec_command.rs`

**Interfaces:**
- Consumes: legacy `{ cmd, workdir?, tty?, yield_time_ms? }` or strict script `{ script, language: shell|python, timeout, workdir? }`
- Produces: `requested_invocation(input) -> Result<PreparedInvocation, String>` with `InvocationMode::Legacy` or `InvocationMode::Script`
- Produces: completed and timed-out script results prefixed with mode, language, interpreter, workdir, and elapsed time
- Preserves: existing legacy workdir/TTY/yield/numeric-session behavior only for `cmd`

- [x] **Step 1: Write failing parser and execution tests**

Cover legacy `cmd`, schema compatibility, shell script mapping, direct Python program mapping, both/neither input rejection, blank source, missing/unknown language, missing/out-of-range timeout, any `tty` or `yield_time_ms` in script mode, one real multiline shell script, Unicode/literal Python source, bounded `describe` output, and a timeout that cancels without retaining a live session.

- [x] **Step 2: Run tests and verify RED**

Run: `cargo test -p nomi-tools exec_command --lib`

Expected: new script inputs fail because `cmd` is currently mandatory.

- [x] **Step 3: Implement command selection**

Add a focused invocation model:

```rust
struct PreparedInvocation {
    command: CommandSpec,
    env: BTreeMap<OsString, OsString>,
    transport: Transport,
    mode: InvocationMode,
}

enum InvocationMode {
    Legacy { yield_ms: u64 },
    Script { language: &'static str, interpreter: String, timeout_ms: u64 },
}

fn requested_invocation(input: &Value) -> Result<PreparedInvocation, String>;
fn python_command(script: String) -> Result<(CommandSpec, String), String>;
```

Require exactly one of `cmd` or `script`. Script mode requires non-blank source, `language`, and integer `timeout` in 1–600,000 ms; it rejects the presence of `tty` and `yield_time_ms` and always selects pipe transport. Map shell to literal PowerShell source on Windows (no legacy compatibility rewrite) and POSIX `sh` on macOS/Linux. Add the workspace `which` dependency, collect host Python candidates, and validate them with a supervised `-I -c` Python-3 marker probe capped at two seconds and by the total script deadline. Divide the remaining probe budget among remaining candidates and reserve a bounded cleanup slice inside every slot, so a hanging earlier Windows launcher cannot consume its successors' validation time. If the requested script timeout cannot contain even one safe validation window, report script-timeout insufficiency rather than `python_unavailable`. Then build direct argv (`python3 -u -c <script>` on Unix; `py -3`, then `python3`, then `python` on Windows), keeping source as one literal argv value and returning stable `python_unavailable` before user source when no candidate passes. Set `PYTHONUTF8=1` and `PYTHONIOENCODING=utf-8`.

- [x] **Step 4: Add one-shot hard-deadline execution**

Put the absolute script deadline into `ExecutionPolicy` and enforce it inside `ProcessSupervisor`, with `TimedOut` distinct from user cancellation. Preserve a natural exit observed before the deadline even if final output draining completes slightly later; classify an exit observed at or after the boundary as timed out. Retain output in `TimedOut` and `Lost`, cap script output at 48,000 bytes, report that dependent effects must be inspected, and never insert the script process into `ProcessStore` or return a `session_id`. The tool-side deadline branch calls the supervisor's timeout path as a defensive adapter. Legacy `cmd` retains its current yield/session branches unchanged.

- [x] **Step 5: Update schema, description, and `describe`**

Use a strict schema `oneOf` for legacy command versus script mode. Document deterministic batching, hard timeout, one-shot non-interactive behavior, Python host dependency, direct execution, failure checking, bounded summaries, and the prohibition on using scripts to bypass dedicated tools. Include script language and an 80-character UTF-8-safe source preview in `describe`.

- [x] **Step 6: Verify GREEN**

Run: `cargo test -p nomi-tools exec_command --lib`

Expected: new and legacy exec tests pass.

### Task 3: Preserve Native `exec_command`/`write_stdin` Precedence over MCP Tools

**Files:**
- Modify: `crates/agent/nomi-agent/src/bootstrap.rs`
- Modify: `crates/agent/nomi-agent/tests/bootstrap_test.rs`
- Modify: `crates/shared/nomi-execution/tests/architecture_contract.rs`

**Interfaces:**
- Produces: native `exec_command` and `write_stdin` registrations before the MCP built-in-name snapshot
- Preserves: one shared `ProcessSupervisor` and one numeric `ProcessStore` adapter for the native legacy pair

- [x] **Step 1: Write registration-order tests**

Assert that the default native tool set includes `exec_command` and `write_stdin`, allowlists may include or omit the pair as configured, and the architecture contract requires both registrations to precede the `builtin_names` snapshot.

- [x] **Step 2: Move native registration before the snapshot**

Create the `ProcessStore`, register native `exec_command` and `write_stdin`, and only then collect `builtin_names` for MCP proxy registration. A same-named MCP tool must therefore be namespaced rather than shadow the native registry entry.

- [x] **Step 3: Verify GREEN**

Run:

```bash
cargo test -p nomi-agent --test bootstrap_test
cargo test -p nomi-execution --test architecture_contract native_exec_tools_are_registered_before_mcp_collision_snapshot
```

Expected: native tool visibility, allowlist behavior, and registration-order contracts pass.

### Task 4: Model Routing and Plan-Density Guidance

**Files:**
- Modify: `crates/agent/nomi-agent/src/context.rs`
- Modify: `crates/agent/nomi-tools/src/update_plan.rs`
- Modify: `crates/agent/nomi-agent/tests/tool_guidance_prompt_test.rs`
- Test: `crates/agent/nomi-tools/src/update_plan.rs`

**Interfaces:**
- Produces: model-visible rules for batch Read, ApplyPatch, deterministic scripts, and retained checkpoints
- Produces: milestone-level `update_plan` contract

- [x] **Step 1: Write failing prompt/description tests**

Assert the prompt contains `file_paths`, `ApplyPatch`, `deterministic`, `script`, `intermediate result`, and `meaningful milestone`. Assert update_plan guidance says not to update after individual tool calls or internal sub-steps.

- [x] **Step 2: Run tests and verify RED**

Run:

```bash
cargo test -p nomi-agent --test tool_guidance_prompt_test tool_call_efficiency
cargo test -p nomi-tools update_plan --lib
```

Expected: assertions fail because the routing and milestone language is absent.

- [x] **Step 3: Add concise routing rules**

Extend `tool_usage_guidance()` with availability-aware rules that select batch Read and ApplyPatch first when those tools are registered, use `exec_command` script mode only for deterministic/local/non-interactive batches when supported, require precondition/error/output discipline, and preserve separate calls for state-dependent/approval/browser/UI/MCP/destructive work. This keeps reduced subagent registries from being instructed to call unavailable tools.

- [x] **Step 4: Reduce plan update noise**

Change system, tool description, and tool-result reminders from every internal step to every user-meaningful milestone. Explicitly prohibit update calls after individual tool invocations or internal sub-steps while retaining final verification and all-complete snapshot requirements.

- [x] **Step 5: Verify GREEN**

Run the two focused commands from Step 2 and the full `tool_guidance_prompt_test` file.

Expected: efficiency and existing checkpoint tests pass.

### Task 5: Per-Run Tool Efficiency Telemetry

**Files:**
- Modify: `crates/agent/nomi-agent/src/engine.rs`
- Modify: `crates/agent/nomi-agent/src/orchestration.rs`
- Test: `crates/agent/nomi-agent/src/engine.rs`

**Interfaces:**
- Produces: private `ToolEfficiencyStats`
- Produces: one structured `nomi_agent::tool_efficiency` INFO event for every `AgentEngine::run` return
- Does not change: `AgentResult`, protocol, persistence, or UI types

- [x] **Step 1: Write failing pure accounting tests**

Construct representative `ContentBlock::ToolUse`/`ToolResult` vectors and assert model turn attempts, model turns with tools, total calls, max calls in a model turn, script-mode calls, batch-read requested-file count, errors, and exact prior-error-barrier skips.

- [x] **Step 2: Run tests and verify RED**

Run: `cargo test -p nomi-agent tool_efficiency --lib`

Expected: compilation fails because `ToolEfficiencyStats` does not exist.

- [x] **Step 3: Implement local counters and structured logging**

Add a private stats struct with methods:

```rust
fn observe_model_turn_attempt(&mut self);
fn observe_calls(&mut self, tool_calls: &[ContentBlock]);
fn observe_results(&mut self, results: &[ContentBlock]);
fn log(
    &self,
    session_id: &str,
    msg_id: &str,
    result: &Result<AgentResult, AgentError>,
);
```

Create it in the public `run` wrapper and pass it into `run_inner`, so slash commands and every success/error return are logged exactly once inside the existing `agent_run` span. Observe a model-turn attempt before each provider stream, tool calls after each stream (including the stream-error early return), and results after orchestration. Normalize tool inputs with the same schema coercion used by execution so stringified objects/arrays are classified correctly. Reuse the exported `SKIPPED_AFTER_PRIOR_ERROR` constant for exact skip accounting, and classify cooperative cancellation as `terminal=cancelled`, `stop_reason=cancelled`, without a spurious error kind. Log stable structured fields for session/message identity, terminal state, stop/error reason, turn count, and every counter rather than formatting one opaque message.

- [x] **Step 4: Verify GREEN**

Run: `cargo test -p nomi-agent tool_efficiency --lib`

Expected: accounting tests pass and existing engine tests compile unchanged.

### Task 6: Integrated Verification

**Files:**
- Modify: only files already listed if verification exposes a defect

- [x] **Step 1: Format**

Run: `cargo fmt --all -- --check`

- [x] **Step 2: Run touched-crate tests**

Run:

```bash
cargo test -p nomi-execution -- --test-threads=1
cargo test -p nomi-tools -- --test-threads=1
cargo test -p nomi-agent -- --test-threads=1
cargo check -p nomi-execution --target x86_64-apple-darwin --all-targets --locked
```

- [x] **Step 3: Lint touched crates**

Run:

```bash
bun run check:process-runtime-boundary
cargo clippy -p nomi-execution -p nomi-tools -p nomi-agent --all-targets --locked -- -D warnings
```

- [x] **Step 4: Audit the diff**

Run:

```bash
git diff --check
git status --short
```

Confirm no unrelated files changed and no generated artifacts are tracked.
