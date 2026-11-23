//! Rendezvous record layer (docs/architecture.md "Rendezvous record").
//!
//! One encrypted record per server ([`RdvRecord`], sealed by tincan-core) is published as a
//! pkarr `SignedPacket` signed by the server's `rdv_sk`:
//! - to n0's pkarr relay (`N0_DNS_PKARR_RELAY_PROD`), an optional accelerator used only with
//!   `RelayChoice::N0Default`, and
//! - to mainline DHT as a BEP44 mutable item without salt, through an ephemeral `n0_mainline`
//!   client created for each operation and dropped afterwards.
//!
//! Write rule: resolve first, then `seq = max(now_us, highest seen + 1)` and put to the DHT
//! with `cas = the seq the DHT returned` (a node compares cas with its own stored seq, so the
//! relay's seq must not be used there). The packet timestamp is that same seq, re-signed
//! through `MutableItem::new` (pkarr and BEP44 sign the same bytes).
//!
//! Clobber rule: when the resolved record names another host with an epoch at least mine and
//! is younger than 15 minutes, the host does not publish. It dials that host instead
//! (`PeerSeen`), which leads to the partition-merge comparison.
//!
//! The record is only a hint: whatever it names is verified through `Hello`, the
//! `SignedHostClaim` and the member list. Nothing here runs when the rendezvous layer is off
//! (`NetConfig::rendezvous_enabled`).

use std::sync::{Arc, Weak};
use std::time::Duration;

use futures_lite::StreamExt;
use iroh::Endpoint;
use iroh::address_lookup::{N0_DNS_PKARR_RELAY_PROD, PkarrRelayClient};
use iroh_base::{PublicKey, SecretKey};
use iroh_dns::pkarr::{SignedPacket, Timestamp};
use n0_mainline::errors::{ConcurrencyError, PutMutableError};
use n0_mainline::{Dht, MutableItem, SigningKey};
use tincan_core::keys::ServerKeys;
use tincan_core::rdv::{MAX_ONLINE_IN_RECORD, MAX_RELAY_URL_CHARS, RdvRecord, TXT_NAME, from_txt_chunks, to_txt_chunks};
use tincan_core::wire::{AddrHint, Roster};
use tincan_core::{CoreError, PROTOCOL_VERSION, ServerId, UserId, now_ms};
use tokio::time::Instant;

use crate::events::ConnState;
use crate::inner::Inner;
use crate::session::{SessionCmd, SessionShared};

/// TTL of the TXT records in the packet.
const RECORD_TTL_S: u32 = 30;
/// A foreign host's record younger than this wins over publishing (clobber rule).
const FOREIGN_FRESH_MS: u64 = 15 * 60 * 1000;
/// A packet whose timestamp (seq) is further ahead of the clock than this is ignored: any
/// holder of the server secret (ex-members too) could otherwise pin the record forever.
const MAX_PACKET_AHEAD_US: u64 = 10 * 60 * 1_000_000;
/// A record whose `published_ms` is further ahead of the clock than this is ignored.
const MAX_RECORD_AHEAD_MS: u64 = 5 * 60 * 1000;

/// Is a packet's timestamp plausible (not far in the future)?
fn packet_time_ok(seq_us: u64, now_us: u64) -> bool { seq_us <= now_us.saturating_add(MAX_PACKET_AHEAD_US) }

/// Is a record plausible (published at most a little in the future)?
fn record_time_ok(rec: &RdvRecord, now_ms: u64) -> bool { rec.published_ms <= now_ms.saturating_add(MAX_RECORD_AHEAD_MS) }

/// Is a record fresh enough to defer to (published within the last 15 minutes)?
pub(crate) fn record_fresh(rec: &RdvRecord, now_ms: u64) -> bool { record_time_ok(rec, now_ms) && now_ms.saturating_sub(rec.published_ms) < FOREIGN_FRESH_MS }
/// Budget of a relay GET or PUT.
const RELAY_TIMEOUT: Duration = Duration::from_secs(4);
/// Budget of a DHT query (bootstrap included, about 3.5 s cold).
const DHT_BUDGET: Duration = Duration::from_secs(8);
/// After the first DHT item, keep draining this long for a higher seq.
const DHT_DRAIN: Duration = Duration::from_secs(3);
/// Budget of a DHT put (bootstrap included).
const DHT_PUT_BUDGET: Duration = Duration::from_secs(15);
/// Spacing of relay PUTs from this process (n0 allows about one per 4 s per IP).
const RELAY_PUT_SPACING: Duration = Duration::from_secs(4);
/// First publish after becoming host (the first roster is built by then).
const INITIAL_PUBLISH: Duration = Duration::from_secs(2);
/// Roster-change debounce.
const PUBLISH_DEBOUNCE: Duration = Duration::from_secs(30);
/// Minimum spacing between publishes of one server.
const PUBLISH_MIN_GAP: Duration = Duration::from_secs(10);
/// Relay copy refresh.
const RELAY_REFRESH: Duration = Duration::from_secs(5 * 60);
/// DHT copy refresh.
const DHT_REFRESH: Duration = Duration::from_secs(30 * 60);
/// Re-resolve interval while hosting with members (foreign host detection).
const RESOLVE_HOSTING: Duration = Duration::from_secs(5 * 60);
/// Re-resolve interval while solo.
const RESOLVE_SOLO: Duration = Duration::from_secs(60);
/// Solo for this long: re-resolve backs off to `RESOLVE_HOSTING`.
const SOLO_BACKOFF_AFTER: Duration = Duration::from_secs(30 * 60);
/// Retry delay after a failed publish.
const PUBLISH_RETRY: Duration = Duration::from_secs(60);
/// After dialing a foreign host named by the record: if this device still hosts after this
/// long, that host was unreachable or lost the merge, so publish anyway.
const FOREIGN_GRACE: Duration = Duration::from_secs(10);

/// Why building or reading a record failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RdvError {
    /// Sealing, opening or chunking failed.
    #[error("record: {0}")]
    Record(#[from] CoreError),
    /// The pkarr packet could not be built or did not verify.
    #[error("packet: {0}")]
    Packet(String),
    /// The packet is signed by another key.
    #[error("signed by another key")]
    WrongKey,
}

/// `max(now_us, seen + 1)`: the seq of the next write.
pub(crate) fn next_seq(now_us: u64, seen: Option<u64>) -> u64 {
    match seen {
        Some(s) => now_us.max(s.saturating_add(1)),
        None => now_us,
    }
}

/// Seal `rec` into a pkarr packet signed by `rdv_sk` with timestamp `seq_us`.
pub(crate) fn record_packet(rdv_sk: &SecretKey, read_key: &[u8; 32], rec: &RdvRecord, seq_us: u64) -> Result<SignedPacket, RdvError> {
    let sealed = rec.seal(read_key)?;
    let chunks = to_txt_chunks(&sealed);
    let base = SignedPacket::from_txt_strings(rdv_sk, TXT_NAME, &chunks, RECORD_TTL_S).map_err(|e| RdvError::Packet(e.to_string()))?;
    let signing = SigningKey::from_bytes(&rdv_sk.to_bytes());
    let seq = i64::try_from(seq_us).map_err(|_| RdvError::Packet("seq out of range".into()))?;
    item_to_packet(&MutableItem::new(&signing, base.encoded_packet(), seq, None))
}

/// The pkarr packet carried by a DHT item. Verifies the signature.
pub(crate) fn item_to_packet(item: &MutableItem) -> Result<SignedPacket, RdvError> {
    let seq = u64::try_from(item.seq()).map_err(|_| RdvError::Packet("negative seq".into()))?;
    let unchecked = SignedPacket::from_parts_unchecked(item.key(), item.signature(), Timestamp::from_micros(seq), item.value())
        .map_err(|e| RdvError::Packet(e.to_string()))?;
    SignedPacket::from_bytes(unchecked.as_bytes()).map_err(|e| RdvError::Packet(e.to_string()))
}

/// The DHT item for a pkarr packet (same signature, seq = timestamp).
pub(crate) fn packet_to_item(sp: &SignedPacket) -> Result<MutableItem, RdvError> {
    let seq = i64::try_from(sp.timestamp().as_micros()).map_err(|_| RdvError::Packet("timestamp out of range".into()))?;
    Ok(MutableItem::new_signed_unchecked(*sp.public_key().as_bytes(), sp.signature().to_bytes(), sp.encoded_packet(), seq, None))
}

/// Read the record out of a packet signed by `rdv_pk`.
pub(crate) fn open_packet(sp: &SignedPacket, rdv_pk: &PublicKey, read_key: &[u8; 32]) -> Result<RdvRecord, RdvError> {
    if sp.public_key() != *rdv_pk {
        return Err(RdvError::WrongKey);
    }
    let sealed = from_txt_chunks(&sp.txt_records(TXT_NAME))?;
    Ok(RdvRecord::open(&sealed, read_key)?)
}

/// The record a host publishes: online members in succession order first, then the rest of
/// the roster, at most `MAX_ONLINE_IN_RECORD`, the host itself excluded.
pub(crate) fn record_from_roster(roster: Option<&Roster>, epoch: u64, me: UserId, relay: Option<String>, published_ms: u64) -> RdvRecord {
    let mut online: Vec<UserId> = Vec::new();
    if let Some(r) = roster {
        for u in r.succession.iter().chain(r.online.iter().map(|e| &e.user)) {
            if *u != me && !online.contains(u) && online.len() < MAX_ONLINE_IN_RECORD {
                online.push(*u);
            }
        }
    }
    RdvRecord {
        v: PROTOCOL_VERSION,
        epoch,
        host: me,
        host_relay: relay.filter(|r| r.len() <= MAX_RELAY_URL_CHARS),
        published_ms,
        online,
    }
}

/// What a full resolve found.
#[derive(Debug, Default)]
pub(crate) struct Latest {
    /// Newest record that decrypted, with its seq.
    pub record: Option<(u64, RdvRecord)>,
    /// Highest seq of any validly signed packet (decryptable or not).
    pub max_seq: Option<u64>,
    /// Highest seq the DHT returned (the `cas` of the next put).
    pub dht_seq: Option<u64>,
}

impl Latest {
    fn note(&mut self, sp: &SignedPacket, rdv_pk: &PublicKey, read_key: &[u8; 32], from_dht: bool) {
        self.note_at(sp, rdv_pk, read_key, from_dht, Timestamp::now().as_micros(), now_ms());
    }

    /// `note` with explicit clocks. Far-future packets and records are left out entirely
    /// (also from `max_seq` / `dht_seq`, so they cannot push honest publishes out of range).
    fn note_at(&mut self, sp: &SignedPacket, rdv_pk: &PublicKey, read_key: &[u8; 32], from_dht: bool, now_us: u64, now_ms: u64) {
        if sp.public_key() != *rdv_pk {
            return;
        }
        let seq = sp.timestamp().as_micros();
        if !packet_time_ok(seq, now_us) {
            tracing::debug!(seq, "ignoring a rendezvous packet from the far future");
            return;
        }
        let rec = match open_packet(sp, rdv_pk, read_key) {
            Ok(rec) if !record_time_ok(&rec, now_ms) => {
                tracing::debug!("ignoring a rendezvous record published in the future");
                return;
            }
            Ok(rec) => Some(rec),
            Err(e) => {
                tracing::debug!(error = %e, "ignoring an unreadable rendezvous record");
                None
            }
        };
        self.max_seq = Some(self.max_seq.map_or(seq, |m| m.max(seq)));
        if from_dht {
            self.dht_seq = Some(self.dht_seq.map_or(seq, |m| m.max(seq)));
        }
        if let Some(rec) = rec
            && self.record.as_ref().is_none_or(|(s, _)| seq > *s)
        {
            self.record = Some((seq, rec));
        }
    }
}

/// A record from `sp` if it is readable and not from the future.
fn plausible_record(sp: &SignedPacket, rdv_pk: &PublicKey, read_key: &[u8; 32]) -> Option<RdvRecord> {
    if !packet_time_ok(sp.timestamp().as_micros(), Timestamp::now().as_micros()) {
        return None;
    }
    open_packet(sp, rdv_pk, read_key).ok().filter(|r| record_time_ok(r, now_ms()))
}

/// Outcome of a publish attempt.
#[derive(Debug)]
pub(crate) enum Published {
    /// Written to at least one place.
    Done {
        /// The relay accepted it.
        relay: bool,
        /// The DHT accepted it.
        dht: bool,
    },
    /// Another host holds a fresh record; it was not overwritten.
    Foreign(RdvRecord),
    /// Nothing accepted the write.
    Failed,
}

/// Publish/resolve client. One per `Net`.
pub(crate) struct Rendezvous {
    relay: Option<PkarrRelayClient>,
    relay_gate: tokio::sync::Mutex<Option<Instant>>,
}

impl std::fmt::Debug for Rendezvous {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rendezvous").field("relay", &self.relay.is_some()).finish()
    }
}

impl Rendezvous {
    /// A client. `use_relay`: also talk to n0's pkarr relay (needs the endpoint's DNS resolver).
    pub fn new(ep: &Endpoint, use_relay: bool) -> Self {
        let relay = if use_relay {
            match (url::Url::parse(N0_DNS_PKARR_RELAY_PROD), ep.dns_resolver()) {
                (Ok(url), Ok(dns)) => Some(PkarrRelayClient::new(url, ep.tls_config().clone(), dns.clone())),
                (Err(e), _) => {
                    tracing::warn!(error = %e, "bad pkarr relay URL; rendezvous uses the DHT only");
                    None
                }
                (_, Err(e)) => {
                    tracing::warn!(error = %e, "no DNS resolver; rendezvous uses the DHT only");
                    None
                }
            }
        } else {
            None
        };
        Self { relay, relay_gate: tokio::sync::Mutex::new(None) }
    }

    async fn relay_get(&self, rdv_pk: &PublicKey) -> Option<SignedPacket> {
        let client = self.relay.as_ref()?;
        match tokio::time::timeout(RELAY_TIMEOUT, client.resolve(*rdv_pk)).await {
            Ok(Ok(sp)) => Some(sp),
            Ok(Err(e)) => {
                tracing::debug!(error = %e, "pkarr relay resolve failed");
                None
            }
            Err(_) => None,
        }
    }

    /// Valid packets from the DHT. `first_only`: stop at the first item that decrypts.
    async fn dht_get(rdv_pk: &PublicKey, read_key: &[u8; 32], first_only: bool) -> Vec<SignedPacket> {
        let mut out = Vec::new();
        let dht = match Dht::client() {
            Ok(d) => d,
            Err(e) => {
                tracing::debug!(error = %e, "DHT client failed to start");
                return out;
            }
        };
        let deadline = Instant::now() + DHT_BUDGET;
        let mut stream = match tokio::time::timeout_at(deadline, dht.get_mutable(rdv_pk.as_bytes(), None, None)).await {
            Ok(Ok(s)) => s,
            _ => return out,
        };
        let mut end = deadline;
        loop {
            let next = match tokio::time::timeout_at(end, stream.next()).await {
                Ok(Some(item)) => item,
                Ok(None) | Err(_) => break,
            };
            let sp = match item_to_packet(&next) {
                Ok(sp) => sp,
                Err(e) => {
                    tracing::debug!(error = %e, "ignoring an invalid DHT item");
                    continue;
                }
            };
            let readable = open_packet(&sp, rdv_pk, read_key).is_ok();
            out.push(sp);
            if readable {
                if first_only {
                    break;
                }
                end = end.min(Instant::now() + DHT_DRAIN);
            }
        }
        out
    }

    /// The first record that decrypts, from the relay or the DHT, whichever answers first.
    pub async fn resolve_first(&self, rdv_pk: &PublicKey, read_key: &[u8; 32]) -> Option<RdvRecord> {
        let relay = async {
            let sp = self.relay_get(rdv_pk).await?;
            plausible_record(&sp, rdv_pk, read_key)
        };
        let dht = async {
            let items = Self::dht_get(rdv_pk, read_key, true).await;
            items.iter().find_map(|sp| plausible_record(sp, rdv_pk, read_key))
        };
        tokio::pin!(relay);
        tokio::pin!(dht);
        let (mut relay_done, mut dht_done) = (false, false);
        while !(relay_done && dht_done) {
            tokio::select! {
                r = &mut relay, if !relay_done => {
                    relay_done = true;
                    if r.is_some() {
                        return r;
                    }
                }
                d = &mut dht, if !dht_done => {
                    dht_done = true;
                    if d.is_some() {
                        return d;
                    }
                }
            }
        }
        None
    }

    /// Everything the relay and the DHT return, for the write rule and foreign-host checks.
    pub async fn resolve_latest(&self, rdv_pk: &PublicKey, read_key: &[u8; 32]) -> Latest {
        let (relay, dht) = tokio::join!(self.relay_get(rdv_pk), Self::dht_get(rdv_pk, read_key, false));
        let mut latest = Latest::default();
        if let Some(sp) = relay {
            latest.note(&sp, rdv_pk, read_key, false);
        }
        for sp in &dht {
            latest.note(sp, rdv_pk, read_key, true);
        }
        latest
    }

    /// True if the n0 pkarr relay is used.
    pub fn has_relay(&self) -> bool { self.relay.is_some() }

    /// Resolve, apply the clobber rule (unless `force`), then write `rec` to the relay (if
    /// configured) and, when `to_dht`, to the DHT.
    pub async fn publish(&self, keys: &ServerKeys, rec: &RdvRecord, to_dht: bool, force: bool) -> Published {
        let rdv_pk = keys.rdv_pk();
        let latest = self.resolve_latest(&rdv_pk, &keys.rdv_read_key).await;
        if !force
            && let Some((_, r)) = &latest.record
            && r.host != rec.host
            && r.epoch >= rec.epoch
            && record_fresh(r, now_ms())
        {
            return Published::Foreign(r.clone());
        }
        let seq = next_seq(Timestamp::now().as_micros(), latest.max_seq);
        let sp = match record_packet(&keys.rdv_sk, &keys.rdv_read_key, rec, seq) {
            Ok(sp) => sp,
            Err(e) => {
                tracing::warn!(error = %e, "cannot build the rendezvous record");
                return Published::Failed;
            }
        };
        let relay = self.relay_put(&sp).await;
        let dht = to_dht && Self::dht_put(&sp, latest.dht_seq).await;
        if relay || dht { Published::Done { relay, dht } } else { Published::Failed }
    }

    async fn relay_put(&self, sp: &SignedPacket) -> bool {
        let Some(client) = &self.relay else { return false };
        let mut last = self.relay_gate.lock().await;
        if let Some(t) = *last {
            tokio::time::sleep_until(t + RELAY_PUT_SPACING).await;
        }
        let res = tokio::time::timeout(RELAY_TIMEOUT, client.publish(sp)).await;
        *last = Some(Instant::now());
        match res {
            Ok(Ok(())) => true,
            Ok(Err(e)) => {
                tracing::debug!(error = %e, "pkarr relay publish failed");
                false
            }
            Err(_) => false,
        }
    }

    async fn dht_put(sp: &SignedPacket, cas: Option<u64>) -> bool {
        let item = match packet_to_item(sp) {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!(error = %e, "cannot convert the record for the DHT");
                return false;
            }
        };
        let dht = match Dht::client() {
            Ok(d) => d,
            Err(e) => {
                tracing::debug!(error = %e, "DHT client failed to start");
                return false;
            }
        };
        let cas = cas.and_then(|c| i64::try_from(c).ok());
        let put = async {
            match dht.put_mutable(item.clone(), cas).await {
                Ok(_) => true,
                Err(PutMutableError::Concurrency(ConcurrencyError::CasFailed)) if cas.is_some() => {
                    // Some node stores a different seq than the one we read; ours is newer anyway.
                    dht.put_mutable(item, None).await.is_ok()
                }
                Err(e) => {
                    tracing::debug!(error = %e, "DHT put failed");
                    false
                }
            }
        };
        tokio::time::timeout(DHT_PUT_BUDGET, put).await.unwrap_or(false)
    }
}

/// Tell `server`'s session about the peers a record names (dial them; a host that is not us
/// leads to the merge comparison).
fn wake_from_record(net: &Inner, server: &ServerId, rec: &RdvRecord) {
    if rec.host != net.me
        && let Some(relay) = &rec.host_relay
    {
        net.learn_hint(&rec.host, &AddrHint { relay: Some(relay.clone()), direct: Vec::new() });
    }
    let Some(tx) = net.sessions.lock().get(server).map(|s| s.tx.clone()) else { return };
    for user in std::iter::once(&rec.host).chain(rec.online.iter()) {
        if *user != net.me {
            let _ = tx.try_send(SessionCmd::PeerSeen { user: *user });
        }
    }
}

/// Cold open: resolve the record once and dial what it names.
pub(crate) fn spawn_cold_resolve(net: &Arc<Inner>, server: ServerId, keys: ServerKeys) {
    let Some(rdv) = net.rdv.clone() else { return };
    let weak = Arc::downgrade(net);
    net.tasks.spawn(async move {
        let found = rdv.resolve_first(&keys.rdv_pk(), &keys.rdv_read_key).await;
        let Some(net) = weak.upgrade() else { return };
        match found {
            Some(rec) => {
                tracing::debug!(server = %server.short(), host = %rec.host.short(), epoch = rec.epoch, "rendezvous record found");
                wake_from_record(&net, &server, &rec);
            }
            None => tracing::debug!(server = %server.short(), "no rendezvous record found"),
        }
    });
}

/// The host role's publisher: publish on becoming host and on roster changes, refresh, and
/// re-resolve to find a foreign host. Runs until aborted (the host role ends).
pub(crate) fn spawn_host_runner(net: &Arc<Inner>, server: ServerId, keys: ServerKeys, shared: Arc<SessionShared>) -> Option<tokio::task::AbortHandle> {
    let rdv = net.rdv.clone()?;
    let me = net.me;
    Some(net.tasks.spawn(host_runner(Arc::downgrade(net), rdv, server, me, keys, shared)))
}

/// What a publish carries apart from the timestamp (republish only when it changed).
type Content = (u64, Option<String>, Vec<UserId>);

/// Publisher bookkeeping of one hosted server.
#[derive(Debug, Default)]
struct Schedule {
    publish_at: Option<Instant>,
    /// Publish despite a foreign record at this time (clobber rule grace).
    force_at: Option<Instant>,
    last_publish: Option<Instant>,
    last_relay: Option<Instant>,
    last_dht: Option<Instant>,
    last_content: Option<Content>,
    solo_since: Option<Instant>,
}

impl Schedule {
    /// A roster changed: publish after the debounce, at least `PUBLISH_MIN_GAP` after the last.
    fn kick(&mut self, now: Instant) {
        if self.publish_at.is_none() && self.last_publish.is_some() {
            let gap = self.last_publish.map_or(now, |t| t + PUBLISH_MIN_GAP);
            self.publish_at = Some((now + PUBLISH_DEBOUNCE).max(gap));
        }
    }

    /// Re-resolve interval for the current role.
    fn interval(&mut self, now: Instant, solo: bool) -> Duration {
        if solo {
            let since = *self.solo_since.get_or_insert(now);
            if now.duration_since(since) < SOLO_BACKOFF_AFTER {
                return RESOLVE_SOLO;
            }
        } else {
            self.solo_since = None;
        }
        RESOLVE_HOSTING
    }
}

async fn host_runner(net: Weak<Inner>, rdv: Arc<Rendezvous>, server: ServerId, me: UserId, keys: ServerKeys, shared: Arc<SessionShared>) {
    let start = Instant::now();
    let mut sch = Schedule { publish_at: Some(start + INITIAL_PUBLISH), solo_since: Some(start), ..Schedule::default() };
    let mut tick_at = start + RESOLVE_SOLO;
    loop {
        let wake = [sch.publish_at, sch.force_at].into_iter().flatten().fold(tick_at, Instant::min);
        tokio::select! {
            _ = shared.rdv_kick.notified() => {
                sch.kick(Instant::now());
                continue;
            }
            _ = tokio::time::sleep_until(wake) => {}
        }
        let now = Instant::now();
        let Some(inner) = net.upgrade() else { return };
        let info = shared.info.lock().clone();
        let roster = shared.roster.lock().clone();
        let relay_url = inner.my_hint().relay;
        drop(inner);
        let interval = sch.interval(now, info.state == ConnState::Solo);
        let draft = record_from_roster(roster.as_ref(), info.epoch, me, relay_url, now_ms());
        let content: Content = (draft.epoch, draft.host_relay.clone(), draft.online.clone());
        let changed = sch.last_content.as_ref() != Some(&content);
        let pending = sch.publish_at.is_some_and(|t| t <= now);
        let forced = sch.force_at.is_some_and(|t| t <= now);
        let relay_due = rdv.has_relay() && sch.last_relay.is_none_or(|t| now.duration_since(t) >= RELAY_REFRESH);
        let dht_due = sch.last_dht.is_none_or(|t| now.duration_since(t) >= DHT_REFRESH);
        if pending {
            sch.publish_at = None;
        }
        if forced {
            sch.force_at = None;
        }
        if forced || (pending && changed) || (tick_at <= now && (relay_due || dht_due)) {
            match rdv.publish(&keys, &draft, forced || changed || dht_due, forced).await {
                Published::Done { relay, dht } => {
                    let t = Instant::now();
                    sch.last_publish = Some(t);
                    sch.last_content = Some(content);
                    if relay {
                        sch.last_relay = Some(t);
                    }
                    if dht {
                        sch.last_dht = Some(t);
                    }
                    tracing::debug!(server = %server.short(), relay, dht, "rendezvous record published");
                }
                Published::Foreign(other) => {
                    tracing::info!(server = %server.short(), host = %other.host.short(), epoch = other.epoch, "rendezvous names another host");
                    if let Some(inner) = net.upgrade() {
                        wake_from_record(&inner, &server, &other);
                    }
                    // Still hosting after the grace (this task ends with the host role):
                    // that host was unreachable or lost the merge.
                    sch.force_at = Some(Instant::now() + FOREIGN_GRACE);
                }
                Published::Failed => {
                    tracing::debug!(server = %server.short(), "rendezvous publish failed; retrying later");
                    sch.publish_at = Some(Instant::now() + PUBLISH_RETRY);
                }
            }
            tick_at = Instant::now() + interval;
        } else if tick_at <= now {
            let latest = rdv.resolve_latest(&keys.rdv_pk(), &keys.rdv_read_key).await;
            if let Some((_, other)) = latest.record
                && other.host != me
                && record_fresh(&other, now_ms())
                && let Some(inner) = net.upgrade()
            {
                wake_from_record(&inner, &server, &other);
            }
            tick_at = Instant::now() + interval;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tincan_core::keys::Identity;
    use tincan_core::wire::RosterEntry;

    fn keys() -> ServerKeys { ServerKeys::derive(&[3u8; 32], &ServerId::from_bytes([4; 32])) }

    fn record(n: usize) -> RdvRecord {
        RdvRecord {
            v: PROTOCOL_VERSION,
            epoch: 9,
            host: Identity::generate().user(),
            host_relay: Some("https://euw1-1.relay.n0.iroh-canary.iroh.link./".into()),
            published_ms: 1_790_000_000_000,
            online: (0..n).map(|_| Identity::generate().user()).collect(),
        }
    }

    #[test]
    fn packet_round_trip_with_explicit_seq() {
        let k = keys();
        let rec = record(MAX_ONLINE_IN_RECORD);
        let seq = 1_900_000_000_000_000u64;
        let sp = record_packet(&k.rdv_sk, &k.rdv_read_key, &rec, seq).unwrap();
        assert_eq!(sp.timestamp().as_micros(), seq);
        assert!(sp.as_bytes().len() <= SignedPacket::MAX_BYTES, "{} bytes", sp.as_bytes().len());
        // The wire bytes verify on their own.
        let back = SignedPacket::from_bytes(sp.as_bytes()).unwrap();
        assert_eq!(open_packet(&back, &k.rdv_pk(), &k.rdv_read_key).unwrap(), rec);
        // Relay payload round trip.
        let relayed = SignedPacket::from_relay_payload(&k.rdv_pk(), &sp.to_relay_payload()).unwrap();
        assert_eq!(open_packet(&relayed, &k.rdv_pk(), &k.rdv_read_key).unwrap(), rec);
    }

    #[test]
    fn dht_item_conversion_keeps_signature() {
        let k = keys();
        let rec = record(3);
        let sp = record_packet(&k.rdv_sk, &k.rdv_read_key, &rec, 42_000_000).unwrap();
        let item = packet_to_item(&sp).unwrap();
        assert_eq!(item.seq(), 42_000_000);
        assert_eq!(item.key(), k.rdv_pk().as_bytes());
        assert!(item.value().len() <= 1000, "BEP44 value limit");
        let again = item_to_packet(&item).unwrap();
        assert_eq!(again.as_bytes(), sp.as_bytes());
        // A tampered value fails verification.
        let mut v = item.value().to_vec();
        let last = v.len() - 1;
        v[last] ^= 1;
        let bad = MutableItem::new_signed_unchecked(*item.key(), *item.signature(), &v, item.seq(), None);
        assert!(item_to_packet(&bad).is_err());
        // So does a different seq under the same signature.
        let reseq = MutableItem::new_signed_unchecked(*item.key(), *item.signature(), item.value(), item.seq() + 1, None);
        assert!(item_to_packet(&reseq).is_err());
    }

    #[test]
    fn open_rejects_other_keys() {
        let k = keys();
        let other = ServerKeys::derive(&[5u8; 32], &ServerId::from_bytes([4; 32]));
        let rec = record(2);
        let sp = record_packet(&k.rdv_sk, &k.rdv_read_key, &rec, 1).unwrap();
        assert!(matches!(open_packet(&sp, &other.rdv_pk(), &k.rdv_read_key), Err(RdvError::WrongKey)));
        assert!(matches!(open_packet(&sp, &k.rdv_pk(), &other.rdv_read_key), Err(RdvError::Record(_))));
        // A packet without our TXT name.
        let plain = SignedPacket::from_txt_strings(&k.rdv_sk, "_other", ["0=abc"], 30).unwrap();
        assert!(open_packet(&plain, &k.rdv_pk(), &k.rdv_read_key).is_err());
    }

    #[test]
    fn newer_seq_supersedes() {
        let k = keys();
        let rec = record(1);
        let a = record_packet(&k.rdv_sk, &k.rdv_read_key, &rec, 100).unwrap();
        let b = record_packet(&k.rdv_sk, &k.rdv_read_key, &rec, 101).unwrap();
        assert!(b.more_recent_than(&a));
        assert!(!a.more_recent_than(&b));
    }

    #[test]
    fn schedule_debounces_roster_changes() {
        let t0 = Instant::now();
        let mut s = Schedule::default();
        // Before the first publish, roster changes are covered by the initial publish.
        s.kick(t0);
        assert!(s.publish_at.is_none());
        s.last_publish = Some(t0);
        s.kick(t0 + Duration::from_secs(1));
        assert_eq!(s.publish_at, Some(t0 + Duration::from_secs(31)));
        // Further changes do not push it out.
        s.kick(t0 + Duration::from_secs(20));
        assert_eq!(s.publish_at, Some(t0 + Duration::from_secs(31)));
        // Solo re-resolve every minute, backing off after 30 minutes alone.
        assert_eq!(s.interval(t0, true), RESOLVE_SOLO);
        assert_eq!(s.interval(t0 + SOLO_BACKOFF_AFTER, true), RESOLVE_HOSTING);
        assert_eq!(s.interval(t0 + SOLO_BACKOFF_AFTER, false), RESOLVE_HOSTING);
        assert_eq!(s.interval(t0 + SOLO_BACKOFF_AFTER, true), RESOLVE_SOLO, "solo time restarts");
    }

    #[test]
    fn seq_rule() {
        assert_eq!(next_seq(1000, None), 1000);
        assert_eq!(next_seq(1000, Some(10)), 1000);
        // A writer with a fast clock: we go one above it.
        assert_eq!(next_seq(1000, Some(5000)), 5001);
        assert_eq!(next_seq(1000, Some(u64::MAX)), u64::MAX);
    }

    #[test]
    fn latest_tracks_max_seq_and_dht_seq() {
        let k = keys();
        let foreign = ServerKeys::derive(&[6u8; 32], &ServerId::from_bytes([4; 32]));
        let r1 = record(1);
        let r2 = record(2);
        let old = record_packet(&k.rdv_sk, &k.rdv_read_key, &r1, 10).unwrap();
        let new = record_packet(&k.rdv_sk, &k.rdv_read_key, &r2, 20).unwrap();
        // Signed with our key but sealed under another read key (a removed member).
        let unreadable = record_packet(&k.rdv_sk, &foreign.rdv_read_key, &r1, 30).unwrap();
        // Signed by someone else entirely.
        let other_signer = record_packet(&foreign.rdv_sk, &k.rdv_read_key, &r1, 99).unwrap();
        let mut l = Latest::default();
        l.note(&old, &k.rdv_pk(), &k.rdv_read_key, false);
        l.note(&new, &k.rdv_pk(), &k.rdv_read_key, true);
        l.note(&unreadable, &k.rdv_pk(), &k.rdv_read_key, true);
        l.note(&other_signer, &k.rdv_pk(), &k.rdv_read_key, true);
        assert_eq!(l.max_seq, Some(30));
        assert_eq!(l.dht_seq, Some(30));
        assert_eq!(l.record, Some((20, r2)));
    }

    #[test]
    fn far_future_packets_and_records_cannot_pin_the_record() {
        let k = keys();
        let now_us = 1_800_000_000_000_000u64;
        let now = now_us / 1000;
        let honest = RdvRecord { published_ms: now - 1000, ..record(1) };
        let ok = record_packet(&k.rdv_sk, &k.rdv_read_key, &honest, now_us - 1_000_000).unwrap();
        // A former member pins seq near the i64 limit, and one claims "published in 2100".
        let pinned = record_packet(&k.rdv_sk, &k.rdv_read_key, &RdvRecord { published_ms: now, ..record(1) }, i64::MAX as u64 - 10).unwrap();
        let eternal = RdvRecord { published_ms: u64::MAX, ..record(1) };
        let eternal_pkt = record_packet(&k.rdv_sk, &k.rdv_read_key, &eternal, now_us).unwrap();
        let mut l = Latest::default();
        l.note_at(&ok, &k.rdv_pk(), &k.rdv_read_key, true, now_us, now);
        l.note_at(&pinned, &k.rdv_pk(), &k.rdv_read_key, true, now_us, now);
        l.note_at(&eternal_pkt, &k.rdv_pk(), &k.rdv_read_key, true, now_us, now);
        assert_eq!(l.max_seq, Some(now_us - 1_000_000), "the far-future seq is not counted");
        assert_eq!(l.dht_seq, Some(now_us - 1_000_000));
        assert_eq!(l.record.as_ref().map(|(_, r)| r.published_ms), Some(honest.published_ms));
        // The next honest publish stays in range.
        let next = next_seq(now_us, l.max_seq);
        assert!(record_packet(&k.rdv_sk, &k.rdv_read_key, &honest, next).is_ok());
        // Freshness: a record from the future is never "fresh forever".
        assert!(!record_fresh(&eternal, now));
        assert!(record_fresh(&RdvRecord { published_ms: now + 60_000, ..record(1) }, now), "small clock skew is fine");
        assert!(record_fresh(&honest, now));
        assert!(!record_fresh(&RdvRecord { published_ms: now - FOREIGN_FRESH_MS, ..record(1) }, now));
    }

    #[test]
    fn record_lists_succession_first_without_the_host() {
        let me = Identity::generate().user();
        let users: Vec<UserId> = (0..20).map(|_| Identity::generate().user()).collect();
        let entry = |u: UserId| RosterEntry {
            user: u,
            short_id: 1,
            addr: AddrHint::default(),
            since_ms: 0,
            voice: None,
            muted: false,
            deafened: false,
            sharing: None,
            direct: true,
            rtt_ms: 1,
        };
        let mut online = vec![entry(me)];
        online.extend(users.iter().copied().map(entry));
        let roster = Roster { epoch: 4, rev: 1, host: me, online, succession: vec![users[5], users[2]] };
        let rec = record_from_roster(Some(&roster), 4, me, Some("https://r/".into()), 7);
        assert_eq!(rec.online.len(), MAX_ONLINE_IN_RECORD);
        assert_eq!(&rec.online[..2], &[users[5], users[2]]);
        assert!(!rec.online.contains(&me));
        assert_eq!(rec.host, me);
        assert_eq!(rec.epoch, 4);
        // It still fits a packet.
        let k = keys();
        assert!(record_packet(&k.rdv_sk, &k.rdv_read_key, &rec, 1).is_ok());
        let solo = record_from_roster(None, 1, me, Some("x".repeat(MAX_RELAY_URL_CHARS + 1)), 7);
        assert!(solo.online.is_empty() && solo.host_relay.is_none());
    }

    /// Live, end to end: a new host publishes its record (host runner), and a joiner whose
    /// invite lists nobody finds the host through the record alone (join resolve, dial by
    /// relay URL). Run with `cargo test -p tincan-net -- --ignored rendezvous_live`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "uses n0 relays, the n0 pkarr relay and mainline DHT"]
    async fn rendezvous_live_join_through_the_record() {
        use tincan_core::invite::Invite;
        use tincan_core::storage::MemStorage;
        async fn start(name: &str) -> crate::Net {
            let id = Identity::generate();
            let c = crate::NetConfig { mdns: false, display_name: name.into(), ..crate::NetConfig::default() };
            let ep = crate::build_endpoint(&id, &c).await.unwrap();
            crate::Net::start(ep, Arc::new(MemStorage::new()), id, c, |b| b).await.unwrap()
        }
        let a = start("alice").await;
        let server = a.create_server("Live").await.unwrap();
        let code = a.create_invite(server).await.unwrap();
        let keys = a.inner().replica(&server).and_then(|r| r.lock().keys.clone()).unwrap();
        let probe = Rendezvous::new(a.endpoint(), true);
        let t = Instant::now();
        loop {
            if probe.resolve_first(&keys.rdv_pk(), &keys.rdv_read_key).await.is_some_and(|r| r.host == a.me()) {
                break;
            }
            assert!(t.elapsed() < Duration::from_secs(60), "the host's record never appeared");
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        eprintln!("host record resolvable {:?} after create_server", t.elapsed());
        let mut inv = Invite::decode(&code).unwrap();
        inv.bootstrap.clear();
        let b = start("bob").await;
        let t = Instant::now();
        let joined = tokio::time::timeout(Duration::from_secs(30), b.join(&inv.encode())).await;
        eprintln!("join through the record: ok={} in {:?}", matches!(joined, Ok(Ok(_))), t.elapsed());
        assert_eq!(joined.unwrap().unwrap(), server);
        b.shutdown().await;
        a.shutdown().await;
    }

    /// Live: publish a throwaway record to n0's pkarr relay and mainline DHT, then resolve it
    /// both ways. Run with `cargo test -p tincan-net -- --ignored rendezvous_live`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "uses the public n0 pkarr relay and mainline DHT"]
    async fn rendezvous_live_publish_resolve() {
        let ep = iroh::Endpoint::builder(iroh::endpoint::presets::N0).bind().await.unwrap();
        let rdv = Rendezvous::new(&ep, true);
        // Random keys: nothing identifying is published.
        let secret: [u8; 32] = tincan_core::keys::random_bytes();
        let k = ServerKeys::derive(&secret, &ServerId::from_bytes(tincan_core::keys::random_bytes()));
        let rec = record_from_roster(None, 1, Identity::generate().user(), Some("https://example.invalid/".into()), now_ms());
        let t = Instant::now();
        let out = rdv.publish(&k, &rec, true, false).await;
        eprintln!("publish: {out:?} in {:?}", t.elapsed());
        assert!(matches!(out, Published::Done { .. }), "{out:?}");
        let t = Instant::now();
        let relay = rdv.relay_get(&k.rdv_pk()).await;
        eprintln!("relay resolve: found={} in {:?}", relay.is_some(), t.elapsed());
        let t = Instant::now();
        let dht = Rendezvous::dht_get(&k.rdv_pk(), &k.rdv_read_key, true).await;
        eprintln!("dht resolve: items={} in {:?}", dht.len(), t.elapsed());
        let t = Instant::now();
        let first = rdv.resolve_first(&k.rdv_pk(), &k.rdv_read_key).await;
        eprintln!("resolve_first in {:?}", t.elapsed());
        assert_eq!(first.as_ref().map(|r| r.host), Some(rec.host));
        // A second publish must go above the first (write rule).
        let out2 = rdv.publish(&k, &rec, true, false).await;
        eprintln!("second publish: {out2:?}");
        let latest = rdv.resolve_latest(&k.rdv_pk(), &k.rdv_read_key).await;
        eprintln!("latest: max_seq={:?} dht_seq={:?}", latest.max_seq, latest.dht_seq);
        assert!(latest.record.is_some());
        ep.close().await;
    }
}
