//! Conversions between Tincan ids/hints and iroh types.

use iroh::{EndpointAddr, EndpointId, RelayUrl, TransportAddr};
use tincan_core::{BlobHash, UserId, wire::AddrHint};
use tracing::debug;

/// The iroh endpoint id of a user, if the bytes are a valid ed25519 point.
pub(crate) fn endpoint_id(user: &UserId) -> Option<EndpointId> {
    EndpointId::from_bytes(user.as_bytes()).ok()
}

/// The Tincan user id of an iroh endpoint.
pub(crate) fn user_id(id: &EndpointId) -> UserId {
    UserId::from_bytes(*id.as_bytes())
}

/// Address info for the endpoint's lookup, or `None` when the hint carries nothing usable.
pub(crate) fn endpoint_addr(id: EndpointId, hint: &AddrHint) -> Option<EndpointAddr> {
    let mut addrs: Vec<TransportAddr> = hint.direct.iter().copied().map(TransportAddr::Ip).collect();
    if let Some(relay) = hint.relay.as_deref().filter(|r| !r.is_empty()) {
        match relay.parse::<RelayUrl>() {
            Ok(url) => addrs.push(TransportAddr::Relay(url)),
            Err(e) => debug!(peer = %id.fmt_short(), relay, "ignoring unparsable relay url: {e}"),
        }
    }
    if addrs.is_empty() {
        return None;
    }
    Some(EndpointAddr::from_parts(id, addrs))
}

/// iroh-blobs hash of a Tincan blob hash (same 32 bytes).
pub(crate) fn blobs_hash(hash: &BlobHash) -> iroh_blobs::Hash {
    iroh_blobs::Hash::from_bytes(*hash.as_bytes())
}

/// Tincan blob hash of an iroh-blobs hash.
pub(crate) fn blob_hash(hash: &iroh_blobs::Hash) -> BlobHash {
    BlobHash::from_bytes(*hash.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hint_conversion() {
        let id = iroh::SecretKey::generate().public();
        assert!(endpoint_addr(id, &AddrHint::default()).is_none());
        let hint = AddrHint {
            relay: Some("https://relay.example.com".into()),
            direct: vec!["127.0.0.1:4433".parse().expect("valid socket addr")],
        };
        let addr = endpoint_addr(id, &hint).expect("usable hint");
        assert_eq!(addr.id, id);
        assert_eq!(addr.addrs.len(), 2);
        let bad = AddrHint { relay: Some("not a url".into()), direct: vec![] };
        assert!(endpoint_addr(id, &bad).is_none());
        let user = user_id(&id);
        assert_eq!(endpoint_id(&user), Some(id));
        let h = BlobHash::from_bytes([7; 32]);
        assert_eq!(blob_hash(&blobs_hash(&h)), h);
    }
}
