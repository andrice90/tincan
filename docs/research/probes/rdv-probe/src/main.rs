//! Throwaway probe for Tincan research: verifies APIs compile and measures
//! rendezvous-record publish/resolve latency on mainline DHT and n0's pkarr relay.
use std::time::Instant;

use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit},
};
use data_encoding::BASE64URL_NOPAD;
use iroh::{SecretKey, Watcher as _, address_lookup::PkarrRelayClient, endpoint::presets};
use iroh_dns::pkarr::SignedPacket;
use n0_mainline::{Dht, MutableItem};

const RDV_SIGN_CTX: &str = "tincan v1 rendezvous signing key";
const RDV_ENC_CTX: &str = "tincan v1 rendezvous record encryption key";
const TXT_NAME: &str = "_tincan";

fn rdv_keys(server_secret: &[u8; 32]) -> (SecretKey, [u8; 32]) {
    let sk = SecretKey::from_bytes(&blake3::derive_key(RDV_SIGN_CTX, server_secret));
    let ek = blake3::derive_key(RDV_ENC_CTX, server_secret);
    (sk, ek)
}

fn random32() -> [u8; 32] {
    SecretKey::generate().to_bytes()
}

/// plaintext: [ver u8][epoch u64][host 32][n u8][n x 32 online ids][relay url utf8]
fn build_plaintext(epoch: u64, host: [u8; 32], online: &[[u8; 32]], relay: &str) -> Vec<u8> {
    let mut v = vec![1u8];
    v.extend_from_slice(&epoch.to_be_bytes());
    v.extend_from_slice(&host);
    v.push(online.len() as u8);
    for id in online {
        v.extend_from_slice(id);
    }
    v.extend_from_slice(relay.as_bytes());
    v
}

fn seal(ek: &[u8; 32], plaintext: &[u8]) -> Vec<u8> {
    let cipher = XChaCha20Poly1305::new_from_slice(ek).expect("32 byte key");
    let nonce_bytes: [u8; 24] = random32()[..24].try_into().unwrap();
    let nonce = XNonce::try_from(&nonce_bytes[..]).expect("24 byte nonce");
    let ct = cipher.encrypt(&nonce, plaintext).expect("encrypt");
    let mut out = nonce_bytes.to_vec();
    out.extend_from_slice(&ct);
    out
}

fn open(ek: &[u8; 32], sealed: &[u8]) -> Vec<u8> {
    let cipher = XChaCha20Poly1305::new_from_slice(ek).expect("32 byte key");
    let nonce = XNonce::try_from(&sealed[..24]).expect("24 byte nonce");
    cipher.decrypt(&nonce, &sealed[24..]).expect("decrypt")
}

fn to_signed_packet(rk: &SecretKey, sealed: &[u8]) -> SignedPacket {
    let b64 = BASE64URL_NOPAD.encode(sealed);
    // TXT character-strings are limited to 255 bytes: chunk and index them.
    let chunks: Vec<String> = b64
        .as_bytes()
        .chunks(240)
        .enumerate()
        .map(|(i, c)| format!("{i}={}", std::str::from_utf8(c).unwrap()))
        .collect();
    SignedPacket::from_txt_strings(rk, TXT_NAME, chunks, 30).expect("signed packet")
}

fn from_signed_packet(sp: &SignedPacket) -> Vec<u8> {
    let mut parts: Vec<(usize, String)> = sp
        .txt_records(TXT_NAME)
        .into_iter()
        .map(|s| {
            let (i, d) = s.split_once('=').unwrap();
            (i.parse().unwrap(), d.to_string())
        })
        .collect();
    parts.sort();
    let b64: String = parts.into_iter().map(|(_, d)| d).collect();
    BASE64URL_NOPAD.decode(b64.as_bytes()).unwrap()
}

fn to_mutable(sp: &SignedPacket) -> MutableItem {
    MutableItem::new_signed_unchecked(
        *sp.public_key().as_bytes(),
        sp.signature().to_bytes(),
        sp.encoded_packet(),
        sp.timestamp().as_micros() as i64,
        None,
    )
}

/// mode 0: b missing the newest `diff` items (member was offline)
/// mode 1: two-sided, scattered: a lacks every k-th item, b lacks every (k+1)-th item
fn negentropy_probe(n: usize, diff: usize, mode: u8) {
    use negentropy::{Id, Negentropy, NegentropyStorageVector};
    let mut a = NegentropyStorageVector::new();
    let mut b = NegentropyStorageVector::new();
    let k = if diff == 0 { usize::MAX } else { (n / diff).max(2) };
    for i in 0..n {
        let id = *blake3::hash(&(i as u64).to_le_bytes()).as_bytes();
        let ts = 1_700_000_000_000u64 + i as u64 * 1000;
        let (in_a, in_b) = match mode {
            0 => (true, i < n - diff),
            1 => (i % k != 0, i % (k + 1) != 0),
            _ => if i < n - diff { (true, true) } else { (i % 2 == 0, i % 2 == 1) },
        };
        if in_a {
            a.insert(ts, Id::from_slice(&id).unwrap()).unwrap();
        }
        if in_b {
            b.insert(ts, Id::from_slice(&id).unwrap()).unwrap();
        }
    }
    a.seal().unwrap();
    b.seal().unwrap();
    let t = Instant::now();
    let mut client = Negentropy::borrowed(&b, 0).unwrap();
    let mut server = Negentropy::borrowed(&a, 0).unwrap();
    let mut msg = client.initiate().unwrap();
    let mut bytes = msg.len();
    let mut rounds = 1;
    let mut need = Vec::new();
    let mut have = Vec::new();
    loop {
        let resp = server.reconcile(&msg).unwrap();
        bytes += resp.len();
        match client.reconcile_with_ids(&resp, &mut have, &mut need).unwrap() {
            Some(next) => {
                bytes += next.len();
                rounds += 1;
                msg = next;
            }
            None => break,
        }
    }
    let id_bytes = (need.len() + have.len()) * 32;
    println!(
        "negentropy n={n} diff={diff} mode={mode}: rounds={rounds} total_bytes={bytes} (of which ids={id_bytes}, overhead={}) need={} have={} in {:?}",
        bytes.saturating_sub(id_bytes),
        need.len(),
        have.len(),
        t.elapsed()
    );
}

/// Compile-only checks for crates we may recommend.
#[allow(dead_code)]
async fn compile_checks(ep: iroh::Endpoint) {
    use futures_lite::StreamExt;
    // iroh-gossip 0.103
    let gossip = iroh_gossip::Gossip::builder().spawn(ep.clone());
    let _router = iroh::protocol::Router::builder(ep.clone())
        .accept(iroh_gossip::ALPN, gossip.clone())
        .spawn();
    let topic = gossip
        .subscribe(iroh_gossip::TopicId::from_bytes([7u8; 32]), vec![])
        .await
        .unwrap();
    let (sender, mut receiver) = topic.split();
    sender.broadcast(bytes_of(b"hi")).await.unwrap();
    if let Some(Ok(ev)) = receiver.next().await {
        match ev {
            iroh_gossip::Event::Received(m) => println!("{:?}", m.content),
            iroh_gossip::Event::NeighborUp(id) => println!("up {id}"),
            _ => {}
        }
    }
    // iroh-mdns-address-lookup 0.6.1
    let mdns = iroh_mdns_address_lookup::MdnsAddressLookup::builder()
        .service_name("tincan1")
        .discovery_cadence(std::time::Duration::from_secs(5))
        .build(ep.id())
        .unwrap();
    ep.address_lookup().unwrap().add(mdns.clone());
    let mut evs = mdns.subscribe().await;
    if let Some(iroh_mdns_address_lookup::DiscoveryEvent::Discovered { endpoint_info, .. }) =
        evs.next().await
    {
        println!("lan peer {:?}", endpoint_info.endpoint_id);
    }
    // iroh-mainline-address-lookup 0.6.1 (resolve-only, no publish)
    let dht_lookup = iroh_mainline_address_lookup::DhtAddressLookup::builder()
        .no_publish()
        .build()
        .unwrap();
    ep.address_lookup().unwrap().add(dht_lookup);
}

const PROBE_ALPN: &[u8] = b"tincan/probe/0";

/// Measures: online connect time (relay then direct), and how long a dial to a
/// recently-offline peer and to a never-seen id takes to fail under presets::N0.
async fn dial_probe() {
    use std::time::Duration;
    let b = iroh::Endpoint::builder(presets::N0)
        .alpns(vec![PROBE_ALPN.to_vec()])
        .bind()
        .await
        .unwrap();
    let t = Instant::now();
    b.online().await;
    println!("B online (relay connected) in {:?}", t.elapsed());
    let b_id = b.id();
    let b2 = b.clone();
    let acc = tokio::spawn(async move {
        while let Some(inc) = b2.accept().await {
            if let Ok(conn) = inc.await {
                if let Ok((mut send, _recv)) = conn.accept_bi().await {
                    let _ = send.write_all(b"pong").await;
                    let _ = send.finish();
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    });
    // give the pkarr publisher time to publish B's relay url to dns.iroh.link
    tokio::time::sleep(Duration::from_secs(4)).await;

    let a = iroh::Endpoint::bind(presets::N0).await.unwrap();
    a.online().await;
    let t = Instant::now();
    match a.connect(b_id, PROBE_ALPN).await {
        Ok(conn) => {
            println!("A->B connect by id only (online) OK in {:?}", t.elapsed());
            let (mut send, mut recv) = conn.open_bi().await.unwrap();
            send.write_all(b"ping").await.unwrap();
            send.finish().unwrap();
            let r = recv.read_to_end(16).await.unwrap();
            println!("first round trip done in {:?}: {:?}", t.elapsed(), String::from_utf8_lossy(&r));
            for _ in 0..10 {
                let paths: Vec<String> = conn
                    .paths()
                    .iter()
                    .map(|p| format!("sel={} ip={} relay={} rtt={:?}", p.is_selected(), p.is_ip(), p.is_relay(), p.rtt()))
                    .collect();
                println!("  t={:?} paths={paths:?}", t.elapsed());
                if conn.paths().iter().any(|p| p.is_selected() && p.is_ip()) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            conn.close(0u32.into(), b"bye");
        }
        Err(e) => println!("A->B connect failed {e:?} in {:?}", t.elapsed()),
    }

    // Now take B offline and dial again from a fresh endpoint (stale record at n0 DNS).
    acc.abort();
    b.close().await;
    let c = iroh::Endpoint::bind(presets::N0).await.unwrap();
    c.online().await;
    let t = Instant::now();
    let r = tokio::time::timeout(Duration::from_secs(60), c.connect(b_id, PROBE_ALPN)).await;
    println!("C->B (B offline, stale record) result={:?} after {:?}", r.map(|x| x.map(|_| ())), t.elapsed());

    let never = SecretKey::generate().public();
    let t = Instant::now();
    let r = tokio::time::timeout(Duration::from_secs(60), c.connect(never, PROBE_ALPN)).await;
    println!("C->never-seen id result={:?} after {:?}", r.map(|x| x.map(|_| ())), t.elapsed());
    c.close().await;
    a.close().await;
}

fn bytes_of(b: &[u8]) -> bytes::Bytes {
    bytes::Bytes::copy_from_slice(b)
}

#[tokio::main]
async fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "offline".into());

    if mode == "offline" {
        negentropy_probe(100_000, 50, 0);
        negentropy_probe(100_000, 5_000, 0);
        negentropy_probe(100_000, 100_000, 0); // brand-new joiner
        negentropy_probe(100_000, 200, 1); // two-sided scattered
        negentropy_probe(100_000, 5_000, 1);
        negentropy_probe(100_000, 1_000, 2);
        negentropy_probe(100_000, 10_000, 2);
        reseq_probe();
    }

    if mode == "fastclock" {
        let (rk, _) = rdv_keys(&random32());
        let sp = SignedPacket::from_txt_strings(&rk, TXT_NAME, ["0=abc"], 30).unwrap();
        let now = sp.timestamp().as_micros();
        let dht = Dht::client().unwrap();
        dht.bootstrapped().await.unwrap();
        let fut = resign_with_seq(&rk, &sp, now + 3_600_000_000);
        let t = Instant::now();
        let r1 = dht.put_mutable(to_mutable(&fut), None).await;
        println!("put future-seq -> {:?} in {:?}", r1.map(|_| ()), t.elapsed());
        let t = Instant::now();
        let r2 = dht.put_mutable(to_mutable(&sp), None).await;
        println!("put now-seq (older than stored) -> {:?} in {:?}", r2.map(|_| ()), t.elapsed());
        let next = resign_with_seq(&rk, &sp, now + 3_600_000_001);
        let t = Instant::now();
        let r3 = dht.put_mutable(to_mutable(&next), Some((now + 3_600_000_000) as i64)).await;
        println!("put seen+1 with cas=seen -> {:?} in {:?}", r3.map(|_| ()), t.elapsed());
        let got = dht.get_mutable_most_recent(rk.public().as_bytes(), None).await.unwrap();
        println!("most recent seq - now = {:?} us", got.map(|i| i.seq() - now as i64));
        return;
    }
    if mode == "dial" {
        dial_probe().await;
        return;
    }

    // Fixed seed so a later run ("resolve") can check record survival without republishing.
    let server_secret = blake3::derive_key("tincan research probe fixed seed", b"2026-11-22");
    let (rk, ek) = rdv_keys(&server_secret);
    if mode == "resolve" {
        let t = Instant::now();
        let dht = Dht::client().unwrap();
        let mut s = dht.get_mutable(rk.public().as_bytes(), None, None).await.unwrap();
        use futures_lite::StreamExt;
        let first = s.next().await;
        println!("resolve-only first item found={} after {:?}", first.is_some(), t.elapsed());
        if let Some(item) = first {
            let age_s = (iroh_dns::pkarr::Timestamp::now().as_micros() as i64 - item.seq()) / 1_000_000;
            println!("record age {age_s}s");
        }
        return;
    }
    let host = random32();
    let online: Vec<[u8; 32]> = (0..12).map(|_| random32()).collect();
    let pt = build_plaintext(7, host, &online, "https://use1-1.relay.n0.iroh.link./");
    let sealed = seal(&ek, &pt);
    let sp = to_signed_packet(&rk, &sealed);
    println!(
        "plaintext={}B sealed={}B signed_packet={}B dns_packet={}B (max {})",
        pt.len(),
        sealed.len(),
        sp.as_bytes().len(),
        sp.encoded_packet().len(),
        SignedPacket::MAX_BYTES
    );
    assert_eq!(open(&ek, &from_signed_packet(&sp)), pt);
    println!("rendezvous key z32={}", rk.public().to_z32());

    if mode == "netreport" {
        let ep = iroh::Endpoint::bind(presets::N0).await.unwrap();
        let t = Instant::now();
        let r = ep.net_report().initialized().await;
        println!("net_report in {:?}: {:#?}", t.elapsed(), r);
        return;
    }
    if mode != "live" {
        return;
    }

    // --- mainline DHT ---
    let t = Instant::now();
    let dht_a = Dht::client().unwrap();
    let ok = dht_a.bootstrapped().await.unwrap();
    println!("dht A bootstrapped={ok} in {:?}", t.elapsed());
    let t = Instant::now();
    let res = dht_a.put_mutable(to_mutable(&sp), None).await;
    println!("dht put_mutable -> {:?} in {:?}", res.map(|_| ()), t.elapsed());

    for i in 0..3 {
        // first-item latency from a cold client
        let t = Instant::now();
        let dht_c = Dht::client().unwrap();
        let mut s = dht_c.get_mutable(rk.public().as_bytes(), None, None).await.unwrap();
        {
            use futures_lite::StreamExt;
            let first = s.next().await;
            println!("cold dht first-item #{i} found={} in {:?}", first.is_some(), t.elapsed());
        }
        drop(s);
        drop(dht_c);
        let t = Instant::now();
        let dht_b = Dht::client().unwrap();
        let got = dht_b
            .get_mutable_most_recent(rk.public().as_bytes(), None)
            .await
            .unwrap();
        let el = t.elapsed();
        match got {
            Some(item) => {
                let sp2 = SignedPacket::from_parts_unchecked(
                    item.key(),
                    item.signature(),
                    iroh_dns::pkarr::Timestamp::from_micros(item.seq() as u64),
                    item.value(),
                );
                let sp2 = SignedPacket::from_bytes(sp2.unwrap().as_bytes()).unwrap();
                assert_eq!(open(&ek, &from_signed_packet(&sp2)), pt);
                println!("cold dht resolve #{i} (incl. bootstrap) OK in {el:?}");
            }
            None => println!("cold dht resolve #{i} NOT FOUND in {el:?}"),
        }
    }
    let t = Instant::now();
    let warm = dht_a
        .get_mutable_most_recent(rk.public().as_bytes(), None)
        .await
        .unwrap();
    println!("warm dht resolve found={} in {:?}", warm.is_some(), t.elapsed());

    // --- n0 pkarr relay ---
    let ep = iroh::Endpoint::bind(presets::Minimal).await.unwrap();
    let client = PkarrRelayClient::new(
        url::Url::parse(iroh::address_lookup::N0_DNS_PKARR_RELAY_PROD).unwrap(),
        ep.tls_config().clone(),
        ep.dns_resolver().unwrap().clone(),
    );
    let t = Instant::now();
    let r = client.publish(&sp).await;
    println!("n0 pkarr relay publish -> {:?} in {:?}", r.map(|_| ()), t.elapsed());
    for i in 0..2 {
        let t = Instant::now();
        let r = client.resolve(rk.public()).await;
        match r {
            Ok(sp3) => {
                assert_eq!(open(&ek, &from_signed_packet(&sp3)), pt);
                println!("n0 pkarr relay resolve #{i} OK in {:?}", t.elapsed());
            }
            Err(e) => println!("n0 pkarr relay resolve #{i} err {e:?} in {:?}", t.elapsed()),
        }
    }
}

/// Sealed-box style DM envelope: ephemeral X25519 -> recipient dm_prekey,
/// key = blake3::derive_key(ctx, shared || eph_pub || recip_pub), XChaCha20-Poly1305.
#[allow(dead_code)]
pub fn dm_seal(recip_prekey: &[u8; 32], plaintext: &[u8]) -> Vec<u8> {
    use x25519_dalek::{PublicKey, StaticSecret};
    let eph = StaticSecret::from(random32());
    let eph_pub = PublicKey::from(&eph);
    let shared = eph.diffie_hellman(&PublicKey::from(*recip_prekey));
    let mut ikm = Vec::with_capacity(96);
    ikm.extend_from_slice(shared.as_bytes());
    ikm.extend_from_slice(eph_pub.as_bytes());
    ikm.extend_from_slice(recip_prekey);
    let key = blake3::derive_key("tincan v1 dm sealed envelope", &ikm);
    let mut out = eph_pub.as_bytes().to_vec();
    out.extend_from_slice(&seal(&key, plaintext));
    out
}

#[allow(dead_code)]
pub fn dm_open(my_prekey_secret: &[u8; 32], sealed: &[u8]) -> Vec<u8> {
    use x25519_dalek::{PublicKey, StaticSecret};
    let me = StaticSecret::from(*my_prekey_secret);
    let my_pub = PublicKey::from(&me);
    let eph_pub: [u8; 32] = sealed[..32].try_into().unwrap();
    let shared = me.diffie_hellman(&PublicKey::from(eph_pub));
    let mut ikm = Vec::with_capacity(96);
    ikm.extend_from_slice(shared.as_bytes());
    ikm.extend_from_slice(&eph_pub);
    ikm.extend_from_slice(my_pub.as_bytes());
    let key = blake3::derive_key("tincan v1 dm sealed envelope", &ikm);
    open(&key, &sealed[32..])
}

#[test]
fn dm_roundtrip() {
    use x25519_dalek::{PublicKey, StaticSecret};
    let sk = random32();
    let pk = PublicKey::from(&StaticSecret::from(sk)).to_bytes();
    let ct = dm_seal(&pk, b"hello offline friend");
    assert_eq!(ct.len(), 32 + 24 + 20 + 16);
    assert_eq!(dm_open(&sk, &ct), b"hello offline friend");
}

#[test]
fn bench_verify_100k() {
    let sk = SecretKey::generate();
    let pk = sk.public();
    let msgs: Vec<Vec<u8>> = (0..100_000u32).map(|i| { let mut m = vec![0u8; 250]; m[..4].copy_from_slice(&i.to_le_bytes()); m }).collect();
    let t = Instant::now();
    let sigs: Vec<_> = msgs.iter().map(|m| sk.sign(m)).collect();
    let sign_t = t.elapsed();
    let t = Instant::now();
    for (m, s) in msgs.iter().zip(&sigs) { pk.verify(m, s).unwrap(); }
    let verify_t = t.elapsed();
    let t = Instant::now();
    for m in &msgs { let _ = blake3::hash(m); }
    println!("100k x 250B: sign {sign_t:?}, verify {verify_t:?}, blake3 {:?}", t.elapsed());
}

/// Re-sign a pkarr packet with an explicit seq/timestamp (seq = max(now, seen+1)) so a
/// later host can always supersede a record written by a host with a fast clock.
fn resign_with_seq(rk: &SecretKey, sp: &SignedPacket, seq_us: u64) -> SignedPacket {
    let item = MutableItem::new(rk.as_signing_key(), sp.encoded_packet(), seq_us as i64, None);
    let sp2 = SignedPacket::from_parts_unchecked(
        item.key(),
        item.signature(),
        iroh_dns::pkarr::Timestamp::from_micros(item.seq() as u64),
        item.value(),
    )
    .unwrap();
    SignedPacket::from_bytes(sp2.as_bytes()).expect("re-signed packet verifies")
}

fn reseq_probe() {
    let (rk, _) = rdv_keys(&random32());
    let sp = SignedPacket::from_txt_strings(&rk, TXT_NAME, ["0=abc"], 30).unwrap();
    let future = sp.timestamp().as_micros() + 3_600_000_000;
    let sp_future = resign_with_seq(&rk, &sp, future);
    let sp_next = resign_with_seq(&rk, &sp, future + 1);
    println!(
        "reseq: base ts={} future ts={} next ts={} next_more_recent={}",
        sp.timestamp().as_micros(),
        sp_future.timestamp().as_micros(),
        sp_next.timestamp().as_micros(),
        sp_next.more_recent_than(&sp_future)
    );
}
