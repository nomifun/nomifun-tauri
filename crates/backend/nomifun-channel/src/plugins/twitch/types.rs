//! Twitch IRC + OAuth validation wire types.

use serde::Deserialize;

/// Response from `GET https://id.twitch.tv/oauth2/validate`.
///
/// The validate endpoint returns the bot's login, user_id, client_id and
/// granted scopes. We use `login` as the IRC NICK and `user_id` as the
/// canonical bot identifier.
#[derive(Debug, Deserialize)]
pub struct ValidateResponse {
    pub login: String,
    pub user_id: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub scopes: Vec<String>,
}

/// A parsed IRC PRIVMSG line.
///
/// Twitch IRC messages arriving over WebSocket have the form:
/// ```text
/// @<tags> :<nick>!<user>@<host> PRIVMSG #<channel> :<message>
/// ```
/// or without tags:
/// ```text
/// :<nick>!<user>@<host> PRIVMSG #<channel> :<message>
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedPrivmsg {
    /// Stable Twitch message id from the IRC `id` tag.
    ///
    /// The plugin requests `twitch.tv/tags`; an inbound message without this
    /// tag is parsed for diagnostics but must not be enqueued for business
    /// processing because no replay-stable identity can be proven.
    pub message_id: Option<String>,
    /// The sender's IRC nick (lowercase login).
    pub nick: String,
    /// The channel including the leading '#'.
    pub channel: String,
    /// The message text (everything after the second ':').
    pub message: String,
}

/// Result of parsing a raw IRC line.
#[derive(Debug, Clone, PartialEq)]
pub enum IrcLine {
    /// A PING from the server requiring a PONG reply.
    Ping(String),
    /// A PRIVMSG (chat message).
    Privmsg(ParsedPrivmsg),
    /// Any other IRC line we don't need to handle.
    Other,
}
