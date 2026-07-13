# Knowledge Read Cascade Repair Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prevent proactive knowledge reads from failing for lack of a handle and render prior-error-barrier calls as skipped instead of independent failures.

**Architecture:** Keep the existing knowledge handle codec and same-turn failure barrier unchanged. Supply the already-generated opaque handle in proactive context, align every search description with `knowledge_read(handle)`, and derive a frontend-only `skipped` presentation marker from the stable barrier output.

**Tech Stack:** Rust, Tokio, rmcp, TypeScript, React, Bun test, i18next.

## Global Constraints

- Preserve the hard prior-error execution barrier.
- Do not add Debian-, Docker-, locale-, Unicode-, or shell-command rewriting.
- Do not change opaque handle encoding or knowledge-base authorization.
- Only the exact stable prior-error result may be shown as skipped.
- Genuine tool errors must remain failed.

---

### Task 1: Repair the knowledge retrieval contract

**Files:**
- Modify: `crates/backend/nomifun-ai-agent/src/manager/nomi/agent.rs`
- Modify: `crates/agent/nomi-agent/src/knowledge_tools.rs`
- Modify: `crates/backend/nomifun-app/src/commands/knowledge_stdio.rs`

**Interfaces:**
- Consumes: `KnowledgeHit { handle, kb_name, rel_path, heading, snippet }`.
- Produces: proactive context and tool descriptions that always provide and require the exact opaque handle for `knowledge_read`.

- [ ] **Step 1: Write failing proactive-context and description tests**

Extend `knowledge_prelude_tests::prepend_knowledge_context_formats_hits_and_passthrough_when_empty` with:

```rust
assert!(out.contains("handle: h"), "proactive hit must expose its opaque handle: {out}");
assert!(
    out.contains("knowledge_read") && out.contains("unchanged"),
    "proactive guidance must tell the model to copy the handle unchanged: {out}"
);
```

Add this native description test beside the existing knowledge-tool tests:

```rust
#[test]
fn search_description_requires_knowledge_read_handle() {
    let (tool, _) = tool_with(vec![], vec!["kb1".into()]);
    let description = tool.description();
    assert!(description.contains("knowledge_read") && description.contains("handle"));
    assert!(!description.contains("Read tool using the given path"));
}
```

Extend `registers_read_and_write_tools` in `knowledge_stdio.rs`:

```rust
let search = router
    .list_all()
    .iter()
    .find(|tool| tool.name.as_ref() == "knowledge_search")
    .expect("knowledge_search registered");
let description = search.description.as_deref().unwrap_or_default();
assert!(description.contains("knowledge_read") && description.contains("handle"));
assert!(!description.contains("Read tool"));
```

- [ ] **Step 2: Run tests to verify RED**

Run:

```bash
cargo test -p nomifun-ai-agent prepend_knowledge_context_formats_hits_and_passthrough_when_empty
cargo test -p nomi-agent search_description_requires_knowledge_read_handle
cargo test -p nomifun-app registers_read_and_write_tools
```

Expected: each new assertion/test fails because the handle is omitted or the description still points to generic `Read`.

- [ ] **Step 3: Implement the minimal contract repair**

Change the proactive block to include the exact handle:

```rust
let mut block = String::from(
    "[Relevant knowledge-base context, retrieved automatically for this message \
     — to open a full document, call knowledge_read with the exact opaque handle shown below; \
     copy the handle unchanged and do not rebuild it from the path:]\n",
);
for h in hits {
    block.push_str(&format!(
        "- {}/{} § {}\n  {}\n  handle: {}\n",
        h.kb_name, h.rel_path, h.heading, h.snippet, h.handle,
    ));
}
```

Replace the native and stdio search descriptions with wording equivalent to:

```text
Returns ranked results with an opaque `handle`; read a full result by calling
knowledge_read with that exact handle. Do not reconstruct a handle from its path.
```

- [ ] **Step 4: Run tests to verify GREEN**

Run the three commands from Step 2. Expected: PASS.

- [ ] **Step 5: Commit the knowledge contract repair**

```bash
git add crates/backend/nomifun-ai-agent/src/manager/nomi/agent.rs \
  crates/agent/nomi-agent/src/knowledge_tools.rs \
  crates/backend/nomifun-app/src/commands/knowledge_stdio.rs
git commit -m "fix: provide handles for proactive knowledge reads"
```

### Task 2: Distinguish skipped calls from genuine failures

**Files:**
- Modify: `ui/src/common/chat/normalizeToolCall.ts`
- Modify: `ui/src/common/chat/normalizeToolCall.test.ts`
- Modify: `ui/src/renderer/pages/conversation/Messages/components/toolGroupSummaryModel.ts`
- Modify: `ui/src/renderer/pages/conversation/Messages/components/toolGroupSummaryModel.test.ts`
- Modify: `ui/src/renderer/pages/conversation/Messages/turnProcessState.test.ts`
- Modify: `ui/src/renderer/pages/conversation/Messages/MessageList.tsx`
- Modify: `ui/src/renderer/pages/conversation/Messages/components/ProcessTraceItem.tsx`
- Modify: `ui/src/renderer/services/i18n/locales/en-US/messages.json`
- Modify: `ui/src/renderer/services/i18n/locales/zh-CN/messages.json`
- Regenerate: `ui/src/renderer/services/i18n/i18n-keys.d.ts`

**Interfaces:**
- Consumes: a direct Nomi tool call with status `error` and output beginning with the stable `Skipped because a previous tool call in this assistant turn failed.` marker.
- Produces: `NormalizedToolCall { status: 'canceled', skipped: true }`, skipped receipt metadata, and dedicated localized labels.

- [ ] **Step 1: Write failing normalization and receipt-model tests**

Add a direct-tool normalization test using:

```typescript
const output =
  'Skipped because a previous tool call in this assistant turn failed. Inspect the failed result first.';
const result = normalizeToolCall({
  type: 'tool_call',
  content: {
    call_id: 'call-skipped',
    name: 'Bash',
    status: 'error',
    args: { command: 'find /workspace -maxdepth 2 -type d' },
    output,
  },
} as any);
expect(result?.status).toBe('canceled');
expect(result?.skipped).toBe(true);
```

Add summary/detail assertions:

```typescript
const skipped = tool({
  key: 'bash-skipped',
  name: 'Bash',
  status: 'canceled',
  skipped: true,
  input: '{"command":"find /workspace -maxdepth 2 -type d"}',
});
expect(buildToolReceiptSummaryParts([skipped], 'canceled')).toEqual([
  {
    action: 'run_commands',
    count: 1,
    state: 'canceled',
    target: 'find /workspace -maxdepth 2 -type d',
    skipped: true,
  },
]);
expect(buildToolReceiptDetailRows([skipped])[0].skipped).toBe(true);
```

Add a process-state test proving one real `knowledge_read` error plus one skipped Bash call remains `failed`, while the skipped Bash call alone normalizes as canceled.

- [ ] **Step 2: Run tests to verify RED**

Run:

```bash
bun test ui/src/common/chat/normalizeToolCall.test.ts \
  ui/src/renderer/pages/conversation/Messages/components/toolGroupSummaryModel.test.ts \
  ui/src/renderer/pages/conversation/Messages/turnProcessState.test.ts
```

Expected: FAIL because `skipped` does not exist and the direct call remains `error`.

- [ ] **Step 3: Implement exact skip recognition and propagation**

In `normalizeToolCall.ts` add:

```typescript
const skippedAfterPriorErrorPrefix =
  'Skipped because a previous tool call in this assistant turn failed.';

const isSkippedAfterPriorError = (status: unknown, output: unknown): boolean =>
  status === 'error' && toDisplayText(output).trimStart().startsWith(skippedAfterPriorErrorPrefix);
```

Extend `NormalizedToolCall` with `skipped?: boolean`. In `normalizeToolCall`, compute the marker and return `status: 'canceled'` plus `skipped: true` only for that exact prefix.

Extend `ToolReceiptSummaryPart` and `ToolReceiptDetailRow` with `skipped?: boolean`. Track the number of skipped tools per action group; set summary `skipped: true` only when every tool in that action group was skipped, and copy the marker to detail rows.

- [ ] **Step 4: Render dedicated skipped copy**

Add locale keys:

```json
"toolSummary": {
  "skipped": "Skipped {{target}}"
}
```

and:

```json
"toolSummary": {
  "skipped": "已跳过 {{target}}"
}
```

Before failed/canceled handling in `formatToolReceiptPart` and `formatToolReceiptDetailLabel`, render `messages.toolSummary.skipped` when the model carries `skipped: true`.

Regenerate typed i18n keys:

```bash
bun run gen:i18n
```

- [ ] **Step 5: Run tests to verify GREEN**

Run the Step 2 tests and:

```bash
bun run check:i18n
bun run --filter=./ui typecheck
```

Expected: PASS.

- [ ] **Step 6: Commit skipped-result presentation**

```bash
git add ui/src/common/chat/normalizeToolCall.ts \
  ui/src/common/chat/normalizeToolCall.test.ts \
  ui/src/renderer/pages/conversation/Messages/components/toolGroupSummaryModel.ts \
  ui/src/renderer/pages/conversation/Messages/components/toolGroupSummaryModel.test.ts \
  ui/src/renderer/pages/conversation/Messages/turnProcessState.test.ts \
  ui/src/renderer/pages/conversation/Messages/MessageList.tsx \
  ui/src/renderer/pages/conversation/Messages/components/ProcessTraceItem.tsx \
  ui/src/renderer/services/i18n/locales/en-US/messages.json \
  ui/src/renderer/services/i18n/locales/zh-CN/messages.json \
  ui/src/renderer/services/i18n/i18n-keys.d.ts
git commit -m "fix(ui): label barrier-skipped tool calls"
```

### Task 3: Full regression verification

**Files:**
- Verify only; no planned production changes.

**Interfaces:**
- Consumes: Tasks 1 and 2.
- Produces: fresh proof that the original trigger is removed, the barrier remains active, and UI types/tests are clean.

- [ ] **Step 1: Run focused Rust regression tests**

```bash
cargo test -p nomifun-ai-agent knowledge_prelude_tests
cargo test -p nomi-agent knowledge_tools
cargo test -p nomi-agent test_execute_non_concurrent_tools_stops_after_error
cargo test -p nomi-agent test_protocol_execution_stops_after_sequential_error
cargo test -p nomifun-app knowledge_stdio
```

Expected: all selected tests PASS.

- [ ] **Step 2: Run focused UI regression tests and type checks**

```bash
bun test ui/src/common/chat/normalizeToolCall.test.ts \
  ui/src/renderer/pages/conversation/Messages/components/toolGroupSummaryModel.test.ts \
  ui/src/renderer/pages/conversation/Messages/turnProcessState.test.ts
bun run check:i18n
bun run --filter=./ui typecheck
```

Expected: all tests and checks PASS.

- [ ] **Step 3: Inspect the complete patch**

```bash
git diff origin/main...HEAD --stat
git diff origin/main...HEAD --check
git status --short --branch
```

Expected: only the design, plan, knowledge-contract, tests, UI presentation, and generated i18n key files are changed; no unstaged files remain.
