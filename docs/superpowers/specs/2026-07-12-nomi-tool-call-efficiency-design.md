# Nomi Agent Tool-Call Efficiency Design

> Date: 2026-07-12
> Status: implemented and verified
> Scope: native Nomi Agent tools and system guidance

## Decision summary

Nomi will reduce avoidable tool calls without weakening failure checkpoints by adding two batch-capable modes to existing tools instead of adding more model-visible tools:

1. `exec_command` gains a supervised script mode for deterministic local batches.
2. `Read` gains `file_paths` batch input while preserving the native file-state cache.
3. System guidance routes only deterministic, homogeneous, non-interactive work into scripts and keeps state-dependent or approval-sensitive work as separate calls.
4. `update_plan` guidance moves from microscopic step updates to meaningful milestone updates.
5. The engine emits one structured efficiency summary per user run so call count, batching, failures, and script use can be measured before further tuning.

This design deliberately does not add a separate `RunScript` tool. The approved command-reliability architecture converges on a single model-visible `exec_command`, so script execution becomes a backward-compatible mode of that tool.

## Goals

- Reduce tool invocations for repeated deterministic local operations.
- Reduce model round trips for reading several known files.
- Preserve read-before-edit and stale-file protection.
- Preserve the same process supervisor, owner identity, working-directory normalization, capability policy, output, and cancellation path used by `exec_command` today, while keeping numeric sessions exclusive to legacy `cmd` mode.
- Keep failures visible and prevent a script from being recommended when an intermediate result needs model judgment.
- Establish per-run measurements without adding a database or UI migration.

## Non-goals

- Automatically fuse arbitrary tool calls at runtime.
- Bundle a Python runtime with Nomi.
- Replace `Bash`, `write_stdin`, or the wider command-reliability Wave B schema in this change.
- Bypass dedicated file, browser, MCP, approval, or UI tools.
- Add a metrics dashboard or persistence schema.
- Guarantee filesystem transactionality across multiple physical writes.

## 1. `exec_command` script mode

The existing schema remains valid:

```json
{ "cmd": "cargo test", "workdir": "/workspace", "tty": false, "yield_time_ms": 10000 }
```

The new mode is:

```json
{
  "script": "...",
  "language": "shell | python",
  "workdir": "/workspace",
  "timeout": 120000
}
```

Exactly one of `cmd` or `script` is required. `language` and `timeout` are required with `script` and forbidden without it. Script mode is non-interactive, so both `tty` and legacy `yield_time_ms` are rejected. Its timeout is a hard 1–600,000 ms absolute deadline enforced by `ProcessSupervisor`: expiry triggers supervised process-tree timeout cleanup and returns an error with partial output. A natural exit observed before the deadline remains `Exited` even if final output draining crosses the boundary; an exit observed at or after it is `TimedOut`. Script mode never returns a live session, while existing `cmd` behavior, numeric sessions, and `write_stdin` polling remain unchanged.

Execution mapping:

- `language=shell`: POSIX `sh` on macOS/Linux; on Windows, source is passed literally after PowerShell's `-Command` flags without the legacy compatibility prologue/exit-status rewrite.
- `language=python`: direct program execution with `-u -c <script>`, using `python3` on macOS/Linux and `py -3`, `python3`, or `python` in that order on Windows. Source is passed as one argv value and is never interpolated into a shell command.

Python availability remains a host property. Path discovery is followed by a supervised `-I -c` marker probe for every candidate under the same capability and working-directory policy. Probes share a two-second maximum and consume the script's absolute deadline; the remaining budget is divided among remaining candidates, with a bounded interrupt/terminate/reap slice reserved inside every candidate slot, so a hanging earlier Windows launcher cannot starve a valid later fallback. If the requested script timeout cannot contain one safe validation window, the result is explicitly a script-timeout insufficiency, not a false `python_unavailable`. Only exit code zero plus the exact Python-3 marker is accepted; otherwise the tool produces the stable `python_unavailable` validation error before user source starts. No hidden download or non-Python fallback is attempted.

The tool does not silently rewrite scripts. Model guidance requires scripts to validate preconditions, fail non-zero at the first failed dependent operation, bound output, and print a concise final summary. This avoids platform-specific semantic surprises from automatically injecting strict-mode prologues.

## 2. Batched `Read`

The existing single-file input remains valid. The new input is:

```json
{
  "file_paths": ["src/a.rs", "src/b.rs"],
  "offset": 0,
  "limit": 200
}
```

Exactly one of `file_path` or `file_paths` is accepted. A batch contains 1 to 32 paths. Duplicate paths are read once while preserving first-seen order. The common `offset` and `limit` apply to every file.

Each file goes through the same resolution, image handling, binary detection, line numbering, mtime lookup, and `FileStateCache` insertion as a single `Read`. The complete batch runs on Tokio's blocking pool and is rendered incrementally, so it does not retain 32 full member results on an async worker. Output uses explicit per-file headers and shares the 100,000-byte result budget fairly across member bodies with UTF-8-safe middle truncation. Aggregate base64 image attachments are capped at one maximum single-image payload and 20 attachments; once either bound is exhausted, later image files are reported inline without allocating base64 data. A failed member is reported inline and makes the overall tool result an error, while successful members still populate the cache and remain visible for diagnosis.

This mode is preferred over Shell/Python for source inspection because shell reads do not populate the native cache required by Edit and ApplyPatch.

## 3. Routing guidance

The system prompt will route work as follows:

- When Read is available, use one `Read(file_paths=[...])` when several already-known files need the same slice.
- When ApplyPatch is available, use one `ApplyPatch` when one logical edit spans files.
- When `exec_command` script mode is available, use it when operations are deterministic, homogeneous, local, non-interactive, need no intermediate approval, and can return a bounded summary.
- Keep separate calls when the next step depends on observed output, when a user approval boundary matters, or when work touches browser/UI/MCP/external systems/destructive state.
- Do not use scripts to bypass dedicated file tools or read-before-edit checks.
- Give a deterministic script a realistic explicit timeout. If the next action depends on intermediate output, keep separate calls and inspect each checkpoint instead of turning the script into an interactive session.

## 4. Plan update density

When `update_plan` is registered for non-trivial work, a plan step must represent a user-meaningful milestone. The model must not call it after each tool invocation or internal sub-step. It updates after a milestone changes state and once more after verification. This wording is conditional because reduced subagent registries may intentionally omit the tool.

## 5. Efficiency telemetry

Each `AgentEngine::run` maintains local counters:

- model turn attempts;
- model turns containing tools;
- total tool calls;
- maximum calls emitted in one model turn;
- `exec_command` script-mode calls;
- number of files requested through batched Read;
- error results;
- results skipped by the prior-error barrier.

At every `run` return, including successful, slash-command, cancellation, maximum-turn, provider, API, and context failures, the engine emits exactly one structured `tracing::info!` event under `nomi_agent::tool_efficiency`. The event is keyed by `session_id` and `msg_id` and includes terminal status, stop reason, error kind, agent turns, and the counters above. It does not change `AgentResult`, protocol events, database rows, or visible chat output.

Classification first applies the same input coercion as the tool schema path, so object/array arguments serialized as strings are still counted as script or batch calls. Cooperative cancellation is logged as a cancelled terminal/stop reason with no error kind, rather than being misreported as an ordinary success or provider error.

## 6. Safety and error handling

- Script mode uses the same `ProcessSupervisor`, owner identity, working-directory normalization, capability policy, sandbox boundary, output buffer, runtime-deadline watcher, and cleanup/cancellation path as legacy `exec_command`. It always uses pipe transport and never inserts a binding into `ProcessStore` or returns a `session_id`. Deadline watchers hold only weak session references and stop when the lifecycle becomes terminal.
- `ExecutionOutcome::Lost` retains its frozen output snapshot, just like exited/cancelled/timed-out terminal results. Bash prefers this terminal snapshot over the pre-cleanup timeout snapshot, and legacy session rendering preserves exact cursor-gap metadata, so cleanup uncertainty does not erase diagnostics.
- Script output is capped at 48,000 bytes and Python execution forces UTF-8 stdin/stdout behavior.
- Batch Read keeps the 32-file bound and existing 100,000-byte text result cap; aggregate base64 image attachments are capped at one maximum single-image payload and 20 attachment slots.
- Before every provider request, recent-image pruning counts individual attachments rather than image-bearing result blocks, enforces the configured bound and the global 20-image provider ceiling, and appends an explicit text note for every omitted attachment range.
- Any invalid mixed schema is rejected before process or filesystem work.
- Script mode never reports Python as available unless host interpreter resolution succeeds; a missing interpreter returns `python_unavailable` before process start.
- A tool-level failure still activates the existing same-turn barrier.
- Commands whose internal stages need separate approval or inspection must not be combined into one script.

## 6.1. Runtime prerequisite discovered during implementation

Baseline testing exposed a Darwin lifecycle defect: after a watchdog sealed a process group, macOS can return `EPERM` (not `ESRCH`) when a redundant group signal targets a group containing only zombies. The lifecycle now accepts that case only after an independent anchor-exit proof plus two complete, stable `proc_listpgrppids` snapshots; every unique member must be revalidated through `PROC_PIDTBSDINFO` as the same-group `SZOMB`, and the host rechecks its exact `waitid(..., WNOWAIT)` lease after inspection. The proved anchor PID is propagated into the final seal, allowing the legacy cleanup relay to use its exact joined watchdog after the leader was already reaped. Watchdog membership is carried explicitly and becomes true only after the protocol proves a non-external join (validated host registration in the modern path, child ACK ordering in the legacy path); an external-session PTY watchdog remains outside the target group, may be reaped individually, and can never authorize `kill(-pgid, ...)`. If no in-group anchor remains, cleanup quarantines the cached PGID and completes as unproven instead of retrying forever. The external-session watchdog additionally requires the original host to remain its parent before and after its provisional proof; the host retains the sole unreaped leader lease, then always repeats the seal under `WNOWAIT` before leader reap and group-absence proof. Existing exact reaps and post-reap process-group absence remain mandatory, so arbitrary `EPERM` failures are still rejected.

The same tests exposed a fast-exit PTY output race: a child could write and close its terminal before the Tokio reader registered readiness, causing Darwin to report EIO with no retained output. PTY setup now duplicates the master, registers the reader before the watchdog/user fork, waits for that readiness handshake within the shared setup deadline, and only then starts the child. Back-to-back fast PTY sessions now preserve their first output bytes.

Native `exec_command` and `write_stdin` are also registered before the MCP built-in-name snapshot so a same-named MCP tool is namespaced instead of shadowing the native implementation.

Full-package verification also exposed two legacy spawn-failure edge cases. A pre-registration failure such as an invalid working directory no longer waits on a fresh five-second registration timeout, and an exec failure already reaped by Tokio is accepted only after process-group absence is proven. These changes retain failure diagnostics without reporting a false ownership-loss suffix.

## 7. Verification

Focused tests cover schema compatibility, invalid combinations, direct Python command-spec mapping, shell/Python execution, hard timeout without a retained session, batch cache population, partial batch failure, prompt routing, milestone guidance, metrics accounting, native/MCP registration precedence, and the macOS process/PTY lifecycle contracts. Final verification runs `cargo fmt --all -- --check`, the touched `nomi-execution`, `nomi-tools`, and `nomi-agent` tests, the process-runtime architecture boundary check, `cargo clippy` for all three touched crates, and `git diff --check`.

Final result: all 1,159 touched-package tests passed (`nomi-execution` 199, `nomi-tools` 297, `nomi-agent` 663). `cargo clippy -p nomi-execution -p nomi-tools -p nomi-agent --all-targets --locked -- -D warnings`, `bun run check:process-runtime-boundary`, `cargo check -p nomi-execution --target x86_64-apple-darwin --all-targets --locked`, formatting, and diff checks also passed.
