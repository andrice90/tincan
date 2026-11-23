//! Address hints (`tincan_core::wire::AddrHint`) and iroh addresses / paths.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use iroh::endpoint::Connection;
use iroh::{Endpoint, EndpointAddr, RelayUrl, TransportAddr};
use tincan_core::UserId;
use tincan_core::wire::AddrHint;

/// Most direct addresses kept in one hint.
const MAX_DIRECT: usize = 8;

/// iroh address for `user` with the hint's relay and direct addresses.
/// None if `user` is not a valid public key.
pub(crate) fn endpoint_addr(user: &UserId, hint: Option<&AddrHint>) -> Option<EndpointAddr> {
    let pk = user.public_key().ok()?;
    let mut addr = EndpointAddr::new(pk);
    if let Some(h) = hint {
        if let Some(relay) = h.relay.as_deref().and_then(|r| r.parse::<RelayUrl>().ok()) {
            addr = addr.with_relay_url(relay);
        }
        for d in &h.direct {
            addr = addr.with_ip_addr(*d);
        }
    }
    Some(addr)
}

/// Hint from an iroh address (first relay, IP addresses).
pub(crate) fn hint_from_addr(addr: &EndpointAddr) -> AddrHint {
    let mut hint = AddrHint::default();
    for a in &addr.addrs {
        match a {
            TransportAddr::Relay(url) if hint.relay.is_none() => hint.relay = Some(url.to_string()),
            TransportAddr::Ip(sa) if hint.direct.len() < MAX_DIRECT && !hint.direct.contains(sa) => hint.direct.push(*sa),
            _ => {}
        }
    }
    hint
}

/// This endpoint's own hint: its current address, or (before the address watcher has
/// anything) its bound sockets with unspecified IPs replaced by loopback.
pub(crate) fn my_hint(ep: &Endpoint) -> AddrHint {
    let mut hint = hint_from_addr(&ep.addr());
    if hint.direct.is_empty() {
        for s in ep.bound_sockets() {
            let ip = match s.ip() {
                IpAddr::V4(v4) if v4.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V6(v6) if v6.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
                other => other,
            };
            let sa = SocketAddr::new(ip, s.port());
            if hint.direct.len() < MAX_DIRECT && !hint.direct.contains(&sa) {
                hint.direct.push(sa);
            }
        }
    }
    hint
}

/// Union of two hints: `newer`'s relay wins, direct addresses merged (newer first).
pub(crate) fn merge_hint(older: &AddrHint, newer: &AddrHint) -> AddrHint {
    let mut out = AddrHint { relay: newer.relay.clone().or_else(|| older.relay.clone()), direct: Vec::new() };
    for d in newer.direct.iter().chain(older.direct.iter()) {
        if out.direct.len() < MAX_DIRECT && !out.direct.contains(d) {
            out.direct.push(*d);
        }
    }
    out
}

/// What a connection's paths look like right now.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PathSnapshot {
    /// The selected path is a direct IP path.
    pub direct: bool,
    /// The selected path goes through a relay.
    pub relay: bool,
    /// RTT of the selected path.
    pub rtt_ms: Option<u32>,
    /// Remote addresses of all open paths (how to reach the peer).
    pub remote: AddrHint,
}

/// Snapshot the paths of `conn`.
pub(crate) fn path_snapshot(conn: &Connection) -> PathSnapshot {
    let mut snap = PathSnapshot::default();
    for p in conn.paths().iter() {
        match p.remote_addr() {
            TransportAddr::Relay(url) if snap.remote.relay.is_none() => snap.remote.relay = Some(url.to_string()),
            TransportAddr::Ip(sa) if snap.remote.direct.len() < MAX_DIRECT && !snap.remote.direct.contains(sa) => {
                snap.remote.direct.push(*sa)
            }
            _ => {}
        }
        if p.is_selected() {
            snap.direct = p.is_ip();
            snap.relay = p.is_relay();
            snap.rtt_ms = Some(u32::try_from(p.rtt().as_millis()).unwrap_or(u32::MAX));
        }
    }
    snap
}

/// The authenticated peer of a connection as a `UserId`.
pub(crate) fn remote_user(conn: &Connection) -> UserId { UserId::from_public_key(&conn.remote_id()) }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_prefers_newer() {
        let a = AddrHint { relay: Some("https://a/".into()), direct: vec!["1.1.1.1:1".parse().unwrap()] };
        let b = AddrHint { relay: None, direct: vec!["2.2.2.2:2".parse().unwrap(), "1.1.1.1:1".parse().unwrap()] };
        let m = merge_hint(&a, &b);
        assert_eq!(m.relay.as_deref(), Some("https://a/"));
        assert_eq!(m.direct.len(), 2);
        assert_eq!(m.direct[0], "2.2.2.2:2".parse().unwrap());
    }

    #[test]
    fn endpoint_addr_round_trip() {
        let id = tincan_core::keys::Identity::generate();
        let hint = AddrHint { relay: Some("https://relay.example.com./".into()), direct: vec!["127.0.0.1:5".parse().unwrap()] };
        let addr = endpoint_addr(&id.user(), Some(&hint)).unwrap();
        let back = hint_from_addr(&addr);
        assert_eq!(back.direct, hint.direct);
        assert!(back.relay.is_some());
        assert!(endpoint_addr(&UserId([0xff; 32]), None).is_none() || UserId([0xff; 32]).public_key().is_ok());
    }
}
