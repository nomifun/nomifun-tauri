//! Nostr channel plugin — NIP-04 encrypted DMs over relay WebSockets.
//!
//! Fully outbound: connects to relays, subscribes to kind-4 events tagged
//! with the bot's pubkey, decrypts inbound DMs, encrypts + signs outbound
//! replies. One combined task manages all relays via a select loop over
//! individual per-relay connections (each in its own subtask).

use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use nostr::Keys;
use tokio::sync::{mpsc, watch, Mutex};
use tracing::{debug, error, info, warn};

use crate::constants::{NOSTR_MAX_RECONNECT_ATTEMPTS, NOSTR_MAX_RECONNECT_DELAY};
use crate::error::ChannelError;
use crate::plugin::{ChannelPlugin, PluginCallbacks, SharedPluginStatus, mark_error_on_unexpected_exit};
use crate::types::{
    BotInfo, MessageContentType, PluginConfig, PluginStatus, PluginType, UnifiedIncomingMessage,
    UnifiedMessageContent, UnifiedOutgoingMessage, UnifiedUser,
};

use super::crypto;
use super::types::{self, RawEvent, RelayMessage};

/// Maximum number of seen event IDs to track for deduplication.
const SEEN_EVENT_CAP: usize = 10_000;

/// Subscription ID prefix used for the DM subscription.
const SUB_ID_PREFIX: &str = "nomi-dm-";

pub struct NostrPlugin {
    keys: Option<Keys>,
    bot_pubkey_hex: String,
    relay_urls: Vec<String>,
    status: SharedPluginStatus,
    bot_info: Option<BotInfo>,
    last_error: Option<String>,
    callbacks: Option<PluginCallbacks>,
    shutdown_tx: Option<watch::Sender<bool>>,
    /// Shared across relay tasks for cross-relay dedup.
    seen_events: Arc<DashMap<String, ()>>,
    /// Relay connections for publishing events.
    relay_writers: Arc<Mutex<Vec<RelaySender>>>,
}

/// A handle to send messages to one relay's WebSocket.
struct RelaySender {
    url: String,
    tx: mpsc::Sender<String>,
}

impl NostrPlugin {
    pub fn new() -> Self {
        Self {
            keys: None,
            bot_pubkey_hex: String::new(),
            relay_urls: Vec::new(),
            status: SharedPluginStatus::default(),
            bot_info: None,
            last_error: None,
            callbacks: None,
            shutdown_tx: None,
            seen_events: Arc::new(DashMap::new()),
            relay_writers: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn set_error(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        error!(error = %msg, "Nostr plugin error");
        self.last_error = Some(msg);
        self.status.set(PluginStatus::Error);
    }
}

#[async_trait::async_trait]
impl ChannelPlugin for NostrPlugin {
    async fn initialize(
        &mut self,
        config: PluginConfig,
        callbacks: PluginCallbacks,
    ) -> Result<(), ChannelError> {
        self.status.set(PluginStatus::Initializing);

        // Parse private key.
        let sk_raw = config
            .credentials
            .nostr_private_key
            .as_deref()
            .ok_or_else(|| ChannelError::InvalidConfig("missing nostr_private_key".into()))?;

        let sk = match crypto::parse_secret_key(sk_raw) {
            Ok(sk) => sk,
            Err(e) => {
                self.set_error(format!("invalid private key: {e}"));
                return Err(e);
            }
        };

        let keys = Keys::new(sk);
        let pubkey = keys.public_key();
        let pubkey_hex = pubkey.to_hex();
        let npub = crypto::pubkey_to_npub(&pubkey);
        let short_pk = format!("{}...{}", &pubkey_hex[..8], &pubkey_hex[pubkey_hex.len() - 4..]);

        self.keys = Some(keys);
        self.bot_pubkey_hex = pubkey_hex.clone();
        self.relay_urls = crypto::parse_relay_urls(config.credentials.nostr_relays.as_deref());
        self.bot_info = Some(BotInfo {
            id: pubkey_hex,
            username: Some(npub),
            display_name: short_pk,
        });
        self.callbacks = Some(callbacks);

        self.status.set(PluginStatus::Ready);
        info!("Nostr plugin initialized, pubkey = {}", self.bot_info.as_ref().unwrap().id);
        Ok(())
    }

    async fn start(&mut self) -> Result<(), ChannelError> {
        self.status.set(PluginStatus::Starting);

        let keys = self.keys.clone().ok_or_else(|| {
            ChannelError::InvalidConfig("Nostr plugin not initialized".into())
        })?;
        let callbacks = self.callbacks.clone().ok_or_else(|| {
            ChannelError::InvalidConfig("Nostr plugin callbacks not set".into())
        })?;

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        self.shutdown_tx = Some(shutdown_tx);

        let bot_pk_hex = self.bot_pubkey_hex.clone();
        let relay_urls = self.relay_urls.clone();
        let status = self.status.clone();
        let seen_events = self.seen_events.clone();
        let relay_writers = self.relay_writers.clone();

        // Spawn one task per relay.
        for relay_url in &relay_urls {
            let (writer_tx, writer_rx) = mpsc::channel::<String>(64);

            // Register writer.
            {
                let mut writers = relay_writers.lock().await;
                writers.push(RelaySender {
                    url: relay_url.clone(),
                    tx: writer_tx,
                });
            }

            let url = relay_url.clone();
            let keys_c = keys.clone();
            let pk_hex = bot_pk_hex.clone();
            let msg_tx = callbacks.message_tx.clone();
            let status_c = status.clone();
            let seen_c = seen_events.clone();
            let mut shutdown_c = shutdown_rx.clone();

            tokio::spawn(async move {
                run_relay_loop(
                    &url,
                    &keys_c,
                    &pk_hex,
                    msg_tx,
                    writer_rx,
                    seen_c,
                    status_c.clone(),
                    &mut shutdown_c,
                )
                .await;
                mark_error_on_unexpected_exit(&status_c, &shutdown_c, &format!("nostr-relay:{url}"));
            });
        }

        self.status.set(PluginStatus::Running);
        info!("Nostr plugin started with {} relay(s)", relay_urls.len());
        Ok(())
    }

    async fn stop(&mut self) -> Result<(), ChannelError> {
        self.status.set(PluginStatus::Stopping);
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(true);
        }
        // Drop relay writers.
        {
            let mut writers = self.relay_writers.lock().await;
            writers.clear();
        }
        self.status.set(PluginStatus::Stopped);
        info!("Nostr plugin stopped");
        Ok(())
    }

    async fn send_message(
        &self,
        chat_id: &str,
        message: UnifiedOutgoingMessage,
    ) -> Result<String, ChannelError> {
        let keys = self
            .keys
            .as_ref()
            .ok_or_else(|| ChannelError::MessageSendFailed("Nostr plugin not initialized".into()))?;

        let text = message
            .text
            .as_deref()
            .unwrap_or("")
            .to_owned();

        if text.is_empty() {
            return Err(ChannelError::MessageSendFailed("empty message text".into()));
        }

        // chat_id = recipient pubkey hex.
        let recipient_pk = crypto::parse_pubkey_hex(chat_id)?;
        let (event_id, event_json) = crypto::build_dm_event(keys, &recipient_pk, &text)?;

        // Publish to all relays.
        let publish_msg = types::build_event_message(&event_json)
            .map_err(|e| ChannelError::MessageSendFailed(format!("event message build failed: {e}")))?;

        let writers = self.relay_writers.lock().await;
        let mut sent_count = 0;
        for writer in writers.iter() {
            match writer.tx.try_send(publish_msg.clone()) {
                Ok(()) => sent_count += 1,
                Err(e) => warn!(relay = %writer.url, error = %e, "failed to queue event to relay"),
            }
        }

        if sent_count == 0 && !writers.is_empty() {
            return Err(ChannelError::MessageSendFailed(
                "failed to queue event to any relay".into(),
            ));
        }

        debug!(event_id = %event_id, sent_count, "Nostr DM published");
        Ok(event_id)
    }

    async fn edit_message(
        &self,
        chat_id: &str,
        _message_id: &str,
        message: UnifiedOutgoingMessage,
    ) -> Result<(), ChannelError> {
        // Nostr events are immutable — send a new message as fallback.
        self.send_message(chat_id, message).await?;
        Ok(())
    }

    fn active_user_count(&self) -> usize {
        0
    }

    fn bot_info(&self) -> Option<&BotInfo> {
        self.bot_info.as_ref()
    }

    fn plugin_type(&self) -> PluginType {
        PluginType::Nostr
    }

    fn status(&self) -> PluginStatus {
        self.status.get()
    }

    fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }
}

// ---------------------------------------------------------------------------
// Relay connection loop
// ---------------------------------------------------------------------------

/// Maintain a WebSocket connection to a single relay with reconnect backoff.
async fn run_relay_loop(
    relay_url: &str,
    keys: &Keys,
    bot_pk_hex: &str,
    message_tx: mpsc::Sender<UnifiedIncomingMessage>,
    mut writer_rx: mpsc::Receiver<String>,
    seen_events: Arc<DashMap<String, ()>>,
    _status: SharedPluginStatus,
    shutdown_rx: &mut watch::Receiver<bool>,
) {
    let mut consecutive_errors: u32 = 0;

    loop {
        if *shutdown_rx.borrow() {
            debug!(relay = %relay_url, "relay loop received shutdown signal");
            break;
        }

        match connect_relay_once(
            relay_url,
            keys,
            bot_pk_hex,
            &message_tx,
            &mut writer_rx,
            &seen_events,
            shutdown_rx,
        )
        .await
        {
            Ok(()) => {
                consecutive_errors = 0;
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            Err(e) => {
                consecutive_errors += 1;
                warn!(relay = %relay_url, error = %e, attempt = consecutive_errors, "relay connection error");
                if consecutive_errors >= NOSTR_MAX_RECONNECT_ATTEMPTS {
                    error!(relay = %relay_url, "max reconnect attempts reached");
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
}

fn backoff_delay(attempt: u32) -> Duration {
    let secs = 2u64.saturating_pow(attempt).min(NOSTR_MAX_RECONNECT_DELAY.as_secs());
    Duration::from_secs(secs)
}

/// A single relay connection: connect, subscribe, read/write loop.
async fn connect_relay_once(
    relay_url: &str,
    keys: &Keys,
    bot_pk_hex: &str,
    message_tx: &mpsc::Sender<UnifiedIncomingMessage>,
    writer_rx: &mut mpsc::Receiver<String>,
    seen_events: &Arc<DashMap<String, ()>>,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> Result<(), ChannelError> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::connect_async_tls_with_config;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let connector = build_ws_tls_connector()?;
    let (ws_stream, _) =
        connect_async_tls_with_config(relay_url, None, false, Some(connector))
            .await
            .map_err(|e| {
                ChannelError::ConnectionFailed(format!(
                    "Nostr relay connect failed ({relay_url}): {e}"
                ))
            })?;
    info!(relay = %relay_url, "connected to Nostr relay");

    let (mut write, mut read) = ws_stream.split();

    // Subscribe: REQ for kind-4 DMs addressed to us.
    let sub_id = format!("{SUB_ID_PREFIX}{}", &bot_pk_hex[..8]);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let req_msg = types::build_req_message(&sub_id, bot_pk_hex, now);
    write
        .send(WsMessage::Text(req_msg.into()))
        .await
        .map_err(|e| ChannelError::ConnectionFailed(format!("REQ send failed: {e}")))?;
    debug!(relay = %relay_url, sub_id = %sub_id, "subscribed to DMs");

    loop {
        tokio::select! {
            frame = read.next() => {
                match frame {
                    Some(Ok(WsMessage::Text(txt))) => {
                        handle_relay_frame(
                            &txt,
                            keys,
                            bot_pk_hex,
                            message_tx,
                            seen_events,
                            relay_url,
                        ).await;
                    }
                    Some(Ok(WsMessage::Ping(data))) => {
                        let _ = write.send(WsMessage::Pong(data)).await;
                    }
                    Some(Ok(WsMessage::Close(_))) => {
                        info!(relay = %relay_url, "relay sent close frame");
                        return Ok(());
                    }
                    Some(Err(e)) => {
                        return Err(ChannelError::ConnectionFailed(
                            format!("relay read error ({relay_url}): {e}"),
                        ));
                    }
                    None => {
                        info!(relay = %relay_url, "relay connection ended");
                        return Ok(());
                    }
                    _ => {} // Binary, Pong, Frame — ignore.
                }
            }
            outgoing = writer_rx.recv() => {
                match outgoing {
                    Some(msg) => {
                        if let Err(e) = write.send(WsMessage::Text(msg.into())).await {
                            warn!(relay = %relay_url, error = %e, "failed to send event to relay");
                        }
                    }
                    None => {
                        // Channel closed — plugin is stopping.
                        debug!(relay = %relay_url, "writer channel closed");
                        break;
                    }
                }
            }
            _ = shutdown_rx.changed() => {
                debug!(relay = %relay_url, "shutdown signal received");
                // Send CLOSE to be polite.
                let close_msg = types::build_close_message(&sub_id);
                let _ = write.send(WsMessage::Text(close_msg.into())).await;
                break;
            }
        }
    }

    Ok(())
}

/// Handle a single text frame from a relay.
async fn handle_relay_frame(
    raw: &str,
    keys: &Keys,
    bot_pk_hex: &str,
    message_tx: &mpsc::Sender<UnifiedIncomingMessage>,
    seen_events: &Arc<DashMap<String, ()>>,
    relay_url: &str,
) {
    let msg = match RelayMessage::parse(raw) {
        Some(m) => m,
        None => {
            debug!(relay = %relay_url, "unparseable relay frame");
            return;
        }
    };

    match msg {
        RelayMessage::Event {
            subscription_id: _,
            event,
        } => {
            handle_dm_event(event, keys, bot_pk_hex, message_tx, seen_events, relay_url).await;
        }
        RelayMessage::EndOfStoredEvents { subscription_id } => {
            debug!(relay = %relay_url, sub = %subscription_id, "EOSE received");
        }
        RelayMessage::Ok {
            event_id,
            success,
            message,
        } => {
            if success {
                debug!(relay = %relay_url, event_id = %event_id, "event accepted");
            } else {
                warn!(relay = %relay_url, event_id = %event_id, msg = %message, "event rejected");
            }
        }
        RelayMessage::Notice { message } => {
            info!(relay = %relay_url, notice = %message, "relay NOTICE");
        }
        RelayMessage::Unknown(_) => {}
    }
}

/// Process a kind-4 DM event: dedup, self-loop guard, decrypt, emit.
async fn handle_dm_event(
    event: RawEvent,
    keys: &Keys,
    bot_pk_hex: &str,
    message_tx: &mpsc::Sender<UnifiedIncomingMessage>,
    seen_events: &Arc<DashMap<String, ()>>,
    relay_url: &str,
) {
    // The signed event ID is the stable cross-relay identity.  Validate it
    // before even touching the in-memory dedup set so malformed relay input
    // cannot create side effects or reach the durable inbound boundary.
    if event.id.trim().is_empty() {
        warn!(relay = %relay_url, "Ignoring Nostr DM without a provider event ID");
        return;
    }

    // Must be a DM.
    if !event.is_dm() {
        return;
    }

    // Must be addressed to us.
    if !event.has_p_tag(bot_pk_hex) {
        return;
    }

    // Self-loop guard: skip messages from ourselves.
    if event.pubkey == bot_pk_hex {
        debug!(relay = %relay_url, "skipping self-sent DM");
        return;
    }

    // Dedup across relays.
    if seen_events.contains_key(&event.id) {
        return;
    }
    // Cap the dedup set to prevent unbounded growth.
    if seen_events.len() >= SEEN_EVENT_CAP {
        // Simple eviction: clear half the entries.
        let keys_to_remove: Vec<String> = seen_events
            .iter()
            .take(SEEN_EVENT_CAP / 2)
            .map(|r| r.key().clone())
            .collect();
        for k in keys_to_remove {
            seen_events.remove(&k);
        }
    }
    seen_events.insert(event.id.clone(), ());

    // Decrypt content.
    let sender_pk = match crypto::parse_pubkey_hex(&event.pubkey) {
        Ok(pk) => pk,
        Err(e) => {
            warn!(relay = %relay_url, error = %e, "invalid sender pubkey in DM event");
            return;
        }
    };

    let plaintext = match crypto::nip04_decrypt(keys.secret_key(), &sender_pk, &event.content) {
        Ok(pt) => pt,
        Err(e) => {
            warn!(relay = %relay_url, event_id = %event.id, error = %e, "NIP-04 decrypt failed");
            return;
        }
    };

    let sender_pk_hex = event.pubkey.clone();
    let short_pk = format!(
        "{}...{}",
        &sender_pk_hex[..8.min(sender_pk_hex.len())],
        &sender_pk_hex[sender_pk_hex.len().saturating_sub(4)..]
    );

    let unified = UnifiedIncomingMessage {
        id: event.id.clone(),
        platform: PluginType::Nostr,
        chat_id: sender_pk_hex.clone(),
        user: UnifiedUser {
            id: sender_pk_hex,
            username: None,
            display_name: short_pk,
            avatar_url: None,
        },
        content: UnifiedMessageContent {
            content_type: MessageContentType::Text,
            text: plaintext,
            attachments: None,
        },
        timestamp: event.created_at,
        reply_to_message_id: None,
        action: None,
        raw: None,
    };

    if let Err(e) = message_tx.send(unified).await {
        warn!(relay = %relay_url, error = %e, "failed to forward decrypted DM");
    }
}

/// Build a TLS connector for WebSocket connections (same pattern as Discord).
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
        .map_err(|e| ChannelError::ConnectionFailed(format!("Nostr TLS config failed: {e}")))?
        .with_root_certificates(root_store)
        .with_no_client_auth();

    Ok(Connector::Rustls(Arc::new(config)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RECEIVER_SK_HEX: &str =
        "7b911fd37cdf5c81d4c0adb1ab7fa822ed253ab0ad9aa18d77257c88b29b718e";

    #[tokio::test]
    async fn blank_event_id_fails_before_local_dedup_or_enqueue() {
        let receiver_secret = crypto::parse_secret_key(RECEIVER_SK_HEX).unwrap();
        let receiver_keys = Keys::new(receiver_secret);
        let receiver_pk = receiver_keys.public_key().to_hex();
        let event = RawEvent {
            id: " \t".into(),
            pubkey: "not-needed-before-id-validation".into(),
            created_at: 1_700_000_000,
            kind: 4,
            tags: vec![vec!["p".into(), receiver_pk.clone()]],
            content: "not-needed-before-id-validation".into(),
            sig: String::new(),
        };
        let seen_events = Arc::new(DashMap::new());
        let (message_tx, mut message_rx) = mpsc::channel(1);

        handle_dm_event(
            event,
            &receiver_keys,
            &receiver_pk,
            &message_tx,
            &seen_events,
            "wss://relay.example",
        )
        .await;

        assert!(seen_events.is_empty());
        assert!(message_rx.try_recv().is_err());
    }
}
