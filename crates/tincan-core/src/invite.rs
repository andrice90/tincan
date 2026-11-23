//! Invite codes and the join signature.
//!
//! Code format: "tincan1" + lowercase base32 (RFC 4648, no padding) of postcard(Invite).
//! Because `Invite.v` is the first byte and is small, every code starts "tincan1a".
//! `Invite::decode` also accepts surrounding whitespace, line breaks inside the code,
//! uppercase, a `tincan://join/` link prefix (with or without "tincan1" after it) and a
//! trailing slash.
//!
//! User id format for pasting: "tincan1u" + z-base-32 user id.

use serde::{Deserialize, Serialize};

use crate::keys::{SecretKey, Sig64, sign_with};
use crate::wire::AddrHint;
use crate::{CoreError, PROTOCOL_VERSION, Result, ServerId, UserId};

/// Prefix of invite codes.
pub const INVITE_PREFIX: &str = "tincan1";
/// Prefix of pasteable user ids.
pub const USER_PREFIX: &str = "tincan1u";
/// Link form accepted by `Invite::decode`.
pub const INVITE_LINK_PREFIX: &str = "tincan://join/";
/// Most bootstrap peers in an invite.
pub const MAX_BOOTSTRAP: usize = 2;
/// Longest accepted invite code (characters, whitespace excluded).
pub const MAX_INVITE_CODE_CHARS: usize = 4096;

const JOIN_CONTEXT: &[u8] = b"tincan-join1";
const INVITE_KEY_CONTEXT: &str = "tincan v1 invite key";

/// Everything a joiner needs to find and join a server. Never contains `server_secret`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invite {
    /// `PROTOCOL_VERSION`.
    pub v: u8,
    /// The server.
    pub server: ServerId,
    /// Server name at invite time (display only; the joiner learns the real name on join).
    pub name_hint: String,
    /// invite_sk = SecretKey::from_bytes(derive_key("tincan v1 invite key", seed || server))
    pub invite_seed: [u8; 16],
    /// Rendezvous record public key (ServerKeys::rdv_pk).
    pub rdv_pk: [u8; 32],
    /// Rendezvous record read key (ServerKeys::rdv_read_key).
    pub rdv_read_key: [u8; 32],
    /// Up to 2 members to dial first (the inviter and the current host), with address hints.
    pub bootstrap: Vec<(UserId, AddrHint)>,
    /// Unix ms after which the invite stops working (`u64::MAX` = never).
    pub expires_ms: u64,
}

impl Invite {
    /// The shareable code: "tincan1" + lowercase base32.
    pub fn encode(&self) -> String {
        // postcard cannot fail on this type (no maps, no unsized sequences).
        let bytes = postcard::to_stdvec(self).unwrap_or_default();
        let mut s = String::with_capacity(INVITE_PREFIX.len() + bytes.len() * 8 / 5 + 1);
        s.push_str(INVITE_PREFIX);
        s.push_str(&data_encoding::BASE32_NOPAD.encode(&bytes).to_ascii_lowercase());
        s
    }

    /// The `tincan://join/...` link form of the code.
    pub fn link(&self) -> String { format!("{INVITE_LINK_PREFIX}{}", self.encode()) }

    /// Parse a code or link (see module docs for what is accepted).
    pub fn decode(s: &str) -> Result<Self> {
        let bad = |why: &str| CoreError::InvalidInvite(why.to_string());
        let mut t = s.trim();
        if let Some(rest) = strip_prefix_ci(t, INVITE_LINK_PREFIX) {
            t = rest;
        }
        t = t.trim_end_matches('/').trim();
        if strip_prefix_ci(t, USER_PREFIX).is_some() {
            return Err(bad("this is a user ID, not an invite code"));
        }
        if let Some(rest) = strip_prefix_ci(t, INVITE_PREFIX) {
            t = rest;
        }
        let code: String = t.chars().filter(|c| !c.is_whitespace()).collect();
        if code.is_empty() {
            return Err(bad("empty invite code"));
        }
        if code.len() > MAX_INVITE_CODE_CHARS {
            return Err(bad("invite code too long"));
        }
        let bytes = data_encoding::BASE32_NOPAD_NOCASE
            .decode(code.as_bytes())
            .map_err(|_| bad("not an invite code (bad characters)"))?;
        let (inv, rest) = postcard::take_from_bytes::<Invite>(&bytes).map_err(|_| bad("not an invite code (damaged)"))?;
        if !rest.is_empty() {
            return Err(bad("not an invite code (trailing data)"));
        }
        if inv.v != PROTOCOL_VERSION {
            return Err(bad("this invite is for a different Tincan version"));
        }
        if inv.bootstrap.len() > 4 * MAX_BOOTSTRAP {
            return Err(bad("too many bootstrap peers"));
        }
        Ok(inv)
    }

    /// The invite's signing key (derived from the seed and server).
    pub fn secret(&self) -> SecretKey { invite_secret(&self.invite_seed, &self.server) }

    /// The invite's public key (`invite_pk` in InviteCreate and JoinRequest).
    pub fn invite_pk(&self) -> [u8; 32] { *self.secret().public().as_bytes() }

    /// `invite_sig` for a JoinRequest from `joiner` to `host`.
    pub fn sign_join(&self, joiner: &UserId, host: &UserId, ts_ms: u64) -> Sig64 {
        sign_with(&self.secret(), &join_sig_message(&self.server, joiner, host, ts_ms))
    }

    /// True if the invite has expired at `now_ms`.
    pub fn is_expired(&self, now_ms: u64) -> bool { now_ms > self.expires_ms }
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    if head.eq_ignore_ascii_case(prefix) { s.get(prefix.len()..) } else { None }
}

/// Secret key the joiner signs with; its public half is the `invite_pk` in InviteCreate.
/// `SecretKey::from_bytes(blake3::derive_key("tincan v1 invite key", seed || server))`.
pub fn invite_secret(seed: &[u8; 16], server: &ServerId) -> SecretKey {
    let mut material = [0u8; 48];
    material[..16].copy_from_slice(seed);
    material[16..].copy_from_slice(&server.0);
    SecretKey::from_bytes(&blake3::derive_key(INVITE_KEY_CONTEXT, &material))
}

/// "tincan-join1" || server || joiner || host || ts_ms(le u64)
pub fn join_sig_message(server: &ServerId, joiner: &UserId, host: &UserId, ts_ms: u64) -> Vec<u8> {
    let mut m = Vec::with_capacity(JOIN_CONTEXT.len() + 32 * 3 + 8);
    m.extend_from_slice(JOIN_CONTEXT);
    m.extend_from_slice(&server.0);
    m.extend_from_slice(&joiner.0);
    m.extend_from_slice(&host.0);
    m.extend_from_slice(&ts_ms.to_le_bytes());
    m
}

/// `"tincan1u" + z32` -> UserId; also accepts a bare z32 id, surrounding whitespace and uppercase.
pub fn parse_user_code(s: &str) -> Option<UserId> {
    let t = s.trim();
    let t = strip_prefix_ci(t, USER_PREFIX).unwrap_or(t);
    UserId::from_z32(t)
}

/// "tincan1u" + z-base-32 user id ("Copy my ID").
pub fn user_code(user: &UserId) -> String { format!("{USER_PREFIX}{}", user.to_z32()) }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{Identity, ServerKeys, verify};

    fn sample() -> Invite {
        let server = ServerId::from_bytes([7; 32]);
        let keys = ServerKeys::derive(&[1; 32], &server);
        Invite {
            v: PROTOCOL_VERSION,
            server,
            name_hint: "Friends".into(),
            invite_seed: [3; 16],
            rdv_pk: *keys.rdv_pk().as_bytes(),
            rdv_read_key: keys.rdv_read_key,
            bootstrap: vec![
                (
                    Identity::generate().user(),
                    AddrHint {
                        relay: Some("https://euw1-1.relay.n0.iroh.link./".into()),
                        direct: vec!["192.168.1.20:51234".parse().unwrap(), "[2001:db8::1]:51234".parse().unwrap()],
                    },
                ),
                (Identity::generate().user(), AddrHint::default()),
            ],
            expires_ms: 1_800_000_000_000,
        }
    }

    #[test]
    fn encode_decode_round_trip() {
        let inv = sample();
        let code = inv.encode();
        assert!(code.starts_with("tincan1a"), "{code}");
        assert!(code[INVITE_PREFIX.len()..].chars().all(|c| c.is_ascii_lowercase() || ('2'..='7').contains(&c)));
        assert_eq!(Invite::decode(&code).unwrap(), inv);
        assert!(code.len() < 500, "code is {} chars", code.len());
    }

    #[test]
    fn decode_accepts_variants() {
        let inv = sample();
        let code = inv.encode();
        let body = &code[INVITE_PREFIX.len()..];
        let wrapped: String = code.chars().collect::<Vec<_>>().chunks(40).map(|c| c.iter().collect::<String>()).collect::<Vec<_>>().join("\n");
        for s in [
            code.clone(),
            format!("  {code}\n"),
            code.to_uppercase(),
            inv.link(),
            format!("tincan://join/{code}/"),
            format!("TINCAN://JOIN/{}", body.to_uppercase()),
            format!("tincan://join/{body}"),
            body.to_string(),
            wrapped,
        ] {
            assert_eq!(Invite::decode(&s).unwrap(), inv, "failed on {s:?}");
        }
    }

    #[test]
    fn decode_rejects_garbage() {
        let user = user_code(&Identity::generate().user());
        for s in ["", "   ", "tincan1", "tincan1!!!!", "hello world", "tincan1aaaa", user.as_str()] {
            assert!(matches!(Invite::decode(s), Err(CoreError::InvalidInvite(_))), "accepted {s:?}");
        }
        let mut code = sample().encode();
        code.push_str("aaaaaaaa");
        assert!(Invite::decode(&code).is_err());
        let long = format!("tincan1{}", "a".repeat(MAX_INVITE_CODE_CHARS + 1));
        assert!(Invite::decode(&long).is_err());
        // a different version is refused
        let mut v2 = sample();
        v2.v = 2;
        assert!(Invite::decode(&v2.encode()).is_err());
        // non-ASCII input never panics
        assert!(Invite::decode("tincan1é").is_err());
        assert!(Invite::decode("é").is_err());
    }

    #[test]
    fn invite_keys_and_join_signature() {
        let inv = sample();
        let sk = invite_secret(&inv.invite_seed, &inv.server);
        assert_eq!(*sk.public().as_bytes(), inv.invite_pk());
        // different seed or server -> different key
        assert_ne!(*invite_secret(&[4; 16], &inv.server).public().as_bytes(), inv.invite_pk());
        assert_ne!(*invite_secret(&inv.invite_seed, &ServerId::from_bytes([8; 32])).public().as_bytes(), inv.invite_pk());

        let joiner = Identity::generate().user();
        let host = Identity::generate().user();
        let sig = inv.sign_join(&joiner, &host, 1234);
        let pk = UserId::from_bytes(inv.invite_pk());
        verify(&pk, &join_sig_message(&inv.server, &joiner, &host, 1234), &sig).unwrap();
        assert!(verify(&pk, &join_sig_message(&inv.server, &joiner, &host, 1235), &sig).is_err());
        assert!(verify(&pk, &join_sig_message(&inv.server, &host, &joiner, 1234), &sig).is_err());
    }

    #[test]
    fn join_message_layout() {
        let m = join_sig_message(&ServerId::from_bytes([1; 32]), &UserId([2; 32]), &UserId([3; 32]), 0x0102);
        assert_eq!(&m[..12], b"tincan-join1");
        assert_eq!(m.len(), 12 + 96 + 8);
        assert_eq!(&m[12..44], &[1; 32]);
        assert_eq!(&m[44..76], &[2; 32]);
        assert_eq!(&m[76..108], &[3; 32]);
        assert_eq!(&m[108..], &[2, 1, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn user_codes() {
        let u = Identity::generate().user();
        let code = user_code(&u);
        assert!(code.starts_with(USER_PREFIX));
        assert_eq!(parse_user_code(&code), Some(u));
        assert_eq!(parse_user_code(&format!("  {}  ", code.to_uppercase())), Some(u));
        assert_eq!(parse_user_code(&u.to_z32()), Some(u));
        assert_eq!(parse_user_code("tincan1unope"), None);
        assert_eq!(parse_user_code(""), None);
        assert_eq!(parse_user_code(&sample().encode()), None);
    }

    #[test]
    fn expiry() {
        let inv = sample();
        assert!(!inv.is_expired(inv.expires_ms));
        assert!(inv.is_expired(inv.expires_ms + 1));
    }
}
