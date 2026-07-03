use std::{collections::HashMap, time::Instant};

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::config::{AppConfig, SecretString};

const APPS_CONNECTIONS_OPEN_URL: &str = "https://slack.com/api/apps.connections.open";

#[derive(Clone)]
pub struct SlackApiClient {
    http: reqwest::Client,
    bot_token: SecretString,
    app_token: SecretString,
}

impl SlackApiClient {
    pub fn new(bot_token: SecretString, app_token: SecretString) -> Self {
        Self {
            http: reqwest::Client::new(),
            bot_token,
            app_token,
        }
    }

    pub async fn open_socket_connection(&self) -> Result<String, SlackError> {
        let response = self
            .http
            .post(APPS_CONNECTIONS_OPEN_URL)
            .bearer_auth(self.app_token.expose())
            .send()
            .await?
            .error_for_status()?
            .json::<OpenConnectionResponse>()
            .await?;

        if response.ok {
            response.url.ok_or(SlackError::MissingField("url"))
        } else {
            Err(SlackError::Api {
                method: "apps.connections.open",
                error: response.error.unwrap_or_else(|| "unknown_error".to_owned()),
            })
        }
    }

    pub fn bot_token(&self) -> &SecretString {
        &self.bot_token
    }
}

#[derive(Debug, Deserialize)]
struct OpenConnectionResponse {
    ok: bool,
    url: Option<String>,
    error: Option<String>,
}

pub struct SocketModeRunner {
    config: AppConfig,
    api: SlackApiClient,
    started_at: Instant,
}

impl SocketModeRunner {
    pub fn new(config: AppConfig, api: SlackApiClient, started_at: Instant) -> Self {
        Self {
            config,
            api,
            started_at,
        }
    }

    pub async fn run(self) -> Result<(), SlackError> {
        let socket_url = self.api.open_socket_connection().await?;
        tracing::info!(host = %self.config.bot_hostname, "socket mode url acquired");

        let (mut stream, _) = connect_async(socket_url).await?;
        tracing::info!(host = %self.config.bot_hostname, "socket mode connected");

        while let Some(message) = stream.next().await {
            let message = message?;
            match message {
                Message::Text(text) => {
                    if let Some(ack) = self.handle_socket_text(&text)? {
                        stream
                            .send(Message::Text(serde_json::to_string(&ack)?))
                            .await?;
                    }
                }
                Message::Close(frame) => {
                    tracing::warn!(?frame, "socket mode closed");
                    break;
                }
                Message::Ping(payload) => {
                    stream.send(Message::Pong(payload)).await?;
                }
                _ => {}
            }
        }

        Ok(())
    }

    fn handle_socket_text(&self, text: &str) -> Result<Option<SocketAck>, SlackError> {
        let value = serde_json::from_str::<Value>(text)?;
        if value.get("type").and_then(Value::as_str) == Some("hello") {
            tracing::info!("socket mode hello received");
            return Ok(None);
        }

        let envelope = serde_json::from_value::<SocketEnvelope>(value)?;
        Ok(handle_envelope(&self.config, self.started_at, envelope))
    }
}

#[derive(Debug, Deserialize)]
pub struct SocketEnvelope {
    pub envelope_id: String,
    #[serde(rename = "type")]
    pub envelope_type: String,
    #[serde(default)]
    pub accepts_response_payload: bool,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Deserialize)]
pub struct SlashCommandPayload {
    pub team_id: String,
    pub channel_id: String,
    pub user_id: String,
    pub command: String,
    #[serde(default)]
    pub text: String,
}

#[derive(Debug, Serialize, Eq, PartialEq)]
pub struct SocketAck {
    pub envelope_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<SlackResponsePayload>,
}

#[derive(Debug, Serialize, Eq, PartialEq)]
pub struct SlackResponsePayload {
    pub response_type: ResponseType,
    pub text: String,
}

#[derive(Debug, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ResponseType {
    Ephemeral,
}

pub fn handle_envelope(
    config: &AppConfig,
    started_at: Instant,
    envelope: SocketEnvelope,
) -> Option<SocketAck> {
    if envelope.envelope_type != "slash_commands" {
        return Some(SocketAck {
            envelope_id: envelope.envelope_id,
            payload: None,
        });
    }

    let payload = serde_json::from_value::<SlashCommandPayload>(envelope.payload);
    let response = match payload {
        Ok(command) => handle_slash_command(config, started_at, command),
        Err(error) => SlackResponsePayload {
            response_type: ResponseType::Ephemeral,
            text: format!("Could not parse Slack command payload: {error}"),
        },
    };

    Some(SocketAck {
        envelope_id: envelope.envelope_id,
        payload: if envelope.accepts_response_payload {
            Some(response)
        } else {
            None
        },
    })
}

fn handle_slash_command(
    config: &AppConfig,
    started_at: Instant,
    command: SlashCommandPayload,
) -> SlackResponsePayload {
    if !is_command_allowed(config, &command) {
        return SlackResponsePayload {
            response_type: ResponseType::Ephemeral,
            text: "This Slack team or user is not allowed to use this host.".to_owned(),
        };
    }

    if command.command == "/codex-ping" {
        SlackResponsePayload {
            response_type: ResponseType::Ephemeral,
            text: ping_text(&config.bot_hostname, started_at),
        }
    } else {
        SlackResponsePayload {
            response_type: ResponseType::Ephemeral,
            text: format!(
                "Unsupported command {}. Use /codex-ping to check this host.",
                command.command
            ),
        }
    }
}

fn is_command_allowed(config: &AppConfig, command: &SlashCommandPayload) -> bool {
    config
        .allowed_team_id
        .as_ref()
        .map(|team_id| team_id == &command.team_id)
        .unwrap_or(true)
        && (config.allowed_user_ids.is_empty()
            || config.allowed_user_ids.contains(&command.user_id))
}

pub fn ping_text(hostname: &str, started_at: Instant) -> String {
    let uptime_secs = started_at.elapsed().as_secs();
    format!("pong from {hostname} (uptime {uptime_secs}s)")
}

#[derive(Debug, thiserror::Error)]
pub enum SlackError {
    #[error("slack api {method} failed with {error}")]
    Api { method: &'static str, error: String },
    #[error("slack api response missing required field {0}")]
    MissingField(&'static str),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl From<SlackError> for HashMap<String, String> {
    fn from(error: SlackError) -> Self {
        HashMap::from([("error".to_owned(), error.to_string())])
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::config::SecretString;

    fn test_config() -> AppConfig {
        AppConfig {
            slack_bot_token: SecretString::new("xoxb-test-secret"),
            slack_app_token: SecretString::new("xapp-test-secret"),
            allowed_team_id: Some("T123".to_owned()),
            allowed_user_ids: ["U123".to_owned()].into_iter().collect(),
            bot_hostname: "desk".to_owned(),
            max_session_timeout_secs: 600,
            codex_output_max_chars: 39_000,
        }
    }

    #[test]
    fn ping_command_builds_ack_payload() {
        let started_at = Instant::now() - Duration::from_secs(7);
        let envelope = SocketEnvelope {
            envelope_id: "E1".to_owned(),
            envelope_type: "slash_commands".to_owned(),
            accepts_response_payload: true,
            payload: serde_json::json!({
                "team_id": "T123",
                "channel_id": "D123",
                "user_id": "U123",
                "command": "/codex-ping",
                "text": ""
            }),
        };

        let ack = handle_envelope(&test_config(), started_at, envelope).unwrap();

        assert_eq!(ack.envelope_id, "E1");
        let payload = ack.payload.unwrap();
        assert_eq!(payload.response_type, ResponseType::Ephemeral);
        assert_eq!(payload.text, "pong from desk (uptime 7s)");
    }

    #[test]
    fn non_slash_envelope_is_acknowledged_without_payload() {
        let envelope = SocketEnvelope {
            envelope_id: "E2".to_owned(),
            envelope_type: "events_api".to_owned(),
            accepts_response_payload: true,
            payload: serde_json::json!({ "event": { "type": "message" } }),
        };

        assert_eq!(
            handle_envelope(&test_config(), Instant::now(), envelope),
            Some(SocketAck {
                envelope_id: "E2".to_owned(),
                payload: None
            })
        );
    }

    #[test]
    fn ack_serialization_does_not_include_tokens() {
        let started_at = Instant::now();
        let envelope = SocketEnvelope {
            envelope_id: "E3".to_owned(),
            envelope_type: "slash_commands".to_owned(),
            accepts_response_payload: true,
            payload: serde_json::json!({
                "team_id": "T123",
                "channel_id": "D123",
                "user_id": "U123",
                "command": "/codex-ping"
            }),
        };

        let ack = handle_envelope(&test_config(), started_at, envelope).unwrap();
        let encoded = serde_json::to_string(&ack).unwrap();

        assert!(!encoded.contains("xoxb-test-secret"));
        assert!(!encoded.contains("xapp-test-secret"));
        assert!(encoded.contains("pong from desk"));
    }

    #[test]
    fn disallowed_user_gets_sanitized_response() {
        let envelope = SocketEnvelope {
            envelope_id: "E4".to_owned(),
            envelope_type: "slash_commands".to_owned(),
            accepts_response_payload: true,
            payload: serde_json::json!({
                "team_id": "T123",
                "channel_id": "D123",
                "user_id": "U999",
                "command": "/codex-ping"
            }),
        };

        let ack = handle_envelope(&test_config(), Instant::now(), envelope).unwrap();
        let text = ack.payload.unwrap().text;

        assert_eq!(
            text,
            "This Slack team or user is not allowed to use this host."
        );
        assert!(!text.contains("U999"));
    }
}
