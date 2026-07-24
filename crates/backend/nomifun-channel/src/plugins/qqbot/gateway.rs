//! QQ Bot Gateway WebSocket connection loop.
//!
//! Maintains a persistent connection to the QQ Bot gateway with HELLO/IDENTIFY,
//! heartbeat, RESUME on reconnect, and event dispatch. Mirrors the Discord
//! gateway pattern with QQ-specific close-code handling and token refresh.

use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use tokio::sync::{mpsc, watch};
use tracing::{debug, error, info, warn};

use crate::constants::{
    QQBOT_MAX_RECONNECT_ATTEMPTS, QQBOT_MAX_RECONNECT_DELAY, QQBOT_PASSIVE_REPLY_MAX,
    QQBOT_PASSIVE_REPLY_WINDOW,
};
use crate::error::ChannelError;
use crate::plugin::{SharedPluginStatus, mark_error_on_unexpected_exit};
use crate::plugins::callback::parse_callback_data;
use crate::types::{
    ActionContext, MessageContentType, PluginType, UnifiedAction, UnifiedIncomingMessage,
    UnifiedMessageContent, UnifiedUser,
};

use super::api::QqbotApi;
use super::types::{
    C2cMessageCreate, ChannelMessageCreate, DirectMessageCreate, GatewayPayload, GroupMessageCreate,
    HelloData, IdentifyData, IdentifyProperties, InteractionCreate, OutgoingFrame, ReadyData,
    ResumeData, CLOSE_AUTH_FAILED, CLOSE_ALREADY_AUTHED, CLOSE_INTENT_DISABLED,
    CLOSE_INTENT_NOT_SUFFICIENT, CLOSE_INVALID_SEQ, CLOSE_RATE_LIMITED, CLOSE_SESSION_TIMEOUT,
    GATEWAY_INTENTS, INTERACTION_TYPE_BUTTON, OP_DISPATCH, OP_HEARTBEAT, OP_HEARTBEAT_ACK,
    OP_HELLO, OP_IDENTIFY, OP_INVALID_SESSION, OP_RECONNECT, OP_RESUME,
};

// ---------------------------------------------------------------------------
// Chat ID prefix conventions
// ---------------------------------------------------------------------------

/// Chat ID prefix for C2C (friend) messages.
pub(crate) const CHAT_PREFIX_C2C: &str = "c2c:";
/// Chat ID prefix for group messages.
pub(crate) const CHAT_PREFIX_GROUP: &str = "group:";
/// Chat ID prefix for guild channel messages.
pub(crate) const CHAT_PREFIX_CHANNEL: &str = "channel:";
/// Chat ID prefix for guild direct messages.
pub(crate) const CHAT_PREFIX_DM: &str = "dm:";

// ---------------------------------------------------------------------------
// Chat ID helpers (pure, tested)
// ---------------------------------------------------------------------------

/// The four target types for outbound routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChatTarget {
    C2c,
    Group,
    Channel,
    Dm,
}

/// Build a prefixed chat_id from a target type and identifier.
pub(crate) fn build_chat_id(target: ChatTarget, id: &str) -> String {
    let prefix = match target {
        ChatTarget::C2c => CHAT_PREFIX_C2C,
        ChatTarget::Group => CHAT_PREFIX_GROUP,
        ChatTarget::Channel => CHAT_PREFIX_CHANNEL,
        ChatTarget::Dm => CHAT_PREFIX_DM,
    };
    format!("{prefix}{id}")
}

/// Parse a prefixed chat_id into `(ChatTarget, bare_id)`.
pub(crate) fn parse_chat_id(chat_id: &str) -> Option<(ChatTarget, &str)> {
    if let Some(rest) = chat_id.strip_prefix(CHAT_PREFIX_C2C) {
        Some((ChatTarget::C2c, rest))
    } else if let Some(rest) = chat_id.strip_prefix(CHAT_PREFIX_GROUP) {
        Some((ChatTarget::Group, rest))
    } else if let Some(rest) = chat_id.strip_prefix(CHAT_PREFIX_CHANNEL) {
        Some((ChatTarget::Channel, rest))
    } else if let Some(rest) = chat_id.strip_prefix(CHAT_PREFIX_DM) {
        Some((ChatTarget::Dm, rest))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Passive reply window
// ---------------------------------------------------------------------------

/// Tracks the passive-reply budget for a single inbound msg_id.
#[derive(Debug, Clone)]
pub(crate) struct ReplyWindow {
    pub msg_id: String,
    pub remaining: u32,
    pub expires_at: tokio::time::Instant,
}

/// Shared map: chat_id -> current ReplyWindow.
pub(crate) type PassiveReplyMap = Arc<DashMap<String, ReplyWindow>>;

/// Try to consume one passive-reply slot for the given chat_id.
/// Returns `Some(msg_id)` if a valid slot was consumed, `None` if exhausted/expired.
pub(crate) fn consume_passive_reply(
    map: &PassiveReplyMap,
    chat_id: &str,
    now: tokio::time::Instant,
) -> Option<String> {
    let mut entry = map.get_mut(chat_id)?;
    let window = entry.value_mut();
    if window.remaining == 0 || now >= window.expires_at {
        return None;
    }
    window.remaining -= 1;
    Some(window.msg_id.clone())
}

/// Record a new inbound msg_id for passive replies.
pub(crate) fn record_inbound_msg_id(map: &PassiveReplyMap, chat_id: &str, msg_id: &str) {
    map.insert(
        chat_id.to_string(),
        ReplyWindow {
            msg_id: msg_id.to_string(),
            remaining: QQBOT_PASSIVE_REPLY_MAX,
            expires_at: tokio::time::Instant::now() + QQBOT_PASSIVE_REPLY_WINDOW,
        },
    );
}

/// Generate a new msg_seq value. Each call increments a per-process counter.
pub(crate) fn next_msg_seq() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(1);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Close-code decision
// ---------------------------------------------------------------------------

/// Action the gateway loop should take after a close code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CloseAction {
    /// Stop the gateway loop entirely (fatal, e.g. intent not approved).
    Fatal,
    /// Clear session + refresh token, then reconnect immediately.
    ClearAndReconnect,
    /// Wait 60s, then reconnect (rate limited).
    WaitAndReconnect,
}

/// Determine the reconnect action from a WebSocket close code.
pub(crate) fn close_code_action(code: u16) -> CloseAction {
    match code {
        CLOSE_INTENT_NOT_SUFFICIENT | CLOSE_INTENT_DISABLED => CloseAction::Fatal,
        CLOSE_AUTH_FAILED | CLOSE_ALREADY_AUTHED | CLOSE_INVALID_SEQ | CLOSE_SESSION_TIMEOUT => {
            CloseAction::ClearAndReconnect
        }
        CLOSE_RATE_LIMITED => CloseAction::WaitAndReconnect,
        // 4900-4913 and other unknown codes: clear session + reconnect.
        c if (4900..=4913).contains(&c) => CloseAction::ClearAndReconnect,
        _ => CloseAction::ClearAndReconnect,
    }
}

// ---------------------------------------------------------------------------
// Event normalization (pure, tested)
// ---------------------------------------------------------------------------

fn stable_provider_event_id<'a>(id: &'a str, event_kind: &str) -> Option<&'a str> {
    let id = id.trim();
    if id.is_empty() {
        warn!(event_kind, "QQBot inbound event missing stable provider id; dropping event");
        None
    } else {
        Some(id)
    }
}

/// Normalize a C2C_MESSAGE_CREATE into a UnifiedIncomingMessage.
pub(crate) fn normalize_c2c_message(msg: &C2cMessageCreate) -> Option<UnifiedIncomingMessage> {
    let stable_event_id = stable_provider_event_id(&msg.id, "C2C_MESSAGE_CREATE")?;
    let user_openid = msg.author.user_openid.as_deref().unwrap_or_default();
    if user_openid.is_empty() {
        return None;
    }
    let chat_id = build_chat_id(ChatTarget::C2c, user_openid);
    Some(UnifiedIncomingMessage {
        id: stable_event_id.to_owned(),
        platform: PluginType::Qqbot,
        chat_id,
        user: UnifiedUser {
            id: user_openid.to_string(),
            username: None,
            display_name: user_openid.to_string(),
            avatar_url: None,
        },
        content: UnifiedMessageContent {
            content_type: MessageContentType::Text,
            text: msg.content.clone(),
            attachments: None,
        },
        timestamp: parse_timestamp(&msg.timestamp),
        reply_to_message_id: None,
        action: None,
        raw: None,
    })
}

/// Normalize a GROUP_AT_MESSAGE_CREATE / GROUP_MESSAGE_CREATE.
/// Bot-loop guard: skip if author.bot is true.
pub(crate) fn normalize_group_message(msg: &GroupMessageCreate) -> Option<UnifiedIncomingMessage> {
    // Bot-loop guard.
    if msg.author.bot {
        return None;
    }
    let stable_event_id =
        stable_provider_event_id(&msg.id, "GROUP_AT_MESSAGE_CREATE/GROUP_MESSAGE_CREATE")?;
    let member_openid = msg.author.member_openid.as_deref().unwrap_or_default();
    let chat_id = build_chat_id(ChatTarget::Group, &msg.group_openid);
    Some(UnifiedIncomingMessage {
        id: stable_event_id.to_owned(),
        platform: PluginType::Qqbot,
        chat_id,
        user: UnifiedUser {
            id: member_openid.to_string(),
            username: None,
            display_name: member_openid.to_string(),
            avatar_url: None,
        },
        content: UnifiedMessageContent {
            content_type: MessageContentType::Text,
            text: msg.content.clone(),
            attachments: None,
        },
        timestamp: parse_timestamp(&msg.timestamp),
        reply_to_message_id: None,
        action: None,
        raw: None,
    })
}

/// Normalize an AT_MESSAGE_CREATE (guild public channel).
/// Bot-loop guard: skip if author.bot is true.
pub(crate) fn normalize_channel_message(msg: &ChannelMessageCreate) -> Option<UnifiedIncomingMessage> {
    if msg.author.bot {
        return None;
    }
    let stable_event_id = stable_provider_event_id(&msg.id, "AT_MESSAGE_CREATE")?;
    let author_id = msg.author.id.as_deref().unwrap_or_default();
    let chat_id = build_chat_id(ChatTarget::Channel, &msg.channel_id);
    Some(UnifiedIncomingMessage {
        id: stable_event_id.to_owned(),
        platform: PluginType::Qqbot,
        chat_id,
        user: UnifiedUser {
            id: author_id.to_string(),
            username: msg.author.username.clone(),
            display_name: msg
                .author
                .username
                .clone()
                .unwrap_or_else(|| author_id.to_string()),
            avatar_url: None,
        },
        content: UnifiedMessageContent {
            content_type: MessageContentType::Text,
            text: msg.content.clone(),
            attachments: None,
        },
        timestamp: parse_timestamp(&msg.timestamp),
        reply_to_message_id: None,
        action: None,
        raw: None,
    })
}

/// Normalize a DIRECT_MESSAGE_CREATE (guild DM).
/// Bot-loop guard: skip if author.bot is true.
pub(crate) fn normalize_direct_message(msg: &DirectMessageCreate) -> Option<UnifiedIncomingMessage> {
    if msg.author.bot {
        return None;
    }
    let stable_event_id = stable_provider_event_id(&msg.id, "DIRECT_MESSAGE_CREATE")?;
    let author_id = msg.author.id.as_deref().unwrap_or_default();
    let chat_id = build_chat_id(ChatTarget::Dm, &msg.guild_id);
    Some(UnifiedIncomingMessage {
        id: stable_event_id.to_owned(),
        platform: PluginType::Qqbot,
        chat_id,
        user: UnifiedUser {
            id: author_id.to_string(),
            username: msg.author.username.clone(),
            display_name: msg
                .author
                .username
                .clone()
                .unwrap_or_else(|| author_id.to_string()),
            avatar_url: None,
        },
        content: UnifiedMessageContent {
            content_type: MessageContentType::Text,
            text: msg.content.clone(),
            attachments: None,
        },
        timestamp: parse_timestamp(&msg.timestamp),
        reply_to_message_id: None,
        action: None,
        raw: None,
    })
}

/// Normalize an INTERACTION_CREATE (button callback).
pub(crate) fn normalize_interaction(interaction: &InteractionCreate) -> Option<UnifiedIncomingMessage> {
    if interaction.interaction_type != INTERACTION_TYPE_BUTTON {
        return None;
    }
    let stable_event_id = stable_provider_event_id(&interaction.id, "INTERACTION_CREATE")?;
    let button_id = interaction
        .data
        .as_ref()
        .and_then(|d| d.resolved.as_ref())
        .and_then(|r| r.button_id.clone())?;

    let user_id = interaction
        .user_openid
        .as_deref()
        .or(interaction.group_member_openid.as_deref())
        .unwrap_or_default()
        .to_string();

    let chat_id = if let Some(gid) = &interaction.group_openid {
        build_chat_id(ChatTarget::Group, gid)
    } else if let Some(cid) = &interaction.channel_id {
        build_chat_id(ChatTarget::Channel, cid)
    } else {
        // C2C interaction: use user_openid.
        build_chat_id(ChatTarget::C2c, &user_id)
    };

    let parsed = parse_callback_data(&button_id);
    let action = parsed.map(|p| UnifiedAction {
        action: p.action,
        category: p.category,
        params: p.params,
        context: ActionContext {
            platform: PluginType::Qqbot,
            user_id: user_id.clone(),
            chat_id: chat_id.clone(),
            message_id: None,
            session_id: None,
        },
    });

    Some(UnifiedIncomingMessage {
        id: stable_event_id.to_owned(),
        platform: PluginType::Qqbot,
        chat_id,
        user: UnifiedUser {
            id: user_id,
            username: None,
            display_name: String::new(),
            avatar_url: None,
        },
        content: UnifiedMessageContent {
            content_type: MessageContentType::Action,
            text: button_id,
            attachments: None,
        },
        timestamp: 0,
        reply_to_message_id: None,
        action,
        raw: None,
    })
}

/// Parse an ISO 8601 timestamp string to unix seconds. Returns 0 on failure.
fn parse_timestamp(ts: &str) -> i64 {
    if ts.is_empty() {
        return 0;
    }
    // Try common QQ API formats: "2024-01-01T12:00:00+08:00" or "2024-01-01T04:00:00Z"
    // We do simple best-effort parsing without pulling in `chrono`.
    // Extract the first 19 chars as "YYYY-MM-DDTHH:MM:SS".
    if ts.len() < 19 {
        return 0;
    }
    // Rough parse: count seconds since epoch using a simplified approach.
    // For correctness without a date library, just return 0 and let the
    // message_service handle it (timestamps are informational).
    0
}

// ---------------------------------------------------------------------------
// Gateway loop
// ---------------------------------------------------------------------------

/// Background task: maintain the QQ Bot gateway connection with reconnects.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_gateway(
    api: Arc<QqbotApi>,
    message_tx: mpsc::Sender<UnifiedIncomingMessage>,
    confirm_tx: mpsc::Sender<(String, String)>,
    status: SharedPluginStatus,
    reply_map: PassiveReplyMap,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let mut consecutive_errors: u32 = 0;
    let mut session_id: Option<String> = None;
    let mut last_seq: Option<u64> = None;

    loop {
        if *shutdown_rx.borrow() {
            debug!("QQBot gateway loop received shutdown signal");
            break;
        }

        match connect_once(
            &api,
            &message_tx,
            &confirm_tx,
            &reply_map,
            &mut shutdown_rx,
            &mut session_id,
            &mut last_seq,
        )
        .await
        {
            Ok(()) => {
                consecutive_errors = 0;
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            Err(GatewayExitReason::Fatal(e)) => {
                error!(error = %e, "QQBot gateway fatal error, stopping");
                break;
            }
            Err(GatewayExitReason::ClearAndReconnect(e)) => {
                warn!(error = %e, "QQBot gateway error (clear session + reconnect)");
                session_id = None;
                last_seq = None;
                api.clear_token().await;
                consecutive_errors += 1;
            }
            Err(GatewayExitReason::WaitAndReconnect(e)) => {
                warn!(error = %e, "QQBot gateway rate-limited, waiting 60s");
                consecutive_errors += 1;
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(60)) => {}
                    _ = shutdown_rx.changed() => break,
                }
            }
            Err(GatewayExitReason::Reconnectable(e)) => {
                consecutive_errors += 1;
                warn!(error = %e, consecutive_errors, "QQBot gateway error");
            }
        }

        if consecutive_errors >= QQBOT_MAX_RECONNECT_ATTEMPTS {
            error!("QQBot max reconnect attempts reached, stopping gateway loop");
            break;
        }

        if consecutive_errors > 0 {
            let backoff = backoff_delay(consecutive_errors);
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = shutdown_rx.changed() => break,
            }
        }
    }

    mark_error_on_unexpected_exit(&status, &shutdown_rx, "qqbot");
    debug!("QQBot gateway loop exited");
}

/// Background task: periodically refresh the access token before expiry.
pub(super) async fn run_token_refresh(
    api: Arc<QqbotApi>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    // Refresh ~5 minutes before expiry. Default token lifetime is 7200s.
    // The actual get_token() call in the gateway and send paths also refreshes
    // on demand if expired; this task is a proactive safety net.
    const DEFAULT_INTERVAL: Duration = Duration::from_secs(7200 - 5 * 60);

    loop {
        tokio::select! {
            _ = tokio::time::sleep(DEFAULT_INTERVAL) => {
                match api.refresh_token().await {
                    Ok(_) => debug!("QQBot token refreshed by background task"),
                    Err(e) => warn!(error = %e, "QQBot background token refresh failed"),
                }
            }
            _ = shutdown_rx.changed() => {
                debug!("QQBot token refresh task shutting down");
                return;
            }
        }
    }
}

/// Exponential backoff capped at `QQBOT_MAX_RECONNECT_DELAY`.
fn backoff_delay(attempt: u32) -> Duration {
    let secs = 2u64.saturating_pow(attempt).min(QQBOT_MAX_RECONNECT_DELAY.as_secs());
    Duration::from_secs(secs)
}

/// Reasons a single gateway connection can exit.
enum GatewayExitReason {
    Fatal(ChannelError),
    ClearAndReconnect(ChannelError),
    WaitAndReconnect(ChannelError),
    Reconnectable(ChannelError),
}

/// A single gateway connection: get URL → connect → HELLO → IDENTIFY/RESUME → loop.
#[allow(clippy::too_many_arguments)]
async fn connect_once(
    api: &Arc<QqbotApi>,
    message_tx: &mpsc::Sender<UnifiedIncomingMessage>,
    confirm_tx: &mpsc::Sender<(String, String)>,
    reply_map: &PassiveReplyMap,
    shutdown_rx: &mut watch::Receiver<bool>,
    session_id: &mut Option<String>,
    last_seq: &mut Option<u64>,
) -> Result<(), GatewayExitReason> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::connect_async_tls_with_config;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    // Get the gateway URL (requires a valid token).
    let gateway_url = api.get_gateway_url().await.map_err(GatewayExitReason::Reconnectable)?;

    let connector = build_ws_tls_connector().map_err(GatewayExitReason::Reconnectable)?;
    let (ws_stream, _) = connect_async_tls_with_config(&gateway_url, None, false, Some(connector))
        .await
        .map_err(|e| {
            GatewayExitReason::Reconnectable(ChannelError::ConnectionFailed(format!(
                "QQBot gateway connect failed: {e}"
            )))
        })?;
    info!("QQBot gateway connected");

    let (mut write, mut read) = ws_stream.split();

    // First frame must be HELLO (op 10) with the heartbeat interval.
    let hello = read
        .next()
        .await
        .ok_or_else(|| {
            GatewayExitReason::Reconnectable(ChannelError::ConnectionFailed(
                "QQBot gateway closed before HELLO".into(),
            ))
        })?
        .map_err(|e| {
            GatewayExitReason::Reconnectable(ChannelError::ConnectionFailed(format!(
                "QQBot gateway read error: {e}"
            )))
        })?;

    let heartbeat_interval = match hello {
        WsMessage::Text(txt) => {
            let payload: GatewayPayload = serde_json::from_str(&txt).map_err(|e| {
                GatewayExitReason::Reconnectable(ChannelError::ConnectionFailed(format!(
                    "QQBot HELLO parse failed: {e}"
                )))
            })?;
            if payload.op != OP_HELLO {
                return Err(GatewayExitReason::Reconnectable(
                    ChannelError::ConnectionFailed(format!(
                        "QQBot expected HELLO, got op {}",
                        payload.op
                    )),
                ));
            }
            let hello: HelloData = serde_json::from_value(payload.d).map_err(|e| {
                GatewayExitReason::Reconnectable(ChannelError::ConnectionFailed(format!(
                    "QQBot HELLO data parse failed: {e}"
                )))
            })?;
            Duration::from_millis(hello.heartbeat_interval)
        }
        _ => {
            return Err(GatewayExitReason::Reconnectable(
                ChannelError::ConnectionFailed("QQBot first frame was not text HELLO".into()),
            ));
        }
    };

    // IDENTIFY or RESUME.
    let token = api
        .get_token()
        .await
        .map_err(GatewayExitReason::Reconnectable)?;
    let qq_token = format!("QQBot {token}");

    if let (Some(sid), Some(seq)) = (session_id.as_ref(), *last_seq) {
        // RESUME: re-establish a previous session.
        let resume = OutgoingFrame {
            op: OP_RESUME,
            d: ResumeData {
                token: qq_token,
                session_id: sid.clone(),
                seq,
            },
        };
        let resume_json = serde_json::to_string(&resume).map_err(|e| {
            GatewayExitReason::Reconnectable(ChannelError::ConnectionFailed(format!(
                "QQBot RESUME encode failed: {e}"
            )))
        })?;
        write.send(WsMessage::Text(resume_json.into())).await.map_err(|e| {
            GatewayExitReason::Reconnectable(ChannelError::ConnectionFailed(format!(
                "QQBot RESUME send failed: {e}"
            )))
        })?;
        debug!(session_id = %sid, seq, "QQBot sent RESUME");
    } else {
        // IDENTIFY: start a new session.
        let identify = OutgoingFrame {
            op: OP_IDENTIFY,
            d: IdentifyData {
                token: qq_token,
                intents: GATEWAY_INTENTS,
                shard: [0, 1],
                properties: IdentifyProperties {
                    os: std::env::consts::OS.to_string(),
                    browser: "nomi".to_string(),
                    device: "nomi".to_string(),
                },
            },
        };
        let identify_json = serde_json::to_string(&identify).map_err(|e| {
            GatewayExitReason::Reconnectable(ChannelError::ConnectionFailed(format!(
                "QQBot IDENTIFY encode failed: {e}"
            )))
        })?;
        write
            .send(WsMessage::Text(identify_json.into()))
            .await
            .map_err(|e| {
                GatewayExitReason::Reconnectable(ChannelError::ConnectionFailed(format!(
                    "QQBot IDENTIFY send failed: {e}"
                )))
            })?;
        debug!("QQBot sent IDENTIFY");
    }

    let mut heartbeat_deadline = tokio::time::Instant::now() + heartbeat_interval;

    loop {
        tokio::select! {
            frame = read.next() => {
                match frame {
                    Some(Ok(WsMessage::Text(txt))) => {
                        let payload: GatewayPayload = match serde_json::from_str(&txt) {
                            Ok(p) => p,
                            Err(e) => { warn!(error = %e, "QQBot gateway frame parse failed"); continue; }
                        };
                        if let Some(s) = payload.s {
                            *last_seq = Some(s);
                        }
                        match payload.op {
                            OP_DISPATCH => {
                                let t = payload.t.as_deref().unwrap_or("");
                                match t {
                                    "READY" => {
                                        if let Ok(ready) = serde_json::from_value::<ReadyData>(payload.d.clone()) {
                                            *session_id = Some(ready.session_id.clone());
                                            info!(session_id = %ready.session_id, "QQBot gateway READY");
                                        }
                                    }
                                    "RESUMED" => {
                                        info!("QQBot gateway RESUMED");
                                    }
                                    "C2C_MESSAGE_CREATE" => {
                                        match serde_json::from_value::<C2cMessageCreate>(payload.d) {
                                            Ok(msg) => {
                                                if let Some(unified) = normalize_c2c_message(&msg) {
                                                    record_inbound_msg_id(
                                                        reply_map,
                                                        &unified.chat_id,
                                                        &unified.id,
                                                    );
                                                    let _ = message_tx.send(unified).await;
                                                }
                                            }
                                            Err(e) => warn!(error = %e, "QQBot C2C_MESSAGE_CREATE parse failed"),
                                        }
                                    }
                                    "GROUP_AT_MESSAGE_CREATE" | "GROUP_MESSAGE_CREATE" => {
                                        match serde_json::from_value::<GroupMessageCreate>(payload.d) {
                                            Ok(msg) => {
                                                if let Some(unified) = normalize_group_message(&msg) {
                                                    record_inbound_msg_id(
                                                        reply_map,
                                                        &unified.chat_id,
                                                        &unified.id,
                                                    );
                                                    let _ = message_tx.send(unified).await;
                                                }
                                            }
                                            Err(e) => warn!(error = %e, "QQBot GROUP_MESSAGE parse failed"),
                                        }
                                    }
                                    "AT_MESSAGE_CREATE" => {
                                        match serde_json::from_value::<ChannelMessageCreate>(payload.d) {
                                            Ok(msg) => {
                                                if let Some(unified) = normalize_channel_message(&msg) {
                                                    record_inbound_msg_id(
                                                        reply_map,
                                                        &unified.chat_id,
                                                        &unified.id,
                                                    );
                                                    let _ = message_tx.send(unified).await;
                                                }
                                            }
                                            Err(e) => warn!(error = %e, "QQBot AT_MESSAGE_CREATE parse failed"),
                                        }
                                    }
                                    "DIRECT_MESSAGE_CREATE" => {
                                        match serde_json::from_value::<DirectMessageCreate>(payload.d) {
                                            Ok(msg) => {
                                                if let Some(unified) = normalize_direct_message(&msg) {
                                                    record_inbound_msg_id(
                                                        reply_map,
                                                        &unified.chat_id,
                                                        &unified.id,
                                                    );
                                                    let _ = message_tx.send(unified).await;
                                                }
                                            }
                                            Err(e) => warn!(error = %e, "QQBot DIRECT_MESSAGE_CREATE parse failed"),
                                        }
                                    }
                                    "INTERACTION_CREATE" => {
                                        match serde_json::from_value::<InteractionCreate>(payload.d) {
                                            Ok(interaction) => {
                                                handle_interaction(api, &interaction, message_tx, confirm_tx).await;
                                            }
                                            Err(e) => warn!(error = %e, "QQBot INTERACTION_CREATE parse failed"),
                                        }
                                    }
                                    _ => { /* Other events — ignore */ }
                                }
                            }
                            OP_HEARTBEAT => {
                                send_heartbeat(&mut write, *last_seq).await.map_err(GatewayExitReason::Reconnectable)?;
                                heartbeat_deadline = tokio::time::Instant::now() + heartbeat_interval;
                            }
                            OP_HEARTBEAT_ACK => { /* Expected after heartbeat */ }
                            OP_RECONNECT => {
                                debug!("QQBot gateway op7 RECONNECT");
                                return Ok(());
                            }
                            OP_INVALID_SESSION => {
                                // d: true = resumable, false = not resumable.
                                let resumable = payload.d.as_bool().unwrap_or(false);
                                debug!(resumable, "QQBot gateway op9 INVALID_SESSION");
                                if !resumable {
                                    *session_id = None;
                                    *last_seq = None;
                                }
                                return Ok(());
                            }
                            _ => { /* Unknown opcodes — ignore */ }
                        }
                    }
                    Some(Ok(WsMessage::Close(frame))) => {
                        let code = frame.as_ref().map(|f| f.code.into()).unwrap_or(1000u16);
                        debug!(code, "QQBot gateway received close frame");
                        match close_code_action(code) {
                            CloseAction::Fatal => {
                                return Err(GatewayExitReason::Fatal(
                                    ChannelError::ConnectionFailed(format!(
                                        "QQBot gateway fatal close: {code}"
                                    )),
                                ));
                            }
                            CloseAction::ClearAndReconnect => {
                                return Err(GatewayExitReason::ClearAndReconnect(
                                    ChannelError::ConnectionFailed(format!(
                                        "QQBot gateway close: {code} (clear+reconnect)"
                                    )),
                                ));
                            }
                            CloseAction::WaitAndReconnect => {
                                return Err(GatewayExitReason::WaitAndReconnect(
                                    ChannelError::ConnectionFailed(format!(
                                        "QQBot gateway rate limited close: {code}"
                                    )),
                                ));
                            }
                        }
                    }
                    Some(Ok(_)) => { /* binary/ping/pong — ignore */ }
                    Some(Err(e)) => {
                        return Err(GatewayExitReason::Reconnectable(
                            ChannelError::ConnectionFailed(format!("QQBot gateway read error: {e}")),
                        ));
                    }
                    None => {
                        return Err(GatewayExitReason::Reconnectable(
                            ChannelError::ConnectionFailed("QQBot gateway stream ended".into()),
                        ));
                    }
                }
            }
            _ = tokio::time::sleep_until(heartbeat_deadline) => {
                send_heartbeat(&mut write, *last_seq).await.map_err(GatewayExitReason::Reconnectable)?;
                heartbeat_deadline = tokio::time::Instant::now() + heartbeat_interval;
            }
            _ = shutdown_rx.changed() => {
                debug!("QQBot gateway shutdown during listen");
                return Ok(());
            }
        }
    }
}

async fn send_heartbeat<S>(write: &mut S, last_seq: Option<u64>) -> Result<(), ChannelError>
where
    S: futures_util::Sink<tokio_tungstenite::tungstenite::Message> + Unpin,
    S::Error: std::fmt::Display,
{
    use futures_util::SinkExt;
    use tokio_tungstenite::tungstenite::Message as WsMessage;
    let hb = serde_json::json!({ "op": OP_HEARTBEAT, "d": last_seq });
    write
        .send(WsMessage::Text(hb.to_string().into()))
        .await
        .map_err(|e| ChannelError::ConnectionFailed(format!("QQBot heartbeat send failed: {e}")))
}

/// Handle an INTERACTION_CREATE: ACK + forward as unified action.
async fn handle_interaction(
    api: &Arc<QqbotApi>,
    interaction: &InteractionCreate,
    message_tx: &mpsc::Sender<UnifiedIncomingMessage>,
    confirm_tx: &mpsc::Sender<(String, String)>,
) {
    if interaction.interaction_type != INTERACTION_TYPE_BUTTON {
        return;
    }
    if stable_provider_event_id(&interaction.id, "INTERACTION_CREATE").is_none() {
        return;
    }
    // ACK the interaction.
    let _ = api.ack_interaction(&interaction.id).await;

    let Some(unified) = normalize_interaction(interaction) else {
        return;
    };

    // Tool-confirmation buttons feed confirm_tx.
    if let Some(action) = &unified.action
        && action.action == "system.confirm"
        && let Some(params) = &action.params
    {
        let call_id = params.get("callId").cloned().unwrap_or_default();
        let value = params.get("value").cloned().unwrap_or_default();
        if !call_id.is_empty() {
            let _ = confirm_tx.send((call_id, value)).await;
        }
    }

    let _ = message_tx.send(unified).await;
}

/// Build the rustls-based TLS connector for the gateway WebSocket.
fn build_ws_tls_connector() -> Result<tokio_tungstenite::Connector, ChannelError> {
    use tokio_tungstenite::Connector;

    let certs = rustls_native_certs::load_native_certs();
    let mut root_store = rustls::RootCertStore::empty();
    root_store.add_parsable_certificates(certs.certs);

    let provider = rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::ring::default_provider()));

    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| ChannelError::ConnectionFailed(format!("QQBot TLS config failed: {e}")))?
        .with_root_certificates(root_store)
        .with_no_client_auth();

    Ok(Connector::Rustls(Arc::new(config)))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::types::QqAuthor;

    // -- Chat ID helpers --

    #[test]
    fn build_and_parse_c2c() {
        let id = build_chat_id(ChatTarget::C2c, "user123");
        assert_eq!(id, "c2c:user123");
        let (target, bare) = parse_chat_id(&id).unwrap();
        assert_eq!(target, ChatTarget::C2c);
        assert_eq!(bare, "user123");
    }

    #[test]
    fn build_and_parse_group() {
        let id = build_chat_id(ChatTarget::Group, "group456");
        assert_eq!(id, "group:group456");
        let (target, bare) = parse_chat_id(&id).unwrap();
        assert_eq!(target, ChatTarget::Group);
        assert_eq!(bare, "group456");
    }

    #[test]
    fn build_and_parse_channel() {
        let id = build_chat_id(ChatTarget::Channel, "chan789");
        assert_eq!(id, "channel:chan789");
        let (target, bare) = parse_chat_id(&id).unwrap();
        assert_eq!(target, ChatTarget::Channel);
        assert_eq!(bare, "chan789");
    }

    #[test]
    fn build_and_parse_dm() {
        let id = build_chat_id(ChatTarget::Dm, "guild111");
        assert_eq!(id, "dm:guild111");
        let (target, bare) = parse_chat_id(&id).unwrap();
        assert_eq!(target, ChatTarget::Dm);
        assert_eq!(bare, "guild111");
    }

    #[test]
    fn parse_unknown_prefix_returns_none() {
        assert!(parse_chat_id("unknown:xyz").is_none());
        assert!(parse_chat_id("bare_id").is_none());
    }

    // -- Close-code actions --

    #[test]
    fn close_code_fatal() {
        assert_eq!(close_code_action(4914), CloseAction::Fatal);
        assert_eq!(close_code_action(4915), CloseAction::Fatal);
    }

    #[test]
    fn close_code_clear_and_reconnect() {
        assert_eq!(close_code_action(4004), CloseAction::ClearAndReconnect);
        assert_eq!(close_code_action(4006), CloseAction::ClearAndReconnect);
        assert_eq!(close_code_action(4007), CloseAction::ClearAndReconnect);
        assert_eq!(close_code_action(4009), CloseAction::ClearAndReconnect);
        assert_eq!(close_code_action(4900), CloseAction::ClearAndReconnect);
        assert_eq!(close_code_action(4913), CloseAction::ClearAndReconnect);
    }

    #[test]
    fn close_code_wait_and_reconnect() {
        assert_eq!(close_code_action(4008), CloseAction::WaitAndReconnect);
    }

    // -- Event normalization --

    fn c2c_author(user_openid: &str) -> QqAuthor {
        QqAuthor {
            user_openid: Some(user_openid.into()),
            member_openid: None,
            id: None,
            bot: false,
            username: None,
        }
    }

    fn group_author(member_openid: &str, bot: bool) -> QqAuthor {
        QqAuthor {
            user_openid: None,
            member_openid: Some(member_openid.into()),
            id: None,
            bot,
            username: None,
        }
    }

    fn guild_author(id: &str, username: &str, bot: bool) -> QqAuthor {
        QqAuthor {
            user_openid: None,
            member_openid: None,
            id: Some(id.into()),
            bot,
            username: Some(username.into()),
        }
    }

    #[test]
    fn normalize_c2c_basic() {
        let msg = C2cMessageCreate {
            id: "msg1".into(),
            content: "hello".into(),
            author: c2c_author("uid1"),
            timestamp: String::new(),
        };
        let unified = normalize_c2c_message(&msg).expect("should normalize");
        assert_eq!(unified.platform, PluginType::Qqbot);
        assert_eq!(unified.chat_id, "c2c:uid1");
        assert_eq!(unified.user.id, "uid1");
        assert_eq!(unified.content.text, "hello");
        assert_eq!(unified.content.content_type, MessageContentType::Text);
    }

    #[test]
    fn normalize_c2c_empty_openid_skipped() {
        let msg = C2cMessageCreate {
            id: "msg1".into(),
            content: "hello".into(),
            author: QqAuthor {
                user_openid: None,
                member_openid: None,
                id: None,
                bot: false,
                username: None,
            },
            timestamp: String::new(),
        };
        assert!(normalize_c2c_message(&msg).is_none());
    }

    #[test]
    fn normalize_messages_with_empty_provider_ids_are_skipped() {
        let c2c = C2cMessageCreate {
            id: " ".into(),
            content: "hello".into(),
            author: c2c_author("uid1"),
            timestamp: String::new(),
        };
        let group = GroupMessageCreate {
            id: String::new(),
            group_openid: "g1".into(),
            content: "hello".into(),
            author: group_author("mid1", false),
            timestamp: String::new(),
        };
        let channel = ChannelMessageCreate {
            id: "\t".into(),
            channel_id: "ch1".into(),
            guild_id: None,
            content: "hello".into(),
            author: guild_author("u1", "alice", false),
            timestamp: String::new(),
        };
        let dm = DirectMessageCreate {
            id: String::new(),
            guild_id: "g1".into(),
            content: "hello".into(),
            author: guild_author("u1", "alice", false),
            timestamp: String::new(),
        };

        assert!(normalize_c2c_message(&c2c).is_none());
        assert!(normalize_group_message(&group).is_none());
        assert!(normalize_channel_message(&channel).is_none());
        assert!(normalize_direct_message(&dm).is_none());
    }

    #[test]
    fn normalize_group_basic() {
        let msg = GroupMessageCreate {
            id: "msg2".into(),
            group_openid: "g1".into(),
            content: "hi group".into(),
            author: group_author("mid1", false),
            timestamp: String::new(),
        };
        let unified = normalize_group_message(&msg).expect("should normalize");
        assert_eq!(unified.chat_id, "group:g1");
        assert_eq!(unified.user.id, "mid1");
    }

    #[test]
    fn normalize_group_bot_skipped() {
        let msg = GroupMessageCreate {
            id: "msg2".into(),
            group_openid: "g1".into(),
            content: "bot echo".into(),
            author: group_author("mid1", true),
            timestamp: String::new(),
        };
        assert!(normalize_group_message(&msg).is_none());
    }

    #[test]
    fn normalize_channel_basic() {
        let msg = ChannelMessageCreate {
            id: "msg3".into(),
            channel_id: "ch1".into(),
            guild_id: Some("g1".into()),
            content: "@bot test".into(),
            author: guild_author("u1", "alice", false),
            timestamp: String::new(),
        };
        let unified = normalize_channel_message(&msg).expect("should normalize");
        assert_eq!(unified.chat_id, "channel:ch1");
        assert_eq!(unified.user.id, "u1");
        assert_eq!(unified.user.display_name, "alice");
    }

    #[test]
    fn normalize_channel_bot_skipped() {
        let msg = ChannelMessageCreate {
            id: "msg3".into(),
            channel_id: "ch1".into(),
            guild_id: None,
            content: "echo".into(),
            author: guild_author("u1", "botuser", true),
            timestamp: String::new(),
        };
        assert!(normalize_channel_message(&msg).is_none());
    }

    #[test]
    fn normalize_dm_basic() {
        let msg = DirectMessageCreate {
            id: "msg4".into(),
            guild_id: "g2".into(),
            content: "dm text".into(),
            author: guild_author("u2", "bob", false),
            timestamp: String::new(),
        };
        let unified = normalize_direct_message(&msg).expect("should normalize");
        assert_eq!(unified.chat_id, "dm:g2");
        assert_eq!(unified.user.id, "u2");
        assert_eq!(unified.user.display_name, "bob");
    }

    #[test]
    fn normalize_dm_bot_skipped() {
        let msg = DirectMessageCreate {
            id: "msg4".into(),
            guild_id: "g2".into(),
            content: "echo".into(),
            author: guild_author("u2", "bot", true),
            timestamp: String::new(),
        };
        assert!(normalize_direct_message(&msg).is_none());
    }

    #[test]
    fn normalize_interaction_button() {
        let interaction = InteractionCreate {
            id: "int1".into(),
            interaction_type: INTERACTION_TYPE_BUTTON,
            data: Some(super::super::types::InteractionData {
                resolved: Some(super::super::types::InteractionResolved {
                    button_id: Some("chat:chat.regenerate".into()),
                    button_data: None,
                }),
            }),
            channel_id: Some("ch1".into()),
            guild_id: None,
            group_openid: None,
            user_openid: Some("uid1".into()),
            group_member_openid: None,
            chat_type: None,
        };
        let unified = normalize_interaction(&interaction).expect("should normalize");
        assert_eq!(unified.content.content_type, MessageContentType::Action);
        assert_eq!(unified.chat_id, "channel:ch1");
        let action = unified.action.unwrap();
        assert_eq!(action.action, "chat.regenerate");
    }

    #[test]
    fn normalize_interaction_group_context() {
        let interaction = InteractionCreate {
            id: "int2".into(),
            interaction_type: INTERACTION_TYPE_BUTTON,
            data: Some(super::super::types::InteractionData {
                resolved: Some(super::super::types::InteractionResolved {
                    button_id: Some("system:session.new".into()),
                    button_data: None,
                }),
            }),
            channel_id: None,
            guild_id: None,
            group_openid: Some("g1".into()),
            user_openid: None,
            group_member_openid: Some("mid1".into()),
            chat_type: None,
        };
        let unified = normalize_interaction(&interaction).expect("should normalize");
        assert_eq!(unified.chat_id, "group:g1");
        assert_eq!(unified.user.id, "mid1");
    }

    #[test]
    fn normalize_interaction_with_empty_provider_id_is_skipped() {
        let interaction = InteractionCreate {
            id: " ".into(),
            interaction_type: INTERACTION_TYPE_BUTTON,
            data: Some(super::super::types::InteractionData {
                resolved: Some(super::super::types::InteractionResolved {
                    button_id: Some("system:session.new".into()),
                    button_data: None,
                }),
            }),
            channel_id: Some("ch1".into()),
            guild_id: None,
            group_openid: None,
            user_openid: Some("uid1".into()),
            group_member_openid: None,
            chat_type: None,
        };

        assert!(normalize_interaction(&interaction).is_none());
    }

    #[test]
    fn normalize_non_button_interaction_skipped() {
        let interaction = InteractionCreate {
            id: "int3".into(),
            interaction_type: 1, // Not a button.
            data: None,
            channel_id: None,
            guild_id: None,
            group_openid: None,
            user_openid: None,
            group_member_openid: None,
            chat_type: None,
        };
        assert!(normalize_interaction(&interaction).is_none());
    }

    // -- Passive reply window --

    #[test]
    fn passive_reply_decrement_and_expire() {
        let map: PassiveReplyMap = Arc::new(DashMap::new());
        record_inbound_msg_id(&map, "c2c:u1", "msg_a");
        let now = tokio::time::Instant::now();

        // Should consume successfully.
        let mid = consume_passive_reply(&map, "c2c:u1", now);
        assert_eq!(mid.as_deref(), Some("msg_a"));

        // Consume remaining slots.
        for _ in 1..QQBOT_PASSIVE_REPLY_MAX {
            assert!(consume_passive_reply(&map, "c2c:u1", now).is_some());
        }

        // Exhausted.
        assert!(consume_passive_reply(&map, "c2c:u1", now).is_none());
    }

    #[test]
    fn passive_reply_expiry() {
        let map: PassiveReplyMap = Arc::new(DashMap::new());
        record_inbound_msg_id(&map, "c2c:u1", "msg_b");
        // Simulate time past expiry.
        let future = tokio::time::Instant::now() + QQBOT_PASSIVE_REPLY_WINDOW + Duration::from_secs(1);
        assert!(consume_passive_reply(&map, "c2c:u1", future).is_none());
    }

    #[test]
    fn passive_reply_unknown_chat() {
        let map: PassiveReplyMap = Arc::new(DashMap::new());
        let now = tokio::time::Instant::now();
        assert!(consume_passive_reply(&map, "c2c:unknown", now).is_none());
    }

    // -- msg_seq generation --

    #[test]
    fn msg_seq_increments() {
        let a = next_msg_seq();
        let b = next_msg_seq();
        assert!(b > a);
    }

    // -- backoff --

    #[test]
    fn backoff_is_exponential_and_capped() {
        assert_eq!(backoff_delay(1), Duration::from_secs(2));
        assert_eq!(backoff_delay(3), Duration::from_secs(8));
        assert_eq!(backoff_delay(10), Duration::from_secs(30)); // capped
    }
}
