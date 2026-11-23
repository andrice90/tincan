//! Host election: status reports, scoring, succession, claims and partition merge.

use std::cmp::Reverse;

use serde::{Deserialize, Serialize};

use crate::keys::{Identity, Sig64, verify};
use crate::{ChannelId, CoreError, Result, ServerId, UserId};

const CLAIM_CONTEXT: &[u8] = b"tincan-host1";

/// What a member reports about itself (in Hello and on change, at most every 30 s).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// App version string.
    pub app_version: String,
    /// Seconds since the app started.
    pub uptime_s: u32,
    /// On AC power (desktops always true).
    pub ac_power: bool,
    /// Voice room the member is in.
    pub voice: Option<ChannelId>,
    /// Mic muted.
    pub muted: bool,
    /// Deafened.
    pub deafened: bool,
    /// Sharing a screen.
    pub sharing: bool,
    /// False if the user turned off "allow hosting" in settings.
    pub hosting_ok: bool,
    /// The member has finished initial history sync.
    pub synced: bool,
}

/// What the host knows about a candidate when ranking.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// The member.
    pub user: UserId,
    /// Host saw a selected direct path (conn.paths(): is_selected && is_ip).
    pub direct: bool,
    /// From `Status::ac_power`.
    pub ac_power: bool,
    /// From `Status::uptime_s`.
    pub uptime_s: u32,
    /// Measured by the host.
    pub rtt_ms: u32,
    /// From `Status::hosting_ok`.
    pub hosting_ok: bool,
    /// From `Status::synced`.
    pub synced: bool,
}

/// Uptime at which the uptime term saturates (2 h).
pub const UPTIME_CAP_S: u32 = 7_200;
/// RTT at which the rtt term reaches zero.
pub const RTT_CAP_MS: u32 = 300;

/// score = 400*direct + 200*ac_power + 200*min(uptime,7200)/7200 + 50*(1 - min(rtt,300)/300),
/// each term computed with integer math and floored (`50*(300-min(rtt,300))/300`).
/// Not eligible (hosting_ok == false or synced == false) -> 0. An eligible candidate scores at
/// least 1, so `score > 0` is exactly "eligible" (a fresh, battery, relay-only member with
/// rtt >= 300 ms would otherwise compute to 0 and vanish from succession).
pub fn score(c: &Candidate) -> u32 {
    if !c.hosting_ok || !c.synced {
        return 0;
    }
    let direct = if c.direct { 400 } else { 0 };
    let ac = if c.ac_power { 200 } else { 0 };
    let uptime = 200 * c.uptime_s.min(UPTIME_CAP_S) / UPTIME_CAP_S;
    let rtt = 50 * (RTT_CAP_MS - c.rtt_ms.min(RTT_CAP_MS)) / RTT_CAP_MS;
    (direct + ac + uptime + rtt).max(1)
}

/// Most successors kept in the roster.
pub const MAX_SUCCESSORS: usize = 4;

/// Rank `candidates` (the current host excluded by the caller) and return up to 4 successors:
/// eligible only (score > 0); if any eligible candidate is direct, drop all non-direct ones;
/// order by score desc, then UserId asc. A user listed twice counts once (best entry).
pub fn succession(candidates: &[Candidate]) -> Vec<UserId> {
    let mut ranked: Vec<(u32, &Candidate)> = candidates.iter().map(|c| (score(c), c)).filter(|(s, _)| *s > 0).collect();
    if ranked.iter().any(|(_, c)| c.direct) {
        ranked.retain(|(_, c)| c.direct);
    }
    ranked.sort_by_key(|(s, c)| (Reverse(*s), c.user));
    let mut out: Vec<UserId> = Vec::with_capacity(MAX_SUCCESSORS);
    for (_, c) in ranked {
        if out.len() == MAX_SUCCESSORS {
            break;
        }
        if !out.contains(&c.user) {
            out.push(c.user);
        }
    }
    out
}

/// "I am the host of `server` for `epoch`."
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostClaim {
    /// The server.
    pub server: ServerId,
    /// Host epoch (+1 per migration).
    pub epoch: u64,
    /// The claiming host (signer).
    pub host: UserId,
    /// The previous host, if known.
    pub prev: Option<UserId>,
    /// When the claim was made (unix ms, host clock).
    pub issued_ms: u64,
}

/// A `HostClaim` signed by `claim.host`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedHostClaim {
    /// The claim.
    pub claim: HostClaim,
    /// ed25519 by claim.host over "tincan-host1" || postcard(claim)
    pub sig: Sig64,
}

impl SignedHostClaim {
    /// Sign `claim`. `CoreError::BadKey` if `claim.host != identity.user()`.
    pub fn sign(identity: &Identity, claim: HostClaim) -> Result<Self> {
        if claim.host != identity.user() {
            return Err(CoreError::BadKey);
        }
        let msg = claim_message(&claim)?;
        let sig = identity.sign(&msg);
        Ok(Self { claim, sig })
    }
    /// Check the signature by `claim.host`.
    pub fn verify(&self) -> Result<()> {
        let msg = claim_message(&self.claim)?;
        verify(&self.claim.host, &msg, &self.sig)
    }
    /// Verify and check that the claim is for `server`.
    pub fn verify_for(&self, server: &ServerId) -> Result<()> {
        if self.claim.server != *server {
            return Err(CoreError::Other("host claim for another server".into()));
        }
        self.verify()
    }
}

fn claim_message(claim: &HostClaim) -> Result<Vec<u8>> {
    let bytes = postcard::to_stdvec(claim)?;
    let mut m = Vec::with_capacity(CLAIM_CONTEXT.len() + bytes.len());
    m.extend_from_slice(CLAIM_CONTEXT);
    m.extend_from_slice(&bytes);
    Ok(m)
}

/// Inputs to the partition-merge comparison (from Ctl::HostCompare).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostSide {
    /// The host.
    pub host: UserId,
    /// Its epoch.
    pub epoch: u64,
    /// Members connected to it.
    pub connected: u32,
    /// Its own election score.
    pub score: u32,
}

/// Winner = max by (connected, epoch, score, then LOWER UserId). Returns true if `a` wins.
/// Antisymmetric: merge_winner(a, b) == !merge_winner(b, a) when a.host != b.host.
/// Returns false for two sides with the same host (nobody wins against itself).
pub fn merge_winner(a: &HostSide, b: &HostSide) -> bool {
    let key = |s: &HostSide| (s.connected, s.epoch, s.score, Reverse(s.host));
    key(a) > key(b)
}

/// Proactive migration hysteresis (all must hold): no active voice room, best candidate's
/// score >= current * 1.3 continuously for 10 min, and >= 30 min since the last migration.
pub const PROACTIVE_MARGIN_PCT: u32 = 30;
/// How long the better candidate must stay better before a proactive migration.
pub const PROACTIVE_HOLD_MS: u64 = 10 * 60 * 1000;
/// Minimum time between migrations.
pub const PROACTIVE_MIN_INTERVAL_MS: u64 = 30 * 60 * 1000;

/// True if `best` beats `current` by at least `PROACTIVE_MARGIN_PCT` percent.
pub fn beats_by_margin(best: u32, current: u32) -> bool {
    u64::from(best) * 100 >= u64::from(current) * u64::from(100 + PROACTIVE_MARGIN_PCT) && best > current
}

/// Liveness tick interval (both ways).
pub const TICK_MS: u64 = 2_000;
/// Host suspected after this much silence.
pub const HOST_SUSPECT_MS: u64 = 6_000;
/// Host considered dead after this much silence.
pub const HOST_DEAD_MS: u64 = 10_000;
/// Successor k promotes itself after k times this without reaching a higher-ranked one.
pub const SUCCESSOR_STAGGER_MS: u64 = 3_000;
/// How long a leaving host waits for `HandoffAck`.
pub const HANDOFF_ACK_MS: u64 = 2_000;
/// App-level timeout around every dial.
pub const DIAL_TIMEOUT_MS: u64 = 4_000;
/// Nobody reached after this long on cold open: become solo host.
pub const SOLO_AFTER_MS: u64 = 6_000;
/// While solo: retry interval.
pub const SOLO_RETRY_MS: u64 = 60_000;
/// While solo: retry interval after 30 min alone.
pub const SOLO_RETRY_MAX_MS: u64 = 5 * 60_000;
/// Minimum interval between unsolicited status reports.
pub const STATUS_EVERY_MS: u64 = 30_000;
/// Roster broadcast debounce.
pub const ROSTER_DEBOUNCE_MS: u64 = 250;
/// Interval of `Ctl::VvDigest`.
pub const VV_DIGEST_EVERY_MS: u64 = 60_000;

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(b: u8) -> Candidate {
        Candidate { user: UserId([b; 32]), direct: false, ac_power: false, uptime_s: 0, rtt_ms: 0, hosting_ok: true, synced: true }
    }

    #[test]
    fn score_terms() {
        let mut c = cand(1);
        assert_eq!(score(&c), 50); // rtt 0 only
        c.rtt_ms = 150;
        assert_eq!(score(&c), 25);
        c.rtt_ms = 299;
        assert_eq!(score(&c), 1); // 50*1/300 floors to 0, clamped to 1
        c.rtt_ms = 10_000;
        assert_eq!(score(&c), 1);
        c.direct = true;
        assert_eq!(score(&c), 400);
        c.ac_power = true;
        assert_eq!(score(&c), 600);
        c.uptime_s = 3_600;
        assert_eq!(score(&c), 700);
        c.uptime_s = 7_200;
        assert_eq!(score(&c), 800);
        c.uptime_s = u32::MAX;
        assert_eq!(score(&c), 800);
        c.rtt_ms = 0;
        assert_eq!(score(&c), 850); // maximum
        c.uptime_s = 1;
        assert_eq!(score(&c), 650); // 200*1/7200 floors to 0
    }

    #[test]
    fn ineligible_scores_zero() {
        let mut c = cand(1);
        c.direct = true;
        c.ac_power = true;
        c.hosting_ok = false;
        assert_eq!(score(&c), 0);
        c.hosting_ok = true;
        c.synced = false;
        assert_eq!(score(&c), 0);
    }

    #[test]
    fn eligible_zero_raw_score_stays_in_succession() {
        let mut c = cand(1);
        c.rtt_ms = 300;
        assert_eq!(score(&c), 1);
        assert_eq!(succession(&[c.clone()]), vec![c.user]);
    }

    #[test]
    fn succession_orders_and_limits() {
        let mut cs: Vec<Candidate> = (1..=6).map(cand).collect();
        for (i, c) in cs.iter_mut().enumerate() {
            c.direct = true;
            c.uptime_s = (i as u32) * 1_000;
        }
        // highest uptime first
        let s = succession(&cs);
        assert_eq!(s, vec![UserId([6; 32]), UserId([5; 32]), UserId([4; 32]), UserId([3; 32])]);
    }

    #[test]
    fn succession_tie_breaks_by_lower_user() {
        let cs = vec![cand(9), cand(3), cand(5)];
        assert_eq!(succession(&cs), vec![UserId([3; 32]), UserId([5; 32]), UserId([9; 32])]);
    }

    #[test]
    fn relay_only_excluded_while_a_direct_exists() {
        let mut relay = cand(1);
        relay.ac_power = true;
        relay.uptime_s = 7_200;
        let mut direct = cand(2);
        direct.direct = true;
        direct.rtt_ms = 300;
        // the relay-only member scores higher (450 vs 400) but is still excluded
        assert_eq!((score(&relay), score(&direct)), (450, 400));
        assert_eq!(succession(&[relay.clone(), direct.clone()]), vec![direct.user]);
        // an ineligible direct member does not exclude relay-only ones
        let mut direct_off = direct.clone();
        direct_off.hosting_ok = false;
        assert_eq!(succession(&[relay.clone(), direct_off]), vec![relay.user]);
        // no direct at all: relay-only members are ranked normally
        let mut relay2 = cand(3);
        relay2.ac_power = true;
        assert_eq!(succession(&[relay2.clone(), relay.clone()]), vec![relay.user, relay2.user]);
    }

    #[test]
    fn succession_skips_ineligible_and_duplicates() {
        let mut off = cand(1);
        off.synced = false;
        let a = cand(2);
        let mut a_dup = cand(2);
        a_dup.ac_power = true;
        assert_eq!(succession(&[off, a, a_dup]), vec![UserId([2; 32])]);
        assert!(succession(&[]).is_empty());
    }

    #[test]
    fn host_claim_sign_verify() {
        let id = Identity::generate();
        let server = ServerId::from_bytes([1; 32]);
        let claim = HostClaim { server, epoch: 7, host: id.user(), prev: Some(UserId([2; 32])), issued_ms: 123 };
        let s = SignedHostClaim::sign(&id, claim.clone()).unwrap();
        s.verify().unwrap();
        s.verify_for(&server).unwrap();
        assert!(s.verify_for(&ServerId::from_bytes([2; 32])).is_err());
        let mut t = s.clone();
        t.claim.epoch = 8;
        assert!(t.verify().is_err());
        let mut t = s.clone();
        t.claim.prev = None;
        assert!(t.verify().is_err());
        let mut t = s.clone();
        t.sig.0[5] ^= 1;
        assert!(t.verify().is_err());
        // a claim naming someone else cannot be signed
        let other = Identity::generate();
        assert!(SignedHostClaim::sign(&other, claim).is_err());
        // round trip on the wire
        let bytes = postcard::to_stdvec(&s).unwrap();
        let back: SignedHostClaim = postcard::from_bytes(&bytes).unwrap();
        back.verify().unwrap();
    }

    #[test]
    fn merge_winner_order() {
        let side = |h: u8, connected, epoch, score| HostSide { host: UserId([h; 32]), epoch, connected, score };
        assert!(merge_winner(&side(9, 3, 1, 1), &side(1, 2, 9, 9))); // connected first
        assert!(merge_winner(&side(9, 2, 2, 1), &side(1, 2, 1, 9))); // then epoch
        assert!(merge_winner(&side(9, 2, 2, 5), &side(1, 2, 2, 4))); // then score
        assert!(merge_winner(&side(1, 2, 2, 5), &side(9, 2, 2, 5))); // then lower id
        assert!(!merge_winner(&side(9, 2, 2, 5), &side(1, 2, 2, 5)));
        let a = side(4, 1, 1, 1);
        assert!(!merge_winner(&a, &a.clone()));
    }

    #[test]
    fn merge_winner_is_antisymmetric() {
        let mut sides = Vec::new();
        for h in 1..=3u8 {
            for connected in 0..=2 {
                for epoch in 0..=2 {
                    for score in [0, 400, 850] {
                        sides.push(HostSide { host: UserId([h; 32]), epoch, connected, score });
                    }
                }
            }
        }
        for a in &sides {
            for b in &sides {
                if a.host != b.host {
                    assert_eq!(merge_winner(a, b), !merge_winner(b, a), "{a:?} vs {b:?}");
                }
            }
        }
    }

    #[test]
    fn margin() {
        assert!(beats_by_margin(130, 100));
        assert!(!beats_by_margin(129, 100));
        assert!(beats_by_margin(1, 0));
        assert!(!beats_by_margin(0, 0));
        assert!(beats_by_margin(u32::MAX, u32::MAX / 2));
    }
}
