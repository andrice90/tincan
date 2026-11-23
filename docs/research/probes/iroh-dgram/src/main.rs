// Throwaway experiment: verify iroh 1.5 datagram + stream-priority APIs used by a media forwarder,
// and observe drop-oldest behaviour of a small datagram send buffer. Loopback only, relays disabled.
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use iroh::endpoint::{QuicTransportConfig, presets};
use iroh::{Endpoint, RelayMode};

const ALPN: &[u8] = b"tincan/media-test/0";

fn transport() -> QuicTransportConfig {
    QuicTransportConfig::builder()
        // A real-time sender wants stale datagrams dropped, not queued: 1 MiB default would be seconds of audio.
        .datagram_send_buffer_size(16 * 1024)
        .datagram_receive_buffer_size(Some(256 * 1024))
        .congestion_controller_factory(Arc::new(noq_proto::congestion::Bbr3Config::default()))
        .build()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let server = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .transport_config(transport())
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await?;
    let client = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .transport_config(transport())
        .bind()
        .await?;
    let addr = server.addr();

    let srv = tokio::spawn({
        let server = server.clone();
        async move {
            let conn = server.accept().await.unwrap().await.unwrap();
            // Count datagrams for 1.5 s.
            let mut n = 0usize;
            let deadline = tokio::time::sleep(Duration::from_millis(1500));
            tokio::pin!(deadline);
            loop {
                tokio::select! {
                    d = conn.read_datagram() => { if d.is_ok() { n += 1 } else { break } }
                    _ = &mut deadline => break,
                }
            }
            // Then read stream-per-frame "video" frames concurrently and record completion order.
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let t0 = Instant::now();
            for _ in 0..3 {
                let mut r = conn.accept_uni().await.unwrap();
                let tx = tx.clone();
                tokio::spawn(async move {
                    let buf = r.read_to_end(4 << 20).await.unwrap();
                    let _ = tx.send((buf[0], buf.len(), t0.elapsed()));
                });
            }
            drop(tx);
            let mut order = Vec::new();
            while let Some(x) = rx.recv().await { order.push(x); }
            (n, order, conn)
        }
    });

    let conn = client.connect(addr, ALPN).await?;
    println!("max_datagram_size = {:?}", conn.max_datagram_size());
    println!("datagram_send_buffer_space (fresh) = {}", conn.datagram_send_buffer_space());

    // Burst 5000 x 1000-byte datagrams without awaiting: only what fits the 16 KiB buffer + cwnd survives.
    let payload = Bytes::from(vec![7u8; 1000]);
    let t = Instant::now();
    let mut queued = 0;
    for _ in 0..5000 {
        if conn.send_datagram(payload.clone()).is_ok() {
            queued += 1;
        }
    }
    println!("send_datagram accepted {queued} of 5000 in {:?} (older ones may be dropped)", t.elapsed());

    tokio::time::sleep(Duration::from_millis(1600)).await;

    // Stream per frame with priorities: low (old GOP) opened first, high (audio-like) last.
    let mut lo = conn.open_uni().await?;
    lo.set_priority(0)?;
    let mut mid = conn.open_uni().await?;
    mid.set_priority(10)?;
    let mut hi = conn.open_uni().await?;
    hi.set_priority(100)?;
    let big = vec![1u8; 3_000_000];
    let mut b2 = vec![2u8; 3_000_000];
    b2[0] = 2;
    let (a, b, c) = tokio::join!(
        async { lo.write_all(&big).await.unwrap(); lo.finish().unwrap(); },
        async { mid.write_all(&b2).await.unwrap(); mid.finish().unwrap(); },
        async { tokio::time::sleep(Duration::from_millis(5)).await; hi.write_all(&[3u8; 1000]).await.unwrap(); hi.finish().unwrap(); },
    );
    let _ = (a, b, c);

    let (n, order, sconn) = srv.await?;
    println!("server received {n} datagrams of the burst");
    println!("streams completed (prio-tag, bytes, t): {:?}", order);
    let stats = conn.stats();
    println!("client udp tx datagrams: {}", stats.udp_tx.datagrams);
    sconn.close(0u32.into(), b"done");
    conn.close(0u32.into(), b"done");
    client.close().await;
    server.close().await;
    Ok(())
}

// Compile check: per-connection media transport config on both dial and accept sides.
#[allow(dead_code)]
async fn media_connection_per_conn(ep: &Endpoint, host: iroh::EndpointAddr) -> anyhow::Result<()> {
    use iroh::endpoint::ConnectOptions;
    let opts = ConnectOptions::new().with_transport_config(transport());
    let connecting = ep.connect_with_opts(host, ALPN, opts).await?; // returns Connecting
    let conn = connecting.await?;
    let _ = conn.max_datagram_size();
    for p in conn.paths().iter() {
        let _ = (p.is_selected(), p.is_relay(), p.rtt());
    }
    // accept side
    let server_cfg = Arc::new(ep.create_server_config_builder(vec![ALPN.to_vec()]).set_transport_config(transport()).build());
    if let Some(incoming) = ep.accept().await {
        let accepting = incoming.accept_with(server_cfg)?;
        let _c = accepting.await?;
    }
    Ok(())
}
