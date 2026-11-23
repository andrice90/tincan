//! In-process media tests: voice rooms through the host SFU, migration, screen share (order,
//! drop policy, GOP cache), soundboard announcements and 1:1 calls. Relays disabled, loopback.

use std::collections::HashSet;
use std::time::Duration;

use bytes::Bytes;
use tincan_core::event::{Body, ChannelKind};
use tincan_core::media::{AudioHeader, KIND_SCREEN, KIND_VOICE, VideoFrame, track_id};
use tincan_core::wire::ClipInfo;
use tincan_core::{BlobHash, ChannelId, ServerId, UserId};
use tokio::sync::{broadcast, mpsc};
use tokio::time::Instant;

use crate::media::call_short_id;
use crate::tests::{Node, cluster, fast, node, until, voice_channel, wire, wire_to, within};
use crate::timing::Timings;
use crate::{CallEnd, CallState, MediaSession, NetError, NetEvent};

/// A synthetic audio datagram (the net layer re-stamps track and epoch).
fn audio_pkt(seq: u16) -> Bytes { AudioHeader { track: 0, seq, ts: u32::from(seq) * 960, ..AudioHeader::default() }.packet(b"opus") }

/// Track ids of everything waiting in `rx`.
fn drain_tracks(rx: &mut mpsc::Receiver<Bytes>) -> Vec<u16> {
    let mut out = Vec::new();
    while let Ok(p) = rx.try_recv() {
        if let Some((h, _)) = AudioHeader::parse(&p) {
            out.push(h.track);
        }
    }
    out
}

/// Send one packet every 20 ms from `tx` until it closes.
fn pump_audio(tx: mpsc::Sender<Bytes>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut seq = 0u16;
        let mut tick = tokio::time::interval(Duration::from_millis(20));
        loop {
            tick.tick().await;
            if tx.send(audio_pkt(seq)).await.is_err() {
                return;
            }
            seq = seq.wrapping_add(1);
        }
    })
}

fn voice_track(n: &Node) -> u16 { n.net.voice_info().map(|v| v.local_track).unwrap_or(u16::MAX) }

async fn join_voice(n: &Node, server: ServerId, ch: ChannelId) -> MediaSession {
    within("voice_join", Duration::from_secs(5), n.net.voice_join(server, ch)).await.expect("voice_join")
}

async fn connected(nodes: &[&Node]) {
    until("voice sessions connected", Duration::from_secs(10), || nodes.iter().all(|n| n.net.voice_info().is_some_and(|v| v.connected))).await;
}

/// Wait for an event matching `f` (skipping lag).
async fn next_event<T>(rx: &mut broadcast::Receiver<NetEvent>, what: &str, timeout: Duration, mut f: impl FnMut(&NetEvent) -> Option<T>) -> T {
    within(what, timeout, async {
        loop {
            match rx.recv().await {
                Ok(e) => {
                    if let Some(v) = f(&e) {
                        return v;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => std::future::pending::<()>().await,
            }
        }
    })
    .await
}

/// A second voice room, created by the owner `a`, known to every node.
async fn second_voice_room(a: &Node, others: &[&Node], server: ServerId) -> ChannelId {
    let ev = within(
        "create channel",
        Duration::from_secs(5),
        a.net.publish(server, Body::ChannelCreate { name: "Other".into(), kind: ChannelKind::Voice, position: 9 }),
    )
    .await
    .expect("channel create");
    let ch = ev.id;
    until("everyone knows the new room", Duration::from_secs(5), || {
        others.iter().all(|n| n.net.conv_state(&server).is_some_and(|s| s.channels.contains_key(&ch)))
    })
    .await;
    ch
}

fn frame_no(f: &VideoFrame) -> u32 { f.data.get(..4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]])).unwrap_or(u32::MAX) }

/// A synthetic encoder: 30 fps, a keyframe every `gop` frames, and one on request when
/// `honor` (as the app wires `KeyframeRequested` to `ScreenShare::request_keyframe`).
fn spawn_sharer(net: &crate::Net, frames: mpsc::Sender<VideoFrame>, track: u16, gop: u32, size: usize, honor: bool) -> tokio::task::JoinHandle<()> {
    let mut ev = net.subscribe();
    tokio::spawn(async move {
        let mut idx = 0u32;
        let mut tick = tokio::time::interval(Duration::from_millis(33));
        loop {
            tick.tick().await;
            let mut force = false;
            loop {
                match ev.try_recv() {
                    Ok(NetEvent::KeyframeRequested { track: t }) if t == track => force = honor,
                    Ok(_) | Err(broadcast::error::TryRecvError::Lagged(_)) => {}
                    Err(_) => break,
                }
            }
            let key = force || idx.is_multiple_of(gop);
            let mut data = vec![0u8; size.max(4)];
            data[..4].copy_from_slice(&idx.to_be_bytes());
            if frames.send(VideoFrame { ts: idx * 3000, keyframe: key, data: Bytes::from(data) }).await.is_err() {
                return;
            }
            idx += 1;
        }
    })
}

/// Frames received by a viewer, with their arrival time.
async fn collect_frames(rx: &mut mpsc::Receiver<(u16, VideoFrame)>, track: u16, n: usize, timeout: Duration) -> Vec<(Instant, VideoFrame)> {
    let mut out = Vec::new();
    let deadline = Instant::now() + timeout;
    while out.len() < n {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some((t, f))) if t == track => out.push((Instant::now(), f)),
            Ok(Some(_)) => {}
            _ => break,
        }
    }
    out
}

/// Starts at a keyframe, no gap inside a group, every discontinuity lands on a keyframe
/// (skipping ahead to a new group is allowed, going back is not).
fn assert_groups_clean(frames: &[(Instant, VideoFrame)], who: &str) {
    assert!(!frames.is_empty(), "{who}: no frames");
    assert!(frames[0].1.keyframe, "{who}: does not start at a keyframe");
    for w in frames.windows(2) {
        let (a, b) = (frame_no(&w[0].1), frame_no(&w[1].1));
        assert!(b > a, "{who}: frame {b} after {a}");
        if !w[1].1.keyframe {
            assert_eq!(b, a + 1, "{who}: gap inside a group after frame {a}");
        }
    }
}

/// Frame numbers are consecutive and the sequence starts at a keyframe.
fn assert_clean(frames: &[(Instant, VideoFrame)], who: &str) {
    assert!(!frames.is_empty(), "{who}: no frames");
    assert!(frames[0].1.keyframe, "{who}: does not start at a keyframe");
    for w in frames.windows(2) {
        assert_eq!(frame_no(&w[1].1), frame_no(&w[0].1) + 1, "{who}: gap or reorder after frame {}", frame_no(&w[0].1));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn voice_room_audio_reaches_room_members_only() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    let c = node("carol", fast()).await;
    let d = node("dave", fast()).await;
    wire(&[&a, &b, &c, &d]);
    let server = cluster(&a, &[&b, &c, &d]).await;
    let lounge = voice_channel(&a, &server);
    let other = second_voice_room(&a, &[&b, &c, &d], server).await;
    // A text channel is not a voice room.
    let text = crate::tests::text_channel(&a, &server);
    assert!(matches!(b.net.voice_join(server, text).await, Err(NetError::Invalid(_))));
    assert!(matches!(b.net.share_start(), Err(NetError::NotInVoice)));

    let mut sa = join_voice(&a, server, lounge).await;
    let mut sb = join_voice(&b, server, lounge).await;
    let mut sc = join_voice(&c, server, lounge).await;
    let mut sd = join_voice(&d, server, other).await;
    connected(&[&a, &b, &c, &d]).await;
    // The roster shows the rooms (Status is the announce that survives migrations).
    until("roster shows the rooms", Duration::from_secs(5), || {
        a.net.roster(&server).is_some_and(|r| {
            r.online.iter().filter(|e| e.voice == Some(lounge)).count() == 3 && r.online.iter().any(|e| e.user == d.net.me() && e.voice == Some(other))
        })
    })
    .await;
    let (ta, tb, tc, td) = (voice_track(&a), voice_track(&b), voice_track(&c), voice_track(&d));
    assert_eq!(ta, track_id(a.net.my_short_id(&server).unwrap(), KIND_VOICE));
    let pumps: Vec<_> = [&sa, &sb, &sc, &sd].iter().map(|s| pump_audio(s.audio_out.clone())).collect();

    let mut heard: [HashSet<u16>; 4] = Default::default();
    let deadline = Instant::now() + Duration::from_secs(10);
    let want = [[tb, tc], [ta, tc], [ta, tb]];
    loop {
        for (i, s) in [&mut sa, &mut sb, &mut sc, &mut sd].into_iter().enumerate() {
            heard[i].extend(drain_tracks(&mut s.audio_in));
        }
        let done = (0..3).all(|i| want[i].iter().all(|t| heard[i].contains(t)));
        if done {
            break;
        }
        assert!(Instant::now() < deadline, "audio did not reach the room: {heard:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Keep it flowing a little longer, then check nobody got anything they should not.
    tokio::time::sleep(Duration::from_millis(400)).await;
    for (i, s) in [&mut sa, &mut sb, &mut sc, &mut sd].into_iter().enumerate() {
        heard[i].extend(drain_tracks(&mut s.audio_in));
    }
    assert_eq!(heard[0], HashSet::from([tb, tc]), "host's local path");
    assert_eq!(heard[1], HashSet::from([ta, tc]));
    assert_eq!(heard[2], HashSet::from([ta, tb]));
    assert!(heard[3].is_empty(), "a member of another room heard {:?}", heard[3]);
    let _ = td;
    // Leaving stops the flow and clears the roster entry.
    b.net.voice_leave().await;
    assert!(b.net.voice_info().is_none());
    until("roster shows bob left", Duration::from_secs(5), || {
        a.net.roster(&server).is_some_and(|r| r.online.iter().any(|e| e.user == b.net.me() && e.voice.is_none()))
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    drain_tracks(&mut sa.audio_in);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!drain_tracks(&mut sa.audio_in).contains(&tb), "audio after leaving");
    for p in pumps {
        p.abort();
    }
    for n in [a, b, c, d] {
        n.net.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn voice_and_share_survive_host_migration() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    let c = node("carol", fast()).await;
    wire_to(&a, &[&b, &c]);
    let server = cluster(&a, &[&b, &c]).await;
    let lounge = voice_channel(&a, &server);
    crate::tests::wait_succession(&a, &server, 2).await;
    let _sa = join_voice(&a, server, lounge).await;
    let sb = join_voice(&b, server, lounge).await;
    let mut sc = join_voice(&c, server, lounge).await;
    connected(&[&a, &b, &c]).await;
    let pb = pump_audio(sb.audio_out.clone());
    let pc = pump_audio(sc.audio_out.clone());
    // Bob shares, carol watches.
    let (track, frames) = b.net.share_start().unwrap();
    assert_eq!(track, track_id(b.net.my_short_id(&server).unwrap(), KIND_SCREEN));
    let sharer = spawn_sharer(&b.net, frames, track, 30, 2000, true);
    c.net.subscribe_track(track);
    let first = collect_frames(&mut sc.video_in, track, 20, Duration::from_secs(10)).await;
    assert_eq!(first.len(), 20, "carol watches bob before the migration");
    assert_clean(&first, "carol before");
    until("bob's share in the roster", Duration::from_secs(5), || {
        a.net.roster(&server).is_some_and(|r| r.online.iter().any(|e| e.user == b.net.me() && e.sharing == Some(track)))
    })
    .await;
    let tb = voice_track(&b);
    let epoch_before = c.net.voice_info().unwrap().epoch;

    // The host leaves.
    let t0 = Instant::now();
    let leaving = a.net.clone();
    let shutdown = tokio::spawn(async move { leaving.shutdown().await });
    // Audio flows again (packets that left bob after the host was gone reach carol).
    until("new host", Duration::from_secs(15), || {
        let hb = crate::tests::host_of(&b, &server);
        hb.is_some() && hb != Some(a.net.me()) && hb == crate::tests::host_of(&c, &server)
    })
    .await;
    while sc.audio_in.try_recv().is_ok() {}
    let deadline = t0 + Duration::from_secs(15);
    loop {
        if drain_tracks(&mut sc.audio_in).contains(&tb) {
            break;
        }
        assert!(Instant::now() < deadline, "audio did not resume within 15 s");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    eprintln!("voice resumed after the host left in {:?}", t0.elapsed());
    // Video keeps coming over the new path: no gap inside a group, restarts at keyframes.
    let moved_at = Instant::now();
    let mut all = first;
    let deadline = t0 + Duration::from_secs(15);
    while all.iter().filter(|(at, _)| *at > moved_at).count() < 15 {
        assert!(Instant::now() < deadline, "video did not resume within 15 s");
        all.extend(collect_frames(&mut sc.video_in, track, 1, Duration::from_millis(500)).await);
    }
    assert_groups_clean(&all, "carol across the migration");
    eprintln!("video flowing on the new path {:?} after the host left", t0.elapsed());
    let info = c.net.voice_info().unwrap();
    assert!(info.connected);
    assert_ne!(info.epoch, epoch_before, "the epoch byte follows the new host");
    shutdown.await.unwrap();
    sharer.abort();
    pb.abort();
    pc.abort();
    for n in [b, c] {
        n.net.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn screen_share_frames_arrive_in_order_from_a_keyframe() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    let c = node("carol", fast()).await;
    wire(&[&a, &b, &c]);
    let server = cluster(&a, &[&b, &c]).await;
    let lounge = voice_channel(&a, &server);
    let mut sa = join_voice(&a, server, lounge).await;
    let _sb = join_voice(&b, server, lounge).await;
    let mut sc = join_voice(&c, server, lounge).await;
    connected(&[&a, &b, &c]).await;
    // A member shares; the host (local path) and another member watch.
    let (tb, frames) = b.net.share_start().unwrap();
    a.net.subscribe_track(tb);
    c.net.subscribe_track(tb);
    let sharer = spawn_sharer(&b.net, frames, tb, 10, 3000, false);
    let at_c = collect_frames(&mut sc.video_in, tb, 45, Duration::from_secs(10)).await;
    let at_a = collect_frames(&mut sa.video_in, tb, 45, Duration::from_secs(10)).await;
    assert_eq!(at_c.len(), 45);
    assert_eq!(at_a.len(), 45);
    assert_clean(&at_c, "member viewer");
    assert_clean(&at_a, "host viewer");
    assert!(at_c.iter().filter(|(_, f)| f.keyframe).count() >= 4, "several groups");
    sharer.abort();
    b.net.share_stop();
    // The host shares; a member watches.
    let (ta, frames) = a.net.share_start().unwrap();
    c.net.subscribe_track(ta);
    let sharer = spawn_sharer(&a.net, frames, ta, 10, 3000, false);
    let from_host = collect_frames(&mut sc.video_in, ta, 30, Duration::from_secs(10)).await;
    assert_eq!(from_host.len(), 30);
    assert_clean(&from_host, "viewer of the host's share");
    // Unsubscribing stops delivery.
    c.net.unsubscribe_track(ta);
    tokio::time::sleep(Duration::from_millis(300)).await;
    while sc.video_in.try_recv().is_ok() {}
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(sc.video_in.try_recv().is_err(), "frames after unsubscribe");
    sharer.abort();
    for n in [a, b, c] {
        n.net.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_viewer_is_dropped_to_keyframes_without_hurting_a_fast_one() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    let c = node("carol", fast()).await;
    let d = node("dave", fast()).await;
    wire(&[&a, &b, &c, &d]);
    let server = cluster(&a, &[&b, &c, &d]).await;
    let lounge = voice_channel(&a, &server);
    let _sb = join_voice(&b, server, lounge).await;
    let mut sc = join_voice(&c, server, lounge).await;
    let mut sd = join_voice(&d, server, lounge).await;
    connected(&[&b, &c, &d]).await;
    let mut b_ev = b.net.subscribe();
    let (track, frames) = b.net.share_start().unwrap();
    // Dave stops reading his streams: QUIC flow control pushes back on the host.
    d.net.pause_video(true);
    c.net.subscribe_track(track);
    d.net.subscribe_track(track);
    // The fast viewer is read all along, in its own task.
    let mut c_video = std::mem::replace(&mut sc.video_in, mpsc::channel(1).1);
    let fast_task = tokio::spawn(async move { collect_frames(&mut c_video, track, 150, Duration::from_secs(20)).await });
    // 30 KB frames at 30 fps (about 7 Mbit/s), a keyframe every 3 s unless requested.
    let sharer = spawn_sharer(&b.net, frames, track, 90, 30_000, true);
    let first_target = next_event(&mut b_ev, "first bitrate target", Duration::from_secs(10), |e| match e {
        NetEvent::TargetBitrate { track: t, bps } if *t == track => Some(*bps),
        _ => None,
    })
    .await;
    // The slow viewer's drops come back to the sharer as keyframe requests and a lower target.
    let (mut keyframe_asked, mut lowered) = (false, None);
    within("keyframe request and lower target after the drops", Duration::from_secs(15), async {
        while !(keyframe_asked && lowered.is_some()) {
            match b_ev.recv().await {
                Ok(NetEvent::KeyframeRequested { track: t }) if t == track => keyframe_asked = true,
                Ok(NetEvent::TargetBitrate { track: t, bps }) if t == track && bps < first_target => lowered = Some(bps),
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => std::future::pending::<()>().await,
            }
        }
    })
    .await;
    eprintln!("target bitrate {first_target} -> {lowered:?} after the slow viewer's drops");
    // Meanwhile the fast viewer got every frame, in order.
    let fast_frames = fast_task.await.expect("fast viewer task");
    assert_eq!(fast_frames.len(), 150);
    assert_clean(&fast_frames, "fast viewer");
    // Dave resumes: he restarts at a keyframe, and groups are whole from then on.
    while sd.video_in.try_recv().is_ok() {}
    d.net.pause_video(false);
    let resumed = collect_frames(&mut sd.video_in, track, 60, Duration::from_secs(10)).await;
    assert!(resumed.len() >= 30, "slow viewer resumed: {} frames", resumed.len());
    assert_groups_clean(&resumed, "slow viewer after resuming");
    sharer.abort();
    for n in [a, b, c, d] {
        n.net.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gop_cache_starts_a_new_viewer_at_once() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    let c = node("carol", fast()).await;
    wire(&[&a, &b, &c]);
    let server = cluster(&a, &[&b, &c]).await;
    let lounge = voice_channel(&a, &server);
    let _sb = join_voice(&b, server, lounge).await;
    let mut sc = join_voice(&c, server, lounge).await;
    connected(&[&b, &c]).await;
    let (track, frames) = b.net.share_start().unwrap();
    // One keyframe, then deltas only (keyframe requests are ignored): only the cache can help.
    let sharer = spawn_sharer(&b.net, frames, track, 100_000, 2000, false);
    tokio::time::sleep(Duration::from_millis(700)).await;
    let t0 = Instant::now();
    c.net.subscribe_track(track);
    let got = collect_frames(&mut sc.video_in, track, 10, Duration::from_secs(3)).await;
    assert!(!got.is_empty(), "nothing arrived");
    let wait = got[0].0.duration_since(t0);
    eprintln!("new viewer's first frame after {wait:?}");
    assert!(wait < Duration::from_millis(500), "first frame took {wait:?}");
    assert!(got[0].1.keyframe && frame_no(&got[0].1) == 0, "starts with the cached keyframe");
    assert_clean(&got, "cache-started viewer");
    sharer.abort();
    for n in [a, b, c] {
        n.net.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sound_played_reaches_room_members_only() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    let c = node("carol", fast()).await;
    let d = node("dave", fast()).await;
    wire(&[&a, &b, &c, &d]);
    let server = cluster(&a, &[&b, &c, &d]).await;
    let lounge = voice_channel(&a, &server);
    let other = second_voice_room(&a, &[&b, &c, &d], server).await;
    let _sa = join_voice(&a, server, lounge).await;
    let _sb = join_voice(&b, server, lounge).await;
    let _sc = join_voice(&c, server, lounge).await;
    let _sd = join_voice(&d, server, other).await;
    connected(&[&a, &b, &c, &d]).await;
    // Give the VoiceJoins a moment to reach the host.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut evs: Vec<broadcast::Receiver<NetEvent>> = [&a, &b, &c, &d].iter().map(|n| n.net.subscribe()).collect();
    let clip = ClipInfo { hash: BlobHash([3; 32]), name: "boing".into(), size: 1234, duration_ms: 800 };
    b.net.sound_played(clip.clone()).await.unwrap();
    let heard = |rx: &mut broadcast::Receiver<NetEvent>| -> Vec<(UserId, ClipInfo)> {
        let mut out = Vec::new();
        while let Ok(e) = rx.try_recv() {
            if let NetEvent::SoundPlayed { conv, by, clip } = e {
                assert_eq!(conv, server);
                out.push((by, clip));
            }
        }
        out
    };
    let mut got_a = Vec::new();
    let mut got_c = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while (got_a.is_empty() || got_c.is_empty()) && Instant::now() < deadline {
        got_a.extend(heard(&mut evs[0]));
        got_c.extend(heard(&mut evs[2]));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(got_a, vec![(b.net.me(), clip.clone())], "the host in the room");
    assert_eq!(got_c, vec![(b.net.me(), clip.clone())], "a member in the room");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(heard(&mut evs[1]).is_empty(), "the player does not hear its own announcement");
    assert!(heard(&mut evs[3]).is_empty(), "another room does not hear it");
    // The host plays: the members of its room hear it.
    a.net.sound_played(clip.clone()).await.unwrap();
    let by = next_event(&mut evs[1], "bob hears the host's clip", Duration::from_secs(5), |e| match e {
        NetEvent::SoundPlayed { by, .. } => Some(*by),
        _ => None,
    })
    .await;
    assert_eq!(by, a.net.me());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(heard(&mut evs[3]).is_empty());
    // Not in voice: refused.
    d.net.voice_leave().await;
    assert!(matches!(d.net.sound_played(clip).await, Err(NetError::NotInVoice)));
    for n in [a, b, c, d] {
        n.net.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dm_call_start_accept_hangup_with_audio_both_ways() {
    let t = Timings { call_ring: Duration::from_millis(1500), ..fast() };
    let a = node("alice", t.clone()).await;
    let b = node("bob", t).await;
    wire(&[&a, &b]);
    let mut a_ev = a.net.subscribe();
    let mut b_ev = b.net.subscribe();
    let (info, mut sa) = within("call_start", Duration::from_secs(10), a.net.call_start(b.net.me())).await.expect("call_start");
    assert_eq!(info.state, CallState::RingingOut);
    let call_id = next_event(&mut b_ev, "incoming ring", Duration::from_secs(5), |e| match e {
        NetEvent::CallInvite { call } => Some(call.call_id),
        _ => None,
    })
    .await;
    assert_eq!(call_id, info.call_id);
    assert_eq!(b.net.call_info().map(|c| c.state), Some(CallState::RingingIn));
    let mut sb = within("call_accept", Duration::from_secs(5), b.net.call_accept(call_id)).await.expect("call_accept");
    next_event(&mut a_ev, "call active at the caller", Duration::from_secs(5), |e| match e {
        NetEvent::CallUpdated { call } if call.state == CallState::Active => Some(()),
        _ => None,
    })
    .await;
    connected(&[&a, &b]).await;
    let (ta, tb) = (track_id(call_short_id(&a.net.me(), &b.net.me()), KIND_VOICE), track_id(call_short_id(&b.net.me(), &a.net.me()), KIND_VOICE));
    assert_ne!(ta, tb);
    assert_eq!(voice_track(&a), ta);
    let pa = pump_audio(sa.audio_out.clone());
    let pb = pump_audio(sb.audio_out.clone());
    let deadline = Instant::now() + Duration::from_secs(5);
    let (mut at_a, mut at_b) = (HashSet::new(), HashSet::new());
    while !(at_a.contains(&tb) && at_b.contains(&ta)) {
        assert!(Instant::now() < deadline, "call audio: a heard {at_a:?}, b heard {at_b:?}");
        at_a.extend(drain_tracks(&mut sa.audio_in));
        at_b.extend(drain_tracks(&mut sb.audio_in));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!at_a.contains(&ta) && !at_b.contains(&tb), "nobody hears themselves");
    // Mute state reaches the peer.
    a.net.set_status(|s| s.muted = true);
    next_event(&mut b_ev, "peer muted", Duration::from_secs(5), |e| match e {
        NetEvent::CallUpdated { call } if call.peer_muted => Some(()),
        _ => None,
    })
    .await;
    // Screen share inside the call.
    let (share_track, frames) = a.net.share_start().unwrap();
    let peer_sharing = next_event(&mut b_ev, "peer shares", Duration::from_secs(5), |e| match e {
        NetEvent::CallUpdated { call } => call.peer_sharing,
        _ => None,
    })
    .await;
    assert_eq!(peer_sharing, share_track);
    b.net.subscribe_track(share_track);
    let sharer = spawn_sharer(&a.net, frames, share_track, 10, 2000, true);
    let got = collect_frames(&mut sb.video_in, share_track, 20, Duration::from_secs(10)).await;
    assert_eq!(got.len(), 20, "call video");
    assert_clean(&got, "call viewer");
    sharer.abort();
    // Hang up.
    a.net.call_hangup().await;
    let why = next_event(&mut b_ev, "remote hangup", Duration::from_secs(5), |e| match e {
        NetEvent::CallUpdated { call } if call.state == CallState::Ended => call.ended,
        _ => None,
    })
    .await;
    assert_eq!(why, CallEnd::Hangup);
    assert!(a.net.call_info().is_none() && b.net.call_info().is_none());
    assert!(a.net.voice_info().is_none());
    pa.abort();
    pb.abort();

    // Declined.
    let (_info, _s) = a.net.call_start(b.net.me()).await.unwrap();
    let id = next_event(&mut b_ev, "second ring", Duration::from_secs(5), |e| match e {
        NetEvent::CallInvite { call } => Some(call.call_id),
        _ => None,
    })
    .await;
    b.net.call_decline(id).await;
    let why = next_event(&mut a_ev, "declined", Duration::from_secs(5), |e| match e {
        NetEvent::CallUpdated { call } if call.state == CallState::Ended && call.call_id == id => call.ended,
        _ => None,
    })
    .await;
    assert_eq!(why, CallEnd::Declined);

    // Nobody answers: both sides end after the ringing timeout.
    let (third, _s) = a.net.call_start(b.net.me()).await.unwrap();
    let t0 = Instant::now();
    let why_a = next_event(&mut a_ev, "no answer (caller)", Duration::from_secs(5), |e| match e {
        NetEvent::CallUpdated { call } if call.state == CallState::Ended && call.call_id == third.call_id => call.ended,
        _ => None,
    })
    .await;
    assert_eq!(why_a, CallEnd::NoAnswer);
    assert!(t0.elapsed() >= Duration::from_millis(1400), "ended before the ringing timeout");
    let why_b = next_event(&mut b_ev, "no answer (callee)", Duration::from_secs(5), |e| match e {
        NetEvent::CallUpdated { call } if call.state == CallState::Ended && call.call_id == third.call_id => call.ended,
        _ => None,
    })
    .await;
    assert!(matches!(why_b, CallEnd::NoAnswer | CallEnd::Cancelled), "{why_b:?}");
    for n in [a, b] {
        n.net.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn calls_glare_and_busy() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    let c = node("carol", fast()).await;
    wire(&[&a, &b, &c]);
    // Make sure the DM links exist so both invites go out at once.
    a.net.dm_connect(b.net.me()).await.unwrap();
    let mut a_ev = a.net.subscribe();
    let mut b_ev = b.net.subscribe();
    let (ra, rb) = tokio::join!(a.net.call_start(b.net.me()), b.net.call_start(a.net.me()));
    let (_ia, _sa) = ra.expect("a rings b");
    let (_ib, _sb) = rb.expect("b rings a");
    for (ev, who) in [(&mut a_ev, "a"), (&mut b_ev, "b")] {
        next_event(ev, &format!("{who}: call becomes active"), Duration::from_secs(5), |e| match e {
            NetEvent::CallUpdated { call } if call.state == CallState::Active => Some(()),
            _ => None,
        })
        .await;
    }
    let (ia, ib) = (a.net.call_info().unwrap(), b.net.call_info().unwrap());
    assert_eq!(ia.call_id, ib.call_id, "both sides settled on one call");
    // A third caller gets a busy decline.
    let mut c_ev = c.net.subscribe();
    let _ring = c.net.call_start(a.net.me()).await.unwrap();
    let why = next_event(&mut c_ev, "busy", Duration::from_secs(5), |e| match e {
        NetEvent::CallUpdated { call } if call.state == CallState::Ended => call.ended,
        _ => None,
    })
    .await;
    assert_eq!(why, CallEnd::Declined);
    assert_eq!(a.net.call_info().map(|c| c.state), Some(CallState::Active));
    for n in [a, b, c] {
        n.net.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn voice_resumes_after_host_crash() {
    // Real liveness (tick 2 s, dead at 10 s): failover comes from silence.
    let t = Timings { tick: Duration::from_secs(2), host_suspect: Duration::from_secs(6), host_dead: Duration::from_secs(10), ..fast() };
    let a = node("alice", t.clone()).await;
    let b = node("bob", t.clone()).await;
    let c = node("carol", t).await;
    wire_to(&a, &[&b, &c]);
    let server = cluster(&a, &[&b, &c]).await;
    let lounge = voice_channel(&a, &server);
    crate::tests::wait_succession(&a, &server, 2).await;
    until("members know the succession", Duration::from_secs(5), || {
        [&b, &c].iter().all(|n| n.net.roster(&server).is_some_and(|r| r.succession.len() >= 2))
    })
    .await;
    let _sa = join_voice(&a, server, lounge).await;
    let sb = join_voice(&b, server, lounge).await;
    let mut sc = join_voice(&c, server, lounge).await;
    connected(&[&a, &b, &c]).await;
    let pb = pump_audio(sb.audio_out.clone());
    let tb = voice_track(&b);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !drain_tracks(&mut sc.audio_in).contains(&tb) {
        assert!(Instant::now() < deadline, "no audio before the crash");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let t0 = Instant::now();
    // `crash` freezes the session actors with every connection left open (a hung process),
    // so the members notice only through tick silence (10 s).
    a.net.crash();
    let me_a = a.net.me();
    until("failover to a successor", Duration::from_secs(20), || {
        let hb = crate::tests::host_of(&b, &server);
        hb.is_some() && hb != Some(me_a) && hb == crate::tests::host_of(&c, &server)
    })
    .await;
    eprintln!("members moved to the successor {:?} after the crash", t0.elapsed());
    // Audio now flows through the new host.
    while sc.audio_in.try_recv().is_ok() {}
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if drain_tracks(&mut sc.audio_in).contains(&tb) {
            break;
        }
        assert!(Instant::now() < deadline, "audio did not resume through the new host");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let took = t0.elapsed();
    eprintln!("voice resumed {took:?} after the host crashed");
    assert!(took < Duration::from_secs(16), "took {took:?}");
    assert!(c.net.voice_info().is_some_and(|v| v.connected));
    pb.abort();
    for n in [b, c] {
        n.net.shutdown().await;
    }
}
