use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SessionRecord {
    pub thread_ts: String,
    pub session_id: String,
}

#[derive(Default)]
pub struct MemorySessionStore {
    sessions: Mutex<HashMap<String, SessionRecord>>,
}

impl MemorySessionStore {
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn save(&self, thread_ts: impl Into<String>, session_id: impl Into<String>) {
        let thread_ts = thread_ts.into();
        let record = SessionRecord {
            thread_ts: thread_ts.clone(),
            session_id: session_id.into(),
        };
        self.sessions
            .lock()
            .expect("session mutex poisoned")
            .insert(thread_ts, record);
    }

    pub fn get(&self, thread_ts: &str) -> Option<SessionRecord> {
        self.sessions
            .lock()
            .expect("session mutex poisoned")
            .get(thread_ts)
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_and_reads_session_mapping() {
        let store = MemorySessionStore::default();
        store.save("171.1", "session-1");

        assert_eq!(
            store.get("171.1"),
            Some(SessionRecord {
                thread_ts: "171.1".to_owned(),
                session_id: "session-1".to_owned()
            })
        );
    }
}
