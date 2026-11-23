//! Rendezvous record payload (sealing only; publishing to pkarr relay / DHT is in tincan-net).
//!
//! `seal` = postcard(record) encrypted with XChaCha20-Poly1305 under `read_key` with a random
//! nonce, output `nonce24 || ciphertext`; `open` reverses it. `to_txt_chunks` = base64url
//! (no padding) of the sealed bytes split into 240-char strings formatted `"<index>=<chunk>"`;
//! `from_txt_chunks` sorts by index and reassembles. Everything read back from the DHT or the
//! relay is untrusted: these functions return errors, never panic, on malformed input.

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};

use crate::keys::random_bytes;
use crate::{CoreError, Result, UserId};

/// Most online members listed in a record.
pub const MAX_ONLINE_IN_RECORD: usize = 12;
/// TXT name the record lives under.
pub const TXT_NAME: &str = "_tincan";
/// Characters of base64 per TXT string (TXT strings are limited to 255 bytes).
pub const TXT_CHUNK_CHARS: usize = 240;
/// Most TXT strings accepted when reassembling (a pkarr packet is at most 1000 bytes).
pub const MAX_TXT_CHUNKS: usize = 8;
/// Longest accepted relay URL in a record.
pub const MAX_RELAY_URL_CHARS: usize = 256;

const NONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;

/// Who hosts a server right now, for cold joiners.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RdvRecord {
    /// `PROTOCOL_VERSION`.
    pub v: u8,
    /// Host epoch.
    pub epoch: u64,
    /// The host.
    pub host: UserId,
    /// The host's home relay URL.
    pub host_relay: Option<String>,
    /// When the host published this (unix ms).
    pub published_ms: u64,
    /// Online members in succession order, at most MAX_ONLINE_IN_RECORD.
    pub online: Vec<UserId>,
}

impl RdvRecord {
    /// Encrypt under `read_key`: `nonce24 || XChaCha20-Poly1305(postcard(self))`.
    /// Errors if `online` is longer than `MAX_ONLINE_IN_RECORD` or the relay URL is too long.
    pub fn seal(&self, read_key: &[u8; 32]) -> Result<Vec<u8>> {
        self.check()?;
        let plain = postcard::to_stdvec(self)?;
        let cipher = XChaCha20Poly1305::new(&Key::from(*read_key));
        let nonce_bytes: [u8; NONCE_LEN] = random_bytes();
        let nonce = XNonce::from(nonce_bytes);
        let ct = cipher.encrypt(&nonce, plain.as_slice()).map_err(|_| CoreError::Crypto)?;
        let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    /// Decrypt and decode. `CoreError::Crypto` on a wrong key or tampering.
    pub fn open(sealed: &[u8], read_key: &[u8; 32]) -> Result<Self> {
        if sealed.len() < NONCE_LEN + TAG_LEN {
            return Err(CoreError::Crypto);
        }
        let (nonce_bytes, ct) = sealed.split_at(NONCE_LEN);
        let nonce = XNonce::try_from(nonce_bytes).map_err(|_| CoreError::Crypto)?;
        let cipher = XChaCha20Poly1305::new(&Key::from(*read_key));
        let plain = cipher.decrypt(&nonce, ct).map_err(|_| CoreError::Crypto)?;
        let (rec, rest) = postcard::take_from_bytes::<RdvRecord>(&plain)?;
        if !rest.is_empty() {
            return Err(CoreError::Other("trailing bytes in rendezvous record".into()));
        }
        rec.check()?;
        Ok(rec)
    }

    fn check(&self) -> Result<()> {
        if self.online.len() > MAX_ONLINE_IN_RECORD {
            return Err(CoreError::Other(format!("rendezvous record lists {} users (max {MAX_ONLINE_IN_RECORD})", self.online.len())));
        }
        if self.host_relay.as_ref().is_some_and(|r| r.len() > MAX_RELAY_URL_CHARS) {
            return Err(CoreError::Other("relay URL too long".into()));
        }
        Ok(())
    }
}

/// Base64url (no padding) of `sealed`, split into `"<index>=<chunk>"` strings of at most
/// `TXT_CHUNK_CHARS` base64 characters each.
pub fn to_txt_chunks(sealed: &[u8]) -> Vec<String> {
    let b64 = data_encoding::BASE64URL_NOPAD.encode(sealed);
    // base64 output is ASCII, so splitting on bytes never splits a character.
    b64.as_bytes()
        .chunks(TXT_CHUNK_CHARS)
        .enumerate()
        .map(|(i, c)| format!("{i}={}", String::from_utf8_lossy(c)))
        .collect()
}

/// Reassemble `to_txt_chunks` output (any order). Errors on missing, duplicate, malformed or
/// too many chunks, or bad base64.
pub fn from_txt_chunks(chunks: &[String]) -> Result<Vec<u8>> {
    let bad = |why: &str| CoreError::Other(format!("rendezvous TXT: {why}"));
    if chunks.is_empty() {
        return Err(bad("no chunks"));
    }
    if chunks.len() > MAX_TXT_CHUNKS {
        return Err(bad("too many chunks"));
    }
    let mut parts: Vec<(usize, &str)> = Vec::with_capacity(chunks.len());
    for c in chunks {
        let (idx, data) = c.split_once('=').ok_or_else(|| bad("missing index"))?;
        if idx.is_empty() || !idx.bytes().all(|b| b.is_ascii_digit()) {
            return Err(bad("bad index"));
        }
        let idx: usize = idx.parse().map_err(|_| bad("bad index"))?;
        if data.len() > TXT_CHUNK_CHARS {
            return Err(bad("chunk too long"));
        }
        parts.push((idx, data));
    }
    parts.sort_by_key(|(i, _)| *i);
    let mut b64 = String::with_capacity(parts.len() * TXT_CHUNK_CHARS);
    for (expected, (idx, data)) in parts.iter().enumerate() {
        if *idx != expected {
            return Err(bad("missing or duplicate chunk"));
        }
        b64.push_str(data);
    }
    data_encoding::BASE64URL_NOPAD.decode(b64.as_bytes()).map_err(|_| bad("bad base64"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PROTOCOL_VERSION;
    use crate::keys::Identity;

    fn record(n: usize) -> RdvRecord {
        RdvRecord {
            v: PROTOCOL_VERSION,
            epoch: 42,
            host: Identity::generate().user(),
            host_relay: Some("https://euw1-1.relay.n0.iroh-canary.iroh.link./".into()),
            published_ms: 1_790_000_000_000,
            online: (0..n).map(|_| Identity::generate().user()).collect(),
        }
    }

    #[test]
    fn seal_open_round_trip() {
        let key = [7u8; 32];
        let r = record(3);
        let sealed = r.seal(&key).unwrap();
        assert_eq!(RdvRecord::open(&sealed, &key).unwrap(), r);
        // fresh nonce each time
        assert_ne!(r.seal(&key).unwrap(), sealed);
    }

    #[test]
    fn open_rejects_wrong_key_and_tampering() {
        let key = [7u8; 32];
        let sealed = record(2).seal(&key).unwrap();
        assert!(matches!(RdvRecord::open(&sealed, &[8u8; 32]), Err(CoreError::Crypto)));
        for i in [0, 10, 24, 30, sealed.len() - 1] {
            let mut t = sealed.clone();
            t[i] ^= 1;
            assert!(RdvRecord::open(&t, &key).is_err(), "tamper at {i}");
        }
        assert!(RdvRecord::open(&sealed[..sealed.len() - 1], &key).is_err());
        assert!(RdvRecord::open(&[], &key).is_err());
        assert!(RdvRecord::open(&[0u8; 39], &key).is_err());
    }

    #[test]
    fn too_many_online_is_refused() {
        let key = [7u8; 32];
        assert!(record(MAX_ONLINE_IN_RECORD + 1).seal(&key).is_err());
        let mut r = record(1);
        r.host_relay = Some("x".repeat(MAX_RELAY_URL_CHARS + 1));
        assert!(r.seal(&key).is_err());
    }

    #[test]
    fn txt_chunk_round_trip() {
        for len in [1usize, 179, 180, 181, 500, 700] {
            let data: Vec<u8> = (0..len).map(|i| (i * 31 % 251) as u8).collect();
            let chunks = to_txt_chunks(&data);
            assert!(chunks.iter().all(|c| c.len() <= TXT_CHUNK_CHARS + 2));
            assert_eq!(from_txt_chunks(&chunks).unwrap(), data);
            let mut rev = chunks.clone();
            rev.reverse();
            assert_eq!(from_txt_chunks(&rev).unwrap(), data, "order must not matter");
        }
    }

    #[test]
    fn twelve_users_fit_in_a_pkarr_packet() {
        let key = [7u8; 32];
        let r = record(MAX_ONLINE_IN_RECORD);
        let sealed = r.seal(&key).unwrap();
        let chunks = to_txt_chunks(&sealed);
        // Each TXT string costs its bytes plus one length byte. The DNS packet (max 1000 B)
        // adds about 110 B of header and record overhead on top (measured: 501 B sealed ->
        // 785 B packet), so the TXT data must stay under about 880 B.
        let txt_bytes: usize = chunks.iter().map(|c| c.len() + 1).sum();
        assert!(txt_bytes < 880, "{txt_bytes} bytes of TXT data");
        assert!(chunks.len() <= MAX_TXT_CHUNKS);
        let back = from_txt_chunks(&chunks).unwrap();
        assert_eq!(RdvRecord::open(&back, &key).unwrap(), r);
    }

    #[test]
    fn from_txt_chunks_rejects_malformed() {
        let data = vec![1u8; 400];
        let chunks = to_txt_chunks(&data);
        assert!(chunks.len() >= 3);
        // missing chunk
        let missing: Vec<String> = chunks.iter().enumerate().filter(|(i, _)| *i != 1).map(|(_, c)| c.clone()).collect();
        assert!(from_txt_chunks(&missing).is_err());
        // duplicate chunk
        let mut dup = chunks.clone();
        dup.push(chunks[0].clone());
        assert!(from_txt_chunks(&dup).is_err());
        // no index, bad index, non-numeric index, signed index
        for bad in ["abc", "=abc", "x=abc", "-1=abc", "+0=abc", "1a=abc"] {
            assert!(from_txt_chunks(&[bad.to_string()]).is_err(), "accepted {bad}");
        }
        // huge index never panics or allocates
        assert!(from_txt_chunks(&["99999999999999999999999=abc".to_string()]).is_err());
        // bad base64
        assert!(from_txt_chunks(&["0=!!!!".to_string()]).is_err());
        // empty and too many
        assert!(from_txt_chunks(&[]).is_err());
        let many: Vec<String> = (0..=MAX_TXT_CHUNKS).map(|i| format!("{i}=AAAA")).collect();
        assert!(from_txt_chunks(&many).is_err());
        // overlong chunk
        assert!(from_txt_chunks(&[format!("0={}", "A".repeat(TXT_CHUNK_CHARS + 4))]).is_err());
    }
}
