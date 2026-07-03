use std::{
    collections::HashMap,
    env,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use serde_json::Value;
use tokio::{process::Command, time};

pub const BLOCKED_CHILD_ENV: [&str; 3] =
    ["SLACK_BOT_TOKEN", "SLACK_APP_TOKEN", "SLACK_SIGNING_SECRET"];

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct CodexRequest {
    pub prompt: String,
    pub workspace: Option<PathBuf>,
}

impl CodexRequest {
    pub fn parse(input: &str) -> Result<Self, CodexError> {
        let input = input.trim();
        if matches!(input, "--workspace" | "--cd") {
            return Err(CodexError::InvalidWorkspaceRequest);
        }
        for prefix in ["--workspace=", "--cd="] {
            if let Some(rest) = input.strip_prefix(prefix) {
                let (workspace, prompt) = parse_workspace_value(rest)?;
                return Ok(Self { prompt, workspace });
            }
        }
        for prefix in ["--workspace ", "--cd "] {
            if let Some(rest) = input.strip_prefix(prefix) {
                let (workspace, prompt) = parse_workspace_value(rest)?;
                return Ok(Self { prompt, workspace });
            }
        }

        Ok(Self {
            prompt: input.to_owned(),
            workspace: None,
        })
    }
}

fn parse_workspace_value(value: &str) -> Result<(Option<PathBuf>, String), CodexError> {
    let value = value.trim_start();
    if value.is_empty() {
        return Err(CodexError::InvalidWorkspaceRequest);
    }

    if let Some(rest) = value.strip_prefix('"') {
        let Some(end) = rest.find('"') else {
            return Err(CodexError::InvalidWorkspaceRequest);
        };
        let workspace = &rest[..end];
        let prompt = rest[end + 1..].trim_start();
        return Ok((Some(PathBuf::from(workspace)), prompt.to_owned()));
    }

    let (workspace, prompt) = value.split_once(char::is_whitespace).unwrap_or((value, ""));
    Ok((
        Some(PathBuf::from(workspace)),
        prompt.trim_start().to_owned(),
    ))
}

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
    async fn start_session(&self, request: CodexRequest) -> Result<CodexSessionOutput, CodexError>;
    async fn resume_session(
        &self,
        session_id: &str,
        request: CodexRequest,
    ) -> Result<CodexResumeOutput, CodexError>;
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ChildEnvPolicy {
    allowed_names: Vec<String>,
}

impl ChildEnvPolicy {
    pub fn new(allowed_names: Vec<String>) -> Self {
        let allowed_names = allowed_names
            .into_iter()
            .filter(|name| !is_blocked_child_env(name))
            .collect();
        Self { allowed_names }
    }

    pub fn collect_from_current(&self) -> Vec<(String, String)> {
        let env_map = env::vars().collect::<HashMap<_, _>>();
        self.collect_from_map(&env_map)
    }

    pub fn collect_from_map(&self, values: &HashMap<String, String>) -> Vec<(String, String)> {
        self.allowed_names
            .iter()
            .filter(|name| !is_blocked_child_env(name))
            .filter_map(|name| values.get(name).map(|value| (name.clone(), value.clone())))
            .collect()
    }
}

fn is_blocked_child_env(name: &str) -> bool {
    BLOCKED_CHILD_ENV
        .iter()
        .any(|blocked| blocked.eq_ignore_ascii_case(name))
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct WorkspacePolicy {
    allowed_roots: Vec<PathBuf>,
}

impl WorkspacePolicy {
    pub fn new(allowed_roots: Vec<PathBuf>) -> Self {
        Self { allowed_roots }
    }

    pub fn validate(&self, requested: Option<&Path>) -> Result<PathBuf, CodexError> {
        let requested = match requested {
            Some(path) => path.to_path_buf(),
            None => env::current_dir()?,
        };
        let requested = requested
            .canonicalize()
            .map_err(|_| CodexError::WorkspaceNotAllowed)?;

        for root in &self.allowed_roots {
            let Ok(root) = root.canonicalize() else {
                continue;
            };
            if requested == root || requested.starts_with(&root) {
                return Ok(requested);
            }
        }

        Err(CodexError::WorkspaceNotAllowed)
    }
}

pub struct CodexCli {
    timeout: Duration,
    env_policy: ChildEnvPolicy,
    workspace_policy: WorkspacePolicy,
}

impl CodexCli {
    pub fn new(
        timeout_secs: u64,
        env_policy: ChildEnvPolicy,
        workspace_policy: WorkspacePolicy,
    ) -> Self {
        Self {
            timeout: Duration::from_secs(timeout_secs),
            env_policy,
            workspace_policy,
        }
    }

    pub fn shared(
        timeout_secs: u64,
        env_policy: ChildEnvPolicy,
        workspace_policy: WorkspacePolicy,
    ) -> Arc<Self> {
        Arc::new(Self::new(timeout_secs, env_policy, workspace_policy))
    }
}

#[async_trait]
impl CodexExecutor for CodexCli {
    async fn start_session(&self, request: CodexRequest) -> Result<CodexSessionOutput, CodexError> {
        let workspace = self
            .workspace_policy
            .validate(request.workspace.as_deref())?;
        let output = run_codex(
            vec![
                "exec".to_owned(),
                "--json".to_owned(),
                "--cd".to_owned(),
                workspace.to_string_lossy().into_owned(),
                request.prompt,
            ],
            self.timeout,
            &self.env_policy,
        )
        .await?;
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
        request: CodexRequest,
    ) -> Result<CodexResumeOutput, CodexError> {
        if request.workspace.is_some() {
            return Err(CodexError::WorkspaceOnlyOnStart);
        }

        let output = run_codex(
            vec![
                "exec".to_owned(),
                "resume".to_owned(),
                session_id.to_owned(),
                request.prompt,
            ],
            self.timeout,
            &self.env_policy,
        )
        .await?;
        ensure_success(&output)?;

        Ok(CodexResumeOutput {
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

async fn run_codex(
    args: Vec<String>,
    timeout: Duration,
    env_policy: &ChildEnvPolicy,
) -> Result<ProcessOutput, CodexError> {
    let child = codex_command(env_policy)
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

fn codex_command(env_policy: &ChildEnvPolicy) -> Command {
    let mut command = Command::new("codex");
    command.env_clear();
    for (name, value) in env_policy.collect_from_current() {
        command.env(name, value);
    }
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

pub fn display_output(stdout: &str) -> String {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return "Codex completed without text output.".to_owned();
    }
    redact_sensitive_text(trimmed)
}

pub fn redact_sensitive_text(value: &str) -> String {
    value
        .lines()
        .map(|line| {
            if BLOCKED_CHILD_ENV.iter().any(|name| line.contains(name)) {
                "[redacted sensitive output line]"
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
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
    #[error("invalid workspace request")]
    InvalidWorkspaceRequest,
    #[error("workspace is not allowed")]
    WorkspaceNotAllowed,
    #[error("workspace can only be selected when starting a session")]
    WorkspaceOnlyOnStart,
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
                let stderr_tail = redact_sensitive_text(stderr_tail.trim());
                if stderr_tail.is_empty() {
                    format!("Codex command exited with status {status_code:?}.")
                } else {
                    format!(
                        "Codex command exited with status {status_code:?}. Error tail:\n```text\n{}\n```",
                        stderr_tail
                    )
                }
            }
            Self::MissingSessionId => {
                "Codex completed, but no session_id was found in JSON output.".to_owned()
            }
            Self::InvalidWorkspaceRequest => {
                "Workspace option is invalid. Use `--workspace <path> <prompt>`.".to_owned()
            }
            Self::WorkspaceNotAllowed => {
                "Requested workspace is not in CODEX_ALLOWED_WORKSPACES.".to_owned()
            }
            Self::WorkspaceOnlyOnStart => {
                "Workspace can only be selected when starting a new `/codex` session.".to_owned()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    fn unique_temp_dir(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        env::temp_dir().join(format!("slack-codex-{name}-{stamp}"))
    }

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
    fn child_env_policy_allows_only_named_non_slack_vars() {
        let policy = ChildEnvPolicy::new(vec![
            "HOME".to_owned(),
            "SLACK_BOT_TOKEN".to_owned(),
            "PATH".to_owned(),
        ]);
        let values = HashMap::from([
            ("HOME".to_owned(), "/home/test".to_owned()),
            ("PATH".to_owned(), "/bin".to_owned()),
            ("SLACK_BOT_TOKEN".to_owned(), "xoxb-secret".to_owned()),
            ("OTHER".to_owned(), "value".to_owned()),
        ]);

        let collected = policy.collect_from_map(&values);

        assert_eq!(
            collected,
            vec![
                ("HOME".to_owned(), "/home/test".to_owned()),
                ("PATH".to_owned(), "/bin".to_owned())
            ]
        );
    }

    #[test]
    fn workspace_policy_allows_child_path_and_rejects_sibling() {
        let root = unique_temp_dir("allowed");
        let child = root.join("child");
        let sibling = unique_temp_dir("sibling");
        fs::create_dir_all(&child).unwrap();
        fs::create_dir_all(&sibling).unwrap();
        let policy = WorkspacePolicy::new(vec![root.clone()]);

        assert_eq!(
            policy.validate(Some(&child)).unwrap(),
            child.canonicalize().unwrap()
        );
        assert!(matches!(
            policy.validate(Some(&sibling)),
            Err(CodexError::WorkspaceNotAllowed)
        ));
    }

    #[test]
    fn workspace_policy_rejects_traversal_outside_allowed_root() {
        let root = unique_temp_dir("root");
        let sibling = unique_temp_dir("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&sibling).unwrap();
        let requested = root.join("..").join(sibling.file_name().unwrap());
        let policy = WorkspacePolicy::new(vec![root]);

        assert!(matches!(
            policy.validate(Some(&requested)),
            Err(CodexError::WorkspaceNotAllowed)
        ));
    }

    #[test]
    fn codex_request_parses_workspace_prefixes() {
        let request = CodexRequest::parse(r#"--workspace "C:\repo path" do work"#).unwrap();

        assert_eq!(request.workspace, Some(PathBuf::from(r#"C:\repo path"#)));
        assert_eq!(request.prompt, "do work");

        let request = CodexRequest::parse("--cd=./repo do work").unwrap();
        assert_eq!(request.workspace, Some(PathBuf::from("./repo")));
        assert_eq!(request.prompt, "do work");
    }

    #[test]
    fn blocked_child_env_names_include_slack_secrets() {
        assert!(BLOCKED_CHILD_ENV.contains(&"SLACK_BOT_TOKEN"));
        assert!(BLOCKED_CHILD_ENV.contains(&"SLACK_APP_TOKEN"));
        assert!(BLOCKED_CHILD_ENV.contains(&"SLACK_SIGNING_SECRET"));
    }

    #[test]
    fn display_output_redacts_sensitive_lines() {
        let output = "ok\nSLACK_BOT_TOKEN=xoxb-secret\nstill ok";

        assert_eq!(
            display_output(output),
            "ok\n[redacted sensitive output line]\nstill ok"
        );
    }
}
