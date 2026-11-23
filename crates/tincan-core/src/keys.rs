//! Signing identity and key derivation.

use crate::{CoreError, Result, ServerId, UserId};
pub use iroh_base::{PublicKey, SecretKey, Signature};

/// A 64-byte ed25519 signature as plain bytes (serde-friendly: encoded as a byte string).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sig64(pub [u8; 64]);

impl Sig64 {
    /// All zero bytes (never a valid signature).
    pub const ZERO: Sig64 = Sig64([0u8; 64]);
}

impl std::fmt::Debug for Sig64 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Sig64({})", data_encoding::HEXLOWER.encode(&self.0[..4]))
    }
}

impl serde::Serialize for Sig64 {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_bytes(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for Sig64 {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = Sig64;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result { f.write_str("64 bytes") }
            fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> std::result::Result<Sig64, E> {
                let a: [u8; 64] = v.try_into().map_err(|_| E::invalid_length(v.len(), &self))?;
                Ok(Sig64(a))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Sig64, A::Error> {
                let mut a = [0u8; 64];
                for (i, b) in a.iter_mut().enumerate() {
                    *b = seq.next_element()?.ok_or_else(|| serde::de::Error::invalid_length(i, &self))?;
                }
                Ok(Sig64(a))
            }
        }
        d.deserialize_bytes(V)
    }
}

/// The local user's signing identity (same key as the iroh endpoint).
#[derive(Clone)]
pub struct Identity {
    secret: SecretKey,
    user: UserId,
}

impl Identity {
    /// A fresh random identity.
    pub fn generate() -> Self { Self::from_secret(SecretKey::generate()) }
    /// Wrap an existing secret key.
    pub fn from_secret(secret: SecretKey) -> Self {
        let user = UserId::from_public_key(&secret.public());
        Self { secret, user }
    }
    /// From the 32 secret bytes stored in the profile (`identity.key`).
    pub fn from_bytes(b: &[u8; 32]) -> Self { Self::from_secret(SecretKey::from_bytes(b)) }
    /// The 32 secret bytes, for storing in the profile.
    pub fn to_bytes(&self) -> [u8; 32] { self.secret.to_bytes() }
    /// The public user id.
    pub fn user(&self) -> UserId { self.user }
    /// The secret key (for building the iroh endpoint).
    pub fn secret(&self) -> &SecretKey { &self.secret }
    /// ed25519 signature over `msg`.
    pub fn sign(&self, msg: &[u8]) -> Sig64 { Sig64(self.secret.sign(msg).to_bytes()) }
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Identity({:?})", self.user)
    }
}

/// Sign `msg` with an arbitrary secret key.
pub fn sign_with(sk: &SecretKey, msg: &[u8]) -> Sig64 { Sig64(sk.sign(msg).to_bytes()) }

/// Verify an ed25519 signature by `signer` over `msg`.
pub fn verify(signer: &UserId, msg: &[u8], sig: &Sig64) -> Result<()> {
    let pk = signer.public_key()?;
    pk.verify(msg, &Signature::from_bytes(&sig.0)).map_err(|_| CoreError::BadSignature)
}

/// Keys every member of a server derives from `server_secret`.
#[derive(Clone)]
pub struct ServerKeys {
    /// Signs the rendezvous pkarr record. Public half goes in invites.
    pub rdv_sk: SecretKey,
    /// Encrypts the rendezvous record (XChaCha20-Poly1305). Goes in invites.
    pub rdv_read_key: [u8; 32],
    /// MAC key that proves membership in `Hello`. Never in invites.
    pub hello_key: [u8; 32],
    /// Masks the short invite code carried in an `InviteCreate` (`code::seal_code`). Never
    /// in invites.
    pub code_key: [u8; 32],
}

impl std::fmt::Debug for ServerKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ServerKeys(rdv_pk {})", self.rdv_pk().fmt_short())
    }
}

impl ServerKeys {
    /// `blake3::derive_key(context, server_secret || server_id)` for each sub-key.
    pub fn derive(server_secret: &[u8; 32], server: &ServerId) -> Self {
        let mut material = [0u8; 64];
        material[..32].copy_from_slice(server_secret);
        material[32..].copy_from_slice(&server.0);
        let rdv_seed = blake3::derive_key("tincan v1 rendezvous signing key", &material);
        Self {
            rdv_sk: SecretKey::from_bytes(&rdv_seed),
            rdv_read_key: blake3::derive_key("tincan v1 rendezvous record encryption key", &material),
            hello_key: blake3::derive_key("tincan v1 hello mac key", &material),
            code_key: blake3::derive_key("tincan v1 invite code mask key", &material),
        }
    }
    /// Public key of the rendezvous record (what invites carry as `rdv_pk`).
    pub fn rdv_pk(&self) -> PublicKey { self.rdv_sk.public() }
}

/// MAC proving the dialer knows the server's hello_key:
/// `keyed_blake3(hello_key, server || client || host || nonce)`.
pub fn hello_mac(hello_key: &[u8; 32], server: &ServerId, client: &UserId, host: &UserId, nonce: &[u8; 16]) -> [u8; 32] {
    let mut h = blake3::Hasher::new_keyed(hello_key);
    h.update(&server.0);
    h.update(&client.0);
    h.update(&host.0);
    h.update(nonce);
    *h.finalize().as_bytes()
}

/// Check a `Hello` MAC in constant time. `client` is the authenticated QUIC peer id,
/// `host` is the local endpoint id of whoever received the Hello.
pub fn verify_hello_mac(
    hello_key: &[u8; 32],
    server: &ServerId,
    client: &UserId,
    host: &UserId,
    nonce: &[u8; 16],
    mac: &[u8; 32],
) -> bool {
    let expected = blake3::Hash::from_bytes(hello_mac(hello_key, server, client, host, nonce));
    // blake3::Hash equality is constant time.
    expected == blake3::Hash::from_bytes(*mac)
}

/// Random bytes helper.
pub fn random_bytes<const N: usize>() -> [u8; N] {
    use rand::RngCore;
    let mut b = [0u8; N];
    rand::rng().fill_bytes(&mut b);
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_round_trip_and_sign_verify() {
        let id = Identity::generate();
        let again = Identity::from_bytes(&id.to_bytes());
        assert_eq!(again.user(), id.user());
        let sig = id.sign(b"hello");
        verify(&id.user(), b"hello", &sig).unwrap();
        assert!(matches!(verify(&id.user(), b"hellO", &sig), Err(CoreError::BadSignature)));
        let other = Identity::generate();
        assert!(verify(&other.user(), b"hello", &sig).is_err());
        let mut bad = sig;
        bad.0[10] ^= 1;
        assert!(verify(&id.user(), b"hello", &bad).is_err());
        assert_eq!(sign_with(id.secret(), b"hello"), sig);
        assert!(format!("{id:?}").starts_with("Identity(UserId("));
    }

    #[test]
    fn sig64_serde_round_trip() {
        let id = Identity::generate();
        let sig = id.sign(b"x");
        let bytes = postcard::to_stdvec(&sig).unwrap();
        assert_eq!(bytes.len(), 65); // varint length 64 + bytes
        let back: Sig64 = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back, sig);
        // wrong length is an error, not a panic
        let short = postcard::to_stdvec(&serde_bytes_like(&[1u8; 10])).unwrap();
        assert!(postcard::from_bytes::<Sig64>(&short).is_err());
    }

    fn serde_bytes_like(b: &[u8]) -> Vec<u8> { b.to_vec() }

    #[test]
    fn server_keys_are_deterministic_and_distinct() {
        let secret = [9u8; 32];
        let server = ServerId::from_bytes([1; 32]);
        let a = ServerKeys::derive(&secret, &server);
        let b = ServerKeys::derive(&secret, &server);
        assert_eq!(a.rdv_pk(), b.rdv_pk());
        assert_eq!(a.rdv_read_key, b.rdv_read_key);
        assert_eq!(a.hello_key, b.hello_key);
        assert_ne!(a.rdv_read_key, a.hello_key);
        let c = ServerKeys::derive(&secret, &ServerId::from_bytes([2; 32]));
        assert_ne!(a.rdv_pk(), c.rdv_pk());
        assert_ne!(a.hello_key, c.hello_key);
        let d = ServerKeys::derive(&[8u8; 32], &server);
        assert_ne!(a.hello_key, d.hello_key);
        assert!(format!("{a:?}").starts_with("ServerKeys("));
    }

    #[test]
    fn hello_mac_binds_every_field() {
        let key = [3u8; 32];
        let server = ServerId::from_bytes([1; 32]);
        let client = Identity::generate().user();
        let host = Identity::generate().user();
        let nonce = [5u8; 16];
        let mac = hello_mac(&key, &server, &client, &host, &nonce);
        assert!(verify_hello_mac(&key, &server, &client, &host, &nonce, &mac));
        assert!(!verify_hello_mac(&[4u8; 32], &server, &client, &host, &nonce, &mac));
        assert!(!verify_hello_mac(&key, &ServerId::from_bytes([2; 32]), &client, &host, &nonce, &mac));
        assert!(!verify_hello_mac(&key, &server, &host, &client, &nonce, &mac));
        assert!(!verify_hello_mac(&key, &server, &client, &host, &[6u8; 16], &mac));
        let mut bad = mac;
        bad[0] ^= 1;
        assert!(!verify_hello_mac(&key, &server, &client, &host, &nonce, &bad));
    }

    #[test]
    fn random_bytes_differ() {
        let a: [u8; 32] = random_bytes();
        let b: [u8; 32] = random_bytes();
        assert_ne!(a, b);
    }
}
