//! The event pump: turns `NetEvent`s and `FileEvent`s into UI events.
//!
//! Live chat goes out at once (`message-added` per message). Everything else is coalesced:
//!
//! - Non-message changes mark a conversation dirty; one flush after [`DEBOUNCE`] emits the
//!   changed DTOs (`server-updated`, `channels-updated`, `members-updated`, `dm-updated`),
//!   each only if it differs from the last one sent.
//! - Bulk ingest (an initial history sync, a catch-up after being offline) sends thousands of
//!   `EventAdded` per second. A conversation goes bulk on more than [`BATCH_BULK`] messages
//!   of it in one drained batch, more than [`RATE_BULK`] per second, or a window armed by
//!   `join_server`. While bulk, no per-message events go out (attachment bookkeeping and
//!   auto-fetch still run). The window ends with one refresh, plus a `snapshot` so the UI
//!   reloads its message pages, on whichever comes first (see [`Bulks`]):
//!   - `NetEvent::SyncComplete` for the conversation;
//!   - [`QUIET`] without burst messages (a message below the rate and batch thresholds does
//!     not hold the window open);
//!   - its maximum length, [`MAX_BULK`] ([`MAX_SYNC_BULK`] while a server's history sync
//!     is running). After a window was cut at its maximum, only a batch over
//!     [`BATCH_BULK`] opens a new one until the rate drops, so one member who keeps
//!     posting fast cannot hold every live message back.
//! - A lagged receiver is treated as "everything changed": full refresh plus `snapshot`.
//!
//! No timer runs while nothing is pending.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use tincan_core::event::Body;
use tincan_core::state::{Change, dm_channel};
use tincan_core::wire::{AddrHint, Roster};
use tincan_core::{ChannelId, ConvId, UserId};
use tincan_files::FileEvent;
use tincan_net::{ConnState, NetEvent};
use tokio::sync::{broadcast, mpsc};
use tokio::time::Instant;

use crate::app::Core;
use crate::dto::{self, ToastKind};
use crate::emit::Ev;
use crate::views::{Refresh, display};

/// Debounce of non-message refreshes.
pub(crate) const DEBOUNCE: Duration = Duration::from_millis(100);
/// Messages of one conversation in one drained batch that mean bulk ingest.
pub(crate) const BATCH_BULK: usize = 8;
/// Messages of one conversation per second that mean bulk ingest.
pub(crate) const RATE_BULK: u32 = 40;
/// A bulk window ends after this long without burst messages.
pub(crate) const QUIET: Duration = Duration::from_millis(400);
/// A window armed by `join_server` lasts at least this long (re-armed when the join
/// returns, since the history sync starts right after the host's `Welcome`).
pub(crate) const ARMED_WAIT: Duration = Duration::from_secs(1);
/// Longest bulk window, unless a server's history sync is running.
pub(crate) const MAX_BULK: Duration = Duration::from_secs(5);
/// Longest bulk window while a server's history sync runs (from the connection to the
/// host until `SyncComplete`). A host that never ends its sync gets this once per
/// connection.
pub(crate) const MAX_SYNC_BULK: Duration = Duration::from_secs(60);
/// Most events taken from the network receiver per drain.
const MAX_BATCH: usize = 2048;
/// Parked downloads are re-queried at most this often per hash.
pub(crate) const REQUERY_EVERY: Duration = Duration::from_secs(10);
/// After a window was cut at its maximum, the rate must stay under [`RATE_BULK`] this long
/// before it can open a window again.
const UNCAP_AFTER: Duration = Duration::from_secs(2);
/// `update_policy` runs at once on a membership change unless it ran this recently; then
/// it waits for the next flush.
const POLICY_EVERY: Duration = Duration::from_millis(100);

/// Commands from the app to the pump.
#[derive(Debug)]
pub(crate) enum PumpCmd {
    /// Treat this server as bulk until it goes quiet (sent before `Net::join`).
    ArmBulk(ConvId),
}

/// One bulk window.
#[derive(Debug)]
pub(crate) struct Bulk {
    /// When it opened.
    started: Instant,
    /// It ends here unless a burst message moves it.
    quiet_at: Instant,
    /// It ends here at the latest.
    cap_at: Instant,
    /// Message events held back.
    pub suppressed: usize,
}

/// The bulk windows of every conversation, driven with explicit times so the rules can be
/// unit-tested.
#[derive(Debug, Default)]
pub(crate) struct Bulks {
    open: HashMap<ConvId, Bulk>,
    /// The last [`RATE_BULK`] + 1 message times per conversation.
    rate: HashMap<ConvId, VecDeque<Instant>>,
    /// Windows cut at their maximum, with the last time the rate was over the threshold:
    /// the rate alone does not reopen them.
    capped: HashMap<ConvId, Instant>,
    /// Servers whose history sync is running (connected to a host, no `SyncComplete` yet).
    syncing: HashSet<ConvId>,
    /// When `SyncComplete` last ended a conversation's sync.
    synced_at: HashMap<ConvId, Instant>,
}

impl Bulks {
    /// Is `conv` bulk right now?
    pub fn contains(&self, conv: &ConvId) -> bool { self.open.contains_key(conv) }

    /// The earliest time a window ends.
    pub fn next_deadline(&self) -> Option<Instant> { self.open.values().map(|b| b.quiet_at.min(b.cap_at)).min() }

    fn cap_len(&self, conv: &ConvId) -> Duration { if self.syncing.contains(conv) { MAX_SYNC_BULK } else { MAX_BULK } }

    fn open(&mut self, conv: ConvId, now: Instant, quiet: Duration) {
        let cap_at = now + self.cap_len(&conv);
        let b = self.open.entry(conv).or_insert_with(|| {
            tracing::debug!(conv = %conv.short(), "bulk ingest: coalescing message events");
            Bulk { started: now, quiet_at: now, cap_at, suppressed: 0 }
        });
        b.quiet_at = b.quiet_at.max(now + quiet);
    }

    /// `join_server` is about to sync `conv`. Ignored right after its sync completed (the
    /// re-arm when `Net::join` returns can come after `SyncComplete`).
    pub fn arm(&mut self, conv: ConvId, now: Instant) {
        if self.synced_at.get(&conv).is_some_and(|t| now.duration_since(*t) < ARMED_WAIT) {
            return;
        }
        self.open(conv, now, ARMED_WAIT);
    }

    /// A server connected to a host: its history sync starts.
    pub fn sync_started(&mut self, conv: ConvId) {
        self.syncing.insert(conv);
        if let Some(b) = self.open.get_mut(&conv) {
            b.cap_at = b.cap_at.max(b.started + MAX_SYNC_BULK);
        }
    }

    /// `conv` is no longer connected to a host as a member: no sync of a host runs, so new
    /// windows get the short maximum again.
    pub fn sync_ended(&mut self, conv: ConvId) { self.syncing.remove(&conv); }

    /// `SyncComplete`: the sync of `conv` is in. Returns the window it ends, if any.
    pub fn sync_complete(&mut self, conv: ConvId, now: Instant) -> Option<Bulk> {
        self.syncing.remove(&conv);
        self.synced_at.insert(conv, now);
        self.rate.remove(&conv);
        self.open.remove(&conv)
    }

    /// Count a message for the rate check (a sliding second); true when the rate means
    /// bulk.
    fn over_rate(&mut self, conv: ConvId, now: Instant) -> bool {
        let q = self.rate.entry(conv).or_default();
        while q.front().is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(1)) {
            q.pop_front();
        }
        q.push_back(now);
        if q.len() > RATE_BULK as usize + 1 {
            q.pop_front();
        }
        let over = q.len() > RATE_BULK as usize;
        // After a window was cut at its maximum, the rate opens windows again only once
        // it stayed under the threshold for [`UNCAP_AFTER`].
        if let Some(last_over) = self.capped.get_mut(&conv) {
            if over {
                *last_over = now;
            } else if now.duration_since(*last_over) >= UNCAP_AFTER {
                self.capped.remove(&conv);
            }
        }
        over
    }

    /// A message (or deletion) event of `conv`, `in_batch` of them in the drained batch.
    /// True when it is held back (the conversation is bulk).
    pub fn on_message(&mut self, conv: ConvId, in_batch: usize, now: Instant) -> bool {
        let over = self.over_rate(conv, now);
        let big_batch = in_batch > BATCH_BULK;
        if !self.open.contains_key(&conv) && (big_batch || (over && !self.capped.contains_key(&conv))) {
            self.open(conv, now, QUIET);
        }
        let Some(b) = self.open.get_mut(&conv) else { return false };
        b.suppressed += 1;
        // Only burst traffic holds the window open: one message every few hundred ms lets
        // it close.
        if big_batch || over {
            b.quiet_at = b.quiet_at.max(now + QUIET);
        }
        true
    }

    /// Windows that end at `now`. A window cut at its maximum is remembered as capped,
    /// and a server sync that ran that long no longer gets the long maximum.
    pub fn due(&mut self, now: Instant) -> Vec<(ConvId, Bulk)> {
        let ended: Vec<ConvId> = self.open.iter().filter(|(_, b)| b.quiet_at.min(b.cap_at) <= now).map(|(c, _)| *c).collect();
        let mut out = Vec::new();
        for conv in ended {
            let Some(b) = self.open.remove(&conv) else { continue };
            if b.cap_at <= now && b.quiet_at > now {
                tracing::debug!(conv = %conv.short(), "bulk window cut at its maximum");
                self.capped.insert(conv, now);
                self.syncing.remove(&conv);
            } else {
                self.rate.remove(&conv);
            }
            out.push((conv, b));
        }
        out
    }

    /// Forget `conv` (left).
    pub fn forget(&mut self, conv: &ConvId) {
        self.open.remove(conv);
        self.rate.remove(conv);
        self.capped.remove(conv);
        self.syncing.remove(conv);
        self.synced_at.remove(conv);
    }
}

/// What the UI shows of a roster: who is online, in which voice room, muted, deafened,
/// sharing, and who hosts. Roster rebroadcasts that change nothing else (RTT buckets,
/// addresses, revisions) are skipped by the pump.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RosterSig {
    host: UserId,
    online: Vec<(UserId, Option<ChannelId>, bool, bool, bool)>,
}

impl RosterSig {
    /// The shown part of `r`.
    pub fn of(r: &Roster) -> Self {
        let mut online: Vec<_> = r.online.iter().map(|e| (e.user, e.voice, e.muted, e.deafened, e.sharing.is_some())).collect();
        online.sort_by_key(|e| e.0);
        Self { host: r.host, online }
    }
}

/// The addresses of the members online in a roster, by user. The UI does not show them,
/// but parked downloads need them: a new address re-arms a provider that failed to dial.
pub(crate) fn addr_sig(r: &Roster) -> Vec<(UserId, AddrHint)> {
    let mut v: Vec<(UserId, AddrHint)> = r.online.iter().map(|e| (e.user, e.addr.clone())).collect();
    v.sort_by_key(|e| e.0);
    v
}

/// What a roster update changed for the pump.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RosterChange {
    /// Something the UI shows: refresh members, the server and the DM list.
    pub ui: bool,
    /// Something parked downloads use (shown state or addresses): re-query them.
    pub requery: bool,
}

/// The last roster and connection state handled per server, so rebroadcasts that change
/// nothing (RTT, revisions) cost nothing.
#[derive(Debug, Default)]
pub(crate) struct Seen {
    rosters: HashMap<ConvId, (RosterSig, Vec<(UserId, AddrHint)>)>,
    conns: HashMap<ConvId, (ConnState, Option<UserId>)>,
}

impl Seen {
    /// A roster of `server` arrived.
    pub fn roster(&mut self, server: ConvId, r: &Roster) -> RosterChange {
        let next = (RosterSig::of(r), addr_sig(r));
        let change = match self.rosters.get(&server) {
            None => RosterChange { ui: true, requery: true },
            Some((sig, addrs)) => {
                let ui = *sig != next.0;
                RosterChange { ui, requery: ui || *addrs != next.1 }
            }
        };
        self.rosters.insert(server, next);
        change
    }

    /// A `ConnectionChanged` of `server`. Returns (the state or host changed, a history
    /// sync starts: a new connection to a host). Net also sends it on RTT and path moves.
    pub fn conn(&mut self, server: ConvId, state: ConnState, host: Option<UserId>) -> (bool, bool) {
        let now = (state, host);
        let changed = self.conns.insert(server, now) != Some(now);
        if changed {
            // Rosters count only while member, hosting or solo: the next one after a
            // reconnect or a new host is always handled.
            self.rosters.remove(&server);
        }
        (changed, changed && state == ConnState::Member)
    }

    /// Events were lost: a missed roster may differ from the last one handled, so rosters
    /// are compared afresh. Connections are kept: the next RTT-only `ConnectionChanged` of
    /// a server is not a new connection (taking it for one would mark a sync that never
    /// ends with `SyncComplete`).
    pub fn lagged(&mut self) { self.rosters.clear(); }

    /// `server` was left.
    pub fn forget(&mut self, server: &ConvId) {
        self.rosters.remove(server);
        self.conns.remove(server);
    }
}

struct Pump {
    core: Arc<Core>,
    dirty: HashMap<ConvId, Refresh>,
    dirty_at: Option<Instant>,
    dms_dirty: bool,
    policy_dirty: bool,
    /// When `update_policy` last ran from a membership change.
    policy_at: Option<Instant>,
    rosters_changed: bool,
    snapshot_due: bool,
    bulks: Bulks,
    /// Message events per conversation in the batch being handled.
    batch_counts: HashMap<ConvId, usize>,
    /// The last roster and connection state handled per server.
    seen: Seen,
}

/// Run until the network's event stream closes (or the task is aborted on shutdown).
pub(crate) async fn run(
    core: Arc<Core>,
    mut net_rx: broadcast::Receiver<NetEvent>,
    mut file_rx: broadcast::Receiver<FileEvent>,
    mut cmd_rx: mpsc::Receiver<PumpCmd>,
) {
    let mut p = Pump {
        core,
        dirty: HashMap::new(),
        dirty_at: None,
        dms_dirty: false,
        policy_dirty: false,
        policy_at: None,
        rosters_changed: false,
        snapshot_due: false,
        bulks: Bulks::default(),
        batch_counts: HashMap::new(),
        seen: Seen::default(),
    };
    let mut files_open = true;
    loop {
        let deadline = p.next_deadline();
        tokio::select! {
            biased;
            cmd = cmd_rx.recv() => match cmd {
                Some(PumpCmd::ArmBulk(conv)) => p.arm(conv),
                None => break,
            },
            ev = net_rx.recv() => {
                let mut batch = Vec::new();
                match ev {
                    Ok(e) => batch.push(e),
                    Err(broadcast::error::RecvError::Lagged(n)) => p.lagged(n),
                    Err(broadcast::error::RecvError::Closed) => break,
                }
                while batch.len() < MAX_BATCH {
                    match net_rx.try_recv() {
                        Ok(e) => batch.push(e),
                        Err(broadcast::error::TryRecvError::Lagged(n)) => p.lagged(n),
                        Err(_) => break,
                    }
                }
                p.on_batch(batch);
            }
            ev = file_rx.recv(), if files_open => match ev {
                Ok(e) => p.on_file(e),
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    // Completions and final states may be among the lost events: re-read them.
                    tracing::warn!(missed = n, "file events lagged; re-reading transfers");
                    p.core.on_files_lagged();
                }
                Err(broadcast::error::RecvError::Closed) => files_open = false,
            },
            _ = sleep_until(deadline), if deadline.is_some() => p.on_timer(),
        }
    }
    tracing::debug!("event pump stopped");
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

impl Pump {
    fn next_deadline(&self) -> Option<Instant> {
        match (self.dirty_at, self.bulks.next_deadline()) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn mark(&mut self, conv: ConvId, what: Refresh) {
        self.dirty.entry(conv).or_default().merge(what);
        self.schedule();
    }

    fn schedule(&mut self) {
        if self.dirty_at.is_none() {
            self.dirty_at = Some(Instant::now() + DEBOUNCE);
        }
    }

    fn arm(&mut self, conv: ConvId) { self.bulks.arm(conv, Instant::now()); }

    fn lagged(&mut self, missed: u64) {
        tracing::warn!(missed, "network events lagged; refreshing everything");
        for conv in self.core.net.convs() {
            self.core.invalidate_view(&conv);
            self.dirty.entry(conv).or_default().merge(Refresh::ALL);
        }
        self.seen.lagged();
        self.policy_dirty = true;
        self.dms_dirty = true;
        self.rosters_changed = true;
        self.snapshot_due = true;
        self.schedule();
        self.core.media.on_lagged(&self.core);
    }

    fn on_batch(&mut self, batch: Vec<NetEvent>) {
        self.batch_counts.clear();
        for ev in &batch {
            if let NetEvent::EventAdded { conv, change: Change::Message { .. } | Change::Deleted { .. }, .. } = ev {
                *self.batch_counts.entry(*conv).or_default() += 1;
            }
        }
        for ev in batch {
            self.on_net(ev);
        }
    }

    /// A bulk window ended: one refresh of everything in it, a snapshot if messages were
    /// held back, flushed now rather than after another debounce.
    fn end_bulk(&mut self, conv: ConvId, b: Bulk, now: Instant) {
        tracing::debug!(conv = %conv.short(), suppressed = b.suppressed, "bulk ingest ended");
        self.core.invalidate_view(&conv);
        self.dirty.entry(conv).or_default().merge(Refresh::ALL);
        self.policy_dirty = true;
        if b.suppressed > 0 {
            self.snapshot_due = true;
        }
        self.dirty_at = Some(now);
    }

    /// Who is in which conversation changed: recompute the access set at once (a new
    /// member may fetch an attachment right after joining), unless it ran within
    /// [`POLICY_EVERY`] or the conversation is bulk; then the next flush does it.
    fn members_changed(&mut self, conv: ConvId) {
        let now = Instant::now();
        if self.bulks.contains(&conv) || self.policy_at.is_some_and(|t| now.duration_since(t) < POLICY_EVERY) {
            self.policy_dirty = true;
            self.schedule();
            return;
        }
        self.policy_at = Some(now);
        self.core.update_policy();
    }

    fn on_net(&mut self, ev: NetEvent) {
        let core = self.core.clone();
        if matches!(ev, NetEvent::RosterUpdated { .. } | NetEvent::ConnectionChanged { .. }) {
            // Voice participants, paths and shares follow rosters and connections too.
            core.media.on_net_event(&core, &ev);
        }
        match ev {
            NetEvent::EventAdded { conv, event, change } => match change {
                Change::Message { .. } => {
                    // A void message (signed by a removed member after the removal) is
                    // stored for seq continuity only: not shown, not fetched.
                    if core.view(&conv).is_some_and(|v| !v.event_valid(&event.event.author, event.event.seq)) {
                        return;
                    }
                    let n = self.batch_counts.get(&conv).copied().unwrap_or(1);
                    if !self.bulks.on_message(conv, n, Instant::now()) {
                        core.emit_message(Ev::MessageAdded, &conv, &event);
                        // My own messages change a DM's last message time (the DM list
                        // order); others' also the unread state.
                        let what = if event.event.author != core.me { Refresh::UNREAD } else { Refresh { dm: true, ..Refresh::default() } };
                        self.mark(conv, what);
                    }
                    core.auto_fetch(conv, &event);
                }
                Change::Members => {
                    // A live removal of someone else (my own comes as `NetEvent::Removed`);
                    // removals replayed by a history sync stay quiet.
                    if let Body::MemberRemove { user, .. } = &event.event.body
                        && *user != core.me
                        && !self.bulks.contains(&conv)
                        && let Some(view) = core.view(&conv)
                    {
                        let who = core.author_name(&view, user);
                        core.toast(ToastKind::Info, format!("{who} was removed from {}.", display(&view.name, "the server")));
                    }
                }
                Change::Deleted { target } => {
                    let n = self.batch_counts.get(&conv).copied().unwrap_or(1);
                    if !self.bulks.on_message(conv, n, Instant::now()) {
                        if let Ok(Some(stored)) = tincan_core::storage::Storage::event(core.storage.as_ref(), &target) {
                            core.emit_message(Ev::MessageUpdated, &conv, &stored);
                        }
                        self.mark(conv, Refresh::UNREAD);
                    }
                }
                _ => {}
            },
            NetEvent::ConvUpdated { conv, change } => match change {
                Change::ServerRenamed => {
                    core.invalidate_view(&conv);
                    self.mark(conv, Refresh { server: true, ..Refresh::default() });
                }
                Change::Channels => {
                    core.invalidate_view(&conv);
                    self.mark(conv, Refresh { server: true, channels: true, ..Refresh::default() });
                    // A deleted voice room takes the people in it out.
                    core.media.on_channels(&core, conv);
                }
                Change::Members => {
                    core.invalidate_view(&conv);
                    self.mark(conv, Refresh { server: true, members: true, ..Refresh::default() });
                    self.dms_dirty = true;
                    self.members_changed(conv);
                }
                _ => {}
            },
            NetEvent::RosterUpdated { server, roster } => {
                // The host rebroadcasts on RTT and address changes the UI does not show; a
                // new address still re-arms parked downloads.
                let change = self.seen.roster(server, &roster);
                if change.requery {
                    self.rosters_changed = true;
                    self.schedule();
                }
                if !change.ui {
                    return;
                }
                self.mark(server, Refresh { server: true, members: true, ..Refresh::default() });
                self.dms_dirty = true;
            }
            NetEvent::ConnectionChanged { server, info } => {
                self.mark(server, Refresh { server: true, members: true, ..Refresh::default() });
                // Also sent on RTT and path changes; only a new state or host matters here.
                let (changed, sync_starts) = self.seen.conn(server, info.state, info.host);
                if changed {
                    self.dms_dirty = true;
                    if sync_starts {
                        self.bulks.sync_started(server);
                    } else if info.state != ConnState::Member {
                        // No longer a member (hosting now, or looking for a host): no sync
                        // of a host is running, whether or not its SyncComplete came.
                        self.bulks.sync_ended(server);
                    }
                }
                if matches!(info.state, ConnState::Member | ConnState::Hosting | ConnState::Solo) {
                    core.on_connected(server);
                }
            }
            NetEvent::DmNameChanged { conv, peer, .. } => {
                core.on_dm_name(peer);
                self.mark(conv, Refresh { dm: true, ..Refresh::default() });
            }
            NetEvent::SyncComplete { conv } => {
                let now = Instant::now();
                if let Some(b) = self.bulks.sync_complete(conv, now) {
                    self.end_bulk(conv, b, now);
                }
            }
            NetEvent::Typing { conv, channel, user } => {
                if user == core.me {
                    return;
                }
                let Some(view) = core.view(&conv) else { return };
                let channel_id = if view.is_dm || channel == dm_channel(&conv) { None } else { Some(channel.to_hex()) };
                let name = core.author_name(&view, &user);
                core.events.emit(Ev::Typing, dto::Typing { conv_id: conv.to_hex(), channel_id, user_id: user.to_z32(), name });
            }
            NetEvent::DmDelivered { conv, up_to_seq } => core.on_dm_delivered(conv, up_to_seq),
            NetEvent::DmOpened { conv, peer } => {
                core.on_dm_opened(conv, peer);
                self.mark(conv, Refresh { dm: true, ..Refresh::default() });
            }
            NetEvent::Joined { server } => {
                core.invalidate_view(&server);
                self.mark(server, Refresh::ALL);
                self.policy_dirty = true;
                self.dms_dirty = true;
                self.snapshot_due = true;
            }
            NetEvent::Left { server } => {
                core.invalidate_view(&server);
                core.forget_sent(&server);
                self.dirty.remove(&server);
                self.bulks.forget(&server);
                self.seen.forget(&server);
                self.policy_dirty = true;
                self.dms_dirty = true;
                self.snapshot_due = true;
                self.schedule();
            }
            NetEvent::Removed { server } => {
                core.media.on_removed(&core, server);
                core.invalidate_view(&server);
                self.mark(server, Refresh::ALL);
                self.policy_dirty = true;
                self.snapshot_due = true;
                self.schedule();
                if let Some(view) = core.view(&server) {
                    core.toast(ToastKind::Info, format!("You were removed from {}.", display(&view.name, "a server")));
                }
            }
            NetEvent::Error { message, .. } => core.toast(ToastKind::Error, message),
            NetEvent::DmLinkUp { peer } => {
                self.mark(ConvId::dm(&core.me, &peer), Refresh { dm: true, ..Refresh::default() });
                // A DM-only profile has no server that connects: resume downloads here too,
                // and retry parked DM attachments now that the contact is reachable.
                core.resume_downloads();
                self.rosters_changed = true;
            }
            NetEvent::DmLinkDown { peer } => {
                self.mark(ConvId::dm(&core.me, &peer), Refresh { dm: true, ..Refresh::default() });
            }
            NetEvent::MemberLinkUp { .. } | NetEvent::MemberLinkDown { .. } => {}
            other => core.media.on_net_event(&core, &other),
        }
    }

    fn on_file(&mut self, ev: FileEvent) {
        let core = self.core.clone();
        if core.test_files_paused.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        match &ev {
            FileEvent::Transfer(info) => core.on_transfer(info),
            // Share by nomination and re-announce (only what is shared with a server).
            FileEvent::Completed { hash, from, .. } => core.on_completed(*hash, *from),
        }
        core.media.on_file_event(&core, &ev);
    }

    fn on_timer(&mut self) {
        let now = Instant::now();
        for (conv, b) in self.bulks.due(now) {
            self.end_bulk(conv, b, now);
        }
        if self.dirty_at.is_some_and(|t| t <= now) {
            self.flush();
        }
    }

    fn flush(&mut self) {
        self.dirty_at = None;
        let core = self.core.clone();
        let mut deferred = HashMap::new();
        for (conv, what) in std::mem::take(&mut self.dirty) {
            if self.bulks.contains(&conv) {
                deferred.insert(conv, what);
                continue;
            }
            core.refresh(&conv, what);
        }
        self.dirty = deferred;
        if self.policy_dirty {
            self.policy_dirty = false;
            core.update_policy();
        }
        if self.dms_dirty {
            self.dms_dirty = false;
            core.refresh_dms();
        }
        if self.rosters_changed {
            self.rosters_changed = false;
            core.requery_parked();
        }
        if self.snapshot_due {
            self.snapshot_due = false;
            core.emit_snapshot();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tincan_core::wire::{AddrHint, RosterEntry};

    fn ms(n: u64) -> Duration { Duration::from_millis(n) }

    fn conv() -> ConvId { ConvId([7; 32]) }

    /// Feed one message per call at `t` (a batch of one) and say whether it was held back.
    fn feed(b: &mut Bulks, t: Instant) -> bool { b.on_message(conv(), 1, t) }

    /// APP-CORE-05: 41 messages in a second open a window; after that, one message every
    /// 300 ms lets it close, and the next messages go out live.
    #[test]
    fn slow_traffic_after_a_burst_closes_the_window() {
        let mut b = Bulks::default();
        let t0 = Instant::now();
        for i in 0..40 {
            assert!(!feed(&mut b, t0 + ms(i)), "under the rate is live");
        }
        assert!(feed(&mut b, t0 + ms(40)), "the 41st in a second opens a window");
        let mut t = t0 + ms(40);
        let mut live_at = None;
        for _ in 0..20 {
            t += ms(300);
            assert!(b.due(t - ms(1)).len() <= 1);
            if !b.contains(&conv()) {
                live_at.get_or_insert(t);
            }
            let held = feed(&mut b, t);
            if live_at.is_some() {
                assert!(!held, "once closed, slow messages stay live");
            }
        }
        let live_at = live_at.expect("the window closed");
        assert!(live_at <= t0 + ms(2000), "closed {:?} after the burst", live_at - t0);
    }

    /// APP-CORE-05: a sustained flood (60 a second, one per drain) is cut at the maximum
    /// length, and the rate alone does not reopen it while the flood goes on.
    #[test]
    fn a_flood_is_cut_at_the_maximum_and_stays_live() {
        let mut b = Bulks::default();
        let t0 = Instant::now();
        let mut ended_at = None;
        let mut held_after = 0;
        for i in 0..(60 * 12) {
            let t = t0 + ms(i * 1000 / 60);
            if !b.due(t).is_empty() {
                ended_at.get_or_insert(t);
            }
            if feed(&mut b, t) && ended_at.is_some() {
                held_after += 1;
            }
        }
        let ended_at = ended_at.expect("the window was cut");
        assert!(ended_at - t0 <= MAX_BULK + ms(1100), "cut after {:?}", ended_at - t0);
        assert_eq!(held_after, 0, "after the cut the flood's messages go out live");
        // A big batch still opens a window (a real catch-up).
        let t = t0 + ms(12_100);
        assert!(b.on_message(conv(), BATCH_BULK + 1, t));
        // Once the rate drops for a second, the rate opens windows again.
        let mut b = Bulks::default();
        for i in 0..41 {
            feed(&mut b, t0 + ms(i));
        }
        let cut = t0 + MAX_BULK + ms(10);
        assert!(!b.due(cut).is_empty());
        let later = cut + ms(2500);
        for i in 0..40 {
            assert!(!feed(&mut b, later + ms(i)));
        }
        assert!(feed(&mut b, later + ms(41)), "a new burst after a quiet second opens a window");
    }

    /// Q1: `SyncComplete` ends the window at once, and the re-arm when `Net::join`
    /// returns does not reopen it.
    #[test]
    fn sync_complete_ends_the_window() {
        let mut b = Bulks::default();
        let t0 = Instant::now();
        b.arm(conv(), t0);
        b.sync_started(conv());
        assert!(b.on_message(conv(), 50, t0 + ms(100)));
        let ended = b.sync_complete(conv(), t0 + ms(150)).expect("a window was open");
        assert_eq!(ended.suppressed, 1);
        assert!(!b.contains(&conv()));
        b.arm(conv(), t0 + ms(200));
        assert!(!b.contains(&conv()), "a re-arm right after SyncComplete is ignored");
        assert!(!feed(&mut b, t0 + ms(210)), "live right after the sync");
        // Later joins arm again.
        b.arm(conv(), t0 + ms(5000));
        assert!(b.contains(&conv()));
    }

    /// A server's history sync may run past [`MAX_BULK`] without flapping; without
    /// `SyncComplete` it still ends at [`MAX_SYNC_BULK`].
    #[test]
    fn a_running_sync_gets_the_long_maximum() {
        let mut b = Bulks::default();
        let t0 = Instant::now();
        b.sync_started(conv());
        let mut t = t0;
        while t < t0 + MAX_BULK + ms(3000) {
            b.on_message(conv(), 100, t);
            assert!(b.due(t).is_empty(), "no cut during a long sync at {:?}", t - t0);
            t += ms(50);
        }
        while b.due(t).is_empty() {
            b.on_message(conv(), 100, t);
            t += ms(50);
            assert!(t < t0 + MAX_SYNC_BULK + ms(100), "the long maximum still applies");
        }
        // A withheld end of sync gets the long maximum only once.
        b.on_message(conv(), 100, t);
        assert!(!b.due(t + MAX_BULK).is_empty());
    }

    /// A window armed by `join_server` lasts [`ARMED_WAIT`], then closes.
    #[test]
    fn an_armed_window_lasts_and_then_closes() {
        let mut b = Bulks::default();
        let t0 = Instant::now();
        b.arm(conv(), t0);
        assert!(feed(&mut b, t0 + ms(100)));
        assert!(b.due(t0 + ms(900)).is_empty());
        assert_eq!(b.due(t0 + ARMED_WAIT).len(), 1);
        assert!(!feed(&mut b, t0 + ARMED_WAIT + ms(1)));
    }

    fn entry(user: UserId, rtt: u16) -> RosterEntry {
        RosterEntry { user, short_id: 1, addr: AddrHint::default(), since_ms: 1, voice: None, muted: false, deafened: false, sharing: None, direct: true, rtt_ms: rtt }
    }

    /// RESOURCES-9: an RTT or revision change is the same roster for the UI; a mute is not.
    #[test]
    fn roster_signature_ignores_what_the_ui_does_not_show() {
        let (a, b) = (tincan_core::keys::Identity::generate().user(), tincan_core::keys::Identity::generate().user());
        let r = Roster { epoch: 1, rev: 1, host: a, online: vec![entry(a, 10), entry(b, 20)], succession: vec![b] };
        let mut moved = r.clone();
        moved.rev = 2;
        moved.online[1].rtt_ms = 90;
        moved.online.reverse();
        assert_eq!(RosterSig::of(&r), RosterSig::of(&moved));
        let mut muted = r.clone();
        muted.online[1].muted = true;
        assert_ne!(RosterSig::of(&r), RosterSig::of(&muted));
        let mut gone = r.clone();
        gone.online.pop();
        assert_ne!(RosterSig::of(&r), RosterSig::of(&gone));
    }

    /// REGRESSION-1: a roster that changes only an address refreshes nothing on screen but
    /// still re-queries parked downloads (the new address re-arms a provider that failed).
    #[test]
    fn an_address_change_requeries_without_a_refresh() {
        let (a, b) = (tincan_core::keys::Identity::generate().user(), tincan_core::keys::Identity::generate().user());
        let r = Roster { epoch: 1, rev: 1, host: a, online: vec![entry(a, 10), entry(b, 20)], succession: vec![b] };
        let mut seen = Seen::default();
        assert_eq!(seen.roster(conv(), &r), RosterChange { ui: true, requery: true }, "the first one");
        let mut rtt = r.clone();
        rtt.rev = 2;
        rtt.online[1].rtt_ms = 90;
        assert_eq!(seen.roster(conv(), &rtt), RosterChange { ui: false, requery: false }, "RTT and revision only");
        let mut moved = rtt.clone();
        moved.online[1].addr.direct = vec!["192.0.2.7:4433".parse().unwrap()];
        assert_eq!(seen.roster(conv(), &moved), RosterChange { ui: false, requery: true }, "a new address");
        let mut muted = moved.clone();
        muted.online[1].muted = true;
        assert_eq!(seen.roster(conv(), &muted), RosterChange { ui: true, requery: true }, "a mute");
        // After a lag, rosters are compared afresh.
        seen.lagged();
        assert!(seen.roster(conv(), &muted).ui);
    }

    /// REGRESSION-2: after a lag, an RTT-only connection change of a member is not a new
    /// connection (no sync that would never end); a new host still is.
    #[test]
    fn a_lag_does_not_turn_an_rtt_change_into_a_new_sync() {
        let (h1, h2) = (tincan_core::keys::Identity::generate().user(), tincan_core::keys::Identity::generate().user());
        let mut seen = Seen::default();
        assert_eq!(seen.conn(conv(), ConnState::Member, Some(h1)), (true, true));
        assert_eq!(seen.conn(conv(), ConnState::Member, Some(h1)), (false, false), "an RTT move");
        seen.lagged();
        assert_eq!(seen.conn(conv(), ConnState::Member, Some(h1)), (false, false), "an RTT move after a lag");
        assert_eq!(seen.conn(conv(), ConnState::Member, Some(h2)), (true, true), "a new host");
        assert_eq!(seen.conn(conv(), ConnState::Hosting, Some(h2)), (true, false), "hosting: no sync of a host");
    }

    /// REGRESSION-2: leaving the member state ends the sync's long maximum.
    #[test]
    fn a_sync_that_ends_without_sync_complete_loses_the_long_maximum() {
        let mut b = Bulks::default();
        let t0 = Instant::now();
        b.sync_started(conv());
        b.sync_ended(conv());
        b.on_message(conv(), 100, t0);
        assert!(!b.due(t0 + MAX_BULK).is_empty(), "the short maximum");
    }
}
