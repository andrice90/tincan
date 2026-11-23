//! Short codes: the 6-character server invite code and the 6-character friend code
//! (docs/architecture.md "Short codes").
//!
//! - Alphabet: `23456789ABCDEFGHJKLMNPQRSTUVWXYZ` (32 symbols, no 0/O/1/I lookalikes, shown
//!   in uppercase), so 6 characters carry 30 bits. Shown as `ABC-DEF`; `ShortCode::parse`
//!   accepts any case, with or without the dash and spaces.
//! - A code maps to a pkarr record key and a record encryption key through argon2id
//!   ([`CodeKeys::derive`]), with a fixed salt per kind, so the two kinds never share a record.
//!   For invite codes the same KDF output also gives the invite seed, so a code holder can
//!   rebuild the invite key on its own.
//! - Invite codes travel to members inside the owner's `InviteCreate.max_uses`:
//!   `SHORT_CODE_FLAG | (code ^ mask)`, where `mask = keyed_blake3(code_key, invite_pk)` (30
//!   bits) and `code_key` comes from `server_secret` ([`seal_code`], [`open_code`]). The flag
//!   bit makes the use limit at least 2^31, which is unlimited in practice.
//! - Records: an invite code record is the [`Invite`] (postcard), a friend code record is a
//!   [`FriendRecord`] signed by the user's identity over the record key. Both are sealed with
//!   XChaCha20-Poly1305 under the code's read key: `nonce24 || ciphertext`. Everything read
//!   back is untrusted: these functions return errors, never panic, on malformed input.

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};

use crate::invite::{Invite, MAX_BOOTSTRAP, invite_secret};
use crate::keys::{Identity, PublicKey, SecretKey, Sig64, random_bytes, verify};
use crate::rdv::MAX_RELAY_URL_CHARS;
use crate::wire::{AddrHint, MAX_HINT_DIRECT};
use crate::{CoreError, PROTOCOL_VERSION, Result, ServerId, UserId};

/// Characters in a code.
pub const CODE_LEN: usize = 6;
/// The code alphabet, in value order.
pub const CODE_ALPHABET: &[u8; 32] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";
/// Bits a code carries.
pub const CODE_BITS: u32 = 30;
const CODE_MASK: u32 = (1 << CODE_BITS) - 1;

/// Marks an `InviteCreate.max_uses` that carries a short code (see [`seal_code`]).
pub const SHORT_CODE_FLAG: u32 = 1 << 31;

/// argon2id memory cost in KiB.
pub const KDF_MEMORY_KIB: u32 = 64 * 1024;
/// argon2id passes.
pub const KDF_PASSES: u32 = 5;
/// argon2id lanes.
pub const KDF_LANES: u32 = 1;
const KDF_OUT: usize = 32 + 32 + 16;
const INVITE_SALT: &[u8] = b"tincan v1 server invite code";
const FRIEND_SALT: &[u8] = b"tincan v1 friend code";

const FRIEND_CONTEXT: &[u8] = b"tincan-friend1";
const NONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;

/// Server name characters kept in an invite code record (the joiner learns the real name).
pub const RECORD_NAME_CHARS: usize = 32;
/// Longest relay URL kept in a record's address hints (longer ones are dropped).
pub const RECORD_RELAY_CHARS: usize = 96;
/// Direct addresses kept per peer in a record.
pub const RECORD_DIRECT_ADDRS: usize = 2;

/// A 6-character code (30 bits).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ShortCode(u32);

impl ShortCode {
    /// A random code.
    pub fn random() -> Self { Self(u32::from_le_bytes(random_bytes()) & CODE_MASK) }

    /// The code with these 30 bits (higher bits are ignored).
    pub fn from_bits(bits: u32) -> Self { Self(bits & CODE_MASK) }

    /// The 30 bits.
    pub fn bits(self) -> u32 { self.0 }

    /// Parse typed or pasted input: exactly 6 alphabet characters in any case, ignoring
    /// whitespace and dashes. `None` for anything else.
    pub fn parse(s: &str) -> Option<Self> {
        let mut bits = 0u32;
        let mut n = 0usize;
        for c in s.chars() {
            if c.is_whitespace() || c == '-' {
                continue;
            }
            if !c.is_ascii() {
                return None;
            }
            let up = c.to_ascii_uppercase() as u8;
            let v = CODE_ALPHABET.iter().position(|&a| a == up)?;
            n += 1;
            if n > CODE_LEN {
                return None;
            }
            bits = (bits << 5) | u32::try_from(v).ok()?;
        }
        (n == CODE_LEN).then_some(Self(bits))
    }

    /// The 6 characters without a dash (`ABCDEF`): the KDF input.
    pub fn compact(self) -> String {
        (0..CODE_LEN)
            .map(|i| {
                let shift = 5 * (CODE_LEN - 1 - i);
                CODE_ALPHABET[((self.0 >> shift) & 31) as usize] as char
            })
            .collect()
    }
}

impl std::fmt::Display for ShortCode {
    /// `ABC-DEF`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let c = self.compact();
        write!(f, "{}-{}", &c[..3], &c[3..])
    }
}

impl std::fmt::Debug for ShortCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "ShortCode({self})") }
}

/// What a code is for. Each kind has its own KDF salt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CodeKind {
    /// A server invite code.
    Invite,
    /// A friend code (maps to a user id).
    Friend,
}

/// Keys derived from a code.
#[derive(Clone)]
pub struct CodeKeys {
    /// Signs the code's pkarr record.
    pub record_sk: SecretKey,
    /// Encrypts the record.
    pub read_key: [u8; 32],
    /// Invite codes: the invite seed (`invite::invite_secret(seed, server)`).
    pub invite_seed: [u8; 16],
}

impl std::fmt::Debug for CodeKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CodeKeys(record_pk {})", self.record_pk().fmt_short())
    }
}

impl CodeKeys {
    /// argon2id(code, salt of `kind`) -> record signing seed (32) || read key (32) || invite
    /// seed (16). About 200 ms in a release build, 115 ms with the dev profile (`KDF_MEMORY_KIB`, `KDF_PASSES`); run it
    /// off async executors.
    pub fn derive(kind: CodeKind, code: ShortCode) -> Self {
        let salt = match kind {
            CodeKind::Invite => INVITE_SALT,
            CodeKind::Friend => FRIEND_SALT,
        };
        let mut out = [0u8; KDF_OUT];
        // Constant, valid parameters: neither call can fail.
        if let Ok(params) = Params::new(KDF_MEMORY_KIB, KDF_PASSES, KDF_LANES, Some(KDF_OUT)) {
            let _ = Argon2::new(Algorithm::Argon2id, Version::V0x13, params).hash_password_into(code.compact().as_bytes(), salt, &mut out);
        }
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&out[..32]);
        let mut read_key = [0u8; 32];
        read_key.copy_from_slice(&out[32..64]);
        let mut invite_seed = [0u8; 16];
        invite_seed.copy_from_slice(&out[64..]);
        Self { record_sk: SecretKey::from_bytes(&seed), read_key, invite_seed }
    }

    /// Public key the record is published under.
    pub fn record_pk(&self) -> PublicKey { self.record_sk.public() }

    /// Invite codes: the `invite_pk` of the code's `InviteCreate` in `server`.
    pub fn invite_pk(&self, server: &ServerId) -> [u8; 32] { *invite_secret(&self.invite_seed, server).public().as_bytes() }
}

fn code_mask(code_key: &[u8; 32], invite_pk: &[u8; 32]) -> u32 {
    let h = blake3::keyed_hash(code_key, invite_pk);
    let b = h.as_bytes();
    u32::from_le_bytes([b[0], b[1], b[2], b[3]]) & CODE_MASK
}

/// `InviteCreate.max_uses` carrying `code` for members: `SHORT_CODE_FLAG | (code ^ mask)`.
pub fn seal_code(code_key: &[u8; 32], invite_pk: &[u8; 32], code: ShortCode) -> u32 {
    SHORT_CODE_FLAG | (code.bits() ^ code_mask(code_key, invite_pk))
}

/// The code a `max_uses` carries, if it has the flag. Check it against the invite key
/// (`CodeKeys::invite_pk`) before trusting it.
pub fn open_code(code_key: &[u8; 32], invite_pk: &[u8; 32], max_uses: u32) -> Option<ShortCode> {
    (max_uses & SHORT_CODE_FLAG != 0).then(|| ShortCode::from_bits((max_uses & CODE_MASK) ^ code_mask(code_key, invite_pk)))
}

/// `nonce24 || XChaCha20-Poly1305(plain)` under `key`.
pub fn seal_bytes(key: &[u8; 32], plain: &[u8]) -> Result<Vec<u8>> {
    let cipher = XChaCha20Poly1305::new(&Key::from(*key));
    let nonce_bytes: [u8; NONCE_LEN] = random_bytes();
    let ct = cipher.encrypt(&XNonce::from(nonce_bytes), plain).map_err(|_| CoreError::Crypto)?;
    let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Reverse of [`seal_bytes`]. `CoreError::Crypto` on a wrong key or tampering.
pub fn open_bytes(key: &[u8; 32], sealed: &[u8]) -> Result<Vec<u8>> {
    if sealed.len() < NONCE_LEN + TAG_LEN {
        return Err(CoreError::Crypto);
    }
    let (nonce_bytes, ct) = sealed.split_at(NONCE_LEN);
    let nonce = XNonce::try_from(nonce_bytes).map_err(|_| CoreError::Crypto)?;
    XChaCha20Poly1305::new(&Key::from(*key)).decrypt(&nonce, ct).map_err(|_| CoreError::Crypto)
}

fn decode_exact<T: serde::de::DeserializeOwned>(plain: &[u8]) -> Result<T> {
    let (v, rest) = postcard::take_from_bytes::<T>(plain)?;
    if !rest.is_empty() {
        return Err(CoreError::Other("trailing bytes in a code record".into()));
    }
    Ok(v)
}

/// An address hint cut down for a record (relay URL only if short, at most `direct` addrs).
pub fn compact_hint(hint: &AddrHint, direct: usize) -> AddrHint {
    AddrHint {
        relay: hint.relay.clone().filter(|r| r.len() <= RECORD_RELAY_CHARS),
        direct: hint.direct.iter().take(direct).copied().collect(),
    }
}

/// The invite an invite code record carries: `code`'s seed, no expiry, a short name and at
/// most `MAX_BOOTSTRAP` peers with compact hints.
pub fn code_invite(keys: &CodeKeys, base: &Invite) -> Invite {
    Invite {
        v: PROTOCOL_VERSION,
        server: base.server,
        name_hint: base.name_hint.chars().take(RECORD_NAME_CHARS).collect(),
        invite_seed: keys.invite_seed,
        rdv_pk: base.rdv_pk,
        rdv_read_key: base.rdv_read_key,
        bootstrap: base.bootstrap.iter().take(MAX_BOOTSTRAP).map(|(u, h)| (*u, compact_hint(h, RECORD_DIRECT_ADDRS))).collect(),
        expires_ms: u64::MAX,
    }
}

/// Seal an invite code record. The invite must carry this code's seed.
pub fn seal_invite(keys: &CodeKeys, invite: &Invite) -> Result<Vec<u8>> {
    if invite.invite_seed != keys.invite_seed {
        return Err(CoreError::InvalidInvite("the invite does not belong to this code".into()));
    }
    seal_bytes(&keys.read_key, &postcard::to_stdvec(invite)?)
}

/// Open an invite code record: decrypt, decode, check the version, the seed and the bounds.
pub fn open_invite(keys: &CodeKeys, sealed: &[u8]) -> Result<Invite> {
    let inv: Invite = decode_exact(&open_bytes(&keys.read_key, sealed)?)?;
    if inv.v != PROTOCOL_VERSION {
        return Err(CoreError::InvalidInvite("this code is for a different Tincan version".into()));
    }
    if inv.invite_seed != keys.invite_seed {
        return Err(CoreError::InvalidInvite("the record does not match the code".into()));
    }
    if inv.bootstrap.len() > MAX_BOOTSTRAP || inv.bootstrap.iter().any(|(_, h)| !hint_ok(h)) {
        return Err(CoreError::InvalidInvite("bad bootstrap peers in the record".into()));
    }
    if inv.name_hint.chars().count() > RECORD_NAME_CHARS {
        return Err(CoreError::InvalidInvite("server name too long in the record".into()));
    }
    Ok(inv)
}

fn hint_ok(h: &AddrHint) -> bool { h.direct.len() <= MAX_HINT_DIRECT && h.relay.as_ref().is_none_or(|r| r.len() <= MAX_RELAY_URL_CHARS) }

/// What a friend code record says: this code belongs to `user`, reachable at `addr`. Signed
/// by `user` over the record key, so a record cannot name a user who did not sign for it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FriendRecord {
    /// `PROTOCOL_VERSION`.
    pub v: u8,
    /// The code's owner.
    pub user: UserId,
    /// Where to reach them (compact).
    pub addr: AddrHint,
    /// When they published it (unix ms).
    pub published_ms: u64,
    /// `user`'s signature over [`friend_sig_message`].
    pub sig: Sig64,
}

/// Friend code record direct addresses.
pub const FRIEND_DIRECT_ADDRS: usize = 4;

/// `"tincan-friend1" || record_pk || postcard((v, user, addr, published_ms))`.
pub fn friend_sig_message(record_pk: &PublicKey, v: u8, user: &UserId, addr: &AddrHint, published_ms: u64) -> Vec<u8> {
    let mut m = Vec::with_capacity(FRIEND_CONTEXT.len() + 32 + 128);
    m.extend_from_slice(FRIEND_CONTEXT);
    m.extend_from_slice(record_pk.as_bytes());
    // postcard cannot fail on these types.
    m.extend_from_slice(&postcard::to_stdvec(&(v, user, addr, published_ms)).unwrap_or_default());
    m
}

impl FriendRecord {
    /// My record for `keys`' code.
    pub fn new(identity: &Identity, keys: &CodeKeys, addr: &AddrHint, published_ms: u64) -> Self {
        let addr = compact_hint(addr, FRIEND_DIRECT_ADDRS);
        let user = identity.user();
        let sig = identity.sign(&friend_sig_message(&keys.record_pk(), PROTOCOL_VERSION, &user, &addr, published_ms));
        Self { v: PROTOCOL_VERSION, user, addr, published_ms, sig }
    }

    /// Seal under the code's read key.
    pub fn seal(&self, keys: &CodeKeys) -> Result<Vec<u8>> { seal_bytes(&keys.read_key, &postcard::to_stdvec(self)?) }

    /// Decrypt, decode and verify the signature against the code's record key.
    pub fn open(keys: &CodeKeys, sealed: &[u8]) -> Result<Self> {
        let rec: FriendRecord = decode_exact(&open_bytes(&keys.read_key, sealed)?)?;
        if rec.v != PROTOCOL_VERSION {
            return Err(CoreError::Other("friend code record from a different Tincan version".into()));
        }
        if !hint_ok(&rec.addr) {
            return Err(CoreError::Other("bad address in the friend code record".into()));
        }
        verify(&rec.user, &friend_sig_message(&keys.record_pk(), rec.v, &rec.user, &rec.addr, rec.published_ms), &rec.sig)?;
        Ok(rec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::ServerKeys;
    use crate::rdv::to_txt_chunks;

    #[test]
    fn alphabet_is_32_unambiguous_symbols() {
        assert_eq!(CODE_ALPHABET.len(), 32);
        let mut seen = std::collections::HashSet::new();
        for &c in CODE_ALPHABET {
            assert!(seen.insert(c), "duplicate {}", c as char);
            assert!(c.is_ascii_uppercase() || c.is_ascii_digit());
            assert!(!b"01OI".contains(&c), "lookalike {}", c as char);
        }
        assert_eq!(32u32.pow(CODE_LEN as u32), 1 << CODE_BITS);
    }

    #[test]
    fn parse_and_format() {
        for _ in 0..200 {
            let c = ShortCode::random();
            let compact = c.compact();
            assert_eq!(compact.len(), CODE_LEN);
            let shown = c.to_string();
            assert_eq!(shown.len(), CODE_LEN + 1);
            assert_eq!(&shown[3..4], "-");
            for s in [compact.clone(), shown.clone(), compact.to_lowercase(), format!("  {} {}\n", &compact[..3], &compact[3..]), shown.to_lowercase()] {
                assert_eq!(ShortCode::parse(&s), Some(c), "{s:?}");
            }
        }
        assert_eq!(ShortCode::from_bits(0).compact(), "222222");
        assert_eq!(ShortCode::from_bits(CODE_MASK).compact(), "ZZZZZZ");
        assert_eq!(ShortCode::parse("K7Q-M2X").map(|c| c.to_string()).as_deref(), Some("K7Q-M2X"));
        for bad in ["", "ABCDE", "ABCDEFG", "ABC-DE0", "ABCDE1", "ABCDEO", "ABCDEI", "AB_CDE", "ABCDÉF", "tincan1abc", "ABC--DEFG"] {
            assert_eq!(ShortCode::parse(bad), None, "accepted {bad:?}");
        }
        // Higher bits never leak into the code.
        assert_eq!(ShortCode::from_bits(u32::MAX).bits(), CODE_MASK);
    }

    #[test]
    fn kdf_is_deterministic_and_separates_kinds() {
        let code = ShortCode::parse("K7Q-M2X").unwrap();
        let a = CodeKeys::derive(CodeKind::Invite, code);
        let b = CodeKeys::derive(CodeKind::Invite, code);
        assert_eq!(a.record_pk(), b.record_pk());
        assert_eq!(a.read_key, b.read_key);
        assert_eq!(a.invite_seed, b.invite_seed);
        let f = CodeKeys::derive(CodeKind::Friend, code);
        assert_ne!(a.record_pk(), f.record_pk());
        assert_ne!(a.read_key, f.read_key);
        let other = CodeKeys::derive(CodeKind::Invite, ShortCode::parse("K7Q-M2Y").unwrap());
        assert_ne!(a.record_pk(), other.record_pk());
        // Pinned: the parameters and salts are protocol constants. A change here breaks
        // every published code.
        let hex = |b: &[u8]| data_encoding::HEXLOWER.encode(&b[..8]);
        assert_eq!(hex(&a.read_key), "521bad3974ab45a9");
        assert_eq!(hex(a.record_pk().as_bytes()), "e825e287bb9b3b82");
        assert_eq!(hex(&f.read_key), "c6185d9ca9a108b5");
    }

    #[test]
    fn code_rides_in_max_uses() {
        let keys = ServerKeys::derive(&[1; 32], &ServerId::from_bytes([2; 32]));
        for _ in 0..100 {
            let code = ShortCode::random();
            let pk: [u8; 32] = random_bytes();
            let m = seal_code(&keys.code_key, &pk, code);
            assert!(m & SHORT_CODE_FLAG != 0, "flag set, so the use limit is effectively unlimited");
            assert_eq!(open_code(&keys.code_key, &pk, m), Some(code));
        }
        // Another server's key, or a plain max_uses, gives nothing useful.
        let code = ShortCode::random();
        let pk = [9u8; 32];
        let m = seal_code(&keys.code_key, &pk, code);
        let other = ServerKeys::derive(&[3; 32], &ServerId::from_bytes([2; 32]));
        assert_ne!(open_code(&other.code_key, &pk, m), Some(code));
        assert_eq!(open_code(&keys.code_key, &pk, 0), None);
        assert_eq!(open_code(&keys.code_key, &pk, 25), None);
    }

    fn worst_invite(keys: &CodeKeys) -> Invite {
        let v6 = |i: u16| std::net::SocketAddr::new(std::net::IpAddr::V6(std::net::Ipv6Addr::new(0x2001, 0xdb8, i, i, i, i, i, i)), 65535);
        let hint = AddrHint { relay: Some(format!("https://{}/", "r".repeat(RECORD_RELAY_CHARS - 9))), direct: (0..8).map(v6).collect() };
        let base = Invite {
            v: PROTOCOL_VERSION,
            server: ServerId::from_bytes([0xff; 32]),
            name_hint: "\u{1F600}".repeat(100),
            invite_seed: [0; 16],
            rdv_pk: [0xff; 32],
            rdv_read_key: [0xff; 32],
            bootstrap: (0..4).map(|_| (Identity::generate().user(), hint.clone())).collect(),
            expires_ms: 5,
        };
        code_invite(keys, &base)
    }

    #[test]
    fn invite_record_round_trip_and_fits_a_packet() {
        let code = ShortCode::random();
        let keys = CodeKeys::derive(CodeKind::Invite, code);
        let inv = worst_invite(&keys);
        assert_eq!(inv.bootstrap.len(), MAX_BOOTSTRAP);
        assert_eq!(inv.expires_ms, u64::MAX);
        assert!(inv.bootstrap.iter().all(|(_, h)| h.direct.len() == RECORD_DIRECT_ADDRS && h.relay.is_some()));
        let sealed = seal_invite(&keys, &inv).unwrap();
        assert_eq!(open_invite(&keys, &sealed).unwrap(), inv);
        // Same budget as the rendezvous record (rdv::tests::twelve_users_fit_in_a_pkarr_packet).
        let txt_bytes: usize = to_txt_chunks(&sealed).iter().map(|c| c.len() + 1).sum();
        assert!(txt_bytes < 880, "{txt_bytes} bytes of TXT data");
        // The invite key is the code's.
        assert_eq!(inv.invite_pk(), keys.invite_pk(&inv.server));
    }

    #[test]
    fn invite_record_rejects_wrong_keys_and_tampering() {
        let keys = CodeKeys::derive(CodeKind::Invite, ShortCode::random());
        let other = CodeKeys::derive(CodeKind::Invite, ShortCode::random());
        let inv = worst_invite(&keys);
        let sealed = seal_invite(&keys, &inv).unwrap();
        assert!(open_invite(&other, &sealed).is_err());
        let mut t = sealed.clone();
        t[30] ^= 1;
        assert!(open_invite(&keys, &t).is_err());
        assert!(open_invite(&keys, &[]).is_err());
        // An invite with another seed cannot be sealed under this code, nor opened from it.
        assert!(seal_invite(&other, &inv).is_err());
        let foreign = seal_bytes(&keys.read_key, &postcard::to_stdvec(&worst_invite(&other)).unwrap()).unwrap();
        assert!(open_invite(&keys, &foreign).is_err());
    }

    #[test]
    fn friend_record_sign_verify() {
        let me = Identity::generate();
        let keys = CodeKeys::derive(CodeKind::Friend, ShortCode::random());
        let addr = AddrHint { relay: Some("https://euw1-1.relay.n0.iroh-canary.iroh.link./".into()), direct: vec!["127.0.0.1:5000".parse().unwrap(); 6] };
        let rec = FriendRecord::new(&me, &keys, &addr, 1_800_000_000_000);
        assert_eq!(rec.addr.direct.len(), FRIEND_DIRECT_ADDRS);
        let sealed = rec.seal(&keys).unwrap();
        assert!(to_txt_chunks(&sealed).iter().map(|c| c.len() + 1).sum::<usize>() < 880);
        assert_eq!(FriendRecord::open(&keys, &sealed).unwrap(), rec);

        // Someone else's code key: the signature binds the record key, so a valid record
        // cannot be moved to another code.
        let other = CodeKeys::derive(CodeKind::Friend, ShortCode::random());
        let moved = seal_bytes(&other.read_key, &postcard::to_stdvec(&rec).unwrap()).unwrap();
        assert!(FriendRecord::open(&other, &moved).is_err());
        // A record naming another user without their signature fails.
        let mallory = Identity::generate();
        let forged = FriendRecord { user: mallory.user(), ..rec.clone() };
        assert!(FriendRecord::open(&keys, &forged.seal(&keys).unwrap()).is_err());
        let tampered = FriendRecord { published_ms: rec.published_ms + 1, ..rec.clone() };
        assert!(FriendRecord::open(&keys, &tampered.seal(&keys).unwrap()).is_err());
        assert!(FriendRecord::open(&other, &sealed).is_err());
    }

    /// Prints the KDF time (run with `--nocapture` on an optimized build to tune).
    #[test]
    fn kdf_timing() {
        let t = std::time::Instant::now();
        let _ = CodeKeys::derive(CodeKind::Invite, ShortCode::random());
        eprintln!("argon2id m={KDF_MEMORY_KIB} KiB t={KDF_PASSES} p={KDF_LANES}: {:?}", t.elapsed());
    }
}
