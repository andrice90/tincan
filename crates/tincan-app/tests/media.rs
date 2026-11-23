//! Media end-to-end tests: voice rooms, host migration with voice, 1:1 calls, the
//! soundboard and screen share, with two or three in-process `App` instances
//! (`offline_network`, `no_devices`: the synthetic microphone and the synthetic screen
//! pattern). Real protocol timings. The test needing a GPU encoder and screen capture is
//! `#[ignore]`d.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tincan_app::{App, AppConfig, TestOptions, UiEvent};
use tokio::sync::broadcast::{Receiver, error::RecvError};
use tokio::time::Instant;

/// One test at a time: each runs several endpoints with real timers.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const VOICE_KEYS: &[&str] = &["kind", "convId", "channelId", "name", "participants", "muted", "deafened", "sharing", "path", "rttMs"];
const PARTICIPANT_KEYS: &[&str] = &["userId", "name", "muted", "deafened", "speaking", "screenTrack", "volume"];
const CLIP_KEYS: &[&str] = &["hash", "name", "durationMs", "size", "owned", "heardFrom", "transfer"];
const CALL_KEYS: &[&str] = &["callId", "userId", "name", "state"];

async fn start(dir: &Path) -> Arc<App> {
    let cfg = AppConfig {
        data_dir: dir.to_path_buf(),
        app_version: "0.0.0-test".into(),
        file_url_base: "http://tincan.localhost/file/".into(),
        test: TestOptions { offline_network: true, no_devices: true },
    };
    App::start(cfg).await.expect("start")
}

async fn call(app: &App, cmd: &str, args: Value) -> Value {
    match tokio::time::timeout(Duration::from_secs(30), app.call(cmd, args)).await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => panic!("{cmd} failed: {e}"),
        Err(_) => panic!("{cmd} timed out"),
    }
}

async fn call_err(app: &App, cmd: &str, args: Value) -> String {
    match app.call(cmd, args).await {
        Ok(v) => panic!("{cmd} should fail, got {v}"),
        Err(e) => e,
    }
}

struct Node {
    app: Arc<App>,
    rx: Receiver<UiEvent>,
    user: String,
}

async fn node(dir: &Path, name: &str) -> Node {
    let app = start(dir).await;
    let rx = app.subscribe();
    let me = call(&app, "set_name", json!({ "name": name })).await;
    let user = me["userId"].as_str().unwrap().to_string();
    Node { app, rx, user }
}

/// The next `name` event whose payload satisfies `pred`.
async fn wait_for(rx: &mut Receiver<UiEvent>, name: &str, secs: u64, mut pred: impl FnMut(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut skipped: Vec<String> = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Ok(ev)) => {
                assert!(App::event_names().contains(&ev.name), "emitted an unknown event {}", ev.name);
                if ev.name == name && pred(&ev.payload) {
                    return ev.payload;
                }
                if ev.name != "speaking" && ev.name != "mic-level" {
                    let mut s = format!("{} {}", ev.name, ev.payload);
                    s.truncate(300);
                    skipped.push(s);
                }
            }
            Ok(Err(RecvError::Lagged(_))) => continue,
            Ok(Err(RecvError::Closed)) => panic!("event stream closed while waiting for {name}"),
            Err(_) => {
                let tail = &skipped[skipped.len().saturating_sub(12)..];
                panic!("timed out waiting for {name}; last events:\n{}", tail.join("\n"))
            }
        }
    }
}

/// An event name and a predicate on its payload.
type Want<'a> = (&'a str, &'a dyn Fn(&Value) -> bool);

/// Wait until every `(event, predicate)` pair has been seen, in any order.
async fn wait_each(rx: &mut Receiver<UiEvent>, secs: u64, wants: &[Want<'_>]) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut seen = vec![false; wants.len()];
    while seen.iter().any(|s| !s) {
        let left = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Ok(ev)) => {
                for (i, (name, pred)) in wants.iter().enumerate() {
                    if !seen[i] && ev.name == *name && pred(&ev.payload) {
                        seen[i] = true;
                    }
                }
            }
            Ok(Err(RecvError::Lagged(_))) => continue,
            Ok(Err(RecvError::Closed)) => panic!("event stream closed"),
            Err(_) => {
                let missing: Vec<&str> = wants.iter().zip(&seen).filter(|(_, s)| !**s).map(|((n, _), _)| *n).collect();
                panic!("timed out waiting for {missing:?}");
            }
        }
    }
}

async fn eventually<T, F, Fut>(what: &str, secs: u64, mut f: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(v) = f().await {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn keys(v: &Value) -> Vec<String> {
    let mut k: Vec<String> = v.as_object().unwrap_or_else(|| panic!("object expected: {v}")).keys().cloned().collect();
    k.sort();
    k
}

fn assert_keys(v: &Value, want: &[&str]) {
    let mut w: Vec<String> = want.iter().map(|s| s.to_string()).collect();
    w.sort();
    assert_eq!(keys(v), w, "shape of {v}");
}

fn assert_voice_shape(v: &Value) {
    assert_keys(v, VOICE_KEYS);
    for p in v["participants"].as_array().unwrap() {
        assert_keys(p, PARTICIPANT_KEYS);
    }
}

/// User ids of a `VoiceSession`'s participants, sorted.
fn participants(v: &Value) -> Vec<String> {
    let mut out: Vec<String> = v["participants"].as_array().map(|a| a.iter().map(|p| p["userId"].as_str().unwrap().to_string()).collect()).unwrap_or_default();
    out.sort();
    out
}

fn sorted(users: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = users.iter().map(|s| s.to_string()).collect();
    v.sort();
    v
}

fn participant<'a>(v: &'a Value, user: &str) -> Option<&'a Value> { v["participants"].as_array()?.iter().find(|p| p["userId"] == user) }

/// Create a server on `owner`, let `others` join, wait until everyone is online.
async fn server_with(owner: &Node, others: &[&Node], name: &str) -> String {
    let server = call(&owner.app, "create_server", json!({ "name": name })).await["id"].as_str().unwrap().to_string();
    let invite = call(&owner.app, "create_invite", json!({ "serverId": server })).await;
    for n in others {
        call(&n.app, "join_server", json!({ "invite": invite })).await;
    }
    let mut all: Vec<&Node> = vec![owner];
    all.extend_from_slice(others);
    online(&all, &server).await;
    server
}

async fn online(nodes: &[&Node], server: &str) {
    for n in nodes {
        eventually("everyone online", 30, || async {
            let m = call(&n.app, "members", json!({ "serverId": server })).await;
            let on: Vec<String> = m.as_array().unwrap().iter().filter(|x| x["online"] == true).map(|x| x["userId"].as_str().unwrap().to_string()).collect();
            nodes.iter().all(|o| on.contains(&o.user)).then_some(())
        })
        .await;
    }
}

/// The voice room of `server` (every server starts with one).
async fn voice_room(app: &App, server: &str) -> String {
    eventually("a voice room", 20, || async {
        let chans = call(app, "channels", json!({ "serverId": server })).await;
        chans.as_array().unwrap().iter().find(|c| c["kind"] == "voice").map(|c| c["id"].as_str().unwrap().to_string())
    })
    .await
}

fn audio_in(app: &App) -> u64 { app.test_media_counters()["audioIn"].as_u64().unwrap() }

/// Audio datagrams keep reaching `app`'s engine.
async fn audio_flows(app: &App, who: &str) {
    let before = audio_in(app);
    tokio::time::sleep(Duration::from_secs(2)).await;
    let got = audio_in(app) - before;
    assert!(got >= 20, "{who} received only {got} audio packets in 2 s");
}

/// A 16-bit mono PCM WAV of `ms` milliseconds of a 440 Hz tone at `rate`.
fn wav(ms: u32, rate: u32) -> Vec<u8> {
    let n = (u64::from(rate) * u64::from(ms) / 1000) as u32;
    let mut out = Vec::with_capacity(44 + n as usize * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + n * 2).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(n * 2).to_le_bytes());
    for i in 0..n {
        let v = ((i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * 9000.0) as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

fn write(dir: &Path, name: &str, bytes: &[u8]) -> String {
    let p: PathBuf = dir.join(name);
    std::fs::write(&p, bytes).unwrap();
    p.to_string_lossy().into_owned()
}

/// Parse a raw video message: (keyframe, track, ts, access unit).
fn parse_video(m: &[u8]) -> (bool, u16, u32, &[u8]) {
    assert!(m.len() > 7, "video message too short");
    (m[0] & 1 == 1, u16::from_be_bytes([m[1], m[2]]), u32::from_be_bytes([m[3], m[4], m[5], m[6]]), &m[7..])
}

// ---------------------------------------------------------------------------------- tests

/// (a) Two members in one voice room: synthetic audio from each reaches the other's
/// engine (speaking events and packet counts), voice-updated lists both, mute and leave
/// propagate, the mic test reports levels, and a crashed engine is restarted with the
/// session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn voice_room_audio_flows_both_ways() {
    let _g = SERIAL.lock().await;
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = node(da.path(), "Alice").await;
    let mut b = node(db.path(), "Bob").await;
    let server = server_with(&a, &[&b], "Talk").await;
    let room = voice_room(&a.app, &server).await;
    a.app.test_synthetic_voice(true);
    b.app.test_synthetic_voice(true);

    let va = call(&a.app, "voice_join", json!({ "serverId": server, "channelId": room })).await;
    assert_voice_shape(&va);
    assert_eq!(va["kind"], "room");
    assert_eq!(va["convId"], server.as_str());
    assert_eq!(va["channelId"], room.as_str());
    assert!(participant(&va, &a.user).is_some(), "I am in my own session");
    let again = call(&a.app, "voice_join", json!({ "serverId": server, "channelId": room })).await;
    assert_eq!(again["channelId"], room.as_str(), "joining the same room again is a no-op");
    call(&b.app, "voice_join", json!({ "serverId": server, "channelId": room })).await;

    let both = sorted(&[&a.user, &b.user]);
    for n in [&mut a, &mut b] {
        let v = wait_for(&mut n.rx, "voice-updated", 20, |v| !v.is_null() && participants(v) == both && v["path"] != "none").await;
        assert_voice_shape(&v);
    }
    let speaking = wait_for(&mut b.rx, "speaking", 20, |s| s["userId"] == a.user.as_str() && s["speaking"] == true).await;
    assert_keys(&speaking, &["userId", "speaking"]);
    wait_for(&mut a.rx, "speaking", 20, |s| s["userId"] == b.user.as_str() && s["speaking"] == true).await;
    audio_flows(&b.app, "Bob").await;
    audio_flows(&a.app, "Alice").await;
    let snap = call(&b.app, "app_snapshot", json!({})).await;
    assert_voice_shape(&snap["voice"]);
    assert!(snap["call"].is_null());
    eventually("members show the room", 10, || async {
        let m = call(&a.app, "members", json!({ "serverId": server })).await;
        m.as_array().unwrap().iter().any(|x| x["userId"] == b.user.as_str() && x["voiceChannelId"] == room.as_str()).then_some(())
    })
    .await;

    // Mute and deafen follow the UI model and reach the other side.
    call(&a.app, "set_deafened", json!({ "deafened": true })).await;
    wait_for(&mut b.rx, "voice-updated", 20, |v| participant(v, &a.user).is_some_and(|p| p["muted"] == true && p["deafened"] == true)).await;
    let mine = call(&a.app, "app_snapshot", json!({})).await["voice"].clone();
    assert_eq!((mine["muted"].clone(), mine["deafened"].clone()), (json!(true), json!(true)));
    call(&a.app, "set_muted", json!({ "muted": false })).await;
    wait_for(&mut b.rx, "voice-updated", 20, |v| participant(v, &a.user).is_some_and(|p| p["muted"] == false && p["deafened"] == false)).await;
    call(&b.app, "set_user_volume", json!({ "userId": a.user, "volume": 0.5 })).await;
    assert!(call_err(&b.app, "set_user_volume", json!({ "userId": "nope", "volume": 1 })).await.contains("not valid"));

    // A dead engine thread is replaced and the session keeps its audio.
    a.app.test_crash_audio_engine();
    wait_for(&mut a.rx, "toast", 10, |t| t["text"] == "Audio stopped working and was restarted.").await;
    assert_eq!(a.app.test_media_counters()["engineRestarts"], 1);
    // The restart lit every ring off first, so this comes from the new engine decoding Bob.
    wait_for(&mut a.rx, "speaking", 10, |s| s["userId"] == b.user.as_str() && s["speaking"] == true).await;
    // Only Alice sends to Bob: these packets come from her new engine.
    audio_flows(&b.app, "Bob after Alice's engine restart").await;

    call(&a.app, "voice_leave", json!({})).await;
    wait_for(&mut a.rx, "voice-updated", 10, Value::is_null).await;
    wait_for(&mut b.rx, "voice-updated", 20, |v| participants(v) == [b.user.clone()]).await;
    assert!(call(&a.app, "app_snapshot", json!({})).await["voice"].is_null());

    call(&b.app, "voice_leave", json!({})).await;
    call(&b.app, "mic_test", json!({ "on": true })).await;
    let level = wait_for(&mut b.rx, "mic-level", 10, |_| true).await;
    assert!(level["level"].as_f64().is_some_and(|l| (0.0..=1.0).contains(&l)));
    call(&b.app, "mic_test", json!({ "on": false })).await;

    a.app.shutdown().await;
    b.app.shutdown().await;
}

/// (b) Three in voice; the host shuts down; voice resumes between the other two on the
/// new host.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn voice_resumes_after_the_host_leaves() {
    let _g = SERIAL.lock().await;
    let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let a = node(dirs[0].path(), "Alice").await;
    let mut b = node(dirs[1].path(), "Bob").await;
    let mut c = node(dirs[2].path(), "Cam").await;
    let server = server_with(&a, &[&b, &c], "Club").await;
    let room = voice_room(&a.app, &server).await;
    for n in [&a, &b, &c] {
        n.app.test_synthetic_voice(true);
        call(&n.app, "voice_join", json!({ "serverId": server, "channelId": room })).await;
    }
    let all = sorted(&[&a.user, &b.user, &c.user]);
    for n in [&mut b, &mut c] {
        wait_for(&mut n.rx, "voice-updated", 20, |v| participants(v) == all).await;
    }
    // Let statuses and the succession list settle.
    tokio::time::sleep(Duration::from_secs(2)).await;

    a.app.shutdown().await;
    let rest = sorted(&[&b.user, &c.user]);
    for n in [&mut b, &mut c] {
        let v = wait_for(&mut n.rx, "voice-updated", 40, |v| participants(v) == rest && v["path"] != "none").await;
        assert_voice_shape(&v);
        assert_eq!(v["channelId"], room.as_str());
    }
    let host = eventually("a new host", 30, || async {
        let s = call(&b.app, "app_snapshot", json!({})).await;
        s["servers"][0]["connection"]["hostId"].as_str().filter(|h| *h != a.user).map(str::to_string)
    })
    .await;
    assert!(host == b.user || host == c.user);
    // The session state itself (not only an event from before the handoff) is on the new host.
    for n in [&b, &c] {
        let v = call(&n.app, "app_snapshot", json!({})).await["voice"].clone();
        assert_eq!(participants(&v), rest, "{v}");
        assert_ne!(v["path"], "none");
    }
    // Fresh speaking transitions prove audio crosses the new host both ways: silence both
    // until every ring is off (250 ms hangover + 300 ms hold), drop the old events, talk again.
    for n in [&b, &c] {
        n.app.test_synthetic_voice(false);
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;
    while b.rx.try_recv().is_ok() {}
    while c.rx.try_recv().is_ok() {}
    for n in [&b, &c] {
        n.app.test_synthetic_voice(true);
    }
    wait_for(&mut b.rx, "speaking", 10, |s| s["userId"] == c.user.as_str() && s["speaking"] == true).await;
    wait_for(&mut c.rx, "speaking", 10, |s| s["userId"] == b.user.as_str() && s["speaking"] == true).await;
    audio_flows(&b.app, "Bob after the migration").await;
    audio_flows(&c.app, "Cam after the migration").await;

    b.app.shutdown().await;
    c.app.shutdown().await;
}

/// (c) A 1:1 call: ringing on the callee, accept, audio both ways, hang up on both sides;
/// then a declined call.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn call_rings_connects_and_hangs_up() {
    let _g = SERIAL.lock().await;
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = node(da.path(), "Alice").await;
    let mut b = node(db.path(), "Bob").await;
    a.app.test_add_addr(&b.user, &b.app.test_addrs()).unwrap();
    b.app.test_add_addr(&a.user, &a.app.test_addrs()).unwrap();
    a.app.test_synthetic_voice(true);
    b.app.test_synthetic_voice(true);
    assert_eq!(call_err(&a.app, "call_start", json!({ "userId": a.user })).await, "You can't call yourself.");

    let out = call(&a.app, "call_start", json!({ "userId": b.user })).await;
    assert_keys(&out, CALL_KEYS);
    assert_eq!(out["state"], "ringing-out");
    assert_eq!(out["userId"], b.user.as_str());
    let ring = wait_for(&mut b.rx, "call-updated", 20, |c| c["state"] == "ringing-in").await;
    assert_keys(&ring, CALL_KEYS);
    assert_eq!(ring["userId"], a.user.as_str());
    assert_eq!(call(&b.app, "app_snapshot", json!({})).await["call"]["state"], "ringing-in");

    let v = call(&b.app, "call_accept", json!({ "callId": ring["callId"] })).await;
    assert_voice_shape(&v);
    assert_eq!(v["kind"], "call");
    assert!(v["channelId"].is_null());
    assert_eq!(participants(&v), sorted(&[&a.user, &b.user]));
    let active: [Want; 2] =
        [("call-updated", &|c| c["state"] == "active"), ("voice-updated", &|v| v["kind"] == "call" && v["path"] != "none")];
    wait_each(&mut a.rx, 20, &active).await;
    // Q6: calls report the RTT of their connection, like rooms.
    for n in [&a, &b] {
        eventually("the call's RTT", 20, || async { call(&n.app, "app_snapshot", json!({})).await["voice"]["rttMs"].as_u64().map(|_| ()) }).await;
    }
    wait_for(&mut a.rx, "speaking", 20, |s| s["userId"] == b.user.as_str() && s["speaking"] == true).await;
    wait_for(&mut b.rx, "speaking", 20, |s| s["userId"] == a.user.as_str() && s["speaking"] == true).await;
    audio_flows(&a.app, "Alice in the call").await;
    audio_flows(&b.app, "Bob in the call").await;

    call(&a.app, "call_hangup", json!({})).await;
    let ended: [Want; 2] = [("call-updated", &Value::is_null), ("voice-updated", &Value::is_null)];
    wait_each(&mut a.rx, 10, &ended).await;
    wait_each(&mut b.rx, 20, &ended).await;
    for n in [&a, &b] {
        let s = call(&n.app, "app_snapshot", json!({})).await;
        assert!(s["voice"].is_null() && s["call"].is_null(), "{s}");
    }

    // A declined call ends on both sides, with a notice for the caller.
    call(&a.app, "call_start", json!({ "userId": b.user })).await;
    let ring = wait_for(&mut b.rx, "call-updated", 20, |c| c["state"] == "ringing-in").await;
    call(&b.app, "call_decline", json!({ "callId": ring["callId"] })).await;
    wait_for(&mut a.rx, "toast", 20, |t| t["text"].as_str().is_some_and(|s| s.ends_with("declined the call."))).await;
    assert!(call(&a.app, "app_snapshot", json!({})).await["call"].is_null());

    // UI-LOGIC-09: when the callee's audio can't start, accepting ends the call instead of
    // leaving both sides in a silent call nothing in the UI can end.
    b.app.test_fail_audio_start(true);
    call(&a.app, "call_start", json!({ "userId": b.user })).await;
    let ring = wait_for(&mut b.rx, "call-updated", 20, |c| c["state"] == "ringing-in").await;
    assert_eq!(call_err(&b.app, "call_accept", json!({ "callId": ring["callId"] })).await, "Couldn't start audio. Try again.");
    for n in [&a, &b] {
        eventually("the unattached call ended", 15, || async {
            let s = call(&n.app, "app_snapshot", json!({})).await;
            (s["call"].is_null() && s["voice"].is_null()).then_some(())
        })
        .await;
    }
    b.app.test_fail_audio_start(false);

    a.app.shutdown().await;
    b.app.shutdown().await;
}

/// Deleting a voice room takes the people in it out (SPEC-GAPS-3); a held attachment
/// announced as a clip but larger than 5 MB is refused when saved, with no pull left
/// behind (APP-MEDIA-4); the saved per-user volume is part of the session (UI-LOGIC-13).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn voice_room_edges() {
    let _g = SERIAL.lock().await;
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let files = tempfile::tempdir().unwrap();
    let mut a = node(da.path(), "Alice").await;
    let mut b = node(db.path(), "Bob").await;
    let server = server_with(&a, &[&b], "Edges").await;
    let general = eventually("a text channel", 20, || async {
        let chans = call(&a.app, "channels", json!({ "serverId": server })).await;
        chans.as_array().unwrap().iter().find(|c| c["kind"] == "text").map(|c| c["id"].as_str().unwrap().to_string())
    })
    .await;

    // A 6 MB attachment that Bob auto-fetches.
    let big = write(files.path(), "big.bin", &vec![7u8; 6 * 1024 * 1024]);
    let sent = call(&a.app, "send_message", json!({ "convId": server, "channelId": general, "text": "", "attachments": [big] })).await;
    let hash = sent["attachments"][0]["hash"].as_str().unwrap().to_string();
    eventually("Bob holds the attachment", 60, || async {
        (call(&b.app, "transfer", json!({ "hash": hash })).await["state"] == "complete").then_some(())
    })
    .await;

    let room = voice_room(&a.app, &server).await;
    call(&a.app, "voice_join", json!({ "serverId": server, "channelId": room })).await;
    call(&b.app, "voice_join", json!({ "serverId": server, "channelId": room })).await;
    wait_for(&mut b.rx, "voice-updated", 20, |v| participants(v).len() == 2).await;

    // The volume Bob picks for Alice comes back in the session.
    call(&b.app, "set_user_volume", json!({ "userId": a.user, "volume": 0.3 })).await;
    let v = wait_for(&mut b.rx, "voice-updated", 10, |v| participant(v, &a.user).is_some_and(|p| p["volume"] == json!(0.3))).await;
    assert_voice_shape(&v);
    assert_eq!(participant(&v, &b.user).unwrap()["volume"], json!(1.0), "my own volume is 1");

    // Alice announces the attachment's hash as a small clip.
    a.app.test_sound_played(&hash, "Big", 1000, 1000).await.unwrap();
    let heard = wait_for(&mut b.rx, "sound-played", 20, |s| s["clip"]["hash"] == hash.as_str()).await;
    assert_eq!(heard["clip"]["owned"], false);
    assert_eq!(call_err(&b.app, "clip_fetch", json!({ "hash": hash })).await, "That sound is larger than 5 MB, so it wasn't saved.");
    let clips = call(&b.app, "clips", json!({})).await;
    assert!(!clips.as_array().unwrap().iter().any(|c| c["hash"] == hash.as_str()), "{clips}");
    // The heard tile is gone too, so Save can't be clicked again.
    assert_eq!(call_err(&b.app, "clip_fetch", json!({ "hash": hash })).await, "That sound is no longer available.");

    // The owner deletes the room: both leave it.
    call(&a.app, "delete_channel", json!({ "serverId": server, "channelId": room })).await;
    let left: [Want; 2] = [("voice-updated", &Value::is_null), ("toast", &|t| t["text"] == "The voice room was deleted.")];
    wait_each(&mut b.rx, 20, &left).await;
    wait_each(&mut a.rx, 20, &left).await;
    for n in [&a, &b] {
        assert!(call(&n.app, "app_snapshot", json!({})).await["voice"].is_null());
    }
    eventually("nobody in voice", 20, || async {
        let m = call(&a.app, "members", json!({ "serverId": server })).await;
        m.as_array().unwrap().iter().all(|x| x["voiceChannelId"].is_null()).then_some(())
    })
    .await;

    a.app.shutdown().await;
    b.app.shutdown().await;
}

/// (d) Soundboard: limits, playing into the room, pulling a heard clip, and a third member
/// pulling it from the second after the uploader left.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn soundboard_clips_travel_between_members() {
    let _g = SERIAL.lock().await;
    let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let files = tempfile::tempdir().unwrap();
    let a = node(dirs[0].path(), "Alice").await;
    let mut b = node(dirs[1].path(), "Bob").await;

    // Limits: 20 s, 5 MB, real audio only. None of these may take the app down.
    let long = write(files.path(), "long.wav", &wav(25_000, 8_000));
    let e = call_err(&a.app, "clip_add", json!({ "path": long, "name": "Long" })).await;
    assert_eq!(e, "long.wav is longer than 20 seconds. Pick a shorter sound.");
    let big = write(files.path(), "big.wav", &vec![0u8; 5 * 1024 * 1024 + 1]);
    assert_eq!(call_err(&a.app, "clip_add", json!({ "path": big, "name": "Big" })).await, "big.wav is larger than 5 MB. Pick a smaller sound.");
    let mut x = 1u32;
    let junk: Vec<u8> = (0..30_000)
        .map(|_| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (x >> 24) as u8
        })
        .collect();
    let corrupt = write(files.path(), "corrupt.mp3", &junk);
    let e = call_err(&a.app, "clip_add", json!({ "path": corrupt, "name": "Bad" })).await;
    assert!(e.starts_with("corrupt.mp3 isn't a sound Tincan can play"), "{e}");
    let missing = files.path().join("missing.wav").to_string_lossy().into_owned();
    assert!(call_err(&a.app, "clip_add", json!({ "path": missing, "name": "x" })).await.starts_with("Couldn't read missing.wav"));
    let two = write(files.path(), "beep.wav", &wav(2000, 48_000));
    assert_eq!(call_err(&a.app, "clip_add", json!({ "path": two, "name": "x".repeat(40) })).await, "Sound names can be up to 32 characters.");

    let clip = call(&a.app, "clip_add", json!({ "path": two, "name": " Beep " })).await;
    assert_keys(&clip, CLIP_KEYS);
    assert_eq!(clip["name"], "Beep");
    assert_eq!(clip["owned"], true);
    assert!((1900..=2100).contains(&clip["durationMs"].as_u64().unwrap()), "{clip}");
    let hash = clip["hash"].as_str().unwrap().to_string();
    assert_eq!(call(&a.app, "clips", json!({})).await.as_array().unwrap().len(), 1);
    assert_eq!(call_err(&a.app, "clip_play", json!({ "hash": hash })).await, "not-in-voice");

    let server = server_with(&a, &[&b], "Sounds").await;
    let room = voice_room(&a.app, &server).await;
    call(&a.app, "voice_join", json!({ "serverId": server, "channelId": room })).await;
    call(&b.app, "voice_join", json!({ "serverId": server, "channelId": room })).await;
    let both = sorted(&[&a.user, &b.user]);
    wait_for(&mut b.rx, "voice-updated", 20, |v| participants(v) == both).await;
    call(&a.app, "set_deafened", json!({ "deafened": true })).await;
    assert_eq!(call_err(&a.app, "clip_play", json!({ "hash": hash })).await, "deafened");
    call(&a.app, "set_muted", json!({ "muted": false })).await;
    // The soundboard plays while muted.
    call(&a.app, "set_muted", json!({ "muted": true })).await;
    call(&a.app, "clip_play", json!({ "hash": hash })).await;

    let heard = wait_for(&mut b.rx, "sound-played", 20, |p| p["clip"]["hash"] == hash.as_str()).await;
    assert_keys(&heard, &["userId", "name", "clip"]);
    assert_keys(&heard["clip"], CLIP_KEYS);
    assert_eq!(heard["userId"], a.user.as_str());
    assert_eq!(heard["name"], "Alice");
    assert_eq!(heard["clip"]["owned"], false);
    assert_eq!(heard["clip"]["heardFrom"], a.user.as_str());
    let listed = call(&b.app, "clips", json!({})).await;
    assert!(listed.as_array().unwrap().iter().any(|c| c["hash"] == hash.as_str() && c["owned"] == false));
    // The clip's audio arrived inside Alice's stream (her mic is muted).
    audio_flows(&b.app, "Bob hearing the clip").await;

    let pulling = call(&b.app, "clip_fetch", json!({ "hash": hash })).await;
    assert_keys(&pulling, CLIP_KEYS);
    wait_for(&mut b.rx, "clips-updated", 30, |l| l.as_array().unwrap().iter().any(|c| c["hash"] == hash.as_str() && c["owned"] == true)).await;
    let mine = call(&b.app, "clips", json!({})).await;
    let saved = mine.as_array().unwrap().iter().find(|c| c["hash"] == hash.as_str()).unwrap().clone();
    assert_eq!((saved["name"].clone(), saved["owned"].clone()), (json!("Beep"), json!(true)));
    assert_eq!(saved["durationMs"], clip["durationMs"]);
    assert_eq!(call(&b.app, "clip_fetch", json!({ "hash": hash })).await["owned"], true, "fetching an owned clip is a no-op");

    // Alice leaves for good: Bob hosts, Cam joins and pulls the clip from Bob.
    a.app.shutdown().await;
    wait_for(&mut b.rx, "server-updated", 40, |s| s["id"] == server.as_str() && s["connection"]["hostId"] == b.user.as_str()).await;
    let invite = call(&b.app, "create_invite", json!({ "serverId": server })).await;
    let mut c = node(dirs[2].path(), "Cam").await;
    call(&c.app, "join_server", json!({ "invite": invite })).await;
    online(&[&b, &c], &server).await;
    call(&c.app, "voice_join", json!({ "serverId": server, "channelId": room })).await;
    let bc = sorted(&[&b.user, &c.user]);
    wait_for(&mut c.rx, "voice-updated", 30, |v| participants(v) == bc).await;
    wait_for(&mut b.rx, "voice-updated", 30, |v| participants(v) == bc).await;
    call(&b.app, "clip_play", json!({ "hash": hash })).await;
    let heard = wait_for(&mut c.rx, "sound-played", 20, |p| p["clip"]["hash"] == hash.as_str()).await;
    assert_eq!(heard["userId"], b.user.as_str());
    call(&c.app, "clip_fetch", json!({ "hash": hash })).await;
    wait_for(&mut c.rx, "clips-updated", 30, |l| l.as_array().unwrap().iter().any(|x| x["hash"] == hash.as_str() && x["owned"] == true)).await;

    // Rename and remove are local to the library.
    call(&c.app, "clip_rename", json!({ "hash": hash, "name": "Boop" })).await;
    wait_for(&mut c.rx, "clips-updated", 10, |l| l.as_array().unwrap().iter().any(|x| x["name"] == "Boop")).await;
    call(&c.app, "clip_remove", json!({ "hash": hash })).await;
    assert_eq!(call(&c.app, "clips", json!({})).await, json!([]));
    assert_eq!(call_err(&c.app, "clip_rename", json!({ "hash": hash, "name": "Gone" })).await, "That sound was removed.");

    b.app.shutdown().await;
    c.app.shutdown().await;
}

/// (e) Screen share through the app path (synthetic pattern, no GPU needed): the watcher
/// gets keyframe-first raw messages; stopping the share ends the watch.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn screen_share_reaches_a_watcher() {
    let _g = SERIAL.lock().await;
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = node(da.path(), "Alice").await;
    let mut b = node(db.path(), "Bob").await;
    let server = server_with(&a, &[&b], "Screens").await;
    let room = voice_room(&a.app, &server).await;
    call(&a.app, "voice_join", json!({ "serverId": server, "channelId": room })).await;
    call(&b.app, "voice_join", json!({ "serverId": server, "channelId": room })).await;
    let sources = call(&a.app, "screen_sources", json!({})).await;
    assert!(sources.as_array().unwrap().iter().any(|s| s["id"] == "test:pattern"));
    let e = call_err(&a.app, "screen_start", json!({ "sourceId": "display:0", "mode": "text" })).await;
    assert_eq!(e, "Screen capture is off in this test profile.");

    let track = call(&a.app, "screen_start", json!({ "sourceId": "test:pattern", "mode": "text" })).await.as_u64().unwrap() as u16;
    let v = wait_for(&mut b.rx, "voice-updated", 20, |v| participant(v, &a.user).is_some_and(|p| p["screenTrack"] == track)).await;
    assert_voice_shape(&v);
    let mine = call(&a.app, "app_snapshot", json!({})).await["voice"].clone();
    assert_eq!(mine["sharing"], true);
    assert!(b.app.screen_watch(track ^ 4, Box::new(|_| true)).await.is_err(), "not a shared track");

    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    b.app.screen_watch(track, Box::new(move |m| tx.send(m).is_ok())).await.expect("watch");
    let mut msgs = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while msgs.len() < 20 && std::time::Instant::now() < deadline {
        if let Ok(m) = rx.recv_timeout(Duration::from_millis(200)) {
            msgs.push(m);
        }
    }
    assert!(msgs.len() >= 20, "got {} video messages", msgs.len());
    let (key, t, _, au) = parse_video(&msgs[0]);
    assert!(key, "the first message is a keyframe");
    assert_eq!(t, track);
    assert_eq!(&au[..4], &[0, 0, 0, 1], "Annex-B access unit");
    let mut last_ts = None;
    for m in &msgs {
        let (_, t, ts, au) = parse_video(m);
        assert_eq!(t, track);
        assert!(!au.is_empty());
        if let Some(prev) = last_ts {
            assert!(ts > prev, "timestamps increase");
        }
        last_ts = Some(ts);
    }

    call(&a.app, "screen_stop", json!({})).await;
    wait_for(&mut b.rx, "voice-updated", 20, |v| participant(v, &a.user).is_some_and(|p| p["screenTrack"].is_null())).await;
    // The watch ends with the share: the sink is dropped.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            _ => assert!(std::time::Instant::now() < deadline, "the watch did not end with the share"),
        }
    }
    assert_eq!(call(&a.app, "app_snapshot", json!({})).await["voice"]["sharing"], false);

    // screen_unwatch stops delivery too.
    let track = call(&a.app, "screen_start", json!({ "sourceId": "test:pattern", "mode": "motion" })).await.as_u64().unwrap() as u16;
    wait_for(&mut b.rx, "voice-updated", 20, |v| participant(v, &a.user).is_some_and(|p| p["screenTrack"] == track)).await;
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    b.app.screen_watch(track, Box::new(move |m| tx.send(m).is_ok())).await.expect("watch again");
    assert!(rx.recv_timeout(Duration::from_secs(10)).is_ok(), "frames after re-sharing");
    call(&b.app, "screen_unwatch", json!({ "track": track })).await;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            _ => assert!(std::time::Instant::now() < deadline, "unwatch did not drop the sink"),
        }
    }
    // Leaving voice ends the share.
    call(&a.app, "voice_leave", json!({})).await;
    wait_for(&mut b.rx, "voice-updated", 20, |v| participants(v) == [b.user.clone()]).await;

    a.app.shutdown().await;
    b.app.shutdown().await;
}

/// (e, hardware) A real share of the primary display for 2 s reaches the watcher as H.264
/// starting at a keyframe with SPS. Needs a GPU encoder and a desktop session:
/// `cargo test -p tincan-app --test media -- --ignored real_screen_share`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "captures the real primary display and needs a hardware H.264 encoder"]
async fn real_screen_share_of_the_primary_display() {
    let _g = SERIAL.lock().await;
    let hw = tokio::task::spawn_blocking(tincan_screen::hardware_encoder_available).await.unwrap();
    if !hw {
        eprintln!("no hardware H.264 encoder on this machine: screen_start must say so");
    }
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = node(da.path(), "Alice").await;
    let mut b = node(db.path(), "Bob").await;
    a.app.test_allow_screen_capture();
    let server = server_with(&a, &[&b], "Real").await;
    let room = voice_room(&a.app, &server).await;
    call(&a.app, "voice_join", json!({ "serverId": server, "channelId": room })).await;
    call(&b.app, "voice_join", json!({ "serverId": server, "channelId": room })).await;
    let sources = call(&a.app, "screen_sources", json!({})).await;
    eprintln!("sources: {sources}");
    assert!(sources.as_array().unwrap().iter().any(|s| s["id"] == "display:0"));
    if !hw {
        assert_eq!(call_err(&a.app, "screen_start", json!({ "sourceId": "display:0", "mode": "text" })).await, "no-hardware-encoder");
        a.app.shutdown().await;
        b.app.shutdown().await;
        return;
    }
    let started = Instant::now();
    let track = call(&a.app, "screen_start", json!({ "sourceId": "display:0", "mode": "text" })).await.as_u64().unwrap() as u16;
    eprintln!("screen_start took {:?}", started.elapsed());
    wait_for(&mut b.rx, "voice-updated", 20, |v| participant(v, &a.user).is_some_and(|p| p["screenTrack"] == track)).await;
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    b.app.screen_watch(track, Box::new(move |m| tx.send(m).is_ok())).await.expect("watch");
    let mut msgs = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if let Ok(m) = rx.recv_timeout(Duration::from_millis(100)) {
            msgs.push(m);
        }
    }
    assert!(!msgs.is_empty(), "no frames in 2 s");
    let (key, t, _, au) = parse_video(&msgs[0]);
    assert!(key && t == track);
    let codec = tincan_core::media::h264_codec_string(au);
    let bytes: usize = msgs.iter().map(Vec::len).sum();
    let keys = msgs.iter().filter(|m| m[0] & 1 == 1).count();
    eprintln!("{} messages ({keys} keyframes, {bytes} bytes) in 2 s; codec {codec:?}", msgs.len());
    assert!(codec.is_some_and(|c| c.starts_with("avc1.")), "the first keyframe carries an SPS");
    call(&a.app, "screen_stop", json!({})).await;
    a.app.shutdown().await;
    b.app.shutdown().await;
}
