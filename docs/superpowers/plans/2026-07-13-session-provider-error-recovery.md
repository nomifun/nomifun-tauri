# Session Provider-Error Recovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Keep a Nomi conversation usable after Computer/Browser/image-heavy provider failures and model switches on Windows, macOS, and Linux.

**Architecture:** Repair the shared Rust pipeline at five boundaries: image production, history pruning, failed-turn rollback, persisted-history resume, and agent replacement. Provider retries remain bounded and side-effect-aware. Valid text and completed tool pairs are preserved.

**Tech Stack:** Rust, Tokio, serde, image, Nomi provider adapters, Nomi session persistence, backend conversation/task services.

## Global Constraints

- Do not clear valid conversation text or completed tool-call/result pairs.
- Do not retry after partial assistant output or any ambiguous tool side effect.
- Keep recovery bounded; no unbounded retry, image, or teardown loop.
- Use only platform-neutral recovery logic; preserve Windows, macOS, and Linux capture/input behavior.
- Log counts and sizes only, never prompt text, image data, credentials, or raw upstream bodies.

---

### Task 1: Bound image history by count and bytes

**Files:**
- Modify: `crates/agent/nomi-agent/src/engine.rs`
- Test: `crates/agent/nomi-agent/src/engine.rs`

**Interfaces:**
- Consumes: `ContentBlock::ToolResult { images, content, .. }` and `config.tools.max_recent_images`.
- Produces: `MAX_PROVIDER_REQUEST_IMAGE_DATA_BYTES` and byte-aware `AgentEngine::prune_old_tool_images()`.

- [ ] **Step 1: Write failing tests**

Add tests that construct three 3 MiB-equivalent base64 image strings, run
`prune_old_tool_images`, and assert that only the newest images fitting one padded
5 MiB decoded-image budget remain. Add a test that an individually oversized legacy
image is removed and receives exactly one omission note.

- [ ] **Step 2: Verify RED**

Run:

```bash
cargo test -p nomi-agent prune_old_tool_images -- --nocapture
```

Expected: the byte-budget tests fail because current pruning checks only count.

- [ ] **Step 3: Implement the byte budget**

Define the padded base64 ceiling without decoding:

```rust
const MAX_SINGLE_IMAGE_BYTES: usize = 5 * 1024 * 1024;
const MAX_PROVIDER_REQUEST_IMAGE_DATA_BYTES: usize =
    MAX_SINGLE_IMAGE_BYTES.div_ceil(3) * 4;
```

Walk images newest-to-oldest, retaining an image only when both remaining count and
remaining encoded-byte budgets allow it. Strip older/oversized values, preserve text,
and append one deterministic omission note per affected result.

- [ ] **Step 4: Verify GREEN**

Run the focused command from Step 2 and the full `nomi-agent` library suite.

### Task 2: Repair failed runs to their last safe checkpoint

**Files:**
- Modify: `crates/agent/nomi-agent/src/engine.rs`
- Test: `crates/agent/nomi-agent/src/engine.rs`

**Interfaces:**
- Consumes: the message length before a user turn and after complete tool-result groups.
- Produces: `repair_failed_run(first_new_message, safe_message_len)` and failure-safe `run_with_content`.

- [ ] **Step 1: Write failing tests**

Use the existing scripted/recording providers to cover:

```rust
// First request errors, second run succeeds.
// The second request must contain exactly one new user message.
```

and:

```rust
// Tool call + image result completes, next provider pass errors.
// Repair keeps the paired ToolUse/ToolResult text but removes image bytes.
```

- [ ] **Step 2: Verify RED**

Run the two test names directly. Expected: duplicate failed-turn user history and/or
stale image bytes remain.

- [ ] **Step 3: Implement checkpoint repair**

Track `safe_message_len` from the initial boundary, advance it only after a complete
tool-result group is appended, and on any `run_inner` error:

```rust
self.messages.truncate(safe_message_len);
self.last_turn_start_len = None;
self.prune_old_tool_images();
self.save_session();
```

Return the original `AgentError`. Do not truncate completed tool pairs.

- [ ] **Step 4: Verify GREEN**

Run both focused tests, then all `nomi-agent` tests.

### Task 3: Make persisted-history sanitization pair-preserving

**Files:**
- Modify: `crates/backend/nomifun-ai-agent/src/manager/nomi/history_sanitize.rs`
- Modify: `crates/backend/nomifun-ai-agent/src/factory/nomi.rs`
- Test: `crates/backend/nomifun-ai-agent/src/manager/nomi/history_sanitize.rs`

**Interfaces:**
- Consumes: ordered `Vec<Message>`, resumed provider label, selected provider label.
- Produces: `SessionRepairStats` and `sanitize_session_messages(messages, provider_changed)`.

- [ ] **Step 1: Write failing tests**

Add cases for a two-call assistant with only one matching immediate result, a stray
tool result, a late/non-adjacent result, assistant text plus an orphan tool call, and
a provider switch with Thinking and tool-result images.

- [ ] **Step 2: Verify RED**

Run:

```bash
cargo test -p nomifun-ai-agent history_sanitize -- --nocapture
```

Expected: the current whole-message/global-ID sanitizer leaves orphan results and
provider-specific blocks.

- [ ] **Step 3: Implement ordered repair**

For each assistant tool-call group, inspect only the immediately following user
tool-result group. Remove unmatched tool-call blocks and unmatched tool-result
blocks, then remove empty messages. Always strip historical tool-result images; when
`provider_changed`, also remove Thinking blocks. Return counts for removed messages,
tool calls, tool results, images, and thinking blocks.

- [ ] **Step 4: Wire and verify**

Compare `session.provider` with the resolved provider label in the factory/manager,
log repair counts, and run the focused and full `nomifun-ai-agent` suites.

### Task 4: Enforce a Nomi teardown barrier on model/workspace changes

**Files:**
- Modify: `crates/backend/nomifun-ai-agent/src/agent_runtime.rs`
- Modify: `crates/backend/nomifun-ai-agent/src/manager/nomi/agent.rs`
- Modify: `crates/backend/nomifun-conversation/src/service.rs`
- Test: corresponding inline test modules and `crates/backend/nomifun-conversation/src/service_test.rs`

**Interfaces:**
- Produces: `AgentRuntime::wait_until_finished(timeout)` and an actually-awaiting `NomiAgentManager::kill_and_wait`.
- Consumes: existing `IWorkerTaskManager::kill_and_wait` from conversation update.

- [ ] **Step 1: Write failing tests**

Test that runtime waiting blocks while Running and resolves after Finish, and that a
model update invokes/awaits the manager's awaitable kill path rather than `kill`.

- [ ] **Step 2: Verify RED**

Run focused runtime, Nomi manager, and conversation update tests. Expected: no wait
primitive exists and update records the non-awaiting kill.

- [ ] **Step 3: Implement the barrier**

Use a Tokio notification/watch-style condition, not a fixed sleep. Terminal emission
must notify waiters after session repair is complete. Bound the wait (five seconds),
warn on timeout, and let teardown continue rather than hanging forever. Change model
and workspace update to await `kill_and_wait` after persistence and before returning.

- [ ] **Step 4: Verify GREEN**

Run the focused and full affected backend suites.

### Task 5: Retry transient initial 5xx responses

**Files:**
- Modify: `crates/agent/nomi-providers/src/retry.rs`
- Test: `crates/agent/nomi-providers/src/retry.rs`

**Interfaces:**
- Produces: initial request retry predicate covering connection failures and API 500/502/503/504 only.

- [ ] **Step 1: Write failing tests**

With Tokio time paused, assert that an initial 502 followed by success is called twice,
while 400 and 429 are called once. Keep the existing connection-error expectations.

- [ ] **Step 2: Verify RED**

Run `cargo test -p nomi-providers retry::tests -- --nocapture`. Expected: 502 is not retried.

- [ ] **Step 3: Implement and verify**

Rename the helper to `with_initial_request_retry`, use a narrow predicate, retain the
current bounded backoff, update all four provider call sites, and run focused plus
full provider tests.

### Task 6: Bound native screenshot production and complete verification

**Files:**
- Modify: `crates/agent/nomi-computer/src/screen.rs`
- Modify: `docs/guides/computer-browser-use.md`
- Modify: `docs/guides/computer-browser-use.zh.md`
- Test: `crates/agent/nomi-computer/src/screen.rs`

**Interfaces:**
- Produces: byte-bounded `encode_png` output with geometry-consistent deterministic downscaling.

- [ ] **Step 1: Write a failing synthetic-image test**

Generate a deterministic high-entropy RGBA image large enough to exceed the 5 MiB
decoded ceiling, encode it, and assert the returned base64 payload fits the ceiling
and has nonzero dimensions reported by the bounded encoder.

- [ ] **Step 2: Verify RED**

Run `cargo test -p nomi-computer screen::tests -- --nocapture`. Expected: current
`encode_png` returns an oversized payload.

- [ ] **Step 3: Implement bounded encoding**

Encode once; while over budget and above the minimum edge, reduce dimensions by a
deterministic ratio with Triangle filtering and re-encode. Return the final image and
dimensions so Computer coordinate geometry matches what the model receives. Update
both Computer screenshot call sites and the bilingual guide.

- [ ] **Step 4: Verify all affected code**

Run:

```bash
cargo fmt --all -- --check
cargo test -p nomi-providers
cargo test -p nomi-agent
cargo test -p nomi-computer
cargo test -p nomifun-ai-agent
cargo test -p nomifun-conversation
cargo check -p nomifun-app --features computer-use,browser-use
```

Inspect available Rust targets and run Windows/Linux target checks when installed.
Confirm none of the shared tests are excluded with platform `cfg` attributes.

- [ ] **Step 5: Review final diff**

Run `git diff --check`, inspect `git diff`, confirm only scoped files changed, and
record any platform checks that could not run locally.
