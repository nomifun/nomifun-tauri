# Agent Reliability Wave A Pause / Handoff

Date: 2026-07-10  
Status: intentionally paused at the user's request  
Workspace: `C:\Users\rika0\code\nomifun\nomifun-tauri`  
Isolated worktree: `C:\Users\rika0\code\nomifun\nomifun-tauri\.worktrees\agent-reliability-wave-a`  
Branch: `codex/agent-reliability-wave-a`

## Objective

Replace Nomifun's fragmented process, timeout, retry, session, and Agent-loop
behavior with a single cross-platform reliability architecture. Windows is the
highest-risk platform, but the design and acceptance gates cover Windows,
macOS, and Linux. The user explicitly authorized architecture-level changes
and does not want local patches or scope minimization.

## Canonical design and implementation plan

- Design: `docs/superpowers/specs/2026-07-10-nomifun-agent-command-reliability-design.md`
- Wave A plan: `docs/superpowers/plans/2026-07-10-agent-reliability-wave-a-execution-kernel.md`

The design covers five waves. The current plan implements Wave A only:

1. Shared `nomi-execution` kernel.
2. Bounded ordered output and explicit decoding metadata.
3. Supervisor and deterministic child-process helper.
4. Linux/macOS process groups and parent-death watchdogs.
5. Windows suspended spawn plus execution-scoped Job Objects.
6. Unix PTY and Windows suspended ConPTY.
7. UUIDv7 owner-scoped session leases and reaping.
8. Thin `nomifun-runtime` compatibility facade.
9. Bash/sandbox migration.
10. Legacy exec/write internal migration.
11. Three-platform CI and architecture gates.

Waves B-E must receive separate plans based on the actual interfaces delivered
by the preceding wave. Do not fold RunController, persistence/UI, or old-path
deletion into an unrelated Wave A task.

## Git state at pause

The branch contains these relevant commits:

```text
c02e00f feat(execution): define cross-platform process contracts
00e5e34 chore: ignore isolated worktrees
356e5aa docs: plan execution kernel reliability wave
51f3c8e docs: design cross-platform agent reliability architecture
```

`origin/main` was `22bb7ec` when work started. Nothing has been pushed or
merged. The main checkout is not the implementation checkout; continue only in
the isolated worktree and branch above.

## Baseline evidence

On Windows, before production implementation:

- `nomi-tools`: 233 tests passed.
- `nomi-agent --lib`: 449 tests passed.
- `nomifun-runtime`: 40 passed, 5 failed.

The five runtime failures are pre-existing Windows portability defects. The
tests unconditionally invoke bare `sh`; a standard Windows PATH in this
environment provides `cmd.exe` and PowerShell but no `sh.exe`. The failing
tests are:

- `spawn::tests::agent_allows_stdio_override`
- `spawn::tests::agent_strips_env_pollution`
- `spawn::tests::clean_cli_captures_stdout_and_strips_env_pollution`
- `spawn::tests::clean_cli_sets_no_color_and_term_dumb`
- `spawn::tests::spawn_returns_child_with_pid`

Do not misclassify these as a Task 1 regression. They remain in scope for the
cross-platform runtime migration and CI gate.

On this workstation, invoke Cargo as:

```powershell
& "$env:USERPROFILE\.cargo\bin\cargo.exe" ...
```

`cargo fmt --all` currently fails on Windows with error 206 (workspace command
line too long), including through a short drive mapping. Package-scoped
`cargo fmt -p nomi-execution` and `--check` succeed. Preserve this evidence and
do not hide the workspace formatting limitation.

## Task 1 state

Task 1 implementation is committed at full SHA:

```text
c02e00fb661247abcbce2ede0af29b6d2b796429
```

TDD evidence:

- RED: expected `E0432` unresolved imports before public contracts existed.
- GREEN: focused request contract suite passed 15/15.
- GREEN: full `nomi-execution` crate tests passed.
- GREEN: package-scoped formatting and format check passed.
- Dependency check found no backend, Agent, DB, UI, conversation, or Bun
  dependency in `nomi-execution`.

Task 1 resolved a real ambiguity in the plan. The binding execution facts are:

```rust
pub enum ExecutionOutcome {
    Exited { code: Option<i32>, signal: Option<i32>, output: OutputSnapshot, cleanup: CleanupReport },
    SpawnFailed(SpawnFailure),
    Cancelled { output: OutputSnapshot, cleanup: CleanupReport },
    TimedOut { output: OutputSnapshot, cleanup: CleanupReport },
    Lost { last_known: ProcessSnapshot, cleanup: CleanupReport },
}

pub enum ExecutionEvent {
    Output { seq: u64, stream: OutputStream, bytes: Vec<u8>, text: String, encoding: EncodingMetadata },
    StateChanged { seq: u64, state: ProcessState },
    OutputDropped { seq: u64, bytes: u64 },
}
```

The supporting shapes are frozen in the Task 1 brief: absolute byte
`OutputCursor`, `Stdout/Stderr/Pty` identity, raw bytes plus text in each
`OutputChunk`, explicit encoding error counts, structured spawn failure,
process snapshot timestamps/state, exact cleanup facts, and no Wave B
ToolOutcome/retry/side-effect semantics.

## Exact pause point

Task 1 implementation is complete but its independent task-scoped review is
not complete. A fresh read-only reviewer had started and was intentionally
interrupted when the user requested this pause. No reviewer verdict was
produced; do not treat the task as approved yet.

Worktree-local SDD artifacts are intentionally ignored by Git but remain on
disk:

- `.superpowers/sdd/progress.md`
- `.superpowers/sdd/task-1-brief.md`
- `.superpowers/sdd/task-1-report.md`
- `.superpowers/sdd/review-00e5e34..c02e00f.diff`
- `.superpowers/sdd/task-2-brief.md`

The frozen Task 1 review range is:

```text
base = 00e5e34c1699c7841d0282e3c8b8df2e83de689a
head = c02e00fb661247abcbce2ede0af29b6d2b796429
```

Restart that review before implementing Task 2. The reviewer must read the
brief, implementer report, and diff package once; verify spec compliance and
code quality with file/line evidence; remain read-only; and classify findings
as Critical, Important, or Minor. Fix every Critical/Important finding and
re-review the entire task range. Only after an Approved verdict may the SDD
ledger mark Task 1 complete.

Task 2's brief is prepared, but no Task 2 implementation has started.

## Binding architecture invariants

- `nomi-execution` remains backend-neutral and under `crates/shared`.
- Every non-hand-off child obtains an execution owner before user code runs;
  ownership failure is spawn failure, never silent fallback.
- Windows pipe and PTY use suspended process creation, an execution-scoped Job
  with `KILL_ON_JOB_CLOSE`, then resume. The new kernel must not use the current
  global Job, `taskkill`, or portable-pty Windows spawn.
- Linux uses a process group, `PDEATHSIG`, and a descendant-proof parent-death
  watchdog.
- macOS uses a process group and the existing double-fork/kqueue watchdog moved
  into the shared crate; Seatbelt is enforced at the execution boundary.
- Cancellation SLA is exactly 5 seconds: interrupt 1s, terminate 1s,
  force-kill/reap 3s. Unproven cleanup is `Lost`, never `Cancelled`.
- Yield is not a timeout and never kills. Observed child exit must surface
  within 250ms.
- Output is bounded at 4 MiB by default while reading, with exact dropped byte
  counts and explicit decoding errors.
- Execution paths retain `PathBuf`/`OsString`; invalid or unauthorized cwd
  fails before spawn and never falls back to a profile/home directory.
- Wave A preserves model-visible Bash/exec/write schemas while replacing their
  production internals with one supervisor.
- Every task follows RED -> GREEN -> focused regression -> one task commit ->
  independent spec/quality review.

## Resume checklist

1. Open the isolated worktree and verify branch/status/log.
2. Read this handoff, the canonical design, the complete Wave A plan, and
   `.superpowers/sdd/progress.md` before changing code.
3. Use `superpowers:subagent-driven-development`,
   `superpowers:test-driven-development`, and systematic debugging for any
   unexpected failure.
4. Restart Task 1 review using the frozen range and artifacts above.
5. If approved, update the local SDD ledger and begin Task 2 from its brief.
6. If findings exist, dispatch a focused fix agent, append test evidence to the
   Task 1 report, create a new review package for the same base through the new
   head, and re-review.
7. Continue automatically through all Wave A tasks with a fresh implementer
   and reviewer per task. Do not pause merely because a task is difficult.
8. After Task 11, run a broad branch review and full cross-platform/architecture
   verification before claiming Wave A complete.

## Ready-to-use wake prompt

```text
Resume the Nomifun cross-platform Agent reliability program from the persisted
checkpoint. Work only in:
C:\Users\rika0\code\nomifun\nomifun-tauri\.worktrees\agent-reliability-wave-a
on branch codex/agent-reliability-wave-a.

First read completely:
1) docs/superpowers/handoffs/2026-07-10-agent-reliability-wave-a-pause.md
2) docs/superpowers/specs/2026-07-10-nomifun-agent-command-reliability-design.md
3) docs/superpowers/plans/2026-07-10-agent-reliability-wave-a-execution-kernel.md
4) .superpowers/sdd/progress.md

Do not restart the audit or redesign. Task 1 implementation is committed at
c02e00fb661247abcbce2ede0af29b6d2b796429 and passed its implementation tests,
but its independent review was interrupted and has no verdict. Restart the
read-only Task 1 spec/quality review first using:
- brief: .superpowers/sdd/task-1-brief.md
- report: .superpowers/sdd/task-1-report.md
- diff: .superpowers/sdd/review-00e5e34..c02e00f.diff
- base: 00e5e34c1699c7841d0282e3c8b8df2e83de689a
- head: c02e00fb661247abcbce2ede0af29b6d2b796429

Fix and re-review every Critical/Important finding. Only after approval mark
Task 1 complete, then execute Task 2 from .superpowers/sdd/task-2-brief.md.
Continue with subagent-driven development, strict TDD, one reviewable commit
per task, and an independent spec/quality review after every task. Preserve all
architecture invariants in the handoff. Use
& "$env:USERPROFILE\.cargo\bin\cargo.exe" on this Windows host. The existing
nomifun-runtime 5-test bare-sh failure is a recorded pre-existing portability
defect, not a Task 1 regression. Do not claim completion until the planned
Windows/macOS/Linux gates and broad branch review pass.
```
