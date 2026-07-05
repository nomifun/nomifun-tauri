use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use crate::constants::DINGTALK_MESSAGE_LIMIT;
use crate::error::ChannelError;
use crate::plugin::{ChannelPlugin, PluginCallbacks, SharedPluginStatus, mark_error_on_unexpected_exit};
use crate::types::{
    ActionCategory, ActionContext, BotInfo, MessageContentType, OutgoingMessageType, PluginConfig, PluginStatus,
    PluginType, UnifiedAction, UnifiedIncomingMessage, UnifiedMessageContent, UnifiedOutgoingMessage, UnifiedUser,
};

use super::api::DingtalkApi;
use super::types::{
    BotMessageCallback, CardActionCallback, CardData, CreateCardInstanceRequest, DeliverCardRequest,
    ImGroupDeliverModel, ImRobotDeliverModel, SendRobotMessageRequest, SpaceModel, StreamAck, StreamFrame,
    StreamingWriteRequest, SystemEvent, UpdateCardRequest, build_open_space_id, decode_chat_id, encode_chat_id,
    format_dingtalk_callback, parse_dingtalk_callback,
};

/// Maximum reconnect attempts before giving up.
const MAX_RECONNECT_ATTEMPTS: u32 = 10;

/// Maximum backoff delay between reconnection attempts.
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(30);

/// DingTalk standard AI card template ID.
const AI_CARD_TEMPLATE_ID: &str = "382e4302-551d-4880-bf29-a30acfab2e71.schema";

/// DingTalk platform plugin.
///
/// Connects via WebSocket Stream, handles bot message callbacks and
/// card action callbacks. Uses AI Card for streaming message updates
/// with fallback to session webhook or Open API.
#[derive(Default)]
pub struct DingtalkPlugin {
    /// Shared with the WebSocket loop so a dead loop can flip it to `Error`.
    status: SharedPluginStatus,
    bot_info: Option<BotInfo>,
    last_error: Option<String>,
    api: Option<Arc<DingtalkApi>>,
    ws_handle: Option<JoinHandle<()>>,
    shutdown_tx: Option<watch::Sender<bool>>,
}

impl DingtalkPlugin {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl ChannelPlugin for DingtalkPlugin {
    async fn initialize(&mut self, config: PluginConfig, callbacks: PluginCallbacks) -> Result<(), ChannelError> {
        self.status.set(PluginStatus::Initializing);

        let client_id = config
            .credentials
            .client_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                self.status.set(PluginStatus::Error);
                self.last_error = Some("Missing DingTalk client_id".into());
                ChannelError::InvalidConfig("Missing DingTalk client_id".into())
            })?;

        let client_secret = config
            .credentials
            .client_secret
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                self.status.set(PluginStatus::Error);
                self.last_error = Some("Missing DingTalk client_secret".into());
                ChannelError::InvalidConfig("Missing DingTalk client_secret".into())
            })?;

        let http_client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| {
                self.status.set(PluginStatus::Error);
                self.last_error = Some(format!("HTTP client init failed: {e}"));
                ChannelError::ConnectionFailed(format!("HTTP client init failed: {e}"))
            })?;

        let api = Arc::new(DingtalkApi::new(http_client, client_id, client_secret));

        // Validate credentials by getting bot info
        let bot_data = api.get_bot_info().await.map_err(|e| {
            self.status.set(PluginStatus::Error);
            self.last_error = Some(format!("Credential validation failed: {e}"));
            e
        })?;

        self.bot_info = Some(BotInfo {
            id: bot_data.robot_user_id.clone().unwrap_or_default(),
            username: None,
            display_name: bot_data.nick.clone().unwrap_or_default(),
        });

        info!(
            bot_name = bot_data.nick.as_deref().unwrap_or(""),
            bot_id = bot_data.robot_user_id.as_deref().unwrap_or(""),
            "DingTalk bot initialized"
        );

        self.api = Some(api);

        // Set up shutdown channel
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        self.shutdown_tx = Some(shutdown_tx);

        // Spawn the WebSocket Stream connection loop
        let api_clone = Arc::clone(self.api.as_ref().expect("api just set"));
        self.ws_handle = Some(tokio::spawn(ws_stream_loop(
            api_clone,
            callbacks.message_tx,
            callbacks.confirm_tx,
            self.status.clone(),
            shutdown_rx,
        )));

        self.status.set(PluginStatus::Ready);
        Ok(())
    }

    async fn start(&mut self) -> Result<(), ChannelError> {
        self.status.set(PluginStatus::Starting);
        self.status.set(PluginStatus::Running);
        info!("DingTalk plugin started");
        Ok(())
    }

    async fn stop(&mut self) -> Result<(), ChannelError> {
        self.status.set(PluginStatus::Stopping);

        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(true);
        }

        if let Some(handle) = self.ws_handle.take() {
            let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        }

        self.api = None;
        self.status.set(PluginStatus::Stopped);
        info!("DingTalk plugin stopped");
        Ok(())
    }

    async fn send_message(&self, chat_id: &str, message: UnifiedOutgoingMessage) -> Result<String, ChannelError> {
        let api = self
            .api
            .as_ref()
            .ok_or_else(|| ChannelError::PlatformApi("Plugin not initialized".into()))?;

        let text = truncate_message(message.text.as_deref().unwrap_or(""), DINGTALK_MESSAGE_LIMIT);

        // Only use AI Card for streaming (non-final messages). The final turn
        // is marked by the `Buttons` message type and goes one-shot via Open API.
        if message.message_type != OutgoingMessageType::Buttons {
            match send_via_ai_card(api, chat_id).await {
                Ok(card_id) => return Ok(card_id),
                Err(e) => {
                    warn!(error = %e, "AI Card send failed, falling back to Open API");
                }
            }
        }

        // Fallback: send via Open API
        send_via_open_api(api, chat_id, &text).await
    }

    async fn edit_message(
        &self,
        _chat_id: &str,
        message_id: &str,
        message: UnifiedOutgoingMessage,
    ) -> Result<(), ChannelError> {
        let api = self
            .api
            .as_ref()
            .ok_or_else(|| ChannelError::PlatformApi("Plugin not initialized".into()))?;

        let text = truncate_message(message.text.as_deref().unwrap_or(""), DINGTALK_MESSAGE_LIMIT);

        // The `Buttons` message type marks the final message in a streaming
        // sequence (it carries no action buttons anymore — see build_final_message).
        let is_final = message.message_type == OutgoingMessageType::Buttons;

        // AI Card streaming write (always send full content, not deltas)
        let req = StreamingWriteRequest {
            out_track_id: message_id.to_string(),
            key: "msgContent".into(),
            content: text.clone(),
            is_full: true,
            is_finalize: is_final,
            is_error: false,
            guid: generate_guid(),
        };

        api.streaming_write(&req).await?;

        // When finalizing, update the card status to FINISHED.
        if is_final {
            let card_param_map = build_final_card_param_map(&text, message.buttons.as_deref());
            let update_req = UpdateCardRequest {
                out_track_id: message_id.to_string(),
                card_data: CardData {
                    card_param_map: Some(card_param_map),
                },
            };
            api.update_card(&update_req).await?;
        }

        Ok(())
    }

    fn active_user_count(&self) -> usize {
        0
    }

    fn bot_info(&self) -> Option<&BotInfo> {
        self.bot_info.as_ref()
    }

    fn plugin_type(&self) -> PluginType {
        PluginType::Dingtalk
    }

    fn status(&self) -> PluginStatus {
        self.status.get()
    }

    fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }
}

// ---------------------------------------------------------------------------
// AI Card operations
// ---------------------------------------------------------------------------

/// Create an empty AI Card for streaming (create + deliver + set INPUTING).
///
/// Returns the `outTrackId` which is used as the message ID for subsequent streaming writes.
async fn send_via_ai_card(api: &Arc<DingtalkApi>, chat_id: &str) -> Result<String, ChannelError> {
    let (is_group, _) = decode_chat_id(chat_id);

    let out_track_id = generate_out_track_id();

    let create_req = CreateCardInstanceRequest {
        card_template_id: AI_CARD_TEMPLATE_ID.into(),
        out_track_id: out_track_id.clone(),
        callback_type: "STREAM".into(),
        card_data: CardData {
            card_param_map: Some(serde_json::json!({})),
        },
        im_group_open_space_model: Some(SpaceModel { support_forward: true }),
        im_robot_open_space_model: Some(SpaceModel { support_forward: true }),
    };

    api.create_card_instance(&create_req).await?;

    // Deliver the card
    let open_space_id = build_open_space_id(chat_id);

    let deliver_req = DeliverCardRequest {
        out_track_id: out_track_id.clone(),
        open_space_id,
        user_id_type: 1,
        im_group_open_deliver_model: if is_group {
            Some(ImGroupDeliverModel {
                robot_code: api.client_id().to_string(),
            })
        } else {
            None
        },
        im_robot_open_deliver_model: if !is_group {
            Some(ImRobotDeliverModel {
                space_type: "IM_ROBOT".into(),
            })
        } else {
            None
        },
    };

    api.deliver_card(&deliver_req).await?;

    // Transition card to INPUTING state so streaming writes are accepted.
    let inputing_req = UpdateCardRequest {
        out_track_id: out_track_id.clone(),
        card_data: CardData {
            card_param_map: Some(serde_json::json!({
                "flowStatus": "2",
                "msgContent": "",
                "staticMsgContent": "",
                "sys_full_json_obj": serde_json::json!({"order": ["msgContent"]}).to_string(),
            })),
        },
    };
    api.update_card(&inputing_req).await?;

    debug!(card_id = %out_track_id, "DingTalk AI Card delivered");
    Ok(out_track_id)
}

/// Send a message via DingTalk Open API (fallback).
async fn send_via_open_api(api: &Arc<DingtalkApi>, chat_id: &str, text: &str) -> Result<String, ChannelError> {
    let (is_group, raw_id) = decode_chat_id(chat_id);

    let req = SendRobotMessageRequest {
        msg_key: "sampleMarkdown".into(),
        msg_param: serde_json::json!({ "title": "Message", "text": text }).to_string(),
        robot_code: api.client_id().to_string(),
        open_conversation_id: if is_group { Some(raw_id.to_string()) } else { None },
        user_ids: if !is_group {
            Some(vec![raw_id.to_string()])
        } else {
            None
        },
    };

    let resp = api.send_robot_message(&req).await?;
    let msg_id = resp
        .process_query_key
        .unwrap_or_else(|| format!("dt_msg_{}", chrono_now()));
    Ok(msg_id)
}

/// Build the card_param_map for finalizing an AI Card (status = FINISHED).
fn build_final_card_param_map(text: &str, buttons: Option<&[Vec<crate::types::ActionButton>]>) -> serde_json::Value {
    let mut map = serde_json::json!({
        "flowStatus": "3",
        "msgContent": text,
        "staticMsgContent": "",
        "sys_full_json_obj": serde_json::json!({"order": ["msgContent"]}).to_string(),
    });

    if let Some(button_rows) = buttons {
        let mut action_list = Vec::new();
        for row in button_rows {
            for btn in row {
                let callback_value = format_dingtalk_callback(&btn.action, btn.params.as_ref());
                action_list.push(serde_json::json!({
                    "label": btn.label,
                    "action": callback_value
                }));
            }
        }
        if !action_list.is_empty() {
            map["actions"] = serde_json::json!(action_list);
        }
    }

    map
}

/// Generate a unique outTrackId for AI Card instances.
fn generate_out_track_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("nomi_{}_{}", ts, seq)
}

/// Generate a unique GUID for streaming write operations.
fn generate_guid() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}_{}", ts, seq)
}

// ---------------------------------------------------------------------------
// WebSocket Stream connection loop
// ---------------------------------------------------------------------------

/// Background task that maintains a WebSocket Stream connection to DingTalk.
///
/// On disconnect, implements exponential backoff reconnection up to
/// `MAX_RECONNECT_ATTEMPTS`.
async fn ws_stream_loop(
    api: Arc<DingtalkApi>,
    message_tx: mpsc::Sender<UnifiedIncomingMessage>,
    confirm_tx: mpsc::Sender<(String, String)>,
    status: SharedPluginStatus,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let mut consecutive_errors: u32 = 0;

    loop {
        if *shutdown_rx.borrow() {
            debug!("DingTalk WS loop received shutdown signal");
            break;
        }

        // Register and get stream endpoint
        let stream_info = match api.register_stream().await {
            Ok(info) => {
                consecutive_errors = 0;
                info!(endpoint = %info.endpoint, "DingTalk stream registered successfully");
                info
            }
            Err(e) => {
                consecutive_errors += 1;
                warn!(error = %e, consecutive_errors, "DingTalk stream registration failed");
                if consecutive_errors >= MAX_RECONNECT_ATTEMPTS {
                    error!("DingTalk max reconnect attempts reached");
                    break;
                }
                let delay = backoff_delay(consecutive_errors);
                tokio::select! {
                    _ = tokio::time::sleep(delay) => continue,
                    _ = shutdown_rx.changed() => break,
                }
            }
        };

        let ws_url = format!("{}?ticket={}", stream_info.endpoint, stream_info.ticket);

        info!(url = %ws_url, "Connecting to DingTalk WebSocket Stream");

        match connect_and_listen(&ws_url, &message_tx, &confirm_tx, &mut shutdown_rx).await {
            Ok(()) => {
                debug!("DingTalk WS connection closed cleanly");
                break;
            }
            Err(e) => {
                consecutive_errors += 1;
                warn!(
                    error = %e,
                    consecutive_errors,
                    "DingTalk WS connection error"
                );
                if consecutive_errors >= MAX_RECONNECT_ATTEMPTS {
                    error!("DingTalk max reconnect attempts reached");
                    break;
                }
                let delay = backoff_delay(consecutive_errors);
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = shutdown_rx.changed() => break,
                }
            }
        }
    }

    // The loop exits on shutdown, reconnect exhaustion, or a server-side
    // clean close (which the loop does not currently re-dial). For any
    // non-shutdown exit the connection is gone while the facade still says
    // Running — flip the shared status to Error so the manager watchdog can
    // persist/broadcast the real state and attempt a restart.
    mark_error_on_unexpected_exit(&status, &shutdown_rx, "dingtalk");

    debug!("DingTalk WS loop exited");
}

/// Build a TLS connector for WebSocket connections.
///
/// Explicitly sets ALPN to `http/1.1` only — WebSocket requires an HTTP/1.1
/// upgrade handshake and is incompatible with h2. Without this, some servers
/// negotiate h2 via ALPN and the WebSocket upgrade never completes.
fn build_ws_tls_connector() -> Result<tokio_tungstenite::Connector, ChannelError> {
    use std::sync::Arc;
    use tokio_tungstenite::Connector;

    let certs = rustls_native_certs::load_native_certs();
    let mut root_store = rustls::RootCertStore::empty();
    root_store.add_parsable_certificates(certs.certs);

    let provider = rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::ring::default_provider()));

    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| ChannelError::ConnectionFailed(format!("TLS config error: {e}")))?
        .with_root_certificates(root_store)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];

    Ok(Connector::Rustls(Arc::new(config)))
}

/// Connect to the WebSocket and listen for frames until disconnected.
async fn connect_and_listen(
    ws_url: &str,
    message_tx: &mpsc::Sender<UnifiedIncomingMessage>,
    confirm_tx: &mpsc::Sender<(String, String)>,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> Result<(), ChannelError> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::connect_async_tls_with_config;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let connector = build_ws_tls_connector()?;
    let (ws_stream, _) = connect_async_tls_with_config(ws_url, None, false, Some(connector))
        .await
        .map_err(|e| ChannelError::ConnectionFailed(format!("DingTalk WS connect failed: {e}")))?;

    info!("DingTalk WebSocket Stream connected");

    let (mut write, mut read) = ws_stream.split();

    loop {
        tokio::select! {
            msg = read.next() => {
                match msg {
                    Some(Ok(WsMessage::Text(text))) => {
                        if let Some(ack) = handle_stream_frame(
                            &text,
                            message_tx,
                            confirm_tx,
                        ).await {
                            let ack_json = serde_json::to_string(&ack)
                                .unwrap_or_default();
                            if let Err(e) = write.send(WsMessage::Text(ack_json.into())).await {
                                warn!(error = %e, "Failed to send DingTalk stream ack");
                            }
                        }
                    }
                    Some(Ok(WsMessage::Ping(data))) => {
                        if let Err(e) = write.send(WsMessage::Pong(data)).await {
                            warn!(error = %e, "Failed to send DingTalk pong");
                        }
                    }
                    Some(Ok(WsMessage::Close(_))) => {
                        debug!("DingTalk WS received close frame");
                        return Ok(());
                    }
                    Some(Err(e)) => {
                        return Err(ChannelError::ConnectionFailed(
                            format!("DingTalk WS read error: {e}")
                        ));
                    }
                    None => {
                        return Err(ChannelError::ConnectionFailed(
                            "DingTalk WS stream ended unexpectedly".into()
                        ));
                    }
                    _ => {} // Binary, Frame — ignore
                }
            }
            _ = shutdown_rx.changed() => {
                debug!("DingTalk WS shutdown during listen");
                return Ok(());
            }
        }
    }
}

/// Handle a stream frame and optionally return an acknowledgment.
async fn handle_stream_frame(
    text: &str,
    message_tx: &mpsc::Sender<UnifiedIncomingMessage>,
    confirm_tx: &mpsc::Sender<(String, String)>,
) -> Option<StreamAck> {
    let frame: StreamFrame = match serde_json::from_str(text) {
        Ok(f) => f,
        Err(e) => {
            warn!(error = %e, "Failed to parse DingTalk stream frame");
            return None;
        }
    };

    let message_id = frame.headers.message_id.clone().unwrap_or_default();

    match frame.frame_type.as_str() {
        "SYSTEM" => {
            let topic = frame.headers.topic.as_deref().unwrap_or("");
            match topic {
                "ping" => {
                    debug!("DingTalk system ping received, sending pong");
                    Some(StreamAck {
                        code: 200,
                        headers: super::types::AckHeaders {
                            content_type: "application/json".into(),
                            message_id: message_id.clone(),
                        },
                        message: "OK".into(),
                        data: frame.data.unwrap_or_else(|| "{}".into()),
                    })
                }
                _ => {
                    if let Some(ref data_str) = frame.data
                        && let Ok(sys) = serde_json::from_str::<SystemEvent>(data_str)
                    {
                        debug!(
                            code = sys.code,
                            message = sys.message.as_deref().unwrap_or(""),
                            topic,
                            "DingTalk system event"
                        );
                    }
                    None
                }
            }
        }
        "CALLBACK" => {
            let topic = frame.headers.topic.as_deref().unwrap_or("");
            let data_str = frame.data.as_deref().unwrap_or("");

            match topic {
                "/v1.0/im/bot/messages/get" => {
                    handle_bot_message(data_str, message_tx).await;
                }
                "/v1.0/card/instances/callback" => {
                    handle_card_action(data_str, message_tx, confirm_tx).await;
                }
                _ => {
                    debug!(topic, "DingTalk unhandled callback topic");
                }
            }

            // Always ack CALLBACK frames
            Some(build_ack(&message_id))
        }
        "EVENT" => {
            // Ack event frames
            Some(build_ack(&message_id))
        }
        other => {
            debug!(frame_type = other, "DingTalk unhandled stream frame type");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Event handlers
// ---------------------------------------------------------------------------

/// Handle a bot message callback.
async fn handle_bot_message(data_str: &str, message_tx: &mpsc::Sender<UnifiedIncomingMessage>) {
    let cb: BotMessageCallback = match serde_json::from_str(data_str) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, raw = %&data_str[..data_str.len().min(200)], "Failed to parse DingTalk bot message");
            return;
        }
    };

    debug!(
        msg_id = cb.msg_id.as_deref().unwrap_or(""),
        sender = cb.sender_nick.as_deref().unwrap_or(""),
        msgtype = cb.msgtype.as_deref().unwrap_or(""),
        "DingTalk bot message received"
    );

    let sender_staff_id = cb
        .sender_staff_id
        .as_deref()
        .or(cb.sender_id.as_deref())
        .unwrap_or("unknown");

    let chat_id = encode_chat_id(
        cb.conversation_type.as_deref(),
        cb.conversation_id.as_deref(),
        sender_staff_id,
    );

    let user = UnifiedUser {
        id: sender_staff_id.to_string(),
        username: None,
        display_name: cb.sender_nick.clone().unwrap_or_default(),
        avatar_url: None,
    };

    let (content_type, text) = extract_message_content(cb.msgtype.as_deref().unwrap_or("text"), &cb);

    let timestamp = cb.create_at.map(|ms| ms / 1000).unwrap_or_else(chrono_now);

    let unified = UnifiedIncomingMessage {
        id: cb.msg_id.clone().unwrap_or_default(),
        platform: PluginType::Dingtalk,
        chat_id,
        user,
        content: UnifiedMessageContent {
            content_type,
            text,
            attachments: None,
        },
        timestamp,
        reply_to_message_id: None,
        action: None,
        raw: None,
    };

    let _ = message_tx.send(unified).await;
}

/// Handle a card action callback.
async fn handle_card_action(
    data_str: &str,
    message_tx: &mpsc::Sender<UnifiedIncomingMessage>,
    confirm_tx: &mpsc::Sender<(String, String)>,
) {
    let cb: CardActionCallback = match serde_json::from_str(data_str) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "Failed to parse DingTalk card action");
            return;
        }
    };

    debug!(
        user_id = cb.user_id.as_deref().unwrap_or(""),
        "DingTalk card action received"
    );

    // Extract action string from content field
    let action_str = cb
        .content
        .as_deref()
        .and_then(|c| serde_json::from_str::<serde_json::Value>(c).ok())
        .and_then(|v| v.get("action").and_then(|a| a.as_str()).map(String::from))
        .unwrap_or_default();

    let parsed = parse_dingtalk_callback(&action_str);

    // Check if this is a tool confirmation
    if let Some((_, ref action, ref params)) = parsed
        && action == "system.confirm"
        && let Some(p) = params
    {
        let call_id = p.get("callId").cloned().unwrap_or_default();
        let value = p.get("value").cloned().unwrap_or_default();
        if !call_id.is_empty() {
            let _ = confirm_tx.send((call_id, value)).await;
        }
    }

    let user_id = cb.user_id.clone().unwrap_or_default();
    let chat_id = match cb.open_conversation_id.as_deref() {
        Some(cid) if !cid.is_empty() => format!("group:{cid}"),
        _ => format!("user:{}", user_id),
    };

    let user = UnifiedUser {
        id: user_id.clone(),
        username: None,
        display_name: user_id.clone(),
        avatar_url: None,
    };

    let unified_action = parsed.map(|(cat_str, action, params)| {
        let category = match cat_str.as_str() {
            "platform" => ActionCategory::Platform,
            "chat" => ActionCategory::Chat,
            _ => ActionCategory::System,
        };
        UnifiedAction {
            action,
            category,
            params,
            context: ActionContext {
                platform: PluginType::Dingtalk,
                user_id: user_id.clone(),
                chat_id: chat_id.clone(),
                message_id: None,
                session_id: None,
            },
        }
    });

    let msg = UnifiedIncomingMessage {
        id: format!("card_{}", chrono_now()),
        platform: PluginType::Dingtalk,
        chat_id,
        user,
        content: UnifiedMessageContent {
            content_type: MessageContentType::Action,
            text: action_str,
            attachments: None,
        },
        timestamp: chrono_now(),
        reply_to_message_id: None,
        action: unified_action,
        raw: None,
    };

    let _ = message_tx.send(msg).await;
}

// ---------------------------------------------------------------------------
// Message content extraction
// ---------------------------------------------------------------------------

/// Extract content type and text from a DingTalk bot message callback.
fn extract_message_content(msgtype: &str, cb: &BotMessageCallback) -> (MessageContentType, String) {
    match msgtype {
        "text" => {
            let text = cb
                .text
                .as_ref()
                .and_then(|t| t.content.as_deref())
                .unwrap_or("")
                .to_string();

            if text.starts_with('/') {
                (MessageContentType::Command, text)
            } else {
                (MessageContentType::Text, text)
            }
        }
        "picture" => (MessageContentType::Photo, "[Picture]".to_string()),
        "file" => (MessageContentType::Document, "[File]".to_string()),
        "audio" => (MessageContentType::Audio, "[Audio]".to_string()),
        "video" => (MessageContentType::Video, "[Video]".to_string()),
        _ => (
            MessageContentType::Text,
            format!("[Unsupported message type: {msgtype}]"),
        ),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a stream acknowledgment response.
fn build_ack(message_id: &str) -> StreamAck {
    StreamAck {
        code: 200,
        headers: super::types::AckHeaders {
            content_type: "application/json".into(),
            message_id: message_id.to_string(),
        },
        message: "OK".into(),
        data: r#"{"response":"SUCCESS"}"#.into(),
    }
}

/// Truncate a message to the platform limit, appending "..." if truncated.
fn truncate_message(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let truncated: String = text.chars().take(limit - 3).collect();
    format!("{truncated}...")
}

/// Calculate exponential backoff delay, capped at the maximum.
fn backoff_delay(attempt: u32) -> Duration {
    let delay_secs = 2u64.saturating_pow(attempt).min(MAX_RECONNECT_DELAY.as_secs());
    Duration::from_secs(delay_secs)
}

/// Current unix timestamp in seconds.
fn chrono_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- truncate_message ---------------------------------------------------

    #[test]
    fn truncate_within_limit() {
        assert_eq!(truncate_message("Hello", 100), "Hello");
    }

    #[test]
    fn truncate_at_limit() {
        assert_eq!(truncate_message("abc", 3), "abc");
    }

    #[test]
    fn truncate_exceeds_limit() {
        let result = truncate_message("Hello, world!", 10);
        assert_eq!(result, "Hello, ...");
    }

    #[test]
    fn truncate_unicode() {
        let result = truncate_message("你好世界测试文本", 5);
        assert_eq!(result, "你好...");
    }

    // -- backoff_delay ------------------------------------------------------

    #[test]
    fn backoff_exponential() {
        assert_eq!(backoff_delay(1), Duration::from_secs(2));
        assert_eq!(backoff_delay(2), Duration::from_secs(4));
        assert_eq!(backoff_delay(3), Duration::from_secs(8));
    }

    #[test]
    fn backoff_capped() {
        assert_eq!(backoff_delay(5), Duration::from_secs(30));
        assert_eq!(backoff_delay(10), Duration::from_secs(30));
    }

    // -- extract_message_content --------------------------------------------

    #[test]
    fn extract_text_content() {
        let cb = BotMessageCallback {
            conversation_id: None,
            msg_id: None,
            msgtype: Some("text".into()),
            text: Some(super::super::types::TextPayload {
                content: Some("Hello".into()),
            }),
            sender_id: None,
            sender_nick: None,
            sender_staff_id: None,
            session_webhook: None,
            session_webhook_expired_time: None,
            conversation_type: None,
            is_in_at_list: None,
            at_users: None,
            create_at: None,
            robot_code: None,
        };
        let (ct, text) = extract_message_content("text", &cb);
        assert_eq!(ct, MessageContentType::Text);
        assert_eq!(text, "Hello");
    }

    #[test]
    fn extract_text_command() {
        let cb = BotMessageCallback {
            conversation_id: None,
            msg_id: None,
            msgtype: Some("text".into()),
            text: Some(super::super::types::TextPayload {
                content: Some("/start".into()),
            }),
            sender_id: None,
            sender_nick: None,
            sender_staff_id: None,
            session_webhook: None,
            session_webhook_expired_time: None,
            conversation_type: None,
            is_in_at_list: None,
            at_users: None,
            create_at: None,
            robot_code: None,
        };
        let (ct, text) = extract_message_content("text", &cb);
        assert_eq!(ct, MessageContentType::Command);
        assert_eq!(text, "/start");
    }

    #[test]
    fn extract_picture_content() {
        let cb = BotMessageCallback {
            conversation_id: None,
            msg_id: None,
            msgtype: Some("picture".into()),
            text: None,
            sender_id: None,
            sender_nick: None,
            sender_staff_id: None,
            session_webhook: None,
            session_webhook_expired_time: None,
            conversation_type: None,
            is_in_at_list: None,
            at_users: None,
            create_at: None,
            robot_code: None,
        };
        let (ct, _) = extract_message_content("picture", &cb);
        assert_eq!(ct, MessageContentType::Photo);
    }

    #[test]
    fn extract_unsupported_type() {
        let cb = BotMessageCallback {
            conversation_id: None,
            msg_id: None,
            msgtype: Some("richText".into()),
            text: None,
            sender_id: None,
            sender_nick: None,
            sender_staff_id: None,
            session_webhook: None,
            session_webhook_expired_time: None,
            conversation_type: None,
            is_in_at_list: None,
            at_users: None,
            create_at: None,
            robot_code: None,
        };
        let (ct, text) = extract_message_content("richText", &cb);
        assert_eq!(ct, MessageContentType::Text);
        assert!(text.contains("Unsupported"));
    }

    // -- build_final_card_param_map -------------------------------------------

    #[test]
    fn build_final_card_param_map_text_only() {
        let map = build_final_card_param_map("Hello", None);
        assert_eq!(map["flowStatus"], "3");
        assert_eq!(map["msgContent"], "Hello");
        assert!(map.get("actions").is_none());
    }

    #[test]
    fn build_final_card_param_map_with_buttons() {
        use crate::types::ActionButton;
        let buttons = vec![vec![ActionButton {
            label: "Yes".into(),
            action: "system.confirm".into(),
            params: None,
        }]];
        let map = build_final_card_param_map("Choose:", Some(&buttons));
        assert_eq!(map["flowStatus"], "3");
        assert_eq!(map["msgContent"], "Choose:");
        let actions = map["actions"].as_array().unwrap();
        assert_eq!(actions[0]["label"], "Yes");
        assert!(actions[0]["action"].as_str().unwrap().contains("system.confirm"));
    }

    // -- build_ack ----------------------------------------------------------

    #[test]
    fn build_ack_structure() {
        let ack = build_ack("msg_123");
        assert_eq!(ack.code, 200);
        assert_eq!(ack.headers.message_id, "msg_123");
        assert_eq!(ack.message, "OK");
        assert_eq!(ack.data, r#"{"response":"SUCCESS"}"#);
    }

    #[test]
    fn build_ack_data_contains_response_success() {
        let ack = build_ack("msg_456");
        assert_eq!(ack.code, 200);
        assert_eq!(ack.headers.message_id, "msg_456");
        let data: serde_json::Value = serde_json::from_str(&ack.data).unwrap();
        assert_eq!(data["response"], "SUCCESS");
    }

    // -- build_final_card_param_map for update (empty text, buttons only) ----

    #[test]
    fn build_final_card_param_map_empty_text_with_buttons() {
        use crate::types::ActionButton;
        let buttons = vec![vec![ActionButton {
            label: "Confirm".into(),
            action: "system.confirm".into(),
            params: None,
        }]];
        let map = build_final_card_param_map("", Some(&buttons));
        assert_eq!(map["flowStatus"], "3");
        assert_eq!(map["msgContent"], "");
        let actions = map["actions"].as_array().unwrap();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0]["label"], "Confirm");
    }

    // -- edit_message: not initialized guard -----------------------------------

    #[tokio::test]
    async fn edit_message_not_initialized_returns_error() {
        let plugin = DingtalkPlugin::new();
        let msg = UnifiedOutgoingMessage {
            message_type: crate::types::OutgoingMessageType::Text,
            text: Some("hello".into()),
            parse_mode: None,
            buttons: None,
            keyboard: None,
            image_url: None,
            file_url: None,
            file_name: None,
            media_actions: None,
            reply_to_message_id: None,
            silent: None,
        };
        let result = plugin.edit_message("chat1", "msg1", msg).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("not initialized"), "expected init error: {err}");
    }

    // -- send_message: not initialized guard -----------------------------------

    #[tokio::test]
    async fn send_message_not_initialized_returns_error() {
        let plugin = DingtalkPlugin::new();
        let msg = UnifiedOutgoingMessage {
            message_type: crate::types::OutgoingMessageType::Text,
            text: Some("hello".into()),
            parse_mode: None,
            buttons: None,
            keyboard: None,
            image_url: None,
            file_url: None,
            file_name: None,
            media_actions: None,
            reply_to_message_id: None,
            silent: None,
        };
        let result = plugin.send_message("chat1", msg).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("not initialized"), "expected init error: {err}");
    }

    // -- handle_stream_frame: SYSTEM ping -----------------------------------

    #[tokio::test]
    async fn handle_stream_frame_system_ping_returns_ack() {
        let (msg_tx, _msg_rx) = tokio::sync::mpsc::channel(16);
        let (confirm_tx, _confirm_rx) = tokio::sync::mpsc::channel(16);

        let ping_frame = serde_json::json!({
            "type": "SYSTEM",
            "headers": {
                "contentType": "application/json",
                "messageId": "ping_001",
                "topic": "ping"
            },
            "data": "{}"
        });

        let result = handle_stream_frame(&ping_frame.to_string(), &msg_tx, &confirm_tx).await;

        assert!(result.is_some(), "SYSTEM ping should return an ack");
        let ack = result.unwrap();
        assert_eq!(ack.code, 200);
        assert_eq!(ack.headers.message_id, "ping_001");
    }

    #[tokio::test]
    async fn handle_stream_frame_system_connected_returns_none() {
        let (msg_tx, _msg_rx) = tokio::sync::mpsc::channel(16);
        let (confirm_tx, _confirm_rx) = tokio::sync::mpsc::channel(16);

        let connected_frame = serde_json::json!({
            "type": "SYSTEM",
            "headers": { "topic": "CONNECTED" },
            "data": "{\"code\":200,\"message\":\"OK\"}"
        });

        let result = handle_stream_frame(&connected_frame.to_string(), &msg_tx, &confirm_tx).await;

        assert!(result.is_none(), "Non-ping SYSTEM frames should not return ack");
    }

    // -- handle_stream_frame: CALLBACK flow ---------------------------------

    #[tokio::test]
    async fn handle_stream_frame_callback_emits_message() {
        let (msg_tx, mut msg_rx) = tokio::sync::mpsc::channel(16);
        let (confirm_tx, _confirm_rx) = tokio::sync::mpsc::channel(16);

        let callback_frame = serde_json::json!({
            "type": "CALLBACK",
            "headers": {
                "contentType": "application/json",
                "messageId": "cb_msg_001",
                "topic": "/v1.0/im/bot/messages/get"
            },
            "data": serde_json::json!({
                "msgId": "dt_msg_123",
                "msgtype": "text",
                "text": { "content": "hello bot" },
                "senderStaffId": "staff_abc",
                "senderNick": "Alice",
                "conversationType": "1",
                "createAt": 1700000000000_i64
            }).to_string()
        });

        let result = handle_stream_frame(&callback_frame.to_string(), &msg_tx, &confirm_tx).await;

        assert!(result.is_some());
        let ack = result.unwrap();
        assert_eq!(ack.code, 200);
        assert_eq!(ack.headers.message_id, "cb_msg_001");

        let msg = msg_rx.try_recv().unwrap();
        assert_eq!(msg.id, "dt_msg_123");
        assert_eq!(msg.chat_id, "user:staff_abc");
        assert_eq!(msg.content.text, "hello bot");
        assert_eq!(msg.user.display_name, "Alice");
        assert_eq!(msg.platform, PluginType::Dingtalk);
    }

    #[tokio::test]
    async fn handle_stream_frame_card_action_emits_confirm() {
        let (msg_tx, _msg_rx) = tokio::sync::mpsc::channel(16);
        let (confirm_tx, mut confirm_rx) = tokio::sync::mpsc::channel(16);

        let card_frame = serde_json::json!({
            "type": "CALLBACK",
            "headers": {
                "contentType": "application/json",
                "messageId": "cb_card_001",
                "topic": "/v1.0/card/instances/callback"
            },
            "data": serde_json::json!({
                "userId": "user_xyz",
                "openConversationId": "",
                "content": r#"{"action":"chat:system.confirm:callId=call_123,value=yes"}"#
            }).to_string()
        });

        let result = handle_stream_frame(&card_frame.to_string(), &msg_tx, &confirm_tx).await;

        assert!(result.is_some());

        let (call_id, value) = confirm_rx.try_recv().unwrap();
        assert_eq!(call_id, "call_123");
        assert_eq!(value, "yes");
    }

    // -- DingtalkPlugin constructor -----------------------------------------

    #[test]
    fn new_plugin_initial_state() {
        let plugin = DingtalkPlugin::new();
        assert_eq!(plugin.status(), PluginStatus::Created);
        assert!(plugin.bot_info().is_none());
        assert!(plugin.last_error().is_none());
        assert_eq!(plugin.plugin_type(), PluginType::Dingtalk);
        assert_eq!(plugin.active_user_count(), 0);
    }
}
