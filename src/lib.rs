pub mod codex;
pub mod config;
pub mod lifecycle;
pub mod sessions;
pub mod slack;

use std::{env, path::PathBuf, sync::Arc, time::Instant};

use codex::{ChildEnvPolicy, CodexCli, WorkspacePolicy};
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
    #[error(transparent)]
    Codex(#[from] codex::CodexError),
}

pub async fn run() -> Result<(), AppError> {
    let dotenv_load = load_dotenv_files();
    init_tracing();

    let config = AppConfig::from_env()?;
    tracing::info!(host = %config.bot_hostname, "starting slack-codex");
    let workspace_policy = WorkspacePolicy::new(
        config.allowed_workspaces.clone(),
        config.default_workspace.clone(),
    );
    tracing::info!(
        host = %config.bot_hostname,
        dotenv_from_exe_dir = dotenv_load.exe_dir,
        dotenv_from_cwd_search = dotenv_load.cwd_search,
        default_workspace_configured = config.default_workspace.is_some(),
        allowed_workspace_count = config.allowed_workspaces.len(),
        "loaded slack-codex config"
    );
    if config.default_workspace.is_some() {
        workspace_policy.validate(None)?;
        tracing::info!(host = %config.bot_hostname, "default workspace validated");
    }

    let api = SlackApiClient::new(
        config.slack_bot_token.clone(),
        config.slack_app_token.clone(),
    );
    let sessions = SqliteStateStore::shared(&config.queue_db_path)?;
    sessions.recover_running_sessions()?;
    let codex = CodexCli::shared(
        config.codex_cli_path.clone(),
        config.max_session_timeout_secs,
        ChildEnvPolicy::new(config.child_env_allowlist.clone()),
        workspace_policy,
    );
    let lifecycle = SessionLifecycle::shared(
        codex,
        Arc::new(api.clone()),
        sessions,
        config.codex_output_max_chars,
    );
    let runner = SocketModeRunner::new(config, api, lifecycle, Instant::now());
    runner.run().await?;

    Ok(())
}

#[derive(Debug, Default)]
struct DotenvLoad {
    exe_dir: bool,
    cwd_search: bool,
}

fn load_dotenv_files() -> DotenvLoad {
    let mut loaded = DotenvLoad::default();

    if let Ok(exe_path) = env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            let env_path: PathBuf = exe_dir.join(".env");
            if env_path.is_file() && dotenvy::from_path_override(&env_path).is_ok() {
                loaded.exe_dir = true;
            }
        }
    }

    if dotenvy::dotenv().is_ok() {
        loaded.cwd_search = true;
    }

    loaded
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}
