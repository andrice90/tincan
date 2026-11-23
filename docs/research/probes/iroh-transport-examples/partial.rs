//! Does a peer holding only PART of a blob contribute, and does the download
//! survive the original uploader going offline mid-transfer?
use std::time::{Duration, Instant};
use anyhow::Result;
use iroh::{Endpoint, endpoint::presets, protocol::Router};
use iroh_blobs::{
    BlobsProtocol,
    api::downloader::{DownloadProgressItem, Downloader},
    protocol::{ChunkRanges, ChunkRangesExt, GetRequest},
    store::mem::MemStore,
    util::connection_pool,
};
use n0_future::StreamExt;

async fn node() -> Result<(Router, MemStore)> {
    let ep = Endpoint::bind(presets::N0).await?;
    let store = MemStore::new();
    let router = Router::builder(ep).accept(iroh_blobs::ALPN, BlobsProtocol::new(&store, None)).spawn();
    Ok((router, store))
}

fn dl(store: &MemStore, ep: &Endpoint) -> Downloader {
    let mut o = connection_pool::Options::default();
    o.connect_timeout = Duration::from_secs(5);
    Downloader::builder(store, ep).pool_options(o).build()
}

async fn run(d: &Downloader, req: GetRequest, prov: Vec<iroh::EndpointId>, names: &[(&str, iroh::EndpointId)], t0: Instant) -> Result<()> {
    let name = |id: iroh::EndpointId| names.iter().find(|(_, i)| *i == id).map(|(n, _)| *n).unwrap_or("?");
    let mut s = d.download(req, prov).stream().await?;
    let mut last = 0u64;
    while let Some(ev) = s.next().await {
        let t = t0.elapsed().as_millis();
        match ev {
            DownloadProgressItem::TryProvider { id, .. } => println!("  +{t}ms try {}", name(id)),
            DownloadProgressItem::ProviderFailed { id, .. } => println!("  +{t}ms FAILED {} (at {last} bytes)", name(id)),
            DownloadProgressItem::PartComplete { .. } => println!("  +{t}ms part complete"),
            DownloadProgressItem::Progress(b) => last = b,
            DownloadProgressItem::DownloadError => { println!("  +{t}ms DownloadError"); anyhow::bail!("download error") }
            DownloadProgressItem::Error(e) => { println!("  +{t}ms Error {e:#}"); anyhow::bail!("{e:#}") }
        }
    }
    Ok(())
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let t0 = Instant::now();
    let (a, a_store) = node().await?; // uploader
    let (b, b_store) = node().await?; // peer that only fetched the first 4 MiB
    let (c, c_store) = node().await?; // late joiner
    for r in [&a, &b, &c] { r.endpoint().online().await; }
    let data: Vec<u8> = (0..16 * 1024 * 1024u32).map(|i| (i.wrapping_mul(2654435761) >> 24) as u8).collect();
    let hash = a_store.add_bytes(data).await?.hash;
    let names = [("A-uploader", a.endpoint().id()), ("B-partial", b.endpoint().id()), ("C", c.endpoint().id())];

    println!("1) B fetches only bytes 0..4MiB from A");
    run(&dl(&b_store, b.endpoint()), GetRequest::blob_ranges(hash, ChunkRanges::bytes(0..4 * 1024 * 1024)), vec![a.endpoint().id()], &names, t0).await?;

    println!("2) C fetches full blob from [B-partial, A]");
    let dc = dl(&c_store, c.endpoint());
    run(&dc, GetRequest::blob(hash), vec![b.endpoint().id(), a.endpoint().id()], &names, t0).await?;
    println!("   C complete = {}", c_store.blobs().observe(hash).await?.is_complete());

    println!("3) A goes offline; fresh D fetches from [A(offline), B-partial, C-complete]");
    a.shutdown().await?;
    let (d, d_store) = node().await?;
    d.endpoint().online().await;
    run(&dl(&d_store, d.endpoint()), GetRequest::blob(hash), vec![a.endpoint().id(), b.endpoint().id(), c.endpoint().id()], &names, t0).await?;
    println!("   D complete = {}", d_store.blobs().observe(hash).await?.is_complete());

    println!("4) only B-partial left for a fresh E");
    c.shutdown().await?;
    let (e, e_store) = node().await?;
    e.endpoint().online().await;
    let r = run(&dl(&e_store, e.endpoint()), GetRequest::blob(hash), vec![b.endpoint().id()], &names, t0).await;
    let bf = e_store.blobs().observe(hash).await?;
    println!("   result={:?} E has {:?}", r.map_err(|e| e.to_string()), bf.ranges);
    Ok(())
}
