use std::{
    collections::{HashMap, HashSet},
    env, fmt,
    path::PathBuf,
};

#[derive(Clone, Eq, PartialEq)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[redacted]")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppConfig {
    pub slack_bot_token: SecretString,
    pub slack_app_token: SecretString,
    pub allowed_team_id: Option<String>,
    pub allowed_user_ids: HashSet<String>,
    pub bot_hostname: String,
    pub max_session_timeout_secs: u64,
    pub codex_output_max_chars: usize,
    pub codex_cli_path: PathBuf,
    pub queue_db_path: PathBuf,
    pub child_env_allowlist: Vec<String>,
    pub default_workspace: Option<PathBuf>,
    pub allowed_workspaces: Vec<PathBuf>,
}

impl AppConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| env::var(name).ok())
    }

    pub fn from_map(values: &HashMap<String, String>) -> Result<Self, ConfigError> {
        Self::from_lookup(|name| values.get(name).cloned())
    }

    fn from_lookup(mut lookup: impl FnMut(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let slack_bot_token = required_secret(&mut lookup, "SLACK_BOT_TOKEN")?;
        let slack_app_token = required_secret(&mut lookup, "SLACK_APP_TOKEN")?;
        let allowed_team_id = optional_string(&mut lookup, "SLACK_ALLOWED_TEAM_ID");
        let allowed_user_ids = optional_string(&mut lookup, "SLACK_ALLOWED_USER_IDS")
            .map(|ids| split_csv(&ids))
            .unwrap_or_default();
        let bot_hostname =
            optional_string(&mut lookup, "BOT_HOSTNAME").unwrap_or_else(hostname_fallback);
        let max_session_timeout_secs = optional_u64(&mut lookup, "MAX_SESSION_TIMEOUT_SECS", 600)?;
        let codex_output_max_chars = optional_usize(&mut lookup, "CODEX_OUTPUT_MAX_CHARS", 39_000)?;
        let codex_cli_path = optional_string(&mut lookup, "CODEX_CLI_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("codex"));
        let queue_db_path = optional_string(&mut lookup, "QUEUE_DB_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("./data/sessions.db"));
        let child_env_allowlist = optional_string(&mut lookup, "CODEX_CHILD_ENV_ALLOWLIST")
            .map(|value| split_csv_vec(&value))
            .unwrap_or_else(default_child_env_allowlist);
        let default_workspace =
            optional_string(&mut lookup, "CODEX_DEFAULT_WORKSPACE").map(PathBuf::from);
        let allowed_workspaces = optional_string(&mut lookup, "CODEX_ALLOWED_WORKSPACES")
            .map(|value| split_path_list(&value))
            .unwrap_or_default();

        Ok(Self {
            slack_bot_token,
            slack_app_token,
            allowed_team_id,
            allowed_user_ids,
            bot_hostname,
            max_session_timeout_secs,
            codex_output_max_chars,
            codex_cli_path,
            queue_db_path,
            child_env_allowlist,
            default_workspace,
            allowed_workspaces,
        })
    }
}

fn required_secret(
    lookup: &mut impl FnMut(&str) -> Option<String>,
    name: &'static str,
) -> Result<SecretString, ConfigError> {
    optional_string(lookup, name)
        .map(SecretString::new)
        .ok_or(ConfigError::MissingVar(name))
}

fn optional_string(lookup: &mut impl FnMut(&str) -> Option<String>, name: &str) -> Option<String> {
    lookup(name)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn optional_u64(
    lookup: &mut impl FnMut(&str) -> Option<String>,
    name: &'static str,
    default: u64,
) -> Result<u64, ConfigError> {
    match optional_string(lookup, name) {
        Some(value) => value
            .parse()
            .map_err(|_| ConfigError::InvalidNumber { name, value }),
        None => Ok(default),
    }
}

fn optional_usize(
    lookup: &mut impl FnMut(&str) -> Option<String>,
    name: &'static str,
    default: usize,
) -> Result<usize, ConfigError> {
    match optional_string(lookup, name) {
        Some(value) => value
            .parse()
            .map_err(|_| ConfigError::InvalidNumber { name, value }),
        None => Ok(default),
    }
}

fn split_csv(value: &str) -> HashSet<String> {
    split_csv_vec(value).into_iter().collect()
}

fn split_csv_vec(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn split_path_list(value: &str) -> Vec<PathBuf> {
    value
        .split([';', ','])
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(PathBuf::from)
        .collect()
}

fn default_child_env_allowlist() -> Vec<String> {
    [
        "HOME",
        "PATH",
        "USER",
        "SHELL",
        "CODEX_HOME",
        "CODEX_PROFILE_ROOT",
    ]
    .into_iter()
    .map(ToOwned::to_owned)
    .collect()
}

fn hostname_fallback() -> String {
    env::var("COMPUTERNAME")
        .or_else(|_| env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown-host".to_owned())
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum ConfigError {
    #[error("missing required environment variable {0}")]
    MissingVar(&'static str),
    #[error("environment variable {name} must be a number, got {value:?}")]
    InvalidNumber { name: &'static str, value: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_name_is_stable() {
        assert_eq!(env!("CARGO_PKG_NAME"), "slack-codex");
    }

    #[test]
    fn missing_credentials_fail_fast() {
        let values = HashMap::new();
        assert_eq!(
            AppConfig::from_map(&values).unwrap_err(),
            ConfigError::MissingVar("SLACK_BOT_TOKEN")
        );
    }

    #[test]
    fn loads_required_and_optional_values() {
        let values = HashMap::from([
            ("SLACK_BOT_TOKEN".to_owned(), "xoxb-test".to_owned()),
            ("SLACK_APP_TOKEN".to_owned(), "xapp-test".to_owned()),
            ("SLACK_ALLOWED_TEAM_ID".to_owned(), "T123".to_owned()),
            ("SLACK_ALLOWED_USER_IDS".to_owned(), "U1, U2".to_owned()),
            ("BOT_HOSTNAME".to_owned(), "desk".to_owned()),
            ("MAX_SESSION_TIMEOUT_SECS".to_owned(), "30".to_owned()),
            ("CODEX_OUTPUT_MAX_CHARS".to_owned(), "1000".to_owned()),
            ("CODEX_CLI_PATH".to_owned(), "/bin/codex".to_owned()),
            ("QUEUE_DB_PATH".to_owned(), "./tmp/test.db".to_owned()),
            (
                "CODEX_CHILD_ENV_ALLOWLIST".to_owned(),
                "HOME,PATH,SLACK_BOT_TOKEN".to_owned(),
            ),
            ("CODEX_DEFAULT_WORKSPACE".to_owned(), "./a".to_owned()),
            ("CODEX_ALLOWED_WORKSPACES".to_owned(), "./a;./b".to_owned()),
        ]);

        let config = AppConfig::from_map(&values).unwrap();

        assert_eq!(config.slack_bot_token.expose(), "xoxb-test");
        assert_eq!(config.slack_app_token.expose(), "xapp-test");
        assert_eq!(config.allowed_team_id.as_deref(), Some("T123"));
        assert!(config.allowed_user_ids.contains("U1"));
        assert!(config.allowed_user_ids.contains("U2"));
        assert_eq!(config.bot_hostname, "desk");
        assert_eq!(config.max_session_timeout_secs, 30);
        assert_eq!(config.codex_output_max_chars, 1000);
        assert_eq!(config.codex_cli_path, PathBuf::from("/bin/codex"));
        assert_eq!(config.queue_db_path, PathBuf::from("./tmp/test.db"));
        assert_eq!(
            config.child_env_allowlist,
            vec![
                "HOME".to_owned(),
                "PATH".to_owned(),
                "SLACK_BOT_TOKEN".to_owned()
            ]
        );
        assert_eq!(config.default_workspace, Some(PathBuf::from("./a")));
        assert_eq!(
            config.allowed_workspaces,
            vec![PathBuf::from("./a"), PathBuf::from("./b")]
        );
    }

    #[test]
    fn secrets_debug_as_redacted() {
        assert_eq!(
            format!("{:?}", SecretString::new("xoxb-secret")),
            "[redacted]"
        );
    }
}
