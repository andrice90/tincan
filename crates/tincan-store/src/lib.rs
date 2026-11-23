//! SQLite persistence for Tincan.
//!
//! [`SqliteStorage`] is the production implementation of
//! [`tincan_core::storage::Storage`] (conversations, the replicated event log, cached
//! peer addresses, a small key/value store) plus the app-level tables the app layer
//! needs: settings, the soundboard clip library, DM contacts, read markers and
//! downloads. There is one database file per profile.
//!
//! Connection setup: WAL journal, `synchronous = NORMAL`, 5 s busy timeout, foreign keys
//! on, and schema migrations tracked with `PRAGMA user_version` (see [`SCHEMA_VERSION`]).
//! A database written by a newer Tincan (higher `user_version`) is refused rather than
//! guessed at.
//!
//! Threading: a single connection behind a mutex. Every method takes `&self`, holds the
//! lock for one statement or one transaction and uses cached prepared statements, so a
//! `SqliteStorage` can be shared as `Arc<SqliteStorage>` (and coerced to
//! `Arc<dyn Storage>`) across threads and tasks. Calls are blocking but short; async
//! callers on a multi-threaded runtime may call them directly.
//!
//! Events are stored as their exact signed bytes. Reads decode those bytes but do NOT
//! re-verify signatures: every event was verified before it was inserted, and an ed25519
//! check per event on every page load would cost tens of microseconds each for nothing.
//! Deleted events keep their signed bytes (peers still need them to sync), so hiding the
//! text of a deleted message is the caller's job.

#![warn(missing_docs)]

mod app;
mod schema;
mod storage_impl;
#[cfg(test)]
mod tests;

use std::path::Path;
use std::time::Duration;

use parking_lot::{Mutex, MutexGuard};
use rusqlite::Connection;
use tincan_core::{CoreError, Result};

pub use app::{ClipRecord, ContactRecord, DownloadRecord};
pub use schema::SCHEMA_VERSION;

/// How long a statement waits for a lock held by another connection before failing.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// Cached prepared statements per connection (rusqlite's default of 16 is fewer than
/// the distinct statements this crate uses).
const STATEMENT_CACHE_CAPACITY: usize = 64;

/// SQLite-backed storage for one profile. See the crate docs for the guarantees.
pub struct SqliteStorage {
    conn: Mutex<Connection>,
}

impl std::fmt::Debug for SqliteStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteStorage").finish_non_exhaustive()
    }
}

impl SqliteStorage {
    /// Open (or create) the database file at `path`, creating missing parent
    /// directories, apply the connection settings and run pending migrations.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .map_err(|e| CoreError::Storage(format!("create {}: {e}", parent.display())))?;
        }
        let conn = Connection::open(path)
            .map_err(|e| CoreError::Storage(format!("open {}: {e}", path.display())))?;
        let store = Self::init(conn, true)?;
        tracing::debug!(path = %path.display(), "opened sqlite storage");
        Ok(store)
    }

    /// Open a private in-memory database (tests, ephemeral profiles). Same schema and
    /// settings as [`SqliteStorage::open`], except the journal mode is `memory`.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().db()?;
        Self::init(conn, false)
    }

    /// Schema version of the open database (`PRAGMA user_version`); equals
    /// [`SCHEMA_VERSION`] once opened.
    pub fn schema_version(&self) -> Result<u32> {
        schema::user_version(&self.conn())
    }

    fn init(mut conn: Connection, on_disk: bool) -> Result<Self> {
        conn.busy_timeout(BUSY_TIMEOUT).db()?;
        let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0)).db()?;
        if on_disk && !mode.eq_ignore_ascii_case("wal") {
            tracing::warn!(%mode, "sqlite refused WAL journal mode; continuing with it");
        }
        conn.execute_batch(
            "PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA journal_size_limit = 4194304;",
        )
        .db()?;
        conn.set_prepared_statement_cache_capacity(STATEMENT_CACHE_CAPACITY);
        schema::migrate(&mut conn)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// Lock the connection for one statement or transaction.
    pub(crate) fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock()
    }
}

/// Map a rusqlite error into the shared error type (the orphan rule forbids `From`).
pub(crate) fn db_err(e: rusqlite::Error) -> CoreError {
    CoreError::Storage(e.to_string())
}

/// `.db()?` on rusqlite results.
pub(crate) trait DbResult<T> {
    /// Convert the error into [`CoreError::Storage`].
    fn db(self) -> Result<T>;
}

impl<T> DbResult<T> for rusqlite::Result<T> {
    fn db(self) -> Result<T> {
        self.map_err(db_err)
    }
}

/// A `u64` query bound as an SQLite integer, saturating at `i64::MAX` (no stored value
/// exceeds that, so the saturated bound selects the same rows).
pub(crate) fn bound(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// A `usize` LIMIT as an SQLite integer, saturating at `i64::MAX`.
pub(crate) fn limit(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}
