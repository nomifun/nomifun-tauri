//! Discord Gateway WebSocket connection loop (v10, JSON).
//!
//! Mirrors `lark`'s `connect_and_listen`: connect, receive HELLO, IDENTIFY,
//! heartbeat on a deadline, dispatch events, and reconnect with exponential
//! backoff. Inbound MESSAGE_CREATE / INTERACTION_CREATE events are normalized
//! into [`UnifiedIncomingMessage`] (the pure helpers below are unit-tested).

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tracing::{debug, error, info, warn};

use crate::constants::{DISCORD_MAX_RECONNECT_ATTEMPTS, DISCORD_MAX_RECONNECT_DELAY};
use crate::error::ChannelError;
use crate::plugin::{SharedPluginStatus, mark_error_on_unexpected_exit};
use crate::plugins::callback::parse_callback_data;
use crate::types::{
    ActionContext, MessageContentType, PluginType, UnifiedAction, UnifiedAttachment, UnifiedIncomingMessage,
    UnifiedMessageContent, UnifiedUser,
};

use super::api::DiscordApi;
use super::types::{
    GatewayPayload, HelloData, IdentifyData, IdentifyProperties, InteractionCreate, MessageCreate, OutgoingFrame,
    GATEWAY_INTENTS, INTERACTION_CALLBACK_DEFERRED_UPDATE, INTERACTION_TYPE_MESSAGE_COMPONENT, OP_DISPATCH, OP_HEARTBEAT,
    OP_HELLO, OP_IDENTIFY, OP_INVALID_SESSION, OP_RECONNECT,
};

const GATEWAY_URL: &str = "wss://gateway.discord.gg/?v=10&encoding=json";

/// Background task: maintain the Discord gateway connection with reconnects.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_gateway(
    api: Arc<DiscordApi>,
    token: String,
    self_bot_id: String,
    message_tx: mpsc::Sender<UnifiedIncomingMessage>,
    confirm_tx: mpsc::Sender<(String, String)>,
    status: SharedPluginStatus,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let mut consecutive_errors: u32 = 0;

    loop {
        if *shutdown_rx.borrow() {
            debug!("Discord gateway loop received shutdown signal");
            break;
        }

        match connect_once(&api, &token, &self_bot_id, &message_tx, &confirm_tx, &mut shutdown_rx).await {
            Ok(()) => {
                // Clean close (shutdown / server RECONNECT). Reset backoff.
                consecutive_errors = 0;
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            Err(e) => {
                consecutive_errors += 1;
                warn!(error = %e, consecutive_errors, "Discord gateway error");
                if consecutive_errors >= DISCORD_MAX_RECONNECT_ATTEMPTS {
                    error!("Discord max reconnect attempts reached, stopping gateway loop");
                    break;
                }
                let backoff = backoff_delay(consecutive_errors);
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = shutdown_rx.changed() => break,
                }
            }
        }
    }

    mark_error_on_unexpected_exit(&status, &shutdown_rx, "discord");
    debug!("Discord gateway loop exited");
}

/// Exponential backoff capped at `DISCORD_MAX_RECONNECT_DELAY`.
fn backoff_delay(attempt: u32) -> Duration {
    let secs = 2u64.saturating_pow(attempt).min(DISCORD_MAX_RECONNECT_DELAY.as_secs());
    Duration::from_secs(secs)
}

/// A single gateway connection: HELLO → IDENTIFY → heartbeat + dispatch.
async fn connect_once(
    api: &Arc<DiscordApi>,
    token: &str,
    self_bot_id: &str,
    message_tx: &mpsc::Sender<UnifiedIncomingMessage>,
    confirm_tx: &mpsc::Sender<(String, String)>,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> Result<(), ChannelError> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::connect_async_tls_with_config;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let connector = build_ws_tls_connector()?;
    let (ws_stream, _) = connect_async_tls_with_config(GATEWAY_URL, None, false, Some(connector))
        .await
        .map_err(|e| ChannelError::ConnectionFailed(format!("Discord gateway connect failed: {e}")))?;
    info!("Discord gateway connected");

    let (mut write, mut read) = ws_stream.split();

    // First frame must be HELLO (op 10) with the heartbeat interval.
    let hello = read
        .next()
        .await
        .ok_or_else(|| ChannelError::ConnectionFailed("Discord gateway closed before HELLO".into()))?
        .map_err(|e| ChannelError::ConnectionFailed(format!("Discord gateway read error: {e}")))?;
    let heartbeat_interval = match hello {
        WsMessage::Text(txt) => {
            let payload: GatewayPayload = serde_json::from_str(&txt)
                .map_err(|e| ChannelError::ConnectionFailed(format!("Discord HELLO parse failed: {e}")))?;
            if payload.op != OP_HELLO {
                return Err(ChannelError::ConnectionFailed(format!(
                    "Discord expected HELLO, got op {}",
                    payload.op
                )));
            }
            let hello: HelloData = serde_json::from_value(payload.d)
                .map_err(|e| ChannelError::ConnectionFailed(format!("Discord HELLO data parse failed: {e}")))?;
            Duration::from_millis(hello.heartbeat_interval)
        }
        _ => return Err(ChannelError::ConnectionFailed("Discord first frame was not text HELLO".into())),
    };

    // IDENTIFY.
    let identify = OutgoingFrame {
        op: OP_IDENTIFY,
        d: IdentifyData {
            token: token.to_string(),
            intents: GATEWAY_INTENTS,
            properties: IdentifyProperties {
                os: std::env::consts::OS.to_string(),
                browser: "nomi".to_string(),
                device: "nomi".to_string(),
            },
        },
    };
    let identify_json = serde_json::to_string(&identify)
        .map_err(|e| ChannelError::ConnectionFailed(format!("Discord IDENTIFY encode failed: {e}")))?;
    write
        .send(WsMessage::Text(identify_json.into()))
        .await
        .map_err(|e| ChannelError::ConnectionFailed(format!("Discord IDENTIFY send failed: {e}")))?;

    let mut last_seq: Option<u64> = None;
    let mut heartbeat_deadline = tokio::time::Instant::now() + heartbeat_interval;

    loop {
        tokio::select! {
            frame = read.next() => {
                match frame {
                    Some(Ok(WsMessage::Text(txt))) => {
                        let payload: GatewayPayload = match serde_json::from_str(&txt) {
                            Ok(p) => p,
                            Err(e) => { warn!(error = %e, "Discord gateway frame parse failed"); continue; }
                        };
                        if let Some(s) = payload.s {
                            last_seq = Some(s);
                        }
                        match payload.op {
                            OP_DISPATCH => {
                                let t = payload.t.as_deref().unwrap_or("");
                                match t {
                                    "MESSAGE_CREATE" => {
                                        match serde_json::from_value::<MessageCreate>(payload.d) {
                                            Ok(msg) => {
                                                if let Some(unified) = normalize_message_create(&msg, self_bot_id) {
                                                    let _ = message_tx.send(unified).await;
                                                }
                                            }
                                            Err(e) => warn!(error = %e, "Discord MESSAGE_CREATE parse failed"),
                                        }
                                    }
                                    "INTERACTION_CREATE" => {
                                        match serde_json::from_value::<InteractionCreate>(payload.d) {
                                            Ok(interaction) => {
                                                handle_interaction(api, &interaction, message_tx, confirm_tx).await;
                                            }
                                            Err(e) => warn!(error = %e, "Discord INTERACTION_CREATE parse failed"),
                                        }
                                    }
                                    _ => { /* READY, TYPING_START, etc. — ignore */ }
                                }
                            }
                            OP_HEARTBEAT => {
                                // Server asked for an immediate heartbeat.
                                send_heartbeat(&mut write, last_seq).await?;
                                heartbeat_deadline = tokio::time::Instant::now() + heartbeat_interval;
                            }
                            OP_RECONNECT | OP_INVALID_SESSION => {
                                debug!(op = payload.op, "Discord gateway requested reconnect");
                                return Ok(());
                            }
                            _ => { /* HEARTBEAT_ACK and others — ignore */ }
                        }
                    }
                    Some(Ok(WsMessage::Close(_))) => {
                        debug!("Discord gateway received close frame");
                        return Ok(());
                    }
                    Some(Ok(_)) => { /* binary/ping/pong — ignore */ }
                    Some(Err(e)) => {
                        return Err(ChannelError::ConnectionFailed(format!("Discord gateway read error: {e}")));
                    }
                    None => {
                        return Err(ChannelError::ConnectionFailed("Discord gateway stream ended".into()));
                    }
                }
            }
            _ = tokio::time::sleep_until(heartbeat_deadline) => {
                send_heartbeat(&mut write, last_seq).await?;
                heartbeat_deadline = tokio::time::Instant::now() + heartbeat_interval;
            }
            _ = shutdown_rx.changed() => {
                debug!("Discord gateway shutdown during listen");
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
        .map_err(|e| ChannelError::ConnectionFailed(format!("Discord heartbeat send failed: {e}")))
}

/// Acknowledge a component interaction and forward it as a unified action.
async fn handle_interaction(
    api: &Arc<DiscordApi>,
    interaction: &InteractionCreate,
    message_tx: &mpsc::Sender<UnifiedIncomingMessage>,
    confirm_tx: &mpsc::Sender<(String, String)>,
) {
    if interaction.interaction_type != INTERACTION_TYPE_MESSAGE_COMPONENT {
        return;
    }
    // The interaction snowflake is the durable provider event identity.  A
    // blank ID must fail closed before Discord ACK, tool confirmation, or
    // message-loop enqueue side effects.
    if interaction.id.trim().is_empty() {
        warn!("Ignoring Discord component interaction without a provider interaction ID");
        return;
    }
    // Acknowledge within Discord's 3s window so the client stops spinning.
    let _ = api
        .ack_interaction(&interaction.id, &interaction.token, INTERACTION_CALLBACK_DEFERRED_UPDATE)
        .await;

    let Some(unified) = normalize_interaction(interaction) else {
        return;
    };

    // Tool-confirmation buttons (system.confirm) also feed confirm_tx.
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

// ---------------------------------------------------------------------------
// Pure normalization helpers (unit-tested)
// ---------------------------------------------------------------------------

/// Normalize a MESSAGE_CREATE into a [`UnifiedIncomingMessage`], applying the
/// bot-loop guard (skip bot authors) and guild mention gating (guild messages
/// must @mention the bot; DMs always pass). Returns `None` when filtered out.
pub(super) fn normalize_message_create(msg: &MessageCreate, self_bot_id: &str) -> Option<UnifiedIncomingMessage> {
    if msg.id.trim().is_empty() {
        warn!("Ignoring Discord message without a provider message ID");
        return None;
    }

    // Bot-loop guard: ignore all bot authors (includes ourselves).
    if msg.author.bot {
        return None;
    }
    // Guild mention gating: a guild message must mention the bot. DMs (no
    // guild_id) are always processed.
    if msg.guild_id.is_some() {
        let mentioned = msg.mentions.iter().any(|u| u.id == self_bot_id);
        if !mentioned {
            return None;
        }
    }

    let (content_type, attachments) = extract_attachments(msg);
    let user = UnifiedUser {
        id: msg.author.id.clone(),
        username: Some(msg.author.username.clone()),
        display_name: msg.author.display(),
        avatar_url: None,
    };

    Some(UnifiedIncomingMessage {
        id: msg.id.clone(),
        platform: PluginType::Discord,
        chat_id: msg.channel_id.clone(),
        user,
        content: UnifiedMessageContent {
            content_type,
            text: msg.content.clone(),
            attachments,
        },
        timestamp: snowflake_to_unix(&msg.id),
        reply_to_message_id: msg.message_reference.as_ref().and_then(|r| r.message_id.clone()),
        action: None,
        raw: None,
    })
}

/// Map Discord attachments to the unified content type + attachment list.
fn extract_attachments(msg: &MessageCreate) -> (MessageContentType, Option<Vec<UnifiedAttachment>>) {
    if msg.attachments.is_empty() {
        return (MessageContentType::Text, None);
    }
    let is_image = msg
        .attachments
        .iter()
        .any(|a| a.content_type.as_deref().is_some_and(|c| c.starts_with("image/")));
    let atts: Vec<UnifiedAttachment> = msg
        .attachments
        .iter()
        .map(|a| UnifiedAttachment {
            file_id: None,
            file_name: Some(a.filename.clone()),
            mime_type: a.content_type.clone(),
            file_size: a.size,
            url: Some(a.url.clone()),
        })
        .collect();
    let content_type = if is_image {
        MessageContentType::Photo
    } else {
        MessageContentType::Document
    };
    (content_type, Some(atts))
}

/// Normalize a component (button) interaction into a unified action message.
pub(super) fn normalize_interaction(interaction: &InteractionCreate) -> Option<UnifiedIncomingMessage> {
    if interaction.interaction_type != INTERACTION_TYPE_MESSAGE_COMPONENT {
        return None;
    }
    if interaction.id.trim().is_empty() {
        return None;
    }
    let custom_id = interaction.data.as_ref().and_then(|d| d.custom_id.clone())?;
    let user = interaction.acting_user()?;
    let chat_id = interaction.channel_id.clone().unwrap_or_default();
    let message_id = interaction.message.as_ref().map(|m| m.id.clone());

    let parsed = parse_callback_data(&custom_id);
    let action = parsed.map(|p| UnifiedAction {
        action: p.action,
        category: p.category,
        params: p.params,
        context: ActionContext {
            platform: PluginType::Discord,
            user_id: user.id.clone(),
            chat_id: chat_id.clone(),
            message_id: message_id.clone(),
            session_id: None,
        },
    });

    Some(UnifiedIncomingMessage {
        id: interaction.id.clone(),
        platform: PluginType::Discord,
        chat_id,
        user: UnifiedUser {
            id: user.id.clone(),
            username: Some(user.username.clone()),
            display_name: user.display(),
            avatar_url: None,
        },
        content: UnifiedMessageContent {
            content_type: MessageContentType::Action,
            text: custom_id,
            attachments: None,
        },
        timestamp: snowflake_to_unix(&interaction.id),
        reply_to_message_id: message_id,
        action,
        raw: None,
    })
}

/// Derive a unix-seconds timestamp from a Discord snowflake id.
/// `timestamp_ms = (snowflake >> 22) + DISCORD_EPOCH_MS`.
fn snowflake_to_unix(id: &str) -> i64 {
    const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;
    match id.parse::<u64>() {
        Ok(snowflake) => (((snowflake >> 22) + DISCORD_EPOCH_MS) / 1000) as i64,
        Err(_) => 0,
    }
}

/// Build the rustls-based TLS connector for the gateway WebSocket (mirrors the
/// lark plugin's helper).
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
        .map_err(|e| ChannelError::ConnectionFailed(format!("Discord TLS config failed: {e}")))?
        .with_root_certificates(root_store)
        .with_no_client_auth();

    Ok(Connector::Rustls(Arc::new(config)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::types::{DiscordAttachment, DiscordUser, InteractionData, InteractionMessage, MessageReference};

    fn user(id: &str, bot: bool) -> DiscordUser {
        DiscordUser {
            id: id.into(),
            username: format!("user{id}"),
            global_name: None,
            bot,
        }
    }

    fn dm_message(content: &str) -> MessageCreate {
        MessageCreate {
            id: "175928847299117063".into(), // a real-ish snowflake
            channel_id: "chan1".into(),
            guild_id: None,
            author: user("42", false),
            content: content.into(),
            attachments: vec![],
            mentions: vec![],
            message_reference: None,
        }
    }

    #[test]
    fn dm_message_is_normalized() {
        let msg = dm_message("hello");
        let unified = normalize_message_create(&msg, "selfbot").expect("DM should pass");
        assert_eq!(unified.platform, PluginType::Discord);
        assert_eq!(unified.chat_id, "chan1");
        assert_eq!(unified.content.text, "hello");
        assert_eq!(unified.content.content_type, MessageContentType::Text);
        assert_eq!(unified.user.id, "42");
    }

    #[test]
    fn blank_message_id_is_rejected() {
        let mut msg = dm_message("hello");
        msg.id = " \t".into();
        assert!(normalize_message_create(&msg, "selfbot").is_none());
    }

    #[test]
    fn bot_author_is_skipped() {
        let mut msg = dm_message("loop");
        msg.author = user("99", true);
        assert!(normalize_message_create(&msg, "selfbot").is_none());
    }

    #[test]
    fn guild_message_requires_mention() {
        let mut msg = dm_message("no mention");
        msg.guild_id = Some("guild1".into());
        // Not mentioned -> filtered.
        assert!(normalize_message_create(&msg, "selfbot").is_none());
        // Mentioned -> passes.
        msg.mentions = vec![user("selfbot", false)];
        assert!(normalize_message_create(&msg, "selfbot").is_some());
    }

    #[test]
    fn reply_reference_is_carried() {
        let mut msg = dm_message("re");
        msg.message_reference = Some(MessageReference {
            message_id: Some("origmsg".into()),
        });
        let unified = normalize_message_create(&msg, "selfbot").unwrap();
        assert_eq!(unified.reply_to_message_id.as_deref(), Some("origmsg"));
    }

    #[test]
    fn image_attachment_sets_photo() {
        let mut msg = dm_message("pic");
        msg.attachments = vec![DiscordAttachment {
            filename: "a.png".into(),
            content_type: Some("image/png".into()),
            size: Some(123),
            url: "https://cdn/a.png".into(),
        }];
        let unified = normalize_message_create(&msg, "selfbot").unwrap();
        assert_eq!(unified.content.content_type, MessageContentType::Photo);
        let atts = unified.content.attachments.unwrap();
        assert_eq!(atts[0].url.as_deref(), Some("https://cdn/a.png"));
        assert_eq!(atts[0].file_name.as_deref(), Some("a.png"));
    }

    #[test]
    fn non_image_attachment_sets_document() {
        let mut msg = dm_message("file");
        msg.attachments = vec![DiscordAttachment {
            filename: "a.pdf".into(),
            content_type: Some("application/pdf".into()),
            size: Some(10),
            url: "https://cdn/a.pdf".into(),
        }];
        let unified = normalize_message_create(&msg, "selfbot").unwrap();
        assert_eq!(unified.content.content_type, MessageContentType::Document);
    }

    #[test]
    fn interaction_button_becomes_action() {
        let interaction = InteractionCreate {
            id: "175928847299117063".into(),
            token: "tok".into(),
            interaction_type: INTERACTION_TYPE_MESSAGE_COMPONENT,
            data: Some(InteractionData {
                custom_id: Some("chat:chat.regenerate".into()),
            }),
            channel_id: Some("chan1".into()),
            member: None,
            user: Some(user("7", false)),
            message: Some(InteractionMessage { id: "msg9".into() }),
        };
        let unified = normalize_interaction(&interaction).expect("button -> action");
        assert_eq!(unified.content.content_type, MessageContentType::Action);
        let action = unified.action.unwrap();
        assert_eq!(action.action, "chat.regenerate");
        assert_eq!(action.context.chat_id, "chan1");
        assert_eq!(unified.reply_to_message_id.as_deref(), Some("msg9"));
    }

    #[test]
    fn non_component_interaction_ignored() {
        let interaction = InteractionCreate {
            id: "1".into(),
            token: "tok".into(),
            interaction_type: 2, // APPLICATION_COMMAND
            data: None,
            channel_id: Some("c".into()),
            member: None,
            user: Some(user("7", false)),
            message: None,
        };
        assert!(normalize_interaction(&interaction).is_none());
    }

    #[tokio::test]
    async fn blank_interaction_id_fails_before_ack_confirm_or_enqueue() {
        let interaction = InteractionCreate {
            id: " \n".into(),
            token: "tok".into(),
            interaction_type: INTERACTION_TYPE_MESSAGE_COMPONENT,
            data: Some(InteractionData {
                custom_id: Some("chat:system.confirm:callId=call-1,value=yes".into()),
            }),
            channel_id: Some("chan1".into()),
            member: None,
            user: Some(user("7", false)),
            message: Some(InteractionMessage { id: "msg9".into() }),
        };
        let api = Arc::new(DiscordApi::new(reqwest::Client::new(), "unused-test-token"));
        let (message_tx, mut message_rx) = mpsc::channel(1);
        let (confirm_tx, mut confirm_rx) = mpsc::channel(1);

        handle_interaction(&api, &interaction, &message_tx, &confirm_tx).await;

        assert!(normalize_interaction(&interaction).is_none());
        assert!(message_rx.try_recv().is_err());
        assert!(confirm_rx.try_recv().is_err());
    }

    #[test]
    fn backoff_is_exponential_and_capped() {
        assert_eq!(backoff_delay(1), Duration::from_secs(2));
        assert_eq!(backoff_delay(3), Duration::from_secs(8));
        assert_eq!(backoff_delay(10), Duration::from_secs(30)); // capped
    }

    #[test]
    fn snowflake_decodes_to_unix() {
        // Snowflake 175928847299117063 → 2016-04-30 ~11:18:25 UTC ≈ 1462015105
        let ts = snowflake_to_unix("175928847299117063");
        assert!(ts > 1_400_000_000 && ts < 1_500_000_000, "got {ts}");
        assert_eq!(snowflake_to_unix("not-a-number"), 0);
    }
}
