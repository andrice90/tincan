//! Deterministic validation and the derived state of a conversation.
//!
//! Every member runs the same rules, so histories merge by set union and the
//! host is never an authority. See docs/architecture.md "Events".
//!
//! # Verdicts
//!
//! `ConvState::check` validates WITHOUT mutating; `apply` validates then mutates and returns
//! what changed. `Accept`: apply and store. `Stale`: already have it (or an older seq), drop.
//! `Gap`: a prerequisite is missing (an earlier seq of the author, the Genesis, the author's
//! admission, or another author's event it refers to); hold it in a [`PendingBuffer`].
//! `Future`: hlc more than 5 minutes ahead; hold and retry. `Reject`: invalid forever, drop.
//!
//! A `Reject` is permanent and seqs are contiguous, so a rejected event blocks every later
//! event of its author on that replica. Rules therefore reject only on facts that cannot
//! change: the event itself, the author's own earlier events (always applied first) and the
//! Genesis owner. A missing event of ANOTHER author is always `Gap`.
//!
//! # Rules, in order
//!
//! 1. `ev.v == PROTOCOL_VERSION`; `ev.server == self.conv`, except Genesis, which must carry
//!    `ConvId([0; 32])`, have `EventId == conv` (ServerId is the genesis id), seq 1 and
//!    author == owner; seq >= 1.
//! 2. seq must be exactly `head(author) + 1`. `seq <= head` -> `Stale`; `seq > head + 1` -> `Gap`
//!    (rule 4 and the DM party rule are checked first, so malformed events are not buffered).
//! 3. hlc strictly greater than the author's previous hlc (else `Reject`); more than
//!    `MAX_FUTURE_MS` ahead of `now_ms` -> `Future`.
//! 4. Self-contained body rules (`Reject`): names non-empty, at most `MAX_NAME_CHARS`, no
//!    control characters; Message text at most `MAX_TEXT_CHARS` chars, at most
//!    `MAX_ATTACHMENTS`, not empty; `MemberLeave.last_seq`
//!    is `seq - 1` or `seq`; `MemberAdmit.from_seq >= 1`.
//! 5. Servers: no Genesis applied yet -> `Gap`. ChannelCreate / ChannelUpdate / ServerUpdate:
//!    owner only (`Reject`).
//! 6. Membership: the author must be a member at `ev.seq`: some admission (the owner's Genesis
//!    counts as an admission from seq 1, `MemberAdmit{user=author}` from `from_seq`) with
//!    `from_seq <= ev.seq` and `from_seq > last_seq` of the author's latest `MemberLeave`.
//!    Not provable -> `Gap` (during sync a joiner's events can arrive before the host's admit;
//!    a re-admit can arrive after the member's next events).
//! 7. References:
//!    - ChannelUpdate: target must exist (owner's own chain, so unknown -> `Reject`).
//!    - Message: channel exists (unknown -> `Gap`, or `Reject` when the author is the owner)
//!      and is a Text channel (`Reject` otherwise).
//!    - Delete: target is a known Message whose author is `ev.author`, or `ev.author` is the
//!      owner. Unknown target: `Gap` for the owner, `Reject` for anyone else (their own
//!      messages are always known first). Only messages can be deleted.
//!    - InviteCreate: any member; an `invite_pk` that already exists -> `Reject`.
//!    - InviteRevoke: the invite's creator or the owner. Unknown: `Gap` for the owner,
//!      `Reject` for anyone else.
//!    - MemberAdmit: invite known (else `Gap`) and not expired at the admit's hlc ms
//!      (`Reject`). Admitting someone who already is a member is accepted as a no-op
//!      (it only records the admission), so two hosts admitting the same joiner converge.
//! 8. DM conversations (`ConvState::new_dm(a, b)`): only `Message` and `Delete` bodies, the
//!    author must be a or b, `Message.channel` must be `dm_channel(conv)`, a Delete may only
//!    target the author's own message. No membership events.
//! 9. MemberRemove: owner only, and never naming the owner (`Reject`). It needs nothing
//!    else: a removal may arrive before the admission it ends.
//!
//! # Removal: void events (not part of the verdict)
//!
//! A removal is a cut in the removed user's own seq space, not in time: hlc cannot bound
//! a member who keeps publishing, because an author only has to move past their own previous
//! hlc. Rule 6 ignores removals, so the chain stays monotonic and contiguous (a re-join needs
//! the seqs in between). Instead, validity is derived from the applied set:
//! - `MemberRemove { user, last_seq }` makes `user`'s events with seq > `last_seq` void until
//!   a re-admit clears it: a `MemberAdmit` naming `user` that comes after the removal by
//!   (hlc, id), has `from_seq > last_seq` and was signed at a valid seq (the owner's always
//!   are). Admits that came before the removal, such as the user's old admission, never clear
//!   it. Signer validity depends on other removals, so the cleared set is a least fixpoint
//!   (`refresh_removals`), which also stops two removed users from re-admitting each other.
//! - A void event is stored and synced, with no effect: void messages are hidden
//!   ([`ConvState::event_valid`]), a void Profile does not name anyone, an invite created at
//!   a void seq admits nobody ([`ConvState::invite_status`]), and a void admit does not clear
//!   a removal. `is_member` is false while a removal is uncleared.
//! - Still applied from void events: a Delete of the author's own message, InviteRevoke,
//!   MemberLeave, and a MemberAdmit of someone else. They only reach replicas through a
//!   member that relays a removed user's events (honest hosts refuse them), and that member
//!   could do the same with its own events.
//! - Events up to `last_seq` stay valid whenever they arrive.
//!
//! # Convergence (deviations from the literal architecture rules)
//!
//! Two rules from architecture.md depend on arrival order and are NOT enforced here, because a
//! replica that saw the other event first would reject what the rest accepted, and that
//! author's chain would stay blocked on it forever:
//! - A Message to a channel deleted by a concurrent `ChannelUpdate{deleted: true}` is accepted.
//!   Deleted channels and their messages are hidden by display code; authors check
//!   [`ConvState::can_post`] before posting.
//! - A `MemberAdmit` using a revoked invite is accepted. Revocation and `max_uses` are enforced
//!   by the admitting host at join time ([`ConvState::invite_status`]). This costs no
//!   security: any member can create a fresh invite and admit anyone anyway.
//!
//! With these choices every rule but one is monotonic (once valid, valid in any larger
//! history), so any delivery order, drained through a `PendingBuffer`, reaches the same state.
//! The exception is a second `InviteCreate` with an already known `invite_pk`, which is
//! rejected: honest invite keys are random, so only a member deliberately copying another's key
//! hits it (and can then block the original creator's chain on replicas that receive the copy
//! first). Equivocation (an author signing two different events with the same seq) cannot be
//! repaired here either: each replica keeps the first it saw.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::ops::Bound;

use crate::event::{Attachment, Body, ChannelKind, Event, StoredEvent};
use crate::hlc::hlc_ms;
use crate::sync::Heads;
use crate::wire::RejectCode;
use crate::{ChannelId, ConvId, EventId, PROTOCOL_VERSION, UserId};

/// Allowed clock skew into the future.
pub const MAX_FUTURE_MS: u64 = 5 * 60 * 1000;
/// Longest message text, in chars.
pub const MAX_TEXT_CHARS: usize = 4000;
/// Most attachments per message.
pub const MAX_ATTACHMENTS: usize = 10;
/// Longest server, channel or display name, in chars.
pub const MAX_NAME_CHARS: usize = 100;
/// Longest attachment file name, in chars.
pub const MAX_ATTACHMENT_NAME_CHARS: usize = 255;
/// Longest attachment MIME type, in chars.
pub const MAX_MIME_CHARS: usize = 255;
/// Most events a `PendingBuffer` holds (oldest dropped first).
pub const MAX_PENDING_EVENTS: usize = 10_000;
/// Most bytes (approximate) a `PendingBuffer` holds (oldest dropped first).
pub const MAX_PENDING_BYTES: usize = 32 * 1024 * 1024;

/// The single channel of a DM conversation: the conversation id as a ChannelId.
pub fn dm_channel(conv: &ConvId) -> ChannelId { EventId(conv.0) }

/// Result of validating one event against a conversation state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Valid and next in line: store and apply.
    Accept,
    /// Already applied or older than head: drop silently.
    Stale,
    /// A prerequisite is missing (seq gap, admission, referenced event): hold in the
    /// pending buffer until it arrives.
    Gap,
    /// hlc too far in the future: hold and retry later.
    Future,
    /// Invalid: drop and log. The string is for logs only.
    Reject(String),
}

impl Verdict {
    /// `Accept`.
    pub fn is_accept(&self) -> bool { matches!(self, Verdict::Accept) }
    /// `Gap` or `Future`: keep the event in a `PendingBuffer`.
    pub fn should_buffer(&self) -> bool { matches!(self, Verdict::Gap | Verdict::Future) }
}

/// A channel of a server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Channel {
    /// Id of its ChannelCreate event.
    pub id: ChannelId,
    /// Current name.
    pub name: String,
    /// Text or voice.
    pub kind: ChannelKind,
    /// Sort position within its kind.
    pub position: u32,
    /// Deleted by the owner (hidden).
    pub deleted: bool,
}

/// A member of a server (current or former).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    /// The user.
    pub user: UserId,
    /// Latest Profile name (by hlc), empty until known.
    pub name: String,
    /// hlc of the Profile that set `name`.
    pub name_hlc: u64,
    /// `from_seq` of the admission currently in effect (the lowest one after the latest leave).
    pub admitted_from_seq: u64,
    /// Some(last_seq) once they left (and no later re-admission is known).
    pub left_at: Option<u64>,
    /// Every known admission's `from_seq`, ascending, deduplicated (Genesis counts as 1).
    pub admits: Vec<u64>,
    /// `last_seq` of the author's latest `MemberLeave`.
    pub last_leave: Option<u64>,
    /// Lowest hlc among the events that admitted the user (Genesis for the owner).
    pub admitted_hlc: u64,
    /// Every `MemberAdmit` naming the user, by (hlc, id).
    pub admit_log: Vec<AdmitRecord>,
    /// Every `MemberRemove` naming the user, by (hlc, id), with what cleared each.
    pub removals: Vec<RemovalRecord>,
    /// Every Profile the user signed, by (hlc, seq). `name` is the latest valid one.
    pub profiles: Vec<ProfileRecord>,
}

/// One `MemberAdmit` naming a member.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdmitRecord {
    /// hlc of the admit.
    pub hlc: u64,
    /// Id of the admit.
    pub id: EventId,
    /// Its `from_seq`.
    pub from_seq: u64,
    /// Who signed it.
    pub by: UserId,
    /// The signer's seq for it.
    pub by_seq: u64,
}

/// One `MemberRemove` naming a member.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemovalRecord {
    /// hlc of the removal.
    pub hlc: u64,
    /// Id of the removal.
    pub id: EventId,
    /// The member's last valid seq before it.
    pub last_seq: u64,
    /// Derived: the lowest `from_seq` of a re-admit that clears it (after it by (hlc, id),
    /// `from_seq > last_seq`, signed at a valid seq). None while the member stays removed.
    pub cleared_from: Option<u64>,
}

/// One Profile a member signed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileRecord {
    /// hlc of the Profile.
    pub hlc: u64,
    /// The member's seq for it.
    pub seq: u64,
    /// The name.
    pub name: String,
}

/// Is a seq valid under these removals? (See [`Member::seq_valid`].)
fn valid_under(removals: &[RemovalRecord], seq: u64) -> bool {
    removals.iter().all(|r| r.last_seq >= seq || r.cleared_from.is_some_and(|f| f <= seq))
}

impl Member {
    fn new(user: UserId, admitted_hlc: u64) -> Self {
        Self {
            user,
            name: String::new(),
            name_hlc: 0,
            admitted_from_seq: 0,
            left_at: None,
            admits: Vec::new(),
            last_leave: None,
            admitted_hlc,
            admit_log: Vec::new(),
            removals: Vec::new(),
            profiles: Vec::new(),
        }
    }

    /// Is the user's own event at `seq` valid as far as removals go? Every removal whose
    /// `last_seq` is below `seq` must be cleared by a re-admit with `from_seq <= seq`.
    /// Otherwise the event is void: stored and synced for seq continuity, with no effect.
    pub fn seq_valid(&self, seq: u64) -> bool { valid_under(&self.removals, seq) }

    /// Removed by the owner and not re-admitted since.
    pub fn removed(&self) -> bool { self.removals.iter().any(|r| r.cleared_from.is_none()) }

    /// `name` is the latest Profile (by hlc) signed at a valid seq.
    fn refresh_name(&mut self) {
        match self.profiles.iter().rev().find(|p| valid_under(&self.removals, p.seq)) {
            Some(p) => {
                self.name = p.name.clone();
                self.name_hlc = p.hlc;
            }
            None => {
                self.name = String::new();
                self.name_hlc = 0;
            }
        }
    }

    /// Is the user a member at their own seq `seq`? (Some admission covers `seq` and comes
    /// after their latest leave.)
    pub fn valid_at(&self, seq: u64) -> bool {
        self.admits.iter().any(|&f| f <= seq && self.last_leave.is_none_or(|l| f > l))
    }

    fn add_admit(&mut self, from_seq: u64, hlc: u64) {
        if let Err(pos) = self.admits.binary_search(&from_seq) {
            self.admits.insert(pos, from_seq);
        }
        self.admitted_hlc = self.admitted_hlc.min(hlc);
        self.recompute();
    }

    fn add_leave(&mut self, last_seq: u64) {
        self.last_leave = Some(self.last_leave.map_or(last_seq, |l| l.max(last_seq)));
        self.recompute();
    }

    fn recompute(&mut self) {
        let current = self.admits.iter().copied().find(|&f| self.last_leave.is_none_or(|l| f > l));
        match current {
            Some(f) => {
                self.admitted_from_seq = f;
                self.left_at = None;
            }
            None => self.left_at = self.last_leave,
        }
    }
}

/// An invite as created by `InviteCreate`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InviteState {
    /// The invite's public key.
    pub invite_pk: [u8; 32],
    /// Who created it.
    pub creator: UserId,
    /// Unix ms after which it admits nobody.
    pub expires_ms: u64,
    /// Admission limit, 0 = unlimited (best-effort, enforced by the admitting host).
    pub max_uses: u32,
    /// Accepted admissions that used it.
    pub uses: u32,
    /// Revoked by its creator or the owner.
    pub revoked: bool,
    /// The creator's seq for the InviteCreate. An invite created at a void seq (its creator
    /// had been removed) admits nobody: `ConvState::invite_status` calls it Revoked.
    pub created_seq: u64,
}

/// Why an invite cannot be used to admit someone now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InviteProblem {
    /// No InviteCreate with that key is known.
    Unknown,
    /// Revoked.
    Revoked,
    /// Past `expires_ms`.
    Expired,
    /// `uses >= max_uses`.
    UsedUp,
}

impl InviteProblem {
    /// The `RejectCode` to send to the joiner.
    pub fn reject_code(&self) -> RejectCode {
        match self {
            InviteProblem::Expired => RejectCode::InviteExpired,
            InviteProblem::Unknown | InviteProblem::Revoked | InviteProblem::UsedUp => RejectCode::BadInvite,
        }
    }
    /// Short user-facing explanation.
    pub fn message(&self) -> &'static str {
        match self {
            InviteProblem::Unknown => "This invite is not valid for this server.",
            InviteProblem::Revoked => "This invite was revoked.",
            InviteProblem::Expired => "This invite has expired.",
            InviteProblem::UsedUp => "This invite has been used up.",
        }
    }
}

/// Highest applied seq of an author and the hlc of that event.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuthorHead {
    /// Highest applied seq.
    pub seq: u64,
    /// hlc of that event.
    pub hlc: u64,
}

/// What an applied event changed (for UI notifications).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// Server name set (Genesis or ServerUpdate).
    ServerRenamed,
    /// A channel was created, renamed, moved, deleted or restored.
    Channels,
    /// Membership or a display name changed.
    Members,
    /// A new message.
    Message {
        /// The message.
        id: EventId,
        /// Its channel.
        channel: ChannelId,
    },
    /// A message was deleted.
    Deleted {
        /// The deleted message.
        target: EventId,
    },
    /// An invite was created or revoked.
    Invites,
    /// Nothing visible.
    Nothing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Normal validation of incoming events.
    Live,
    /// Rebuild from storage: seq holes allowed (input may be partial), no clock check.
    Trusted,
    /// Rebuild, last pass: also accept events whose references cannot be proven.
    Forced,
}

/// Derived state of one server or DM.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConvState {
    /// Server id or DM conversation id.
    pub conv: ConvId,
    /// A DM.
    pub is_dm: bool,
    /// DM parties (both), empty for servers.
    pub dm_parties: Vec<UserId>,
    /// Server name.
    pub name: String,
    /// Server owner (Genesis author), None until Genesis is applied.
    pub owner: Option<UserId>,
    /// Channels by id, deleted ones included.
    pub channels: BTreeMap<ChannelId, Channel>,
    /// Members, former ones included.
    pub members: HashMap<UserId, Member>,
    /// Invites by public key.
    pub invites: HashMap<[u8; 32], InviteState>,
    /// Per-author heads.
    pub heads: HashMap<UserId, AuthorHead>,
    /// Message id -> author, for Delete authority checks (messages only; bounded by history).
    pub message_authors: HashMap<EventId, UserId>,
    /// Deleted message ids.
    pub deleted: HashSet<EventId>,
}

impl ConvState {
    /// Empty state for a server whose genesis has not been applied yet.
    pub fn new_server(conv: ConvId) -> Self {
        Self {
            conv,
            is_dm: false,
            dm_parties: Vec::new(),
            name: String::new(),
            owner: None,
            channels: BTreeMap::new(),
            members: HashMap::new(),
            invites: HashMap::new(),
            heads: HashMap::new(),
            message_authors: HashMap::new(),
            deleted: HashSet::new(),
        }
    }

    /// Empty state for the DM between `a` and `b` (conv = `ConvId::dm(a, b)`).
    pub fn new_dm(a: UserId, b: UserId) -> Self {
        let mut s = Self::new_server(ConvId::dm(&a, &b));
        s.is_dm = true;
        s.dm_parties = vec![a, b];
        s
    }

    /// Highest contiguous seq applied for `author` (0 if none).
    pub fn head(&self, author: &UserId) -> u64 { self.heads.get(author).map(|h| h.seq).unwrap_or(0) }

    /// The seq `author`'s next event must carry.
    pub fn next_seq(&self, author: &UserId) -> u64 { self.head(author).saturating_add(1) }

    /// Is `user` currently a member (servers) or a party (DMs)?
    pub fn is_member(&self, user: &UserId) -> bool {
        if self.is_dm {
            return self.dm_parties.contains(user);
        }
        self.members.get(user).map(|m| m.left_at.is_none() && !m.admits.is_empty() && !m.removed()).unwrap_or(false)
    }

    /// Was `user` removed by the owner and not re-admitted since? (Servers only.)
    pub fn is_removed(&self, user: &UserId) -> bool { !self.is_dm && self.members.get(user).is_some_and(Member::removed) }

    /// Is `author`'s event at `seq` valid, as opposed to void because the owner removed the
    /// author before it (see [`Member::seq_valid`])? Display code hides void messages.
    /// Always true in DMs and for authors who were never removed.
    pub fn event_valid(&self, author: &UserId, seq: u64) -> bool {
        self.is_dm || self.members.get(author).is_none_or(|m| m.seq_valid(seq))
    }

    /// The removal of `user` in effect now (the latest uncleared one), if any.
    pub fn active_removal(&self, user: &UserId) -> Option<&RemovalRecord> {
        if self.is_dm {
            return None;
        }
        self.members.get(user)?.removals.iter().rev().find(|r| r.cleared_from.is_none())
    }

    /// Is `user` the server owner?
    pub fn is_owner(&self, user: &UserId) -> bool { self.owner.as_ref() == Some(user) }

    /// The DM channel id of this conversation (meaningful for DMs only).
    pub fn dm_channel(&self) -> ChannelId { dm_channel(&self.conv) }

    /// A channel by id (deleted ones included).
    pub fn channel(&self, id: &ChannelId) -> Option<&Channel> { self.channels.get(id) }

    /// May `author` post a new message to `channel` right now? (Member, existing non-deleted
    /// text channel; in a DM: a party, and the DM channel.) Check this before authoring.
    pub fn can_post(&self, author: &UserId, channel: &ChannelId) -> bool {
        if self.is_dm {
            return self.dm_parties.contains(author) && *channel == self.dm_channel();
        }
        self.is_member(author) && self.channels.get(channel).is_some_and(|c| c.kind == ChannelKind::Text && !c.deleted)
    }

    /// Is `ev` an owner-signed removal of `me` in this server that nothing I hold has
    /// cleared? For a removal heard directly from a host (when it refuses my Hello) that may
    /// not apply yet because my history is behind. `ev` must be verified already.
    pub fn removal_of(&self, ev: &StoredEvent, me: &UserId) -> bool {
        let Body::MemberRemove { user, last_seq } = &ev.event.body else { return false };
        if self.is_dm || ev.event.server != self.conv || user != me || self.owner != Some(ev.event.author) || *user == ev.event.author {
            return false;
        }
        let key = (ev.event.hlc, ev.id);
        !self.members.get(me).is_some_and(|m| m.admit_log.iter().any(|a| (a.hlc, a.id) > key && a.from_seq > *last_seq))
    }

    /// Can the invite admit a joiner at `now_ms`? Hosts check this before authoring a
    /// `MemberAdmit` (revocation and `max_uses` are not part of event validation).
    pub fn invite_status(&self, invite_pk: &[u8; 32], now_ms: u64) -> Result<&InviteState, InviteProblem> {
        let inv = self.invites.get(invite_pk).ok_or(InviteProblem::Unknown)?;
        if inv.revoked || !self.event_valid(&inv.creator, inv.created_seq) {
            Err(InviteProblem::Revoked)
        } else if now_ms > inv.expires_ms {
            Err(InviteProblem::Expired)
        } else if inv.max_uses != 0 && inv.uses >= inv.max_uses {
            Err(InviteProblem::UsedUp)
        } else {
            Ok(inv)
        }
    }

    /// Heads for `Ctl::SyncHave` (sorted by author).
    pub fn sync_heads(&self) -> Heads {
        let mut v: Vec<(UserId, u64)> = self.heads.iter().map(|(u, h)| (*u, h.seq)).collect();
        v.sort();
        Heads(v)
    }

    /// Authors in the order a sync sender should stream them so the receiver can apply most
    /// runs as they arrive: the owner first, then members by when they were admitted, then
    /// anyone else (DM parties) by id. Cross-author references to later authors (an owner
    /// Delete of a newer member's message, an admit using a newer member's invite) still wait
    /// briefly in the receiver's `PendingBuffer`.
    pub fn sync_author_order(&self) -> Vec<UserId> {
        let mut v: Vec<(bool, u64, UserId)> = self
            .heads
            .keys()
            .map(|u| (!self.is_owner(u), self.members.get(u).map_or(u64::MAX, |m| m.admitted_hlc), *u))
            .collect();
        v.sort();
        v.into_iter().map(|(_, _, u)| u).collect()
    }

    /// Validate `ev` (already signature-verified, with id `id`) against current state.
    /// Does not mutate. See the module docs for the rules.
    pub fn check(&self, id: &EventId, ev: &Event, now_ms: u64) -> Verdict { self.verdict(id, ev, now_ms, Mode::Live) }

    /// Validate and, if `Verdict::Accept`, mutate state. Returns the verdict and the change
    /// (`Change::Nothing` unless accepted).
    pub fn apply(&mut self, stored: &StoredEvent, now_ms: u64) -> (Verdict, Change) {
        let verdict = self.verdict(&stored.id, &stored.event, now_ms, Mode::Live);
        if !verdict.is_accept() {
            return (verdict, Change::Nothing);
        }
        let change = self.mutate(&stored.id, &stored.event);
        (Verdict::Accept, change)
    }

    /// Rebuild from stored events (any order: sorts by (author, seq) and resolves cross-author
    /// dependencies to a fixpoint). Used at startup; events from storage are trusted to be
    /// previously validated, but the same rules run so state is derived identically.
    ///
    /// Pass EVERY stored event of the conversation (for each author in `Storage::heads`:
    /// `events_range(conv, author, 1, head)`). `Storage::state_events` alone leaves heads
    /// short and loses Delete authority for old messages. Partial input still works: seq
    /// holes are tolerated and events whose references are missing are applied as trusted
    /// in a last pass. `now_ms` is raised to the newest stored hlc, so clock changes since the
    /// events were accepted do not hold any of them back. Duplicate (author, seq) pairs keep
    /// the lowest id. `is_dm` gives the parties of a DM; `conv` wins if it differs from
    /// `ConvId::dm(a, b)`.
    pub fn rebuild(conv: ConvId, is_dm: Option<(UserId, UserId)>, events: impl IntoIterator<Item = StoredEvent>, now_ms: u64) -> Self {
        let mut state = match is_dm {
            Some((a, b)) => {
                let mut s = Self::new_dm(a, b);
                if s.conv != conv {
                    tracing::warn!(conv = %conv.short(), derived = %s.conv.short(), "rebuild: DM id does not match its parties");
                    s.conv = conv;
                }
                s
            }
            None => Self::new_server(conv),
        };
        let mut list: Vec<StoredEvent> = events.into_iter().collect();
        list.sort_by_key(|e| (e.event.author, e.event.seq, e.id));
        list.dedup_by(|later, earlier| later.event.author == earlier.event.author && later.event.seq == earlier.event.seq);
        let now = list.iter().map(|e| hlc_ms(e.event.hlc)).max().unwrap_or(0).max(now_ms);

        let mut queues: BTreeMap<UserId, VecDeque<StoredEvent>> = BTreeMap::new();
        for e in list {
            queues.entry(e.event.author).or_default().push_back(e);
        }
        // Pass 1: apply each author's run as far as its dependencies allow, to a fixpoint.
        loop {
            let mut progressed = false;
            for q in queues.values_mut() {
                while let Some(e) = q.front() {
                    match state.verdict(&e.id, &e.event, now, Mode::Trusted) {
                        Verdict::Accept => {
                            state.mutate(&e.id, &e.event);
                            q.pop_front();
                            progressed = true;
                        }
                        Verdict::Stale => {
                            q.pop_front();
                        }
                        Verdict::Reject(reason) => {
                            tracing::warn!(id = %e.id.short(), %reason, "rebuild: stored event rejected");
                            q.pop_front();
                        }
                        Verdict::Gap | Verdict::Future => break,
                    }
                }
            }
            queues.retain(|_, q| !q.is_empty());
            if !progressed {
                break;
            }
        }
        // Pass 2: whatever is left refers to events that are not in the input. Trust storage.
        let mut rest: Vec<StoredEvent> = queues.into_values().flatten().collect();
        if !rest.is_empty() {
            tracing::debug!(count = rest.len(), "rebuild: applying events with unprovable references");
            rest.sort_by_key(|e| (e.event.hlc, e.id));
            for e in rest {
                match state.verdict(&e.id, &e.event, now, Mode::Forced) {
                    Verdict::Accept => {
                        state.mutate(&e.id, &e.event);
                    }
                    other => tracing::warn!(id = %e.id.short(), verdict = ?other, "rebuild: stored event dropped"),
                }
            }
        }
        state
    }

    /// Text channels then voice channels, each by (position, name), deleted excluded.
    pub fn sorted_channels(&self) -> Vec<&Channel> {
        let mut v: Vec<&Channel> = self.channels.values().filter(|c| !c.deleted).collect();
        v.sort_by(|a, b| {
            let ka = (a.kind == ChannelKind::Voice, a.position, &a.name, a.id);
            let kb = (b.kind == ChannelKind::Voice, b.position, &b.name, b.id);
            ka.cmp(&kb)
        });
        v
    }

    /// Display name for a user, falling back to the first 8 chars of the z32 id.
    pub fn display_name(&self, user: &UserId) -> String {
        match self.members.get(user) {
            Some(m) if !m.name.is_empty() => m.name.clone(),
            _ => user.to_z32().chars().take(8).collect(),
        }
    }

    // ------------------------------------------------------------------ validation

    fn verdict(&self, id: &EventId, ev: &Event, now_ms: u64, mode: Mode) -> Verdict {
        // 1. version, conversation, seq origin
        if ev.v != PROTOCOL_VERSION {
            return reject(format!("unsupported event version {}", ev.v));
        }
        let genesis = matches!(ev.body, Body::Genesis { .. });
        if let Body::Genesis { owner, .. } = &ev.body {
            if ev.server != ConvId::default() {
                return reject("genesis must carry server = [0; 32]");
            }
            if ConvId::from_event(id) != self.conv {
                return reject("genesis id is not this conversation's id");
            }
            if ev.seq != 1 {
                return reject("genesis must be seq 1");
            }
            if *owner != ev.author {
                return reject("genesis must be authored by its owner");
            }
        } else if ev.server != self.conv {
            return reject("event belongs to another conversation");
        }
        if ev.seq == 0 {
            return reject("seq starts at 1");
        }
        // 2. contiguous seq (self-contained rules 4 and the DM party rule run before a Gap is
        // returned, so malformed events are never buffered)
        let head = self.heads.get(&ev.author).cloned().unwrap_or_default();
        if ev.seq <= head.seq {
            return Verdict::Stale;
        }
        if let Err(reason) = self.check_shape(ev) {
            return Verdict::Reject(reason);
        }
        if self.is_dm && !self.dm_parties.contains(&ev.author) {
            return reject("author is not a party of this DM");
        }
        if mode == Mode::Live && ev.seq != head.seq + 1 {
            return Verdict::Gap;
        }
        // 3. clock
        if head.seq > 0 && ev.hlc <= head.hlc {
            return reject("hlc not after the author's previous event");
        }
        if hlc_ms(ev.hlc) > now_ms.saturating_add(MAX_FUTURE_MS) {
            return Verdict::Future;
        }
        // 8. DMs
        if self.is_dm {
            return self.check_dm(ev, mode);
        }
        if genesis {
            if self.owner.is_some() {
                return reject("genesis already applied");
            }
            return Verdict::Accept;
        }
        // 5. genesis known, owner-only bodies
        let Some(owner) = self.owner else {
            return if mode == Mode::Forced { reject("no genesis") } else { Verdict::Gap };
        };
        let by_owner = ev.author == owner;
        if matches!(ev.body, Body::ChannelCreate { .. } | Body::ChannelUpdate { .. } | Body::ServerUpdate { .. }) && !by_owner {
            return reject("only the owner can change channels or the server");
        }
        if let Body::MemberRemove { user, .. } = &ev.body {
            if !by_owner {
                return reject("only the owner can remove members");
            }
            if *user == owner {
                return reject("the owner cannot be removed");
            }
        }
        // 6. membership
        let member = self.members.get(&ev.author).is_some_and(|m| m.valid_at(ev.seq));
        if !member && mode != Mode::Forced {
            return Verdict::Gap;
        }
        // 7. references
        match &ev.body {
            Body::Genesis { .. }
            | Body::Profile { .. }
            | Body::ChannelCreate { .. }
            | Body::ServerUpdate { .. }
            | Body::MemberLeave { .. }
            | Body::MemberRemove { .. } => Verdict::Accept,
            Body::ChannelUpdate { channel, .. } => {
                if self.channels.contains_key(channel) {
                    Verdict::Accept
                } else {
                    missing(mode, true, "channel")
                }
            }
            Body::Message { channel, .. } => match self.channels.get(channel) {
                Some(c) if c.kind != ChannelKind::Text => reject("messages can only go in text channels"),
                Some(_) => Verdict::Accept,
                None => missing(mode, by_owner, "channel"),
            },
            Body::Delete { target } => match self.message_authors.get(target) {
                Some(a) if *a == ev.author || by_owner => Verdict::Accept,
                Some(_) => reject("only the author or the owner can delete a message"),
                None => missing(mode, !by_owner, "message to delete"),
            },
            Body::InviteCreate { invite_pk, .. } => {
                if self.invites.contains_key(invite_pk) {
                    reject("invite already exists")
                } else {
                    Verdict::Accept
                }
            }
            Body::InviteRevoke { invite_pk } => match self.invites.get(invite_pk) {
                Some(i) if i.creator == ev.author || by_owner => Verdict::Accept,
                Some(_) => reject("only the invite's creator or the owner can revoke it"),
                None => missing(mode, !by_owner, "invite"),
            },
            Body::MemberAdmit { invite_pk, .. } => match self.invites.get(invite_pk) {
                Some(i) if hlc_ms(ev.hlc) > i.expires_ms => reject("invite had expired at admission"),
                Some(_) => Verdict::Accept,
                None => missing(mode, false, "invite"),
            },
        }
    }

    fn check_dm(&self, ev: &Event, mode: Mode) -> Verdict {
        match &ev.body {
            Body::Message { .. } => Verdict::Accept,
            Body::Delete { target } => match self.message_authors.get(target) {
                Some(a) if *a == ev.author => Verdict::Accept,
                Some(_) => reject("only the author can delete a DM message"),
                None => missing(mode, true, "message to delete"),
            },
            _ => reject("DMs carry only messages and deletes"),
        }
    }

    fn check_shape(&self, ev: &Event) -> Result<(), String> {
        if self.is_dm && !matches!(ev.body, Body::Message { .. } | Body::Delete { .. }) {
            return Err("DMs carry only messages and deletes".into());
        }
        match &ev.body {
            Body::Genesis { name, .. } => check_name(name, "server name"),
            Body::Profile { name } => check_name(name, "display name"),
            Body::ChannelCreate { name, .. } => check_name(name, "channel name"),
            Body::ChannelUpdate { name, .. } => name.as_deref().map_or(Ok(()), |n| check_name(n, "channel name")),
            Body::ServerUpdate { name } => name.as_deref().map_or(Ok(()), |n| check_name(n, "server name")),
            Body::Message { channel, text, attachments } => {
                if self.is_dm && *channel != self.dm_channel() {
                    return Err("DM messages must use the DM channel".into());
                }
                validate_message(text, attachments)
            }
            Body::Delete { .. } | Body::InviteCreate { .. } | Body::InviteRevoke { .. } | Body::MemberRemove { .. } => Ok(()),
            Body::MemberAdmit { from_seq, .. } => {
                if *from_seq == 0 {
                    Err("admission from_seq starts at 1".into())
                } else {
                    Ok(())
                }
            }
            Body::MemberLeave { last_seq } => {
                if *last_seq == ev.seq || *last_seq == ev.seq - 1 {
                    Ok(())
                } else {
                    Err("leave last_seq must be this seq or the one before".into())
                }
            }
        }
    }

    // ------------------------------------------------------------------ mutation

    fn mutate(&mut self, id: &EventId, ev: &Event) -> Change {
        let head = self.heads.entry(ev.author).or_default();
        head.seq = ev.seq;
        head.hlc = ev.hlc;
        match &ev.body {
            Body::Genesis { name, owner, .. } => {
                self.name = name.clone();
                self.owner = Some(*owner);
                self.members.entry(*owner).or_insert_with(|| Member::new(*owner, ev.hlc)).add_admit(1, ev.hlc);
                Change::ServerRenamed
            }
            Body::Profile { name } => match self.members.get_mut(&ev.author) {
                Some(m) => {
                    let rec = ProfileRecord { hlc: ev.hlc, seq: ev.seq, name: name.clone() };
                    let pos = m.profiles.partition_point(|p| (p.hlc, p.seq) < (rec.hlc, rec.seq));
                    m.profiles.insert(pos, rec);
                    let before = m.name_hlc;
                    m.refresh_name();
                    if m.name_hlc != before { Change::Members } else { Change::Nothing }
                }
                None => Change::Nothing,
            },
            Body::ChannelCreate { name, kind, position } => {
                self.channels.insert(*id, Channel { id: *id, name: name.clone(), kind: *kind, position: *position, deleted: false });
                Change::Channels
            }
            Body::ChannelUpdate { channel, name, position, deleted } => match self.channels.get_mut(channel) {
                Some(c) => {
                    if let Some(n) = name {
                        c.name = n.clone();
                    }
                    if let Some(p) = position {
                        c.position = *p;
                    }
                    if let Some(d) = deleted {
                        c.deleted = *d;
                    }
                    Change::Channels
                }
                None => Change::Nothing,
            },
            Body::ServerUpdate { name } => match name {
                Some(n) => {
                    self.name = n.clone();
                    Change::ServerRenamed
                }
                None => Change::Nothing,
            },
            Body::Message { channel, .. } => {
                self.message_authors.insert(*id, ev.author);
                Change::Message { id: *id, channel: *channel }
            }
            Body::Delete { target } => {
                self.deleted.insert(*target);
                Change::Deleted { target: *target }
            }
            Body::InviteCreate { invite_pk, expires_ms, max_uses } => {
                self.invites.entry(*invite_pk).or_insert(InviteState {
                    invite_pk: *invite_pk,
                    creator: ev.author,
                    expires_ms: *expires_ms,
                    max_uses: *max_uses,
                    uses: 0,
                    revoked: false,
                    created_seq: ev.seq,
                });
                Change::Invites
            }
            Body::InviteRevoke { invite_pk } => {
                if let Some(i) = self.invites.get_mut(invite_pk) {
                    i.revoked = true;
                }
                Change::Invites
            }
            Body::MemberAdmit { user, invite_pk, from_seq } => {
                if let Some(i) = self.invites.get_mut(invite_pk) {
                    i.uses = i.uses.saturating_add(1);
                }
                let m = self.members.entry(*user).or_insert_with(|| Member::new(*user, ev.hlc));
                m.add_admit(*from_seq, ev.hlc);
                let rec = AdmitRecord { hlc: ev.hlc, id: *id, from_seq: *from_seq, by: ev.author, by_seq: ev.seq };
                let pos = m.admit_log.partition_point(|a| (a.hlc, a.id) < (rec.hlc, rec.id));
                m.admit_log.insert(pos, rec);
                self.refresh_removals();
                Change::Members
            }
            Body::MemberLeave { last_seq } => {
                if let Some(m) = self.members.get_mut(&ev.author) {
                    m.add_leave(*last_seq);
                }
                Change::Members
            }
            Body::MemberRemove { user, last_seq } => {
                let m = self.members.entry(*user).or_insert_with(|| Member::new(*user, u64::MAX));
                let rec = RemovalRecord { hlc: ev.hlc, id: *id, last_seq: *last_seq, cleared_from: None };
                let pos = m.removals.partition_point(|r| (r.hlc, r.id) < (rec.hlc, rec.id));
                m.removals.insert(pos, rec);
                self.refresh_removals();
                Change::Members
            }
        }
    }

    /// Recompute which removals are cleared, and with them which seqs are void, who is
    /// removed and the affected display names. A re-admit clears a removal only if it comes
    /// after it by (hlc, id), has `from_seq > last_seq`, and its signer was valid at the
    /// admit's seq (the owner always is). Signer validity depends on other removals, so this
    /// is a least fixpoint: start with nothing cleared and widen until stable. It is a pure
    /// function of the applied admits and removals, so every delivery order ends the same.
    fn refresh_removals(&mut self) {
        let mut removed: Vec<UserId> = self.members.iter().filter(|(_, m)| !m.removals.is_empty()).map(|(u, _)| *u).collect();
        if removed.is_empty() {
            return;
        }
        removed.sort();
        for u in &removed {
            if let Some(m) = self.members.get_mut(u) {
                for r in &mut m.removals {
                    r.cleared_from = None;
                }
            }
        }
        let owner = self.owner;
        loop {
            let mut changed = false;
            for u in &removed {
                let Some(m) = self.members.get(u) else { continue };
                let mut updates = Vec::new();
                for (i, r) in m.removals.iter().enumerate() {
                    let best = m
                        .admit_log
                        .iter()
                        .filter(|a| (a.hlc, a.id) > (r.hlc, r.id) && a.from_seq > r.last_seq)
                        .filter(|a| Some(a.by) == owner || self.members.get(&a.by).is_none_or(|s| s.seq_valid(a.by_seq)))
                        .map(|a| a.from_seq)
                        .min();
                    if best != r.cleared_from {
                        updates.push((i, best));
                    }
                }
                if !updates.is_empty() {
                    changed = true;
                    if let Some(m) = self.members.get_mut(u) {
                        for (i, best) in updates {
                            m.removals[i].cleared_from = best;
                        }
                    }
                }
            }
            if !changed {
                break;
            }
        }
        for u in &removed {
            if let Some(m) = self.members.get_mut(u) {
                m.refresh_name();
            }
        }
    }
}

fn reject(reason: impl Into<String>) -> Verdict { Verdict::Reject(reason.into()) }

/// A referenced event is not known. `own_chain`: it could only be an earlier event of the
/// author itself, which in live validation is always applied first.
fn missing(mode: Mode, own_chain: bool, what: &str) -> Verdict {
    match mode {
        Mode::Forced => Verdict::Accept,
        Mode::Live if own_chain => Verdict::Reject(format!("unknown {what}")),
        Mode::Live | Mode::Trusted => Verdict::Gap,
    }
}

fn check_name(name: &str, what: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err(format!("{what} is empty"));
    }
    if name.len() > MAX_NAME_CHARS * 4 || name.chars().count() > MAX_NAME_CHARS {
        return Err(format!("{what} is longer than {MAX_NAME_CHARS} characters"));
    }
    if name.chars().any(char::is_control) {
        return Err(format!("{what} contains control characters"));
    }
    Ok(())
}

/// Check a server, channel or display name against the self-contained rules (non-empty after
/// trimming, at most `MAX_NAME_CHARS` chars, no control characters).
pub fn validate_name(name: &str) -> Result<(), String> { check_name(name, "name") }

/// Check message text and attachments against the self-contained rules (at most
/// `MAX_TEXT_CHARS` chars, at most `MAX_ATTACHMENTS`, not both empty, attachment names and
/// types within limits).
pub fn validate_message(text: &str, attachments: &[Attachment]) -> Result<(), String> {
    if text.len() > MAX_TEXT_CHARS * 4 || text.chars().count() > MAX_TEXT_CHARS {
        return Err(format!("message is longer than {MAX_TEXT_CHARS} characters"));
    }
    if attachments.len() > MAX_ATTACHMENTS {
        return Err(format!("more than {MAX_ATTACHMENTS} attachments"));
    }
    if text.is_empty() && attachments.is_empty() {
        return Err("empty message".into());
    }
    for a in attachments {
        if a.name.is_empty() || a.name.chars().count() > MAX_ATTACHMENT_NAME_CHARS {
            return Err("bad attachment name".into());
        }
        if a.mime.chars().count() > MAX_MIME_CHARS {
            return Err("bad attachment type".into());
        }
    }
    Ok(())
}

/// Holds events that arrived early (seq gap, missing reference or future hlc) until they can
/// be applied.
///
/// Use one buffer per conversation. Bounded: beyond `MAX_PENDING_EVENTS` events or
/// `MAX_PENDING_BYTES` (approximate) the oldest inserted events are dropped (sync re-sends
/// anything still missing). Keyed by (author, seq); a second event for the same key is
/// ignored (the first one is kept).
#[derive(Debug, Default)]
pub struct PendingBuffer {
    /// Buffered events by (author, seq). Treat as read-only: mutate through the methods so
    /// the bounds stay accurate.
    pub items: BTreeMap<(UserId, u64), StoredEvent>,
    stamps: HashMap<(UserId, u64), u64>,
    order: BTreeMap<u64, (UserId, u64)>,
    next_stamp: u64,
    bytes: usize,
    limits: PendingLimits,
    per_author: HashMap<UserId, (usize, usize)>,
    non_members: HashSet<(UserId, u64)>,
    /// Events dropped under the limits so far (refused on arrival or evicted).
    dropped: u64,
}

/// Bounds of a [`PendingBuffer`]. The default keeps only the global caps; buffers fed from
/// the network use [`PendingLimits::network`] (or [`PendingLimits::dm`]) so that one author,
/// or a crowd of fresh non-member keys, cannot evict everyone else's early events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingLimits {
    /// Most events held (oldest dropped first).
    pub max_events: usize,
    /// Most bytes (approximate) held (oldest dropped first).
    pub max_bytes: usize,
    /// Most events held per author (over: the new event is dropped).
    pub per_author_events: usize,
    /// Most bytes held per author (over: the new event is dropped).
    pub per_author_bytes: usize,
    /// Most events held whose author is not a member yet, all such authors together.
    pub non_member_events: usize,
    /// A `Gap` event more than this many seqs past its author's head is dropped.
    pub max_ahead: u64,
    /// A `Future` event more than this far ahead of the clock (ms) is dropped.
    pub max_future_ms: u64,
}

impl Default for PendingLimits {
    fn default() -> Self {
        Self {
            max_events: MAX_PENDING_EVENTS,
            max_bytes: MAX_PENDING_BYTES,
            per_author_events: usize::MAX,
            per_author_bytes: usize::MAX,
            non_member_events: usize::MAX,
            max_ahead: u64::MAX,
            max_future_ms: u64::MAX,
        }
    }
}

impl PendingLimits {
    /// For server replicas fed by peers. Generous enough for honest out-of-order sync (an
    /// author's run waiting on another author's event), tight enough that no single source
    /// fills the buffer.
    pub fn network() -> Self {
        Self {
            per_author_events: 2048,
            per_author_bytes: 8 * 1024 * 1024,
            non_member_events: 256,
            max_ahead: 4096,
            max_future_ms: 60 * 60 * 1000,
            ..Self::default()
        }
    }

    /// For DM replicas (two parties, one contiguous log each).
    pub fn dm() -> Self {
        Self {
            max_events: 64,
            max_bytes: 256 * 1024,
            per_author_events: 64,
            per_author_bytes: 256 * 1024,
            non_member_events: 0,
            max_ahead: 64,
            max_future_ms: 60 * 60 * 1000,
        }
    }
}

impl PendingBuffer {
    /// Empty buffer.
    pub fn new() -> Self { Self::default() }

    /// Empty buffer with these bounds.
    pub fn with_limits(limits: PendingLimits) -> Self { Self { limits, ..Self::default() } }

    /// Buffer an event (typically one whose verdict was `Gap` or `Future`).
    pub fn insert(&mut self, ev: StoredEvent) { self.insert_as(ev, false); }

    fn insert_as(&mut self, ev: StoredEvent, non_member: bool) {
        let key = (ev.event.author, ev.event.seq);
        if self.items.contains_key(&key) {
            return;
        }
        let stamp = self.next_stamp;
        self.next_stamp += 1;
        let size = ev.approx_size();
        self.bytes += size;
        let a = self.per_author.entry(key.0).or_default();
        a.0 += 1;
        a.1 += size;
        if non_member {
            self.non_members.insert(key);
        }
        self.items.insert(key, ev);
        self.stamps.insert(key, stamp);
        self.order.insert(stamp, key);
        while self.items.len() > self.limits.max_events || self.bytes > self.limits.max_bytes {
            let Some((_, oldest)) = self.order.pop_first() else { break };
            self.stamps.remove(&oldest);
            if self.remove_item(&oldest).is_some() {
                self.dropped += 1;
                tracing::debug!(author = %oldest.0.short(), seq = oldest.1, "pending buffer full: dropped oldest event");
            }
        }
    }

    /// May `ev` (verdict `verdict` against `state`) be buffered under the limits?
    fn admits(&self, state: &ConvState, ev: &StoredEvent, verdict: &Verdict, now_ms: u64) -> Option<bool> {
        let l = &self.limits;
        let author = ev.event.author;
        if hlc_ms(ev.event.hlc) > now_ms.saturating_add(l.max_future_ms) {
            return None;
        }
        if *verdict == Verdict::Gap && ev.event.seq > state.head(&author).saturating_add(l.max_ahead) {
            return None;
        }
        let (n, b) = self.per_author.get(&author).copied().unwrap_or_default();
        if n >= l.per_author_events || b.saturating_add(ev.approx_size()) > l.per_author_bytes {
            return None;
        }
        let non_member = !state.is_member(&author);
        if non_member && self.non_members.len() >= l.non_member_events {
            return None;
        }
        Some(non_member)
    }

    fn remove_item(&mut self, key: &(UserId, u64)) -> Option<StoredEvent> {
        let ev = self.items.remove(key)?;
        let size = ev.approx_size();
        self.bytes = self.bytes.saturating_sub(size);
        if let Some(a) = self.per_author.get_mut(&key.0) {
            a.0 = a.0.saturating_sub(1);
            a.1 = a.1.saturating_sub(size);
            if a.0 == 0 {
                self.per_author.remove(&key.0);
            }
        }
        self.non_members.remove(key);
        Some(ev)
    }

    /// Remove and return every buffered event that is now acceptable, applying each one to
    /// `state` (each may unblock the next). The caller must persist the returned events in
    /// the returned order (that is a valid dependency order) and emit the changes. Buffered
    /// events that turned `Stale` or `Reject` are discarded.
    pub fn drain_ready(&mut self, state: &mut ConvState, now_ms: u64) -> Vec<(StoredEvent, Change)> {
        let mut out = Vec::new();
        loop {
            let mut progressed = false;
            let mut cursor: Option<UserId> = None;
            while let Some(author) = self.next_author(cursor) {
                cursor = Some(author);
                self.drop_stale(&author, state.head(&author));
                loop {
                    let key = (author, state.next_seq(&author));
                    let Some(ev) = self.items.get(&key) else { break };
                    let (verdict, change) = state.apply(ev, now_ms);
                    match verdict {
                        Verdict::Accept => {
                            if let Some(ev) = self.take(&key) {
                                out.push((ev, change));
                            }
                            progressed = true;
                        }
                        Verdict::Stale => {
                            self.take(&key);
                        }
                        Verdict::Reject(reason) => {
                            tracing::debug!(author = %author.short(), seq = key.1, %reason, "pending event rejected");
                            self.take(&key);
                            break;
                        }
                        Verdict::Gap | Verdict::Future => break,
                    }
                }
            }
            if !progressed {
                break;
            }
        }
        out
    }

    /// Validate and apply one incoming event, buffering it if it is early, then drain
    /// whatever it unblocked. Returns its verdict and every event applied, in order (the
    /// incoming one first if accepted). The caller persists the applied events in order.
    pub fn ingest(&mut self, state: &mut ConvState, ev: StoredEvent, now_ms: u64) -> (Verdict, Vec<(StoredEvent, Change)>) {
        let (verdict, change) = state.apply(&ev, now_ms);
        match verdict {
            Verdict::Accept => {
                let mut out = vec![(ev, change)];
                out.extend(self.drain_ready(state, now_ms));
                (Verdict::Accept, out)
            }
            Verdict::Gap | Verdict::Future => {
                match self.admits(state, &ev, &verdict, now_ms) {
                    Some(non_member) => self.insert_as(ev, non_member),
                    None => {
                        self.dropped += 1;
                        tracing::debug!(author = %ev.event.author.short(), seq = ev.event.seq, "pending limits: early event dropped");
                    }
                }
                (verdict, Vec::new())
            }
            Verdict::Stale | Verdict::Reject(_) => (verdict, Vec::new()),
        }
    }

    /// Is an event for (author, seq) buffered?
    pub fn contains(&self, author: &UserId, seq: u64) -> bool { self.items.contains_key(&(*author, seq)) }

    /// Number of buffered events.
    pub fn len(&self) -> usize { self.items.len() }
    /// True if nothing is buffered.
    pub fn is_empty(&self) -> bool { self.items.is_empty() }
    /// Approximate bytes held.
    pub fn bytes(&self) -> usize { self.bytes }
    /// Early events dropped under the limits so far (refused on arrival or evicted). A sync
    /// receiver compares it before and after a sync: anything dropped must be asked for again.
    pub fn dropped(&self) -> u64 { self.dropped }
    /// Drop everything.
    pub fn clear(&mut self) {
        self.items.clear();
        self.stamps.clear();
        self.order.clear();
        self.bytes = 0;
        self.per_author.clear();
        self.non_members.clear();
    }

    fn next_author(&self, after: Option<UserId>) -> Option<UserId> {
        let first = match after {
            None => self.items.keys().next(),
            Some(a) => self.items.range((Bound::Excluded((a, u64::MAX)), Bound::Unbounded)).next().map(|(k, _)| k),
        };
        first.map(|(u, _)| *u)
    }

    fn drop_stale(&mut self, author: &UserId, head: u64) {
        let stale: Vec<(UserId, u64)> = self.items.range((*author, 0)..=(*author, head)).map(|(k, _)| *k).collect();
        for k in stale {
            self.take(&k);
        }
    }

    fn take(&mut self, key: &(UserId, u64)) -> Option<StoredEvent> {
        let ev = self.remove_item(key)?;
        if let Some(stamp) = self.stamps.remove(key) {
            self.order.remove(&stamp);
        }
        Some(ev)
    }
}

#[cfg(test)]
mod tests;
