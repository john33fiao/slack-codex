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
    pub workspace: PathBuf,
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
        workspace: Option<PathBuf>,
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
    default_workspace: Option<PathBuf>,
}

impl WorkspacePolicy {
    pub fn new(allowed_roots: Vec<PathBuf>, default_workspace: Option<PathBuf>) -> Self {
        Self {
            allowed_roots,
            default_workspace,
        }
    }

    pub fn validate(&self, requested: Option<&Path>) -> Result<PathBuf, CodexError> {
        let requested = match requested {
            Some(path) => path.to_path_buf(),
            None => self
                .default_workspace
                .clone()
                .map(Ok)
                .unwrap_or_else(env::current_dir)?,
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
    executable: PathBuf,
    timeout: Duration,
    env_policy: ChildEnvPolicy,
    workspace_policy: WorkspacePolicy,
}

impl CodexCli {
    pub fn new(
        executable: PathBuf,
        timeout_secs: u64,
        env_policy: ChildEnvPolicy,
        workspace_policy: WorkspacePolicy,
    ) -> Self {
        Self {
            executable,
            timeout: Duration::from_secs(timeout_secs),
            env_policy,
            workspace_policy,
        }
    }

    pub fn shared(
        executable: PathBuf,
        timeout_secs: u64,
        env_policy: ChildEnvPolicy,
        workspace_policy: WorkspacePolicy,
    ) -> Arc<Self> {
        Arc::new(Self::new(
            executable,
            timeout_secs,
            env_policy,
            workspace_policy,
        ))
    }
}

#[async_trait]
impl CodexExecutor for CodexCli {
    async fn start_session(&self, request: CodexRequest) -> Result<CodexSessionOutput, CodexError> {
        let workspace = self
            .workspace_policy
            .validate(request.workspace.as_deref())?;
        let output = run_codex(
            &self.executable,
            start_args(&workspace, request.prompt),
            self.timeout,
            &self.env_policy,
        )
        .await?;
        ensure_success(&output)?;
        let session_id = parse_session_id(&output.stdout).ok_or(CodexError::MissingSessionId)?;

        Ok(CodexSessionOutput {
            session_id,
            workspace,
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    async fn resume_session(
        &self,
        session_id: &str,
        workspace: Option<PathBuf>,
        request: CodexRequest,
    ) -> Result<CodexResumeOutput, CodexError> {
        if request.workspace.is_some() {
            return Err(CodexError::WorkspaceOnlyOnStart);
        }
        let workspace = self.workspace_policy.validate(workspace.as_deref())?;

        let output = run_codex(
            &self.executable,
            resume_args(&workspace, session_id, request.prompt),
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

fn start_args(workspace: &Path, prompt: String) -> Vec<String> {
    vec![
        "exec".to_owned(),
        "--json".to_owned(),
        "--cd".to_owned(),
        workspace.to_string_lossy().into_owned(),
        prompt,
    ]
}

fn resume_args(workspace: &Path, session_id: &str, prompt: String) -> Vec<String> {
    vec![
        "exec".to_owned(),
        "--json".to_owned(),
        "--cd".to_owned(),
        workspace.to_string_lossy().into_owned(),
        "resume".to_owned(),
        session_id.to_owned(),
        prompt,
    ]
}

async fn run_codex(
    executable: &Path,
    args: Vec<String>,
    timeout: Duration,
    env_policy: &ChildEnvPolicy,
) -> Result<ProcessOutput, CodexError> {
    let child = codex_command(executable, env_policy)
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

fn codex_command(executable: &Path, env_policy: &ChildEnvPolicy) -> Command {
    let mut command = Command::new(executable);
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
    let rendered = render_codex_jsonl_output(trimmed).unwrap_or_else(|| trimmed.to_owned());
    redact_sensitive_text(&rendered)
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

fn render_codex_jsonl_output(jsonl: &str) -> Option<String> {
    let mut saw_json = false;
    let mut display_id = None;
    let mut assistant_text = None;
    let mut usage = None;

    for line in jsonl.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        saw_json = true;

        if display_id.is_none() {
            display_id = find_string_key(&value, "thread_id")
                .or_else(|| find_string_key(&value, "session_id"));
        }

        if is_event_type(&value, "item.completed") {
            if let Some(text) = value
                .get("item")
                .and_then(extract_agent_message_text)
                .filter(|text| !text.trim().is_empty())
            {
                assistant_text = Some(text);
            }
        }

        if is_event_type(&value, "turn.completed") {
            usage = value.get("usage").and_then(extract_token_usage).or(usage);
        }
    }

    if !saw_json || (display_id.is_none() && assistant_text.is_none() && usage.is_none()) {
        return None;
    }

    let mut parts = Vec::new();
    if let Some(display_id) = display_id {
        parts.push(display_id);
    }
    parts.push(
        assistant_text.unwrap_or_else(|| "Codex completed without assistant text.".to_owned()),
    );
    if let Some(usage) = usage {
        parts.push(format!(
            "_tokens: in {} / out {}_",
            format_token_count(usage.input),
            format_token_count(usage.output)
        ));
    }

    Some(parts.join("\n\n"))
}

fn is_event_type(value: &Value, expected: &str) -> bool {
    value.get("type").and_then(Value::as_str) == Some(expected)
}

fn extract_agent_message_text(item: &Value) -> Option<String> {
    let is_agent_message = item.get("type").and_then(Value::as_str) == Some("agent_message")
        || item.get("role").and_then(Value::as_str) == Some("assistant");
    if !is_agent_message {
        return None;
    }

    string_field(item, "text")
        .or_else(|| string_field(item, "message"))
        .or_else(|| item.get("content").and_then(extract_content_text))
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(ToOwned::to_owned)
}

fn extract_content_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => non_empty_text(text),
        Value::Array(items) => {
            let text = items
                .iter()
                .filter_map(extract_content_text)
                .collect::<Vec<_>>()
                .join("\n");
            non_empty_text(&text)
        }
        Value::Object(_) => string_field(value, "text").or_else(|| string_field(value, "content")),
        _ => None,
    }
}

fn non_empty_text(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_owned())
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct TokenUsage {
    input: u64,
    output: u64,
}

fn extract_token_usage(value: &Value) -> Option<TokenUsage> {
    Some(TokenUsage {
        input: number_field(
            value,
            &["input_tokens", "prompt_tokens", "total_input_tokens"],
        )?,
        output: number_field(
            value,
            &["output_tokens", "completion_tokens", "total_output_tokens"],
        )?,
    })
}

fn number_field(value: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_u64))
}

fn format_token_count(value: u64) -> String {
    let digits = value.to_string();
    let mut reversed = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            reversed.push(',');
        }
        reversed.push(ch);
    }
    reversed.chars().rev().collect()
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
        let policy = WorkspacePolicy::new(vec![root.clone()], None);

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
        let policy = WorkspacePolicy::new(vec![root], None);

        assert!(matches!(
            policy.validate(Some(&requested)),
            Err(CodexError::WorkspaceNotAllowed)
        ));
    }

    #[test]
    fn workspace_policy_uses_default_workspace_when_request_omits_one() {
        let root = unique_temp_dir("default-root");
        fs::create_dir_all(&root).unwrap();
        let policy = WorkspacePolicy::new(vec![root.clone()], Some(root.clone()));

        assert_eq!(policy.validate(None).unwrap(), root.canonicalize().unwrap());
    }

    #[test]
    fn workspace_policy_rejects_default_workspace_outside_allowed_root() {
        let root = unique_temp_dir("default-allowed");
        let outside = unique_temp_dir("default-outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let policy = WorkspacePolicy::new(vec![root], Some(outside));

        assert!(matches!(
            policy.validate(None),
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
    fn start_args_use_validated_workspace() {
        assert_eq!(
            start_args(Path::new(r"C:\repo"), "do work".to_owned()),
            vec!["exec", "--json", "--cd", r"C:\repo", "do work"]
        );
    }

    #[test]
    fn resume_args_use_validated_workspace_before_resume_subcommand() {
        assert_eq!(
            resume_args(Path::new(r"C:\repo"), "session-1", "continue".to_owned()),
            vec![
                "exec",
                "--json",
                "--cd",
                r"C:\repo",
                "resume",
                "session-1",
                "continue"
            ]
        );
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

    #[test]
    fn display_output_renders_codex_jsonl_as_slack_text() {
        let output = r#"{"type":"thread.started","thread_id":"019f2599-235d"}
{"type":"turn.started"}
{"type":"item.completed","item":{"type":"agent_message","text":"테스트 확인됐어요. 잘 연결되어 있습니다."}}
{"type":"turn.completed","usage":{"input_tokens":26265,"output_tokens":119}}"#;

        assert_eq!(
            display_output(output),
            "019f2599-235d\n\n테스트 확인됐어요. 잘 연결되어 있습니다.\n\n_tokens: in 26,265 / out 119_"
        );
    }

    #[test]
    fn display_output_renders_jsonl_without_usage() {
        let output = r#"{"type":"thread.started","thread_id":"019f2599-235d"}
{"type":"item.completed","item":{"type":"agent_message","text":"done"}}"#;

        assert_eq!(display_output(output), "019f2599-235d\n\ndone");
    }

    #[test]
    fn display_output_uses_completion_text_when_agent_message_is_missing() {
        let output = r#"{"type":"thread.started","thread_id":"019f2599-235d"}
{"type":"turn.completed","usage":{"input_tokens":10,"output_tokens":2}}"#;

        assert_eq!(
            display_output(output),
            "019f2599-235d\n\nCodex completed without assistant text.\n\n_tokens: in 10 / out 2_"
        );
    }

    #[test]
    fn display_output_uses_last_agent_message() {
        let output = r#"{"type":"thread.started","thread_id":"019f2599-235d"}
{"type":"item.completed","item":{"type":"agent_message","text":"draft"}}
{"type":"item.completed","item":{"type":"agent_message","text":"final"}}"#;

        assert_eq!(display_output(output), "019f2599-235d\n\nfinal");
    }
}
