//! Resume a partial download across process restarts with FsStore.
//! modes: seed | partial | crash | finish
use std::{path::Path, time::Instant};
use anyhow::Result;
use iroh::{Endpoint, EndpointId, SecretKey, endpoint::presets, protocol::Router};
use iroh_blobs::{
    BlobsProtocol, Hash,
    api::downloader::DownloadProgressItem,
    protocol::{ChunkRanges, ChunkRangesExt, GetRequest},
    store::fs::FsStore,
};
use n0_future::StreamExt;

fn key(path: &Path) -> Result<SecretKey> {
    if let Ok(b) = std::fs::read(path) {
        return Ok(SecretKey::from_bytes(&b.as_slice().try_into()?));
    }
    let k = SecretKey::generate();
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(path, k.to_bytes())?;
    Ok(k)
}

async fn node(dir: &Path) -> Result<(Router, FsStore)> {
    let ep = Endpoint::builder(presets::N0).secret_key(key(&dir.join("key"))?).bind().await?;
    let store = FsStore::load(dir.join("blobs")).await?;
    let router = Router::builder(ep).accept(iroh_blobs::ALPN, BlobsProtocol::new(&store, None)).spawn();
    router.endpoint().online().await;
    Ok((router, store))
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let mode = std::env::args().nth(1).unwrap_or_default();
    let root = Path::new("data-resume");
    if mode == "seed" {
        let (router, store) = node(&root.join("seed")).await?;
        let data: Vec<u8> = (0..32 * 1024 * 1024u32).map(|i| (i.wrapping_mul(2654435761) >> 24) as u8).collect();
        let tag = store.add_bytes(data).await?;
        std::fs::write(root.join("ticket.txt"), format!("{} {}", router.endpoint().id(), tag.hash))?;
        println!("seeding {}", tag.hash);
        tokio::signal::ctrl_c().await?;
        router.shutdown().await?;
        return Ok(());
    }
    let t = std::fs::read_to_string(root.join("ticket.txt"))?;
    let mut it = t.split_whitespace();
    let seeder: EndpointId = it.next().unwrap().parse()?;
    let hash: Hash = it.next().unwrap().parse()?;
    let t0 = Instant::now();
    let (router, store) = node(&root.join("get")).await?;
    let before = store.remote().local(hash).await?;
    println!("[{mode}] on load: local_bytes={} complete={} bitfield={:?}", before.local_bytes(), before.is_complete(), store.blobs().observe(hash).await?.ranges);
    let req = match mode.as_str() {
        "partial" | "crash-late" => GetRequest::blob_ranges(hash, ChunkRanges::bytes(0..8 * 1024 * 1024)),
        _ => GetRequest::blob(hash),
    };
    let mut s = store.downloader(router.endpoint()).download(req, vec![seeder]).stream().await?;
    let mut first: Option<u64> = None;
    let mut last = 0;
    while let Some(ev) = s.next().await {
        match ev {
            DownloadProgressItem::Progress(b) => {
                first.get_or_insert(b);
                last = b;
                if mode == "crash" && b > 12 * 1024 * 1024 {
                    println!("[crash] hard exit at {b} bytes, no shutdown");
                    std::process::exit(3);
                }
            }
            DownloadProgressItem::Error(e) => anyhow::bail!("{e:#}"),
            DownloadProgressItem::DownloadError => anyhow::bail!("download error"),
            _ => {}
        }
    }
    if mode == "crash-late" {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        println!("[crash-late] hard exit 3 s after range completed, no shutdown");
        std::process::exit(3);
    }
    let after = store.remote().local(hash).await?;
    println!(
        "[{mode}] done in {}ms: first_progress={first:?} last_progress={last} local_bytes={} complete={}",
        t0.elapsed().as_millis(),
        after.local_bytes(),
        after.is_complete()
    );
    router.shutdown().await?; // clean: also shuts down the FsStore
    Ok(())
}
