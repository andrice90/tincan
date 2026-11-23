//! The event pump: turns `NetEvent`s and `FileEvent`s into UI events.
//!
//! Live chat goes out at once (`message-added` per message). Everything else is coalesced:
//!
//! - Non-message changes mark a conversation dirty; one flush after [`DEBOUNCE`] emits the
//!   changed DTOs (`server-updated`, `channels-updated`, `members-updated`, `dm-updated`),
//!   each only if it differs from the last one sent.
//! - Bulk ingest (an initial history sync, a catch-up after being offline) sends thousands of
//!   `EventAdded` per second. tincan-net does not surface its sync-complete marker, so the
//!   pump detects bulk itself: more than [`BATCH_BULK`] messages of one conversation in one
//!   drained batch, more than [`RATE_BULK`] per second, or a bulk window armed by
//!   `join_server`. While bulk, no per-message events go out (attachment bookkeeping and
//!   auto-fetch still run); after [`QUIET`] without messages one refresh is emitted, plus a
//!   `snapshot` so the UI reloads its message pages.
//! - A lagged receiver is treated as "everything changed": full refresh plus `snapshot`.
//!
//! No timer runs while nothing is pending.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tincan_core::ConvId;
use tincan_core::state::{Change, dm_channel};
use tincan_files::FileEvent;
use tincan_net::{ConnState, NetEvent};
use tokio::sync::{broadcast, mpsc};
use tokio::time::Instant;

use crate::app::Core;
use crate::dto::{self, ToastKind};
use crate::emit::Ev;
use crate::views::Refresh;

/// Debounce of non-message refreshes.
pub(crate) const DEBOUNCE: Duration = Duration::from_millis(100);
/// Messages of one conversation in one drained batch that mean bulk ingest.
pub(crate) const BATCH_BULK: usize = 8;
/// Messages of one conversation per second that mean bulk ingest.
pub(crate) const RATE_BULK: u32 = 40;
/// A bulk window ends after this long without messages.
pub(crate) const QUIET: Duration = Duration::from_millis(400);
/// A window armed by `join_server` lasts at least this long (re-armed when the join
/// returns, since the history sync starts right after the host's `Welcome`).
pub(crate) const ARMED_WAIT: Duration = Duration::from_secs(1);
/// Most events taken from the network receiver per drain.
const MAX_BATCH: usize = 2048;
/// Parked downloads are re-queried at most this often per hash.
pub(crate) const REQUERY_EVERY: Duration = Duration::from_secs(10);

/// Commands from the app to the pump.
#[derive(Debug)]
pub(crate) enum PumpCmd {
    /// Treat this server as bulk until it goes quiet (sent before `Net::join`).
    ArmBulk(ConvId),
}

#[derive(Debug)]
struct Bulk {
    quiet_at: Instant,
    suppressed: usize,
}

struct Pump {
    core: Arc<Core>,
    dirty: HashMap<ConvId, Refresh>,
    dirty_at: Option<Instant>,
    dms_dirty: bool,
    policy_dirty: bool,
    rosters_changed: bool,
    snapshot_due: bool,
    bulk: HashMap<ConvId, Bulk>,
    rate: HashMap<ConvId, (Instant, u32)>,
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
        rosters_changed: false,
        snapshot_due: false,
        bulk: HashMap::new(),
        rate: HashMap::new(),
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
        let bulk = self.bulk.values().map(|b| b.quiet_at).min();
        match (self.dirty_at, bulk) {
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

    fn arm(&mut self, conv: ConvId) {
        let quiet_at = Instant::now() + ARMED_WAIT;
        let b = self.bulk.entry(conv).or_insert(Bulk { quiet_at, suppressed: 0 });
        b.quiet_at = b.quiet_at.max(quiet_at);
    }

    fn enter_bulk(&mut self, conv: ConvId) {
        let quiet_at = Instant::now() + QUIET;
        self.bulk.entry(conv).or_insert_with(|| {
            tracing::debug!(conv = %conv.short(), "bulk ingest: coalescing message events");
            Bulk { quiet_at, suppressed: 0 }
        });
    }

    /// Count a message for the rate check; true when the rate means bulk.
    fn over_rate(&mut self, conv: ConvId) -> bool {
        let now = Instant::now();
        let e = self.rate.entry(conv).or_insert((now, 0));
        if now.duration_since(e.0) >= Duration::from_secs(1) {
            *e = (now, 0);
        }
        e.1 += 1;
        e.1 > RATE_BULK
    }

    fn lagged(&mut self, missed: u64) {
        tracing::warn!(missed, "network events lagged; refreshing everything");
        for conv in self.core.net.convs() {
            self.core.invalidate_view(&conv);
            self.dirty.entry(conv).or_default().merge(Refresh::ALL);
        }
        self.policy_dirty = true;
        self.snapshot_due = true;
        self.schedule();
        self.core.media.on_lagged(&self.core);
    }

    fn on_batch(&mut self, batch: Vec<NetEvent>) {
        let mut counts: HashMap<ConvId, usize> = HashMap::new();
        for ev in &batch {
            if let NetEvent::EventAdded { conv, change: Change::Message { .. }, .. } = ev {
                *counts.entry(*conv).or_default() += 1;
            }
        }
        for (conv, n) in counts {
            if n > BATCH_BULK {
                self.enter_bulk(conv);
            }
        }
        for ev in batch {
            self.on_net(ev);
        }
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
                    if self.over_rate(conv) {
                        self.enter_bulk(conv);
                    }
                    if let Some(b) = self.bulk.get_mut(&conv) {
                        b.suppressed += 1;
                        b.quiet_at = Instant::now() + QUIET;
                    } else {
                        core.emit_message(Ev::MessageAdded, &conv, &event);
                        if event.event.author != core.me {
                            self.mark(conv, Refresh::UNREAD);
                        }
                    }
                    core.auto_fetch(conv, &event);
                }
                Change::Deleted { target } => {
                    if let Some(b) = self.bulk.get_mut(&conv) {
                        b.suppressed += 1;
                        b.quiet_at = Instant::now() + QUIET;
                    } else {
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
                }
                Change::Members => {
                    core.invalidate_view(&conv);
                    self.mark(conv, Refresh { server: true, members: true, ..Refresh::default() });
                    self.dms_dirty = true;
                    if self.bulk.contains_key(&conv) {
                        self.policy_dirty = true;
                    } else {
                        // At once: a new member may fetch an attachment right after joining.
                        core.update_policy();
                    }
                }
                _ => {}
            },
            NetEvent::RosterUpdated { server, .. } => {
                self.mark(server, Refresh { server: true, members: true, ..Refresh::default() });
                self.dms_dirty = true;
                self.rosters_changed = true;
            }
            NetEvent::ConnectionChanged { server, info } => {
                self.mark(server, Refresh { server: true, members: true, ..Refresh::default() });
                if matches!(info.state, ConnState::Member | ConnState::Hosting | ConnState::Solo) {
                    core.on_connected(server);
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
                self.bulk.remove(&server);
                self.rate.remove(&server);
                self.policy_dirty = true;
                self.dms_dirty = true;
                self.snapshot_due = true;
                self.schedule();
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
        let ended: Vec<ConvId> = self.bulk.iter().filter(|(_, b)| b.quiet_at <= now).map(|(c, _)| *c).collect();
        for conv in ended {
            if let Some(b) = self.bulk.remove(&conv) {
                tracing::debug!(conv = %conv.short(), suppressed = b.suppressed, "bulk ingest ended");
                self.rate.remove(&conv);
                self.core.invalidate_view(&conv);
                self.dirty.entry(conv).or_default().merge(Refresh::ALL);
                self.policy_dirty = true;
                if b.suppressed > 0 {
                    self.snapshot_due = true;
                }
                // Flush now rather than after another debounce.
                self.dirty_at = Some(now);
            }
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
            if self.bulk.contains_key(&conv) {
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
