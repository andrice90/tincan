//! Protocol timings in one place. Defaults come from `tincan_core::election` and
//! docs/architecture.md; tests shrink some of them to stay fast.

use std::time::Duration;

use tincan_core::election;

/// Every timer the network layer uses.
#[derive(Clone, Debug)]
pub(crate) struct Timings {
    /// Liveness tick, both ways.
    pub tick: Duration,
    /// Host suspected after this much silence (logged only).
    pub host_suspect: Duration,
    /// Host (or member, on the host side) considered dead after this much silence.
    pub host_dead: Duration,
    /// Successor k (0-based) promotes itself after k times this.
    pub stagger: Duration,
    /// How long a leaving host waits for `HandoffAck`.
    pub handoff_ack: Duration,
    /// App-level timeout around every dial (connect plus first reply).
    pub dial: Duration,
    /// Delay between staggered dials of one cold-open round.
    pub dial_stagger: Duration,
    /// Cold open: become solo host when nobody answered after this long.
    pub solo_after: Duration,
    /// While solo: retry interval for dialing cached peers.
    pub solo_retry: Duration,
    /// While solo: retry interval after `solo_backoff_after`.
    pub solo_retry_max: Duration,
    /// Time alone after which the solo retry backs off.
    pub solo_backoff_after: Duration,
    /// While hosting with members: interval for probing offline cached peers (partition merge).
    pub host_probe: Duration,
    /// Roster broadcast debounce.
    pub roster_debounce: Duration,
    /// `VvDigest` interval.
    pub vv_digest: Duration,
    /// Minimum interval between unsolicited status refreshes (power, uptime).
    pub status_every: Duration,
    /// How long `publish` waits for the host's acknowledgement.
    pub publish_timeout: Duration,
    /// Overall budget of `join`.
    pub join_timeout: Duration,
    /// Merge: how long to wait for the other host's reply.
    pub merge_timeout: Duration,
    /// Redial interval for an answering-but-not-hosting successor during failover.
    pub failover_redial: Duration,
    /// DM delivery retry schedule (the last value repeats).
    pub dm_retry: Vec<Duration>,
    /// Close an idle DM connection after this long without traffic.
    pub dm_idle: Duration,
    /// Watched contacts (`Net::dm_watch`): redial schedule after a lost or failed DM
    /// connection (the last value repeats), and the spacing of failed reachability probes.
    pub dm_watch_retry: Vec<Duration>,
    /// Watched contacts: a keepalive frame at least this often, well below the peer's
    /// `dm_idle`, so their side does not close the connection as idle.
    pub dm_keepalive: Duration,
    /// How often the host evaluates proactive migration.
    pub proactive_check: Duration,
    /// Proactive migration: better candidate must stay better this long.
    pub proactive_hold: Duration,
    /// Proactive migration: minimum time since the last migration.
    pub proactive_interval: Duration,
    /// `who_has` reply budget.
    pub who_has: Duration,
    /// A call rings this long before it ends unanswered.
    pub call_ring: Duration,
    /// A host handles at most one partition-merge comparison per peer this often.
    pub compare_cooldown: Duration,
    /// How often a member re-attests to its host (`Ctl::Attest`).
    pub attest_every: Duration,
    /// Minimum interval between roster broadcasts (more after many changes in a minute).
    pub roster_min_interval: Duration,
    /// A published event the host refused is re-sent this often (and after every Welcome).
    pub publish_park_retry: Duration,
    /// A joiner must send its Hello within this after JoinAccepted.
    pub joiner_hello: Duration,
    /// A member is established once this replica stored its admission at least this long ago
    /// (local clock). Only established members' attestations count against a live host in a
    /// partition-merge comparison (members admitted just now, sybils, cannot outvote it),
    /// and a member that is neither established nor older than the host role spends its
    /// admitter's admission budget.
    pub established_after: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            tick: Duration::from_millis(election::TICK_MS),
            host_suspect: Duration::from_millis(election::HOST_SUSPECT_MS),
            host_dead: Duration::from_millis(election::HOST_DEAD_MS),
            stagger: Duration::from_millis(election::SUCCESSOR_STAGGER_MS),
            handoff_ack: Duration::from_millis(election::HANDOFF_ACK_MS),
            dial: Duration::from_millis(election::DIAL_TIMEOUT_MS),
            dial_stagger: Duration::from_millis(150),
            solo_after: Duration::from_millis(election::SOLO_AFTER_MS),
            solo_retry: Duration::from_millis(election::SOLO_RETRY_MS),
            solo_retry_max: Duration::from_millis(election::SOLO_RETRY_MAX_MS),
            solo_backoff_after: Duration::from_secs(30 * 60),
            host_probe: Duration::from_secs(5 * 60),
            roster_debounce: Duration::from_millis(election::ROSTER_DEBOUNCE_MS),
            vv_digest: Duration::from_millis(election::VV_DIGEST_EVERY_MS),
            status_every: Duration::from_millis(election::STATUS_EVERY_MS),
            publish_timeout: Duration::from_secs(15),
            join_timeout: Duration::from_secs(20),
            merge_timeout: Duration::from_secs(3),
            failover_redial: Duration::from_secs(1),
            dm_retry: vec![
                Duration::from_secs(30),
                Duration::from_secs(60),
                Duration::from_secs(120),
                Duration::from_secs(300),
                Duration::from_secs(900),
            ],
            dm_idle: Duration::from_secs(120),
            dm_watch_retry: vec![Duration::from_secs(2), Duration::from_secs(5), Duration::from_secs(15), Duration::from_secs(30), Duration::from_secs(60)],
            dm_keepalive: Duration::from_secs(45),
            proactive_check: Duration::from_secs(30),
            proactive_hold: Duration::from_millis(election::PROACTIVE_HOLD_MS),
            proactive_interval: Duration::from_millis(election::PROACTIVE_MIN_INTERVAL_MS),
            who_has: Duration::from_secs(3),
            call_ring: Duration::from_secs(45),
            compare_cooldown: Duration::from_secs(60),
            attest_every: Duration::from_millis(election::ATTEST_EVERY_MS),
            roster_min_interval: Duration::from_secs(1),
            publish_park_retry: Duration::from_secs(30),
            joiner_hello: Duration::from_secs(10),
            established_after: Duration::from_secs(24 * 60 * 60),
        }
    }
}

impl Timings {
    /// DM retry delay for the `attempt`-th consecutive failure (0-based).
    pub fn dm_retry_delay(&self, attempt: usize) -> Duration {
        self.dm_retry
            .get(attempt)
            .or_else(|| self.dm_retry.last())
            .copied()
            .unwrap_or(Duration::from_secs(900))
    }

    /// Watched-contact redial (and probe) delay after `attempt` consecutive failures.
    pub fn dm_watch_retry_delay(&self, attempt: usize) -> Duration {
        self.dm_watch_retry
            .get(attempt)
            .or_else(|| self.dm_watch_retry.last())
            .copied()
            .unwrap_or(Duration::from_secs(60))
    }
}
