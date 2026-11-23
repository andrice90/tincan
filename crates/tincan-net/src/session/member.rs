//! Member role: connected to the host of a server.
//!
//! The host is not trusted either: it cannot raise my seqs (`Welcome.your_next_seq` is
//! ignored), cannot make me release a seq I signed and sent (a refused publish is parked and
//! re-sent, never re-signed), and what it relays (rosters, holders, typing) is cut to current
//! members and bounded.

use std::time::Duration;

use tincan_core::election::{self, SignedHostClaim};
use tincan_core::state::Verdict;
use tincan_core::sync::Heads;
use tincan_core::wire::{self, AddrHint, Ctl, HeadsAssembler, Roster};
use tincan_core::{UserId, now_ms};
use tokio::time::Instant;

use super::{HostStart, Next, Role, SeekKind, Session};
use crate::addr::PathSnapshot;
use crate::error::NetError;
use crate::events::NetEvent;
use crate::hooks::{CtlSource, is_extra};
use crate::link::{Link, LinkEvent};
use crate::media::MemberMedia;

/// Rosters from the host are processed at most this often (honest hosts send at most one
/// per `roster_min_interval`); the latest one waits for the tick.
const ROSTER_MIN_GAP: Duration = Duration::from_millis(250);
/// Longest refusal text from a host shown to the user.
const MAX_REFUSAL_CHARS: usize = 200;

/// State while connected to a host.
pub(crate) struct MemberState {
    /// Control link to the host.
    pub link: Link,
    /// The host.
    pub host: UserId,
    /// The host's claim (given out in Redirects).
    pub claim: SignedHostClaim,
    /// Latest roster from the host, cut to current members.
    pub roster: Roster,
    /// The same roster as received (bounded), re-filtered once my history is complete.
    raw_roster: Roster,
    /// A roster that arrived too soon after the previous one (processed on the tick).
    pending_roster: Option<Roster>,
    /// When the last roster was processed.
    roster_at: Option<Instant>,
    /// The host's sync of what I lacked finished.
    pub synced: bool,
    /// My sync of what the host lacked finished (publishes wait for it).
    pub sync_out_done: bool,
    /// A sync to the host is running; the latest further request waits here.
    sync_running: bool,
    sync_queued: Option<Heads>,
    parts: HeadsAssembler,
    /// Last frame from the host.
    pub last_rx: Instant,
    /// Suspected dead already (logged once).
    pub suspected: bool,
    /// Last time I sent SyncHave because of a digest mismatch.
    pub last_resync: Option<Instant>,
    /// Last attestation sent.
    last_attest: Instant,
    /// Path to the host.
    pub path: PathSnapshot,
    /// Media rides this link while the state lives (dropped with it).
    _media: MemberMedia,
}

impl MemberState {
    /// Fresh state after Welcome (`roster` already cut to members, `raw` as received).
    pub fn new(link: Link, claim: SignedHostClaim, roster: Roster, raw: Roster, path: PathSnapshot, media: MemberMedia) -> Self {
        Self {
            host: claim.claim.host,
            link,
            claim,
            roster,
            raw_roster: raw,
            pending_roster: None,
            roster_at: None,
            synced: false,
            sync_out_done: false,
            sync_running: false,
            sync_queued: None,
            parts: HeadsAssembler::default(),
            last_rx: Instant::now(),
            suspected: false,
            last_resync: None,
            last_attest: Instant::now(),
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
                m.sync_running = false;
                if let Some(heads) = m.sync_queued.take() {
                    m.sync_running = true;
                    s.ctx.spawn_sync(&m.link, heads);
                }
                s.pump_publishes();
            }
            Next::Stay
        }
        LinkEvent::Frame { link, ctl, .. } => {
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

/// Take a roster from the host: bounded, members only.
fn take_roster(s: &mut Session, r: Roster) {
    let Role::Member(m) = &mut s.role else { return };
    if r.host != m.host || r.epoch < m.claim.claim.epoch || r.online.len() > wire::MAX_ROSTER {
        return;
    }
    let now = Instant::now();
    if m.roster_at.is_some_and(|t| now.duration_since(t) < ROSTER_MIN_GAP) {
        m.pending_roster = Some(r);
        return;
    }
    m.roster_at = Some(now);
    m.pending_roster = None;
    m.raw_roster = r.clone();
    if let Some(clean) = s.ctx.sanitize_roster(r) {
        let Role::Member(m) = &mut s.role else { return };
        m.roster = clean.clone();
        s.ctx.note_roster(&clean);
    }
}

fn on_frame(s: &mut Session, ctl: Ctl) -> Next {
    let Role::Member(m) = &mut s.role else { return Next::Stay };
    match ctl {
        Ctl::Roster(r) => {
            take_roster(s, r);
            Next::Stay
        }
        Ctl::Events { events, more } => {
            let terminal = events.is_empty() && !more;
            s.ctx.ingest_signed(events);
            let Role::Member(m) = &mut s.role else { return Next::Stay };
            if terminal && !m.synced {
                m.synced = true;
                let status = s.ctx.net.status_for(&s.ctx.server, true);
                let _ = m.link.send(&Ctl::Status(status));
                s.ctx.last_status_at = Instant::now();
                tracing::debug!(server = %s.ctx.server.short(), "initial sync complete");
                s.ctx.net.emit(NetEvent::SyncComplete { conv: s.ctx.server });
                // Members I did not know before the sync may be in the roster.
                let raw = m.raw_roster.clone();
                if let Some(clean) = s.ctx.sanitize_roster(raw) {
                    let Role::Member(m) = &mut s.role else { return Next::Stay };
                    m.roster = clean.clone();
                    s.ctx.note_roster(&clean);
                }
                s.pump_publishes();
            }
            Next::Stay
        }
        Ctl::SyncHave { heads } => {
            host_sync_have(s, heads);
            Next::Stay
        }
        Ctl::SyncHavePart { heads, more } => {
            if let Some(full) = m.parts.push(heads, more) {
                host_sync_have(s, full);
            }
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
                s.ctx.send_heads(&m.link, mine);
            }
            Next::Stay
        }
        Ctl::Tick { .. } => Next::Stay,
        Ctl::Holders { hash, holders } => {
            // Only current members, and a bounded list.
            let holders: Vec<(UserId, AddrHint)> = {
                let r = s.ctx.replica.lock();
                holders.into_iter().filter(|(u, _)| r.state.is_member(u)).take(wire::MAX_HOLDERS).map(|(u, a)| (u, a.bounded())).collect()
            };
            if let Some(waiters) = s.ctx.whohas.remove(&hash) {
                for w in waiters {
                    let _ = w.send(holders.clone());
                }
            }
            Next::Stay
        }
        Ctl::Typing { channel, by } => {
            // The host could name anyone: only current members that are online.
            let online = m.roster.online.iter().any(|e| e.user == by);
            if by != s.ctx.me && online && s.ctx.is_member(&by) {
                s.ctx.net.emit(NetEvent::Typing { conv: s.ctx.server, channel, user: by });
            }
            Next::Stay
        }
        Ctl::HostMoved { claim, addr } => host_moved(s, claim, addr),
        Ctl::Handoff { claim, roster } => accept_handoff(s, claim, roster),
        Ctl::Reject { code, reason } => {
            tracing::warn!(?code, reason = %wire::sanitize_text(&reason, MAX_REFUSAL_CHARS), "host rejected us mid-session");
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

/// The host asked for what it lacks: one sync at a time, the latest request queued.
fn host_sync_have(s: &mut Session, heads: Heads) {
    let Role::Member(m) = &mut s.role else { return };
    if m.sync_running {
        m.sync_queued = Some(heads);
        return;
    }
    m.sync_running = true;
    s.ctx.spawn_sync(&m.link, heads);
}

/// Refusal text from a host, safe to show.
fn refusal_text(reason: &str) -> String {
    let clean = wire::sanitize_text(reason, MAX_REFUSAL_CHARS);
    if clean.trim().is_empty() { "The host refused this.".to_string() } else { crate::error::capitalize(&clean) }
}

/// The error for a refusal: the host's words when it gave some, else my own sentence.
fn refusal(reason: &str) -> NetError {
    if wire::sanitize_text(reason, MAX_REFUSAL_CHARS).trim().is_empty() {
        NetError::Rejected(refusal_text(reason))
    } else {
        NetError::RejectedByPeer(refusal_text(reason))
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
    } else if reason == "gap" || reason == "busy" {
        if let Some(inf) = &mut s.ctx.pubs.inflight
            && inf.gap_retries < super::MAX_GAP_RETRIES
        {
            inf.gap_retries += 1;
            inf.retry_at = Some(Instant::now() + super::GAP_RETRY);
            return;
        }
        refused(s, reason);
    } else {
        refused(s, reason);
    }
    s.pump_publishes();
}

/// The host refused my in-flight event. The signed event has left this device, so its seq
/// is released only if my own validation rejects it too (then no honest replica can store
/// it). Otherwise it stays in flight unchanged (never re-signed with another body) and is
/// re-sent later and after the next Welcome from any host.
fn refused(s: &mut Session, reason: String) {
    let Some(inf) = &s.ctx.pubs.inflight else { return };
    let verdict = s.ctx.replica.lock().state.check(&inf.event.id, &inf.event.event, now_ms());
    tracing::info!(reason = %wire::sanitize_text(&reason, MAX_REFUSAL_CHARS), ?verdict, "host refused my event");
    match verdict {
        Verdict::Reject(_) => {
            let Some(inf) = s.ctx.pubs.inflight.take() else { return };
            s.ctx.replica.lock().release_own_seq(s.ctx.net.storage.as_ref(), inf.event.event.seq);
            if let Some(r) = inf.reply {
                let _ = r.send(Err(refusal(&reason)));
            }
        }
        Verdict::Stale => {
            // It reached me through sync meanwhile: done.
            let Some(inf) = s.ctx.pubs.inflight.take() else { return };
            let stored = matches!(s.ctx.net.storage.event(&inf.event.id), Ok(Some(_)));
            if let Some(r) = inf.reply {
                let _ = r.send(if stored { Ok(inf.event) } else { Err(refusal(&reason)) });
            }
        }
        _ => {
            let retry = Instant::now() + s.ctx.net.timings.publish_park_retry;
            let Some(inf) = &mut s.ctx.pubs.inflight else { return };
            inf.retry_at = Some(retry);
            inf.gap_retries = 0;
            if let Some(r) = inf.reply.take() {
                let _ = r.send(Err(refusal(&reason)));
            }
        }
    }
}

/// The host says it moved (handoff, merge) or refreshed its claim.
fn host_moved(s: &mut Session, claim: SignedHostClaim, addr: AddrHint) -> Next {
    let Role::Member(m) = &mut s.role else { return Next::Stay };
    if claim.verify_for(&s.ctx.server).is_err() || claim.claim.epoch <= m.claim.claim.epoch || !s.ctx.epoch_ok(claim.claim.epoch) {
        return Next::Stay;
    }
    s.ctx.epoch_seen = s.ctx.epoch_seen.max(claim.claim.epoch);
    if claim.claim.host == m.host {
        // Same host, new epoch (after a merge it won): persist it and re-attest for it.
        s.ctx.update_record(claim.claim.epoch, m.host);
        s.ctx.attest(&m.link, &claim);
        m.last_attest = Instant::now();
        m.claim = claim;
        s.ctx.update_info(&s.role);
        return Next::Stay;
    }
    let target = claim.claim.host;
    if target == s.ctx.me || !s.ctx.may_host(&target) {
        return Next::Stay;
    }
    s.ctx.net.learn_hint(&target, &addr.bounded());
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
    let epoch = election::next_epoch(s.ctx.epoch_seen.max(m.claim.claim.epoch));
    if !valid || !s.ctx.net.allow_hosting() || epoch.is_none() || s.ctx.never_promote() {
        tracing::debug!(valid, "declining handoff");
        return Next::Stay;
    }
    let roster = s.ctx.sanitize_roster(roster).unwrap_or_default();
    let Role::Member(m) = std::mem::replace(&mut s.role, Role::Stopped) else { return Next::Stay };
    let m = *m;
    *s.ctx.shared.host_conn.lock() = None;
    let epoch = epoch.unwrap_or(1);
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
    if m.pending_roster.is_some()
        && m.roster_at.is_none_or(|at| now.duration_since(at) >= ROSTER_MIN_GAP)
        && let Some(r) = m.pending_roster.take()
    {
        take_roster(s, r);
    }
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
    if now.duration_since(m.last_attest) >= t.attest_every {
        m.last_attest = now;
        s.ctx.attest(&m.link, &m.claim);
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusal_text_is_bounded_and_clean() {
        let evil = format!("\u{1b}[2J\u{7}Your account is locked, visit http://x{}", "!".repeat(1_000_000));
        let t = refusal_text(&evil);
        assert!(t.chars().count() <= MAX_REFUSAL_CHARS);
        assert!(!t.chars().any(char::is_control));
        assert_eq!(refusal_text("\u{7}\u{8}"), "The host refused this.");
        assert!(matches!(refusal("\u{7}"), NetError::Rejected(_)), "no words of the host: my own sentence");
        assert!(matches!(refusal("busy"), NetError::RejectedByPeer(t) if t == "Busy."));
    }
}
