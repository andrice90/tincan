//! 32-byte identifiers. All serialize as raw 32-byte arrays (postcard) and
//! display as lowercase z-base-32 (UserId) or lowercase hex (everything else).

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! id32 {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default)]
        pub struct $name(pub [u8; 32]);

        impl $name {
            /// Wrap raw bytes.
            pub const fn from_bytes(b: [u8; 32]) -> Self { Self(b) }
            /// The raw bytes.
            pub fn as_bytes(&self) -> &[u8; 32] { &self.0 }
            /// Lowercase hex, 64 chars.
            pub fn to_hex(&self) -> String { data_encoding::HEXLOWER.encode(&self.0) }
            /// Parse 64 hex chars (either case). None on bad length or characters.
            pub fn from_hex(s: &str) -> Option<Self> {
                let v = data_encoding::HEXLOWER_PERMISSIVE.decode(s.trim().as_bytes()).ok()?;
                Some(Self(v.try_into().ok()?))
            }
            /// First 5 bytes as hex, for logs.
            pub fn short(&self) -> String { data_encoding::HEXLOWER.encode(&self.0[..5]) }
        }
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.short())
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.to_hex()) }
        }
    };
}

id32!(
    /// BLAKE3 of the postcard-encoded `Event`.
    EventId
);
id32!(
    /// A conversation: a server (ServerId) or a DM pair (see `ConvId::dm`).
    ConvId
);
id32!(
    /// BLAKE3 content hash of a blob (same bytes as `iroh_blobs::Hash`).
    BlobHash
);

/// A server is identified by the EventId of its Genesis event.
pub type ServerId = ConvId;
/// A channel is identified by the EventId of its ChannelCreate event.
/// In a DM the only channel is `state::dm_channel(conv)`.
pub type ChannelId = EventId;

impl ConvId {
    /// The conversation id of the DM between `a` and `b` (order independent):
    /// `blake3("tincan dm" || min(a, b) || max(a, b))`.
    pub fn dm(a: &UserId, b: &UserId) -> ConvId {
        let (x, y) = if a <= b { (a, b) } else { (b, a) };
        let mut h = blake3::Hasher::new();
        h.update(b"tincan dm");
        h.update(&x.0);
        h.update(&y.0);
        ConvId(*h.finalize().as_bytes())
    }
    /// The server id named by a Genesis event id.
    pub fn from_event(id: &EventId) -> ConvId { ConvId(id.0) }
}

/// A user: the ed25519 public key that is also the iroh EndpointId.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default)]
pub struct UserId(pub [u8; 32]);

impl UserId {
    /// Wrap raw bytes (not checked to be a valid curve point).
    pub const fn from_bytes(b: [u8; 32]) -> Self { Self(b) }
    /// The raw bytes.
    pub fn as_bytes(&self) -> &[u8; 32] { &self.0 }
    /// The ed25519 public key. `CoreError::BadKey` if the bytes are not a valid point.
    pub fn public_key(&self) -> crate::Result<iroh_base::PublicKey> {
        iroh_base::PublicKey::from_bytes(&self.0).map_err(|_| crate::CoreError::BadKey)
    }
    /// From an iroh public key (EndpointId).
    pub fn from_public_key(pk: &iroh_base::PublicKey) -> Self { Self(*pk.as_bytes()) }
    /// z-base-32, 52 chars. This is what users copy and paste.
    /// Falls back to hex for bytes that are not a valid key.
    pub fn to_z32(&self) -> String {
        match self.public_key() {
            Ok(pk) => pk.to_z32(),
            Err(_) => data_encoding::HEXLOWER.encode(&self.0),
        }
    }
    /// Parse a z-base-32 id (surrounding whitespace and uppercase accepted).
    pub fn from_z32(s: &str) -> Option<Self> {
        let lower = s.trim().to_ascii_lowercase();
        iroh_base::PublicKey::from_z32(&lower).ok().map(|pk| Self::from_public_key(&pk))
    }
    /// Lowercase hex, 64 chars.
    pub fn to_hex(&self) -> String { data_encoding::HEXLOWER.encode(&self.0) }
    /// Parse 64 hex chars (either case).
    pub fn from_hex(s: &str) -> Option<Self> {
        let v = data_encoding::HEXLOWER_PERMISSIVE.decode(s.trim().as_bytes()).ok()?;
        Some(Self(v.try_into().ok()?))
    }
    /// First 5 bytes as hex, for logs.
    pub fn short(&self) -> String { data_encoding::HEXLOWER.encode(&self.0[..5]) }
}
impl fmt::Debug for UserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "UserId({})", self.short()) }
}
impl fmt::Display for UserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.to_z32()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::Identity;

    #[test]
    fn hex_round_trip_and_case() {
        let id = EventId([0xab; 32]);
        let hex = id.to_hex();
        assert_eq!(hex.len(), 64);
        assert_eq!(EventId::from_hex(&hex), Some(id));
        assert_eq!(EventId::from_hex(&hex.to_uppercase()), Some(id));
        assert_eq!(EventId::from_hex(&format!("  {hex}\n")), Some(id));
        assert_eq!(EventId::from_hex("abcd"), None);
        assert_eq!(EventId::from_hex(&"zz".repeat(32)), None);
        assert_eq!(id.short(), "ababababab");
        assert_eq!(format!("{id}"), hex);
        assert_eq!(format!("{id:?}"), "EventId(ababababab)");
    }

    #[test]
    fn dm_id_is_order_independent() {
        let a = Identity::generate().user();
        let b = Identity::generate().user();
        assert_eq!(ConvId::dm(&a, &b), ConvId::dm(&b, &a));
        assert_ne!(ConvId::dm(&a, &b), ConvId::dm(&a, &a));
    }

    #[test]
    fn user_z32_round_trip() {
        let u = Identity::generate().user();
        let z = u.to_z32();
        assert_eq!(z.len(), 52);
        assert_eq!(UserId::from_z32(&z), Some(u));
        assert_eq!(UserId::from_z32(&format!(" {} ", z.to_uppercase())), Some(u));
        assert_eq!(UserId::from_z32("not-an-id"), None);
        assert_eq!(UserId::from_hex(&u.to_hex()), Some(u));
        assert_eq!(format!("{u}"), z);
        assert!(u.public_key().is_ok());
    }

    #[test]
    fn ids_serialize_as_32_raw_bytes() {
        let id = ConvId([7; 32]);
        let bytes = postcard::to_stdvec(&id).unwrap();
        assert_eq!(bytes.len(), 32);
        let back: ConvId = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back, id);
    }
}
