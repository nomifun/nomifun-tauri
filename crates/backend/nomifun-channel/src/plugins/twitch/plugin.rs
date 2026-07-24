//! Twitch channel plugin: IRC-over-WebSocket (fully outbound).
//!
//! Mirrors the Discord plugin structure: `initialize` validates the OAuth
//! token via the Twitch API, `start` spawns a background WS loop that
//! connects to `wss://irc-ws.chat.twitch.tv:443`, authenticates with
//! CAP/PASS/NICK/JOIN, and reads PRIVMSG lines. Outbound messages are
//! sent as `PRIVMSG #channel :text`.

use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use crate::constants::{TWITCH_MAX_RECONNECT_ATTEMPTS, TWITCH_MAX_RECONNECT_DELAY, TWITCH_MESSAGE_LIMIT};
use crate::error::ChannelError;
use crate::plugin::{ChannelPlugin, PluginCallbacks, SharedPluginStatus, mark_error_on_unexpected_exit};
use crate::types::{
    BotInfo, MessageContentType, PluginConfig, PluginStatus, PluginType, UnifiedIncomingMessage,
    UnifiedMessageContent, UnifiedOutgoingMessage, UnifiedUser,
};

use super::api::TwitchApi;
use super::types::{IrcLine, ParsedPrivmsg};

const TWITCH_WS_URL: &str = "wss://irc-ws.chat.twitch.tv:443";

/// Twitch IRC-over-WebSocket plugin.
pub struct TwitchPlugin {
    status: SharedPluginStatus,
    bot_info: Option<BotInfo>,
    last_error: Option<String>,
    /// The bot's IRC login (lowercase), used as NICK and for self-loop guard.
    bot_login: Option<String>,
    /// The OAuth access token (without the "oauth:" prefix).
    token: Option<String>,
    /// The target channel (with leading '#', lowercased).
    channel: Option<String>,
    callbacks: Option<PluginCallbacks>,
    /// Handle for outbound writes from `send_message`.
    write_tx: Option<mpsc::Sender<String>>,
    gateway_handle: Option<JoinHandle<()>>,
    shutdown_tx: Option<watch::Sender<bool>>,
}

impl Default for TwitchPlugin {
    fn default() -> Self {
        Self {
            status: SharedPluginStatus::default(),
            bot_info: None,
            last_error: None,
            bot_login: None,
            token: None,
            channel: None,
            callbacks: None,
            write_tx: None,
            gateway_handle: None,
            shutdown_tx: None,
        }
    }
}

impl TwitchPlugin {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl ChannelPlugin for TwitchPlugin {
    async fn initialize(&mut self, config: PluginConfig, callbacks: PluginCallbacks) -> Result<(), ChannelError> {
        self.status.set(PluginStatus::Initializing);

        let token = config
            .credentials
            .token
            .as_deref()
            .filter(|t| !t.is_empty())
            .ok_or_else(|| {
                self.status.set(PluginStatus::Error);
                self.last_error = Some("Missing Twitch OAuth token".into());
                ChannelError::InvalidConfig("Missing Twitch OAuth token".into())
            })?
            .to_string();

        let client = Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| {
                self.status.set(PluginStatus::Error);
                self.last_error = Some(format!("HTTP client init failed: {e}"));
                ChannelError::ConnectionFailed(format!("HTTP client init failed: {e}"))
            })?;

        let api = TwitchApi::new(client);
        let validated = api.validate(&token).await.map_err(|e| {
            self.status.set(PluginStatus::Error);
            self.last_error = Some(format!("Token validation failed: {e}"));
            e
        })?;

        let login = validated.login.to_lowercase();
        self.bot_info = Some(BotInfo {
            id: validated.user_id.clone(),
            username: Some(login.clone()),
            display_name: login.clone(),
        });

        // Determine the target channel. If the user provided one, normalize it;
        // otherwise default to the bot's own login.
        let raw_channel = config
            .credentials
            .twitch_channel
            .as_deref()
            .filter(|c| !c.is_empty());
        let channel = normalize_channel(raw_channel.unwrap_or(&login));

        info!(
            bot_login = %login,
            user_id = %validated.user_id,
            channel = %channel,
            "Twitch bot initialized"
        );

        self.bot_login = Some(login);
        self.token = Some(token);
        self.channel = Some(channel);
        self.callbacks = Some(callbacks);
        self.status.set(PluginStatus::Ready);
        Ok(())
    }

    async fn start(&mut self) -> Result<(), ChannelError> {
        self.status.set(PluginStatus::Starting);

        if self.gateway_handle.is_some() {
            self.status.set(PluginStatus::Running);
            return Ok(());
        }

        let token = self
            .token
            .clone()
            .ok_or_else(|| ChannelError::PlatformApi("Twitch plugin not initialized".into()))?;
        let bot_login = self
            .bot_login
            .clone()
            .ok_or_else(|| ChannelError::PlatformApi("Twitch bot login not initialized".into()))?;
        let channel = self
            .channel
            .clone()
            .ok_or_else(|| ChannelError::PlatformApi("Twitch channel not initialized".into()))?;
        let callbacks = self
            .callbacks
            .clone()
            .ok_or_else(|| ChannelError::PlatformApi("Twitch callbacks not initialized".into()))?;

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (write_tx, write_rx) = mpsc::channel::<String>(64);

        self.shutdown_tx = Some(shutdown_tx);
        self.write_tx = Some(write_tx);
        self.gateway_handle = Some(tokio::spawn(run_irc_loop(
            token,
            bot_login,
            channel,
            callbacks.message_tx,
            self.status.clone(),
            shutdown_rx,
            write_rx,
        )));

        self.status.set(PluginStatus::Running);
        info!("Twitch plugin started");
        Ok(())
    }

    async fn stop(&mut self) -> Result<(), ChannelError> {
        self.status.set(PluginStatus::Stopping);

        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(true);
        }
        // Drop the write channel so the loop can exit cleanly.
        self.write_tx = None;

        if let Some(handle) = self.gateway_handle.take() {
            let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        }

        self.callbacks = None;
        self.status.set(PluginStatus::Stopped);
        info!("Twitch plugin stopped");
        Ok(())
    }

    async fn send_message(&self, chat_id: &str, message: UnifiedOutgoingMessage) -> Result<String, ChannelError> {
        let write_tx = self
            .write_tx
            .as_ref()
            .ok_or_else(|| ChannelError::PlatformApi("Twitch plugin not running".into()))?;

        let text = message.text.as_deref().unwrap_or("");
        if text.is_empty() {
            return Ok(String::new());
        }

        let channel = if chat_id.is_empty() {
            self.channel.as_deref().unwrap_or("#unknown")
        } else {
            chat_id
        };

        let lines = format_privmsgs(channel, text);
        for line in &lines {
            write_tx.send(line.clone()).await.map_err(|_| {
                ChannelError::MessageSendFailed("Twitch write channel closed".into())
            })?;
        }

        // Twitch IRC has no message IDs; return a synthetic one.
        Ok(format!("twitch-{}", std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()))
    }

    async fn edit_message(
        &self,
        chat_id: &str,
        _message_id: &str,
        message: UnifiedOutgoingMessage,
    ) -> Result<(), ChannelError> {
        // Twitch IRC has no edit capability. Send as a new message (fallback).
        // Stream relay classifies Twitch as send-once, so edits are rare.
        let _ = self.send_message(chat_id, message).await?;
        Ok(())
    }

    fn active_user_count(&self) -> usize {
        0
    }

    fn bot_info(&self) -> Option<&BotInfo> {
        self.bot_info.as_ref()
    }

    fn plugin_type(&self) -> PluginType {
        PluginType::Twitch
    }

    fn status(&self) -> PluginStatus {
        self.status.get()
    }

    fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }
}

// ---------------------------------------------------------------------------
// Background IRC WebSocket loop
// ---------------------------------------------------------------------------

/// Background task: maintain the Twitch IRC connection with reconnects.
async fn run_irc_loop(
    token: String,
    bot_login: String,
    channel: String,
    message_tx: mpsc::Sender<UnifiedIncomingMessage>,
    status: SharedPluginStatus,
    mut shutdown_rx: watch::Receiver<bool>,
    mut write_rx: mpsc::Receiver<String>,
) {
    let mut consecutive_errors: u32 = 0;

    loop {
        if *shutdown_rx.borrow() {
            debug!("Twitch IRC loop received shutdown signal");
            break;
        }

        match connect_once(
            &token,
            &bot_login,
            &channel,
            &message_tx,
            &mut shutdown_rx,
            &mut write_rx,
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
                warn!(error = %e, consecutive_errors, "Twitch IRC error");
                if consecutive_errors >= TWITCH_MAX_RECONNECT_ATTEMPTS {
                    error!("Twitch max reconnect attempts reached, stopping IRC loop");
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

    mark_error_on_unexpected_exit(&status, &shutdown_rx, "twitch");
    debug!("Twitch IRC loop exited");
}

/// Exponential backoff capped at `TWITCH_MAX_RECONNECT_DELAY`.
fn backoff_delay(attempt: u32) -> Duration {
    let secs = 2u64.saturating_pow(attempt).min(TWITCH_MAX_RECONNECT_DELAY.as_secs());
    Duration::from_secs(secs)
}

/// A single IRC connection: CAP → PASS → NICK → JOIN → read loop.
async fn connect_once(
    token: &str,
    bot_login: &str,
    channel: &str,
    message_tx: &mpsc::Sender<UnifiedIncomingMessage>,
    shutdown_rx: &mut watch::Receiver<bool>,
    write_rx: &mut mpsc::Receiver<String>,
) -> Result<(), ChannelError> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::connect_async_tls_with_config;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let connector = build_ws_tls_connector()?;
    let (ws_stream, _) = connect_async_tls_with_config(TWITCH_WS_URL, None, false, Some(connector))
        .await
        .map_err(|e| ChannelError::ConnectionFailed(format!("Twitch IRC connect failed: {e}")))?;
    info!("Twitch IRC connected");

    let (mut write, mut read) = ws_stream.split();

    // Send IRC registration sequence.
    let cap_req = "CAP REQ :twitch.tv/tags twitch.tv/commands";
    let pass = format!("PASS oauth:{token}");
    let nick = format!("NICK {bot_login}");
    let join = format!("JOIN {channel}");

    for line in [cap_req.to_string(), pass, nick, join] {
        write
            .send(WsMessage::Text(line.into()))
            .await
            .map_err(|e| ChannelError::ConnectionFailed(format!("Twitch IRC send failed: {e}")))?;
    }

    // Read loop: dispatch PING/PRIVMSG, forward outbound writes.
    loop {
        tokio::select! {
            frame = read.next() => {
                match frame {
                    Some(Ok(WsMessage::Text(txt))) => {
                        // Twitch may batch multiple lines in one frame.
                        for raw_line in txt.lines() {
                            let raw_line = raw_line.trim();
                            if raw_line.is_empty() {
                                continue;
                            }
                            match parse_irc_line(raw_line) {
                                IrcLine::Ping(payload) => {
                                    let pong = format!("PONG :{payload}");
                                    if let Err(e) = write.send(WsMessage::Text(pong.into())).await {
                                        warn!(error = %e, "Twitch PONG send failed");
                                    }
                                }
                                IrcLine::Privmsg(msg) => {
                                    // Self-loop guard: skip our own messages.
                                    if msg.nick.eq_ignore_ascii_case(bot_login) {
                                        continue;
                                    }
                                    if let Some(unified) = privmsg_to_unified(&msg) {
                                        let _ = message_tx.send(unified).await;
                                    } else {
                                        warn!(
                                            nick = %msg.nick,
                                            channel = %msg.channel,
                                            "Twitch PRIVMSG missing stable IRC id tag; dropping message"
                                        );
                                    }
                                }
                                IrcLine::Other => { /* JOIN/PART/USERNOTICE/etc. */ }
                            }
                        }
                    }
                    Some(Ok(WsMessage::Close(_))) => {
                        debug!("Twitch IRC received close frame");
                        return Ok(());
                    }
                    Some(Ok(_)) => { /* binary/ping/pong */ }
                    Some(Err(e)) => {
                        return Err(ChannelError::ConnectionFailed(format!("Twitch IRC read error: {e}")));
                    }
                    None => {
                        return Err(ChannelError::ConnectionFailed("Twitch IRC stream ended".into()));
                    }
                }
            }
            outbound = write_rx.recv() => {
                match outbound {
                    Some(line) => {
                        if let Err(e) = write.send(WsMessage::Text(line.into())).await {
                            warn!(error = %e, "Twitch outbound write failed");
                        }
                    }
                    None => {
                        // write_rx closed → plugin stopping.
                        debug!("Twitch outbound channel closed");
                        return Ok(());
                    }
                }
            }
            _ = shutdown_rx.changed() => {
                debug!("Twitch IRC shutdown during listen");
                return Ok(());
            }
        }
    }
}

/// Build the rustls-based TLS connector for the IRC WebSocket.
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
        .map_err(|e| ChannelError::ConnectionFailed(format!("Twitch TLS config failed: {e}")))?
        .with_root_certificates(root_store)
        .with_no_client_auth();

    Ok(Connector::Rustls(Arc::new(config)))
}

// ---------------------------------------------------------------------------
// Pure helpers (unit-tested)
// ---------------------------------------------------------------------------

/// Normalize a channel name: strip leading '#', lowercase, then re-add '#'.
pub(crate) fn normalize_channel(input: &str) -> String {
    let stripped = input.trim().trim_start_matches('#');
    format!("#{}", stripped.to_lowercase())
}

/// Parse a single raw IRC line into an `IrcLine` variant.
///
/// Handles:
/// - `PING :tmi.twitch.tv` → `IrcLine::Ping`
/// - `@<tags> :<nick>!<user>@<host> PRIVMSG #<channel> :<msg>` → `IrcLine::Privmsg`
/// - `:<nick>!<user>@<host> PRIVMSG #<channel> :<msg>` → `IrcLine::Privmsg`
/// - Everything else → `IrcLine::Other`
pub(crate) fn parse_irc_line(line: &str) -> IrcLine {
    let line = line.trim();

    // PING handling.
    if let Some(payload) = line.strip_prefix("PING :") {
        return IrcLine::Ping(payload.to_string());
    }
    if line == "PING" {
        return IrcLine::Ping("tmi.twitch.tv".to_string());
    }

    // Split the optional @tags prefix from the :<prefix> COMMAND part.
    let (tags, remainder) = if line.starts_with('@') {
        // Tags end at the first space, then the rest is :<prefix> COMMAND ...
        match line.find(' ') {
            Some(idx) => (Some(&line[1..idx]), line[idx + 1..].trim_start()),
            None => return IrcLine::Other,
        }
    } else {
        (None, line)
    };
    let message_id = tags.and_then(|raw_tags| {
        raw_tags.split(';').find_map(|tag| {
            let (key, value) = tag.split_once('=')?;
            (key == "id" && !value.trim().is_empty()).then(|| value.trim().to_owned())
        })
    });

    // Must start with ':' (prefix).
    let remainder = match remainder.strip_prefix(':') {
        Some(r) => r,
        None => return IrcLine::Other,
    };

    // Extract nick from prefix (nick!user@host).
    let (prefix, after_prefix) = match remainder.find(' ') {
        Some(idx) => (&remainder[..idx], &remainder[idx + 1..]),
        None => return IrcLine::Other,
    };

    let nick = match prefix.find('!') {
        Some(idx) => &prefix[..idx],
        None => return IrcLine::Other,
    };

    // Check for PRIVMSG command.
    let after_prefix = after_prefix.trim_start();
    let privmsg_rest = match after_prefix.strip_prefix("PRIVMSG ") {
        Some(rest) => rest.trim_start(),
        None => return IrcLine::Other,
    };

    // Parse channel and message: #<channel> :<message>
    let (channel, message) = match privmsg_rest.find(" :") {
        Some(idx) => (
            privmsg_rest[..idx].trim(),
            privmsg_rest[idx + 2..].to_string(),
        ),
        None => return IrcLine::Other,
    };

    IrcLine::Privmsg(ParsedPrivmsg {
        message_id,
        nick: nick.to_string(),
        channel: channel.to_string(),
        message,
    })
}

/// Convert a parsed PRIVMSG into a `UnifiedIncomingMessage`.
fn privmsg_to_unified(msg: &ParsedPrivmsg) -> Option<UnifiedIncomingMessage> {
    let stable_event_id = msg.message_id.as_deref().map(str::trim).filter(|id| !id.is_empty())?;
    let content_type = if msg.message.starts_with('!') {
        MessageContentType::Command
    } else {
        MessageContentType::Text
    };

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    Some(UnifiedIncomingMessage {
        id: stable_event_id.to_owned(),
        platform: PluginType::Twitch,
        chat_id: msg.channel.clone(),
        user: UnifiedUser {
            id: msg.nick.clone(),
            username: Some(msg.nick.clone()),
            display_name: msg.nick.clone(),
            avatar_url: None,
        },
        content: UnifiedMessageContent {
            content_type,
            text: msg.message.clone(),
            attachments: None,
        },
        timestamp: now,
        reply_to_message_id: None,
        action: None,
        raw: None,
    })
}

/// Format outbound PRIVMSG(s) for a channel, splitting and truncating as needed.
///
/// Multi-line text is split into separate PRIVMSG commands. Each line is
/// truncated to `TWITCH_MESSAGE_LIMIT` characters.
pub(crate) fn format_privmsgs(channel: &str, text: &str) -> Vec<String> {
    let mut result = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let truncated = truncate_text(line, TWITCH_MESSAGE_LIMIT);
        result.push(format!("PRIVMSG {channel} :{truncated}"));
    }
    if result.is_empty() {
        // If the text had no non-empty lines, send the full text as one line.
        let truncated = truncate_text(text.trim(), TWITCH_MESSAGE_LIMIT);
        if !truncated.is_empty() {
            result.push(format!("PRIVMSG {channel} :{truncated}"));
        }
    }
    result
}

/// Truncate text at a char boundary to `limit`, appending "..." if cut.
pub(crate) fn truncate_text(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let truncated: String = text.chars().take(limit.saturating_sub(3)).collect();
    format!("{truncated}...")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Channel normalization ─────────────────────────────────────────────

    #[test]
    fn normalize_channel_plain() {
        assert_eq!(normalize_channel("mychannel"), "#mychannel");
    }

    #[test]
    fn normalize_channel_with_hash() {
        assert_eq!(normalize_channel("#MyChannel"), "#mychannel");
    }

    #[test]
    fn normalize_channel_with_whitespace() {
        assert_eq!(normalize_channel("  #FooBar  "), "#foobar");
    }

    #[test]
    fn normalize_channel_already_lowercase() {
        assert_eq!(normalize_channel("#already"), "#already");
    }

    // ── IRC line parsing ──────────────────────────────────────────────────

    #[test]
    fn parse_ping() {
        assert_eq!(
            parse_irc_line("PING :tmi.twitch.tv"),
            IrcLine::Ping("tmi.twitch.tv".into())
        );
    }

    #[test]
    fn parse_ping_bare() {
        assert_eq!(
            parse_irc_line("PING"),
            IrcLine::Ping("tmi.twitch.tv".into())
        );
    }

    #[test]
    fn parse_privmsg_without_tags() {
        let line = ":alice!alice@alice.tmi.twitch.tv PRIVMSG #mychannel :Hello world";
        let expected = IrcLine::Privmsg(ParsedPrivmsg {
            message_id: None,
            nick: "alice".into(),
            channel: "#mychannel".into(),
            message: "Hello world".into(),
        });
        assert_eq!(parse_irc_line(line), expected);
    }

    #[test]
    fn parse_privmsg_with_tags() {
        let line = "@badge-info=;badges=broadcaster/1;color=#FF0000;display-name=Alice;emotes=;flags=;id=abc-123;mod=0;room-id=12345;subscriber=0;tmi-sent-ts=1600000000000;turbo=0;user-id=67890;user-type= :alice!alice@alice.tmi.twitch.tv PRIVMSG #mychannel :Hello world";
        let expected = IrcLine::Privmsg(ParsedPrivmsg {
            message_id: Some("abc-123".into()),
            nick: "alice".into(),
            channel: "#mychannel".into(),
            message: "Hello world".into(),
        });
        assert_eq!(parse_irc_line(line), expected);
    }

    #[test]
    fn parse_privmsg_command() {
        let line = ":bob!bob@bob.tmi.twitch.tv PRIVMSG #stream :!help me";
        let result = parse_irc_line(line);
        match result {
            IrcLine::Privmsg(msg) => {
                assert_eq!(msg.nick, "bob");
                assert_eq!(msg.channel, "#stream");
                assert_eq!(msg.message, "!help me");
            }
            _ => panic!("expected Privmsg"),
        }
    }

    #[test]
    fn parse_privmsg_with_colons_in_message() {
        let line = ":nick!nick@nick.tmi.twitch.tv PRIVMSG #ch :hello: world: test";
        match parse_irc_line(line) {
            IrcLine::Privmsg(msg) => {
                assert_eq!(msg.message, "hello: world: test");
            }
            _ => panic!("expected Privmsg"),
        }
    }

    #[test]
    fn parse_join_is_other() {
        let line = ":nick!nick@nick.tmi.twitch.tv JOIN #channel";
        assert_eq!(parse_irc_line(line), IrcLine::Other);
    }

    #[test]
    fn parse_notice_is_other() {
        let line = "@msg-id=slow_off :tmi.twitch.tv NOTICE #channel :This room is no longer in slow mode.";
        assert_eq!(parse_irc_line(line), IrcLine::Other);
    }

    #[test]
    fn parse_empty_line() {
        assert_eq!(parse_irc_line(""), IrcLine::Other);
    }

    #[test]
    fn parse_garbage() {
        assert_eq!(parse_irc_line("not a valid irc line"), IrcLine::Other);
    }

    // ── Self-loop guard ───────────────────────────────────────────────────

    #[test]
    fn self_loop_guard() {
        let bot_login = "mybot";
        let msg = ParsedPrivmsg {
            message_id: Some("self-1".into()),
            nick: "mybot".into(),
            channel: "#ch".into(),
            message: "echo".into(),
        };
        assert!(msg.nick.eq_ignore_ascii_case(bot_login));

        let other = ParsedPrivmsg {
            message_id: Some("other-1".into()),
            nick: "someone".into(),
            channel: "#ch".into(),
            message: "hello".into(),
        };
        assert!(!other.nick.eq_ignore_ascii_case(bot_login));
    }

    // ── PRIVMSG formatting ────────────────────────────────────────────────

    #[test]
    fn format_single_line() {
        let lines = format_privmsgs("#ch", "hello");
        assert_eq!(lines, vec!["PRIVMSG #ch :hello"]);
    }

    #[test]
    fn format_multi_line() {
        let lines = format_privmsgs("#ch", "line1\nline2\nline3");
        assert_eq!(
            lines,
            vec![
                "PRIVMSG #ch :line1",
                "PRIVMSG #ch :line2",
                "PRIVMSG #ch :line3",
            ]
        );
    }

    #[test]
    fn format_skips_empty_lines() {
        let lines = format_privmsgs("#ch", "first\n\n\nsecond");
        assert_eq!(
            lines,
            vec!["PRIVMSG #ch :first", "PRIVMSG #ch :second"]
        );
    }

    #[test]
    fn format_truncates_long_line() {
        let long = "x".repeat(600);
        let lines = format_privmsgs("#ch", &long);
        assert_eq!(lines.len(), 1);
        // The PRIVMSG prefix is not counted in the limit.
        let content = lines[0].strip_prefix("PRIVMSG #ch :").unwrap();
        assert!(content.chars().count() <= TWITCH_MESSAGE_LIMIT);
        assert!(content.ends_with("..."));
    }

    // ── Text truncation ───────────────────────────────────────────────────

    #[test]
    fn truncate_short_text() {
        assert_eq!(truncate_text("short", 480), "short");
    }

    #[test]
    fn truncate_exact_limit() {
        let text = "a".repeat(480);
        assert_eq!(truncate_text(&text, 480), text);
    }

    #[test]
    fn truncate_over_limit() {
        let text = "b".repeat(500);
        let result = truncate_text(&text, 480);
        assert_eq!(result.chars().count(), 480);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn truncate_unicode() {
        let text = "你好世界测试这是一个很长的中文字符串";
        // 16 chars, truncate to 10
        let result = truncate_text(text, 10);
        assert_eq!(result.chars().count(), 10);
        assert!(result.ends_with("..."));
    }

    // ── PRIVMSG → UnifiedIncomingMessage ──────────────────────────────────

    #[test]
    fn privmsg_to_unified_text() {
        let msg = ParsedPrivmsg {
            message_id: Some("twitch-msg-1".into()),
            nick: "alice".into(),
            channel: "#test".into(),
            message: "hello there".into(),
        };
        let unified = privmsg_to_unified(&msg).unwrap();
        assert_eq!(unified.id, "twitch-msg-1");
        assert_eq!(unified.platform, PluginType::Twitch);
        assert_eq!(unified.chat_id, "#test");
        assert_eq!(unified.content.text, "hello there");
        assert_eq!(unified.content.content_type, MessageContentType::Text);
        assert_eq!(unified.user.id, "alice");
        assert_eq!(unified.user.display_name, "alice");
        assert!(unified.action.is_none());
    }

    #[test]
    fn privmsg_to_unified_command() {
        let msg = ParsedPrivmsg {
            message_id: Some("twitch-msg-2".into()),
            nick: "bob".into(),
            channel: "#game".into(),
            message: "!roll 20".into(),
        };
        let unified = privmsg_to_unified(&msg).unwrap();
        assert_eq!(unified.content.content_type, MessageContentType::Command);
        assert_eq!(unified.content.text, "!roll 20");
    }

    #[test]
    fn privmsg_without_stable_id_is_not_normalized() {
        let msg = ParsedPrivmsg {
            message_id: None,
            nick: "alice".into(),
            channel: "#test".into(),
            message: "hello there".into(),
        };

        assert!(privmsg_to_unified(&msg).is_none());
    }

    // ── Plugin initial state ──────────────────────────────────────────────

    #[test]
    fn new_plugin_initial_state() {
        let p = TwitchPlugin::new();
        assert_eq!(p.status(), PluginStatus::Created);
        assert!(p.bot_info().is_none());
        assert_eq!(p.plugin_type(), PluginType::Twitch);
        assert_eq!(p.active_user_count(), 0);
        assert!(p.last_error().is_none());
    }

    // ── Backoff ───────────────────────────────────────────────────────────

    #[test]
    fn backoff_is_exponential_and_capped() {
        assert_eq!(backoff_delay(1), Duration::from_secs(2));
        assert_eq!(backoff_delay(3), Duration::from_secs(8));
        assert_eq!(backoff_delay(10), Duration::from_secs(30)); // capped
    }
}
