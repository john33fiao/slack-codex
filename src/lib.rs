pub mod config;
pub mod slack;

use std::time::Instant;

use config::AppConfig;
use slack::{SlackApiClient, SocketModeRunner};

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error(transparent)]
    Config(#[from] config::ConfigError),
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
    let runner = SocketModeRunner::new(config, api, Instant::now());
    runner.run().await?;

    Ok(())
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}
