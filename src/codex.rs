use std::{process::Stdio, sync::Arc, time::Duration};

use async_trait::async_trait;
use serde_json::Value;
use tokio::{process::Command, time};

pub const BLOCKED_CHILD_ENV: [&str; 3] =
    ["SLACK_BOT_TOKEN", "SLACK_APP_TOKEN", "SLACK_SIGNING_SECRET"];

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct CodexSessionOutput {
    pub session_id: String,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct CodexResumeOutput {
    pub stdout: String,
    pub stderr: String,
}

#[async_trait]
pub trait CodexExecutor: Send + Sync {
    async fn start_session(&self, prompt: &str) -> Result<CodexSessionOutput, CodexError>;
    async fn resume_session(
        &self,
        session_id: &str,
        prompt: &str,
    ) -> Result<CodexResumeOutput, CodexError>;
}

pub struct CodexCli {
    timeout: Duration,
}

impl CodexCli {
    pub fn new(timeout_secs: u64) -> Self {
        Self {
            timeout: Duration::from_secs(timeout_secs),
        }
    }

    pub fn shared(timeout_secs: u64) -> Arc<Self> {
        Arc::new(Self::new(timeout_secs))
    }
}

#[async_trait]
impl CodexExecutor for CodexCli {
    async fn start_session(&self, prompt: &str) -> Result<CodexSessionOutput, CodexError> {
        let output = run_codex(["exec", "--json", prompt], self.timeout).await?;
        ensure_success(&output)?;
        let session_id = parse_session_id(&output.stdout).ok_or(CodexError::MissingSessionId)?;

        Ok(CodexSessionOutput {
            session_id,
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    async fn resume_session(
        &self,
        session_id: &str,
        prompt: &str,
    ) -> Result<CodexResumeOutput, CodexError> {
        let output = run_codex(["exec", "resume", session_id, prompt], self.timeout).await?;
        ensure_success(&output)?;

        Ok(CodexResumeOutput {
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

async fn run_codex<const N: usize>(
    args: [&str; N],
    timeout: Duration,
) -> Result<ProcessOutput, CodexError> {
    let child = codex_command()
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;

    let output = time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| CodexError::Timeout { timeout })??;

    Ok(ProcessOutput {
        status_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

fn codex_command() -> Command {
    let mut command = Command::new("codex");
    for name in BLOCKED_CHILD_ENV {
        command.env_remove(name);
    }
    command
}

fn ensure_success(output: &ProcessOutput) -> Result<(), CodexError> {
    if output.status_code == Some(0) {
        Ok(())
    } else {
        Err(CodexError::Failed {
            status_code: output.status_code,
            stdout_tail: tail(&output.stdout, 1200),
            stderr_tail: tail(&output.stderr, 1200),
        })
    }
}

pub fn parse_session_id(jsonl: &str) -> Option<String> {
    jsonl
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find_map(|value| {
            find_string_key(&value, "session_id").or_else(|| find_string_key(&value, "thread_id"))
        })
}

fn find_string_key(value: &Value, key: &str) -> Option<String> {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(value)) = map.get(key) {
                return Some(value.clone());
            }

            map.values().find_map(|value| find_string_key(value, key))
        }
        Value::Array(values) => values.iter().find_map(|value| find_string_key(value, key)),
        _ => None,
    }
}

pub fn tail(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars().rev().take(max_chars).collect::<Vec<_>>();
    chars.reverse();
    chars.into_iter().collect()
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct ProcessOutput {
    status_code: Option<i32>,
    stdout: String,
    stderr: String,
}

#[derive(Debug, thiserror::Error)]
pub enum CodexError {
    #[error("codex executable failed to start: {0}")]
    Io(#[from] std::io::Error),
    #[error("codex command timed out after {timeout:?}")]
    Timeout { timeout: Duration },
    #[error("codex command exited with status {status_code:?}; stderr tail: {stderr_tail}")]
    Failed {
        status_code: Option<i32>,
        stdout_tail: String,
        stderr_tail: String,
    },
    #[error("codex JSON output did not include a session_id")]
    MissingSessionId,
}

impl CodexError {
    pub fn user_message(&self) -> String {
        match self {
            Self::Io(_) => "Codex CLI could not be started on this host.".to_owned(),
            Self::Timeout { timeout } => {
                format!("Codex command timed out after {}s.", timeout.as_secs())
            }
            Self::Failed {
                status_code,
                stderr_tail,
                ..
            } => {
                if stderr_tail.trim().is_empty() {
                    format!("Codex command exited with status {status_code:?}.")
                } else {
                    format!(
                        "Codex command exited with status {status_code:?}. Error tail:\n```text\n{}\n```",
                        stderr_tail.trim()
                    )
                }
            }
            Self::MissingSessionId => {
                "Codex completed, but no session_id was found in JSON output.".to_owned()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_session_id_from_json_lines() {
        let output = r#"{"type":"event","message":"hello"}
{"type":"session","session_id":"019f-session"}"#;

        assert_eq!(parse_session_id(output).as_deref(), Some("019f-session"));
    }

    #[test]
    fn parses_nested_session_id() {
        let output = r#"{"type":"event","payload":{"session_id":"nested-session"}}"#;

        assert_eq!(parse_session_id(output).as_deref(), Some("nested-session"));
    }

    #[test]
    fn parses_current_cli_thread_id_as_session_id() {
        let output = r#"{"type":"thread.started","thread_id":"019f2599-235d"}"#;

        assert_eq!(parse_session_id(output).as_deref(), Some("019f2599-235d"));
    }

    #[test]
    fn ignores_non_json_lines() {
        let output = "not json\n{\"type\":\"done\"}";

        assert_eq!(parse_session_id(output), None);
    }

    #[test]
    fn tail_limits_by_chars() {
        assert_eq!(tail("abcdef", 3), "def");
        assert_eq!(tail("가나다라마", 2), "라마");
    }

    #[test]
    fn blocked_child_env_names_include_slack_secrets() {
        assert!(BLOCKED_CHILD_ENV.contains(&"SLACK_BOT_TOKEN"));
        assert!(BLOCKED_CHILD_ENV.contains(&"SLACK_APP_TOKEN"));
        assert!(BLOCKED_CHILD_ENV.contains(&"SLACK_SIGNING_SECRET"));
    }
}
