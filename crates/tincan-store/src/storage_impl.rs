//! `impl Storage for SqliteStorage` plus the event helpers it shares with tests and the
//! app layer (`insert_events_batch`).

use rusqlite::{Connection, ErrorCode, OptionalExtension, Params, Row, params};
use tincan_core::event::{Body, SignedEvent, StoredEvent};
use tincan_core::keys::Sig64;
use tincan_core::storage::{ConvRecord, PeerRecord, Storage};
use tincan_core::wire::AddrHint;
use tincan_core::{ChannelId, ConvId, CoreError, EventId, Result, UserId};

use crate::{DbResult, SqliteStorage, bound, db_err, limit};

const INSERT_EVENT: &str = "
    INSERT INTO events (id, conv, author, seq, hlc, kind, channel, bytes, sig, deleted)
    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
    ON CONFLICT (id) DO NOTHING";

pub(crate) const MSG_CHANNEL: &str = "
    SELECT id, bytes, sig, deleted FROM events
    WHERE conv = ?1 AND channel = ?2
    ORDER BY hlc DESC, id DESC LIMIT ?3";
pub(crate) const MSG_CHANNEL_BEFORE: &str = "
    SELECT id, bytes, sig, deleted FROM events
    WHERE conv = ?1 AND channel = ?2 AND (hlc, id) < (?4, ?5)
    ORDER BY hlc DESC, id DESC LIMIT ?3";
pub(crate) const MSG_ALL: &str = "
    SELECT id, bytes, sig, deleted FROM events
    WHERE conv = ?1 AND channel IS NOT NULL
    ORDER BY hlc DESC, id DESC LIMIT ?2";
pub(crate) const MSG_ALL_BEFORE: &str = "
    SELECT id, bytes, sig, deleted FROM events
    WHERE conv = ?1 AND channel IS NOT NULL AND (hlc, id) < (?3, ?4)
    ORDER BY hlc DESC, id DESC LIMIT ?2";

pub(crate) const STATE_EVENTS: &str = "
    SELECT id, bytes, sig, deleted FROM events
    WHERE conv = ?1 AND channel IS NULL
    ORDER BY author, seq";

/// The `channel` column: set for `Message` events only. Matched here rather than via
/// `Body::channel` because the partial indexes and `state_events` depend on exactly this
/// rule ("channel IS NULL" = every non-message event).
fn channel_col(body: &Body) -> Option<[u8; 32]> {
    match body {
        Body::Message { channel, .. } => Some(channel.0),
        _ => None,
    }
}

/// Event columns as read from a row, decoded after the lock is released.
struct RawEvent {
    id: [u8; 32],
    bytes: Vec<u8>,
    sig: [u8; 64],
    deleted: bool,
}

impl RawEvent {
    /// Map a row selected as `id, bytes, sig, deleted`.
    fn from_row(r: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self { id: r.get(0)?, bytes: r.get(1)?, sig: r.get(2)?, deleted: r.get(3)? })
    }

    /// Decode the stored bytes (no signature check: verified before insert).
    fn into_stored(self) -> Result<StoredEvent> {
        let signed = SignedEvent { bytes: self.bytes, sig: Sig64(self.sig) };
        let event = signed.decode()?;
        Ok(StoredEvent { id: EventId(self.id), signed, event, deleted: self.deleted })
    }
}

/// Run a cached event query and collect the raw rows.
fn query_raw<P: Params>(conn: &Connection, sql: &str, p: P) -> Result<Vec<RawEvent>> {
    let mut stmt = conn.prepare_cached(sql).db()?;
    let rows = stmt.query_map(p, RawEvent::from_row).db()?;
    rows.collect::<rusqlite::Result<Vec<_>>>().db()
}

/// Decode raw rows (outside the connection lock).
fn decode_all(raws: Vec<RawEvent>) -> Result<Vec<StoredEvent>> {
    raws.into_iter().map(RawEvent::into_stored).collect()
}

/// Insert one event with a cached statement. `Ok(true)` if newly stored.
fn insert_one(conn: &Connection, ev: &StoredEvent) -> Result<bool> {
    let e = &ev.event;
    // Not `event.server`: a Genesis belongs to the conversation named by its own id.
    let conv = ev.conv();
    let channel = channel_col(&e.body);
    let mut stmt = conn.prepare_cached(INSERT_EVENT).db()?;
    let res = stmt.execute(params![
        ev.id.0,
        conv.0,
        e.author.0,
        e.seq,
        e.hlc,
        e.body.kind_tag(),
        channel,
        ev.signed.bytes,
        ev.signed.sig.0,
        ev.deleted,
    ]);
    match res {
        Ok(n) => Ok(n == 1),
        Err(rusqlite::Error::SqliteFailure(f, _)) if f.code == ErrorCode::ConstraintViolation => {
            tracing::warn!(
                conv = %conv.short(),
                author = %e.author.short(),
                seq = e.seq,
                id = %ev.id.short(),
                "refusing a different event for an already stored (author, seq)"
            );
            Err(CoreError::Storage(format!(
                "conflicting event {} for author {} seq {} in conversation {}",
                ev.id.short(),
                e.author.short(),
                e.seq,
                conv.short()
            )))
        }
        Err(other) => Err(db_err(other)),
    }
}

impl SqliteStorage {
    /// Insert many events in one transaction (sync pages, bulk import). Same per-event
    /// semantics as [`Storage::insert_event`]: duplicates by id are skipped, nothing is
    /// validated. Returns how many were newly stored. All-or-nothing: on any error
    /// (including a conflicting `(author, seq)`) nothing from this batch is kept.
    pub fn insert_events_batch(&self, events: &[StoredEvent]) -> Result<usize> {
        let mut conn = self.conn();
        let tx = conn.transaction().db()?;
        let mut added = 0;
        for ev in events {
            if insert_one(&tx, ev)? {
                added += 1;
            }
        }
        tx.commit().db()?;
        Ok(added)
    }
}

/// Map a `convs` row selected as
/// `id, is_dm, peer, name, server_secret, last_epoch, last_host, joined_ms`.
fn conv_from_row(r: &Row<'_>) -> rusqlite::Result<ConvRecord> {
    Ok(ConvRecord {
        id: ConvId(r.get(0)?),
        is_dm: r.get(1)?,
        peer: r.get::<_, Option<[u8; 32]>>(2)?.map(UserId),
        name: r.get(3)?,
        server_secret: r.get(4)?,
        last_epoch: r.get(5)?,
        last_host: r.get::<_, Option<[u8; 32]>>(6)?.map(UserId),
        joined_ms: r.get(7)?,
    })
}

impl Storage for SqliteStorage {
    /// Insert or replace every field of the record with that id.
    fn upsert_conv(&self, c: &ConvRecord) -> Result<()> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare_cached(
                "INSERT INTO convs (id, is_dm, peer, name, server_secret, last_epoch, last_host, joined_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT (id) DO UPDATE SET
                    is_dm = excluded.is_dm, peer = excluded.peer, name = excluded.name,
                    server_secret = excluded.server_secret, last_epoch = excluded.last_epoch,
                    last_host = excluded.last_host, joined_ms = excluded.joined_ms",
            )
            .db()?;
        stmt.execute(params![
            c.id.0,
            c.is_dm,
            c.peer.map(|u| u.0),
            c.name,
            c.server_secret,
            c.last_epoch,
            c.last_host.map(|u| u.0),
            c.joined_ms,
        ])
        .db()?;
        Ok(())
    }

    fn conv(&self, id: &ConvId) -> Result<Option<ConvRecord>> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(
            "SELECT id, is_dm, peer, name, server_secret, last_epoch, last_host, joined_ms
             FROM convs WHERE id = ?1",
        ).db()?;
        stmt.query_row([id.0], conv_from_row).optional().db()
    }

    /// Ordered by join time, then id.
    fn convs(&self) -> Result<Vec<ConvRecord>> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(
            "SELECT id, is_dm, peer, name, server_secret, last_epoch, last_host, joined_ms
             FROM convs ORDER BY joined_ms, id",
        ).db()?;
        let rows = stmt.query_map([], conv_from_row).db()?;
        rows.collect::<rusqlite::Result<Vec<_>>>().db()
    }

    /// Removes the record and everything stored under the conversation: its events,
    /// cached peers and read markers (one transaction). A no-op for an unknown id.
    fn delete_conv(&self, id: &ConvId) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction().db()?;
        for sql in [
            "DELETE FROM events WHERE conv = ?1",
            "DELETE FROM peers WHERE conv = ?1",
            "DELETE FROM read_markers WHERE conv = ?1",
            "DELETE FROM convs WHERE id = ?1",
        ] {
            tx.prepare_cached(sql).db()?.execute([id.0]).db()?;
        }
        tx.commit().db()?;
        Ok(())
    }

    /// Idempotent by id (`false` for a duplicate); not validated. A *different* event
    /// for an already stored `(conv, author, seq)` is refused with `CoreError::Storage`
    /// (it can only come from an equivocating author; `ConvState` rejects it first).
    fn insert_event(&self, ev: &StoredEvent) -> Result<bool> {
        insert_one(&self.conn(), ev)
    }

    /// One transaction for the whole slice: one WAL commit per sync page instead of one per
    /// event. A conflicting `(author, seq)` fails only its own statement (SQLite keeps the
    /// transaction), so the rest of the page is still stored. If the transaction cannot be
    /// opened or committed, the events are inserted one by one instead.
    fn insert_events(&self, evs: &[&StoredEvent]) -> Vec<Result<bool>> {
        if evs.len() < 2 {
            return evs.iter().map(|e| self.insert_event(e)).collect();
        }
        let batched = {
            let mut conn = self.conn();
            match conn.transaction() {
                Ok(tx) => {
                    let res: Vec<Result<bool>> = evs.iter().map(|e| insert_one(&tx, e)).collect();
                    match tx.commit() {
                        Ok(()) => Some(res),
                        Err(e) => {
                            tracing::warn!(error = %e, "committing a batch of events failed; inserting one by one");
                            None
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "starting a batch of events failed; inserting one by one");
                    None
                }
            }
        };
        batched.unwrap_or_else(|| evs.iter().map(|e| self.insert_event(e)).collect())
    }

    fn event(&self, id: &EventId) -> Result<Option<StoredEvent>> {
        let raw = {
            let conn = self.conn();
            let mut stmt = conn.prepare_cached("SELECT id, bytes, sig, deleted FROM events WHERE id = ?1").db()?;
            stmt.query_row([id.0], RawEvent::from_row).optional().db()?
        };
        raw.map(RawEvent::into_stored).transpose()
    }

    /// Highest stored seq per author, ordered by author.
    fn heads(&self, conv: &ConvId) -> Result<Vec<(UserId, u64)>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare_cached("SELECT author, MAX(seq) FROM events WHERE conv = ?1 GROUP BY author ORDER BY author")
            .db()?;
        let rows = stmt.query_map([conv.0], |r| Ok((UserId(r.get(0)?), r.get::<_, u64>(1)?))).db()?;
        rows.collect::<rusqlite::Result<Vec<_>>>().db()
    }

    /// Inclusive range, ascending seq. Empty when `from_seq > to_seq`.
    fn events_range(&self, conv: &ConvId, author: &UserId, from_seq: u64, to_seq: u64) -> Result<Vec<StoredEvent>> {
        if from_seq > to_seq {
            return Ok(Vec::new());
        }
        let raws = {
            let conn = self.conn();
            query_raw(
                &conn,
                "SELECT id, bytes, sig, deleted FROM events
                 WHERE conv = ?1 AND author = ?2 AND seq BETWEEN ?3 AND ?4
                 ORDER BY seq",
                params![conv.0, author.0, bound(from_seq), bound(to_seq)],
            )?
        };
        decode_all(raws)
    }

    /// Every non-Message event (Deletes included), ordered by (author, seq).
    fn state_events(&self, conv: &ConvId) -> Result<Vec<StoredEvent>> {
        let raws = query_raw(&self.conn(), STATE_EVENTS, [conv.0])?;
        decode_all(raws)
    }

    /// `channel = None` pages every message of the conversation regardless of channel
    /// (DMs). Newest first by (hlc, id); `before` is exclusive. Deleted messages are
    /// included with `deleted = true` (their signed bytes are kept for sync).
    fn messages(
        &self,
        conv: &ConvId,
        channel: Option<&ChannelId>,
        before: Option<(u64, EventId)>,
        limit_n: usize,
    ) -> Result<Vec<StoredEvent>> {
        if limit_n == 0 {
            return Ok(Vec::new());
        }
        let n = limit(limit_n);
        let raws = {
            let conn = self.conn();
            match (channel, before) {
                (Some(ch), None) => query_raw(&conn, MSG_CHANNEL, params![conv.0, ch.0, n])?,
                (Some(ch), Some((hlc, id))) => {
                    query_raw(&conn, MSG_CHANNEL_BEFORE, params![conv.0, ch.0, n, bound(hlc), id.0])?
                }
                (None, None) => query_raw(&conn, MSG_ALL, params![conv.0, n])?,
                (None, Some((hlc, id))) => query_raw(&conn, MSG_ALL_BEFORE, params![conv.0, n, bound(hlc), id.0])?,
            }
        };
        decode_all(raws)
    }

    /// Sets the flag on `target` if it is stored under `conv`; otherwise a no-op.
    fn mark_deleted(&self, conv: &ConvId, target: &EventId) -> Result<()> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached("UPDATE events SET deleted = 1 WHERE id = ?1 AND conv = ?2").db()?;
        stmt.execute([target.0, conv.0]).db()?;
        Ok(())
    }

    /// Insert or replace the cached address and last-seen time of `peer.user` in `conv`.
    fn upsert_peer(&self, conv: &ConvId, peer: &PeerRecord) -> Result<()> {
        let addr = postcard::to_stdvec(&peer.addr).map_err(CoreError::from)?;
        let conn = self.conn();
        let mut stmt = conn
            .prepare_cached(
                "INSERT INTO peers (conv, user, addr, last_seen_ms) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (conv, user) DO UPDATE SET addr = excluded.addr, last_seen_ms = excluded.last_seen_ms",
            )
            .db()?;
        stmt.execute(params![conv.0, peer.user.0, addr, peer.last_seen_ms]).db()?;
        Ok(())
    }

    /// Most recently seen first (ties by user id), at most `limit_n`.
    fn peers(&self, conv: &ConvId, limit_n: usize) -> Result<Vec<PeerRecord>> {
        if limit_n == 0 {
            return Ok(Vec::new());
        }
        let rows: Vec<([u8; 32], Vec<u8>, u64)> = {
            let conn = self.conn();
            let mut stmt = conn
                .prepare_cached(
                    "SELECT user, addr, last_seen_ms FROM peers WHERE conv = ?1
                     ORDER BY last_seen_ms DESC, user LIMIT ?2",
                )
                .db()?;
            let rows = stmt.query_map(params![conv.0, limit(limit_n)], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).db()?;
            rows.collect::<rusqlite::Result<Vec<_>>>().db()?
        };
        rows.into_iter()
            .map(|(user, addr, last_seen_ms)| {
                let addr: AddrHint = postcard::from_bytes(&addr)?;
                Ok(PeerRecord { user: UserId(user), addr, last_seen_ms })
            })
            .collect()
    }

    fn get_kv(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached("SELECT value FROM kv WHERE key = ?1").db()?;
        stmt.query_row([key], |r| r.get(0)).optional().db()
    }

    /// Insert or overwrite.
    fn set_kv(&self, key: &str, value: &[u8]) -> Result<()> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare_cached("INSERT INTO kv (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value")
            .db()?;
        stmt.execute(params![key, value]).db()?;
        Ok(())
    }
}
