//! Schema and migrations, tracked with `PRAGMA user_version`.
//!
//! `MIGRATIONS[i]` upgrades a database from version `i` to `i + 1`. Each step runs in
//! its own transaction together with the `user_version` bump, so a crash mid-migration
//! leaves the previous version intact. Never edit a released step; append a new one.

use rusqlite::{Connection, TransactionBehavior};
use tincan_core::{CoreError, Result};

use crate::DbResult;

/// Schema version this build writes and understands.
pub const SCHEMA_VERSION: u32 = 1;

/// Version 1.
///
/// Notes on the layout:
/// - `events.channel` is set for `Message` events only (`Body::channel`), so
///   `channel IS NULL` selects exactly the non-message events. The two partial indexes
///   rely on that.
/// - `events` has no foreign key to `convs`: the `Storage` contract lets events (and
///   peers) arrive before their conversation record. `delete_conv` cleans up explicitly.
/// - `read_markers.channel` is never NULL (NULLs are distinct in a primary key); the empty
///   blob means "the whole conversation" (DMs).
const V1: &str = "
CREATE TABLE convs (
    id            BLOB PRIMARY KEY NOT NULL,
    is_dm         INTEGER NOT NULL,
    peer          BLOB,
    name          TEXT NOT NULL,
    server_secret BLOB,
    last_epoch    INTEGER NOT NULL,
    last_host     BLOB,
    joined_ms     INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TABLE events (
    id      BLOB PRIMARY KEY NOT NULL,
    conv    BLOB NOT NULL,
    author  BLOB NOT NULL,
    seq     INTEGER NOT NULL,
    hlc     INTEGER NOT NULL,
    kind    TEXT NOT NULL,
    channel BLOB,
    bytes   BLOB NOT NULL,
    sig     BLOB NOT NULL,
    deleted INTEGER NOT NULL DEFAULT 0,
    UNIQUE (conv, author, seq)
);
-- Message paging and unread checks within one channel.
CREATE INDEX events_channel_order ON events (conv, channel, hlc, id);
-- Message paging and unread checks across all channels of a conversation (DMs).
CREATE INDEX events_message_order ON events (conv, hlc, id) WHERE channel IS NOT NULL;
-- state_events without touching message rows.
CREATE INDEX events_state ON events (conv, author, seq) WHERE channel IS NULL;

CREATE TABLE peers (
    conv         BLOB NOT NULL,
    user         BLOB NOT NULL,
    addr         BLOB NOT NULL,
    last_seen_ms INTEGER NOT NULL,
    PRIMARY KEY (conv, user)
) WITHOUT ROWID;
CREATE INDEX peers_recent ON peers (conv, last_seen_ms DESC, user);

CREATE TABLE kv (
    key   TEXT PRIMARY KEY NOT NULL,
    value BLOB NOT NULL
) WITHOUT ROWID;

CREATE TABLE settings (
    id   INTEGER PRIMARY KEY CHECK (id = 1),
    json TEXT NOT NULL
);

CREATE TABLE clips (
    hash        BLOB PRIMARY KEY NOT NULL,
    name        TEXT NOT NULL,
    duration_ms INTEGER NOT NULL,
    size        INTEGER NOT NULL,
    added_ms    INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TABLE contacts (
    user     BLOB PRIMARY KEY NOT NULL,
    name     TEXT NOT NULL,
    added_ms INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TABLE read_markers (
    conv          BLOB NOT NULL,
    channel       BLOB NOT NULL,
    last_read_hlc INTEGER NOT NULL,
    PRIMARY KEY (conv, channel)
) WITHOUT ROWID;

CREATE TABLE downloads (
    hash  BLOB PRIMARY KEY NOT NULL,
    name  TEXT NOT NULL,
    size  INTEGER NOT NULL,
    state TEXT NOT NULL
);
";

/// Upgrade steps, index = version upgraded from.
const MIGRATIONS: &[&str] = &[V1];

// Keep the constant and the step list in lockstep.
const _: () = assert!(MIGRATIONS.len() == SCHEMA_VERSION as usize);

/// Current `PRAGMA user_version`.
pub(crate) fn user_version(conn: &Connection) -> Result<u32> {
    let v: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).db()?;
    u32::try_from(v).map_err(|_| CoreError::Storage(format!("invalid schema version {v}")))
}

/// Bring the database up to [`SCHEMA_VERSION`].
///
/// Each step takes the write lock first (`BEGIN IMMEDIATE`) and re-reads the version
/// inside the transaction, so two connections opening a fresh file at the same time
/// cannot both run the same step.
pub(crate) fn migrate(conn: &mut Connection) -> Result<()> {
    loop {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).db()?;
        let current = user_version(&tx)?;
        if current > SCHEMA_VERSION {
            return Err(CoreError::Storage(format!(
                "database schema version {current} is newer than this Tincan supports \
                 ({SCHEMA_VERSION}); update Tincan"
            )));
        }
        let Some(sql) = MIGRATIONS.get(current as usize) else {
            // Up to date; the empty transaction rolls back on drop.
            return Ok(());
        };
        let to = current + 1;
        tx.execute_batch(sql).db()?;
        tx.pragma_update(None, "user_version", i64::from(to)).db()?;
        tx.commit().db()?;
        tracing::info!(from = current, to, "migrated sqlite schema");
    }
}
