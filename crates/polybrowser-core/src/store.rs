//! The SQLite database that holds a profile's history and bookmarks.
//!
//! It belongs to the library rather than to a web engine, so a profile's
//! history looks the same whichever engine rendered the pages.

use std::{
    path::Path,
    sync::{Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use rusqlite::Connection;

use crate::Result;

const SCHEMA_VERSION: i64 = 1;

const SCHEMA_V1: &str = "
CREATE TABLE urls (
    id              INTEGER PRIMARY KEY,
    url             TEXT NOT NULL UNIQUE,
    title           TEXT,
    visit_count     INTEGER NOT NULL DEFAULT 0,
    last_visit_time INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX urls_last_visit ON urls (last_visit_time);

-- Times are microseconds since the Unix epoch. The unique index is what makes
-- importing the same browser twice a no-op.
CREATE TABLE visits (
    id         INTEGER PRIMARY KEY,
    url_id     INTEGER NOT NULL REFERENCES urls (id) ON DELETE CASCADE,
    visit_time INTEGER NOT NULL,
    source     TEXT NOT NULL,
    UNIQUE (url_id, visit_time)
);
CREATE INDEX visits_time ON visits (visit_time);

CREATE TABLE bookmarks (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    parent_id  INTEGER REFERENCES bookmarks (id) ON DELETE CASCADE,
    position   INTEGER NOT NULL,
    title      TEXT NOT NULL,
    url        TEXT,
    date_added INTEGER NOT NULL
);
CREATE INDEX bookmarks_parent ON bookmarks (parent_id, position);
";

pub(crate) struct Store {
    connection: Mutex<Connection>,
}

impl Store {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        Self::prepare(connection)
    }

    /// A store that lives only as long as the value, for ephemeral profiles.
    pub(crate) fn in_memory() -> Result<Self> {
        Self::prepare(Connection::open_in_memory()?)
    }

    fn prepare(mut connection: Connection) -> Result<Self> {
        connection.pragma_update(None, "foreign_keys", true)?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version < SCHEMA_VERSION {
            let transaction = connection.transaction()?;
            if version < 1 {
                transaction.execute_batch(SCHEMA_V1)?;
            }
            transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            transaction.commit()?;
        }
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, Connection> {
        // A panic while holding the lock cannot leave SQLite in a state worse
        // than a rolled-back transaction, so a poisoned lock is still usable.
        self.connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}
