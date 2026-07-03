use std::{
    collections::{HashMap, HashSet},
    fs,
    path::Path,
    sync::{Arc, Mutex},
};

use rusqlite::{params, Connection, OptionalExtension};

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SessionRecord {
    pub thread_ts: String,
    pub session_id: String,
    pub status: SessionStatus,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum SessionStatus {
    Idle,
    Running,
}

impl SessionStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
        }
    }

    fn from_str(value: &str) -> Self {
        match value {
            "running" => Self::Running,
            _ => Self::Idle,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ProcessedEvent {
    pub event_key: String,
    pub thread_ts: Option<String>,
    pub source: String,
}

impl ProcessedEvent {
    pub fn new(
        event_key: impl Into<String>,
        thread_ts: Option<String>,
        source: impl Into<String>,
    ) -> Self {
        Self {
            event_key: event_key.into(),
            thread_ts,
            source: source.into(),
        }
    }
}

pub trait SessionStore: Send + Sync {
    fn save_session(&self, thread_ts: &str, session_id: &str) -> Result<(), StateError>;
    fn get_session(&self, thread_ts: &str) -> Result<Option<SessionRecord>, StateError>;
    fn set_session_status(&self, thread_ts: &str, status: SessionStatus) -> Result<(), StateError>;
    fn try_record_event(&self, event: &ProcessedEvent) -> Result<bool, StateError>;
    fn recover_running_sessions(&self) -> Result<usize, StateError>;
}

pub struct SqliteStateStore {
    connection: Mutex<Connection>,
}

impl SqliteStateStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StateError> {
        let path = path.as_ref();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)?;
        let store = Self {
            connection: Mutex::new(connection),
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn open_in_memory() -> Result<Self, StateError> {
        let store = Self {
            connection: Mutex::new(Connection::open_in_memory()?),
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn shared(path: impl AsRef<Path>) -> Result<Arc<Self>, StateError> {
        Ok(Arc::new(Self::open(path)?))
    }

    fn migrate(&self) -> Result<(), StateError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS sessions (
              thread_ts   TEXT PRIMARY KEY,
              session_id  TEXT NOT NULL,
              status      TEXT NOT NULL DEFAULT 'idle',
              created_at  TEXT NOT NULL,
              updated_at  TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS processed_events (
              event_key   TEXT PRIMARY KEY,
              thread_ts   TEXT,
              source      TEXT NOT NULL,
              created_at  TEXT NOT NULL
            );
            "#,
        )?;
        Ok(())
    }
}

impl SessionStore for SqliteStateStore {
    fn save_session(&self, thread_ts: &str, session_id: &str) -> Result<(), StateError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection.execute(
            r#"
            INSERT INTO sessions (thread_ts, session_id, status, created_at, updated_at)
            VALUES (?1, ?2, 'idle', datetime('now'), datetime('now'))
            ON CONFLICT(thread_ts) DO UPDATE SET
              session_id = excluded.session_id,
              status = 'idle',
              updated_at = datetime('now')
            "#,
            params![thread_ts, session_id],
        )?;
        Ok(())
    }

    fn get_session(&self, thread_ts: &str) -> Result<Option<SessionRecord>, StateError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection
            .query_row(
                "SELECT thread_ts, session_id, status FROM sessions WHERE thread_ts = ?1",
                params![thread_ts],
                |row| {
                    let status: String = row.get(2)?;
                    Ok(SessionRecord {
                        thread_ts: row.get(0)?,
                        session_id: row.get(1)?,
                        status: SessionStatus::from_str(&status),
                    })
                },
            )
            .optional()
            .map_err(StateError::from)
    }

    fn set_session_status(&self, thread_ts: &str, status: SessionStatus) -> Result<(), StateError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection.execute(
            "UPDATE sessions SET status = ?2, updated_at = datetime('now') WHERE thread_ts = ?1",
            params![thread_ts, status.as_str()],
        )?;
        Ok(())
    }

    fn try_record_event(&self, event: &ProcessedEvent) -> Result<bool, StateError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let changed = connection.execute(
            r#"
            INSERT OR IGNORE INTO processed_events (event_key, thread_ts, source, created_at)
            VALUES (?1, ?2, ?3, datetime('now'))
            "#,
            params![event.event_key, event.thread_ts, event.source],
        )?;
        Ok(changed == 1)
    }

    fn recover_running_sessions(&self) -> Result<usize, StateError> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let changed = connection.execute(
            "UPDATE sessions SET status = 'idle', updated_at = datetime('now') WHERE status = 'running'",
            [],
        )?;
        Ok(changed)
    }
}

#[derive(Default)]
pub struct MemorySessionStore {
    sessions: Mutex<HashMap<String, SessionRecord>>,
    processed_events: Mutex<HashSet<String>>,
}

impl MemorySessionStore {
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

impl SessionStore for MemorySessionStore {
    fn save_session(&self, thread_ts: &str, session_id: &str) -> Result<(), StateError> {
        let record = SessionRecord {
            thread_ts: thread_ts.to_owned(),
            session_id: session_id.to_owned(),
            status: SessionStatus::Idle,
        };
        self.sessions
            .lock()
            .expect("session mutex poisoned")
            .insert(thread_ts.to_owned(), record);
        Ok(())
    }

    fn get_session(&self, thread_ts: &str) -> Result<Option<SessionRecord>, StateError> {
        Ok(self
            .sessions
            .lock()
            .expect("session mutex poisoned")
            .get(thread_ts)
            .cloned())
    }

    fn set_session_status(&self, thread_ts: &str, status: SessionStatus) -> Result<(), StateError> {
        if let Some(record) = self
            .sessions
            .lock()
            .expect("session mutex poisoned")
            .get_mut(thread_ts)
        {
            record.status = status;
        }
        Ok(())
    }

    fn try_record_event(&self, event: &ProcessedEvent) -> Result<bool, StateError> {
        Ok(self
            .processed_events
            .lock()
            .expect("processed event mutex poisoned")
            .insert(event.event_key.clone()))
    }

    fn recover_running_sessions(&self) -> Result<usize, StateError> {
        let mut changed = 0;
        for record in self
            .sessions
            .lock()
            .expect("session mutex poisoned")
            .values_mut()
        {
            if record.status == SessionStatus::Running {
                record.status = SessionStatus::Idle;
                changed += 1;
            }
        }
        Ok(changed)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_saves_and_reads_session_mapping() {
        let store = SqliteStateStore::open_in_memory().unwrap();
        store.save_session("171.1", "session-1").unwrap();

        assert_eq!(
            store.get_session("171.1").unwrap(),
            Some(SessionRecord {
                thread_ts: "171.1".to_owned(),
                session_id: "session-1".to_owned(),
                status: SessionStatus::Idle
            })
        );
    }

    #[test]
    fn duplicate_event_key_is_ignored() {
        let store = SqliteStateStore::open_in_memory().unwrap();
        let event = ProcessedEvent::new("E1", Some("171.1".to_owned()), "events_api");

        assert!(store.try_record_event(&event).unwrap());
        assert!(!store.try_record_event(&event).unwrap());
    }

    #[test]
    fn startup_recovery_moves_running_sessions_to_idle() {
        let store = SqliteStateStore::open_in_memory().unwrap();
        store.save_session("171.1", "session-1").unwrap();
        store
            .set_session_status("171.1", SessionStatus::Running)
            .unwrap();

        assert_eq!(store.recover_running_sessions().unwrap(), 1);
        assert_eq!(
            store.get_session("171.1").unwrap().unwrap().status,
            SessionStatus::Idle
        );
    }
}
