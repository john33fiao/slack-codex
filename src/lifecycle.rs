use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use crate::{
    codex::{display_output, CodexError, CodexExecutor, CodexRequest},
    sessions::{ProcessedEvent, SessionStatus, SessionStore, StateError},
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

    async fn publish_result(
        &self,
        channel_id: &str,
        thread_ts: &str,
        text: &str,
        max_chars: usize,
    ) -> Result<(), SlackError>;
}

#[derive(Default)]
pub struct SessionLocks {
    locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
}

impl SessionLocks {
    pub async fn lock(&self, key: &str) -> OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.locks.lock().expect("session lock map poisoned");
            locks
                .entry(key.to_owned())
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        lock.lock_owned().await
    }
}

pub struct SessionLifecycle {
    codex: Arc<dyn CodexExecutor>,
    publisher: Arc<dyn SlackPublisher>,
    sessions: Arc<dyn SessionStore>,
    locks: Arc<SessionLocks>,
    output_max_chars: usize,
}

impl SessionLifecycle {
    pub fn new(
        codex: Arc<dyn CodexExecutor>,
        publisher: Arc<dyn SlackPublisher>,
        sessions: Arc<dyn SessionStore>,
        output_max_chars: usize,
    ) -> Self {
        Self {
            codex,
            publisher,
            sessions,
            locks: Arc::new(SessionLocks::default()),
            output_max_chars,
        }
    }

    pub fn shared(
        codex: Arc<dyn CodexExecutor>,
        publisher: Arc<dyn SlackPublisher>,
        sessions: Arc<dyn SessionStore>,
        output_max_chars: usize,
    ) -> Arc<Self> {
        Arc::new(Self::new(codex, publisher, sessions, output_max_chars))
    }

    pub async fn start_from_slash(
        &self,
        command: SlashCommandPayload,
        processed_event: ProcessedEvent,
    ) -> Result<(), LifecycleError> {
        if !self.sessions.try_record_event(&processed_event)? {
            return Ok(());
        }

        let request = CodexRequest::parse(command.text.trim())?;
        if request.prompt.is_empty() {
            return Ok(());
        }

        let thread = self
            .publisher
            .start_session_thread(&command.channel_id, &command.user_id)
            .await?;

        match self.codex.start_session(request).await {
            Ok(output) => {
                self.sessions
                    .save_session(&thread.thread_ts, &output.session_id)?;
                self.publisher
                    .publish_result(
                        &thread.channel_id,
                        &thread.thread_ts,
                        &display_output(&output.stdout),
                        self.output_max_chars,
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
        processed_event: ProcessedEvent,
    ) -> Result<(), LifecycleError> {
        if !self.sessions.try_record_event(&processed_event)? {
            return Ok(());
        }

        let thread_ts = event.thread_ts();
        let Some(record) = self.sessions.get_session(thread_ts)? else {
            self.publisher
                .post_thread_message(
                    &event.channel,
                    thread_ts,
                    "This thread is not registered with a Codex session. Start with `/codex <prompt>`.",
                )
                .await?;
            return Ok(());
        };

        let _guard = self.locks.lock(&record.thread_ts).await;
        self.sessions
            .set_session_status(&record.thread_ts, SessionStatus::Running)?;
        let resume_result = self
            .codex
            .resume_session(&record.session_id, CodexRequest::parse(event.text.trim())?)
            .await;
        self.sessions
            .set_session_status(&record.thread_ts, SessionStatus::Idle)?;

        match resume_result {
            Ok(output) => {
                self.publisher
                    .publish_result(
                        &event.channel,
                        thread_ts,
                        &display_output(&output.stdout),
                        self.output_max_chars,
                    )
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
    #[error(transparent)]
    State(#[from] StateError),
}

#[cfg(test)]
mod tests {
    use std::{sync::Mutex, time::Duration};

    use crate::{
        codex::{CodexResumeOutput, CodexSessionOutput},
        sessions::MemorySessionStore,
    };

    use super::*;

    #[derive(Default)]
    struct FakeCodex {
        starts: Mutex<Vec<String>>,
        resumes: Mutex<Vec<(String, String)>>,
        active_resumes: Mutex<usize>,
        max_active_resumes: Mutex<usize>,
    }

    #[async_trait]
    impl CodexExecutor for FakeCodex {
        async fn start_session(
            &self,
            request: CodexRequest,
        ) -> Result<CodexSessionOutput, CodexError> {
            self.starts.lock().unwrap().push(request.prompt);
            Ok(CodexSessionOutput {
                session_id: "session-1".to_owned(),
                stdout: "started output".to_owned(),
                stderr: String::new(),
            })
        }

        async fn resume_session(
            &self,
            session_id: &str,
            request: CodexRequest,
        ) -> Result<CodexResumeOutput, CodexError> {
            {
                let mut active = self.active_resumes.lock().unwrap();
                *active += 1;
                let mut max_active = self.max_active_resumes.lock().unwrap();
                *max_active = (*max_active).max(*active);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            self.resumes
                .lock()
                .unwrap()
                .push((session_id.to_owned(), request.prompt));
            *self.active_resumes.lock().unwrap() -= 1;
            Ok(CodexResumeOutput {
                stdout: "resume output".to_owned(),
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

        async fn publish_result(
            &self,
            channel_id: &str,
            thread_ts: &str,
            text: &str,
            _max_chars: usize,
        ) -> Result<(), SlackError> {
            self.post_thread_message(channel_id, thread_ts, text).await
        }
    }

    fn processed_event(key: &str) -> ProcessedEvent {
        ProcessedEvent::new(key, Some("171.0001".to_owned()), "test")
    }

    #[tokio::test]
    async fn slash_command_starts_session_and_saves_mapping() {
        let codex = Arc::new(FakeCodex::default());
        let publisher = Arc::new(FakePublisher::default());
        let sessions = MemorySessionStore::shared();
        let lifecycle =
            SessionLifecycle::new(codex.clone(), publisher.clone(), sessions.clone(), 1000);

        lifecycle
            .start_from_slash(
                SlashCommandPayload {
                    team_id: "T1".to_owned(),
                    channel_id: "D1".to_owned(),
                    user_id: "U1".to_owned(),
                    command: "/codex".to_owned(),
                    text: "say hi".to_owned(),
                },
                processed_event("E1"),
            )
            .await
            .unwrap();

        assert_eq!(codex.starts.lock().unwrap().as_slice(), ["say hi"]);
        assert_eq!(
            sessions
                .get_session("171.0001")
                .unwrap()
                .unwrap()
                .session_id,
            "session-1".to_owned()
        );
        assert!(publisher
            .messages
            .lock()
            .unwrap()
            .iter()
            .any(|(_, _, text)| text == "started output"));
    }

    #[tokio::test]
    async fn duplicate_start_event_does_not_run_twice() {
        let codex = Arc::new(FakeCodex::default());
        let publisher = Arc::new(FakePublisher::default());
        let sessions = MemorySessionStore::shared();
        let lifecycle = SessionLifecycle::new(codex.clone(), publisher, sessions, 1000);
        let command = SlashCommandPayload {
            team_id: "T1".to_owned(),
            channel_id: "D1".to_owned(),
            user_id: "U1".to_owned(),
            command: "/codex".to_owned(),
            text: "say hi".to_owned(),
        };

        lifecycle
            .start_from_slash(command.clone(), processed_event("E1"))
            .await
            .unwrap();
        lifecycle
            .start_from_slash(command, processed_event("E1"))
            .await
            .unwrap();

        assert_eq!(codex.starts.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn registered_thread_resumes_same_session() {
        let codex = Arc::new(FakeCodex::default());
        let publisher = Arc::new(FakePublisher::default());
        let sessions = MemorySessionStore::shared();
        sessions.save_session("171.0001", "session-1").unwrap();
        let lifecycle = SessionLifecycle::new(codex.clone(), publisher.clone(), sessions, 1000);

        lifecycle
            .resume_from_message(
                SlackMessageEvent {
                    channel: "D1".to_owned(),
                    user: Some("U1".to_owned()),
                    text: "continue".to_owned(),
                    ts: "171.0002".to_owned(),
                    thread_ts: Some("171.0001".to_owned()),
                },
                processed_event("E2"),
            )
            .await
            .unwrap();

        assert_eq!(
            codex.resumes.lock().unwrap().as_slice(),
            [("session-1".to_owned(), "continue".to_owned())]
        );
        assert!(publisher
            .messages
            .lock()
            .unwrap()
            .iter()
            .any(|(_, _, text)| text == "resume output"));
    }

    #[tokio::test]
    async fn simultaneous_replies_in_one_thread_are_serialized() {
        let codex = Arc::new(FakeCodex::default());
        let publisher = Arc::new(FakePublisher::default());
        let sessions = MemorySessionStore::shared();
        sessions.save_session("171.0001", "session-1").unwrap();
        let lifecycle = Arc::new(SessionLifecycle::new(
            codex.clone(),
            publisher,
            sessions,
            1000,
        ));
        let event = SlackMessageEvent {
            channel: "D1".to_owned(),
            user: Some("U1".to_owned()),
            text: "continue".to_owned(),
            ts: "171.0002".to_owned(),
            thread_ts: Some("171.0001".to_owned()),
        };

        let first = lifecycle.resume_from_message(event.clone(), processed_event("E2"));
        let second = lifecycle.resume_from_message(event, processed_event("E3"));
        let (first, second) = tokio::join!(first, second);

        first.unwrap();
        second.unwrap();
        assert_eq!(*codex.max_active_resumes.lock().unwrap(), 1);
        assert_eq!(codex.resumes.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn unregistered_thread_gets_guide_without_resume() {
        let codex = Arc::new(FakeCodex::default());
        let publisher = Arc::new(FakePublisher::default());
        let sessions = MemorySessionStore::shared();
        let lifecycle = SessionLifecycle::new(codex.clone(), publisher.clone(), sessions, 1000);

        lifecycle
            .resume_from_message(
                SlackMessageEvent {
                    channel: "D1".to_owned(),
                    user: Some("U1".to_owned()),
                    text: "hello".to_owned(),
                    ts: "171.0002".to_owned(),
                    thread_ts: None,
                },
                ProcessedEvent::new("E4", None, "test"),
            )
            .await
            .unwrap();

        assert!(codex.resumes.lock().unwrap().is_empty());
        let messages = publisher.messages.lock().unwrap();
        assert!(messages[0].2.contains("not registered"));
    }
}
