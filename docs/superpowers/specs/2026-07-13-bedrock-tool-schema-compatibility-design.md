# Bedrock Tool Schema Compatibility Design

## Problem

User-configured Claude models can be exposed through an OpenAI-compatible or
Anthropic-compatible gateway while the gateway ultimately dispatches the request
to AWS Bedrock. Bedrock rejects tool `input_schema` values that contain
top-level `oneOf`, `allOf`, or `anyOf` keywords. The rejection happens before
model inference and is returned by some gateways as a nested HTTP 500 error with
reason `TOOL_SCHEMA_INVALID`.

Two code paths expose the defect:

1. Normal agent turns send every registered tool schema. The recently expanded
   `Read` and `exec_command` schemas contain a top-level `oneOf`.
2. Provider health probes intend to run without tools, but an empty
   `builtin_allowlist` means "unrestricted" rather than "no tools". The probe
   therefore sends the same incompatible schemas.

The existing Bedrock sanitizer is insufficient. It enforces an object root,
removes `additionalProperties`, and normalizes nullable type arrays, but it does
not remove the top-level composition keywords named in the Bedrock error.

## Goals

- Make provider health probes genuinely tool-free.
- Make direct Bedrock requests compatible with Bedrock's top-level tool-schema
  restriction.
- Support user-entered OpenAI-compatible and Anthropic-compatible gateways that
  secretly route to Bedrock without relying on provider names, model aliases, or
  URL heuristics.
- Preserve full JSON Schema for providers that already accept it.
- Never retry unrelated provider failures as schema-compatibility failures.

## Non-goals

- Rewriting individual built-in tool schemas to remove useful constraints for
  every provider.
- Guessing an upstream runtime from a model name such as `claude-opus-4-8`.
- Treating arbitrary HTTP 500 responses as evidence of Bedrock compatibility.
- Changing tool execution validation or argument shape.

## Considered Approaches

### 1. Adaptive schema fallback (selected)

Send the full schema first. When a request with tools fails with an explicit
tool-schema incompatibility signal, retry once with sanitized schemas and cache
that requirement in the current provider instance.

This keeps official OpenAI and Anthropic behavior unchanged, handles opaque
gateways, and limits the extra round trip to the first incompatible request in a
provider instance.

### 2. Sanitize every OpenAI and Anthropic request

This avoids the first failed round trip and is simpler, but unnecessarily
weakens schemas for providers that support top-level composition. It would make
model tool selection less precise everywhere to accommodate a subset of
gateways.

### 3. Detect Bedrock by platform, URL, or model name

This has no retry cost but cannot reliably identify gateways that hide their
upstream runtime. Provider display names and model aliases are user-controlled,
and new gateways would repeatedly escape the heuristic.

## Design

### Schema sanitization

Extend `nomi_config::compat::sanitize_json_schema` so the returned root is an
object and contains none of `oneOf`, `allOf`, or `anyOf`. Only the root-level
keywords are removed. Nested composition remains available because the reported
Bedrock restriction is specifically at the top level.

For the current affected schemas, `type`, `properties`, descriptions, and other
field constraints remain at the root, so removing the composition keywords does
not wrap or rename arguments. Existing recursive cleanup for
`additionalProperties` and nullable type arrays remains in place.

### Precise error classification

Add a provider-error predicate that recognizes explicit schema incompatibility
evidence in an API error body. It accepts the stable reason
`TOOL_SCHEMA_INVALID` and the specific `input_schema` plus top-level composition
restriction wording. It does not match by HTTP status alone.

Rate limits, connection errors, authentication failures, generic 500 responses,
and other validation errors do not activate the fallback.

### OpenAI-compatible and Anthropic-compatible fallback

Each provider instance maintains a thread-safe boolean indicating that the
endpoint requires sanitized tool schemas.

For an initial model request:

1. Build the request with full schemas unless the provider instance already
   learned that sanitization is required.
2. Send through the existing connection-retry path.
3. If the request has tools and fails with the precise schema error, mark the
   provider instance as requiring sanitization.
4. Rebuild the body with sanitized schemas and retry exactly once.
5. Use the successful body for any existing empty-stream retry, so subsequent
   retries cannot reintroduce the rejected schema.

If the sanitized retry fails, surface that failure normally. There is no loop
between full and sanitized modes. Later turns on the same provider instance start
in sanitized mode and avoid the known bad request.

Direct Bedrock keeps its proactive sanitizer because its restriction is known in
advance; fixing the shared sanitizer is sufficient for that path.

### Health probe isolation

Add an explicit registry operation for removing all tools. After the probe
engine is bootstrapped, clear its registry before calling the provider. This is
done after bootstrap because bootstrap always registers native tools and the
deferred `ToolSearch` tool.

The health request therefore omits the `tools` field entirely. It remains a
single-turn text reachability probe and does not depend on schema fallback.

## Error Handling and Observability

- Emit a warning when the adaptive fallback activates, including provider
  protocol and HTTP status but not API keys or request bodies.
- Preserve the original error if classification does not match.
- Surface the sanitized retry's error if the retry also fails, because it is the
  most current actionable provider response.
- Do not log full tool arguments or credentials.

## Testing

Follow a red-green cycle with these regression cases:

1. The sanitizer removes root `oneOf`, `allOf`, and `anyOf` while preserving
   root properties, required fields, and nested composition.
2. A direct Bedrock request body built from an affected tool schema contains no
   forbidden root composition keyword.
3. An OpenAI-compatible mock gateway rejects the full schema with
   `TOOL_SCHEMA_INVALID`, accepts the sanitized schema, and returns a successful
   stream.
4. The equivalent Anthropic-compatible mock gateway behaves the same way.
5. The learned provider capability causes a later turn to send the sanitized
   schema immediately.
6. An unrelated HTTP 500 is sent once and remains an error.
7. A built provider health-probe engine has zero registered tools, and its model
   request therefore has no tools.

Run focused crate tests first, followed by formatting, the related workspace
tests, and a compile/check of the affected crates.

## Success Criteria

- The reported Bedrock error shape automatically recovers for both OpenAI and
  Anthropic compatible user-entered gateways.
- Direct Bedrock no longer sends the prohibited top-level schema keywords.
- Health probes do not send any tool definitions.
- Official providers retain their full schema unless they explicitly reject it.
- Non-schema provider failures retain existing behavior.
