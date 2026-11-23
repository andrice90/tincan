//! Short code records (docs/architecture.md "Short codes").
//!
//! A 6-character code (`tincan_core::code`) maps through argon2id to a record key and a read
//! key. The record is a pkarr `SignedPacket` signed by the record key, holding the sealed
//! payload as TXT chunks under `_tincan` (the same layout as the rendezvous record):
//! - invite codes: the full `Invite` (server, invite seed, rendezvous keys, the host and a
//!   successor with address hints). The host of the server publishes and refreshes it, and
//!   the owner publishes it at once when rotating.
//! - friend codes: a `FriendRecord` (user id and address, signed by the identity key). The
//!   user's own app publishes and refreshes it.
//!
//! Where records live ([`CodeDir`]): n0's pkarr relay and mainline DHT through
//! [`Rendezvous`] when the rendezvous layer is on, otherwise a shared local folder
//! (`NetConfig::code_dir`: offline test mode, so two profiles on one machine and the
//! in-process tests resolve codes without any network). Writes follow the rendezvous write
//! rule (`seq = max(now_us, highest seen + 1)`); a rotated code gets a tombstone (a packet
//! with an empty payload) with a higher seq, so it stops resolving.
//!
//! Holding a code gives write access to its slot, never authority: the code commits to its
//! target (`tincan_core::code::invite_commit`, `friend_commit`), and a record counts only
//! when its target and nonce recompute to the code. Resolving tries the plausible packets
//! newest first and takes the first one that verifies, so junk written into a slot only
//! hides the record until its owner rewrites it (the publishers overwrite anything that does
//! not verify).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::Duration;

use iroh_base::{PublicKey, SecretKey};
use iroh_dns::pkarr::{SignedPacket, Timestamp};
use n0_mainline::{MutableItem, SigningKey};
use tincan_core::code::{CodeKeys, CodeKind, FriendCode, FriendRecord, InviteRecord, ShortCode, invite_commit, invite_material, verify_friend, verify_invite};
use tincan_core::invite::invite_secret;
use tincan_core::invite::Invite;
use tincan_core::keys::ServerKeys;
use tincan_core::rdv::{TXT_NAME, from_txt_chunks, to_txt_chunks};
use tincan_core::wire::AddrHint;
use tincan_core::{PROTOCOL_VERSION, ServerId, UserId, now_ms};
use tokio::time::Instant;

use crate::error::{NetError, Result};
use crate::inner::Inner;
use crate::rdv::{DHT_REFRESH, RECORD_TTL_S, RELAY_REFRESH, Rendezvous, item_to_packet, next_seq, packet_time_ok};
use crate::session::SessionShared;

/// Derived keys kept per (kind, code), so refreshes do not re-run the KDF.
const KEY_CACHE: usize = 64;
/// A foreign invite record younger than this is not overwritten (code collision).
const FOREIGN_FRESH_US: u64 = 2 * 60 * 60 * 1_000_000;
/// First publish after becoming host.
const HOST_FIRST_PUBLISH: Duration = Duration::from_secs(2);
/// How often the host checks whether its invite record needs a write.
const HOST_CHECK: Duration = Duration::from_secs(30);
/// How often the friend publisher checks my addresses.
const FRIEND_CHECK: Duration = Duration::from_secs(60);
/// Retry after a failed write.
const RETRY: Duration = Duration::from_secs(60);
/// Local folder: refresh interval (only so a deleted file comes back).
const LOCAL_REFRESH: Duration = Duration::from_secs(5 * 60);

/// Not-found text for invite codes.
pub(crate) const NO_SUCH_INVITE_CODE: &str = "No server uses that code. Check it, or ask for a new one.";
/// Not-found text for friend codes.
pub(crate) const NO_SUCH_FRIEND_CODE: &str = "No one uses that friend code. Check it, or ask for a new one.";
/// Short codes are off (no rendezvous layer and no local folder).
pub(crate) const CODES_OFF: &str = "Short codes need an internet connection. Use the full invite or ID instead.";

/// Where code records are written and read.
#[derive(Debug)]
pub(crate) enum CodeDir {
    /// n0 pkarr relay (when configured) and mainline DHT.
    Pkarr(Arc<Rendezvous>),
    /// A folder shared by every profile on this machine (offline mode, tests).
    Local(PathBuf),
}

/// Outcome of a write.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Written {
    /// Stored somewhere: (relay, DHT); the folder counts as both.
    Done(bool, bool),
    /// A fresh record of someone else is there (a code collision); left alone.
    Foreign,
    /// Nothing accepted it.
    Failed,
}

/// Process-wide lock for read-compare-write on the local folder.
static LOCAL_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

impl CodeDir {
    /// The local folder (writes finish at once).
    pub(crate) fn is_local(&self) -> bool { matches!(self, CodeDir::Local(_)) }

    /// Every verified packet under `pk` (with whether it came from the DHT).
    async fn get(&self, pk: &PublicKey, readable: &(dyn Fn(&SignedPacket) -> bool + Sync)) -> Vec<(SignedPacket, bool)> {
        match self {
            CodeDir::Pkarr(rdv) => rdv.get_all(pk, readable).await,
            CodeDir::Local(dir) => read_local(dir, pk).await.map(|sp| (sp, false)).into_iter().collect(),
        }
    }

    /// Write `sealed` (empty = tombstone) under `sk`. `veto` marks a fresh record that must
    /// not be overwritten: it returns a check to run on a blocking thread (the commitment
    /// check runs the KDF) for a packet worth checking. `to_dht`: also write the DHT copy
    /// (ignored for the folder).
    pub(crate) async fn put(&self, sk: &SecretKey, sealed: &[u8], to_dht: bool, readable: &(dyn Fn(&SignedPacket) -> bool + Sync), veto: Veto<'_>) -> Written {
        let pk = sk.public();
        let _guard = match self {
            CodeDir::Local(_) => Some(LOCAL_LOCK.lock().await),
            CodeDir::Pkarr(_) => None,
        };
        let now_us = Timestamp::now().as_micros();
        let seen = self.get(&pk, readable).await;
        let plausible: Vec<&(SignedPacket, bool)> = seen.iter().filter(|(sp, _)| packet_time_ok(sp.timestamp().as_micros(), now_us)).collect();
        if let Some((newest, _)) = plausible.iter().max_by_key(|(sp, _)| sp.timestamp().as_micros())
            && now_us.saturating_sub(newest.timestamp().as_micros()) < FOREIGN_FRESH_US
            && let Some(check) = veto(newest)
            && tokio::task::spawn_blocking(check).await.unwrap_or(false)
        {
            return Written::Foreign;
        }
        let max_seq = plausible.iter().map(|(sp, _)| sp.timestamp().as_micros()).max();
        let dht_seq = plausible.iter().filter(|(_, d)| *d).map(|(sp, _)| sp.timestamp().as_micros()).max();
        let sp = match code_packet(sk, sealed, next_seq(now_us, max_seq)) {
            Ok(sp) => sp,
            Err(e) => {
                tracing::warn!(error = %e, "cannot build a code record");
                return Written::Failed;
            }
        };
        match self {
            CodeDir::Pkarr(rdv) => match rdv.put(&sp, dht_seq, to_dht).await {
                (false, false) => Written::Failed,
                (relay, dht) => Written::Done(relay, dht),
            },
            CodeDir::Local(dir) => {
                if write_local(dir, &sp).await {
                    Written::Done(true, true)
                } else {
                    Written::Failed
                }
            }
        }
    }

    /// Every plausible packet under `pk`, newest first (unverified: check each).
    pub(crate) async fn candidates(&self, pk: &PublicKey, readable: &(dyn Fn(&SignedPacket) -> bool + Sync)) -> Vec<SignedPacket> {
        let now_us = Timestamp::now().as_micros();
        let mut v: Vec<SignedPacket> =
            self.get(pk, readable).await.into_iter().map(|(sp, _)| sp).filter(|sp| packet_time_ok(sp.timestamp().as_micros(), now_us)).collect();
        v.sort_by_key(|sp| std::cmp::Reverse(sp.timestamp().as_micros()));
        v.dedup_by(|a, b| a.as_bytes() == b.as_bytes());
        v
    }

    fn refresh(&self) -> (Duration, Duration) {
        match self {
            CodeDir::Pkarr(rdv) if rdv.has_relay() => (RELAY_REFRESH, DHT_REFRESH),
            CodeDir::Pkarr(_) => (DHT_REFRESH, DHT_REFRESH),
            CodeDir::Local(_) => (LOCAL_REFRESH, LOCAL_REFRESH),
        }
    }
}

/// A pkarr packet carrying `sealed` (empty: a tombstone) signed by `sk` with timestamp `seq_us`.
pub(crate) fn code_packet(sk: &SecretKey, sealed: &[u8], seq_us: u64) -> std::result::Result<SignedPacket, String> {
    let chunks = if sealed.is_empty() { vec!["0=".to_string()] } else { to_txt_chunks(sealed) };
    let base = SignedPacket::from_txt_strings(sk, TXT_NAME, &chunks, RECORD_TTL_S).map_err(|e| e.to_string())?;
    let signing = SigningKey::from_bytes(&sk.to_bytes());
    let seq = i64::try_from(seq_us).map_err(|_| "seq out of range".to_string())?;
    item_to_packet(&MutableItem::new(&signing, base.encoded_packet(), seq, None)).map_err(|e| e.to_string())
}

/// The sealed payload of a packet (empty for a tombstone or garbage).
pub(crate) fn payload(sp: &SignedPacket) -> Vec<u8> { from_txt_chunks(&sp.txt_records(TXT_NAME)).unwrap_or_default() }

fn local_path(dir: &std::path::Path, pk: &PublicKey) -> PathBuf { dir.join(format!("{}.pkarr", pk.to_z32())) }

async fn read_local(dir: &std::path::Path, pk: &PublicKey) -> Option<SignedPacket> {
    let bytes = tokio::fs::read(local_path(dir, pk)).await.ok()?;
    let sp = SignedPacket::from_bytes(&bytes).ok()?;
    (sp.public_key() == *pk).then_some(sp)
}

async fn write_local(dir: &std::path::Path, sp: &SignedPacket) -> bool {
    if let Err(e) = tokio::fs::create_dir_all(dir).await {
        tracing::warn!(error = %e, "cannot create the local code folder");
        return false;
    }
    let path = local_path(dir, &sp.public_key());
    let tmp = path.with_extension(format!("tmp{}", u32::from_le_bytes(tincan_core::keys::random_bytes())));
    if let Err(e) = tokio::fs::write(&tmp, sp.as_bytes()).await {
        tracing::warn!(error = %e, "cannot write a local code record");
        return false;
    }
    match tokio::fs::rename(&tmp, &path).await {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(error = %e, "cannot write a local code record");
            let _ = tokio::fs::remove_file(&tmp).await;
            false
        }
    }
}

/// Derived keys, cached (the KDF runs on a blocking thread).
pub(crate) async fn code_keys(inner: &Inner, kind: CodeKind, code: ShortCode) -> CodeKeys {
    if let Some(k) = inner.code_keys.lock().get(&(kind, code)) {
        return k.clone();
    }
    let keys = match tokio::task::spawn_blocking(move || CodeKeys::derive(kind, code)).await {
        Ok(k) => k,
        Err(_) => CodeKeys::derive(kind, code),
    };
    let mut cache = inner.code_keys.lock();
    if cache.len() >= KEY_CACHE {
        cache.clear();
    }
    cache.insert((kind, code), keys.clone());
    keys
}

/// The server's current invite code: the newest owner-made short-code `InviteCreate` that
/// is not revoked and whose key matches its marker's seed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ActiveCode {
    /// The code.
    pub code: ShortCode,
    /// The invite key.
    pub invite_pk: [u8; 32],
    /// The invite seed.
    pub seed: [u8; 16],
    /// The commitment nonce.
    pub nonce: [u8; 16],
}

/// The code of a short-code invite (the commitment runs the KDF; cached per invite key).
pub(crate) async fn commit_code(inner: &Inner, server: ServerId, invite_pk: [u8; 32], nonce: [u8; 16]) -> ShortCode {
    if let Some(c) = inner.invite_codes.lock().get(&invite_pk) {
        return *c;
    }
    let code = match tokio::task::spawn_blocking(move || invite_commit(&server, &invite_pk, &nonce)).await {
        Ok(c) => c,
        Err(_) => invite_commit(&server, &invite_pk, &nonce),
    };
    let mut cache = inner.invite_codes.lock();
    if cache.len() >= KEY_CACHE {
        cache.clear();
    }
    cache.insert(invite_pk, code);
    code
}

/// See [`ActiveCode`].
pub(crate) async fn active_invite_code(inner: &Inner, server: &ServerId) -> Option<ActiveCode> {
    let rep = inner.replica(server)?;
    let (code_key, candidates) = {
        let r = rep.lock();
        (r.keys.as_ref()?.code_key, r.state.short_code_invites())
    };
    for (marker, invite_pk) in candidates {
        let (seed, nonce) = invite_material(&code_key, marker);
        if *invite_secret(&seed, server).public().as_bytes() == invite_pk {
            let code = commit_code(inner, *server, invite_pk, nonce).await;
            return Some(ActiveCode { code, invite_pk, seed, nonce });
        }
    }
    None
}

/// The record for `server`'s code right now: the host (or me) and the first successor (or
/// me) with their addresses.
fn record_for(inner: &Inner, server: &ServerId, active: &ActiveCode, shared: Option<&SessionShared>) -> Option<InviteRecord> {
    let rep = inner.replica(server)?;
    let (skeys, name) = {
        let r = rep.lock();
        (r.keys.clone()?, r.state.name.clone())
    };
    let roster = shared.and_then(|s| s.roster.lock().clone());
    let host = shared.and_then(|s| s.info.lock().host);
    let mut bootstrap: Vec<(UserId, AddrHint)> = Vec::new();
    let mut push = |u: UserId, hint: Option<AddrHint>| {
        if !bootstrap.iter().any(|(x, _)| *x == u) {
            bootstrap.push((u, hint.unwrap_or_default()));
        }
    };
    let addr_of = |u: &UserId| -> Option<AddrHint> {
        if *u == inner.me {
            return Some(inner.my_hint());
        }
        roster.as_ref().and_then(|r| r.online.iter().find(|e| e.user == *u).map(|e| e.addr.clone())).or_else(|| inner.hint(u))
    };
    if let Some(h) = host {
        push(h, addr_of(&h));
    }
    if let Some(s) = roster.as_ref().and_then(|r| r.succession.iter().find(|u| Some(**u) != host).copied()) {
        push(s, addr_of(&s));
    }
    push(inner.me, Some(inner.my_hint()));
    bootstrap.truncate(tincan_core::invite::MAX_BOOTSTRAP);
    let base = Invite {
        v: PROTOCOL_VERSION,
        server: *server,
        name_hint: name,
        invite_seed: active.seed,
        rdv_pk: *skeys.rdv_pk().as_bytes(),
        rdv_read_key: skeys.rdv_read_key,
        bootstrap,
        expires_ms: u64::MAX,
    };
    Some(InviteRecord::new(&base, active.nonce))
}

/// Write `server`'s invite code record now.
pub(crate) async fn publish_invite_record(inner: &Inner, server: &ServerId, active: &ActiveCode, shared: Option<&SessionShared>, to_dht: bool) -> Written {
    let Some(dir) = inner.codes.clone() else { return Written::Failed };
    let keys = code_keys(inner, CodeKind::Invite, active.code).await;
    let Some(rec) = record_for(inner, server, active, shared) else { return Written::Failed };
    let sealed = match rec.seal(&keys) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "cannot seal the invite code record");
            return Written::Failed;
        }
    };
    let readable = |sp: &SignedPacket| InviteRecord::open(&keys, &payload(sp)).is_ok();
    // A record of another server that verifies: two servers drew the same code. A record
    // that does not verify is junk and gets overwritten.
    let (code, me, vkeys) = (active.code, *server, keys.clone());
    let veto = move |sp: &SignedPacket| -> Option<Check> {
        let rec = InviteRecord::open(&vkeys, &payload(sp)).ok().filter(|r| r.invite.server != me)?;
        Some(Box::new(move || verify_invite(code, &rec)))
    };
    let out = dir.put(&keys.record_sk, &sealed, to_dht, &readable, &veto).await;
    if out == Written::Foreign {
        tracing::warn!(server = %server.short(), "another server holds this invite code; rotate it");
    }
    out
}

/// Write a tombstone over `code`'s record (a rotated code).
pub(crate) async fn clear_record(inner: &Inner, kind: CodeKind, code: ShortCode) -> Written {
    let Some(dir) = inner.codes.clone() else { return Written::Failed };
    let keys = code_keys(inner, kind, code).await;
    dir.put(&keys.record_sk, &[], true, &|_| false, &|_| None).await
}

/// Resolve an invite code to its invite: the newest record that verifies.
pub(crate) async fn resolve_invite_code(inner: &Inner, code: ShortCode) -> Result<Invite> {
    let dir = inner.codes.clone().ok_or_else(|| NetError::Invalid(CODES_OFF.into()))?;
    let keys = code_keys(inner, CodeKind::Invite, code).await;
    let readable = |sp: &SignedPacket| InviteRecord::open(&keys, &payload(sp)).is_ok();
    for sp in dir.candidates(&keys.record_pk(), &readable).await {
        let Ok(rec) = InviteRecord::open(&keys, &payload(&sp)) else { continue };
        let checked = rec.clone();
        if tokio::task::spawn_blocking(move || verify_invite(code, &checked)).await.unwrap_or(false) {
            return Ok(rec.invite);
        }
        tracing::debug!("ignoring an invite code record that does not match its code");
    }
    Err(NetError::Rejected(NO_SUCH_INVITE_CODE.into()))
}

/// Resolve a friend code to the user and their address: the newest record that verifies.
pub(crate) async fn resolve_friend_code(inner: &Inner, code: ShortCode) -> Result<(UserId, AddrHint)> {
    let dir = inner.codes.clone().ok_or_else(|| NetError::Invalid(CODES_OFF.into()))?;
    let keys = code_keys(inner, CodeKind::Friend, code).await;
    let readable = |sp: &SignedPacket| FriendRecord::open(&keys, &payload(sp)).is_ok();
    for sp in dir.candidates(&keys.record_pk(), &readable).await {
        let Ok(rec) = FriendRecord::open(&keys, &payload(&sp)) else { continue };
        let checked = rec.clone();
        if tokio::task::spawn_blocking(move || verify_friend(code, &checked, now_ms())).await.unwrap_or(false) {
            return Ok((rec.user, rec.addr));
        }
        tracing::debug!("ignoring a friend code record that does not match its code");
    }
    Err(NetError::Rejected(NO_SUCH_FRIEND_CODE.into()))
}

/// Write my friend code record now.
pub(crate) async fn publish_friend_record(inner: &Inner, fc: FriendCode, to_dht: bool) -> Written {
    let Some(dir) = inner.codes.clone() else { return Written::Failed };
    let keys = code_keys(inner, CodeKind::Friend, fc.code).await;
    let rec = FriendRecord::new(&inner.identity, &fc, &keys, &inner.my_hint(), now_ms());
    let sealed = match rec.seal(&keys) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "cannot seal the friend code record");
            return Written::Failed;
        }
    };
    let me = inner.me;
    let readable = |sp: &SignedPacket| FriendRecord::open(&keys, &payload(sp)).is_ok();
    let vkeys = keys.clone();
    let veto = move |sp: &SignedPacket| -> Option<Check> {
        let rec = FriendRecord::open(&vkeys, &payload(sp)).ok().filter(|r| r.user != me)?;
        Some(Box::new(move || verify_friend(fc.code, &rec, now_ms())))
    };
    dir.put(&keys.record_sk, &sealed, to_dht, &readable, &veto).await
}

/// When a record was last written.
#[derive(Debug, Default)]
struct Fresh {
    content: Option<Vec<u8>>,
    /// Any copy (relay, DHT or folder).
    any: Option<Instant>,
    /// The DHT copy (or the folder).
    dht: Option<Instant>,
    retry_at: Option<Instant>,
}

impl Fresh {
    /// (write now, include the DHT copy).
    fn due(&self, dir: &CodeDir, content: &[u8], now: Instant) -> (bool, bool) {
        if self.retry_at.is_some_and(|t| now < t) {
            return (false, false);
        }
        let (any_every, dht_every) = dir.refresh();
        let changed = self.content.as_deref() != Some(content);
        let dht_due = self.dht.is_none_or(|t| now.duration_since(t) >= dht_every);
        let any_due = self.any.is_none_or(|t| now.duration_since(t) >= any_every);
        (changed || dht_due || any_due, changed || dht_due)
    }

    fn note(&mut self, out: &Written, content: Vec<u8>, now: Instant) {
        match out {
            Written::Done(_, dht) => {
                self.content = Some(content);
                self.retry_at = None;
                self.any = Some(now);
                if *dht {
                    self.dht = Some(now);
                }
            }
            Written::Foreign | Written::Failed => self.retry_at = Some(now + RETRY),
        }
    }
}

/// Host role: keep `server`'s invite code record current (the host and successor
/// addresses) and fresh. Runs until aborted (the host role ends).
pub(crate) async fn host_invite_runner(net: Weak<Inner>, server: ServerId, _keys: ServerKeys, shared: Arc<SessionShared>) {
    tokio::time::sleep(HOST_FIRST_PUBLISH).await;
    let mut fresh = Fresh::default();
    let mut last_code: Option<ShortCode> = None;
    loop {
        {
            let Some(inner) = net.upgrade() else { return };
            let Some(dir) = inner.codes.clone() else { return };
            if let Some(active) = active_invite_code(&inner, &server).await {
                if last_code != Some(active.code) {
                    fresh = Fresh::default();
                    last_code = Some(active.code);
                }
                let content = record_for(&inner, &server, &active, Some(&shared)).and_then(|r| postcard::to_stdvec(&r).ok()).unwrap_or_default();
                let now = Instant::now();
                let (write, to_dht) = fresh.due(&dir, &content, now);
                if write {
                    let out = publish_invite_record(&inner, &server, &active, Some(&shared), to_dht).await;
                    tracing::debug!(server = %server.short(), ?out, "invite code record written");
                    fresh.note(&out, content, Instant::now());
                }
            }
        }
        tokio::time::sleep(HOST_CHECK).await;
    }
}

/// Keep my friend code record current and fresh. Runs until aborted (the code changed or
/// the network stopped).
pub(crate) async fn friend_runner(net: Weak<Inner>, code: FriendCode) {
    let mut fresh = Fresh::default();
    loop {
        {
            let Some(inner) = net.upgrade() else { return };
            let Some(dir) = inner.codes.clone() else { return };
            let content = postcard::to_stdvec(&inner.my_hint()).unwrap_or_default();
            let now = Instant::now();
            let (write, to_dht) = fresh.due(&dir, &content, now);
            if write {
                let out = publish_friend_record(&inner, code, to_dht).await;
                tracing::debug!(?out, "friend code record written");
                if out == Written::Foreign {
                    tracing::warn!("someone else's record holds my friend code; rotate it");
                }
                fresh.note(&out, content, Instant::now());
            }
        }
        tokio::time::sleep(FRIEND_CHECK).await;
    }
}

/// A deferred veto check (runs the KDF, so on a blocking thread).
pub(crate) type Check = Box<dyn FnOnce() -> bool + Send>;
/// See [`CodeDir::put`].
pub(crate) type Veto<'a> = &'a (dyn Fn(&SignedPacket) -> Option<Check> + Sync);

/// Bounded cache type for derived keys.
pub(crate) type KeyCache = HashMap<(CodeKind, ShortCode), CodeKeys>;
