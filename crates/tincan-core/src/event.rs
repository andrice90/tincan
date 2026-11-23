//! Signed, replicated events. A conversation's state is the set of its events.

use serde::{Deserialize, Serialize};

use crate::keys::{Identity, Sig64, verify};
use crate::{BlobHash, ChannelId, ConvId, CoreError, EventId, Result, UserId};

const SIG_CONTEXT: &[u8] = b"tincan-ev1";

/// One replicated event. Its id is `blake3(postcard(Event))`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// Protocol version (`PROTOCOL_VERSION`).
    pub v: u8,
    /// Server id, or DM conversation id. `ConvId([0; 32])` in a Genesis event.
    pub server: ConvId,
    /// Signer of the event.
    pub author: UserId,
    /// Per author per conversation, contiguous from 1.
    pub seq: u64,
    /// Hybrid logical clock, see `hlc`. Strictly increasing per author.
    pub hlc: u64,
    /// What the event does.
    pub body: Body,
}

/// Text or voice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ChannelKind {
    /// Text channel (messages).
    Text,
    /// Voice room (no messages).
    Voice,
}

/// A file attached to a message (transferred as an iroh blob).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    /// BLAKE3 hash of the file.
    pub hash: BlobHash,
    /// Size in bytes.
    pub size: u64,
    /// File name shown to users (at most `state::MAX_ATTACHMENT_NAME_CHARS`).
    pub name: String,
    /// MIME type (at most `state::MAX_MIME_CHARS`).
    pub mime: String,
}

/// Event bodies. Append new variants only at the end (postcard encodes the index).
/// The validation rules are in `state` (module docs).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Body {
    /// First event of a server, authored by the owner with seq 1, with `Event.server` set to
    /// `ConvId([0; 32])`. Its EventId is the ServerId used by every later event.
    Genesis {
        /// Server name.
        name: String,
        /// The owner (must equal the author).
        owner: UserId,
        /// Random, so two servers with the same name and owner differ.
        nonce: [u8; 16],
    },
    /// Display name of the author. Latest by (hlc, id) wins.
    Profile {
        /// Display name.
        name: String,
    },
    /// Owner only. The channel id is this event's id.
    ChannelCreate {
        /// Channel name.
        name: String,
        /// Text or voice.
        kind: ChannelKind,
        /// Sort position within its kind.
        position: u32,
    },
    /// Owner only. `None` fields are unchanged.
    ChannelUpdate {
        /// The channel (its ChannelCreate id).
        channel: ChannelId,
        /// New name.
        name: Option<String>,
        /// New position.
        position: Option<u32>,
        /// Delete (true) or restore (false).
        deleted: Option<bool>,
    },
    /// Owner only.
    ServerUpdate {
        /// New server name.
        name: Option<String>,
    },
    /// A chat message. In a DM, `channel` is `state::dm_channel(conv)`.
    Message {
        /// Text channel the message is in.
        channel: ChannelId,
        /// Text (at most `state::MAX_TEXT_CHARS` chars).
        text: String,
        /// At most `state::MAX_ATTACHMENTS` files.
        attachments: Vec<Attachment>,
    },
    /// By the author of `target`, or the owner. Only messages can be deleted.
    Delete {
        /// The message to delete.
        target: EventId,
    },
    /// Any member.
    InviteCreate {
        /// Public key of the invite (`invite::invite_secret(seed, server).public()`).
        invite_pk: [u8; 32],
        /// Unix ms after which the invite no longer admits anyone (`u64::MAX` = never).
        expires_ms: u64,
        /// Maximum admissions, enforced best-effort by the admitting host. 0 = unlimited.
        max_uses: u32,
    },
    /// The invite's creator or the owner.
    InviteRevoke {
        /// The invite to revoke.
        invite_pk: [u8; 32],
    },
    /// Authored by the host that admitted `user`. `from_seq` is the joiner's first seq after
    /// (re)joining: 1 for a first join, the next seq after their `MemberLeave` for a re-join.
    MemberAdmit {
        /// The joiner.
        user: UserId,
        /// The invite used.
        invite_pk: [u8; 32],
        /// First seq of the joiner that this admission covers.
        from_seq: u64,
    },
    /// Author leaves; events with seq > last_seq are invalid (until a later re-admit).
    MemberLeave {
        /// The author's last valid seq: this event's seq or the one before.
        last_seq: u64,
    },
}

impl Body {
    /// Short tag stored in SQLite for filtering.
    pub fn kind_tag(&self) -> &'static str {
        match self {
            Body::Genesis { .. } => "genesis",
            Body::Profile { .. } => "profile",
            Body::ChannelCreate { .. } => "channel_create",
            Body::ChannelUpdate { .. } => "channel_update",
            Body::ServerUpdate { .. } => "server_update",
            Body::Message { .. } => "message",
            Body::Delete { .. } => "delete",
            Body::InviteCreate { .. } => "invite_create",
            Body::InviteRevoke { .. } => "invite_revoke",
            Body::MemberAdmit { .. } => "member_admit",
            Body::MemberLeave { .. } => "member_leave",
        }
    }
    /// Channel the event belongs to, if any (messages).
    pub fn channel(&self) -> Option<ChannelId> {
        match self {
            Body::Message { channel, .. } => Some(*channel),
            _ => None,
        }
    }
    /// True for events that shape server state (everything except messages and deletes).
    pub fn is_state(&self) -> bool { !matches!(self, Body::Message { .. } | Body::Delete { .. }) }
}

/// An event as it travels and is stored: the exact encoded bytes plus the signature.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedEvent {
    /// postcard(Event)
    pub bytes: Vec<u8>,
    /// ed25519 by `Event.author` over `"tincan-ev1" || bytes`.
    pub sig: Sig64,
}

impl std::fmt::Debug for SignedEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SignedEvent({} bytes, id {})", self.bytes.len(), self.id().short())
    }
}

impl SignedEvent {
    /// Encode and sign. `CoreError::InvalidEvent` if `ev.author != identity.user()`.
    pub fn sign(identity: &Identity, ev: &Event) -> Result<Self> {
        if ev.author != identity.user() {
            return Err(CoreError::InvalidEvent("event author must be the signer".into()));
        }
        let bytes = postcard::to_stdvec(ev)?;
        let sig = identity.sign(&sig_message(&bytes));
        Ok(Self { bytes, sig })
    }
    /// BLAKE3 of the encoded event.
    pub fn id(&self) -> EventId { EventId(*blake3::hash(&self.bytes).as_bytes()) }
    /// Decode without verifying. Trailing bytes after the event are an error, so one
    /// event has exactly one encoding (and one id).
    pub fn decode(&self) -> Result<Event> {
        let (ev, rest) = postcard::take_from_bytes::<Event>(&self.bytes)?;
        if !rest.is_empty() {
            return Err(CoreError::InvalidEvent("trailing bytes after event".into()));
        }
        Ok(ev)
    }
    /// Decode and verify the author's signature.
    pub fn verify(&self) -> Result<Event> {
        let ev = self.decode()?;
        verify(&ev.author, &sig_message(&self.bytes), &self.sig)?;
        Ok(ev)
    }
}

fn sig_message(bytes: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(SIG_CONTEXT.len() + bytes.len());
    m.extend_from_slice(SIG_CONTEXT);
    m.extend_from_slice(bytes);
    m
}

/// A verified event with its id, as kept in storage and passed around.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredEvent {
    /// `signed.id()`.
    pub id: EventId,
    /// The bytes and signature exactly as received (needed to forward and sync).
    pub signed: SignedEvent,
    /// The decoded event.
    pub event: Event,
    /// Set when a valid Delete targets this event. Display code hides the body; storage must
    /// keep `signed` intact so the event can still be synced (seqs are contiguous).
    pub deleted: bool,
}

impl StoredEvent {
    /// Verify and wrap.
    pub fn from_signed(signed: SignedEvent) -> Result<Self> {
        let event = signed.verify()?;
        Ok(Self { id: signed.id(), signed, event, deleted: false })
    }
    /// Display order key.
    pub fn order_key(&self) -> (u64, EventId) { (self.event.hlc, self.id) }
    /// The conversation this event belongs to. Same as `event.server`, except for a Genesis
    /// event (which carries `ConvId([0; 32])`): its conversation is its own id. Storage must
    /// index by this, not by `event.server`.
    pub fn conv(&self) -> ConvId {
        match self.event.body {
            Body::Genesis { .. } => ConvId::from_event(&self.id),
            _ => self.event.server,
        }
    }
    /// Approximate heap footprint in bytes (used to bound buffers).
    pub fn approx_size(&self) -> usize {
        // Encoded bytes are held twice (signed bytes plus decoded strings) plus fixed overhead.
        2 * self.signed.bytes.len() + 256
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PROTOCOL_VERSION;

    fn sample(author: UserId) -> Event {
        Event {
            v: PROTOCOL_VERSION,
            server: ConvId([1; 32]),
            author,
            seq: 1,
            hlc: 42,
            body: Body::Message {
                channel: EventId([2; 32]),
                text: "hi".into(),
                attachments: vec![Attachment { hash: BlobHash([3; 32]), size: 5, name: "a.txt".into(), mime: "text/plain".into() }],
            },
        }
    }

    #[test]
    fn sign_verify_round_trip() {
        let id = Identity::generate();
        let ev = sample(id.user());
        let s = SignedEvent::sign(&id, &ev).unwrap();
        assert_eq!(s.verify().unwrap(), ev);
        assert_eq!(s.decode().unwrap(), ev);
        assert_eq!(s.id(), EventId(*blake3::hash(&postcard::to_stdvec(&ev).unwrap()).as_bytes()));
        let st = StoredEvent::from_signed(s.clone()).unwrap();
        assert_eq!(st.id, s.id());
        assert_eq!(st.conv(), ConvId([1; 32]));
        assert_eq!(st.order_key(), (42, s.id()));
        assert!(!st.deleted);
        assert!(st.approx_size() > s.bytes.len());
        // SignedEvent itself round-trips through postcard
        let wire = postcard::to_stdvec(&s).unwrap();
        let back: SignedEvent = postcard::from_bytes(&wire).unwrap();
        assert_eq!(back, s);
        assert!(format!("{s:?}").starts_with("SignedEvent("));
    }

    #[test]
    fn sign_requires_author_to_be_signer() {
        let id = Identity::generate();
        let other = Identity::generate();
        let ev = sample(other.user());
        assert!(matches!(SignedEvent::sign(&id, &ev), Err(CoreError::InvalidEvent(_))));
    }

    #[test]
    fn tampered_bytes_fail_verification() {
        let id = Identity::generate();
        let s = SignedEvent::sign(&id, &sample(id.user())).unwrap();
        // flip every byte position in turn: each must fail to decode or fail to verify
        for i in 0..s.bytes.len() {
            let mut t = s.clone();
            t.bytes[i] ^= 0x01;
            assert!(t.verify().is_err(), "tamper at byte {i} not detected");
            assert!(StoredEvent::from_signed(t).is_err());
        }
        let mut t = s.clone();
        t.sig.0[0] ^= 0x80;
        assert!(matches!(t.verify(), Err(CoreError::BadSignature)));
    }

    #[test]
    fn re_signing_by_another_key_fails() {
        let id = Identity::generate();
        let mallory = Identity::generate();
        let s = SignedEvent::sign(&id, &sample(id.user())).unwrap();
        let forged = SignedEvent { bytes: s.bytes.clone(), sig: mallory.sign(&sig_message(&s.bytes)) };
        assert!(forged.verify().is_err());
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let id = Identity::generate();
        let ev = sample(id.user());
        let mut bytes = postcard::to_stdvec(&ev).unwrap();
        bytes.push(0);
        let sig = id.sign(&sig_message(&bytes));
        let s = SignedEvent { bytes, sig };
        assert!(s.decode().is_err());
        assert!(s.verify().is_err());
    }

    #[test]
    fn genesis_conv_is_its_own_id() {
        let id = Identity::generate();
        let ev = Event {
            v: PROTOCOL_VERSION,
            server: ConvId::default(),
            author: id.user(),
            seq: 1,
            hlc: 1,
            body: Body::Genesis { name: "s".into(), owner: id.user(), nonce: [0; 16] },
        };
        let st = StoredEvent::from_signed(SignedEvent::sign(&id, &ev).unwrap()).unwrap();
        assert_eq!(st.conv(), ConvId::from_event(&st.id));
        assert_ne!(st.conv(), ConvId::default());
    }

    #[test]
    fn body_helpers() {
        let m = Body::Message { channel: EventId([2; 32]), text: String::new(), attachments: vec![] };
        assert_eq!(m.channel(), Some(EventId([2; 32])));
        assert!(!m.is_state());
        assert!(!Body::Delete { target: EventId([0; 32]) }.is_state());
        assert!(Body::Profile { name: "x".into() }.is_state());
        assert_eq!(Body::Profile { name: "x".into() }.channel(), None);
        let tags = [
            Body::Genesis { name: String::new(), owner: UserId::default(), nonce: [0; 16] }.kind_tag(),
            Body::Profile { name: String::new() }.kind_tag(),
            Body::ChannelCreate { name: String::new(), kind: ChannelKind::Text, position: 0 }.kind_tag(),
            Body::ChannelUpdate { channel: EventId::default(), name: None, position: None, deleted: None }.kind_tag(),
            Body::ServerUpdate { name: None }.kind_tag(),
            m.kind_tag(),
            Body::Delete { target: EventId::default() }.kind_tag(),
            Body::InviteCreate { invite_pk: [0; 32], expires_ms: 0, max_uses: 0 }.kind_tag(),
            Body::InviteRevoke { invite_pk: [0; 32] }.kind_tag(),
            Body::MemberAdmit { user: UserId::default(), invite_pk: [0; 32], from_seq: 1 }.kind_tag(),
            Body::MemberLeave { last_seq: 0 }.kind_tag(),
        ];
        let unique: std::collections::HashSet<_> = tags.iter().collect();
        assert_eq!(unique.len(), tags.len());
    }

    #[test]
    fn body_variant_indices_are_stable() {
        // postcard encodes the variant index first; these must never change.
        let idx = |b: &Body| postcard::to_stdvec(b).unwrap()[0];
        assert_eq!(idx(&Body::Genesis { name: String::new(), owner: UserId::default(), nonce: [0; 16] }), 0);
        assert_eq!(idx(&Body::Message { channel: EventId::default(), text: String::new(), attachments: vec![] }), 5);
        assert_eq!(idx(&Body::MemberLeave { last_seq: 0 }), 10);
    }
}
