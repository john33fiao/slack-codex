use std::sync::Arc;

use async_trait::async_trait;

use crate::{
    codex::{CodexError, CodexExecutor},
    sessions::MemorySessionStore,
    slack::{SlackError, SlackMessageEvent, SlashCommandPayload},
};

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SlackThread {
    pub channel_id: String,
    pub thread_ts: String,
}

#[async_trait]
pub trait SlackPublisher: Send + Sync {
    async fn start_session_thread(
        &self,
        channel_id: &str,
        user_id: &str,
    ) -> Result<SlackThread, SlackError>;

    async fn post_thread_message(
        &self,
        channel_id: &str,
        thread_ts: &str,
        text: &str,
    ) -> Result<(), SlackError>;
}

pub struct SessionLifecycle {
    codex: Arc<dyn CodexExecutor>,
    publisher: Arc<dyn SlackPublisher>,
    sessions: Arc<MemorySessionStore>,
}

impl SessionLifecycle {
    pub fn new(
        codex: Arc<dyn CodexExecutor>,
        publisher: Arc<dyn SlackPublisher>,
        sessions: Arc<MemorySessionStore>,
    ) -> Self {
        Self {
            codex,
            publisher,
            sessions,
        }
    }

    pub fn shared(
        codex: Arc<dyn CodexExecutor>,
        publisher: Arc<dyn SlackPublisher>,
        sessions: Arc<MemorySessionStore>,
    ) -> Arc<Self> {
        Arc::new(Self::new(codex, publisher, sessions))
    }

    pub async fn start_from_slash(
        &self,
        command: SlashCommandPayload,
    ) -> Result<(), LifecycleError> {
        let prompt = command.text.trim();
        if prompt.is_empty() {
            return Ok(());
        }

        let thread = self
            .publisher
            .start_session_thread(&command.channel_id, &command.user_id)
            .await?;

        match self.codex.start_session(prompt).await {
            Ok(output) => {
                self.sessions
                    .save(thread.thread_ts.clone(), output.session_id);
                self.publisher
                    .post_thread_message(
                        &thread.channel_id,
                        &thread.thread_ts,
                        "Codex session started. Reply in this thread to continue.",
                    )
                    .await?;
            }
            Err(error) => {
                self.publisher
                    .post_thread_message(
                        &thread.channel_id,
                        &thread.thread_ts,
                        &format!("Codex start failed: {}", error.user_message()),
                    )
                    .await?;
                return Err(error.into());
            }
        }

        Ok(())
    }

    pub async fn resume_from_message(
        &self,
        event: SlackMessageEvent,
    ) -> Result<(), LifecycleError> {
        let thread_ts = event.thread_ts();
        let Some(record) = self.sessions.get(thread_ts) else {
            self.publisher
                .post_thread_message(
                    &event.channel,
                    thread_ts,
                    "This thread is not registered with a Codex session. Start with `/codex <prompt>`.",
                )
                .await?;
            return Ok(());
        };

        match self
            .codex
            .resume_session(&record.session_id, event.text.trim())
            .await
        {
            Ok(_) => {
                self.publisher
                    .post_thread_message(&event.channel, thread_ts, "Codex resume completed.")
                    .await?;
            }
            Err(error) => {
                self.publisher
                    .post_thread_message(
                        &event.channel,
                        thread_ts,
                        &format!("Codex resume failed: {}", error.user_message()),
                    )
                    .await?;
                return Err(error.into());
            }
        }

        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LifecycleError {
    #[error(transparent)]
    Codex(#[from] CodexError),
    #[error(transparent)]
    Slack(#[from] SlackError),
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use crate::codex::{CodexResumeOutput, CodexSessionOutput};

    use super::*;

    #[derive(Default)]
    struct FakeCodex {
        starts: Mutex<Vec<String>>,
        resumes: Mutex<Vec<(String, String)>>,
    }

    #[async_trait]
    impl CodexExecutor for FakeCodex {
        async fn start_session(&self, prompt: &str) -> Result<CodexSessionOutput, CodexError> {
            self.starts.lock().unwrap().push(prompt.to_owned());
            Ok(CodexSessionOutput {
                session_id: "session-1".to_owned(),
                stdout: String::new(),
                stderr: String::new(),
            })
        }

        async fn resume_session(
            &self,
            session_id: &str,
            prompt: &str,
        ) -> Result<CodexResumeOutput, CodexError> {
            self.resumes
                .lock()
                .unwrap()
                .push((session_id.to_owned(), prompt.to_owned()));
            Ok(CodexResumeOutput {
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    #[derive(Default)]
    struct FakePublisher {
        messages: Mutex<Vec<(String, String, String)>>,
    }

    #[async_trait]
    impl SlackPublisher for FakePublisher {
        async fn start_session_thread(
            &self,
            channel_id: &str,
            _user_id: &str,
        ) -> Result<SlackThread, SlackError> {
            Ok(SlackThread {
                channel_id: channel_id.to_owned(),
                thread_ts: "171.0001".to_owned(),
            })
        }

        async fn post_thread_message(
            &self,
            channel_id: &str,
            thread_ts: &str,
            text: &str,
        ) -> Result<(), SlackError> {
            self.messages.lock().unwrap().push((
                channel_id.to_owned(),
                thread_ts.to_owned(),
                text.to_owned(),
            ));
            Ok(())
        }
    }

    #[tokio::test]
    async fn slash_command_starts_session_and_saves_mapping() {
        let codex = Arc::new(FakeCodex::default());
        let publisher = Arc::new(FakePublisher::default());
        let sessions = MemorySessionStore::shared();
        let lifecycle = SessionLifecycle::new(codex.clone(), publisher, sessions.clone());

        lifecycle
            .start_from_slash(SlashCommandPayload {
                team_id: "T1".to_owned(),
                channel_id: "D1".to_owned(),
                user_id: "U1".to_owned(),
                command: "/codex".to_owned(),
                text: "say hi".to_owned(),
            })
            .await
            .unwrap();

        assert_eq!(codex.starts.lock().unwrap().as_slice(), ["say hi"]);
        assert_eq!(
            sessions.get("171.0001").unwrap().session_id,
            "session-1".to_owned()
        );
    }

    #[tokio::test]
    async fn registered_thread_resumes_same_session() {
        let codex = Arc::new(FakeCodex::default());
        let publisher = Arc::new(FakePublisher::default());
        let sessions = MemorySessionStore::shared();
        sessions.save("171.0001", "session-1");
        let lifecycle = SessionLifecycle::new(codex.clone(), publisher, sessions);

        lifecycle
            .resume_from_message(SlackMessageEvent {
                channel: "D1".to_owned(),
                user: Some("U1".to_owned()),
                text: "continue".to_owned(),
                ts: "171.0002".to_owned(),
                thread_ts: Some("171.0001".to_owned()),
            })
            .await
            .unwrap();

        assert_eq!(
            codex.resumes.lock().unwrap().as_slice(),
            [("session-1".to_owned(), "continue".to_owned())]
        );
    }

    #[tokio::test]
    async fn unregistered_thread_gets_guide_without_resume() {
        let codex = Arc::new(FakeCodex::default());
        let publisher = Arc::new(FakePublisher::default());
        let sessions = MemorySessionStore::shared();
        let lifecycle = SessionLifecycle::new(codex.clone(), publisher.clone(), sessions);

        lifecycle
            .resume_from_message(SlackMessageEvent {
                channel: "D1".to_owned(),
                user: Some("U1".to_owned()),
                text: "hello".to_owned(),
                ts: "171.0002".to_owned(),
                thread_ts: None,
            })
            .await
            .unwrap();

        assert!(codex.resumes.lock().unwrap().is_empty());
        let messages = publisher.messages.lock().unwrap();
        assert!(messages[0].2.contains("not registered"));
    }
}
