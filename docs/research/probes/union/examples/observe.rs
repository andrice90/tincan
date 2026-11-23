//! Does iroh-blobs 0.105 let a peer ask another peer which ranges it holds?
//! A: full 1 MiB blob. B: fetches only bytes 0..256 KiB from A. C: Observe(B) and Observe(A).
use iroh::{Endpoint, RelayMode, endpoint::presets, protocol::Router};
use iroh_blobs::{
    BlobsProtocol,
    protocol::{ChunkRanges, ChunkRangesExt, GetRequest, ObserveRequest},
    provider::events::{EventMask, EventSender, ObserveMode},
    store::mem::MemStore,
};
use n0_future::StreamExt;

async fn node(observe: ObserveMode) -> anyhow::Result<(Router, MemStore, tokio::sync::mpsc::Receiver<iroh_blobs::provider::events::ProviderMessage>)> {
    let ep = Endpoint::builder(presets::Minimal).relay_mode(RelayMode::Disabled).bind().await?;
    let store = MemStore::new();
    let (tx, rx) = EventSender::channel(32, EventMask { observe, ..EventMask::DEFAULT });
    let blobs = BlobsProtocol::new(&store, Some(tx));
    let router = Router::builder(ep).accept(iroh_blobs::ALPN, blobs).spawn();
    Ok((router, store, rx))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (a, a_store, _a_rx) = node(ObserveMode::None).await?;
    let (b, b_store, _b_rx) = node(ObserveMode::None).await?;
    let (c, c_store, _c_rx) = node(ObserveMode::Disabled).await?; // default: observe off
    let data: Vec<u8> = (0..1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    let hash = a_store.add_bytes(data.clone()).await?.hash;
    let _ = c_store.add_bytes(data).await?; // C holds the full blob but keeps the default observe mask

    // B fetches only the first 256 KiB from A.
    let conn = b.endpoint().connect(a.endpoint().addr(), iroh_blobs::ALPN).await?;
    b_store.remote().execute_get(conn, GetRequest::blob_ranges(hash, ChunkRanges::bytes(0..256 * 1024))).await?;
    println!("B local bitfield: {:?}", b_store.blobs().observe(hash).await?.ranges);

    // C asks B (partial holder) and A (full holder) what they have, without downloading.
    for (name, r) in [("B", &b), ("A", &a)] {
        let conn = c.endpoint().connect(r.endpoint().addr(), iroh_blobs::ALPN).await?;
        let mut s = c_store.remote().observe(conn, ObserveRequest::new(hash));
        match tokio::time::timeout(std::time::Duration::from_secs(3), s.next()).await {
            Ok(Some(Ok(bf))) => println!("Observe({name}) -> size={} ranges={:?} complete={}", bf.size(), bf.ranges, bf.is_complete()),
            other => println!("Observe({name}) -> {other:?}"),
        }
    }
    // Observe against C (ObserveMode::Disabled = the default mask).
    let conn = b.endpoint().connect(c.endpoint().addr(), iroh_blobs::ALPN).await?;
    let mut s = b_store.remote().observe(conn, ObserveRequest::new(hash));
    println!("Observe(C, full holder, default mask) -> {:?}", tokio::time::timeout(std::time::Duration::from_secs(3), s.next()).await.map(|o| o.map(|r| r.map(|bf| bf.ranges))));
    for r in [a, b, c] { r.shutdown().await?; }
    Ok(())
}
