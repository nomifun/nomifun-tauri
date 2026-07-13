# Session Provider-Error Recovery Design

## Problem

A Nomi conversation can become unusable after a provider/gateway failure during a
Computer Use loop. Switching the conversation to another model does not recover it,
while a newly-created conversation can use the same models normally.

The failure is session-scoped and affects the shared Rust agent engine on Windows,
macOS, and Linux. The operating systems use different screen-capture and input
backends, but they all feed `ToolImage` values into the same message history,
provider adapters, session persistence, and task lifecycle.

## Evidence and Root Causes

### 1. Historical image payloads are count-bounded but byte-unbounded

`AgentEngine::prune_old_tool_images` limits history by image count only. The default
keeps three images and the provider compatibility ceiling allows twenty. A Read or
MCP image may contain up to 5 MiB of decoded data, and native Computer screenshots
have no encoded-size guard. Base64 expands those bytes again before the provider
adapter serializes the request.

Consequently, a normal screenshot/action/screenshot loop can repeatedly send a
multi-megabyte history. Gateways commonly surface body-limit or overloaded-proxy
failures as HTTP 5xx, which Nomi correctly classifies for the UI as
`USER_LLM_PROVIDER_GATEWAY_ERROR`. A fresh conversation succeeds because it has no
historical screenshots.

### 2. A failed model pass does not restore a safe history checkpoint

`run_inner` appends the user message before compaction and the provider call. Both
an initial provider error and a streamed `LlmEvent::Error` return without repairing
the in-memory history. Tool-result checkpoints are persisted during the loop, but a
first-pass failure leaves an uncommitted user message in the reused engine. Retrying
in the same conversation appends another user message to that state.

### 3. Resume sanitization can create a different invalid tool history

The current sanitizer collects matching tool-result IDs globally, drops some whole
assistant messages, and intentionally leaves their tool results behind. Strict
OpenAI-compatible providers reject a `tool` message whose `tool_call_id` has no
preceding assistant tool call. The global lookup also accepts a result that is not
the immediate response to its tool-call group.

### 4. Model switching has no Nomi teardown barrier

Conversation update persists the new model and calls the non-awaiting `kill` path.
`NomiAgentManager::kill_and_wait` is also currently an immediately-ready future.
An old Computer tool can therefore finish and persist stale state while a new agent
for the selected model is already being constructed.

### 5. Initial HTTP 5xx responses bypass the existing retry policy

`ProviderError::is_retryable` marks transient 5xx responses retryable, and empty
mid-stream failures are retried. However, `with_initial_connect_retry` retries only
connection errors, so an initial 500/502/503/504 response is surfaced immediately.

## Chosen Approach

Repair the invariant at every boundary that can introduce or replay poisoned state.
This preserves valid text and completed tool side effects, rather than clearing the
whole conversation.

Two narrower alternatives were rejected:

- Clearing all context on model switch would recover but destroy useful session
  memory and hide the underlying unbounded-payload defect.
- Retrying every gateway error without repairing the payload would resend the same
  oversized or malformed request and multiply cost.

## Design

### A. Enforce both image-count and encoded-byte budgets

Extend the engine's history pruning so it walks newest-to-oldest and retains images
only while both limits permit:

- count: `min(config.tools.max_recent_images, 20)`;
- encoded payload: one maximum Read/MCP image budget (the padded base64 size of
  5 MiB decoded data).

Older images are stripped first. A legacy image that individually exceeds the
single-image budget is also stripped, even when it is the newest; its text tells the
model to capture a fresh observation. Every stripped result keeps its text and
receives one deterministic omission note. Apply the pruning before
compaction/provider calls, after tool results are appended, and during failed-turn
repair. This keeps the newest bounded view while preventing history from multiplying
request-body size.

Native Computer screenshot encoding must also enforce the same single-image decoded
budget. If the first PNG is too large, downscale and re-encode deterministically
until it fits or reaches a conservative minimum edge. This makes the guarantee true
at the producer as well as the consumer.

### B. Restore the last safe checkpoint on provider failure

Within one `run_with_content` call, track the message length at the initial boundary
and after every structurally complete tool-result group. On `ProviderError`,
compaction error, or streamed `LlmEvent::Error`:

1. truncate only messages added after the last safe checkpoint;
2. keep already-saved assistant tool calls and matching tool results so completed
   desktop/file side effects are not forgotten and repeated;
3. strip stale tool-result images under the new byte budget;
4. clear the turn rewind marker and persist the repaired history before returning
   the original error.

For a failure before any tool completes, this removes the just-appended user message.
For a failure after a Computer action, the completed tool pair remains resumable.

### C. Make resume sanitization structurally pair-preserving

Replace global ID matching with an ordered pass:

- retain only tool results belonging to the immediately preceding assistant
  tool-call group;
- remove only unmatched `ToolUse` blocks rather than dropping unrelated assistant
  text or valid calls;
- remove orphan `ToolResult` blocks and empty messages created by repair;
- remove historical tool images under the byte budget;
- drop persisted Thinking blocks when the resumed session's provider protocol differs
  from the selected provider, because reasoning signatures are provider-specific and
  are not durable conversational content.

Return structured repair counts for safe logging without logging prompts, image data,
credentials, or provider response bodies.

### D. Make model/workspace changes await old Nomi termination

Conversation update will use `kill_and_wait` when a model or workspace changes.
Nomi's implementation will signal cancellation and wait, with a bounded timeout, for
the runtime to reach its terminal state. Session repair and image redaction occur
before that state is published, making terminal status the teardown barrier. ACP and
other agent implementations retain their existing awaited teardown semantics.

### E. Apply the declared transient-error retry policy to initial requests

Generalize the initial request retry helper to retry connection failures and
`ProviderError::Api` 500/502/503/504 with the existing small exponential backoff.
Do not retry authentication, permission, billing, invalid request, model-not-found,
context, or rate-limit responses through this path. Mid-stream partial responses
remain non-retryable to avoid duplicated text or tool calls.

## Error and Recovery Semantics

- Provider errors remain visible if bounded retries fail.
- No recovery path repeats a tool action automatically after a partial response or
  completed side effect.
- A user retry in the same session starts from the last structurally valid checkpoint.
- A model switch preserves text and completed tool results but does not replay stale
  screenshots or provider-specific reasoning signatures.
- Cancellation remains a clean `Finish(Cancelled)`, not a provider fault.
- Repair work is bounded by message/image counts and cannot loop indefinitely.

## Cross-Platform Scope

- Windows: UI Automation/input and Windows screen capture feed the shared image and
  history pipeline; the fix applies before provider serialization.
- macOS: Accessibility/Screen Recording and Retina geometry remain unchanged; the
  same producer and engine budgets apply after downscaling.
- Linux: AT-SPI/input support is partial, but available screenshot/tool results use
  the same pipeline and receive identical recovery behavior.
- Headless/server builds without Computer Use still benefit for Browser, Read, and
  MCP image results.

No platform-specific shell commands, path separators, or filesystem assumptions are
introduced.

## Testing

Add focused tests that first reproduce each failure:

1. several individually-valid images exceed the cumulative history budget;
2. a provider failure before the first assistant response is followed by a successful
   retry in the same engine without duplicate user history;
3. a provider failure after a completed tool action retains the valid tool pair but
   strips stale image bytes;
4. partial/misaligned multi-tool histories are repaired without orphan results;
5. a provider-protocol switch drops incompatible Thinking blocks and historical
   images while preserving text;
6. model update awaits task termination before returning;
7. initial 502 succeeds on a bounded retry, while 400 and partial-stream failures are
   not retried;
8. Computer screenshot encoding respects the single-image byte ceiling.

Run the focused crate tests, full affected-crate suites, formatting, Clippy/checks,
and the existing cross-platform reliability tests. Platform-neutral tests exercise
the shared behavior on the current host; Windows and Linux compile/test jobs must run
the same tests in CI without `cfg` exclusions.

## Non-Goals

- Changing Computer Use permissions, coordinate mapping, or input synthesis.
- Silently clearing all conversation text/history.
- Retrying partial streamed output or tool side effects.
- Adding provider-specific gateway size guesses or OS-specific recovery branches.
