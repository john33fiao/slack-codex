pub mod codex;
pub mod config;
pub mod lifecycle;
pub mod sessions;
pub mod slack;

use std::{sync::Arc, time::Instant};

use codex::CodexCli;
use config::AppConfig;
use lifecycle::SessionLifecycle;
use sessions::{SessionStore, SqliteStateStore};
use slack::{SlackApiClient, SocketModeRunner};

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error(transparent)]
    Config(#[from] config::ConfigError),
    #[error(transparent)]
    State(#[from] sessions::StateError),
    #[error(transparent)]
    Slack(#[from] slack::SlackError),
}

pub async fn run() -> Result<(), AppError> {
    let _ = dotenvy::dotenv();
    init_tracing();

    let config = AppConfig::from_env()?;
    tracing::info!(host = %config.bot_hostname, "starting slack-codex");

    let api = SlackApiClient::new(
        config.slack_bot_token.clone(),
        config.slack_app_token.clone(),
    );
    let sessions = SqliteStateStore::shared(&config.queue_db_path)?;
    sessions.recover_running_sessions()?;
    let codex = CodexCli::shared(config.max_session_timeout_secs);
    let lifecycle = SessionLifecycle::shared(codex, Arc::new(api.clone()), sessions);
    let runner = SocketModeRunner::new(config, api, lifecycle, Instant::now());
    runner.run().await?;

    Ok(())
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}
