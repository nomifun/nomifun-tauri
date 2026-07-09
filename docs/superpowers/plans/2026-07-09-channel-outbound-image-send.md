# Channel Outbound Image/File Send Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a channel (Telegram first, WeChat later) deliver AI-generated images/files to the remote IM user, instead of only forwarding the assistant's text.

**Architecture:** The image an agent produces is a *workshop asset* (`wsa_…` id), surfaced today only to the desktop (Markdown link → localhost fetch). This plan taps the same turn's signals — the completed `nomi_workshop_*` tool-call `output` JSON (`result_asset_ids`) plus any `/api/workshop/files/{id}` URL in the assistant text — accumulates the asset ids in the stream relay, resolves them to raw bytes via a new injectable `AssetResolver` trait (implemented in `nomifun-app` over `WorkshopService::read_asset_bytes`), and hands each to a new `ChannelPlugin::send_media` method that uploads the bytes to the platform. `send_media` has a graceful no-op default so the other 11 plugins compile and text-only channels degrade cleanly; Telegram overrides it with a multipart `sendPhoto`/`sendDocument`. No change to `UnifiedOutgoingMessage` (avoids serde/literal churn) and no new AgentStreamEvent variant.

**Tech Stack:** Rust, `async-trait`, `tokio`, `reqwest` (already has the `multipart` feature at root `Cargo.toml:111`), `serde_json`, `regex` (already a `nomifun-channel` dependency). Tests via `cargo nextest`.

## Global Constraints

- **Touched crates only for tests:** run `cargo nextest run -p nomifun-channel` (and `-p nomifun-app` for wiring); do NOT run the full workspace test suite until final review. (Ref: memory `test-workflow-rules`.)
- **`nomifun-channel` MUST NOT depend on `nomifun-workshop`** — that is why asset resolution is an injected trait (`AssetResolver`) implemented in `nomifun-app`, mirroring the existing `MasterAgentProfile` pattern (`message_service.rs:29`).
- **Struct-literal gotcha:** `UnifiedOutgoingMessage` is constructed as a full literal in ~10 places; this plan deliberately does NOT add a field to it. Do not add one. Carry bytes in the new `OutgoingMedia` type passed through the new `send_media` path instead. (Ref: memory `dag-node-preconfig-delivered` — "全枚举字面量 struct 加字段优先专用方法".)
- **`cargo check` does not compile tests:** always verify test code with `cargo nextest run`, never rely on `cargo check` alone. (Ref: memory `knowledge-bugfix-sweep`.)
- **Do not `cargo fmt`** the whole repo; format only touched files if needed. (Ref: memory `external-capability-exposure-p0-delivered`.)
- **No new secrets/logging of bytes:** never log raw image bytes; log sizes/mime only.
- Brand/user-facing copy: Chinese for user-visible notices (this feature adds none beyond existing).

---

## File Structure

**Phase A — shared pipeline (crate `nomifun-channel`):**
- Modify `crates/backend/nomifun-channel/src/types.rs` — add `OutgoingMedia` + `MediaKind`.
- Create `crates/backend/nomifun-channel/src/media_refs.rs` — pure asset-id extraction helpers.
- Modify `crates/backend/nomifun-channel/src/lib.rs` — register `pub mod media_refs;` and re-export `AssetResolver`.
- Modify `crates/backend/nomifun-channel/src/message_service.rs` — `StreamAction::MediaProduced`, `AssetResolver` trait, `process_stream_event` change, `with_asset_resolver`/`asset_resolver()`.
- Modify `crates/backend/nomifun-channel/src/plugin.rs` — `ChannelPlugin::send_media` default method.
- Modify `crates/backend/nomifun-channel/src/stream_relay.rs` — `ChannelSender::send_media`, relay media accumulation + flush, `MessageRecorder` impl update, constructor signature.
- Modify `crates/backend/nomifun-channel/src/manager.rs` — `ChannelManager::send_media` inherent + trait impl.
- Modify `crates/backend/nomifun-channel/src/orchestrator.rs` — pass resolver into `ChannelStreamRelay::new`.

**Phase B — Telegram pilot:**
- Modify `crates/backend/nomifun-channel/src/plugins/telegram/api.rs` — `send_photo`/`send_document` multipart.
- Modify `crates/backend/nomifun-channel/src/plugins/telegram/plugin.rs` — `send_media` override.
- Create `crates/backend/nomifun-app/src/channel_asset_resolver.rs` — `AssetResolver` impl over `WorkshopService`.
- Modify `crates/backend/nomifun-app/src/lib.rs` (or module root) — register the new module.
- Modify `crates/backend/nomifun-app/src/router/state.rs:749-761` — `.with_asset_resolver(...)`.

**Phase C — WeChat (discovery-gated):**
- Modify `crates/backend/nomifun-channel/src/plugins/weixin/{api.rs,types.rs,plugin.rs}` — media upload + `send_media` (contract filled from the C1 spike).

---

## Interfaces (shared contract used across tasks)

```rust
// types.rs — runtime-only (NOT serde): carries resolved bytes in-process.
pub struct OutgoingMedia {
    pub bytes: Vec<u8>,
    pub mime: String,
    pub filename: String,
    pub kind: MediaKind,
}
pub enum MediaKind { Image, File }   // Copy, Eq

// media_refs.rs
pub fn asset_ids_from_tool_output(output: &str) -> Vec<String>;
pub fn asset_ids_from_text(text: &str) -> Vec<String>;

// message_service.rs
#[async_trait::async_trait]
pub trait AssetResolver: Send + Sync {
    async fn resolve(&self, asset_id: &str) -> Option<crate::types::OutgoingMedia>;
}
pub enum StreamAction { /* …existing… */ MediaProduced(Vec<String>) }
impl ChannelMessageService {
    pub fn with_asset_resolver(self, r: Arc<dyn AssetResolver>) -> Self;
    pub fn asset_resolver(&self) -> Option<Arc<dyn AssetResolver>>;
}

// plugin.rs
async fn ChannelPlugin::send_media(&self, chat_id: &str, media: OutgoingMedia, caption: Option<&str>) -> Result<String, ChannelError>; // default no-op

// stream_relay.rs
async fn ChannelSender::send_media(&self, plugin_id: &str, chat_id: &str, media: OutgoingMedia, caption: Option<&str>) -> Result<String, ChannelError>;
ChannelStreamRelay::new(config, sender, pending, asset_resolver: Option<Arc<dyn AssetResolver>>)
```

---

## Phase A — Shared outbound-media pipeline

### Task A1: `OutgoingMedia` type + asset-id extraction helpers

**Files:**
- Modify: `crates/backend/nomifun-channel/src/types.rs` (add types near `ChannelMediaAction`, ~line 486)
- Create: `crates/backend/nomifun-channel/src/media_refs.rs`
- Modify: `crates/backend/nomifun-channel/src/lib.rs` (add `pub mod media_refs;`)

**Interfaces:**
- Produces: `OutgoingMedia`, `MediaKind`, `media_refs::asset_ids_from_tool_output(&str) -> Vec<String>`, `media_refs::asset_ids_from_text(&str) -> Vec<String>`.

- [ ] **Step 1: Add the runtime media types to `types.rs`**

Append after the `ChannelMediaAction` struct (around line 492):

```rust
/// Media resolved from a workshop asset, ready for a plugin to upload.
///
/// Runtime-only (NOT `Serialize`/`Deserialize`): it carries raw bytes and is
/// passed in-process from the relay to a plugin's [`ChannelPlugin::send_media`],
/// never over the wire. Kept out of `UnifiedOutgoingMessage` so that type stays
/// a serializable, literal-constructed wire struct.
#[derive(Debug, Clone, PartialEq)]
pub struct OutgoingMedia {
    pub bytes: Vec<u8>,
    pub mime: String,
    pub filename: String,
    pub kind: MediaKind,
}

/// Whether an [`OutgoingMedia`] should be sent as an inline image or a document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    File,
}
```

- [ ] **Step 2: Write the failing test for `media_refs`**

Create `crates/backend/nomifun-channel/src/media_refs.rs`:

```rust
//! Pure helpers that extract workshop asset ids (`wsa_…`) from the two
//! machine-readable signals a channel turn carries: a completed
//! `nomi_workshop_*` tool call's `output` JSON (`result_asset_ids`), and any
//! `/api/workshop/files/{id}` URL the assistant wrote into its visible text
//! (the same link the desktop renders). Both are deduped by the caller.

use regex::Regex;
use std::sync::OnceLock;

/// Matches a workshop capability URL and captures the asset id, e.g.
/// `/api/workshop/files/wsa_01H…` (host optional, `?thumb=1` tolerated).
fn files_url_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"/api/workshop/files/(wsa_[A-Za-z0-9]+)").unwrap())
}

/// Extract asset ids from a completed tool call's `output` string by parsing it
/// as JSON and collecting every `result_asset_ids` string array found anywhere
/// in the tree. Non-JSON or absent key → empty.
pub fn asset_ids_from_tool_output(output: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(output) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    collect_result_asset_ids(&value, &mut out);
    out
}

fn collect_result_asset_ids(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                if k == "result_asset_ids"
                    && let Some(arr) = v.as_array()
                {
                    for item in arr {
                        if let Some(s) = item.as_str() {
                            out.push(s.to_owned());
                        }
                    }
                }
                collect_result_asset_ids(v, out);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                collect_result_asset_ids(v, out);
            }
        }
        _ => {}
    }
}

/// Extract asset ids from assistant text by matching workshop capability URLs.
pub fn asset_ids_from_text(text: &str) -> Vec<String> {
    files_url_re()
        .captures_iter(text)
        .map(|c| c[1].to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_output_top_level_ids() {
        let out = r#"{"status":"succeeded","result_asset_ids":["wsa_1","wsa_2"]}"#;
        assert_eq!(asset_ids_from_tool_output(out), vec!["wsa_1", "wsa_2"]);
    }

    #[test]
    fn tool_output_nested_ids() {
        let out = r#"{"data":{"task":{"result_asset_ids":["wsa_9"]}}}"#;
        assert_eq!(asset_ids_from_tool_output(out), vec!["wsa_9"]);
    }

    #[test]
    fn tool_output_non_json_or_missing_is_empty() {
        assert!(asset_ids_from_tool_output("not json").is_empty());
        assert!(asset_ids_from_tool_output(r#"{"status":"running"}"#).is_empty());
    }

    #[test]
    fn text_extracts_capability_urls() {
        let text = "图来咯～ ![cat](/api/workshop/files/wsa_abc123) and http://127.0.0.1:8080/api/workshop/files/wsa_def456?thumb=1";
        assert_eq!(asset_ids_from_text(text), vec!["wsa_abc123", "wsa_def456"]);
    }

    #[test]
    fn text_without_urls_is_empty() {
        assert!(asset_ids_from_text("just text, no image").is_empty());
    }
}
```

- [ ] **Step 3: Register the module in `lib.rs`**

Add alongside the other `pub mod` declarations in `crates/backend/nomifun-channel/src/lib.rs`:

```rust
pub mod media_refs;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo nextest run -p nomifun-channel media_refs`
Expected: PASS (5 tests). `OutgoingMedia`/`MediaKind` compile (unused-warning is fine — they are consumed in A3).

- [ ] **Step 5: Commit**

```bash
git add crates/backend/nomifun-channel/src/types.rs crates/backend/nomifun-channel/src/media_refs.rs crates/backend/nomifun-channel/src/lib.rs
git commit -m "feat(channel): add OutgoingMedia type + workshop asset-id extraction helpers"
```

---

### Task A2: `AssetResolver` trait + `StreamAction::MediaProduced` + tool-output surfacing

**Files:**
- Modify: `crates/backend/nomifun-channel/src/message_service.rs`
- Modify: `crates/backend/nomifun-channel/src/lib.rs` (re-export `AssetResolver` if convenient)

**Interfaces:**
- Consumes: `media_refs::asset_ids_from_tool_output` (A1), `OutgoingMedia` (A1).
- Produces: `AssetResolver` trait, `StreamAction::MediaProduced(Vec<String>)`, `ChannelMessageService::with_asset_resolver` + `asset_resolver()`.

- [ ] **Step 1: Add the `MediaProduced` variant and a failing `process_stream_event` test**

In `message_service.rs`, add to the `StreamAction` enum (after `Decision {…}`, ~line 822):

```rust
    /// One or more workshop asset ids produced by a *completed* tool call
    /// (e.g. `nomi_workshop_get_task` `result_asset_ids`). The relay resolves
    /// each to bytes and sends it as media after the final text.
    MediaProduced(Vec<String>),
```

Add this test in the `tests` module (near `tool_call_event_produces_tool_call`, ~line 1247):

```rust
    #[test]
    fn completed_workshop_tool_call_produces_media() {
        let event = AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "c1".into(),
            name: "nomi_workshop_get_task".into(),
            args: serde_json::Value::Null,
            status: ToolCallStatus::Completed,
            description: None,
            input: None,
            output: Some(r#"{"status":"succeeded","result_asset_ids":["wsa_1"]}"#.into()),
        });
        match ChannelMessageService::process_stream_event(&event) {
            Some(StreamAction::MediaProduced(ids)) => assert_eq!(ids, vec!["wsa_1"]),
            other => panic!("expected MediaProduced, got {other:?}"),
        }
    }

    #[test]
    fn running_tool_call_still_produces_tool_call_status() {
        let event = AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "c1".into(),
            name: "nomi_workshop_generate".into(),
            args: serde_json::Value::Null,
            status: ToolCallStatus::Running,
            description: None,
            input: None,
            output: None,
        });
        assert!(matches!(
            ChannelMessageService::process_stream_event(&event),
            Some(StreamAction::ToolCall { .. })
        ));
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo nextest run -p nomifun-channel completed_workshop_tool_call_produces_media`
Expected: FAIL (variant not yet produced — currently maps to `ToolCall`).

- [ ] **Step 3: Change `process_stream_event` to surface completed tool-output asset ids**

Replace the `AgentStreamEvent::ToolCall(data)` arm (message_service.rs:608-611) with:

```rust
            AgentStreamEvent::ToolCall(data) => {
                // A completed tool call may carry produced workshop asset ids in
                // its output JSON (nomi_workshop_get_task/generate `result_asset_ids`).
                // Surface those as MediaProduced so the relay can send the picture;
                // otherwise keep the cosmetic {name,status} progress update.
                if matches!(
                    data.status,
                    nomifun_ai_agent::protocol::events::ToolCallStatus::Completed
                ) && let Some(output) = data.output.as_deref()
                {
                    let ids = crate::media_refs::asset_ids_from_tool_output(output);
                    if !ids.is_empty() {
                        return Some(StreamAction::MediaProduced(ids));
                    }
                }
                Some(StreamAction::ToolCall {
                    name: data.name.clone(),
                    status: format!("{:?}", data.status),
                })
            }
```

- [ ] **Step 4: Add the `AssetResolver` trait and the field + accessors**

Near the `MasterAgentProfile` trait (top of `message_service.rs`, after the imports), add:

```rust
/// Resolves a workshop asset id (`wsa_…`) to raw bytes for outbound media.
///
/// Defined here (not in `nomifun-workshop`) so `nomifun-channel` stays free of
/// a workshop dependency; the concrete impl lives in `nomifun-app`
/// (`channel_asset_resolver.rs`), mirroring [`MasterAgentProfile`].
#[async_trait::async_trait]
pub trait AssetResolver: Send + Sync {
    /// Load `asset_id` as bytes + mime + a suggested filename. `None` when the
    /// asset can't be found or read (the relay then simply skips that image).
    async fn resolve(&self, asset_id: &str) -> Option<crate::types::OutgoingMedia>;
}
```

Add a field to `ChannelMessageService` (after `pending_decisions`, ~line 115):

```rust
    /// Optional resolver turning `wsa_…` ids into raw bytes for outbound media.
    /// `None` (default / tests) disables channel image sending gracefully.
    asset_resolver: Option<Arc<dyn AssetResolver>>,
```

Initialize it to `None` in `ChannelMessageService::new` (add `asset_resolver: None,` to the struct literal, ~line 133). Add the builder + accessor next to `with_master_profile` (~line 139):

```rust
    /// Wire the asset resolver so channel replies can send AI-generated images.
    /// Without it, image sending is disabled (text-only behaviour, unchanged).
    pub fn with_asset_resolver(mut self, resolver: Arc<dyn AssetResolver>) -> Self {
        self.asset_resolver = Some(resolver);
        self
    }

    /// The wired asset resolver, if any. The orchestrator hands this to each
    /// `ChannelStreamRelay` it spawns.
    pub fn asset_resolver(&self) -> Option<Arc<dyn AssetResolver>> {
        self.asset_resolver.clone()
    }
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo nextest run -p nomifun-channel process_stream_event completed_workshop running_tool_call`
Expected: PASS. Then `cargo check -p nomifun-channel` — clean.

- [ ] **Step 6: Commit**

```bash
git add crates/backend/nomifun-channel/src/message_service.rs crates/backend/nomifun-channel/src/lib.rs
git commit -m "feat(channel): surface workshop asset ids as StreamAction::MediaProduced + AssetResolver trait"
```

---

### Task A3: `send_media` on `ChannelPlugin` + `ChannelSender` + manager routing

**Files:**
- Modify: `crates/backend/nomifun-channel/src/plugin.rs` (trait default method)
- Modify: `crates/backend/nomifun-channel/src/stream_relay.rs` (`ChannelSender` trait + `MessageRecorder` impl)
- Modify: `crates/backend/nomifun-channel/src/manager.rs` (inherent + trait impl)

**Interfaces:**
- Consumes: `OutgoingMedia` (A1).
- Produces: `ChannelPlugin::send_media` (default no-op), `ChannelSender::send_media`, `ChannelManager::send_media`.

- [ ] **Step 1: Add the default `send_media` to the `ChannelPlugin` trait**

In `plugin.rs`, add inside `trait ChannelPlugin` after `edit_message` (~line 127), a *provided* (default) method:

```rust
    /// Send a media item (image/file) to a chat, returning the platform message
    /// id. Default: not supported — log at debug and no-op. The assistant's text
    /// is delivered separately by the relay, so text-only / not-yet-wired
    /// channels keep working; platforms with media support override this.
    async fn send_media(
        &self,
        _chat_id: &str,
        _media: crate::types::OutgoingMedia,
        _caption: Option<&str>,
    ) -> Result<String, ChannelError> {
        tracing::debug!(
            plugin = %self.plugin_type(),
            "send_media not supported for this platform; skipping media"
        );
        Ok(String::new())
    }
```

(No change needed to the in-file `MockPlugin` — it inherits the default.)

- [ ] **Step 2: Add `send_media` to the `ChannelSender` trait and the `MessageRecorder` test impl**

In `stream_relay.rs`, add to `trait ChannelSender` (after `edit_message`, ~line 47):

```rust
    async fn send_media(
        &self,
        plugin_id: &str,
        chat_id: &str,
        media: crate::types::OutgoingMedia,
        caption: Option<&str>,
    ) -> Result<String, ChannelError>;
```

Extend `MessageRecorder` (the test sender) to record media. Add a field to the struct (~line 405) and initializer (~line 411):

```rust
    media: std::sync::Mutex<Vec<crate::types::OutgoingMedia>>,
```
```rust
            media: std::sync::Mutex::new(Vec::new()),
```

Add a `take_media` accessor next to `take_edits` (~line 422):

```rust
    pub fn take_media(&self) -> Vec<crate::types::OutgoingMedia> {
        std::mem::take(&mut self.media.lock().unwrap())
    }
```

Implement the new trait method in `impl ChannelSender for MessageRecorder` (~line 434):

```rust
    async fn send_media(
        &self,
        _plugin_id: &str,
        _chat_id: &str,
        media: crate::types::OutgoingMedia,
        _caption: Option<&str>,
    ) -> Result<String, ChannelError> {
        self.media.lock().unwrap().push(media);
        Ok("media-1".into())
    }
```

- [ ] **Step 3: Add `send_media` to `ChannelManager` (inherent + trait routing)**

In `manager.rs`, add an inherent method after `edit_message` (~line 768):

```rust
    /// Sends a media item (image/file) through a specific plugin.
    pub async fn send_media(
        &self,
        plugin_id: &str,
        chat_id: &str,
        media: crate::types::OutgoingMedia,
        caption: Option<&str>,
    ) -> Result<String, ChannelError> {
        let plugin = self
            .plugins
            .get(plugin_id)
            .ok_or_else(|| ChannelError::PluginNotFound(plugin_id.to_owned()))?;
        plugin.send_media(chat_id, media, caption).await
    }
```

Add the trait method to `impl crate::stream_relay::ChannelSender for ChannelManager` (~line 1191, before the closing `}`):

```rust
    async fn send_media(
        &self,
        plugin_id: &str,
        chat_id: &str,
        media: crate::types::OutgoingMedia,
        caption: Option<&str>,
    ) -> Result<String, crate::error::ChannelError> {
        self.send_media(plugin_id, chat_id, media, caption).await
    }
```

- [ ] **Step 4: Verify it compiles and existing tests pass**

Run: `cargo check -p nomifun-channel`
Expected: clean (all plugins get the default `send_media`; only `MessageRecorder` and `ChannelManager` implement the `ChannelSender` method).

Run: `cargo nextest run -p nomifun-channel`
Expected: PASS (no regressions).

- [ ] **Step 5: Commit**

```bash
git add crates/backend/nomifun-channel/src/plugin.rs crates/backend/nomifun-channel/src/stream_relay.rs crates/backend/nomifun-channel/src/manager.rs
git commit -m "feat(channel): add send_media path (ChannelPlugin default no-op + ChannelSender + manager routing)"
```

---

### Task A4: Relay accumulates + flushes media

**Files:**
- Modify: `crates/backend/nomifun-channel/src/stream_relay.rs`
- Modify: `crates/backend/nomifun-channel/src/orchestrator.rs` (constructor call site, ~line 334)

**Interfaces:**
- Consumes: `StreamAction::MediaProduced` (A2), `AssetResolver` (A2), `ChannelSender::send_media` (A3), `media_refs::asset_ids_from_text` (A1).
- Produces: `ChannelStreamRelay::new(config, sender, pending, asset_resolver)` (4-arg).

- [ ] **Step 1: Write a failing relay test (media flushed on finish)**

Add to `stream_relay.rs` a `#[cfg(test)] mod media_tests` at the end of the file:

```rust
#[cfg(test)]
mod media_tests {
    use super::*;
    use crate::types::{MediaKind, OutgoingMedia};
    use async_trait::async_trait;
    use std::sync::Arc;

    struct StubResolver;
    #[async_trait]
    impl crate::message_service::AssetResolver for StubResolver {
        async fn resolve(&self, asset_id: &str) -> Option<OutgoingMedia> {
            Some(OutgoingMedia {
                bytes: vec![1, 2, 3],
                mime: "image/png".into(),
                filename: format!("{asset_id}.png"),
                kind: MediaKind::Image,
            })
        }
    }

    fn cfg(platform: PluginType) -> RelayConfig {
        RelayConfig {
            platform,
            plugin_id: "p1".into(),
            chat_id: "c1".into(),
            throttle_ms: 0,
            conversation_id: "conv1".into(),
        }
    }

    // Drives the relay with one MediaProduced event then Finish, and asserts the
    // resolved image is sent exactly once via send_media (deduped).
    #[tokio::test]
    async fn relay_flushes_media_on_finish() {
        use nomifun_ai_agent::protocol::events::{FinishEventData, TextEventData, ToolCallEventData, ToolCallStatus};

        let recorder = Arc::new(MessageRecorder::new());
        let pending = crate::pending_decision::PendingDecisionStore::new();
        let relay = ChannelStreamRelay::new(
            cfg(PluginType::Telegram),
            recorder.clone(),
            pending,
            Some(Arc::new(StubResolver)),
        );

        let (tx, rx) = tokio::sync::broadcast::channel(16);
        tx.send(AgentStreamEvent::Text(TextEventData { content: "图来咯～".into() })).unwrap();
        // Two completed tool calls returning the SAME asset id → must dedupe to one send.
        for _ in 0..2 {
            tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
                call_id: "t".into(),
                name: "nomi_workshop_get_task".into(),
                args: serde_json::Value::Null,
                status: ToolCallStatus::Completed,
                description: None,
                input: None,
                output: Some(r#"{"result_asset_ids":["wsa_x"]}"#.into()),
            })).unwrap();
        }
        tx.send(AgentStreamEvent::Finish(FinishEventData { session_id: None, stop_reason: None })).unwrap();
        drop(tx);

        relay.run(rx).await;

        let media = recorder.take_media();
        assert_eq!(media.len(), 1, "one deduped image sent");
        assert_eq!(media[0].filename, "wsa_x.png");
        // Text still delivered.
        assert!(!recorder.take_sends().is_empty(), "final text sent too");
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo nextest run -p nomifun-channel relay_flushes_media_on_finish`
Expected: FAIL to COMPILE (`ChannelStreamRelay::new` still takes 3 args; no media handling).

- [ ] **Step 3: Add the resolver field + 4-arg constructor**

In `stream_relay.rs`, add a field to `ChannelStreamRelay` (~line 61):

```rust
    /// Resolves `wsa_…` ids to bytes for outbound media. `None` disables sending.
    asset_resolver: Option<Arc<dyn crate::message_service::AssetResolver>>,
```

Replace `ChannelStreamRelay::new` (~line 65) to take the resolver:

```rust
    pub fn new(
        config: RelayConfig,
        sender: Arc<dyn ChannelSender>,
        pending: Arc<PendingDecisionStore>,
        asset_resolver: Option<Arc<dyn crate::message_service::AssetResolver>>,
    ) -> Self {
        Self {
            config,
            sender,
            pending,
            asset_resolver,
        }
    }
```

- [ ] **Step 4: Add a media-flush helper**

Add to `impl ChannelStreamRelay` (after `record_and_send_decision`, ~line 368):

```rust
    /// Resolve each not-yet-sent asset id to bytes and send it via the plugin's
    /// media path. `seen` dedupes across a turn (get_task may be polled). No-ops
    /// when no resolver is wired. Failures are logged, never fatal.
    async fn flush_media(&self, ids: &[String], seen: &mut std::collections::HashSet<String>) {
        let Some(resolver) = self.asset_resolver.as_ref() else {
            return;
        };
        for id in ids {
            if !seen.insert(id.clone()) {
                continue;
            }
            match resolver.resolve(id).await {
                Some(media) => {
                    if let Err(e) = self
                        .sender
                        .send_media(&self.config.plugin_id, &self.config.chat_id, media, None)
                        .await
                    {
                        warn!(asset_id = %id, error = %e, "failed to send channel media");
                    }
                }
                None => warn!(asset_id = %id, "asset could not be resolved for channel media"),
            }
        }
    }
```

- [ ] **Step 5: Accumulate + flush in both relay loops**

In `run_send_once` (~line 84): declare state at the top, next to `text_buffer`:

```rust
        let mut media_ids: Vec<String> = Vec::new();
        let mut media_seen: std::collections::HashSet<String> = std::collections::HashSet::new();
```

Add a match arm for `MediaProduced` (alongside the other `Some(StreamAction::…)` arms):

```rust
                    Some(StreamAction::MediaProduced(ids)) => {
                        media_ids.extend(ids);
                    }
```

In the `Some(StreamAction::Finish)` arm, after the final `send_message(...).await` and before `break;`, add:

```rust
                        // Also catch asset URLs written into the visible text
                        // (the same link the desktop renders), then send images.
                        media_ids.extend(crate::media_refs::asset_ids_from_text(&visible));
                        self.flush_media(&media_ids, &mut media_seen).await;
```

In the `Err(broadcast::error::RecvError::Closed)` arm, after its `send_message(...).await` and before `break;`, add the same two lines (using its local `visible`).

Repeat the identical pattern in `run_editable` (~line 187): declare `media_ids`/`media_seen` next to `text_buffer` (~line 203), add the `MediaProduced` arm, and flush after finalization. In `run_editable` the `Finish` arm calls `self.send_final_edit(...)`; add after it:

```rust
                        let visible = strip_reasoning(&text_buffer, Stage::Final);
                        media_ids.extend(crate::media_refs::asset_ids_from_text(&visible));
                        self.flush_media(&media_ids, &mut media_seen).await;
```

And the same in `run_editable`'s `RecvError::Closed` arm after `send_final_edit`.

- [ ] **Step 6: Update the orchestrator call site**

In `orchestrator.rs`, change the relay construction (~line 334) to pass the resolver from the message service:

```rust
        let relay = ChannelStreamRelay::new(
            relay_config,
            Arc::clone(sender),
            msg_svc.pending_decisions(),
            msg_svc.asset_resolver(),
        );
```

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo nextest run -p nomifun-channel relay_flushes_media_on_finish`
Expected: PASS. Then `cargo nextest run -p nomifun-channel` — no regressions. Then `cargo check -p nomifun-channel`.

- [ ] **Step 8: Fix the `orchestrator_test.rs` constructor call if present**

Run: `cargo nextest run -p nomifun-channel` — if `tests/orchestrator_test.rs` fails to compile because it calls `ChannelStreamRelay::new`, grep it (`grep -n "ChannelStreamRelay::new" crates/backend/nomifun-channel/tests/orchestrator_test.rs`) and add a trailing `None,` argument. (The relay is usually spawned via the orchestrator, so this may not appear — only fix if the compiler flags it.)

- [ ] **Step 9: Commit**

```bash
git add crates/backend/nomifun-channel/src/stream_relay.rs crates/backend/nomifun-channel/src/orchestrator.rs
git commit -m "feat(channel): relay accumulates workshop asset ids and flushes them as media after the final text"
```

---

## Phase B — Telegram pilot (end-to-end proof)

### Task B1: `TelegramApi::send_photo` / `send_document` (multipart)

**Files:**
- Modify: `crates/backend/nomifun-channel/src/plugins/telegram/api.rs`

**Interfaces:**
- Produces: `TelegramApi::send_photo(chat_id: i64, bytes: Vec<u8>, filename: &str, mime: &str, caption: Option<&str>) -> Result<TgMessage, ChannelError>` and `send_document(...)` with the same signature.

- [ ] **Step 1: Verify reqwest multipart is available**

Run: `grep -n "reqwest" Cargo.toml`
Expected: line `reqwest = { version = "0.12", features = [… "multipart" …] }` (root `Cargo.toml:111`). No change needed — the feature is already enabled workspace-wide.

- [ ] **Step 2: Add the multipart upload methods**

In `telegram/api.rs`, add to `impl TelegramApi` (after `send_message`, ~line 108):

```rust
    /// `sendPhoto` — upload raw image bytes as a photo. Returns the sent message.
    pub async fn send_photo(
        &self,
        chat_id: i64,
        bytes: Vec<u8>,
        filename: &str,
        mime: &str,
        caption: Option<&str>,
    ) -> Result<TgMessage, ChannelError> {
        self.send_media_multipart("sendPhoto", "photo", chat_id, bytes, filename, mime, caption)
            .await
    }

    /// `sendDocument` — upload raw bytes as a file attachment.
    pub async fn send_document(
        &self,
        chat_id: i64,
        bytes: Vec<u8>,
        filename: &str,
        mime: &str,
        caption: Option<&str>,
    ) -> Result<TgMessage, ChannelError> {
        self.send_media_multipart("sendDocument", "document", chat_id, bytes, filename, mime, caption)
            .await
    }

    /// Shared multipart POST for photo/document uploads.
    #[allow(clippy::too_many_arguments)]
    async fn send_media_multipart(
        &self,
        method: &str,
        field: &str,
        chat_id: i64,
        bytes: Vec<u8>,
        filename: &str,
        mime: &str,
        caption: Option<&str>,
    ) -> Result<TgMessage, ChannelError> {
        let url = format!("{}/{method}", self.base_url);
        debug!(chat_id, bytes = bytes.len(), method, "Uploading Telegram media");

        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(filename.to_owned())
            .mime_str(mime)
            .map_err(|e| ChannelError::MessageSendFailed(format!("invalid media mime {mime}: {e}")))?;

        let mut form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .part(field.to_owned(), part);
        if let Some(c) = caption {
            form = form.text("caption", c.to_owned());
        }

        let resp: TgResponse<TgMessage> = self
            .client
            .post(&url)
            .multipart(form)
            .send()
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("{method} request failed: {e}")))?
            .json()
            .await
            .map_err(|e| ChannelError::MessageSendFailed(format!("{method} parse failed: {e}")))?;

        if !resp.ok {
            let desc = resp.description.unwrap_or_default();
            return Err(ChannelError::MessageSendFailed(format!("{method} failed: {desc}")));
        }
        resp.result
            .ok_or_else(|| ChannelError::MessageSendFailed(format!("{method} returned no result")))
    }
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo check -p nomifun-channel --features telegram`
Expected: clean. (No unit test here — the method performs real HTTP; it is exercised by the manual end-to-end check in Task B4.)

- [ ] **Step 4: Commit**

```bash
git add crates/backend/nomifun-channel/src/plugins/telegram/api.rs
git commit -m "feat(channel/telegram): add sendPhoto/sendDocument multipart upload"
```

---

### Task B2: Telegram plugin `send_media` override

**Files:**
- Modify: `crates/backend/nomifun-channel/src/plugins/telegram/plugin.rs`

**Interfaces:**
- Consumes: `TelegramApi::send_photo`/`send_document` (B1), `OutgoingMedia`/`MediaKind` (A1), `parse_chat_id` (existing, used by `send_message` at plugin.rs:192).
- Produces: `impl ChannelPlugin for TelegramPlugin { async fn send_media … }`.

- [ ] **Step 1: Add the override in the `ChannelPlugin` impl**

In `telegram/plugin.rs`, add after `edit_message` (~line 244), inside `impl ChannelPlugin for TelegramPlugin`:

```rust
    async fn send_media(
        &self,
        chat_id: &str,
        media: crate::types::OutgoingMedia,
        caption: Option<&str>,
    ) -> Result<String, ChannelError> {
        use crate::types::MediaKind;
        let api = self
            .api
            .as_ref()
            .ok_or_else(|| ChannelError::PlatformApi("Plugin not initialized".into()))?;
        let chat_id_num = parse_chat_id(chat_id)?;

        let sent = match media.kind {
            MediaKind::Image => {
                api.send_photo(chat_id_num, media.bytes, &media.filename, &media.mime, caption)
                    .await?
            }
            MediaKind::File => {
                api.send_document(chat_id_num, media.bytes, &media.filename, &media.mime, caption)
                    .await?
            }
        };
        Ok(sent.message_id.to_string())
    }
```

Ensure `crate::types::OutgoingMedia`/`MediaKind` are importable (they are `pub` in `types.rs`; the `use crate::types::MediaKind;` inside the method is enough — `OutgoingMedia` is named via the parameter's path already).

- [ ] **Step 2: Verify it compiles**

Run: `cargo check -p nomifun-channel --features telegram`
Expected: clean.

Run: `cargo nextest run -p nomifun-channel`
Expected: PASS (no regressions).

- [ ] **Step 3: Commit**

```bash
git add crates/backend/nomifun-channel/src/plugins/telegram/plugin.rs
git commit -m "feat(channel/telegram): implement send_media (photo/document upload)"
```

---

### Task B3: `AssetResolver` implementation in `nomifun-app`

**Files:**
- Create: `crates/backend/nomifun-app/src/channel_asset_resolver.rs`
- Modify: `crates/backend/nomifun-app/src/lib.rs` (register module — match the existing `mod`/`pub mod` style; `workshop_bridge` is declared there, add next to it)

**Interfaces:**
- Consumes: `nomifun_workshop::WorkshopService::read_asset_bytes(&str) -> Result<(Vec<u8>, String), AppError>` (service.rs:556), `nomifun_channel::message_service::AssetResolver` (A2), `nomifun_channel::types::{OutgoingMedia, MediaKind}` (A1).
- Produces: `ChannelAssetResolver` (constructed from `Arc<WorkshopService>`).

- [ ] **Step 1: Write the resolver + a filename-extension unit test**

Create `crates/backend/nomifun-app/src/channel_asset_resolver.rs`:

```rust
//! Bridges `nomifun-channel`'s [`AssetResolver`] to the workshop asset store,
//! so channel replies can upload AI-generated images. Kept in `nomifun-app`
//! (not `nomifun-channel`) so the channel crate has no workshop dependency —
//! same layering as `CompanionMasterAgentProfile`.

use std::sync::Arc;

use nomifun_channel::message_service::AssetResolver;
use nomifun_channel::types::{MediaKind, OutgoingMedia};
use nomifun_workshop::WorkshopService;

pub struct ChannelAssetResolver {
    pub workshop: Arc<WorkshopService>,
}

impl ChannelAssetResolver {
    pub fn new(workshop: Arc<WorkshopService>) -> Self {
        Self { workshop }
    }
}

/// Suggested file extension for a mime type (Telegram/most platforms infer the
/// type from the mime part, but a sensible filename improves the UX).
fn ext_for_mime(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "bin",
    }
}

#[async_trait::async_trait]
impl AssetResolver for ChannelAssetResolver {
    async fn resolve(&self, asset_id: &str) -> Option<OutgoingMedia> {
        match self.workshop.read_asset_bytes(asset_id).await {
            Ok((bytes, mime)) => {
                let kind = if mime.starts_with("image/") {
                    MediaKind::Image
                } else {
                    MediaKind::File
                };
                let filename = format!("{asset_id}.{}", ext_for_mime(&mime));
                Some(OutgoingMedia { bytes, mime, filename, kind })
            }
            Err(e) => {
                tracing::warn!(asset_id = %asset_id, error = %e, "failed to read workshop asset for channel media");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ext_mapping() {
        assert_eq!(ext_for_mime("image/png"), "png");
        assert_eq!(ext_for_mime("image/jpeg"), "jpg");
        assert_eq!(ext_for_mime("application/pdf"), "bin");
    }
}
```

- [ ] **Step 2: Register the module**

Run: `grep -n "mod workshop_bridge" crates/backend/nomifun-app/src/lib.rs`
Add a sibling declaration matching its visibility, e.g.:

```rust
pub mod channel_asset_resolver;
```

- [ ] **Step 3: Verify it compiles and the unit test passes**

Run: `cargo nextest run -p nomifun-app ext_mapping`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/backend/nomifun-app/src/channel_asset_resolver.rs crates/backend/nomifun-app/src/lib.rs
git commit -m "feat(app): ChannelAssetResolver bridging workshop assets to channel media send"
```

---

### Task B4: Wire the resolver into the channel message service + end-to-end verification

**Files:**
- Modify: `crates/backend/nomifun-app/src/router/state.rs:749-761`

**Interfaces:**
- Consumes: `ChannelAssetResolver` (B3), `ChannelMessageService::with_asset_resolver` (A2), `services.workshop_service` (services.rs:115).

- [ ] **Step 1: Add `.with_asset_resolver(...)` to the message-service builder**

In `state.rs`, extend the `ChannelMessageService` construction (currently ending `.with_master_profile(Arc::clone(&master_profile))` at ~line 760):

```rust
    let message_service = Arc::new(
        nomifun_channel::message_service::ChannelMessageService::new(
            conversation_svc,
            services.worker_task_manager.clone(),
            Arc::clone(&channel_settings),
            repo.clone(),
            owner_user_id,
        )
        .with_master_profile(Arc::clone(&master_profile))
        // Outbound media: resolve `wsa_…` ids to bytes so channel replies can
        // send AI-generated images/files.
        .with_asset_resolver(Arc::new(crate::channel_asset_resolver::ChannelAssetResolver::new(
            services.workshop_service.clone(),
        ))),
    );
```

- [ ] **Step 2: Verify the whole app compiles**

Run: `cargo check -p nomifun-app`
Expected: clean.

- [ ] **Step 3: End-to-end manual verification (Telegram)**

This exercises the real path (no automated test — it needs a live bot + model). Steps:

1. Build & run the desktop dev app (Ref: memory `dev-data-dir-is-nomi-dev`): `bun run desktop:dev`.
2. Settings → Channel → add a Telegram bot (token), bind it to a companion with a working image-generation-capable model, start it.
3. From Telegram, DM the bot: `画一张猫猫满天飞的图`.
4. **Expected:** the bot replies with the assistant text (e.g. "图来咯～") AND then sends the generated image as a photo. Confirm the photo renders inline in Telegram.
5. Check logs for `Uploading Telegram media` (debug) and absence of `failed to send channel media`.

- [ ] **Step 4: Verify with the `verify` skill**

Invoke the `verify` skill (drive the affected flow, observe behavior) against the Telegram image-send path before claiming completion. If a live bot is unavailable, document that the automated relay test (`relay_flushes_media_on_finish`) + `cargo check` pass and that live verification is pending.

- [ ] **Step 5: Commit**

```bash
git add crates/backend/nomifun-app/src/router/state.rs
git commit -m "feat(app): wire ChannelAssetResolver so channels send AI-generated images (Telegram end-to-end)"
```

---

## Phase C — WeChat (discovery-gated)

> The WeChat iLink gateway (`ilinkai.weixin.qq.com`) is a closed third-party bot API. Its inbound protocol already models encrypted media (`image_item`/`file_item`, `MediaEncryptInfo{encrypt_query_param, aes_key}`, weixin/types.rs:96-141) and reserves `ITEM_TYPE_IMAGE=2`/`ITEM_TYPE_FILE=4` (types.rs:8-12), but there is **no outbound media-upload endpoint in the code and none documented**. C1 discovers that contract; C2 implements it. Do not fabricate the upload endpoint — C2 is blocked on C1's findings.

### Task C1: Discovery spike — characterize the iLink media-upload contract

**Files:** none (investigation + a written findings note).

- [ ] **Step 1: Capture the outbound send shape and hunt for a media/upload route**

- Re-read `weixin/api.rs` (send path) and `weixin/types.rs` (item structs).
- Search vendor/community docs and any captured traffic for the iLink bot's media-upload endpoint (likely a `POST /ilink/bot/upload…` returning a `media` handle + `aeskey`/`encrypt_query_param`, mirroring the inbound `MediaItemData`).
- Determine whether outbound image send is: (a) upload-then-reference (most likely, per inbound evidence), (b) a public URL, or (c) base64 inline.

- [ ] **Step 2: Write findings to a note and decide the gate**

Write `docs/superpowers/specs/2026-07-XX-wechat-media-upload-findings.md` with: the upload endpoint (URL, headers, request/response schema), the outbound image `item_list` shape (fields on a `type=2` item), and any `message_type`/`message_state` differences for media. **Decision gate:** if the contract cannot be characterized with confidence, STOP the WeChat track and record that WeChat image send requires vendor cooperation; the shared pipeline (Phases A–B) still delivers value on all upload-bytes channels.

- [ ] **Step 3: Commit the findings note**

```bash
git add docs/superpowers/specs/2026-07-XX-wechat-media-upload-findings.md
git commit -m "docs(channel/weixin): iLink outbound media-upload contract findings"
```

### Task C2: Implement WeChat `send_media` (gated on C1)

> Fill the request/response types and the upload call from C1's findings. The parts below that are already known are concrete; the `upload_media` body is the only C1-dependent piece.

**Files:**
- Modify: `crates/backend/nomifun-channel/src/plugins/weixin/types.rs` — add outbound `SendImageItem`/`SendFileItem` and extend `SendMessageItem` with optional `image_item`/`file_item`.
- Modify: `crates/backend/nomifun-channel/src/plugins/weixin/api.rs` — add `upload_media(bytes, mime) -> MediaHandle` (from C1) and `send_image(to_user_id, handle, context_token)`.
- Modify: `crates/backend/nomifun-channel/src/plugins/weixin/plugin.rs` — override `send_media` (fetch bytes → `upload_media` → `send_image`).

- [ ] **Step 1: Extend outbound item structs (known)**

In `weixin/types.rs`, add to `SendMessageItem` (currently text-only, ~line 164) optional media fields and new item structs:

```rust
#[derive(Debug, Serialize)]
pub(crate) struct SendMessageItem {
    #[serde(rename = "type")]
    pub item_type: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_item: Option<SendTextItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_item: Option<SendImageItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_item: Option<SendFileItem>,
}

// Shape TBD from C1 — the media reference the upload call returns.
#[derive(Debug, Serialize)]
pub(crate) struct SendImageItem { /* fields from C1: media handle / aeskey / encrypt params */ }
#[derive(Debug, Serialize)]
pub(crate) struct SendFileItem { /* fields from C1 */ }
```

Update the existing text construction in `api.rs::send_message` to set `image_item: None, file_item: None` on the text `SendMessageItem`.

- [ ] **Step 2: Add the upload + image-send API calls (C1-dependent)**

In `weixin/api.rs`, add `upload_media` (endpoint/schema from C1) and `send_image`, using item_type `ITEM_TYPE_IMAGE` (2). Then override `send_media` in `weixin/plugin.rs`:

```rust
    async fn send_media(
        &self,
        chat_id: &str,
        media: crate::types::OutgoingMedia,
        _caption: Option<&str>,
    ) -> Result<String, ChannelError> {
        let api = self.api.as_ref().ok_or_else(|| ChannelError::PlatformApi("Plugin not initialized".into()))?;
        let context_token = self.context_tokens.get(chat_id).map(|v| v.clone());
        let handle = api.upload_media(media.bytes, &media.mime).await?; // C1
        api.send_image(chat_id, handle, context_token.as_deref()).await?;
        Ok(String::new())
    }
```

- [ ] **Step 3: Verify + live-test**

Run: `cargo check -p nomifun-channel --features weixin` (clean), then live-test the WeChat image reply exactly as in Task B4 step 3 (via WeChat instead of Telegram).

- [ ] **Step 4: Commit**

```bash
git add crates/backend/nomifun-channel/src/plugins/weixin/
git commit -m "feat(channel/weixin): send AI-generated images via iLink media upload"
```

---

## Rollout guide — remaining channels (follow the Telegram recipe)

The shared pipeline (Phases A) is done once; each channel is then a self-contained follow-up = **override `ChannelPlugin::send_media`** + **add the platform's media endpoint in its `api.rs`**. Per-channel specifics (from the codebase audit):

| Channel | Endpoint to add | Transport | Notes |
|---|---|---|---|
| Discord | multipart message-create (or embed) | bytes | `discord/api.rs` create_message → add `files[]` multipart |
| Slack | `files.getUploadURLExternal` + `files.completeUploadExternal` | bytes (multi-step) | |
| Lark | `POST im/v1/images` → `image_key` then `msg_type:"image"` | bytes | `lark/api.rs` |
| Matrix | `POST /_matrix/media/v3/upload` → `mxc://` then `m.image` | bytes | `matrix/api.rs` |
| Mattermost | `POST /files` → `file_ids` on `create_post` | bytes | |
| QQ Bot | rich-media upload → `msg_type:7` | bytes | official API |
| DingTalk | `send_robot_message` `msg_key:"sampleImageMsg"` | **needs public picURL** | our asset URL is localhost-only → prefer upload if available |
| WeCom | media frame over `aibot_send_msg` WS | unverified | discovery spike like WeChat |
| Twitch / Nostr | — | — | **protocol-level text-only**; do NOT implement `send_media` (default no-op is correct; optionally append the URL into text) |

Each channel: write the `api.rs` upload method, override `send_media`, `cargo check --features <channel>`, then live-verify. No pipeline changes.

---

## Self-Review

**1. Spec coverage (against the root-cause analysis):**
- Root cause "no image-carrying stream signal" → A2 taps completed tool-call `output` (`result_asset_ids`) + A4 taps text URLs. ✅
- "relay only forwards text" → A4 adds media accumulation/flush. ✅
- "plugins drop image_url" → A3 adds `send_media`; B2 implements Telegram. ✅
- "asset URL is localhost-only → remote can't fetch" → resolved by uploading BYTES (B1/B3), never handing a URL. ✅
- "extend to other channels" → Rollout guide + capability matrix; default no-op keeps text-only channels correct. ✅
- WeChat's unknown upload contract → honestly gated in C1/C2 (no fabricated endpoint). ✅

**2. Placeholder scan:** Phase A/B steps contain full code. The only intentionally-deferred content is C2's `upload_media` body and the `SendImageItem` fields, which are gated on the C1 discovery task and explicitly flagged — this is a real unknown (closed vendor API), not a lazy placeholder.

**3. Type consistency:** `OutgoingMedia`/`MediaKind` (A1) are used identically in A3 (`send_media` params), A4 (`flush_media`/StubResolver), B2 (Telegram), B3 (resolver). `AssetResolver::resolve(&self, asset_id: &str) -> Option<OutgoingMedia>` matches across A2 (trait), A4 (stub), B3 (impl). `ChannelStreamRelay::new` is 4-arg everywhere after A4. `send_media` signature `(chat_id/plugin_id, media, caption)` matches across `ChannelPlugin` (A3), `ChannelSender` (A3), `ChannelManager` (A3), Telegram (B2), WeChat (C2). `read_asset_bytes -> Result<(Vec<u8>, String), AppError>` used correctly in B3.
