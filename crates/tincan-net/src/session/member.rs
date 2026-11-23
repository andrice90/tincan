//! Member role: connected to the host of a server.

use tincan_core::election::SignedHostClaim;
use tincan_core::state::Verdict;
use tincan_core::wire::{AddrHint, Ctl, Roster};
use tincan_core::{UserId, now_ms};
use tokio::time::Instant;

use super::{HostStart, Next, Role, SeekKind, Session};
use crate::addr::PathSnapshot;
use crate::error::NetError;
use crate::events::NetEvent;
use crate::hooks::{CtlSource, is_extra};
use crate::link::{Link, LinkEvent};
use crate::media::MemberMedia;

/// State while connected to a host.
pub(crate) struct MemberState {
    /// Control link to the host.
    pub link: Link,
    /// The host.
    pub host: UserId,
    /// The host's claim (given out in Redirects).
    pub claim: SignedHostClaim,
    /// Latest roster from the host.
    pub roster: Roster,
    /// Host's view of my next seq.
    pub next_seq_hint: u64,
    /// The host's sync of what I lacked finished.
    pub synced: bool,
    /// My sync of what the host lacked finished (publishes wait for it).
    pub sync_out_done: bool,
    /// Last frame from the host.
    pub last_rx: Instant,
    /// Suspected dead already (logged once).
    pub suspected: bool,
    /// Last time I sent SyncHave because of a digest mismatch.
    pub last_resync: Option<Instant>,
    /// Path to the host.
    pub path: PathSnapshot,
    /// Media rides this link while the state lives (dropped with it).
    _media: MemberMedia,
}

impl MemberState {
    /// Fresh state after Welcome.
    pub fn new(link: Link, claim: SignedHostClaim, roster: Roster, next_seq: u64, path: PathSnapshot, media: MemberMedia) -> Self {
        Self {
            host: claim.claim.host,
            link,
            claim,
            roster,
            next_seq_hint: next_seq,
            synced: false,
            sync_out_done: false,
            last_rx: Instant::now(),
            suspected: false,
            last_resync: None,
            path,
            _media: media,
        }
    }
}

/// The host is gone: walk the succession list.
fn host_lost(s: &mut Session, why: &str) -> Next {
    let Role::Member(m) = &s.role else { return Next::Stay };
    tracing::info!(server = %s.ctx.server.short(), host = %m.host.short(), why, "lost the host");
    let failed_host = m.host;
    let failed_epoch = m.claim.claim.epoch;
    let succession: Vec<UserId> = m.roster.succession.iter().copied().filter(|u| *u != failed_host).collect();
    let rank = succession.iter().position(|u| *u == s.ctx.me);
    Next::Seek(SeekKind::Failover { failed_host, failed_epoch, succession, rank }, None)
}

pub(super) fn on_link(s: &mut Session, ev: LinkEvent) -> Next {
    let Role::Member(m) = &mut s.role else { return Next::Stay };
    match ev {
        LinkEvent::Closed { link, code } => {
            if link != m.link.id {
                return Next::Stay;
            }
            let why = if code == Some(u64::from(tincan_core::CLOSE_HOST_LEAVING)) { "host leaving" } else { "connection closed" };
            host_lost(s, why)
        }
        LinkEvent::SyncSent { link } => {
            if link == m.link.id {
                m.sync_out_done = true;
                s.pump_publishes();
            }
            Next::Stay
        }
        LinkEvent::Frame { link, ctl } => {
            if link != m.link.id {
                return Next::Stay;
            }
            // Liveness follows the host's session actor: media control frames come from
            // media tasks that may keep running while the session is stuck.
            if !is_extra(&ctl) {
                m.last_rx = Instant::now();
                m.suspected = false;
            }
            on_frame(s, *ctl)
        }
    }
}

fn on_frame(s: &mut Session, ctl: Ctl) -> Next {
    let Role::Member(m) = &mut s.role else { return Next::Stay };
    match ctl {
        Ctl::Roster(r) => {
            if r.host == m.host && r.epoch >= m.claim.claim.epoch {
                m.roster = r.clone();
                s.ctx.note_roster(&r);
            }
            Next::Stay
        }
        Ctl::Events { events, more } => {
            let terminal = events.is_empty() && !more;
            s.ctx.ingest_signed(events);
            if terminal && !m.synced {
                m.synced = true;
                let status = s.ctx.net.status_for(&s.ctx.server, true);
                let _ = m.link.send(&Ctl::Status(status));
                s.ctx.last_status_at = Instant::now();
                tracing::debug!(server = %s.ctx.server.short(), "initial sync complete");
            }
            Next::Stay
        }
        Ctl::SyncHave { heads } => {
            s.ctx.spawn_sync(&m.link, heads);
            Next::Stay
        }
        Ctl::PublishAck { id, ok, reason } => {
            on_ack(s, id, ok, reason);
            Next::Stay
        }
        Ctl::VvDigest { digest } => {
            let mine = s.ctx.heads();
            let interval = s.ctx.net.timings.vv_digest;
            if mine.digest() != digest && m.last_resync.is_none_or(|t| t.elapsed() >= interval / 2) {
                m.last_resync = Some(Instant::now());
                let _ = m.link.send(&Ctl::SyncHave { heads: mine });
            }
            Next::Stay
        }
        Ctl::Tick { .. } => Next::Stay,
        Ctl::Holders { hash, holders } => {
            if let Some(waiters) = s.ctx.whohas.remove(&hash) {
                for w in waiters {
                    let _ = w.send(holders.clone());
                }
            }
            Next::Stay
        }
        Ctl::Typing { channel, by } => {
            if by != s.ctx.me {
                s.ctx.net.emit(NetEvent::Typing { conv: s.ctx.server, channel, user: by });
            }
            Next::Stay
        }
        Ctl::HostMoved { claim, addr } => host_moved(s, claim, addr),
        Ctl::Handoff { claim, roster } => accept_handoff(s, claim, roster),
        Ctl::Reject { code, reason } => {
            tracing::warn!(?code, %reason, "host rejected us mid-session");
            host_lost(s, "rejected")
        }
        other if is_extra(&other) => {
            s.ctx.net.extra(CtlSource::FromHost { server: s.ctx.server, host: m.host }, other);
            Next::Stay
        }
        other => {
            tracing::trace!(?other, "member: ignoring frame");
            Next::Stay
        }
    }
}

fn on_ack(s: &mut Session, id: tincan_core::EventId, ok: bool, reason: String) {
    let matches = s.ctx.pubs.inflight.as_ref().is_some_and(|i| i.event.id == id);
    if !matches {
        return;
    }
    if ok {
        let Some(inf) = s.ctx.pubs.inflight.take() else { return };
        let ev = inf.event;
        let applied = {
            let mut r = s.ctx.replica.lock();
            r.ingest(s.ctx.net.storage.as_ref(), &s.ctx.net.hlc, ev.clone())
        };
        if let Verdict::Reject(why) = &applied.verdict {
            tracing::warn!(%why, "host accepted an event we reject");
        }
        s.ctx.after_applied(&applied.applied);
        if let Some(r) = inf.reply {
            let _ = r.send(Ok(ev));
        }
    } else if reason == "gap" {
        if let Some(inf) = &mut s.ctx.pubs.inflight
            && inf.gap_retries < super::MAX_GAP_RETRIES
        {
            inf.gap_retries += 1;
            inf.retry_at = Some(Instant::now() + super::GAP_RETRY);
            return;
        }
        reject_inflight(s, reason);
    } else {
        reject_inflight(s, reason);
    }
    s.pump_publishes();
}

fn reject_inflight(s: &mut Session, reason: String) {
    let Some(inf) = s.ctx.pubs.inflight.take() else { return };
    {
        let mut r = s.ctx.replica.lock();
        r.release_own_seq(s.ctx.net.storage.as_ref(), inf.event.event.seq);
    }
    tracing::info!(%reason, "host refused my event");
    if let Some(r) = inf.reply {
        let text = if reason.is_empty() { "The host refused this.".to_string() } else { crate::error::capitalize(&reason) };
        let _ = r.send(Err(NetError::Rejected(text)));
    }
}

/// The host says it moved (handoff, merge) or refreshed its claim.
fn host_moved(s: &mut Session, claim: SignedHostClaim, addr: AddrHint) -> Next {
    let Role::Member(m) = &mut s.role else { return Next::Stay };
    if claim.verify_for(&s.ctx.server).is_err() || claim.claim.epoch <= m.claim.claim.epoch {
        return Next::Stay;
    }
    s.ctx.epoch_seen = s.ctx.epoch_seen.max(claim.claim.epoch);
    if claim.claim.host == m.host {
        // Same host, new epoch (after a merge it won).
        m.claim = claim;
        s.ctx.update_info(&s.role);
        return Next::Stay;
    }
    let target = claim.claim.host;
    if target == s.ctx.me {
        return Next::Stay;
    }
    s.ctx.net.learn_hint(&target, &addr);
    tracing::info!(server = %s.ctx.server.short(), to = %target.short(), "host moved");
    let failed_host = m.host;
    let failed_epoch = m.claim.claim.epoch;
    let mut succession = vec![target];
    succession.extend(m.roster.succession.iter().copied().filter(|u| *u != target && *u != failed_host));
    let rank = succession.iter().position(|u| *u == s.ctx.me);
    // The new host is first in line; never promote ahead of it.
    let rank = rank.filter(|r| *r > 0);
    Next::Seek(SeekKind::Failover { failed_host, failed_epoch, succession, rank }, Some(target))
}

/// Clean handoff: my host picked me.
fn accept_handoff(s: &mut Session, claim: SignedHostClaim, roster: Roster) -> Next {
    let Role::Member(m) = &s.role else { return Next::Stay };
    let valid = claim.verify_for(&s.ctx.server).is_ok() && claim.claim.host == m.host && claim.claim.epoch == m.claim.claim.epoch;
    if !valid || !s.ctx.net.allow_hosting() {
        tracing::debug!(valid, "declining handoff");
        return Next::Stay;
    }
    let Role::Member(m) = std::mem::replace(&mut s.role, Role::Stopped) else { return Next::Stay };
    let m = *m;
    *s.ctx.shared.host_conn.lock() = None;
    let epoch = s.ctx.epoch_seen.max(m.claim.claim.epoch) + 1;
    tracing::info!(server = %s.ctx.server.short(), epoch, "taking over (handoff)");
    Next::Host(Box::new(HostStart {
        epoch,
        prev: Some(m.host),
        inherit: Some(roster),
        accept: None,
        retire: vec![m.link],
        handoff_reply: true,
    }))
}

pub(super) fn on_timer(s: &mut Session, now: Instant) -> Next {
    let t = s.ctx.net.timings.clone();
    let Role::Member(m) = &mut s.role else { return Next::Stay };
    let Some(tick_at) = s.ctx.tick_at else { return Next::Stay };
    if tick_at > now {
        return Next::Stay;
    }
    s.ctx.tick_at = Some(now + t.tick);
    s.ctx.ticks += 1;
    let silent = now.duration_since(m.last_rx);
    if silent >= t.host_dead {
        m.link.close(tincan_core::CLOSE_NORMAL, "host silent");
        return host_lost(s, "silent");
    }
    if silent >= t.host_suspect && !m.suspected {
        m.suspected = true;
        tracing::info!(server = %s.ctx.server.short(), silent_ms = silent.as_millis() as u64, "host suspected");
    }
    let _ = m.link.send(&Ctl::Tick { epoch: m.claim.claim.epoch, t_ms: now_ms() });
    let path = crate::addr::path_snapshot(&m.link.conn);
    if path != m.path {
        m.path = path;
        s.ctx.update_info(&s.role);
    }
    let Role::Member(m) = &mut s.role else { return Next::Stay };
    if now.duration_since(s.ctx.last_status_at) >= t.status_every {
        let ac = crate::power::ac_power();
        if ac != s.ctx.last_ac {
            s.ctx.last_ac = ac;
            let _ = m.link.send(&Ctl::Status(s.ctx.net.status_for(&s.ctx.server, m.synced)));
        }
        s.ctx.last_status_at = now;
    }
    Next::Stay
}
