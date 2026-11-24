//! In-process tests of screen share audio: only those watching a share get its audio, in
//! rooms (member and host paths) and in 1:1 calls; unwatching stops it. Relays disabled.

use std::collections::HashSet;
use std::time::Duration;

use bytes::Bytes;
use tincan_core::media::{AudioHeader, KIND_SCREEN, KIND_SHARE_AUDIO, VideoFrame, track_id, track_kind};
use tokio::sync::{broadcast, mpsc};
use tokio::time::Instant;

use crate::media::call_short_id;
use crate::tests::{Node, cluster, fast, node, until, voice_channel, wire, within};
use crate::{CallState, MediaSession, NetEvent};

fn pkt(seq: u16) -> Bytes { AudioHeader { track: 0, seq, ts: u32::from(seq) * 960, ..AudioHeader::default() }.packet(b"stereo-opus") }

/// Send one packet every 20 ms on `tx` until it closes.
fn pump(tx: mpsc::Sender<Bytes>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut seq = 0u16;
        let mut tick = tokio::time::interval(Duration::from_millis(20));
        loop {
            tick.tick().await;
            if tx.send(pkt(seq)).await.is_err() {
                return;
            }
            seq = seq.wrapping_add(1);
        }
    })
}

/// 15 frames a second into a share, a keyframe every 10, until the channel closes.
fn frames(tx: mpsc::Sender<VideoFrame>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut i = 0u32;
        let mut tick = tokio::time::interval(Duration::from_millis(66));
        loop {
            tick.tick().await;
            let f = VideoFrame { ts: i * 6000, keyframe: i.is_multiple_of(10), data: Bytes::from(vec![7u8; 500]) };
            if tx.send(f).await.is_err() {
                return;
            }
            i += 1;
        }
    })
}

/// Share-audio track ids waiting in `rx` (voice is skipped).
fn share_tracks(rx: &mut mpsc::Receiver<Bytes>) -> Vec<u16> {
    let mut out = Vec::new();
    while let Ok(p) = rx.try_recv() {
        if let Some((h, _)) = AudioHeader::parse(&p)
            && track_kind(h.track) == KIND_SHARE_AUDIO
        {
            out.push(h.track);
        }
    }
    out
}

async fn join(n: &Node, server: tincan_core::ServerId, ch: tincan_core::ChannelId) -> MediaSession {
    within("voice_join", Duration::from_secs(5), n.net.voice_join(server, ch)).await.expect("voice_join")
}

async fn connected(nodes: &[&Node]) {
    until("voice connected", Duration::from_secs(10), || nodes.iter().all(|n| n.net.voice_info().is_some_and(|v| v.connected))).await;
}

/// Wait until `rx` delivers `track`, or fail.
async fn hears(rx: &mut mpsc::Receiver<Bytes>, track: u16, who: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if share_tracks(rx).contains(&track) {
            return;
        }
        assert!(Instant::now() < deadline, "{who} never got share audio {track}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Collect share-audio tracks arriving over `ms`.
async fn heard_over(rx: &mut mpsc::Receiver<Bytes>, ms: u64) -> HashSet<u16> {
    let mut got = HashSet::new();
    let end = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < end {
        got.extend(share_tracks(rx));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    got
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn share_audio_reaches_watchers_only() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    let c = node("carol", fast()).await;
    let d = node("dave", fast()).await;
    wire(&[&a, &b, &c, &d]);
    let server = cluster(&a, &[&b, &c, &d]).await;
    let lounge = voice_channel(&a, &server);
    let mut sa = join(&a, server, lounge).await;
    let sb = join(&b, server, lounge).await;
    let mut sc = join(&c, server, lounge).await;
    let mut sd = join(&d, server, lounge).await;
    connected(&[&a, &b, &c, &d]).await;

    // Share audio before a share starts goes nowhere.
    let audio_b = pump(sb.share_audio_out.clone());
    let short_b = b.net.my_short_id(&server).unwrap();
    let screen_b = track_id(short_b, KIND_SCREEN);
    let audio_track_b = track_id(short_b, KIND_SHARE_AUDIO);
    c.net.subscribe_track(screen_b);
    assert!(heard_over(&mut sc.audio_in, 400).await.is_empty(), "share audio without a share");

    // Bob shares: carol (member) and alice (the host's own app) watch, dave does not.
    let (track, f) = b.net.share_start().unwrap();
    assert_eq!(track, screen_b);
    let video_b = frames(f);
    a.net.subscribe_track(screen_b);
    hears(&mut sc.audio_in, audio_track_b, "carol (member viewer)").await;
    hears(&mut sa.audio_in, audio_track_b, "alice (host viewer)").await;
    let at_d = heard_over(&mut sd.audio_in, 500).await;
    assert!(at_d.is_empty(), "dave does not watch but got {at_d:?}");
    // The packets carry bob's share-audio track, stamped by the network layer.
    assert_eq!(heard_over(&mut sc.audio_in, 300).await, HashSet::from([audio_track_b]));

    // Carol stops watching: her share audio stops.
    c.net.unsubscribe_track(screen_b);
    tokio::time::sleep(Duration::from_millis(300)).await;
    share_tracks(&mut sc.audio_in);
    assert!(heard_over(&mut sc.audio_in, 400).await.is_empty(), "share audio after unwatching");
    // Dave starts watching: now he gets it.
    d.net.subscribe_track(screen_b);
    hears(&mut sd.audio_in, audio_track_b, "dave after subscribing").await;

    // Bob stops sharing: nothing more, even though his app keeps pushing share audio.
    b.net.share_stop();
    tokio::time::sleep(Duration::from_millis(300)).await;
    share_tracks(&mut sd.audio_in);
    assert!(heard_over(&mut sd.audio_in, 400).await.is_empty(), "share audio after the share stopped");
    audio_b.abort();
    video_b.abort();

    // The host shares: a member watching gets the host's share audio, others do not.
    let (screen_a, f) = a.net.share_start().unwrap();
    let video_a = frames(f);
    let audio_a = pump(sa.share_audio_out.clone());
    c.net.subscribe_track(screen_a);
    let audio_track_a = track_id(a.net.my_short_id(&server).unwrap(), KIND_SHARE_AUDIO);
    hears(&mut sc.audio_in, audio_track_a, "carol watching the host").await;
    let at_d = heard_over(&mut sd.audio_in, 400).await;
    assert!(!at_d.contains(&audio_track_a), "dave does not watch the host's share");
    audio_a.abort();
    video_a.abort();
    for n in [a, b, c, d] {
        n.net.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn call_share_audio_flows_only_while_the_peer_watches() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    wire(&[&a, &b]);
    let mut b_ev = b.net.subscribe();
    let (_info, sa) = within("call_start", Duration::from_secs(10), a.net.call_start(b.net.me())).await.expect("call_start");
    let call_id = loop {
        match within("ring", Duration::from_secs(5), b_ev.recv()).await {
            Ok(NetEvent::CallInvite { call }) => break call.call_id,
            Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(e) => panic!("{e}"),
        }
    };
    let mut sb = within("call_accept", Duration::from_secs(5), b.net.call_accept(call_id)).await.expect("call_accept");
    until("call active", Duration::from_secs(5), || a.net.call_info().is_some_and(|c| c.state == CallState::Active)).await;
    connected(&[&a, &b]).await;
    let (screen, f) = a.net.share_start().unwrap();
    let video = frames(f);
    let audio = pump(sa.share_audio_out.clone());
    let audio_track = track_id(call_short_id(&a.net.me(), &b.net.me()), KIND_SHARE_AUDIO);
    assert_eq!(screen, track_id(call_short_id(&a.net.me(), &b.net.me()), KIND_SCREEN));
    assert!(heard_over(&mut sb.audio_in, 500).await.is_empty(), "the peer does not watch yet");
    b.net.subscribe_track(screen);
    hears(&mut sb.audio_in, audio_track, "the call peer").await;
    b.net.unsubscribe_track(screen);
    tokio::time::sleep(Duration::from_millis(300)).await;
    share_tracks(&mut sb.audio_in);
    assert!(heard_over(&mut sb.audio_in, 400).await.is_empty(), "share audio after the peer unwatched");
    audio.abort();
    video.abort();
    a.net.call_hangup().await;
    for n in [a, b] {
        n.net.shutdown().await;
    }
}
