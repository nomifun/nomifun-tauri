//! Opt-in event collector. Subscribes to the global broadcast bus and appends
//! normalized JSONL records to `{companion_dir}/events/YYYYMMDD.jsonl` for the
//! sources the user has enabled. Assistant replies are accumulated per
//! `(conversation_id, msg_id)` from `message.stream` content chunks and only
//! flushed on `turn.completed` — the bus has no single "assistant reply
//! finished" event carrying the full text.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nomifun_api_types::WebSocketMessage;
use nomifun_common::{AppError, CompanionEventId, CompanionId, now_ms, validate_uuidv7};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::profile::SharedCompanionConfig;

const MAX_FIELD_CHARS: usize = 2000;
const MAX_REPLY_CHARS: usize = 4000;
/// Global cap on concurrently buffered assistant replies. A `turn.completed`
/// lost to a Lagged bus receiver orphans its buffers; without a cap they
/// accumulate for the life of the process (companion_dialogues defaults ON, so
/// every conversation buffers). Oldest-created entries are evicted first.
const MAX_REPLY_BUFFERS: usize = 64;

/// One normalized JSONL record.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CollectedEvent {
    #[serde(deserialize_with = "deserialize_event_id")]
    pub event_id: String,
    pub ts: i64,
    pub source: String,
    pub name: String,
    pub data: serde_json::Value,
}

fn deserialize_event_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let event_id = String::deserialize(deserializer)?;
    validate_uuidv7(&event_id).map_err(serde::de::Error::custom)?;
    Ok(event_id)
}

/// Shared live view of the cross-companion config (updated on every config write).
pub type SharedConfig = Arc<RwLock<SharedCompanionConfig>>;

pub struct Collector {
    companion_dir: PathBuf,
    config: SharedConfig,
    /// Companion-thread membership + XP. Companion conversations are nomi
    /// talking — they must never feed the learner (self-learning loop), but
    /// each completed companion turn earns XP.
    store: crate::store::CompanionStore,
    /// (conversation_id, msg_id) -> accumulated assistant text.
    reply_buffers: HashMap<(String, String), String>,
    /// Buffer creation order for [`MAX_REPLY_BUFFERS`] eviction. May hold
    /// tombstones for already-flushed keys; pruned lazily.
    reply_buffer_order: VecDeque<(String, String)>,
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{truncated}…")
    }
}

/// Truncate every top-level string field of a JSON object to `max` chars (non-objects
/// returned unchanged). Used for `cron_runs`, whose payload (`job_id`/`status`/`error`)
/// carries an `error` field that can be an arbitrarily long traceback/model error.
fn truncate_json_strings(mut value: serde_json::Value, max: usize) -> serde_json::Value {
    if let Some(obj) = value.as_object_mut() {
        for v in obj.values_mut() {
            if let serde_json::Value::String(s) = v {
                *v = serde_json::Value::String(truncate_chars(s, max));
            }
        }
    }
    value
}

/// Normalized SHAPE of a tool's args: sorted `"key:type"` for each top-level key,
/// where type ∈ {string,number,bool,array,object,null}. Carries NO values, so a
/// secret in an arg value can never be persisted. Non-object args → empty shape.
fn param_shape(args: &serde_json::Value) -> Vec<String> {
    let Some(obj) = args.as_object() else {
        return Vec::new();
    };
    let mut shape: Vec<String> = obj
        .iter()
        .map(|(k, v)| {
            let t = match v {
                serde_json::Value::String(_) => "string",
                serde_json::Value::Number(_) => "number",
                serde_json::Value::Bool(_) => "bool",
                serde_json::Value::Array(_) => "array",
                serde_json::Value::Object(_) => "object",
                serde_json::Value::Null => "null",
            };
            format!("{k}:{t}")
        })
        .collect();
    shape.sort();
    shape
}

/// The non-empty `origin` marker of a broadcast payload, if any. The
/// conversation domain stamps `"companion"` / `"cron"` / `"autowork"` / `"idmm"`
/// onto messages that were NOT typed by the human owner; absent/empty means
/// a real person spoke.
fn payload_origin(data: &serde_json::Value) -> Option<&str> {
    data.get("origin")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn events_dir(companion_dir: &Path) -> PathBuf {
    companion_dir.join("events")
}

/// Day-stamped file name like `20260611.jsonl` (local time — the user reads
/// the "today" stat in their own timezone; rotation granularity only).
fn day_file_name(ts_ms: i64) -> String {
    use chrono::{Local, TimeZone};
    let dt = Local
        .timestamp_millis_opt(ts_ms)
        .single()
        .unwrap_or_else(chrono::Local::now);
    format!("{}.jsonl", dt.format("%Y%m%d"))
}

impl Collector {
    pub fn new(companion_dir: PathBuf, config: SharedConfig, store: crate::store::CompanionStore) -> Self {
        Self {
            companion_dir,
            config,
            store,
            reply_buffers: HashMap::new(),
            reply_buffer_order: VecDeque::new(),
        }
    }

    /// True when the conversation is a companion thread (nomi's own chats).
    /// Errors degrade to `false` — collection proceeds, XP is skipped.
    async fn is_companion(&self, conversation_id: &str) -> bool {
        self.store
            .is_companion_thread(conversation_id)
            .await
            .unwrap_or(false)
    }

    /// Companion determination for one event: the wire marker
    /// (`companion: true`, stamped by the conversation domain from
    /// `extra.companion_session`) wins, falling back to the local thread registry
    /// for events that predate the marker. The marker also covers entry
    /// points the registry never sees (Channel Agent sessions).
    async fn is_companion_event(&self, data: &serde_json::Value) -> bool {
        if data.get("companion").and_then(|v| v.as_bool()).unwrap_or(false) {
            return true;
        }
        match data.get("conversation_id").and_then(|v| v.as_str()) {
            Some(conv) => self.is_companion(conv).await,
            None => false,
        }
    }

    /// Attribution target for a companion event: the wire `companion_id` when
    /// present, else the thread registry's owner, else the default companion,
    /// else nobody.
    async fn resolve_companion_id(&self, data: &serde_json::Value) -> Option<String> {
        if let Some(id) = data
            .get("companion_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .and_then(|id| CompanionId::try_from(id).ok())
        {
            return Some(id.into_string());
        }
        if let Some(conv) = data.get("conversation_id").and_then(|v| v.as_str())
            && let Some(id) = self.store.thread_companion_id(conv).await.ok().flatten()
        {
            return Some(id);
        }
        self.config.read().await.default_companion_id.clone()
    }

    /// Spawn the bus-tap loop. The collector observes both instance-public and
    /// owner-scoped events, but never changes either event's delivery audience.
    /// Lagged receivers skip ahead; closing either half of the shared bus ends
    /// the task (both senders have the same owner and lifetime).
    pub fn spawn(mut self, bus: Arc<nomifun_realtime::BroadcastEventBus>) {
        let mut public_rx = bus.subscribe();
        let mut user_rx = bus.subscribe_user();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = public_rx.recv() => match result {
                        Ok(msg) => self.handle(&msg).await,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                            tracing::debug!(
                                skipped,
                                audience = "public",
                                "companion collector lagged behind the event bus"
                            );
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    },
                    result = user_rx.recv() => match result {
                        Ok(envelope) => self.handle(&envelope.event).await,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                            tracing::debug!(
                                skipped,
                                audience = "user",
                                "companion collector lagged behind the event bus"
                            );
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        });
    }

    /// Mutable access to one reply buffer, creating it under the global
    /// [`MAX_REPLY_BUFFERS`] cap (oldest-created entries evicted first).
    fn reply_buffer_mut(&mut self, key: (String, String)) -> &mut String {
        if !self.reply_buffers.contains_key(&key) {
            while self.reply_buffers.len() >= MAX_REPLY_BUFFERS {
                let Some(oldest) = self.reply_buffer_order.pop_front() else { break };
                if self.reply_buffers.remove(&oldest).is_some() {
                    tracing::debug!(
                        conversation_id = %oldest.0,
                        msg_id = %oldest.1,
                        "companion collector evicted oldest reply buffer (global cap)"
                    );
                }
            }
            // Flushed buffers leave tombstone keys in the order queue; prune
            // them once they dominate so the queue stays O(cap).
            if self.reply_buffer_order.len() >= MAX_REPLY_BUFFERS * 4 {
                let live = &self.reply_buffers;
                self.reply_buffer_order.retain(|k| live.contains_key(k));
            }
            self.reply_buffer_order.push_back(key.clone());
        }
        self.reply_buffers.entry(key).or_default()
    }

    async fn handle(&mut self, msg: &WebSocketMessage<serde_json::Value>) {
        let collect = { self.config.read().await.collect.clone() };
        match msg.name.as_str() {
            "message.userCreated" if collect.chat_user_messages || collect.companion_dialogues => {
                // Hidden messages (system-injected prompts, cron kickoffs) are
                // not the user speaking — never collect them.
                if msg.data.get("hidden").and_then(|h| h.as_bool()).unwrap_or(false) {
                    return;
                }
                // A non-empty `origin` (companion/cron/autowork/idmm) marks a
                // message injected by an agent, not typed by the owner.
                // Treating those as owner speech is exactly the self-
                // reinforcing loop that made companions re-execute old requests —
                // skip them for every collection source.
                if payload_origin(&msg.data).is_some() {
                    return;
                }
                if self.is_companion_event(&msg.data).await {
                    // Companion threads: the owner talking TO a companion. Collect
                    // as a dedicated high-value source (default ON); never as
                    // a generic work-chat message.
                    if collect.companion_dialogues {
                        let companion_id = self.resolve_companion_id(&msg.data).await;
                        let data = serde_json::json!({
                            "conversation_id": msg.data.get("conversation_id"),
                            "companion_id": companion_id,
                            "content": truncate_chars(msg.data.get("content").and_then(|c| c.as_str()).unwrap_or(""), MAX_FIELD_CHARS),
                        });
                        self.append("companion_dialogues", "companion.user_message", data);
                    }
                    return;
                }
                if !collect.chat_user_messages {
                    return;
                }
                let data = serde_json::json!({
                    "conversation_id": msg.data.get("conversation_id"),
                    "content": truncate_chars(msg.data.get("content").and_then(|c| c.as_str()).unwrap_or(""), MAX_FIELD_CHARS),
                });
                self.append("chat_user_messages", &msg.name, data);
            }
            "message.stream" if collect.chat_assistant_replies || collect.companion_dialogues || collect.tool_calls => {
                // Accumulate visible content chunks; flushed on turn.completed.
                let kind = msg.data.get("type").and_then(|t| t.as_str()).unwrap_or("");
                // Tool-call signal (design §5.1): the primary skill-mining input.
                // Tool calls arrive on this same bus event with type=="tool_call".
                // We persist ONLY the tool name + a normalized param SHAPE (sorted
                // top-level arg keys + JSON types) — NEVER arg/input/output values
                // (secrets). One record per call (on Completed only).
                if kind == "tool_call" {
                    if !collect.tool_calls {
                        return;
                    }
                    // Same anti-self-reinforcement guard as the content path: agent-
                    // driven turns (companion/cron/autowork/idmm) are not owner work.
                    if payload_origin(&msg.data).is_some() {
                        return;
                    }
                    if msg.data.get("hidden").and_then(|h| h.as_bool()).unwrap_or(false) {
                        return;
                    }
                    let d = msg.data.get("data");
                    if d.and_then(|x| x.get("status")).and_then(|s| s.as_str()) != Some("completed") {
                        return; // one record per call (skip the earlier "running" update)
                    }
                    let name = d.and_then(|x| x.get("name")).and_then(|n| n.as_str()).unwrap_or("");
                    if name.is_empty() {
                        return;
                    }
                    let shape = d.and_then(|x| x.get("args")).map(param_shape).unwrap_or_default();
                    let data = serde_json::json!({
                        "name": name,
                        "param_shape": shape,
                        "conversation_id": msg.data.get("conversation_id"),
                        "call_id": d.and_then(|x| x.get("call_id")).and_then(|c| c.as_str()).unwrap_or(""),
                    });
                    self.append("tool_calls", "tool.call", data);
                    return;
                }
                if kind != "content" && kind != "text" {
                    return;
                }
                let (Some(conv), Some(mid)) = (
                    msg.data.get("conversation_id").and_then(|v| v.as_str()),
                    msg.data.get("msg_id").and_then(|v| v.as_str()),
                ) else {
                    return;
                };
                let key = (conv.to_owned(), mid.to_owned());
                // Agent-driven turns (origin: companion/cron/autowork/idmm, stamped
                // by the stream relay) are NOT the owner's work — buffering
                // their replies would let companion/cron-driven output be distilled
                // as owner intent (the indirect feedback loop). Mirrors the
                // userCreated origin filter; drop anything already buffered.
                if payload_origin(&msg.data).is_some() {
                    self.reply_buffers.remove(&key);
                    return;
                }
                let chunk = match msg.data.get("data") {
                    Some(serde_json::Value::String(s)) => s.clone(),
                    Some(obj) => obj
                        .get("content")
                        .and_then(|c| c.as_str())
                        .map(str::to_owned)
                        .unwrap_or_default(),
                    None => String::new(),
                };
                let hidden = msg.data.get("hidden").and_then(|h| h.as_bool()).unwrap_or(false);
                let replace = msg.data.get("replace").and_then(|r| r.as_bool()).unwrap_or(false);
                // Middleware final-text overrides (`replace: true`) rewrite
                // what the user actually sees (e.g. cron directives stripped
                // from the reply). They must be applied BEFORE the hidden
                // check: a hidden override means "this text was cleaned
                // away" — keeping the raw buffer would persist content the
                // user never saw (including directive originals).
                if replace {
                    if hidden || chunk.is_empty() {
                        self.reply_buffers.remove(&key);
                    } else {
                        *self.reply_buffer_mut(key) = chunk;
                    }
                    return;
                }
                if hidden || chunk.is_empty() {
                    return;
                }
                let buf = self.reply_buffer_mut(key);
                buf.push_str(&chunk);
                // Hard cap so a runaway stream can't balloon memory.
                if buf.chars().count() > MAX_REPLY_CHARS * 2 {
                    *buf = truncate_chars(buf, MAX_REPLY_CHARS);
                }
            }
            "turn.completed" => {
                let Some(conv) = msg.data.get("conversation_id").and_then(|v| v.as_str()) else {
                    return;
                };
                // Agent-driven turn (origin: companion/cron/autowork/idmm): nothing
                // here is the owner working or chatting. Drop the buffered
                // replies unflushed (defense in depth alongside the per-chunk
                // origin filter) and skip XP — a cron job must not farm
                // companion XP.
                if payload_origin(&msg.data).is_some() {
                    self.reply_buffers.retain(|(c, _), _| c != conv);
                    return;
                }
                // Companion turn: award XP to the owning companion (the old
                // in-crate chat gave +2 per turn; the conversation engine
                // path keeps the same curve). With companion_dialogues enabled the
                // buffered companion reply is collected as `companion.reply` (context for
                // the learner — its rules forbid reading it as owner intent);
                // with it disabled the reply is dropped as before.
                if self.is_companion_event(&msg.data).await {
                    let companion_id = self.resolve_companion_id(&msg.data).await;
                    if let Some(companion_id) = &companion_id {
                        let _ = self.store.add_companion_xp(companion_id, 2).await;
                    }
                    let drained: Vec<(String, String)> = self
                        .reply_buffers
                        .keys()
                        .filter(|(c, _)| c == conv)
                        .cloned()
                        .collect();
                    for key in drained {
                        let Some(text) = self.reply_buffers.remove(&key) else { continue };
                        if !collect.companion_dialogues || text.trim().is_empty() {
                            continue;
                        }
                        let data = serde_json::json!({
                            "conversation_id": key.0,
                            "companion_id": companion_id,
                            "content": truncate_chars(&text, MAX_REPLY_CHARS),
                        });
                        self.append("companion_dialogues", "companion.reply", data);
                    }
                    return;
                }
                if !collect.chat_assistant_replies {
                    // Only this conversation's buffers — another (companion)
                    // conversation may still have a companion_dialogues flush
                    // pending.
                    self.reply_buffers.retain(|(c, _), _| c != conv);
                    return;
                }
                let drained: Vec<(String, String)> = self
                    .reply_buffers
                    .iter()
                    .filter(|((c, _), _)| c == conv)
                    .map(|((c, m), _)| (c.clone(), m.clone()))
                    .collect();
                for key in drained {
                    if let Some(text) = self.reply_buffers.remove(&key) {
                        if text.trim().is_empty() {
                            continue;
                        }
                        let data = serde_json::json!({
                            "conversation_id": key.0,
                            "content": truncate_chars(&text, MAX_REPLY_CHARS),
                        });
                        self.append("chat_assistant_replies", "assistant.reply", data);
                    }
                }
            }
            "requirement.created" if collect.requirements => {
                // Agent-created requirements (gateway tools, autowork) are
                // the system's own output — distilling them as "the owner
                // wants X" closes the duplicate-creation feedback loop.
                if msg.data.get("created_by").and_then(|v| v.as_str()) == Some("agent") {
                    return;
                }
                let data = serde_json::json!({
                    "title": msg.data.get("title"),
                    "created_by": msg.data.get("created_by"),
                    "content": truncate_chars(
                        msg.data.get("content").and_then(|d| d.as_str()).unwrap_or(""),
                        MAX_FIELD_CHARS
                    ),
                    "tag": msg.data.get("tag"),
                });
                self.append("requirements", &msg.name, data);
            }
            "cron.job-executed" if collect.cron_runs => {
                self.append("cron_runs", &msg.name, truncate_json_strings(msg.data.clone(), MAX_FIELD_CHARS));
            }
            "conversation.listChanged" if collect.conversation_lifecycle => {
                self.append("conversation_lifecycle", &msg.name, msg.data.clone());
            }
            "terminal.created" | "terminal.exit" | "terminal.removed" if collect.terminal_sessions => {
                // Metadata only — never PTY output content.
                let data = serde_json::json!({
                    "terminal_id": msg.data.get("terminal_id"),
                    "exit_code": msg.data.get("exit_code")
                });
                self.append("terminal_sessions", &msg.name, data);
            }
            _ => {}
        }
    }

    fn append(&self, source: &str, name: &str, data: serde_json::Value) {
        let event = CollectedEvent {
            event_id: CompanionEventId::new().into_string(),
            ts: now_ms(),
            source: source.to_owned(),
            name: name.to_owned(),
            data,
        };
        if let Err(e) = append_event(&self.companion_dir, &event) {
            tracing::warn!(error = %e, source, "companion collector failed to append event");
        }
    }
}

/// Append one event to the day file. Standalone so the learner/tests reuse it.
pub fn append_event(companion_dir: &Path, event: &CollectedEvent) -> std::io::Result<()> {
    use std::io::Write;
    validate_uuidv7(&event.event_id)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    let dir = events_dir(companion_dir);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(day_file_name(event.ts));
    let created = !path.exists();
    let mut line = serde_json::to_string(event)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    line.push('\n');
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(line.as_bytes())?;
    file.sync_data()?;
    if created {
        crate::fsio::sync_dir(&dir)?;
    }
    Ok(())
}

fn event_files(companion_dir: &Path) -> Result<Vec<PathBuf>, AppError> {
    let dir = events_dir(companion_dir);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(AppError::Internal(format!(
                "read companion event directory {}: {error}",
                dir.display()
            )));
        }
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            AppError::Internal(format!(
                "scan companion event directory {}: {error}",
                dir.display()
            ))
        })?;
        let file_type = entry.file_type().map_err(|error| {
            AppError::Internal(format!(
                "inspect companion event entry {}: {error}",
                entry.path().display()
            ))
        })?;
        if !file_type.is_file() {
            return Err(AppError::Internal(format!(
                "companion event directory contains non-regular entry {}",
                entry.path().display()
            )));
        }
        validate_event_file_name(&entry.path())?;
        files.push(entry.path());
    }
    files.sort();
    Ok(files)
}

fn validate_event_file_name(path: &Path) -> Result<(), AppError> {
    let name = path.file_name().and_then(|name| name.to_str()).ok_or_else(|| {
        AppError::Internal(format!("companion event file has a non-UTF8 name: {}", path.display()))
    })?;
    let Some(day) = name.strip_suffix(".jsonl") else {
        return Err(AppError::Internal(format!(
            "companion event directory contains unsupported file {name:?}"
        )));
    };
    if day.len() != 8 || !day.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(AppError::Internal(format!(
            "companion event file name must be YYYYMMDD.jsonl: {name:?}"
        )));
    }
    Ok(())
}

fn parse_event_file(path: &Path) -> Result<Vec<CollectedEvent>, AppError> {
    validate_event_file_name(path)?;
    let raw = std::fs::read(path).map_err(|error| {
        AppError::Internal(format!("read companion event file {}: {error}", path.display()))
    })?;
    if !raw.ends_with(b"\n") {
        return Err(AppError::Internal(format!(
            "companion event file {} has an incomplete final record",
            path.display()
        )));
    }
    let mut events = Vec::new();
    let lines: Vec<&[u8]> = raw.split(|byte| *byte == b'\n').collect();
    for (index, line) in lines.iter().enumerate() {
        if line.is_empty() {
            if index + 1 == lines.len() && raw.ends_with(b"\n") {
                continue;
            }
            return Err(AppError::Internal(format!(
                "companion event file {} contains an empty record at line {}",
                path.display(),
                index + 1
            )));
        }
        match serde_json::from_slice::<CollectedEvent>(line) {
            Ok(event) => events.push(event),
            Err(error) => {
                return Err(AppError::Internal(format!(
                    "companion event file {} is corrupt at line {}: {error}",
                    path.display(),
                    index + 1
                )));
            }
        }
    }
    Ok(events)
}

/// Strict integrity check for callers that do not need the parsed rows.
pub(crate) fn validate_event_file(path: &Path) -> Result<(), AppError> {
    parse_event_file(path).map(|_| ())
}

pub(crate) fn validate_event_store(companion_dir: &Path) -> Result<(), AppError> {
    for path in event_files(companion_dir)? {
        validate_event_file(&path)?;
    }
    Ok(())
}

/// Read events newer than `cursor_ts`, oldest first, up to `limit`.
/// Returns `(events, truncated)`.
///
/// The caller advances its cursor to the last returned event's timestamp and
/// later skips `ts <= cursor`. Timestamps are millisecond-granular and NOT
/// unique, so a truncation cut must never land inside a same-millisecond
/// group — events sharing the last included timestamp are pulled in even if
/// that overshoots `limit` slightly, otherwise they would be skipped forever.
pub fn read_events_since(
    companion_dir: &Path,
    cursor_ts: i64,
    limit: usize,
) -> Result<(Vec<CollectedEvent>, bool), AppError> {
    let files = event_files(companion_dir)?;
    let mut events: Vec<CollectedEvent> = Vec::new();
    let mut truncated = false;
    'outer: for file in files {
        for event in parse_event_file(&file)? {
            if event.ts <= cursor_ts {
                continue;
            }
            if events.len() >= limit {
                let last_ts = events.last().map(|e| e.ts).unwrap_or(cursor_ts);
                if event.ts == last_ts {
                    events.push(event);
                    continue;
                }
                truncated = true;
                break 'outer;
            }
            events.push(event);
        }
    }
    Ok((events, truncated))
}

/// Read the newest `limit` events (chronological order). Walks day files
/// newest-first and stops as soon as enough events are gathered — never
/// loads the whole (unbounded) history the way `read_events_since(.., 0, ..)`
/// would.
pub fn read_recent_events(
    companion_dir: &Path,
    limit: usize,
) -> Result<Vec<CollectedEvent>, AppError> {
    let files = event_files(companion_dir)?;
    let mut newest_first: Vec<CollectedEvent> = Vec::new();
    for file in files.iter().rev() {
        let mut day = parse_event_file(file)?;
        day.reverse();
        newest_first.extend(day);
        if newest_first.len() >= limit {
            break;
        }
    }
    newest_first.truncate(limit);
    newest_first.reverse();
    Ok(newest_first)
}

/// Per-source counts: (today, total). "Today" is the current local-time day
/// file (matching `day_file_name`, which buckets in local time).
pub fn event_stats(
    companion_dir: &Path,
) -> Result<HashMap<String, (u64, u64)>, AppError> {
    let today = day_file_name(now_ms());
    let mut stats: HashMap<String, (u64, u64)> = HashMap::new();
    for path in event_files(companion_dir)? {
        let is_today = path.file_name().is_some_and(|n| n.to_string_lossy() == today);
        for event in parse_event_file(&path)? {
            let slot = stats.entry(event.source).or_default();
            slot.1 += 1;
            if is_today {
                slot.0 += 1;
            }
        }
    }
    Ok(stats)
}

/// Delete every collected event file.
pub fn clear_events(companion_dir: &Path) -> std::io::Result<()> {
    let dir = events_dir(companion_dir);
    crate::fsio::remove_path_entry(&dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn companion_fixture(sequence: u64) -> String {
        let raw = format!("0190f5fe-7c00-7a00-8abc-{sequence:012}");
        nomifun_common::CompanionId::try_from(raw.as_str()).unwrap().into_string()
    }

    fn conversation_fixture(sequence: u64) -> String {
        let raw = format!("0190f5fe-7c00-7a00-8abc-{sequence:012}");
        nomifun_common::ConversationId::try_from(raw.as_str()).unwrap().into_string()
    }

    #[tokio::test]
    async fn tool_calls_collected_as_name_and_shape_without_values() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::CompanionStore::open_memory().await.unwrap();
        let conversation = conversation_fixture(10);
        let mut config = SharedCompanionConfig::default();
        config.collect.tool_calls = true;
        let mut collector = Collector::new(dir.path().to_path_buf(), Arc::new(RwLock::new(config)), store);

        // A completed tool call with a secret in its args.
        collector
            .handle(&WebSocketMessage::new(
                "message.stream",
                serde_json::json!({
                    "conversation_id": conversation,
                    "msg_id": "m1",
                    "type": "tool_call",
                    "data": {"call_id": "tc1", "name": "grep", "args": {"pattern": "SECRET_TOKEN", "path": "/x"}, "status": "completed"}
                }),
            ))
            .await;

        let (events, _) = read_events_since(dir.path(), 0, 10).unwrap();
        assert_eq!(events.len(), 1, "completed tool call must be collected");
        assert_eq!(events[0].source, "tool_calls");
        assert_eq!(events[0].data["name"], "grep");
        assert_eq!(events[0].data["call_id"], "tc1");
        // Shape carries keys+types, never values.
        let serialized = serde_json::to_string(&events[0]).unwrap();
        assert!(!serialized.contains("SECRET_TOKEN"), "secret value must never be persisted: {serialized}");
        assert!(serialized.contains("pattern:string"));
        assert!(serialized.contains("path:string"));
    }

    #[tokio::test]
    async fn cron_runs_truncates_long_error_payload() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::CompanionStore::open_memory().await.unwrap();
        let mut config = SharedCompanionConfig::default();
        config.collect.cron_runs = true;
        let mut collector = Collector::new(dir.path().to_path_buf(), Arc::new(RwLock::new(config)), store);

        let long_err = "x".repeat(MAX_FIELD_CHARS * 3);
        collector
            .handle(&WebSocketMessage::new(
                "cron.job-executed",
                serde_json::json!({"job_id": "j1", "status": "failed", "error": long_err}),
            ))
            .await;

        let (events, _) = read_events_since(dir.path(), 0, 10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].source, "cron_runs");
        assert_eq!(events[0].data["job_id"], "j1");
        assert_eq!(events[0].data["status"], "failed");
        // error truncated to the field cap (+ ellipsis), not the raw 3× blob.
        let err = events[0].data["error"].as_str().unwrap();
        assert!(err.chars().count() <= MAX_FIELD_CHARS + 1, "cron error must be truncated, got {} chars", err.chars().count());
        assert!(err.ends_with('…'));
    }

    #[tokio::test]
    async fn running_tool_calls_and_origin_marked_are_not_collected() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::CompanionStore::open_memory().await.unwrap();
        let conversation = conversation_fixture(20);
        let mut config = SharedCompanionConfig::default();
        config.collect.tool_calls = true;
        let mut collector = Collector::new(dir.path().to_path_buf(), Arc::new(RwLock::new(config)), store);

        // status=running → not a final record.
        collector
            .handle(&WebSocketMessage::new(
                "message.stream",
                serde_json::json!({"conversation_id": &conversation, "msg_id": "m", "type": "tool_call",
                    "data": {"call_id": "t", "name": "grep", "args": {}, "status": "running"}}),
            ))
            .await;
        // origin-stamped (agent-driven) → anti-self-reinforcement skip.
        collector
            .handle(&WebSocketMessage::new(
                "message.stream",
                serde_json::json!({"conversation_id": &conversation, "msg_id": "m2", "type": "tool_call", "origin": "companion",
                    "data": {"call_id": "t2", "name": "read", "args": {}, "status": "completed"}}),
            ))
            .await;
        let (events, _) = read_events_since(dir.path(), 0, 10).unwrap();
        assert!(events.is_empty(), "running + origin-marked tool calls must be dropped, got {events:?}");
    }

    #[tokio::test]
    async fn tool_calls_not_collected_when_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::CompanionStore::open_memory().await.unwrap();
        let conversation = conversation_fixture(21);
        // tool_calls defaults false; companion_dialogues default true keeps the arm guard active.
        let config = SharedCompanionConfig::default();
        let mut collector = Collector::new(dir.path().to_path_buf(), Arc::new(RwLock::new(config)), store);
        collector
            .handle(&WebSocketMessage::new(
                "message.stream",
                serde_json::json!({"conversation_id": conversation, "msg_id": "m", "type": "tool_call",
                    "data": {"call_id": "t", "name": "grep", "args": {}, "status": "completed"}}),
            ))
            .await;
        let (events, _) = read_events_since(dir.path(), 0, 10).unwrap();
        assert!(events.is_empty(), "tool calls must not be collected when tool_calls=false");
    }

    #[tokio::test]
    async fn companion_turns_earn_xp_and_skip_collection_when_companion_dialogues_off() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::CompanionStore::open_memory().await.unwrap();
        let companion_conversation = conversation_fixture(1);
        let work_conversation = conversation_fixture(2);
        let companion = companion_fixture(1);
        let other_companion = companion_fixture(2);
        store.insert_companion_thread(&companion_conversation, &companion, "聊天").await.unwrap();
        let mut config = SharedCompanionConfig::default();
        config.collect.chat_user_messages = true;
        config.collect.chat_assistant_replies = true;
        config.collect.companion_dialogues = false;
        let mut collector = Collector::new(
            dir.path().to_path_buf(),
            Arc::new(RwLock::new(config)),
            store.clone(),
        );

        // Companion user message: not collected (companion_dialogues off).
        collector
            .handle(&WebSocketMessage::new(
                "message.userCreated",
                serde_json::json!({"conversation_id": companion_conversation, "content": "你好 nomi"}),
            ))
            .await;
        // Normal conversation user message: collected.
        collector
            .handle(&WebSocketMessage::new(
                "message.userCreated",
                serde_json::json!({"conversation_id": work_conversation, "content": "帮我修 bug"}),
            ))
            .await;
        let (events, _) = read_events_since(dir.path(), 0, 10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data["conversation_id"], work_conversation);

        // Companion reply stream + turn.completed: buffered text dropped, XP
        // awarded to the owning companion only.
        collector
            .handle(&WebSocketMessage::new(
                "message.stream",
                serde_json::json!({"conversation_id": companion_conversation, "msg_id": "m1", "type": "content", "data": "我自己的回复"}),
            ))
            .await;
        collector
            .handle(&WebSocketMessage::new(
                "turn.completed",
                serde_json::json!({"conversation_id": companion_conversation}),
            ))
            .await;
        let (events, _) = read_events_since(dir.path(), 0, 10).unwrap();
        assert_eq!(events.len(), 1, "companion reply must not be collected with companion_dialogues off");
        assert_eq!(store.get_companion_state_i64(&companion, "xp").await.unwrap(), 2);
        // Shared state remains untouched; other companions remain untouched.
        assert_eq!(store.get_state_i64("xp").await.unwrap(), 0);
        assert_eq!(store.get_companion_state_i64(&other_companion, "xp").await.unwrap(), 0);

        // Normal conversation turn: reply collected, no XP.
        collector
            .handle(&WebSocketMessage::new(
                "message.stream",
                serde_json::json!({"conversation_id": work_conversation, "msg_id": "m2", "type": "content", "data": "修好了"}),
            ))
            .await;
        collector
            .handle(&WebSocketMessage::new(
                "turn.completed",
                serde_json::json!({"conversation_id": work_conversation}),
            ))
            .await;
        let (events, _) = read_events_since(dir.path(), 0, 10).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(store.get_companion_state_i64(&companion, "xp").await.unwrap(), 2);
    }

    #[tokio::test]
    async fn companion_dialogues_collects_companion_dialogue_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::CompanionStore::open_memory().await.unwrap();
        let companion_conversation = conversation_fixture(1);
        let work_conversation = conversation_fixture(2);
        let companion = companion_fixture(1);
        store.insert_companion_thread(&companion_conversation, &companion, "聊天").await.unwrap();
        // Default config: every work-event source OFF, companion_dialogues ON.
        let config = SharedCompanionConfig::default();
        assert!(config.collect.companion_dialogues);
        let mut collector = Collector::new(
            dir.path().to_path_buf(),
            Arc::new(RwLock::new(config)),
            store.clone(),
        );

        // Owner speaking to the companion → companion.user_message.
        collector
            .handle(&WebSocketMessage::new(
                "message.userCreated",
                serde_json::json!({"conversation_id": companion_conversation, "content": "记得我喜欢深色主题"}),
            ))
            .await;
        // Companion replying → buffered, flushed as companion.reply on turn.completed.
        collector
            .handle(&WebSocketMessage::new(
                "message.stream",
                serde_json::json!({"conversation_id": companion_conversation, "msg_id": "m1", "type": "content", "data": "记住啦！"}),
            ))
            .await;
        collector
            .handle(&WebSocketMessage::new(
                "turn.completed",
                serde_json::json!({"conversation_id": companion_conversation}),
            ))
            .await;

        let (events, _) = read_events_since(dir.path(), 0, 10).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].source, "companion_dialogues");
        assert_eq!(events[0].name, "companion.user_message");
        assert_eq!(events[0].data["companion_id"], companion);
        assert_eq!(events[0].data["content"], "记得我喜欢深色主题");
        assert_eq!(events[1].name, "companion.reply");
        assert_eq!(events[1].data["content"], "记住啦！");
        assert_eq!(events[1].data["companion_id"], companion);
        // +2 turn XP preserved.
        assert_eq!(store.get_companion_state_i64(&companion, "xp").await.unwrap(), 2);

        // Normal conversation messages stay un-collected (work sources off).
        collector
            .handle(&WebSocketMessage::new(
                "message.userCreated",
                serde_json::json!({"conversation_id": work_conversation, "content": "帮我修 bug"}),
            ))
            .await;
        let (events, _) = read_events_since(dir.path(), 0, 10).unwrap();
        assert_eq!(events.len(), 2);
    }

    #[tokio::test]
    async fn payload_marker_identifies_companion_without_registry() {
        // Channel Agent sessions never register in companion_threads —
        // the wire markers (companion / companion_id) must be enough.
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::CompanionStore::open_memory().await.unwrap();
        let conversation = conversation_fixture(3);
        let companion = companion_fixture(3);
        let mut config = SharedCompanionConfig::default();
        config.collect.chat_user_messages = true;
        let mut collector = Collector::new(
            dir.path().to_path_buf(),
            Arc::new(RwLock::new(config)),
            store.clone(),
        );

        collector
            .handle(&WebSocketMessage::new(
                "message.userCreated",
                serde_json::json!({
                    "conversation_id": conversation,
                    "content": "今晚提醒我备份",
                    "companion": true,
                    "companion_id": companion,
                }),
            ))
            .await;
        collector
            .handle(&WebSocketMessage::new(
                "message.stream",
                serde_json::json!({"conversation_id": conversation, "msg_id": "m1", "type": "content", "data": "好～到点喊你",
                                   "companion": true, "companion_id": companion}),
            ))
            .await;
        collector
            .handle(&WebSocketMessage::new(
                "turn.completed",
                serde_json::json!({"conversation_id": conversation, "companion": true, "companion_id": companion}),
            ))
            .await;

        let (events, _) = read_events_since(dir.path(), 0, 10).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].name, "companion.user_message");
        assert_eq!(events[0].data["companion_id"], companion);
        assert_eq!(events[1].name, "companion.reply");
        // XP credited via the wire companion_id, not the (empty) registry chain.
        assert_eq!(store.get_companion_state_i64(&companion, "xp").await.unwrap(), 2);
        // And the message never leaked into the generic work-chat source.
        assert!(events.iter().all(|e| e.source == "companion_dialogues"));
    }

    #[tokio::test]
    async fn origin_marked_messages_are_never_collected_as_owner_speech() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::CompanionStore::open_memory().await.unwrap();
        let companion_conversation = conversation_fixture(1);
        let work_conversation = conversation_fixture(2);
        let companion = companion_fixture(1);
        store.insert_companion_thread(&companion_conversation, &companion, "聊天").await.unwrap();
        let mut config = SharedCompanionConfig::default();
        config.collect.chat_user_messages = true;
        let mut collector = Collector::new(
            dir.path().to_path_buf(),
            Arc::new(RwLock::new(config)),
            store.clone(),
        );

        // Gateway-injected message into a work conversation (origin=companion):
        // skipped even with chat_user_messages on.
        collector
            .handle(&WebSocketMessage::new(
                "message.userCreated",
                serde_json::json!({"conversation_id": work_conversation, "content": "请创建报表任务", "origin": "companion"}),
            ))
            .await;
        // Cron kickoff into a companion conversation: skipped for
        // companion_dialogues too.
        collector
            .handle(&WebSocketMessage::new(
                "message.userCreated",
                serde_json::json!({"conversation_id": companion_conversation, "content": "定时唤醒", "origin": "cron"}),
            ))
            .await;
        let (events, _) = read_events_since(dir.path(), 0, 10).unwrap();
        assert!(events.is_empty(), "origin-marked messages must never be collected");

        // origin: null / absent → real owner speech, collected as before.
        collector
            .handle(&WebSocketMessage::new(
                "message.userCreated",
                serde_json::json!({"conversation_id": work_conversation, "content": "我自己说的", "origin": null}),
            ))
            .await;
        let (events, _) = read_events_since(dir.path(), 0, 10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data["content"], "我自己说的");
    }

    #[tokio::test]
    async fn origin_marked_turn_replies_are_never_collected() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::CompanionStore::open_memory().await.unwrap();
        let work_conversation = conversation_fixture(2);
        let cron_conversation = conversation_fixture(4);
        let mut config = SharedCompanionConfig::default();
        config.collect.chat_assistant_replies = true;
        let mut collector = Collector::new(
            dir.path().to_path_buf(),
            Arc::new(RwLock::new(config)),
            store.clone(),
        );

        // Companion-driven work turn: every stream fragment carries origin="companion"
        // (stamped by the relay) — nothing may be buffered or flushed, even
        // with chat_assistant_replies on.
        collector
            .handle(&WebSocketMessage::new(
                "message.stream",
                serde_json::json!({"conversation_id": work_conversation, "msg_id": "m1", "type": "content",
                                   "data": "报表任务已创建", "origin": "companion"}),
            ))
            .await;
        collector
            .handle(&WebSocketMessage::new(
                "turn.completed",
                serde_json::json!({"conversation_id": work_conversation, "origin": "companion"}),
            ))
            .await;
        let (events, _) = read_events_since(dir.path(), 0, 10).unwrap();
        assert!(events.is_empty(), "companion-driven replies must not become assistant.reply");

        // Defense in depth: chunks already buffered (e.g. before a Lagged
        // skip) are dropped the moment an origin-marked fragment or
        // turn.completed for the conversation arrives.
        collector
            .handle(&WebSocketMessage::new(
                "message.stream",
                serde_json::json!({"conversation_id": cron_conversation, "msg_id": "m2", "type": "content", "data": "先囤一点"}),
            ))
            .await;
        collector
            .handle(&WebSocketMessage::new(
                "turn.completed",
                serde_json::json!({"conversation_id": cron_conversation, "origin": "cron"}),
            ))
            .await;
        let (events, _) = read_events_since(dir.path(), 0, 10).unwrap();
        assert!(events.is_empty(), "origin-marked turn must drop buffered replies unflushed");
        assert!(collector.reply_buffers.is_empty());

        // origin: null → owner-driven turn, collected as before.
        collector
            .handle(&WebSocketMessage::new(
                "message.stream",
                serde_json::json!({"conversation_id": work_conversation, "msg_id": "m3", "type": "content",
                                   "data": "修好了", "origin": null}),
            ))
            .await;
        collector
            .handle(&WebSocketMessage::new(
                "turn.completed",
                serde_json::json!({"conversation_id": work_conversation, "origin": null}),
            ))
            .await;
        let (events, _) = read_events_since(dir.path(), 0, 10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "assistant.reply");
        assert_eq!(events[0].data["content"], "修好了");
    }

    #[tokio::test]
    async fn replace_override_rewrites_buffer_even_when_hidden() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::CompanionStore::open_memory().await.unwrap();
        let conversation_a = conversation_fixture(11);
        let conversation_b = conversation_fixture(12);
        let mut config = SharedCompanionConfig::default();
        config.collect.chat_assistant_replies = true;
        let mut collector = Collector::new(
            dir.path().to_path_buf(),
            Arc::new(RwLock::new(config)),
            store.clone(),
        );

        // Visible override: the cleaned text supersedes the raw buffer.
        collector
            .handle(&WebSocketMessage::new(
                "message.stream",
                serde_json::json!({"conversation_id": conversation_a, "msg_id": "m1", "type": "content",
                                   "data": "好的 [CRON_CREATE {\"name\":\"备份\"}]"}),
            ))
            .await;
        collector
            .handle(&WebSocketMessage::new(
                "message.stream",
                serde_json::json!({"conversation_id": conversation_a, "msg_id": "m1", "type": "content",
                                   "data": {"content": "好的"}, "hidden": false, "replace": true}),
            ))
            .await;
        collector
            .handle(&WebSocketMessage::new(
                "turn.completed",
                serde_json::json!({"conversation_id": conversation_a}),
            ))
            .await;
        let (events, _) = read_events_since(dir.path(), 0, 10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data["content"], "好的", "collected reply must be the cleaned text");

        // Hidden override (middleware emptied the whole reply): the raw
        // directive text the user never saw must NOT be persisted.
        collector
            .handle(&WebSocketMessage::new(
                "message.stream",
                serde_json::json!({"conversation_id": conversation_b, "msg_id": "m2", "type": "content",
                                   "data": "[CRON_DELETE job_1]"}),
            ))
            .await;
        collector
            .handle(&WebSocketMessage::new(
                "message.stream",
                serde_json::json!({"conversation_id": conversation_b, "msg_id": "m2", "type": "content",
                                   "data": {"content": ""}, "hidden": true, "replace": true}),
            ))
            .await;
        collector
            .handle(&WebSocketMessage::new(
                "turn.completed",
                serde_json::json!({"conversation_id": conversation_b}),
            ))
            .await;
        let (events, _) = read_events_since(dir.path(), 0, 10).unwrap();
        assert_eq!(events.len(), 1, "hidden replace must clear the buffer, not flush the original");
    }

    #[tokio::test]
    async fn reply_buffers_enforce_global_entry_cap() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::CompanionStore::open_memory().await.unwrap();
        let mut config = SharedCompanionConfig::default();
        config.collect.chat_assistant_replies = true;
        let mut collector = Collector::new(
            dir.path().to_path_buf(),
            Arc::new(RwLock::new(config)),
            store.clone(),
        );

        // 10 over the cap, no turn.completed in between (orphan scenario).
        let total = MAX_REPLY_BUFFERS + 10;
        for i in 0..total {
            let conversation_id = conversation_fixture(i as u64 + 100);
            collector
                .handle(&WebSocketMessage::new(
                    "message.stream",
                    serde_json::json!({"conversation_id": conversation_id, "msg_id": "m", "type": "content",
                                       "data": format!("回复 {i}")}),
                ))
                .await;
        }
        assert_eq!(collector.reply_buffers.len(), MAX_REPLY_BUFFERS);
        // Oldest evicted, newest retained.
        assert!(
            !collector
                .reply_buffers
                .contains_key(&(conversation_fixture(100), "m".to_owned()))
        );
        assert!(
            collector
                .reply_buffers
                .contains_key(&(conversation_fixture(total as u64 + 99), "m".to_owned()))
        );
        // A surviving buffer still flushes normally.
        collector
            .handle(&WebSocketMessage::new(
                "turn.completed",
                serde_json::json!({"conversation_id": conversation_fixture(total as u64 + 99)}),
            ))
            .await;
        let (events, _) = read_events_since(dir.path(), 0, 200).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data["content"], format!("回复 {}", total - 1));
        assert_eq!(collector.reply_buffers.len(), MAX_REPLY_BUFFERS - 1);
    }

    #[tokio::test]
    async fn requirement_created_by_agent_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::CompanionStore::open_memory().await.unwrap();
        let mut config = SharedCompanionConfig::default();
        config.collect.requirements = true;
        let mut collector = Collector::new(
            dir.path().to_path_buf(),
            Arc::new(RwLock::new(config)),
            store.clone(),
        );

        collector
            .handle(&WebSocketMessage::new(
                "requirement.created",
                serde_json::json!({"title": "伙伴自建需求", "content": "agent 自动创建", "tag": "auto", "created_by": "agent"}),
            ))
            .await;
        let (events, _) = read_events_since(dir.path(), 0, 10).unwrap();
        assert!(events.is_empty(), "agent-created requirements must not feed the learner");

        collector
            .handle(&WebSocketMessage::new(
                "requirement.created",
                serde_json::json!({"title": "主人提的需求", "content": "做个导出功能", "tag": "default", "created_by": "user"}),
            ))
            .await;
        let (events, _) = read_events_since(dir.path(), 0, 10).unwrap();
        assert_eq!(events.len(), 1);
        // The collected record reads the real Requirement fields
        // (content/tag), not the phantom description/tags keys.
        assert_eq!(events[0].data["content"], "做个导出功能");
        assert_eq!(events[0].data["tag"], "default");
        assert_eq!(events[0].data["created_by"], "user");
    }

    #[tokio::test]
    async fn unregistered_companion_turn_falls_back_to_default_companion() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::CompanionStore::open_memory().await.unwrap();
        let conversation = conversation_fixture(5);
        let default_companion = companion_fixture(5);
        let mut config = SharedCompanionConfig::default();
        config.default_companion_id = Some(default_companion.clone());
        let config = Arc::new(RwLock::new(config));
        let mut collector = Collector::new(dir.path().to_path_buf(), config.clone(), store.clone());

        collector
            .handle(&WebSocketMessage::new(
                "turn.completed",
                serde_json::json!({"conversation_id": conversation, "companion": true}),
            ))
            .await;
        assert_eq!(store.get_companion_state_i64(&default_companion, "xp").await.unwrap(), 2);

        // No default companion either: the XP is skipped entirely.
        config.write().await.default_companion_id = None;
        collector
            .handle(&WebSocketMessage::new(
                "turn.completed",
                serde_json::json!({"conversation_id": conversation, "companion": true}),
            ))
            .await;
        assert_eq!(store.get_companion_state_i64(&default_companion, "xp").await.unwrap(), 2);
        assert_eq!(store.get_state_i64("xp").await.unwrap(), 0);
    }

    #[test]
    fn append_read_stats_clear_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..5 {
            append_event(
                dir.path(),
                &CollectedEvent {
                    event_id: nomifun_common::generate_id(),
                    ts: now_ms() + i,
                    source: "chat_user_messages".into(),
                    name: "message.userCreated".into(),
                    data: serde_json::json!({"content": format!("hello {i}")}),
                },
            )
            .unwrap();
        }
        let (events, truncated) = read_events_since(dir.path(), 0, 10).unwrap();
        assert_eq!(events.len(), 5);
        assert!(!truncated);

        let (limited, truncated) = read_events_since(dir.path(), 0, 3).unwrap();
        assert_eq!(limited.len(), 3);
        assert!(truncated);

        let cursor = events[2].ts;
        let (after, _) = read_events_since(dir.path(), cursor, 10).unwrap();
        assert_eq!(after.len(), 2);

        let stats = event_stats(dir.path()).unwrap();
        assert_eq!(stats.get("chat_user_messages").unwrap().1, 5);

        clear_events(dir.path()).unwrap();
        let (none, _) = read_events_since(dir.path(), 0, 10).unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn event_wire_contract_uses_event_id_and_rejects_generic_id() {
        let event_id = nomifun_common::generate_id();
        let event = CollectedEvent {
            event_id: event_id.clone(),
            ts: 1,
            source: "tool_calls".into(),
            name: "tool.call".into(),
            data: serde_json::json!({}),
        };
        let wire = serde_json::to_value(&event).unwrap();
        assert_eq!(wire["event_id"], event_id);
        assert!(wire.get("id").is_none());

        let legacy = serde_json::json!({
            "id": nomifun_common::generate_id(),
            "ts": 1,
            "source": "tool_calls",
            "name": "tool.call",
            "data": {}
        });
        assert!(serde_json::from_value::<CollectedEvent>(legacy).is_err());
    }

    #[test]
    fn event_wire_contract_requires_bare_uuidv7_event_id() {
        for event_id in [
            "event_0190f5fe-7c00-7a00-8abc-000000000001",
            "0190f5fe-7c00-4a00-8abc-000000000001",
        ] {
            let wire = serde_json::json!({
                "event_id": event_id,
                "ts": 1,
                "source": "tool_calls",
                "name": "tool.call",
                "data": {}
            });
            assert!(
                serde_json::from_value::<CollectedEvent>(wire).is_err(),
                "{event_id:?} must not be accepted as a collected event id"
            );
        }
    }

    #[test]
    fn event_file_rejects_legacy_generic_id_record() {
        let dir = tempfile::tempdir().unwrap();
        let events = events_dir(dir.path());
        std::fs::create_dir_all(&events).unwrap();
        std::fs::write(
            events.join("20260722.jsonl"),
            format!(
                "{{\"id\":\"{}\",\"ts\":1,\"source\":\"tool_calls\",\"name\":\"tool.call\",\"data\":{{}}}}\n",
                nomifun_common::generate_id()
            ),
        )
        .unwrap();

        let error = read_recent_events(dir.path(), 10).unwrap_err();
        assert!(error.to_string().contains("corrupt at line 1"), "{error}");
    }

    #[test]
    fn truncation_appends_ellipsis() {
        let long = "啊".repeat(3000);
        let t = truncate_chars(&long, 2000);
        assert!(t.ends_with('…'));
        assert_eq!(t.chars().count(), 2001);
    }

    #[test]
    fn read_recent_events_returns_newest_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let base = now_ms();
        for i in 0..7i64 {
            append_event(
                dir.path(),
                &CollectedEvent {
                    event_id: nomifun_common::generate_id(),
                    ts: base + i,
                    source: "cron_runs".into(),
                    name: "cron.job-executed".into(),
                    data: serde_json::json!({"n": i}),
                },
            )
            .unwrap();
        }
        let recent = read_recent_events(dir.path(), 3).unwrap();
        assert_eq!(recent.len(), 3);
        // Newest 3, chronological order.
        assert_eq!(recent[0].data["n"], 4);
        assert_eq!(recent[2].data["n"], 6);
        assert!(read_recent_events(dir.path(), 100).unwrap().len() == 7);
        assert!(read_recent_events(&dir.path().join("nope"), 5).unwrap().is_empty());
    }

    #[test]
    fn truncation_never_splits_same_millisecond_group() {
        let dir = tempfile::tempdir().unwrap();
        let base = now_ms();
        // 5 events: two distinct, then three sharing one millisecond.
        for (i, ts) in [base, base + 1, base + 2, base + 2, base + 2].iter().enumerate() {
            append_event(
                dir.path(),
                &CollectedEvent {
                    event_id: nomifun_common::generate_id(),
                    ts: *ts,
                    source: "chat_user_messages".into(),
                    name: "message.userCreated".into(),
                    data: serde_json::json!({"content": format!("m{i}")}),
                },
            )
            .unwrap();
        }
        // limit=3 lands inside the base+2 group: the group must be kept whole,
        // otherwise advancing the cursor to base+2 would skip the rest forever.
        let (events, truncated) = read_events_since(dir.path(), 0, 3).unwrap();
        assert_eq!(events.len(), 5);
        assert!(!truncated);
        // limit=2 cuts cleanly between base+1 and base+2.
        let (events, truncated) = read_events_since(dir.path(), 0, 2).unwrap();
        assert_eq!(events.len(), 2);
        assert!(truncated);
        let (rest, _) = read_events_since(dir.path(), events.last().unwrap().ts, 10).unwrap();
        assert_eq!(rest.len(), 3);
    }
}
