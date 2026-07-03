use std::{collections::HashMap, fmt, sync::Arc, time::Instant};

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use reqwest::header::CONTENT_TYPE;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::{
    codex::CodexRequest,
    config::{AppConfig, SecretString},
    lifecycle::{SessionLifecycle, SlackPublisher, SlackThread},
    sessions::ProcessedEvent,
};

const APPS_CONNECTIONS_OPEN_URL: &str = "https://slack.com/api/apps.connections.open";
const CHAT_POST_MESSAGE_URL: &str = "https://slack.com/api/chat.postMessage";
const FILES_GET_UPLOAD_URL_EXTERNAL_URL: &str = "https://slack.com/api/files.getUploadURLExternal";
const FILES_COMPLETE_UPLOAD_EXTERNAL_URL: &str =
    "https://slack.com/api/files.completeUploadExternal";

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
            Err(slack_api_error(
                "apps.connections.open",
                response.error,
                response.response_metadata,
            ))
        }
    }

    pub async fn post_message(
        &self,
        channel: &str,
        thread_ts: Option<&str>,
        text: &str,
    ) -> Result<PostedMessage, SlackError> {
        let request = ChatPostMessageRequest {
            channel,
            thread_ts,
            text,
            unfurl_links: false,
            unfurl_media: false,
        };
        let response = self
            .http
            .post(CHAT_POST_MESSAGE_URL)
            .bearer_auth(self.bot_token.expose())
            .json(&request)
            .send()
            .await?
            .error_for_status()?
            .json::<ChatPostMessageResponse>()
            .await?;

        if response.ok {
            Ok(PostedMessage {
                channel: response
                    .channel
                    .ok_or(SlackError::MissingField("channel"))?,
                ts: response.ts.ok_or(SlackError::MissingField("ts"))?,
            })
        } else {
            Err(slack_api_error(
                "chat.postMessage",
                response.error,
                response.response_metadata,
            ))
        }
    }

    pub async fn upload_text_file(
        &self,
        channel: &str,
        thread_ts: &str,
        filename: &str,
        content: &str,
    ) -> Result<(), SlackError> {
        let bytes = content.as_bytes().to_vec();
        let upload = self
            .http
            .post(FILES_GET_UPLOAD_URL_EXTERNAL_URL)
            .bearer_auth(self.bot_token.expose())
            .json(&GetUploadUrlExternalRequest {
                filename,
                length: bytes.len(),
            })
            .send()
            .await?
            .error_for_status()?
            .json::<GetUploadUrlExternalResponse>()
            .await?;

        if !upload.ok {
            return Err(slack_api_error(
                "files.getUploadURLExternal",
                upload.error,
                upload.response_metadata,
            ));
        }
        let upload_url = upload
            .upload_url
            .ok_or(SlackError::MissingField("upload_url"))?;
        let file_id = upload.file_id.ok_or(SlackError::MissingField("file_id"))?;

        self.http
            .post(upload_url)
            .header(CONTENT_TYPE, "text/plain; charset=utf-8")
            .body(bytes)
            .send()
            .await?
            .error_for_status()?;

        let complete = self
            .http
            .post(FILES_COMPLETE_UPLOAD_EXTERNAL_URL)
            .bearer_auth(self.bot_token.expose())
            .json(&CompleteUploadExternalRequest {
                files: vec![CompleteUploadExternalFile {
                    id: file_id,
                    title: filename.to_owned(),
                }],
                channel_id: channel,
                thread_ts,
                initial_comment:
                    "Codex output was too long for a Slack message; uploaded as a file.",
            })
            .send()
            .await?
            .error_for_status()?
            .json::<CompleteUploadExternalResponse>()
            .await?;

        if complete.ok {
            Ok(())
        } else {
            Err(slack_api_error(
                "files.completeUploadExternal",
                complete.error,
                complete.response_metadata,
            ))
        }
    }
}

#[async_trait]
impl SlackPublisher for SlackApiClient {
    async fn start_session_thread(
        &self,
        channel_id: &str,
        user_id: &str,
    ) -> Result<SlackThread, SlackError> {
        let posted = self
            .post_message(
                channel_id,
                None,
                &format!("Codex task started for <@{user_id}>."),
            )
            .await?;

        Ok(SlackThread {
            channel_id: posted.channel,
            thread_ts: posted.ts,
        })
    }

    async fn post_thread_message(
        &self,
        channel_id: &str,
        thread_ts: &str,
        text: &str,
    ) -> Result<(), SlackError> {
        self.post_message(channel_id, Some(thread_ts), text).await?;
        Ok(())
    }

    async fn publish_result(
        &self,
        channel_id: &str,
        thread_ts: &str,
        text: &str,
        max_chars: usize,
    ) -> Result<(), SlackError> {
        match plan_result_publish(text, max_chars) {
            ResultPublishPlan::Message(text) => {
                self.post_thread_message(channel_id, thread_ts, &text).await
            }
            ResultPublishPlan::ExternalFile { filename, content } => {
                self.upload_text_file(channel_id, thread_ts, &filename, &content)
                    .await
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct OpenConnectionResponse {
    ok: bool,
    url: Option<String>,
    error: Option<String>,
    response_metadata: Option<SlackResponseMetadata>,
}

#[derive(Debug, Serialize)]
struct ChatPostMessageRequest<'a> {
    channel: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    thread_ts: Option<&'a str>,
    text: &'a str,
    unfurl_links: bool,
    unfurl_media: bool,
}

#[derive(Debug, Deserialize)]
struct ChatPostMessageResponse {
    ok: bool,
    channel: Option<String>,
    ts: Option<String>,
    error: Option<String>,
    response_metadata: Option<SlackResponseMetadata>,
}

#[derive(Debug, Serialize)]
struct GetUploadUrlExternalRequest<'a> {
    filename: &'a str,
    length: usize,
}

#[derive(Debug, Deserialize)]
struct GetUploadUrlExternalResponse {
    ok: bool,
    upload_url: Option<String>,
    file_id: Option<String>,
    error: Option<String>,
    response_metadata: Option<SlackResponseMetadata>,
}

#[derive(Debug, Serialize)]
struct CompleteUploadExternalRequest<'a> {
    files: Vec<CompleteUploadExternalFile>,
    channel_id: &'a str,
    thread_ts: &'a str,
    initial_comment: &'a str,
}

#[derive(Debug, Serialize)]
struct CompleteUploadExternalFile {
    id: String,
    title: String,
}

#[derive(Debug, Deserialize)]
struct CompleteUploadExternalResponse {
    ok: bool,
    error: Option<String>,
    response_metadata: Option<SlackResponseMetadata>,
}

#[derive(Debug, Deserialize)]
struct SlackResponseMetadata {
    #[serde(default)]
    messages: Vec<String>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct PostedMessage {
    pub channel: String,
    pub ts: String,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum ResultPublishPlan {
    Message(String),
    ExternalFile { filename: String, content: String },
}

pub fn plan_result_publish(text: &str, max_chars: usize) -> ResultPublishPlan {
    if text.chars().count() <= max_chars {
        ResultPublishPlan::Message(text.to_owned())
    } else {
        ResultPublishPlan::ExternalFile {
            filename: "codex-output.txt".to_owned(),
            content: text.to_owned(),
        }
    }
}

pub struct SocketModeRunner {
    config: AppConfig,
    api: SlackApiClient,
    lifecycle: Arc<SessionLifecycle>,
    started_at: Instant,
}

impl SocketModeRunner {
    pub fn new(
        config: AppConfig,
        api: SlackApiClient,
        lifecycle: Arc<SessionLifecycle>,
        started_at: Instant,
    ) -> Self {
        Self {
            config,
            api,
            lifecycle,
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
                    let prepared = self.prepare_socket_text(&text)?;
                    if let Some(ack) = prepared.ack {
                        stream
                            .send(Message::Text(serde_json::to_string(&ack)?))
                            .await?;
                    }
                    if let Some(action) = prepared.action {
                        self.spawn_action(action);
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

    fn prepare_socket_text(&self, text: &str) -> Result<PreparedSocketEvent, SlackError> {
        let value = serde_json::from_str::<Value>(text)?;
        if value.get("type").and_then(Value::as_str) == Some("hello") {
            tracing::info!("socket mode hello received");
            return Ok(PreparedSocketEvent::default());
        }

        let envelope = serde_json::from_value::<SocketEnvelope>(value)?;
        Ok(prepare_envelope(&self.config, self.started_at, envelope))
    }

    fn spawn_action(&self, action: SocketAction) {
        let lifecycle = Arc::clone(&self.lifecycle);
        tokio::spawn(async move {
            let result = match action {
                SocketAction::StartCodex {
                    command,
                    processed_event,
                } => lifecycle.start_from_slash(command, processed_event).await,
                SocketAction::ResumeCodex {
                    event,
                    processed_event,
                } => lifecycle.resume_from_message(event, processed_event).await,
            };

            if let Err(error) = result {
                tracing::error!(error = %error, "socket action failed");
            }
        });
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

#[derive(Debug, Clone, Deserialize, Eq, PartialEq)]
pub struct SlashCommandPayload {
    pub team_id: String,
    pub channel_id: String,
    pub user_id: String,
    pub command: String,
    #[serde(default)]
    pub text: String,
}

#[derive(Debug, Clone, Deserialize, Eq, PartialEq)]
pub struct SlackMessageEvent {
    pub channel: String,
    pub user: Option<String>,
    #[serde(default)]
    pub text: String,
    pub ts: String,
    pub thread_ts: Option<String>,
}

impl SlackMessageEvent {
    pub fn thread_ts(&self) -> &str {
        self.thread_ts.as_deref().unwrap_or(&self.ts)
    }
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

#[derive(Debug, Default, Eq, PartialEq)]
pub struct PreparedSocketEvent {
    pub ack: Option<SocketAck>,
    pub action: Option<SocketAction>,
}

#[derive(Debug, Eq, PartialEq)]
pub enum SocketAction {
    StartCodex {
        command: SlashCommandPayload,
        processed_event: ProcessedEvent,
    },
    ResumeCodex {
        event: SlackMessageEvent,
        processed_event: ProcessedEvent,
    },
}

pub fn prepare_envelope(
    config: &AppConfig,
    started_at: Instant,
    envelope: SocketEnvelope,
) -> PreparedSocketEvent {
    match envelope.envelope_type.as_str() {
        "slash_commands" => prepare_slash_command(config, started_at, envelope),
        "events_api" => prepare_events_api(envelope),
        _ => PreparedSocketEvent {
            ack: Some(SocketAck {
                envelope_id: envelope.envelope_id,
                payload: None,
            }),
            action: None,
        },
    }
}

fn prepare_slash_command(
    config: &AppConfig,
    started_at: Instant,
    envelope: SocketEnvelope,
) -> PreparedSocketEvent {
    let event_key = slack_event_key(Some(&envelope.envelope_id), &envelope.payload)
        .unwrap_or_else(|| envelope.envelope_id.clone());
    let (response, action) = match serde_json::from_value::<SlashCommandPayload>(envelope.payload) {
        Ok(command) => handle_slash_command(config, started_at, command, event_key),
        Err(error) => (
            SlackResponsePayload {
                response_type: ResponseType::Ephemeral,
                text: format!("Could not parse Slack command payload: {error}"),
            },
            None,
        ),
    };

    PreparedSocketEvent {
        ack: Some(SocketAck {
            envelope_id: envelope.envelope_id,
            payload: if envelope.accepts_response_payload {
                Some(response)
            } else {
                None
            },
        }),
        action,
    }
}

fn prepare_events_api(envelope: SocketEnvelope) -> PreparedSocketEvent {
    let event_key = slack_event_key(Some(&envelope.envelope_id), &envelope.payload)
        .unwrap_or_else(|| envelope.envelope_id.clone());
    let action = parse_message_event(envelope.payload).map(|event| SocketAction::ResumeCodex {
        processed_event: ProcessedEvent::new(
            event_key,
            Some(event.thread_ts().to_owned()),
            "events_api",
        ),
        event,
    });

    PreparedSocketEvent {
        ack: Some(SocketAck {
            envelope_id: envelope.envelope_id,
            payload: None,
        }),
        action,
    }
}

pub fn slack_event_key(envelope_id: Option<&str>, payload: &Value) -> Option<String> {
    if let Some(envelope_id) = envelope_id.filter(|value| !value.is_empty()) {
        return Some(envelope_id.to_owned());
    }
    if let Some(event_id) = payload.get("event_id").and_then(Value::as_str) {
        return Some(event_id.to_owned());
    }

    let event = payload.get("event").unwrap_or(payload);
    if let Some(client_msg_id) = event.get("client_msg_id").and_then(Value::as_str) {
        return Some(client_msg_id.to_owned());
    }

    let channel = event.get("channel").and_then(Value::as_str)?;
    let ts = event.get("ts").and_then(Value::as_str)?;
    let user = event.get("user").and_then(Value::as_str)?;
    Some(format!("{channel}:{ts}:{user}"))
}

pub fn parse_message_event(payload: Value) -> Option<SlackMessageEvent> {
    let event = payload.get("event")?;
    if event.get("type").and_then(Value::as_str) != Some("message") {
        return None;
    }
    if event.get("bot_id").is_some() {
        return None;
    }
    if matches!(
        event.get("subtype").and_then(Value::as_str),
        Some("bot_message" | "message_deleted" | "message_changed")
    ) {
        return None;
    }

    serde_json::from_value::<SlackMessageEvent>(event.clone()).ok()
}

fn handle_slash_command(
    config: &AppConfig,
    started_at: Instant,
    command: SlashCommandPayload,
    event_key: String,
) -> (SlackResponsePayload, Option<SocketAction>) {
    if !is_command_allowed(config, &command) {
        return (
            SlackResponsePayload {
                response_type: ResponseType::Ephemeral,
                text: "This Slack team or user is not allowed to use this host.".to_owned(),
            },
            None,
        );
    }

    match command.command.as_str() {
        "/codex-ping" => (
            SlackResponsePayload {
                response_type: ResponseType::Ephemeral,
                text: ping_text(&config.bot_hostname, started_at),
            },
            None,
        ),
        "/codex" => match CodexRequest::parse(command.text.trim()) {
            Ok(request) if request.prompt.is_empty() => (
                SlackResponsePayload {
                    response_type: ResponseType::Ephemeral,
                    text: "Usage: /codex <prompt>".to_owned(),
                },
                None,
            ),
            Ok(_) => (
                SlackResponsePayload {
                    response_type: ResponseType::Ephemeral,
                    text: "Starting Codex. A thread reply will appear shortly.".to_owned(),
                },
                Some(SocketAction::StartCodex {
                    processed_event: ProcessedEvent::new(event_key, None, "slash_commands"),
                    command,
                }),
            ),
            Err(error) => (
                SlackResponsePayload {
                    response_type: ResponseType::Ephemeral,
                    text: error.user_message(),
                },
                None,
            ),
        },
        _ => (
            SlackResponsePayload {
                response_type: ResponseType::Ephemeral,
                text: format!(
                    "Unsupported command {}. Use /codex-ping or /codex <prompt>.",
                    command.command
                ),
            },
            None,
        ),
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
    #[error("slack api {method} failed with {error}{detail}")]
    Api {
        method: &'static str,
        error: String,
        detail: SlackApiErrorDetail,
    },
    #[error("slack api response missing required field {0}")]
    MissingField(&'static str),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SlackApiErrorDetail(String);

impl fmt::Display for SlackApiErrorDetail {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

fn slack_api_error(
    method: &'static str,
    error: Option<String>,
    response_metadata: Option<SlackResponseMetadata>,
) -> SlackError {
    SlackError::Api {
        method,
        error: error.unwrap_or_else(|| "unknown_error".to_owned()),
        detail: slack_api_error_detail(response_metadata),
    }
}

fn slack_api_error_detail(response_metadata: Option<SlackResponseMetadata>) -> SlackApiErrorDetail {
    let messages = response_metadata
        .into_iter()
        .flat_map(|metadata| metadata.messages)
        .filter_map(|message| sanitize_api_diagnostic(&message))
        .collect::<Vec<_>>();

    if messages.is_empty() {
        SlackApiErrorDetail(String::new())
    } else {
        SlackApiErrorDetail(format!("; metadata: {}", messages.join("; ")))
    }
}

fn sanitize_api_diagnostic(message: &str) -> Option<String> {
    let message = message.trim();
    if message.is_empty() {
        return None;
    }

    let lower = message.to_ascii_lowercase();
    if lower.contains("slack_bot_token")
        || lower.contains("slack_app_token")
        || lower.contains("slack_signing_secret")
        || lower.contains("xoxb-")
        || lower.contains("xapp-")
    {
        return Some("[redacted slack api diagnostic]".to_owned());
    }

    Some(take_chars(message, 300))
}

fn take_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

impl From<SlackError> for HashMap<String, String> {
    fn from(error: SlackError) -> Self {
        HashMap::from([("error".to_owned(), error.to_string())])
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, time::Duration};

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
            codex_cli_path: PathBuf::from("codex"),
            queue_db_path: PathBuf::from("./data/test.db"),
            child_env_allowlist: vec!["PATH".to_owned()],
            default_workspace: None,
            allowed_workspaces: vec![PathBuf::from(".")],
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

        let prepared = prepare_envelope(&test_config(), started_at, envelope);
        let ack = prepared.ack.unwrap();

        assert_eq!(ack.envelope_id, "E1");
        let payload = ack.payload.unwrap();
        assert_eq!(payload.response_type, ResponseType::Ephemeral);
        assert_eq!(payload.text, "pong from desk (uptime 7s)");
        assert_eq!(prepared.action, None);
    }

    #[test]
    fn codex_command_acknowledges_and_queues_action() {
        let envelope = SocketEnvelope {
            envelope_id: "E2".to_owned(),
            envelope_type: "slash_commands".to_owned(),
            accepts_response_payload: true,
            payload: serde_json::json!({
                "team_id": "T123",
                "channel_id": "D123",
                "user_id": "U123",
                "command": "/codex",
                "text": "do work"
            }),
        };

        let prepared = prepare_envelope(&test_config(), Instant::now(), envelope);

        assert!(prepared
            .ack
            .unwrap()
            .payload
            .unwrap()
            .text
            .contains("Starting Codex"));
        assert_eq!(
            prepared.action,
            Some(SocketAction::StartCodex {
                processed_event: ProcessedEvent::new("E2", None, "slash_commands"),
                command: SlashCommandPayload {
                    team_id: "T123".to_owned(),
                    channel_id: "D123".to_owned(),
                    user_id: "U123".to_owned(),
                    command: "/codex".to_owned(),
                    text: "do work".to_owned()
                }
            })
        );
    }

    #[test]
    fn invalid_workspace_prefix_returns_ephemeral_error_without_action() {
        let envelope = SocketEnvelope {
            envelope_id: "E2B".to_owned(),
            envelope_type: "slash_commands".to_owned(),
            accepts_response_payload: true,
            payload: serde_json::json!({
                "team_id": "T123",
                "channel_id": "D123",
                "user_id": "U123",
                "command": "/codex",
                "text": "--workspace"
            }),
        };

        let prepared = prepare_envelope(&test_config(), Instant::now(), envelope);
        let text = prepared.ack.unwrap().payload.unwrap().text;

        assert!(text.contains("Workspace option is invalid"));
        assert_eq!(prepared.action, None);
    }

    #[test]
    fn result_publish_plan_uses_message_under_limit() {
        assert_eq!(
            plan_result_publish("short", 5),
            ResultPublishPlan::Message("short".to_owned())
        );
    }

    #[test]
    fn result_publish_plan_uses_external_file_over_limit() {
        assert_eq!(
            plan_result_publish("123456", 5),
            ResultPublishPlan::ExternalFile {
                filename: "codex-output.txt".to_owned(),
                content: "123456".to_owned()
            }
        );
    }

    #[test]
    fn get_upload_url_request_omits_snippet_type_for_plain_text_file() {
        let request = GetUploadUrlExternalRequest {
            filename: "codex-output.txt",
            length: "a가".as_bytes().len(),
        };
        let encoded = serde_json::to_value(&request).unwrap();

        assert_eq!(
            encoded,
            serde_json::json!({
                "filename": "codex-output.txt",
                "length": 4
            })
        );
        assert!(encoded.get("snippet_type").is_none());
    }

    #[test]
    fn slack_api_error_includes_response_metadata_messages() {
        let error = slack_api_error(
            "files.getUploadURLExternal",
            Some("invalid_arguments".to_owned()),
            Some(SlackResponseMetadata {
                messages: vec!["[ERROR] unsupported field: snippet_type".to_owned()],
            }),
        );

        assert_eq!(
            error.to_string(),
            "slack api files.getUploadURLExternal failed with invalid_arguments; metadata: [ERROR] unsupported field: snippet_type"
        );
    }

    #[test]
    fn slack_api_error_redacts_token_like_metadata_messages() {
        let error = slack_api_error(
            "files.getUploadURLExternal",
            Some("invalid_arguments".to_owned()),
            Some(SlackResponseMetadata {
                messages: vec!["SLACK_BOT_TOKEN appeared here".to_owned()],
            }),
        );

        assert_eq!(
            error.to_string(),
            "slack api files.getUploadURLExternal failed with invalid_arguments; metadata: [redacted slack api diagnostic]"
        );
    }

    #[test]
    fn non_slash_envelope_is_acknowledged_without_payload() {
        let envelope = SocketEnvelope {
            envelope_id: "E3".to_owned(),
            envelope_type: "unknown".to_owned(),
            accepts_response_payload: true,
            payload: serde_json::json!({ "event": { "type": "message" } }),
        };

        assert_eq!(
            prepare_envelope(&test_config(), Instant::now(), envelope),
            PreparedSocketEvent {
                ack: Some(SocketAck {
                    envelope_id: "E3".to_owned(),
                    payload: None
                }),
                action: None
            }
        );
    }

    #[test]
    fn message_event_queues_resume_action() {
        let envelope = SocketEnvelope {
            envelope_id: "E4".to_owned(),
            envelope_type: "events_api".to_owned(),
            accepts_response_payload: false,
            payload: serde_json::json!({
                "event": {
                    "type": "message",
                    "channel": "D123",
                    "user": "U123",
                    "text": "continue",
                    "ts": "171.0002",
                    "thread_ts": "171.0001"
                }
            }),
        };

        let prepared = prepare_envelope(&test_config(), Instant::now(), envelope);

        assert_eq!(
            prepared.action,
            Some(SocketAction::ResumeCodex {
                processed_event: ProcessedEvent::new(
                    "E4",
                    Some("171.0001".to_owned()),
                    "events_api"
                ),
                event: SlackMessageEvent {
                    channel: "D123".to_owned(),
                    user: Some("U123".to_owned()),
                    text: "continue".to_owned(),
                    ts: "171.0002".to_owned(),
                    thread_ts: Some("171.0001".to_owned())
                }
            })
        );
    }

    #[test]
    fn event_key_priority_matches_architecture() {
        let payload = serde_json::json!({
            "event_id": "Ev1",
            "event": {
                "client_msg_id": "Cm1",
                "channel": "D123",
                "ts": "171.0002",
                "user": "U123"
            }
        });
        assert_eq!(
            slack_event_key(Some("Envelope1"), &payload).as_deref(),
            Some("Envelope1")
        );
        assert_eq!(slack_event_key(None, &payload).as_deref(), Some("Ev1"));

        let no_event_id = serde_json::json!({
            "event": {
                "client_msg_id": "Cm1",
                "channel": "D123",
                "ts": "171.0002",
                "user": "U123"
            }
        });
        assert_eq!(slack_event_key(None, &no_event_id).as_deref(), Some("Cm1"));

        let fallback = serde_json::json!({
            "event": {
                "channel": "D123",
                "ts": "171.0002",
                "user": "U123"
            }
        });
        assert_eq!(
            slack_event_key(None, &fallback).as_deref(),
            Some("D123:171.0002:U123")
        );
    }

    #[test]
    fn bot_message_event_is_ignored() {
        let payload = serde_json::json!({
            "event": {
                "type": "message",
                "bot_id": "B123",
                "channel": "D123",
                "text": "from bot",
                "ts": "171.0002"
            }
        });

        assert_eq!(parse_message_event(payload), None);
    }

    #[test]
    fn ack_serialization_does_not_include_tokens() {
        let started_at = Instant::now();
        let envelope = SocketEnvelope {
            envelope_id: "E5".to_owned(),
            envelope_type: "slash_commands".to_owned(),
            accepts_response_payload: true,
            payload: serde_json::json!({
                "team_id": "T123",
                "channel_id": "D123",
                "user_id": "U123",
                "command": "/codex-ping"
            }),
        };

        let ack = prepare_envelope(&test_config(), started_at, envelope)
            .ack
            .unwrap();
        let encoded = serde_json::to_string(&ack).unwrap();

        assert!(!encoded.contains("xoxb-test-secret"));
        assert!(!encoded.contains("xapp-test-secret"));
        assert!(encoded.contains("pong from desk"));
    }

    #[test]
    fn disallowed_user_gets_sanitized_response() {
        let envelope = SocketEnvelope {
            envelope_id: "E6".to_owned(),
            envelope_type: "slash_commands".to_owned(),
            accepts_response_payload: true,
            payload: serde_json::json!({
                "team_id": "T123",
                "channel_id": "D123",
                "user_id": "U999",
                "command": "/codex-ping"
            }),
        };

        let ack = prepare_envelope(&test_config(), Instant::now(), envelope)
            .ack
            .unwrap();
        let text = ack.payload.unwrap().text;

        assert_eq!(
            text,
            "This Slack team or user is not allowed to use this host."
        );
        assert!(!text.contains("U999"));
    }
}
