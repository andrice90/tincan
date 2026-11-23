//! In-process swarm tests: several endpoints on loopback, relays disabled, addresses passed
//! as `AddrHint`s (no discovery service).

use std::{
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{Arc, Once},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use bytes::Bytes;
use iroh::{Endpoint, RelayMode, SecretKey, endpoint::presets, protocol::Router};
use tincan_core::{BlobHash, UserId, wire::AddrHint};
use tincan_files::{AccessPolicy, BlobKind, FileEvent, Files, FilesError, TransferInfo, TransferState};
use tokio::sync::broadcast::{Receiver, error::RecvError};

const MIB: u64 = 1024 * 1024;
const KIB: u64 = 1024;

fn init() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: std documents `set_var` as always safe on Windows (the only target), and it
        // runs once, before any test creates an FsStore runtime that reads the variable.
        unsafe { std::env::set_var("TOKIO_WORKER_THREADS", "2") };
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_test_writer()
            .try_init();
    });
}

fn allow_all() -> Arc<dyn AccessPolicy> {
    Arc::new(|_: &UserId| true)
}

/// Deterministic pseudo-random content.
fn content(len: u64, seed: u32) -> Vec<u8> {
    (0..len as u32).map(|i| (i.wrapping_add(seed).wrapping_mul(2_654_435_761) >> 24) as u8).collect()
}

struct Node {
    files: Files,
    router: Router,
    endpoint: Endpoint,
    user: UserId,
    hint: AddrHint,
}

impl Node {
    async fn spawn(dir: &Path, key: SecretKey, policy: Arc<dyn AccessPolicy>) -> Result<Node> {
        init();
        let endpoint = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .secret_key(key)
            .bind()
            .await?;
        let files = Files::load(dir, endpoint.clone(), policy).await?;
        let router = Router::builder(endpoint.clone()).accept(tincan_files::ALPN, files.protocol()).spawn();
        let port = endpoint
            .bound_sockets()
            .into_iter()
            .find(SocketAddr::is_ipv4)
            .map(|a| a.port())
            .context("no ipv4 socket")?;
        let hint = AddrHint { relay: None, direct: vec![SocketAddr::from((Ipv4Addr::LOCALHOST, port))] };
        let user = UserId::from_bytes(*endpoint.id().as_bytes());
        Ok(Node { files, router, endpoint, user, hint })
    }

    async fn fresh(root: &Path, name: &str) -> Result<Node> {
        Node::spawn(&root.join(name), SecretKey::generate(), allow_all()).await
    }

    fn provider(&self) -> (UserId, AddrHint) {
        (self.user, self.hint.clone())
    }

    /// Router shutdown closes the endpoint and (through the protocol handler) the store.
    async fn shutdown(&self) -> Result<()> {
        self.router.shutdown().await?;
        // Idempotent: already done by the router.
        self.files.shutdown().await;
        ensure!(self.files.is_closed());
        Ok(())
    }
}

/// Next transfer event for `hash`, skipping others and tolerating lag.
async fn next_transfer(rx: &mut Receiver<FileEvent>, hash: BlobHash, deadline: Instant) -> Result<TransferInfo> {
    loop {
        let left = deadline.checked_duration_since(Instant::now()).context("timed out waiting for an event")?;
        match tokio::time::timeout(left, rx.recv()).await {
            Err(_) => bail!("timed out waiting for an event for {hash:?}"),
            Ok(Err(RecvError::Lagged(_))) => continue,
            Ok(Err(RecvError::Closed)) => bail!("event channel closed"),
            Ok(Ok(FileEvent::Transfer(info))) if info.hash == hash => return Ok(info),
            Ok(Ok(_)) => continue,
        }
    }
}

/// Waits for `Complete`; fails on `Failed`. Returns every transfer event seen.
async fn wait_complete(rx: &mut Receiver<FileEvent>, hash: BlobHash, secs: u64) -> Result<Vec<TransferInfo>> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut seen = Vec::new();
    loop {
        let info = next_transfer(rx, hash, deadline).await?;
        seen.push(info.clone());
        match info.state {
            TransferState::Complete => return Ok(seen),
            TransferState::Failed => bail!("download failed: {seen:?}"),
            _ => {}
        }
    }
}

/// Waits until at least `min` bytes are held.
async fn wait_have(rx: &mut Receiver<FileEvent>, hash: BlobHash, min: u64, secs: u64) -> Result<u64> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let info = next_transfer(rx, hash, deadline).await?;
        ensure!(info.state != TransferState::Complete, "completed before reaching the partial mark");
        if info.have >= min {
            return Ok(info.have);
        }
    }
}

async fn wait_state(rx: &mut Receiver<FileEvent>, hash: BlobHash, state: TransferState, secs: u64) -> Result<TransferInfo> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let info = next_transfer(rx, hash, deadline).await?;
        if info.state == state {
            return Ok(info);
        }
        ensure!(info.state != TransferState::Complete, "completed, expected {state:?}");
    }
}

/// Asks `target` which ranges of `hash` it holds (iroh-blobs Observe request), dialing from
/// `from`'s endpoint. Returns (complete, verified bytes, size).
async fn observe(from: &Node, target: &Node, hash: BlobHash) -> Result<(bool, u64, u64)> {
    let ip = *target.hint.direct.first().context("no direct addr")?;
    let id = iroh::EndpointId::from_bytes(target.user.as_bytes())?;
    let addr = iroh::EndpointAddr::from_parts(id, [iroh::TransportAddr::Ip(ip)]);
    let conn = from.endpoint.connect(addr, tincan_files::ALPN).await?;
    let client = iroh_blobs::store::mem::MemStore::new();
    let request = iroh_blobs::protocol::ObserveRequest::new(iroh_blobs::Hash::from_bytes(*hash.as_bytes()));
    let stream = client.remote().observe(conn, request);
    tokio::pin!(stream);
    let first = tokio::time::timeout(Duration::from_secs(5), n0_future::StreamExt::next(&mut stream))
        .await?
        .context("observe stream ended")??;
    Ok((first.is_complete(), first.total_bytes(), first.size()))
}

fn write_file(dir: &Path, name: &str, data: &[u8]) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(name);
    std::fs::write(&path, data)?;
    Ok(path)
}

/// (1) A imports 6 MiB, B downloads from A, A goes offline, C downloads from [A (dead), B].
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uploader_offline_download_from_second_holder() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let a = Node::fresh(tmp.path(), "a").await?;
    let b = Node::fresh(tmp.path(), "b").await?;
    let c = Node::fresh(tmp.path(), "c").await?;

    let data = content(6 * MIB, 1);
    let path = write_file(&tmp.path().join("src"), "video.bin", &data)?;
    let imported = a.files.import(&path, BlobKind::Attachment).await?;
    assert_eq!(imported.size, 6 * MIB);
    assert!(a.files.complete_hashes().contains(&imported.hash));
    let hash = imported.hash;

    let mut rb = b.files.subscribe();
    b.files.download(hash, BlobKind::Attachment, vec![a.provider()]);
    wait_complete(&mut rb, hash, 30).await?;
    assert!(b.files.complete_hashes().contains(&hash));

    let a_provider = a.provider();
    a.shutdown().await?;

    let mut rc = c.files.subscribe();
    let t0 = Instant::now();
    c.files.download(hash, BlobKind::Attachment, vec![a_provider, b.provider()]);
    let events = wait_complete(&mut rc, hash, 40).await?;
    tracing::info!("C completed in {:?} after {} events", t0.elapsed(), events.len());
    let got = c.files.read_to_bytes(hash, 8 * MIB).await?;
    assert_eq!(got.len() as u64, 6 * MIB);
    assert!(got == data, "content mismatch");
    assert!(b.files.uploaded_bytes() >= 6 * MIB, "B served C");

    b.shutdown().await?;
    c.shutdown().await?;
    Ok(())
}

/// (2) A partial holder and a complete holder serve a fresh peer while the uploader is offline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn partial_and_complete_holders_serve_fresh_peer() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let a = Node::fresh(tmp.path(), "a").await?;
    let b = Node::fresh(tmp.path(), "b").await?;
    let c = Node::fresh(tmp.path(), "c").await?;
    let d = Node::fresh(tmp.path(), "d").await?;

    let data = content(4 * MIB, 2);
    let hash = a.files.import_bytes(Bytes::from(data.clone()), BlobKind::Attachment).await?.hash;

    // B gets only part of it: throttle A, cancel B part way.
    a.files.set_upload_limit(Some(2 * MIB));
    let mut rb = b.files.subscribe();
    b.files.download(hash, BlobKind::Attachment, vec![a.provider()]);
    wait_have(&mut rb, hash, MIB, 20).await?;
    b.files.cancel(hash);
    let b_status = b.files.status(hash).await?;
    assert!(!b_status.complete, "B must stay partial");
    assert!(b_status.have >= MIB, "B kept its partial data: {b_status:?}");
    assert!(b.files.transfer(hash).is_none(), "cancelled request is gone");

    // C gets all of it.
    a.files.set_upload_limit(None);
    let mut rc = c.files.subscribe();
    c.files.download(hash, BlobKind::Attachment, vec![a.provider()]);
    wait_complete(&mut rc, hash, 30).await?;

    // Observe is enabled: D can ask holders what they have without downloading.
    let (b_complete, b_have, b_size) = observe(&d, &b, hash).await?;
    assert!(!b_complete && b_have >= MIB && b_size == 4 * MIB, "B: {b_complete} {b_have} {b_size}");
    let (c_complete, c_have, _) = observe(&d, &c, hash).await?;
    assert!(c_complete && c_have == 4 * MIB, "C: {c_complete} {c_have}");

    let a_provider = a.provider();
    a.shutdown().await?;

    // D: uploader offline, partial holder first, complete holder second.
    let mut rd = d.files.subscribe();
    d.files.download(hash, BlobKind::Attachment, vec![a_provider, b.provider(), c.provider()]);
    wait_complete(&mut rd, hash, 40).await?;
    let got = d.files.read_to_bytes(hash, 4 * MIB).await?;
    assert!(got == data, "content mismatch");
    assert!(b.files.uploaded_bytes() > 0, "the partial holder contributed");
    assert!(c.files.uploaded_bytes() > 0, "the complete holder finished it");
    let b_after = b.files.status(hash).await?;
    assert!(!b_after.complete, "serving does not complete B");

    for n in [&b, &c, &d] {
        n.shutdown().await?;
    }
    Ok(())
}

/// (3) A download interrupted by a clean shutdown resumes after reload and fetches only the rest.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_after_restart_fetches_only_remainder() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let a = Node::fresh(tmp.path(), "a").await?;
    let b_dir = tmp.path().join("b");
    let b_key = SecretKey::generate();

    let total = 4 * MIB;
    let data = content(total, 3);
    let hash = a.files.import_bytes(Bytes::from(data.clone()), BlobKind::Attachment).await?.hash;
    a.files.set_upload_limit(Some(2 * MIB));

    let b1 = Node::spawn(&b_dir, b_key.clone(), allow_all()).await?;
    let mut rb = b1.files.subscribe();
    b1.files.download(hash, BlobKind::Attachment, vec![a.provider()]);
    wait_have(&mut rb, hash, MIB, 20).await?;
    b1.shutdown().await?;
    drop(b1);

    let b2 = Node::spawn(&b_dir, b_key, allow_all()).await?;
    let st = b2.files.status(hash).await?;
    assert!(!st.complete, "still partial after reload");
    assert!(st.have >= MIB / 2, "partial progress survived the restart: {st:?}");
    let pending = b2.files.partial_downloads().await?;
    assert!(pending.contains(&(hash, BlobKind::Attachment)), "partial download is listed: {pending:?}");
    assert!(!b2.files.complete_hashes().contains(&hash));

    a.files.set_upload_limit(None);
    let before = a.files.uploaded_bytes();
    let mut rb2 = b2.files.subscribe();
    b2.files.download(hash, BlobKind::Attachment, vec![a.provider()]);
    let events = wait_complete(&mut rb2, hash, 30).await?;
    let first_progress = events.iter().map(|e| e.have).find(|h| *h > 0).context("no progress reported")?;
    assert!(first_progress >= st.have, "progress starts from the kept bytes: {first_progress} < {}", st.have);
    let sent = a.files.uploaded_bytes() - before;
    let remainder = total - st.have;
    tracing::info!(kept = st.have, first_progress, sent, remainder, "resume");
    assert!(sent <= remainder + 64 * KIB, "A sent {sent} bytes, the remainder was {remainder}");
    assert!(sent < total, "not refetched from scratch");
    let got = b2.files.read_to_bytes(hash, total).await?;
    assert!(got == data, "content mismatch");

    b2.shutdown().await?;
    a.shutdown().await?;
    Ok(())
}

/// (4) A peer the access policy denies cannot fetch; a member can.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn access_policy_denies_non_member() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let member_key = SecretKey::generate();
    let member = UserId::from_bytes(*member_key.public().as_bytes());
    let policy: Arc<dyn AccessPolicy> = Arc::new(move |peer: &UserId| *peer == member);
    let a = Node::spawn(&tmp.path().join("a"), SecretKey::generate(), policy).await?;
    let b = Node::spawn(&tmp.path().join("b"), member_key, allow_all()).await?;
    let outsider = Node::fresh(tmp.path(), "x").await?;

    let hash = a.files.import_bytes(Bytes::from(content(512 * KIB, 4)), BlobKind::Attachment).await?.hash;

    let mut rx = outsider.files.subscribe();
    outsider.files.download(hash, BlobKind::Attachment, vec![a.provider()]);
    let info = wait_state(&mut rx, hash, TransferState::Unavailable, 20).await?;
    assert_eq!(info.peers, 0);
    let st = outsider.files.status(hash).await?;
    assert!(!st.complete);
    assert_eq!(st.have, 0, "nothing leaked to the outsider");
    assert_eq!(a.files.uploaded_bytes(), 0);

    let mut rb = b.files.subscribe();
    b.files.download(hash, BlobKind::Attachment, vec![a.provider()]);
    wait_complete(&mut rb, hash, 20).await?;
    assert_eq!(a.files.uploaded_bytes(), 512 * KIB);

    // The parked request picks up a holder it is allowed to use.
    outsider.files.add_providers(hash, vec![b.provider()]);
    wait_complete(&mut rx, hash, 20).await?;

    for n in [&a, &b, &outsider] {
        n.shutdown().await?;
    }
    Ok(())
}

/// (5) Export, read and remove round trips.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn export_round_trip_is_byte_equal() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let a = Node::fresh(tmp.path(), "a").await?;
    let b = Node::fresh(tmp.path(), "b").await?;

    // Not chunk aligned on purpose.
    let data = content(MIB + 1234, 5);
    let src = write_file(&tmp.path().join("src"), "doc.pdf", &data)?;
    let imported = a.files.import(&src, BlobKind::Attachment).await?;
    let out = tmp.path().join("exports").join("nested").join("doc.pdf");
    a.files.export(imported.hash, &out).await?;
    assert!(std::fs::read(&out)? == data, "exported file differs");

    // Clips: bytes in, bytes out, size cap.
    let clip = content(20 * KIB + 7, 6);
    let clip_hash = a.files.import_bytes(Bytes::from(clip.clone()), BlobKind::Clip).await?.hash;
    assert_eq!(a.files.read_to_bytes(clip_hash, 5 * MIB).await?, Bytes::from(clip.clone()));
    match a.files.read_to_bytes(clip_hash, 1024).await {
        Err(FilesError::TooLarge { size, max }) => assert_eq!((size, max), (clip.len() as u64, 1024)),
        other => bail!("expected TooLarge, got {other:?}"),
    }

    // Downloaded copy exports identically.
    let mut rb = b.files.subscribe();
    b.files.download(imported.hash, BlobKind::Attachment, vec![a.provider()]);
    wait_complete(&mut rb, imported.hash, 20).await?;
    let out_b = tmp.path().join("exports").join("b.pdf");
    b.files.export(imported.hash, &out_b).await?;
    assert!(std::fs::read(&out_b)? == data, "downloaded export differs");

    // Unknown blobs are reported, not exported.
    let missing = BlobHash::from_bytes([9; 32]);
    assert!(matches!(b.files.export(missing, &tmp.path().join("nope")).await, Err(FilesError::NotComplete(_))));

    // A second kind on the same bytes, then removing one kind keeps the blob held.
    b.files.download(imported.hash, BlobKind::Clip, vec![]);
    let completed_as_clip = loop {
        match tokio::time::timeout(Duration::from_secs(10), rb.recv()).await? {
            Ok(FileEvent::Completed { hash, kind: BlobKind::Clip, from }) if hash == imported.hash => {
                assert_eq!(from, None, "already complete locally: no provider delivered it");
                break true;
            }
            Ok(_) | Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => break false,
        }
    };
    assert!(completed_as_clip);
    b.files.remove_kind(imported.hash, BlobKind::Attachment).await?;
    assert!(b.files.complete_hashes().contains(&imported.hash), "still held as a clip");
    b.files.remove(imported.hash).await?;
    assert!(!b.files.complete_hashes().contains(&imported.hash));

    // Removing on A forgets the hash; the source file itself is untouched.
    a.files.remove(imported.hash).await?;
    assert!(!a.files.complete_hashes().contains(&imported.hash));
    assert!(std::fs::read(&src)? == data, "referenced source file must not be touched");

    a.shutdown().await?;
    b.shutdown().await?;
    Ok(())
}

/// (6) The event stream reports queued, progress, then complete.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn events_report_progress_then_complete() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let a = Node::fresh(tmp.path(), "a").await?;
    let b = Node::fresh(tmp.path(), "b").await?;

    let total = 3 * MIB;
    let hash = a.files.import_bytes(Bytes::from(content(total, 7)), BlobKind::Attachment).await?.hash;
    a.files.set_upload_limit(Some(2 * MIB));

    let mut rx = b.files.subscribe();
    b.files.download(hash, BlobKind::Attachment, vec![a.provider()]);
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut transfers = Vec::new();
    let mut completed = false;
    while !completed {
        let left = deadline.checked_duration_since(Instant::now()).context("timed out")?;
        match tokio::time::timeout(left, rx.recv()).await? {
            Ok(FileEvent::Transfer(info)) if info.hash == hash => transfers.push(info),
            Ok(FileEvent::Completed { hash: h, kind, from }) if h == hash => {
                assert_eq!(kind, BlobKind::Attachment);
                assert_eq!(from, Some(a.user), "the provider that served it is reported");
                assert_eq!(b.files.provided_by(&hash), Some(a.user));
                completed = true;
            }
            Ok(_) | Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => bail!("closed"),
        }
    }
    let first = transfers.first().context("no events")?;
    assert_eq!(first.state, TransferState::Queued);
    let progress: Vec<&TransferInfo> =
        transfers.iter().filter(|t| t.state == TransferState::Downloading && t.have > 0 && t.have < total).collect();
    assert!(progress.len() >= 2, "expected several progress events: {transfers:?}");
    assert!(progress.iter().any(|t| t.total == Some(total)), "size is reported while downloading");
    assert!(progress.iter().all(|t| t.peers >= 1));
    let haves: Vec<u64> = transfers.iter().map(|t| t.have).collect();
    assert!(haves.windows(2).all(|w| w[0] <= w[1]), "progress never goes back: {haves:?}");
    let last = transfers.last().context("no events")?;
    assert_eq!((last.state, last.have, last.total), (TransferState::Complete, total, Some(total)));
    assert_eq!(b.files.transfer(hash).map(|t| t.state), Some(TransferState::Complete));

    // Asking again for a held blob only repeats Complete.
    b.files.download(hash, BlobKind::Attachment, vec![a.provider()]);
    let again = next_transfer(&mut rx, hash, Instant::now() + Duration::from_secs(5)).await?;
    assert_eq!(again.state, TransferState::Complete);

    a.shutdown().await?;
    b.shutdown().await?;
    Ok(())
}

/// (7) An upload limit slows a transfer down measurably.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upload_limit_slows_transfer() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let a = Node::fresh(tmp.path(), "a").await?;
    let b = Node::fresh(tmp.path(), "b").await?;
    let c = Node::fresh(tmp.path(), "c").await?;

    let size = MIB;
    let hash = a.files.import_bytes(Bytes::from(content(size, 8)), BlobKind::Clip).await?.hash;

    let mut rb = b.files.subscribe();
    let t0 = Instant::now();
    b.files.download(hash, BlobKind::Clip, vec![a.provider()]);
    wait_complete(&mut rb, hash, 20).await?;
    let unlimited = t0.elapsed();

    let limit = 512 * KIB;
    a.files.set_upload_limit(Some(limit));
    assert_eq!(a.files.upload_limit(), Some(limit));
    let mut rc = c.files.subscribe();
    let t1 = Instant::now();
    c.files.download(hash, BlobKind::Clip, vec![a.provider()]);
    wait_complete(&mut rc, hash, 20).await?;
    let limited = t1.elapsed();
    tracing::info!(?unlimited, ?limited, "1 MiB transfer times");

    // 1 MiB at 512 KiB/s is about 2 s.
    assert!(limited >= Duration::from_millis(1500), "limited transfer took only {limited:?}");
    assert!(limited > unlimited * 3, "unlimited {unlimited:?} vs limited {limited:?}");

    for n in [&a, &b, &c] {
        n.shutdown().await?;
    }
    Ok(())
}

/// Allows one peer to connect, and to fetch only the listed hashes.
struct PerBlob {
    peer: UserId,
    hashes: parking_lot::Mutex<Vec<BlobHash>>,
}

impl AccessPolicy for PerBlob {
    fn allow(&self, peer: &UserId) -> bool {
        *peer == self.peer
    }
    fn allow_blob(&self, peer: &UserId, hash: &BlobHash) -> bool {
        *peer == self.peer && self.hashes.lock().contains(hash)
    }
}

/// Dials `target` with the blobs ALPN from `from`'s endpoint.
async fn dial(from: &Node, target: &Node) -> Result<iroh::endpoint::Connection> {
    let ip = *target.hint.direct.first().context("no direct addr")?;
    let id = iroh::EndpointId::from_bytes(target.user.as_bytes())?;
    let addr = iroh::EndpointAddr::from_parts(id, [iroh::TransportAddr::Ip(ip)]);
    Ok(from.endpoint.connect(addr, tincan_files::ALPN).await?)
}

/// (8) A peer allowed to connect gets only the blobs the policy allows it per hash, and
/// cannot even observe the others.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn per_blob_policy_serves_only_allowed_hashes() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let b_key = SecretKey::generate();
    let b_user = UserId::from_bytes(*b_key.public().as_bytes());
    let policy = Arc::new(PerBlob { peer: b_user, hashes: parking_lot::Mutex::new(Vec::new()) });
    let a = Node::spawn(&tmp.path().join("a"), SecretKey::generate(), policy.clone()).await?;
    let b = Node::spawn(&tmp.path().join("b"), b_key, allow_all()).await?;

    let shared = a.files.import_bytes(Bytes::from(content(300 * KIB, 9)), BlobKind::Attachment).await?.hash;
    let private = a.files.import_bytes(Bytes::from(content(200 * KIB, 10)), BlobKind::Attachment).await?.hash;
    policy.hashes.lock().push(shared);

    let mut rb = b.files.subscribe();
    b.files.download(private, BlobKind::Attachment, vec![a.provider()]);
    wait_state(&mut rb, private, TransferState::Unavailable, 20).await?;
    assert_eq!(b.files.status(private).await?.have, 0, "nothing of the private blob leaked");
    assert_eq!(a.files.uploaded_bytes(), 0);
    assert!(observe(&b, &a, private).await.is_err(), "possession of a denied hash cannot be probed");

    b.files.download(shared, BlobKind::Attachment, vec![a.provider()]);
    wait_complete(&mut rb, shared, 20).await?;
    assert_eq!(a.files.uploaded_bytes(), 300 * KIB, "only the allowed blob was served");
    let (complete, have, _) = observe(&b, &a, shared).await?;
    assert!(complete && have == 300 * KIB);

    // Allowing the second hash later lets the parked request finish.
    policy.hashes.lock().push(private);
    b.files.add_providers(private, vec![a.provider()]);
    wait_complete(&mut rb, private, 20).await?;

    a.shutdown().await?;
    b.shutdown().await?;
    Ok(())
}

/// (9) A get with child ranges would make the provider read an allowed blob as a hash
/// sequence and send the blobs it lists. Such requests are refused outright; a plain get of
/// the same root still works.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hash_sequence_requests_are_refused() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let b_key = SecretKey::generate();
    let b_user = UserId::from_bytes(*b_key.public().as_bytes());
    let policy = Arc::new(PerBlob { peer: b_user, hashes: parking_lot::Mutex::new(Vec::new()) });
    let a = Node::spawn(&tmp.path().join("a"), SecretKey::generate(), policy.clone()).await?;
    let b = Node::spawn(&tmp.path().join("b"), b_key, allow_all()).await?;

    let secret = a.files.import_bytes(Bytes::from(content(64 * KIB, 11)), BlobKind::Attachment).await?.hash;
    // A blob whose bytes are the secret's hash: a valid one-entry hash sequence.
    let list = a.files.import_bytes(Bytes::copy_from_slice(secret.as_bytes()), BlobKind::Attachment).await?.hash;
    policy.hashes.lock().push(list);

    let client = iroh_blobs::store::mem::MemStore::new();
    let root = iroh_blobs::Hash::from_bytes(*list.as_bytes());
    let res = client.remote().execute_get(dial(&b, &a).await?, iroh_blobs::protocol::GetRequest::all(root)).complete().await;
    assert!(res.is_err(), "a hash sequence request is refused: {res:?}");
    assert!(!client.blobs().has(iroh_blobs::Hash::from_bytes(*secret.as_bytes())).await?, "the listed blob was not sent");
    let res = client.remote().execute_get(dial(&b, &a).await?, iroh_blobs::protocol::GetRequest::blob(root)).complete().await;
    assert!(res.is_ok(), "the root alone is allowed: {res:?}");
    assert!(client.blobs().has(root).await?);
    assert!(!client.blobs().has(iroh_blobs::Hash::from_bytes(*secret.as_bytes())).await?);

    a.shutdown().await?;
    b.shutdown().await?;
    Ok(())
}

/// (10) Size caps per kind: a clip over 5 MiB and an attachment over 25 MiB are refused as
/// soon as the size is known, their tags are dropped (no resume), and asking again does
/// not refetch. The same bytes are fine as a kind whose cap allows them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_downloads_are_refused() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let a = Node::fresh(tmp.path(), "a").await?;
    let b = Node::fresh(tmp.path(), "b").await?;
    let mut rb = b.files.subscribe();

    // Over the clip cap, under the attachment cap.
    let size = tincan_files::MAX_CLIP_BYTES + 1;
    let hash = a.files.import_bytes(Bytes::from(content(size, 12)), BlobKind::Attachment).await?.hash;
    b.files.download(hash, BlobKind::Clip, vec![a.provider()]);
    let refused = wait_state(&mut rb, hash, TransferState::TooLarge, 20).await?;
    assert_eq!(refused.total, Some(size));
    assert!(!b.files.status(hash).await?.complete);
    assert!(!b.files.partial_downloads().await?.iter().any(|(h, _)| *h == hash), "a refused download is never resumed");
    assert_eq!(b.files.transfer(hash).map(|t| t.state), Some(TransferState::TooLarge));
    let sent = a.files.uploaded_bytes();
    b.files.download(hash, BlobKind::Clip, vec![a.provider()]);
    let again = next_transfer(&mut rb, hash, Instant::now() + Duration::from_secs(5)).await?;
    assert_eq!(again.state, TransferState::TooLarge, "asking again repeats the refusal");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(a.files.uploaded_bytes(), sent, "and fetches nothing");

    // The same bytes as an attachment are within its cap.
    b.files.download(hash, BlobKind::Attachment, vec![a.provider()]);
    wait_complete(&mut rb, hash, 30).await?;
    // Held as an attachment, still too large to become a clip.
    b.files.download(hash, BlobKind::Clip, vec![]);
    let held = next_transfer(&mut rb, hash, Instant::now() + Duration::from_secs(5)).await?;
    assert_eq!(held.state, TransferState::TooLarge);
    assert!(b.files.complete_hashes().contains(&hash), "the attachment stays held");

    // Over the attachment cap.
    let big = tincan_files::MAX_ATTACHMENT_BYTES + 1;
    let big_hash = a.files.import_bytes(Bytes::from(vec![7u8; big as usize]), BlobKind::Attachment).await?.hash;
    b.files.download(big_hash, BlobKind::Attachment, vec![a.provider()]);
    let refused = wait_state(&mut rb, big_hash, TransferState::TooLarge, 30).await?;
    assert_eq!(refused.total, Some(big));
    let st = b.files.status(big_hash).await?;
    assert!(!st.complete && st.have < big, "stopped before the end: {st:?}");
    assert!(!b.files.partial_downloads().await?.iter().any(|(h, _)| *h == big_hash));

    a.shutdown().await?;
    b.shutdown().await?;
    Ok(())
}

/// (11) A request that started as a clip and merged an attachment request keeps going as
/// an attachment when the blob turns out too large for a clip, and its partial data stays
/// tagged as an attachment (the clip tag is gone), so garbage collection keeps it and a
/// restart resumes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_kind_leaves_the_partial_data_tagged() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let a = Node::fresh(tmp.path(), "a").await?;
    let b = Node::fresh(tmp.path(), "b").await?;
    let size = tincan_files::MAX_CLIP_BYTES + MIB;
    let hash = a.files.import_bytes(Bytes::from(content(size, 13)), BlobKind::Attachment).await?.hash;
    a.files.set_upload_limit(Some(MIB));

    let mut rb = b.files.subscribe();
    b.files.download(hash, BlobKind::Clip, vec![a.provider()]);
    b.files.download(hash, BlobKind::Attachment, vec![a.provider()]);
    wait_have(&mut rb, hash, 256 * KIB, 20).await?;
    let t = b.files.transfer(hash).context("in progress")?;
    assert_eq!(t.state, TransferState::Downloading, "{t:?}");
    assert_eq!(t.kind, BlobKind::Attachment, "only the attachment kind is left");
    b.files.cancel(hash);

    let partial = b.files.partial_downloads().await?;
    assert!(partial.contains(&(hash, BlobKind::Attachment)), "the partial data is tagged as an attachment: {partial:?}");
    assert!(!partial.contains(&(hash, BlobKind::Clip)), "the clip tag was dropped: {partial:?}");

    a.shutdown().await?;
    b.shutdown().await?;
    Ok(())
}
