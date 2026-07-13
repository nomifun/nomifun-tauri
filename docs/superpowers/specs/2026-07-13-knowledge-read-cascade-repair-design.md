# Knowledge Read Cascade Repair Design

## Problem

A Debian user running NomiFun in Docker saw one failed `knowledge_read` entry
followed by several failed-looking `ls` and `find` entries. The visible sequence
suggested that the container shell, Unicode paths, or command quoting had broken.
The current runtime path shows a different three-part failure:

1. Proactive knowledge retrieval tells the model to call `knowledge_read` when
   it needs a full document, but the injected hits omit the opaque `handle`
   required by that tool.
2. Once `knowledge_read` fails, the same-turn prior-error barrier correctly
   prevents later sequential commands from running.
3. Skipped tool results retain an error status and the conversation UI renders
   every one as `Failed <original command>`, making skipped commands look like
   independent shell failures.

There is also stale tool guidance that says a knowledge-search result should be
opened with the generic `Read` tool and a path, while the actual retrieval
contract requires `knowledge_read(handle)`.

## Goals

- Give the model a valid opaque handle for every proactively retrieved hit.
- Make every native and stdio knowledge-search instruction describe the same
  `knowledge_search` -> `knowledge_read(handle)` protocol.
- Preserve the hard prior-error execution barrier.
- Render calls skipped by that barrier as skipped, not as independently failed.
- Keep the first real tool error visible and actionable.
- Cover the complete trigger, barrier, and presentation lifecycle with
  regression tests.

## Non-goals

- Do not weaken or remove the same-turn failure barrier.
- Do not attempt to repair arbitrary model-generated shell syntax.
- Do not add Debian-, Docker-, locale-, or Unicode-specific command rewriting.
- Do not change the opaque handle encoding or knowledge-base authorization
  boundary.

## Root Cause and Data Flow

### Proactive retrieval

`NomiAgent::run` searches the mounted knowledge bases before the model turn and
passes the top hits to `prepend_knowledge_context`. Each `KnowledgeHit` already
contains `handle`, `kb_name`, `rel_path`, `heading`, and `snippet`. The formatter
currently emits every field except `handle`, even though its preamble directs
the model to `knowledge_read`.

The formatter will emit a dedicated `handle: <opaque value>` line for each hit
and state explicitly that the value must be copied unchanged. Paths remain
visible as human-readable context, but are not presented as a substitute for
the handle.

### Knowledge-tool contract

The following descriptions must agree:

- native `KnowledgeSearchTool::description`;
- native search-result footer;
- Nomi knowledge prelude;
- proactive retrieval preamble;
- stdio MCP `knowledge_search` description;
- stdio MCP search-result footer.

Their contract is:

1. Use `knowledge_search` to obtain ranked hits.
2. Use `knowledge_read` with the exact opaque `handle` from a hit to read the
   full document.
3. Never construct a handle from a displayed path.

### Prior-error barrier

The execution barrier in `nomi-agent` remains authoritative. If a tool result
is a real error, later sequential calls from that assistant turn remain
unexecuted and receive the stable `SKIPPED_AFTER_PRIOR_ERROR` result. This
prevents commands that may depend on missing knowledge or failed setup from
running blindly.

### Presentation

The frontend normalizer will recognize the stable prior-error skip result and
carry an explicit `skipped` marker on the normalized tool model. Receipt
aggregation will treat skipped calls as a canceled execution state for ordering
and styling, while using dedicated `Skipped {{target}}` / `已跳过 {{target}}`
copy instead of the generic canceled or failed copy.

The underlying tool output remains available in expanded details, including
the reason it was skipped. The original failing `knowledge_read` remains in the
failed state, so the turn still exposes the real checkpoint rather than looking
successful.

## Components and File Boundaries

### Knowledge context and descriptions

- `crates/backend/nomifun-ai-agent/src/manager/nomi/agent.rs`
  - Include `KnowledgeHit.handle` in proactive context.
  - Strengthen the formatter test so omission of the handle fails.
- `crates/agent/nomi-agent/src/knowledge_tools.rs`
  - Replace stale generic-`Read` guidance with the handle-based protocol.
  - Add description-level regression coverage.
- `crates/backend/nomifun-app/src/commands/knowledge_stdio.rs`
  - Align the stdio MCP search description with `knowledge_read(handle)`.
  - Extend its tool-description test.

### Skipped-result presentation

- `ui/src/common/chat/normalizeToolCall.ts`
  - Detect the stable prior-error skip result.
  - Expose a `skipped` boolean without converting genuine failures.
- `ui/src/common/chat/normalizeToolCall.test.ts`
  - Prove skipped direct tool calls are distinguished from failures.
- `ui/src/renderer/pages/conversation/Messages/components/toolGroupSummaryModel.ts`
  - Propagate the skipped marker to summary parts and detail rows.
  - Map skipped calls to canceled execution ordering while retaining dedicated
    skipped copy.
- `ui/src/renderer/pages/conversation/Messages/components/toolGroupSummaryModel.test.ts`
  - Cover a real failure followed by multiple skipped Bash calls.
- `ui/src/renderer/pages/conversation/Messages/MessageList.tsx`
- `ui/src/renderer/pages/conversation/Messages/components/ProcessTraceItem.tsx`
  - Render skipped summary and detail labels.
- `ui/src/renderer/services/i18n/locales/en-US/messages.json`
- `ui/src/renderer/services/i18n/locales/zh-CN/messages.json`
  - Add exact skipped labels.

The implementation should avoid changing the shared backend tool status enum:
the stable skip marker is already present in the persisted output, and the
presentation distinction does not need a protocol migration.

## Error Handling

- Missing or malformed handles remain real `knowledge_read` errors.
- Handles outside the current session's mounted base IDs remain forbidden.
- A knowledge search failure or empty result remains best-effort for proactive
  retrieval and leaves the user message unchanged.
- A real tool error continues to halt later sequential calls in the same turn.
- Only the exact stable prior-error skip result receives skipped presentation;
  other error output must not be reclassified.

## Testing

### Red tests

1. Assert proactive context contains the exact opaque handle and the instruction
   to pass it unchanged to `knowledge_read`.
2. Assert native and stdio knowledge-search descriptions mention
   `knowledge_read` and `handle`, and no longer direct the model to generic
   `Read` with a path.
3. Assert a direct tool call whose output is the stable prior-error message is
   normalized with `skipped: true`, while a genuine command error is not.
4. Assert a receipt containing one failed `knowledge_read` and skipped Bash
   calls keeps the knowledge read failed and labels the Bash calls as skipped.

### Green verification

- Run the targeted Rust tests for `nomi-agent`, `nomifun-ai-agent`, and the
  knowledge stdio command.
- Run the targeted Bun tests for normalization and receipt summaries.
- Run UI type checking.
- Run the existing orchestration regression proving skipped Bash calls are not
  executed after a prior error.
- Run `git diff --check`.

## Acceptance Criteria

- Proactive hits always include the same opaque handle produced by the search
  service.
- A model following any exposed knowledge-search instruction has enough data to
  call `knowledge_read` successfully without reconstructing a path.
- The hard failure barrier still prevents later sequential tools from running.
- The UI shows one genuine `knowledge_read` failure and later calls as
  `已跳过`, rather than showing every command as `运行失败`.
- Genuine Bash, knowledge, permission, and infrastructure failures retain their
  existing failed presentation.
- No Docker- or Debian-specific behavior is introduced.
