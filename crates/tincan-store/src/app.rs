//! App-level tables: settings, soundboard clip library, DM contacts, read markers and
//! downloads. Not part of the `Storage` trait; only `tincan-app` uses them.

use rusqlite::{OptionalExtension, Row, params};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tincan_core::{BlobHash, ChannelId, ConvId, CoreError, Result, UserId};

use crate::{DbResult, SqliteStorage, bound};

/// A clip in the local soundboard library (the blob itself lives in the blob store).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipRecord {
    /// BLAKE3 hash of the clip file.
    pub hash: BlobHash,
    /// Display name.
    pub name: String,
    /// Decoded length in milliseconds.
    pub duration_ms: u32,
    /// File size in bytes.
    pub size: u64,
    /// When it entered the library (unix ms).
    pub added_ms: u64,
}

/// A DM contact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContactRecord {
    /// The contact's user id.
    pub user: UserId,
    /// Cached display name.
    pub name: String,
    /// When the contact was first added (unix ms).
    pub added_ms: u64,
}

/// A file download the user asked for (or an auto-prefetch), kept across restarts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadRecord {
    /// BLAKE3 hash of the file.
    pub hash: BlobHash,
    /// File name to export it under.
    pub name: String,
    /// Expected size in bytes.
    pub size: u64,
    /// Free-form state chosen by the app; the UI's `TransferState` strings
    /// (`queued`, `downloading`, `complete`, `failed`, ...) are the intended values.
    pub state: String,
}

/// Any unread message in one channel: `?1` conv, `?2` channel, `?3` marker hlc, `?4` me.
pub(crate) const UNREAD_CHANNEL: &str = "
    SELECT EXISTS (SELECT 1 FROM events
    WHERE conv = ?1 AND channel = ?2 AND hlc > ?3 AND deleted = 0 AND author <> ?4)";
/// Any unread message in the conversation: `?1` conv, `?2` marker hlc, `?3` me.
pub(crate) const UNREAD_ALL: &str = "
    SELECT EXISTS (SELECT 1 FROM events
    WHERE conv = ?1 AND channel IS NOT NULL AND hlc > ?2 AND deleted = 0 AND author <> ?3)";

/// Read-marker channel key: the channel id, or the empty blob for "whole conversation".
fn marker_channel(channel: Option<&ChannelId>) -> &[u8] {
    channel.map(|c| c.0.as_slice()).unwrap_or(&[])
}

fn clip_from_row(r: &Row<'_>) -> rusqlite::Result<ClipRecord> {
    Ok(ClipRecord {
        hash: BlobHash(r.get(0)?),
        name: r.get(1)?,
        duration_ms: r.get(2)?,
        size: r.get(3)?,
        added_ms: r.get(4)?,
    })
}

fn contact_from_row(r: &Row<'_>) -> rusqlite::Result<ContactRecord> {
    Ok(ContactRecord { user: UserId(r.get(0)?), name: r.get(1)?, added_ms: r.get(2)? })
}

fn download_from_row(r: &Row<'_>) -> rusqlite::Result<DownloadRecord> {
    Ok(DownloadRecord { hash: BlobHash(r.get(0)?), name: r.get(1)?, size: r.get(2)?, state: r.get(3)? })
}

impl SqliteStorage {
    // ---- settings -------------------------------------------------------------------

    /// The stored settings document, or `None` before the first save.
    pub fn get_settings_json(&self) -> Result<Option<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached("SELECT json FROM settings WHERE id = 1").db()?;
        stmt.query_row([], |r| r.get(0)).optional().db()
    }

    /// Replace the settings document. `json` must be syntactically valid JSON (any
    /// value); anything else is refused with `CoreError::Storage`.
    pub fn set_settings_json(&self, json: &str) -> Result<()> {
        serde_json::from_str::<serde::de::IgnoredAny>(json)
            .map_err(|e| CoreError::Storage(format!("settings are not valid JSON: {e}")))?;
        let conn = self.conn();
        let mut stmt = conn
            .prepare_cached("INSERT INTO settings (id, json) VALUES (1, ?1) ON CONFLICT (id) DO UPDATE SET json = excluded.json")
            .db()?;
        stmt.execute([json]).db()?;
        Ok(())
    }

    /// Typed wrapper over [`SqliteStorage::get_settings_json`]. A stored document that no
    /// longer matches `T` is an error, so callers can fall back to defaults explicitly.
    pub fn get_settings<T: DeserializeOwned>(&self) -> Result<Option<T>> {
        match self.get_settings_json()? {
            None => Ok(None),
            Some(json) => serde_json::from_str(&json)
                .map(Some)
                .map_err(|e| CoreError::Storage(format!("stored settings do not match: {e}"))),
        }
    }

    /// Typed wrapper over [`SqliteStorage::set_settings_json`].
    pub fn set_settings<T: Serialize>(&self, settings: &T) -> Result<()> {
        let json = serde_json::to_string(settings).map_err(|e| CoreError::Storage(format!("encode settings: {e}")))?;
        self.set_settings_json(&json)
    }

    // ---- soundboard clips -----------------------------------------------------------

    /// Add a clip to the library. `Ok(false)` if that hash is already in it (the
    /// existing entry, including its name, is left unchanged).
    pub fn add_clip(&self, clip: &ClipRecord) -> Result<bool> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare_cached(
                "INSERT INTO clips (hash, name, duration_ms, size, added_ms) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (hash) DO NOTHING",
            )
            .db()?;
        let n = stmt.execute(params![clip.hash.0, clip.name, clip.duration_ms, clip.size, clip.added_ms]).db()?;
        Ok(n == 1)
    }

    /// One clip of the library.
    pub fn get_clip(&self, hash: &BlobHash) -> Result<Option<ClipRecord>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare_cached("SELECT hash, name, duration_ms, size, added_ms FROM clips WHERE hash = ?1")
            .db()?;
        stmt.query_row([hash.0], clip_from_row).optional().db()
    }

    /// The whole library, oldest addition first (ties by hash).
    pub fn list_clips(&self) -> Result<Vec<ClipRecord>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare_cached("SELECT hash, name, duration_ms, size, added_ms FROM clips ORDER BY added_ms, hash")
            .db()?;
        let rows = stmt.query_map([], clip_from_row).db()?;
        rows.collect::<rusqlite::Result<Vec<_>>>().db()
    }

    /// Rename a clip. `Ok(false)` if it is not in the library.
    pub fn rename_clip(&self, hash: &BlobHash, name: &str) -> Result<bool> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached("UPDATE clips SET name = ?2 WHERE hash = ?1").db()?;
        Ok(stmt.execute(params![hash.0, name]).db()? == 1)
    }

    /// Remove a clip from the library. `Ok(false)` if it was not in it.
    pub fn remove_clip(&self, hash: &BlobHash) -> Result<bool> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached("DELETE FROM clips WHERE hash = ?1").db()?;
        Ok(stmt.execute([hash.0]).db()? == 1)
    }

    // ---- contacts -------------------------------------------------------------------

    /// Add a contact or update its name. `added_ms` is kept from the first insert.
    pub fn upsert_contact(&self, contact: &ContactRecord) -> Result<()> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare_cached(
                "INSERT INTO contacts (user, name, added_ms) VALUES (?1, ?2, ?3)
                 ON CONFLICT (user) DO UPDATE SET name = excluded.name",
            )
            .db()?;
        stmt.execute(params![contact.user.0, contact.name, contact.added_ms]).db()?;
        Ok(())
    }

    /// One contact.
    pub fn get_contact(&self, user: &UserId) -> Result<Option<ContactRecord>> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached("SELECT user, name, added_ms FROM contacts WHERE user = ?1").db()?;
        stmt.query_row([user.0], contact_from_row).optional().db()
    }

    /// All contacts, oldest first (ties by user id).
    pub fn list_contacts(&self) -> Result<Vec<ContactRecord>> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached("SELECT user, name, added_ms FROM contacts ORDER BY added_ms, user").db()?;
        let rows = stmt.query_map([], contact_from_row).db()?;
        rows.collect::<rusqlite::Result<Vec<_>>>().db()
    }

    /// Remove a contact (its DM history is untouched). `Ok(false)` if unknown.
    pub fn remove_contact(&self, user: &UserId) -> Result<bool> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached("DELETE FROM contacts WHERE user = ?1").db()?;
        Ok(stmt.execute([user.0]).db()? == 1)
    }

    // ---- read markers ---------------------------------------------------------------

    /// Record that everything up to `hlc` in `conv`/`channel` has been read. Markers
    /// never move backwards: an older `hlc` than the stored one is ignored. `channel =
    /// None` is the conversation-wide marker (DMs).
    pub fn set_read(&self, conv: &ConvId, channel: Option<&ChannelId>, hlc: u64) -> Result<()> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare_cached(
                "INSERT INTO read_markers (conv, channel, last_read_hlc) VALUES (?1, ?2, ?3)
                 ON CONFLICT (conv, channel) DO UPDATE
                 SET last_read_hlc = max(last_read_hlc, excluded.last_read_hlc)",
            )
            .db()?;
        stmt.execute(params![conv.0, marker_channel(channel), hlc]).db()?;
        Ok(())
    }

    /// The read marker of `conv`/`channel`, `None` if never set.
    pub fn last_read(&self, conv: &ConvId, channel: Option<&ChannelId>) -> Result<Option<u64>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare_cached("SELECT last_read_hlc FROM read_markers WHERE conv = ?1 AND channel = ?2")
            .db()?;
        stmt.query_row(params![conv.0, marker_channel(channel)], |r| r.get(0)).optional().db()
    }

    /// Is there any non-deleted message by someone other than `me` newer than the read
    /// marker? With no marker every such message counts. `channel = Some` checks that
    /// channel against its own marker; `None` checks every message of the conversation
    /// against the conversation-wide marker (DMs).
    pub fn unread(&self, conv: &ConvId, channel: Option<&ChannelId>, me: &UserId) -> Result<bool> {
        let conn = self.conn();
        let after: u64 = conn
            .prepare_cached("SELECT last_read_hlc FROM read_markers WHERE conv = ?1 AND channel = ?2")
            .db()?
            .query_row(params![conv.0, marker_channel(channel)], |r| r.get(0))
            .optional()
            .db()?
            .unwrap_or(0);
        let after = bound(after);
        let found: bool = match channel {
            Some(ch) => conn
                .prepare_cached(UNREAD_CHANNEL)
                .db()?
                .query_row(params![conv.0, ch.0, after, me.0], |r| r.get(0))
                .db()?,
            None => conn
                .prepare_cached(UNREAD_ALL)
                .db()?
                .query_row(params![conv.0, after, me.0], |r| r.get(0))
                .db()?,
        };
        Ok(found)
    }

    // ---- downloads ------------------------------------------------------------------

    /// Insert or replace the download for `d.hash`.
    pub fn upsert_download(&self, d: &DownloadRecord) -> Result<()> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare_cached(
                "INSERT INTO downloads (hash, name, size, state) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (hash) DO UPDATE SET name = excluded.name, size = excluded.size, state = excluded.state",
            )
            .db()?;
        stmt.execute(params![d.hash.0, d.name, d.size, d.state]).db()?;
        Ok(())
    }

    /// One download.
    pub fn get_download(&self, hash: &BlobHash) -> Result<Option<DownloadRecord>> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached("SELECT hash, name, size, state FROM downloads WHERE hash = ?1").db()?;
        stmt.query_row([hash.0], download_from_row).optional().db()
    }

    /// All downloads in the order they were first recorded.
    pub fn list_downloads(&self) -> Result<Vec<DownloadRecord>> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached("SELECT hash, name, size, state FROM downloads ORDER BY rowid").db()?;
        let rows = stmt.query_map([], download_from_row).db()?;
        rows.collect::<rusqlite::Result<Vec<_>>>().db()
    }

    /// Forget a download. `Ok(false)` if unknown.
    pub fn remove_download(&self, hash: &BlobHash) -> Result<bool> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached("DELETE FROM downloads WHERE hash = ?1").db()?;
        Ok(stmt.execute([hash.0]).db()? == 1)
    }
}
