Verified probe (compiles on Rust 1.99.1 MSVC; live runs on 2026-11-23): C:/Users/andri/AppData/Local/Temp/claude/d3bbc0386af2202e/d39a4a52-4271-48fa-9546-1005d34914ef/scratchpad/research/group-protocol/rdv-probe/src/main.rs
Modes: offline (sizes and negentropy), live (DHT and relay), resolve, fastclock, dial, netreport; plus tests dm_roundtrip and bench_verify_100k.

Cargo.toml (all compiled together):
iroh = { version = "=1.5.0", features = ["unstable-net-report"] }
iroh-dns = "=1.5.0"            # pkarr SignedPacket
n0-mainline = "=0.8.0"         # BEP44 DHT client
iroh-mdns-address-lookup = "=0.6.1"
chacha20poly1305 = "=0.11.0"
x25519-dalek = { version = "=3.0.0", features = ["static_secrets"] }
blake3 = "1.8"
data-encoding = "2"
# compiled but not recommended for v1:
iroh-gossip = "=0.103.0"
iroh-mainline-address-lookup = "=0.6.1"   # pulls in n0-mainline 0.7.x, a second copy
negentropy = "=0.5.3"

Key derivation:
fn rdv_keys(secret: &[u8;32]) -> (iroh::SecretKey, [u8;32]) {
  let sk = iroh::SecretKey::from_bytes(&blake3::derive_key("tincan v1 rendezvous signing key", secret));
  let ek = blake3::derive_key("tincan v1 rendezvous record encryption key", secret);
  (sk, ek)
}
In Tincan, pass server_secret || server_id as the key material.

Seal the record (chacha20poly1305 0.11 API):
use chacha20poly1305::{XChaCha20Poly1305, XNonce, aead::{Aead, KeyInit}};
let cipher = XChaCha20Poly1305::new_from_slice(&ek)?;
let nonce = XNonce::try_from(&nonce24[..])?;
let ct = cipher.encrypt(&nonce, plaintext)?;   // store nonce24 || ct

Pkarr packet (TXT strings are limited to 255 bytes, so chunk them):
let chunks: Vec<String> = b64.as_bytes().chunks(240).enumerate()
  .map(|(i,c)| format!("{i}={}", std::str::from_utf8(c).unwrap())).collect();
let sp = iroh_dns::pkarr::SignedPacket::from_txt_strings(&rk, "_tincan", chunks, 30)?;
// read back: sp.txt_records("_tincan"), split on '=', sort by index, concatenate, decode

Explicit seq (write rule seq = max(now_us, seen+1)):
let item = n0_mainline::MutableItem::new(rk.as_signing_key(), sp.encoded_packet(), seq as i64, None);
let sp2 = SignedPacket::from_parts_unchecked(item.key(), item.signature(),
  iroh_dns::pkarr::Timestamp::from_micros(item.seq() as u64), item.value())?;
SignedPacket::from_bytes(sp2.as_bytes())?;   // verifies

DHT publish and first-item resolve:
let dht = n0_mainline::Dht::client()?;       // must be inside a tokio runtime
dht.put_mutable(item, Some(seen_seq)).await?;   // Err(NotMostRecent) if the stored seq is higher
let mut s = dht.get_mutable(rk.public().as_bytes(), None, None).await?;
let first = futures_lite::StreamExt::next(&mut s).await;   // about 0.5 s cold; keep draining up to 3 s for a higher seq
// MutableItem -> SignedPacket: SignedPacket::from_parts_unchecked(item.key(), item.signature(), Timestamp::from_micros(item.seq() as u64), item.value())

n0 pkarr relay (optional accelerator):
let c = iroh::address_lookup::PkarrRelayClient::new(
  url::Url::parse(iroh::address_lookup::N0_DNS_PKARR_RELAY_PROD)?,
  ep.tls_config().clone(), ep.dns_resolver()?.clone());
c.publish(&sp).await?;
let sp = c.resolve(rdv_pk).await?;

LAN discovery:
let mdns = iroh_mdns_address_lookup::MdnsAddressLookup::builder()
  .service_name("tincan1").discovery_cadence(Duration::from_secs(5)).build(ep.id())?;
ep.address_lookup()?.add(mdns.clone());
let mut ev = mdns.subscribe().await;   // DiscoveryEvent::Discovered { endpoint_info, .. } -> endpoint_info.endpoint_id

Connectivity inputs:
let report = ep.net_report().initialized().await;   // udp_v4, mapping_varies_by_dest_ipv4, global_v4/v6, preferred_relay, relay_latency
for p in conn.paths().iter() { p.is_selected(); p.is_ip(); p.is_relay(); p.rtt(); }
Endpoint::builder(presets::N0).alpns(vec![ALPN_SERVER.to_vec(), ALPN_DM.to_vec()]).bind().await?;
ep.online().await;
ep.connect(endpoint_id_or_addr, ALPN_SERVER).await;   // always wrap in tokio::time::timeout(4 s)

Sealed DM (tested):
eph = StaticSecret::from(random32); shared = eph.diffie_hellman(&PublicKey::from(recip_prekey));
key = blake3::derive_key("tincan v1 dm sealed envelope", shared || eph_pub || recip_prekey);
out = eph_pub || nonce24 || XChaCha20Poly1305(key).encrypt(pt)   // 72 B overhead

WIRE FORMAT (v1)
Framing is postcard with a u32 little-endian length prefix and a maximum frame of 1 MiB. Each connection has one long-lived bidirectional stream for control and sync.
pub const ALPN_SERVER: &[u8] = b"tincan/server/1";
pub const ALPN_DM: &[u8] = b"tincan/dm/1";
type UserId = [u8;32];      // iroh EndpointId
type EventId = [u8;32];     // blake3(postcard(Event))
type ServerId = EventId;    // Genesis event id

struct Event { v: u8, server: ServerId, author: UserId, seq: u64, hlc: u64, body: Body }
struct SignedEvent { bytes: Vec<u8>, sig: [u8;64] }   // ed25519("tincan-ev1" || bytes)
enum Body {
  Genesis { name: String, owner: UserId, nonce: [u8;16] },
  Profile { name: String, avatar: Option<[u8;32]>, dm_prekey: [u8;32] },
  Message { channel: EventId, text: String, reply_to: Option<EventId>, attachments: Vec<Attachment> },
  Edit { target: EventId, text: String },
  Delete { target: EventId },
  React { target: EventId, emoji: String, add: bool },
  ChannelCreate { name: String, kind: ChannelKind, position: u32 },
  ChannelUpdate { channel: EventId, name: Option<String>, position: Option<u32>, archived: Option<bool> },
  ServerUpdate { name: Option<String>, icon: Option<[u8;32]> },
  InviteCreate { invite_pk: [u8;32], expires_ms: u64, max_uses: u32 },
  InviteRevoke { invite_pk: [u8;32] },
  MemberAdmit { user: UserId, invite_pk: [u8;32], from_seq: u64 },
  MemberLeave { last_seq: u64 },
  Kick { user: UserId, last_seq: u64, reason: String },
  Ban { user: UserId, last_seq: u64, reason: String },
  Unban { user: UserId },
  RoleSet { user: UserId, admin: bool, last_seq: u64 },   // owner only
}
struct Attachment { blob: [u8;32], size: u64, name: String, mime: String }

enum Ctl {
  Hello { server: ServerId, nonce: [u8;16], mac: [u8;32], epoch_seen: u64, status: Status },
  JoinRequest { server: ServerId, invite_pk: [u8;32], next_seq: u64, profile: SignedEvent, ts_ms: u64, invite_sig: [u8;64] },
  Welcome { claim: SignedHostClaim, roster: Roster, your_next_seq: u64 },
  Redirect { claim: SignedHostClaim, host: AddrHint },
  Reject { code: u16 },
  JoinAccepted { server_secret: [u8;32], genesis: SignedEvent, admit: SignedEvent },
  SyncHave { have: Vec<(UserId, Vec<(u64,u64)>)> },
  Events { events: Vec<SignedEvent>, more: bool },
  Publish { event: SignedEvent },
  PublishAck { id: EventId, ok: bool, code: u16 },
  VvDigest { digest: [u8;32] },
  Status(Status),
  Roster(Roster),
  RosterDelta { epoch: u64, rev: u32, up: Vec<RosterEntry>, down: Vec<UserId>, succession: Vec<UserId> },
  Tick { epoch: u64, t_ms: u64 },
  Handoff { claim: SignedHostClaim, roster: Roster },
  HandoffAck { epoch: u64 },
  HostMoved { claim: SignedHostClaim, host: AddrHint },
  HostCompare { claim: SignedHostClaim, connected: u32, score: u32 },
  Ephemeral { channel: EventId, kind: u8, data: Vec<u8> },   // typing, voice state
}
struct Status { hosting_ok: bool, ac_power: bool, battery_saver: bool, metered: bool, nat: u8, uptime_s: u32, avg_session_s: u32, relay_rtt_ms: u16, synced: bool, voice: Option<EventId> }
struct HostClaim { server: ServerId, epoch: u64, host: UserId, prev: Option<UserId>, issued_ms: u64 }
struct SignedHostClaim { claim: HostClaim, sig: [u8;64] }
struct Roster { epoch: u64, rev: u32, online: Vec<RosterEntry>, succession: Vec<UserId> }
struct RosterEntry { user: UserId, addr: AddrHint, voice: Option<EventId>, since_ms: u64 }
struct AddrHint { relay: Option<String>, direct: Vec<SocketAddr> }   // -> iroh::EndpointAddr
struct RdvRecord { v: u8, epoch: u64, host: UserId, host_relay: Option<String>, published_ms: u64, online: Vec<UserId> }
struct Invite { v: u8, server: ServerId, name_hint: String, invite_seed: [u8;16], rdv_pk: [u8;32], rdv_read_key: [u8;32], bootstrap: Vec<UserId>, expires_ms: u64 }

mac = blake3::keyed_hash(hello_key, server || client_ep || host_ep || nonce)
invite_sig = ed25519(invite_sk, "tincan-join1" || server || joiner || host_ep || next_seq || ts_ms)

Timings:
- Tick 2 s; suspect at 6 s; dead at 10 s.
- Successor stagger 3 s per rank; handoff budget 2 s.
- Dial timeout 4 s; solo host at 6 s.
- Solo re-dial and record re-resolve every 60 s, backing off to 5 min.
- Host record publish: on change (30 s debounce), relay every 5 min, DHT every 30 min; host re-resolve every 5 min.
- Roster deltas debounced 250 ms, full roster every 5 min; Status every 30 s.
- VvDigest every 60 s.
- Hysteresis: 30% margin for 10 min, at least 30 min between migrations.
- HLC future-skew buffer 5 min.