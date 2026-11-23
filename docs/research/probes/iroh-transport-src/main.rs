//! Tincan transport spike: iroh 1.5.0 + iroh-blobs 0.105.0 (+ mDNS / Mainline DHT lookup).
//!
//! (a) endpoint with a persistent secret key and a custom ALPN
//! (b) Router with a custom ProtocolHandler (bi stream echo + datagram + path/RTT probe)
//! (c) FsStore + add a file from disk (streamed, TryReference) + BlobsProtocol on the same Router
//! (d) downloader fetching a hash from a list of provider ids [dead, seeder], plus a range request
//!
//! Usage: cargo run --release -- [--no-mdns] [--dht] [--idle SECS]

use std::{
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use bytes::Bytes;
use iroh::{
    Endpoint, EndpointId, SecretKey,
    endpoint::{Connection, QuicTransportConfig, presets},
    protocol::{AcceptError, ProtocolHandler, Router},
};
use iroh_blobs::{
    BlobFormat, BlobsProtocol, Hash,
    api::{
        Store,
        blobs::{AddPathOptions, AddProgressItem, ImportMode},
        downloader::{DownloadProgressItem, Downloader},
    },
    protocol::{ChunkRanges, ChunkRangesExt, GetRequest},
    store::fs::FsStore,
    util::connection_pool,
};
use iroh_mainline_address_lookup::DhtAddressLookup;
use iroh_mdns_address_lookup::MdnsAddressLookup;
use n0_future::StreamExt;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

/// Tincan's own control protocol (chat, presence, host election, media forwarding).
const TINCAN_ALPN: &[u8] = b"tincan/control/1";

// ---------------------------------------------------------------- (a) endpoint

/// Load a 32-byte ed25519 secret from disk, or create and persist one.
/// The public half is the EndpointId == the user's permanent identity.
fn load_or_create_secret(path: &Path) -> Result<SecretKey> {
    if let Ok(bytes) = std::fs::read(path) {
        let arr: [u8; 32] = bytes.as_slice().try_into().context("bad key file")?;
        return Ok(SecretKey::from_bytes(&arr));
    }
    let key = SecretKey::generate();
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::File::create(path)?.write_all(&key.to_bytes())?;
    Ok(key)
}

async fn make_endpoint(secret: SecretKey, mdns: bool, dht: bool) -> Result<Endpoint> {
    // Defaults in iroh 1.5: 30 s connection idle timeout, 5 s keep-alive,
    // 15 s per-path idle timeout. A shorter idle timeout = faster host-loss detection.
    let transport = QuicTransportConfig::builder()
        .max_idle_timeout(Some(Duration::from_secs(10).try_into()?))
        .keep_alive_interval(Duration::from_secs(2))
        // Datagrams are on by default (1.25 MB receive buffer); set explicitly for clarity.
        .datagram_receive_buffer_size(Some(1024 * 1024))
        .build();

    // presets::N0 = ring crypto + n0 relays (RelayMode::Default) + pkarr publish/resolve
    // + DNS lookup against dns.iroh.link. This is what makes "connect by id only" work.
    let mut builder = Endpoint::builder(presets::N0)
        .secret_key(secret)
        .transport_config(transport);
    if mdns {
        // LAN lookup: separate crate in 1.x (iroh 0.95 feature `discovery-local-network`,
        // 0.96-0.98 feature `address-lookup-mdns`).
        builder = builder.address_lookup(MdnsAddressLookup::builder());
    }
    if dht {
        // BitTorrent Mainline lookup: separate crate in 1.x (iroh 0.95 feature `discovery-pkarr-dht`,
        // 0.96-0.98 feature `address-lookup-pkarr-dht`).
        builder = builder.address_lookup(DhtAddressLookup::builder());
    }
    // ALPNs are set by Router::spawn from the registered handlers, so no .alpns() here.
    Ok(builder.bind().await?)
}

// ---------------------------------------------------------------- (b) custom handler

#[derive(Debug, Clone)]
struct TincanControl;

impl ProtocolHandler for TincanControl {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let peer = conn.remote_id();
        println!("[ctrl] accepted {} alpn={:?}", peer.fmt_short(), String::from_utf8_lossy(conn.alpn()));

        // Reliable request/response on a bi stream.
        let (mut send, mut recv) = conn.accept_bi().await?;
        let msg = recv.read_to_end(64 * 1024).await.map_err(AcceptError::from_err)?;
        send.write_all(&msg).await.map_err(AcceptError::from_err)?;
        send.finish()?;

        // Unreliable datagram (what voice/video frames would use).
        match conn.read_datagram().await {
            Ok(d) => println!("[ctrl] got datagram {} bytes", d.len()),
            Err(e) => println!("[ctrl] datagram err {e}"),
        }
        conn.closed().await;
        Ok(())
    }
}

/// Snapshot of a connection for host-stability scoring. `Path` borrows the
/// connection, so copy values out before moving across tasks.
fn describe_paths(conn: &Connection) -> String {
    let mut out = Vec::new();
    for p in conn.paths().iter() {
        out.push(format!(
            "{}{} rtt={:?} {}",
            if p.is_selected() { "*" } else { " " },
            if p.is_relay() { "relay" } else if p.is_ip() { "direct" } else { "custom" },
            p.rtt(),
            p.remote_addr()
        ));
    }
    out.join(" | ")
}

// ---------------------------------------------------------------- (c) blobs

/// Import a file without loading it into RAM. TryReference = hash in place, keep
/// referencing the original file (no copy into the store; file must not change).
async fn add_file(store: &Store, path: &Path) -> Result<Hash> {
    let abs = std::path::absolute(path)?; // add_path requires an absolute path
    let mut progress = store
        .blobs()
        .add_path_with_opts(AddPathOptions {
            path: abs,
            format: BlobFormat::Raw,
            mode: ImportMode::TryReference,
        })
        .stream()
        .await;
    while let Some(item) = progress.next().await {
        match item {
            AddProgressItem::Size(s) => println!("[add] size {s}"),
            AddProgressItem::OutboardProgress(o) => println!("[add] outboard {o}"),
            AddProgressItem::CopyProgress(_) | AddProgressItem::CopyDone => {}
            AddProgressItem::Done(tt) => {
                let hash = tt.hash();
                // Persist a named tag so GC never deletes it; tag name = app-level id.
                store.tags().set(format!("attachment-{hash}"), tt.hash_and_format()).await?;
                return Ok(hash);
            }
            AddProgressItem::Error(e) => return Err(e.into()),
        }
    }
    anyhow::bail!("import stream ended early")
}

struct Node {
    router: Router,
    store: FsStore,
}

async fn spawn_node(dir: &Path, mdns: bool, dht: bool) -> Result<Node> {
    let secret = load_or_create_secret(&dir.join("secret.key"))?;
    let endpoint = make_endpoint(secret, mdns, dht).await?;
    let store = FsStore::load(dir.join("blobs")).await?; // redb db + data files
    let blobs = BlobsProtocol::new(&store, None); // None = no access-control events
    let router = Router::builder(endpoint)
        .accept(TINCAN_ALPN, TincanControl)
        .accept(iroh_blobs::ALPN, blobs)
        .spawn();
    Ok(Node { router, store })
}

// ---------------------------------------------------------------- (d) download

fn downloader(node: &Node) -> Downloader {
    // Default pool connect_timeout is 1 s. Raised here as a precaution for WAN dials
    // (untested; a local by-id dial via n0 DNS took ~0.3 s). It also bounds how long
    // an offline provider stalls the queue (measured: exactly this value).
    let mut opts = connection_pool::Options::default();
    opts.connect_timeout = Duration::from_secs(10);
    Downloader::builder(&node.store, node.router.endpoint())
        .pool_options(opts)
        .build()
}

async fn fetch(dl: &Downloader, req: GetRequest, providers: Vec<EndpointId>, t0: Instant) -> Result<()> {
    // Providers are tried in order; on failure the next one is asked only for the
    // ranges still missing locally. Vec<EndpointId> implements ContentDiscovery.
    let mut stream = dl.download(req, providers).stream().await?;
    let mut last = 0;
    while let Some(ev) = stream.next().await {
        let t = t0.elapsed().as_millis();
        match ev {
            DownloadProgressItem::TryProvider { id, .. } => println!("[dl +{t}ms] try {}", id.fmt_short()),
            DownloadProgressItem::ProviderFailed { id, .. } => println!("[dl +{t}ms] FAILED {}", id.fmt_short()),
            DownloadProgressItem::PartComplete { .. } => println!("[dl +{t}ms] part complete"),
            DownloadProgressItem::Progress(b) => {
                if b - last > 2_000_000 {
                    println!("[dl +{t}ms] {b} bytes");
                    last = b;
                }
            }
            DownloadProgressItem::DownloadError => anyhow::bail!("download error"),
            DownloadProgressItem::Error(e) => anyhow::bail!("download error: {e:#}"),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- main

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mdns = !args.iter().any(|a| a == "--no-mdns");
    let dht = args.iter().any(|a| a == "--dht");
    let idle: u64 = args
        .iter()
        .position(|a| a == "--idle")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let root = PathBuf::from("data");
    let _ = std::fs::remove_dir_all(root.join("downloader").join("blobs")); // fresh download each run
    let t0 = Instant::now();

    // Seeder: owns a 24 MiB file.
    let seeder = spawn_node(&root.join("seeder"), mdns, dht).await?;
    let file = root.join("seeder").join("attachment.bin");
    if !file.exists() {
        let data: Vec<u8> = (0..24 * 1024 * 1024u32).map(|i| (i.wrapping_mul(2654435761) >> 24) as u8).collect();
        std::fs::write(&file, data)?;
    }
    let hash = add_file(&seeder.store, &file).await?;
    println!("[seed] id={} hash={hash}", seeder.router.endpoint().id());

    // A provider id that is gone: bind, go online (publishes to n0 DNS), close.
    let dead = Endpoint::bind(presets::N0).await?;
    dead.online().await;
    let dead_id = dead.id();
    dead.close().await;

    // Downloader: also a full node (it re-seeds whatever it downloads).
    let dlnode = spawn_node(&root.join("downloader"), mdns, dht).await?;
    seeder.router.endpoint().online().await; // home relay connected + addr published
    dlnode.router.endpoint().online().await;
    println!("[+{}ms] both online", t0.elapsed().as_millis());

    // Custom ALPN round trip, by EndpointId only (address comes from DNS/mDNS lookup).
    let seeder_id = seeder.router.endpoint().id();
    let conn = dlnode.router.endpoint().connect(seeder_id, TINCAN_ALPN).await?;
    let (mut s, mut r) = conn.open_bi().await?;
    s.write_all(b"hello tincan").await?;
    s.finish()?;
    let echo = r.read_to_end(1024).await?;
    println!("[ctrl] echo={:?} max_datagram={:?}", String::from_utf8_lossy(&echo), conn.max_datagram_size());
    conn.send_datagram(Bytes::from_static(b"opus-frame"))?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    println!("[ctrl] paths: {}", describe_paths(&conn));
    let st = conn.stats();
    println!("[ctrl] stats: udp_tx={:?} udp_rx={:?}", st.udp_tx.bytes, st.udp_rx.bytes);
    conn.close(0u32.into(), b"bye");

    let dl = downloader(&dlnode);

    // Range request first: only the first 1 MiB (e.g. preview / start of a video).
    fetch(&dl, GetRequest::blob_ranges(hash, ChunkRanges::bytes(0..1024 * 1024)), vec![seeder_id], t0).await?;
    let bf = dlnode.store.blobs().observe(hash).await?;
    println!("[range] have ranges {:?} of size {} complete={}", bf.ranges, bf.size(), bf.is_complete());
    let mut rd = dlnode.store.blobs().reader(hash);
    let mut head = [0u8; 16];
    rd.read_exact(&mut head).await?;
    println!("[range] first bytes ok: {head:?}");
    rd.seek(std::io::SeekFrom::Start(20 * 1024 * 1024)).await?;
    let mut tail = [0u8; 16];
    println!("[range] read past downloaded range -> {:?}", rd.read_exact(&mut tail).await.map(|_| ()));

    // Full download, dead provider listed first. Only missing ranges are requested.
    fetch(&dl, GetRequest::blob(hash), vec![dead_id, seeder_id], t0).await?;
    let bf = dlnode.store.blobs().observe(hash).await?;
    println!("[full] complete={} size={}", bf.is_complete(), bf.size());

    // The downloader now serves the blob too: seeder can go offline.
    seeder.router.shutdown().await?;
    println!("[+{}ms] seeder shut down; downloader still serves {hash}", t0.elapsed().as_millis());

    if idle > 0 {
        println!("idling {idle}s, pid={}", std::process::id());
        tokio::time::sleep(Duration::from_secs(idle)).await;
    }
    dlnode.router.shutdown().await?; // also shuts down the FsStore cleanly
    Ok(())
}
