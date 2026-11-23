//! Short codes: the 6-character server invite code and the 6-character friend code
//! (docs/architecture.md "Short codes").
//!
//! - Alphabet: `23456789ABCDEFGHJKLMNPQRSTUVWXYZ` (32 symbols, no 0/O/1/I lookalikes, shown
//!   in uppercase), so 6 characters carry 30 bits. Shown as `ABC-DEF`; `ShortCode::parse`
//!   accepts any case, with or without the dash and spaces.
//! - The code commits to its target. A friend code is the first 30 bits of
//!   `argon2id("tincan-friend" || user || nonce)` ([`friend_commit`]); an invite code is the
//!   first 30 bits of `argon2id("tincan-invite" || server || invite_pk || nonce)`
//!   ([`invite_commit`]). The nonce is random, so rotating means a new nonce. A record carries
//!   the target and the nonce, and a resolver recomputes the commitment before trusting it,
//!   so pointing a code at another target needs about 2^30 argon2id evaluations.
//! - The record slot: a code maps through argon2id ([`CodeKeys::derive`], a salt per kind)
//!   to a pkarr record key and a read key. Holding the code gives write access to the slot,
//!   never authority: a record that fails the commitment is ignored.
//! - Invite codes travel to members inside the owner's `InviteCreate.max_uses`:
//!   `SHORT_CODE_FLAG | r` with `r` 31 random bits; the invite seed and the nonce are
//!   `keyed_blake3(code_key, ...)` of `r` ([`invite_material`]), with `code_key` from
//!   `server_secret`. Every member can rebuild the code and the record; nobody without the
//!   current log (an ex-member) learns `r`. The flag bit makes the use limit at least 2^31,
//!   which is unlimited in practice.
//! - Records are sealed with XChaCha20-Poly1305 under the code's read key:
//!   `nonce24 || ciphertext`. Everything read back is untrusted: these functions return
//!   errors, never panic, on malformed input.

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};

use crate::invite::{Invite, MAX_BOOTSTRAP};
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

/// Marks an `InviteCreate.max_uses` that carries a short code (see [`invite_material`]).
pub const SHORT_CODE_FLAG: u32 = 1 << 31;
const MARKER_MASK: u32 = SHORT_CODE_FLAG - 1;

/// argon2id memory cost in KiB.
pub const KDF_MEMORY_KIB: u32 = 64 * 1024;
/// argon2id passes.
pub const KDF_PASSES: u32 = 5;
/// argon2id lanes.
pub const KDF_LANES: u32 = 1;
const SLOT_OUT: usize = 64;
const COMMIT_OUT: usize = 8;
const INVITE_SALT: &[u8] = b"tincan v1 server invite code";
const FRIEND_SALT: &[u8] = b"tincan v1 friend code";
const INVITE_COMMIT_SALT: &[u8] = b"tincan v1 invite code commitment";
const FRIEND_COMMIT_SALT: &[u8] = b"tincan v1 friend code commitment";

const FRIEND_CONTEXT: &[u8] = b"tincan-friend1";
const NONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;

/// Server name characters kept in an invite code record (the joiner learns the real name).
pub const RECORD_NAME_CHARS: usize = 24;
/// Longest relay URL kept in a record's address hints (longer ones are dropped).
pub const RECORD_RELAY_CHARS: usize = 96;
/// Direct addresses kept per peer in a record.
pub const RECORD_DIRECT_ADDRS: usize = 2;
/// A friend record older than this is ignored (the owner refreshes it every 30 minutes
/// while online), so a stale copy of a rotated code dies out.
pub const FRIEND_RECORD_MAX_AGE_MS: u64 = 14 * 24 * 60 * 60 * 1000;

/// A 6-character code (30 bits).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ShortCode(u32);

impl ShortCode {
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

fn argon2id(password: &[u8], salt: &[u8], out: &mut [u8]) {
    // Constant, valid parameters and a salt of at least 8 bytes: neither call can fail.
    if let Ok(params) = Params::new(KDF_MEMORY_KIB, KDF_PASSES, KDF_LANES, Some(out.len())) {
        let _ = Argon2::new(Algorithm::Argon2id, Version::V0x13, params).hash_password_into(password, salt, out);
    }
}

fn commit(salt: &[u8], password: &[u8]) -> ShortCode {
    let mut out = [0u8; COMMIT_OUT];
    argon2id(password, salt, &mut out);
    ShortCode::from_bits(u32::from_be_bytes([out[0], out[1], out[2], out[3]]) >> 2)
}

/// The friend code `user` gets with `nonce` (argon2id, about 200 ms in release).
pub fn friend_commit(user: &UserId, nonce: &[u8; 16]) -> ShortCode {
    let mut p = Vec::with_capacity(13 + 32 + 16);
    p.extend_from_slice(b"tincan-friend");
    p.extend_from_slice(&user.0);
    p.extend_from_slice(nonce);
    commit(FRIEND_COMMIT_SALT, &p)
}

/// The invite code of `invite_pk` in `server` with `nonce` (argon2id, about 200 ms in release).
pub fn invite_commit(server: &ServerId, invite_pk: &[u8; 32], nonce: &[u8; 16]) -> ShortCode {
    let mut p = Vec::with_capacity(13 + 32 + 32 + 16);
    p.extend_from_slice(b"tincan-invite");
    p.extend_from_slice(&server.0);
    p.extend_from_slice(invite_pk);
    p.extend_from_slice(nonce);
    commit(INVITE_COMMIT_SALT, &p)
}

/// My friend code: the code and the nonce it commits with.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct FriendCode {
    /// The code.
    pub code: ShortCode,
    /// Its nonce (a new one rotates the code).
    pub nonce: [u8; 16],
}

impl std::fmt::Debug for FriendCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "FriendCode({})", self.code) }
}

impl FriendCode {
    /// A fresh friend code for `user` (runs the KDF).
    pub fn generate(user: &UserId) -> Self {
        let nonce: [u8; 16] = random_bytes();
        Self { code: friend_commit(user, &nonce), nonce }
    }

    /// `ABCDEF:<nonce hex>`, for the profile (trusted local storage).
    pub fn to_stored(&self) -> String { format!("{}:{}", self.code.compact(), data_encoding::HEXLOWER.encode(&self.nonce)) }

    /// Reverse of `to_stored`.
    pub fn from_stored(s: &str) -> Option<Self> {
        let (code, nonce) = s.split_once(':')?;
        let nonce: [u8; 16] = data_encoding::HEXLOWER.decode(nonce.as_bytes()).ok()?.try_into().ok()?;
        Some(Self { code: ShortCode::parse(code)?, nonce })
    }
}

/// The invite seed and commitment nonce of the short-code invite marked `r`
/// (`max_uses = SHORT_CODE_FLAG | r`).
pub fn invite_material(code_key: &[u8; 32], r: u32) -> ([u8; 16], [u8; 16]) {
    let half = |label: &[u8]| {
        let mut h = blake3::Hasher::new_keyed(code_key);
        h.update(label);
        h.update(&r.to_le_bytes());
        let mut out = [0u8; 16];
        out.copy_from_slice(&h.finalize().as_bytes()[..16]);
        out
    };
    (half(b"invite seed"), half(b"code nonce"))
}

/// A random marker `r` (31 bits).
pub fn random_marker() -> u32 { u32::from_le_bytes(random_bytes()) & MARKER_MASK }

/// `max_uses` of the short-code invite marked `r`.
pub fn marker_max_uses(r: u32) -> u32 { SHORT_CODE_FLAG | (r & MARKER_MASK) }

/// The marker a `max_uses` carries, if it has the flag.
pub fn open_marker(max_uses: u32) -> Option<u32> { (max_uses & SHORT_CODE_FLAG != 0).then_some(max_uses & MARKER_MASK) }

/// What a code is for. Each kind has its own slot salt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CodeKind {
    /// A server invite code.
    Invite,
    /// A friend code (maps to a user id).
    Friend,
}

/// The record slot of a code: who may write it is not who it points at (see the module
/// docs); records are checked with [`verify_invite`] / [`verify_friend`].
#[derive(Clone)]
pub struct CodeKeys {
    /// Signs the code's pkarr record.
    pub record_sk: SecretKey,
    /// Encrypts the record.
    pub read_key: [u8; 32],
}

impl std::fmt::Debug for CodeKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CodeKeys(record_pk {})", self.record_pk().fmt_short())
    }
}

impl CodeKeys {
    /// argon2id(code, salt of `kind`) -> record signing seed (32) || read key (32). About 200
    /// ms in a release build, 115 ms with the dev profile; run it off async executors.
    pub fn derive(kind: CodeKind, code: ShortCode) -> Self {
        let salt = match kind {
            CodeKind::Invite => INVITE_SALT,
            CodeKind::Friend => FRIEND_SALT,
        };
        let mut out = [0u8; SLOT_OUT];
        argon2id(code.compact().as_bytes(), salt, &mut out);
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&out[..32]);
        let mut read_key = [0u8; 32];
        read_key.copy_from_slice(&out[32..]);
        Self { record_sk: SecretKey::from_bytes(&seed), read_key }
    }

    /// Public key the record is published under.
    pub fn record_pk(&self) -> PublicKey { self.record_sk.public() }
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

fn hint_ok(h: &AddrHint) -> bool { h.direct.len() <= MAX_HINT_DIRECT && h.relay.as_ref().is_none_or(|r| r.len() <= MAX_RELAY_URL_CHARS) }

/// An invite code record: the invite and the nonce its code commits with.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteRecord {
    /// The invite (no expiry, a short name, at most `MAX_BOOTSTRAP` peers).
    pub invite: Invite,
    /// `invite_commit(invite.server, invite.invite_pk(), nonce)` is the code.
    pub nonce: [u8; 16],
}

impl InviteRecord {
    /// The record for an invite (its name and peers cut down to fit a pkarr packet).
    pub fn new(base: &Invite, nonce: [u8; 16]) -> Self {
        let invite = Invite {
            v: PROTOCOL_VERSION,
            server: base.server,
            name_hint: base.name_hint.chars().take(RECORD_NAME_CHARS).collect(),
            invite_seed: base.invite_seed,
            rdv_pk: base.rdv_pk,
            rdv_read_key: base.rdv_read_key,
            bootstrap: base.bootstrap.iter().take(MAX_BOOTSTRAP).map(|(u, h)| (*u, compact_hint(h, RECORD_DIRECT_ADDRS))).collect(),
            expires_ms: u64::MAX,
        };
        Self { invite, nonce }
    }

    /// Seal under the slot's read key.
    pub fn seal(&self, keys: &CodeKeys) -> Result<Vec<u8>> { seal_bytes(&keys.read_key, &postcard::to_stdvec(self)?) }

    /// Decrypt and decode, with the cheap checks (version, bounds). Not yet trusted: check
    /// it with [`verify_invite`].
    pub fn open(keys: &CodeKeys, sealed: &[u8]) -> Result<Self> {
        let rec: InviteRecord = decode_exact(&open_bytes(&keys.read_key, sealed)?)?;
        let inv = &rec.invite;
        if inv.v != PROTOCOL_VERSION {
            return Err(CoreError::InvalidInvite("this code is for a different Tincan version".into()));
        }
        if inv.bootstrap.len() > MAX_BOOTSTRAP || inv.bootstrap.iter().any(|(_, h)| !hint_ok(h)) {
            return Err(CoreError::InvalidInvite("bad bootstrap peers in the record".into()));
        }
        if inv.name_hint.chars().count() > RECORD_NAME_CHARS {
            return Err(CoreError::InvalidInvite("server name too long in the record".into()));
        }
        Ok(rec)
    }
}

/// Does `rec` belong to `code`? Recomputes the commitment over the server, the invite key
/// (the public half of the invite's own secret) and the nonce. Runs the KDF.
pub fn verify_invite(code: ShortCode, rec: &InviteRecord) -> bool { invite_commit(&rec.invite.server, &rec.invite.invite_pk(), &rec.nonce) == code }

/// What a friend code record says: this code belongs to `user`, reachable at `addr`. Signed
/// by `user`, and `friend_commit(user, nonce)` is the code.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FriendRecord {
    /// `PROTOCOL_VERSION`.
    pub v: u8,
    /// The code's owner.
    pub user: UserId,
    /// The commitment nonce.
    pub nonce: [u8; 16],
    /// Where to reach them (compact).
    pub addr: AddrHint,
    /// When they published it (unix ms).
    pub published_ms: u64,
    /// `user`'s signature over [`friend_sig_message`].
    pub sig: Sig64,
}

/// Friend code record direct addresses.
pub const FRIEND_DIRECT_ADDRS: usize = 4;

/// `"tincan-friend1" || record_pk || postcard((v, user, nonce, addr, published_ms))`.
pub fn friend_sig_message(record_pk: &PublicKey, v: u8, user: &UserId, nonce: &[u8; 16], addr: &AddrHint, published_ms: u64) -> Vec<u8> {
    let mut m = Vec::with_capacity(FRIEND_CONTEXT.len() + 32 + 160);
    m.extend_from_slice(FRIEND_CONTEXT);
    m.extend_from_slice(record_pk.as_bytes());
    // postcard cannot fail on these types.
    m.extend_from_slice(&postcard::to_stdvec(&(v, user, nonce, addr, published_ms)).unwrap_or_default());
    m
}

impl FriendRecord {
    /// My record for `code` (its slot is `keys`).
    pub fn new(identity: &Identity, code: &FriendCode, keys: &CodeKeys, addr: &AddrHint, published_ms: u64) -> Self {
        let addr = compact_hint(addr, FRIEND_DIRECT_ADDRS);
        let user = identity.user();
        let sig = identity.sign(&friend_sig_message(&keys.record_pk(), PROTOCOL_VERSION, &user, &code.nonce, &addr, published_ms));
        Self { v: PROTOCOL_VERSION, user, nonce: code.nonce, addr, published_ms, sig }
    }

    /// Seal under the slot's read key.
    pub fn seal(&self, keys: &CodeKeys) -> Result<Vec<u8>> { seal_bytes(&keys.read_key, &postcard::to_stdvec(self)?) }

    /// Decrypt, decode and verify the signature against the slot key (cheap). Not yet
    /// trusted: check it with [`verify_friend`].
    pub fn open(keys: &CodeKeys, sealed: &[u8]) -> Result<Self> {
        let rec: FriendRecord = decode_exact(&open_bytes(&keys.read_key, sealed)?)?;
        if rec.v != PROTOCOL_VERSION {
            return Err(CoreError::Other("friend code record from a different Tincan version".into()));
        }
        if !hint_ok(&rec.addr) {
            return Err(CoreError::Other("bad address in the friend code record".into()));
        }
        verify(&rec.user, &friend_sig_message(&keys.record_pk(), rec.v, &rec.user, &rec.nonce, &rec.addr, rec.published_ms), &rec.sig)?;
        Ok(rec)
    }
}

/// Does `rec` belong to `code`, and is it recent? Recomputes the commitment. Runs the KDF.
pub fn verify_friend(code: ShortCode, rec: &FriendRecord, now_ms: u64) -> bool {
    now_ms.saturating_sub(rec.published_ms) <= FRIEND_RECORD_MAX_AGE_MS && friend_commit(&rec.user, &rec.nonce) == code
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::ServerKeys;
    use crate::rdv::to_txt_chunks;

    fn some_code() -> ShortCode { ShortCode::from_bits(u32::from_le_bytes(random_bytes())) }

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
            let c = some_code();
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
        assert_eq!(ShortCode::from_bits(u32::MAX).bits(), CODE_MASK);
    }

    #[test]
    fn kdf_is_deterministic_and_separates_kinds() {
        let code = ShortCode::parse("K7Q-M2X").unwrap();
        let a = CodeKeys::derive(CodeKind::Invite, code);
        let b = CodeKeys::derive(CodeKind::Invite, code);
        assert_eq!(a.record_pk(), b.record_pk());
        assert_eq!(a.read_key, b.read_key);
        let f = CodeKeys::derive(CodeKind::Friend, code);
        assert_ne!(a.record_pk(), f.record_pk());
        assert_ne!(a.read_key, f.read_key);
        let other = CodeKeys::derive(CodeKind::Invite, ShortCode::parse("K7Q-M2Y").unwrap());
        assert_ne!(a.record_pk(), other.record_pk());
        // Commitments are deterministic and bind every input.
        let user = UserId([3; 32]);
        assert_eq!(friend_commit(&user, &[1; 16]), friend_commit(&user, &[1; 16]));
        let server = ServerId::from_bytes([4; 32]);
        assert_eq!(invite_commit(&server, &[5; 32], &[6; 16]), invite_commit(&server, &[5; 32], &[6; 16]));
        // Pinned: the parameters and salts are protocol constants. A change here breaks
        // every published code.
        let hex = |b: &[u8]| data_encoding::HEXLOWER.encode(&b[..8]);
        assert_eq!(hex(&a.read_key), "0825832a7810c486");
        assert_eq!(hex(a.record_pk().as_bytes()), "5933cb5443658ae2");
        assert_eq!(hex(&f.read_key), "817d7e8f239751fa");
        assert_eq!(friend_commit(&user, &[1; 16]).to_string(), "FUX-QGJ");
        assert_eq!(invite_commit(&server, &[5; 32], &[6; 16]).to_string(), "AER-KNX");
    }

    #[test]
    fn marker_rides_in_max_uses() {
        let keys = ServerKeys::derive(&[1; 32], &ServerId::from_bytes([2; 32]));
        for _ in 0..100 {
            let r = random_marker();
            let m = marker_max_uses(r);
            assert!(m & SHORT_CODE_FLAG != 0, "flag set, so the use limit is effectively unlimited");
            assert_eq!(open_marker(m), Some(r));
        }
        assert_eq!(open_marker(0), None);
        assert_eq!(open_marker(25), None);
        // The material depends on the server's secret and the marker.
        let other = ServerKeys::derive(&[3; 32], &ServerId::from_bytes([2; 32]));
        assert_ne!(invite_material(&keys.code_key, 7), invite_material(&other.code_key, 7));
        assert_ne!(invite_material(&keys.code_key, 7), invite_material(&keys.code_key, 8));
        let (seed, nonce) = invite_material(&keys.code_key, 7);
        assert_ne!(seed, nonce);
    }

    fn worst_record(nonce: [u8; 16]) -> InviteRecord {
        let v6 = |i: u16| std::net::SocketAddr::new(std::net::IpAddr::V6(std::net::Ipv6Addr::new(0x2001, 0xdb8, i, i, i, i, i, i)), 65535);
        let hint = AddrHint { relay: Some(format!("https://{}/", "r".repeat(RECORD_RELAY_CHARS - 9))), direct: (0..8).map(v6).collect() };
        let base = Invite {
            v: PROTOCOL_VERSION,
            server: ServerId::from_bytes(random_bytes()),
            name_hint: "\u{1F600}".repeat(100),
            invite_seed: random_bytes(),
            rdv_pk: [0xff; 32],
            rdv_read_key: [0xff; 32],
            bootstrap: (0..4).map(|_| (Identity::generate().user(), hint.clone())).collect(),
            expires_ms: 5,
        };
        InviteRecord::new(&base, nonce)
    }

    #[test]
    fn invite_record_round_trip_verify_and_fits_a_packet() {
        let rec = worst_record([9; 16]);
        let code = invite_commit(&rec.invite.server, &rec.invite.invite_pk(), &rec.nonce);
        let keys = CodeKeys::derive(CodeKind::Invite, code);
        assert_eq!(rec.invite.bootstrap.len(), MAX_BOOTSTRAP);
        assert_eq!(rec.invite.expires_ms, u64::MAX);
        assert!(rec.invite.bootstrap.iter().all(|(_, h)| h.direct.len() == RECORD_DIRECT_ADDRS && h.relay.is_some()));
        let sealed = rec.seal(&keys).unwrap();
        let back = InviteRecord::open(&keys, &sealed).unwrap();
        assert_eq!(back, rec);
        assert!(verify_invite(code, &back));
        // Same budget as the rendezvous record (rdv::tests::twelve_users_fit_in_a_pkarr_packet).
        let txt_bytes: usize = to_txt_chunks(&sealed).iter().map(|c| c.len() + 1).sum();
        assert!(txt_bytes < 880, "{txt_bytes} bytes of TXT data");
    }

    #[test]
    fn forged_invite_records_fail_the_commitment() {
        let rec = worst_record([9; 16]);
        let code = invite_commit(&rec.invite.server, &rec.invite.invite_pk(), &rec.nonce);
        let keys = CodeKeys::derive(CodeKind::Invite, code);
        // A code holder writes a record for another server into the slot: it opens (they
        // hold the read key) but does not belong to the code.
        let lookalike = InviteRecord { invite: Invite { server: ServerId::from_bytes(random_bytes()), ..rec.invite.clone() }, ..rec.clone() };
        assert!(InviteRecord::open(&keys, &lookalike.seal(&keys).unwrap()).is_ok());
        assert!(!verify_invite(code, &lookalike));
        // ... or the same server with another invite key, or another nonce.
        let other_key = InviteRecord { invite: Invite { invite_seed: random_bytes(), ..rec.invite.clone() }, ..rec.clone() };
        assert!(!verify_invite(code, &other_key));
        assert!(!verify_invite(code, &InviteRecord { nonce: [8; 16], ..rec.clone() }));
        // Wrong slot or tampering does not even open.
        let other = CodeKeys::derive(CodeKind::Invite, ShortCode::from_bits(code.bits() ^ 1));
        let sealed = rec.seal(&keys).unwrap();
        assert!(InviteRecord::open(&other, &sealed).is_err());
        let mut t = sealed.clone();
        t[30] ^= 1;
        assert!(InviteRecord::open(&keys, &t).is_err());
        assert!(InviteRecord::open(&keys, &[]).is_err());
    }

    #[test]
    fn friend_record_sign_verify_and_commitment() {
        let now = 1_800_000_000_000;
        let me = Identity::generate();
        let fc = FriendCode::generate(&me.user());
        assert_eq!(FriendCode::from_stored(&fc.to_stored()), Some(fc));
        let keys = CodeKeys::derive(CodeKind::Friend, fc.code);
        let addr = AddrHint { relay: Some("https://euw1-1.relay.n0.iroh-canary.iroh.link./".into()), direct: vec!["127.0.0.1:5000".parse().unwrap(); 6] };
        let rec = FriendRecord::new(&me, &fc, &keys, &addr, now);
        assert_eq!(rec.addr.direct.len(), FRIEND_DIRECT_ADDRS);
        let sealed = rec.seal(&keys).unwrap();
        assert!(to_txt_chunks(&sealed).iter().map(|c| c.len() + 1).sum::<usize>() < 880);
        let back = FriendRecord::open(&keys, &sealed).unwrap();
        assert_eq!(back, rec);
        assert!(verify_friend(fc.code, &back, now));
        assert!(!verify_friend(fc.code, &back, now + FRIEND_RECORD_MAX_AGE_MS + 1), "stale records are ignored");

        // Mallory holds the code (so the slot), signs a record for herself: the signature is
        // valid, the commitment is not.
        let mallory = Identity::generate();
        let theirs = FriendRecord::new(&mallory, &fc, &keys, &addr, now);
        let opened = FriendRecord::open(&keys, &theirs.seal(&keys).unwrap()).unwrap();
        assert!(!verify_friend(fc.code, &opened, now));
        let own_nonce = FriendRecord::new(&mallory, &FriendCode { code: fc.code, nonce: random_bytes() }, &keys, &addr, now);
        assert!(!verify_friend(fc.code, &own_nonce, now));
        // A record naming me without my signature fails to open.
        let forged = FriendRecord { user: me.user(), ..theirs.clone() };
        assert!(FriendRecord::open(&keys, &forged.seal(&keys).unwrap()).is_err());
        // A valid record cannot be moved to another slot: the signature covers the slot key.
        let other = CodeKeys::derive(CodeKind::Friend, ShortCode::from_bits(fc.code.bits() ^ 1));
        let moved = seal_bytes(&other.read_key, &postcard::to_stdvec(&rec).unwrap()).unwrap();
        assert!(FriendRecord::open(&other, &moved).is_err());
    }

    /// Prints the KDF time (run with `--nocapture` on an optimized build to tune).
    #[test]
    fn kdf_timing() {
        let t = std::time::Instant::now();
        let _ = CodeKeys::derive(CodeKind::Invite, some_code());
        eprintln!("argon2id m={KDF_MEMORY_KIB} KiB t={KDF_PASSES} p={KDF_LANES}: {:?}", t.elapsed());
    }
}
