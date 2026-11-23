//! End-to-end tests: two or three in-process `App` instances on temp profiles with relays,
//! mDNS and rendezvous off (`TestOptions::offline_network`), driven only through
//! `App::call` JSON and `App::subscribe` events. Real protocol timings (6 s solo, 4 s dial).

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tincan_app::{App, AppConfig, TestOptions, UiEvent};
use tokio::sync::broadcast::{Receiver, error::RecvError};
use tokio::time::Instant;

/// The tests start several iroh endpoints each; run them one at a time so the real
/// protocol timers are not starved by each other.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn serial() -> tokio::sync::MutexGuard<'static, ()> { SERIAL.lock().await }

const FILE_BASE: &str = "http://tincan.localhost/file/";

async fn start(dir: &Path) -> Arc<App> {
    let cfg = AppConfig {
        data_dir: dir.to_path_buf(),
        app_version: "0.0.0-test".into(),
        file_url_base: FILE_BASE.into(),
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

/// A started instance with a name and an event receiver.
struct Node {
    app: Arc<App>,
    rx: Receiver<UiEvent>,
    user: String,
}

async fn node(dir: &Path, name: &str) -> Node {
    let app = start(dir).await;
    let rx = app.subscribe();
    let me = call(&app, "set_name", json!({ "name": name })).await;
    assert_eq!(me["name"], name);
    let user = me["userId"].as_str().unwrap().to_string();
    assert_eq!(me["userCode"], format!("tincan1u{user}"));
    Node { app, rx, user }
}

/// Wait for the next `name` event whose payload satisfies `pred` (other events are skipped).
async fn wait_for(rx: &mut Receiver<UiEvent>, name: &str, secs: u64, mut pred: impl FnMut(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Ok(ev)) => {
                assert!(App::event_names().contains(&ev.name), "emitted an unknown event {}", ev.name);
                if ev.name == name && pred(&ev.payload) {
                    return ev.payload;
                }
            }
            Ok(Err(RecvError::Lagged(_))) => continue,
            Ok(Err(RecvError::Closed)) => panic!("event stream closed while waiting for {name}"),
            Err(_) => panic!("timed out waiting for {name}"),
        }
    }
}

/// Poll `f` until it yields a value.
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
    let mut k: Vec<String> = v.as_object().expect("object").keys().cloned().collect();
    k.sort();
    k
}

fn assert_keys(v: &Value, want: &[&str]) {
    let mut w: Vec<String> = want.iter().map(|s| s.to_string()).collect();
    w.sort();
    assert_eq!(keys(v), w, "shape of {v}");
}

fn is_hex64(v: &Value) -> bool { v.as_str().is_some_and(|s| s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())) }

const MESSAGE_KEYS: &[&str] = &["id", "convId", "channelId", "authorId", "authorName", "text", "attachments", "sentAt", "deleted", "pending"];
const MEMBER_KEYS: &[&str] = &["userId", "name", "online", "isHost", "isOwner", "voiceChannelId", "muted", "deafened", "sharing", "speaking"];
const SERVER_KEYS: &[&str] = &["id", "name", "ownerId", "isOwner", "connection", "unread", "onlineCount"];
const CHANNEL_KEYS: &[&str] = &["id", "serverId", "name", "kind", "position", "unread"];
const TRANSFER_KEYS: &[&str] = &["hash", "state", "have", "total", "peers"];
const DM_KEYS: &[&str] = &["convId", "userId", "name", "online", "unread", "lastMessageAt"];

/// The first text channel of `server` (waits for the history sync to bring it).
async fn text_channel(app: &App, server: &str) -> String {
    eventually("a text channel", 20, || async {
        let chans = call(app, "channels", json!({ "serverId": server })).await;
        for c in chans.as_array().unwrap() {
            assert_keys(c, CHANNEL_KEYS);
        }
        chans.as_array().unwrap().iter().find(|c| c["kind"] == "text").map(|c| c["id"].as_str().unwrap().to_string())
    })
    .await
}

/// Members of `server` as seen by `app` that are online.
async fn online_members(app: &App, server: &str) -> Vec<String> {
    let m = call(app, "members", json!({ "serverId": server })).await;
    m.as_array().unwrap().iter().filter(|x| x["online"] == true).map(|x| x["userId"].as_str().unwrap().to_string()).collect()
}

async fn all_online(nodes: &[&Node], server: &str) {
    for n in nodes {
        eventually("everyone online", 30, || async {
            let online = online_members(&n.app, server).await;
            nodes.iter().all(|o| online.contains(&o.user)).then_some(())
        })
        .await;
    }
}

/// Every message text of a conversation, oldest first, paged `limit` at a time.
async fn all_texts(app: &App, conv: &str, channel: Option<&str>, limit: u32) -> (Vec<String>, usize) {
    let mut before: Option<String> = None;
    let mut pages: Vec<Vec<String>> = Vec::new();
    loop {
        let page = call(app, "messages", json!({ "convId": conv, "channelId": channel, "before": before, "limit": limit })).await;
        assert_keys(&page, &["messages", "cursor"]);
        let msgs = page["messages"].as_array().unwrap();
        assert!(msgs.len() <= limit as usize);
        let sent: Vec<u64> = msgs.iter().map(|m| m["sentAt"].as_u64().unwrap()).collect();
        assert!(sent.windows(2).all(|w| w[0] <= w[1]), "page is oldest first");
        pages.push(msgs.iter().map(|m| m["text"].as_str().unwrap().to_string()).collect());
        match page["cursor"].as_str() {
            Some(c) => {
                let (hlc, id) = c.split_once(':').expect("cursor is <hlc>:<id>");
                assert!(hlc.parse::<u64>().is_ok() && is_hex64(&json!(id)));
                before = Some(c.to_string());
            }
            None => break,
        }
    }
    let n = pages.len();
    pages.reverse();
    (pages.into_iter().flatten().collect(), n)
}

/// Pseudo-random bytes.
fn data(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed;
    (0..len)
        .map(|_| {
            x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            (x >> 33) as u8
        })
        .collect()
}

fn server_id(v: &Value) -> String { v["id"].as_str().unwrap().to_string() }

/// A join folds the first second of events into one refresh (history sync); tests that
/// assert live `message-added` events right after a join wait it out.
async fn settle_after_join() { tokio::time::sleep(Duration::from_millis(1500)).await; }

/// Messages in a channel, or None while the channel is not synced yet.
async fn message_count(app: &App, server: &str, channel: &str) -> Option<usize> {
    let mut before: Option<String> = None;
    let mut n = 0;
    loop {
        let page = app.call("messages", json!({ "convId": server, "channelId": channel, "before": before, "limit": 200 })).await.ok()?;
        n += page["messages"].as_array()?.len();
        match page["cursor"].as_str() {
            Some(c) => before = Some(c.to_string()),
            None => return Some(n),
        }
    }
}

/// Count events until a `snapshot` after which `server`'s channel holds `want` messages.
/// Returns (message-added count, total events).
async fn until_synced(rx: &mut Receiver<UiEvent>, app: &App, server: &str, channel: &str, want: usize) -> (usize, usize) {
    let deadline = Instant::now() + Duration::from_secs(40);
    let (mut added, mut total) = (0, 0);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let ev = match tokio::time::timeout(left, rx.recv()).await {
            Ok(Ok(ev)) => ev,
            Ok(Err(RecvError::Lagged(n))) => panic!("the UI stream lagged by {n} events"),
            Ok(Err(RecvError::Closed)) => panic!("event stream closed"),
            Err(_) => panic!("no snapshot with {want} messages"),
        };
        total += 1;
        if ev.name == "message-added" {
            added += 1;
        }
        if ev.name == "snapshot" && message_count(app, server, channel).await == Some(want) {
            return (added, total);
        }
    }
}

/// The shell spawns these futures on Tauri's runtime: they must be `Send`. Type-checked
/// only, never run.
#[allow(dead_code)]
fn public_futures_are_send(app: &App, cfg: AppConfig) {
    fn assert_send<T: Send>(_: &T) {}
    assert_send(&App::start(cfg));
    assert_send(&app.call("app_snapshot", json!({})));
    assert_send(&app.screen_watch(1, Box::new(|_| true)));
    assert_send(&app.file_for_url(""));
    assert_send(&app.shutdown());
}

// ---------------------------------------------------------------------------------- tests

/// (a) create, invite, join; members online; messages both ways as events and pages;
/// paging over several pages.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn create_join_chat_and_page() {
    let _g = serial().await;
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = node(da.path(), "Alice").await;
    let mut b = node(db.path(), "Bob").await;

    let snap = call(&a.app, "app_snapshot", json!({})).await;
    assert_keys(&snap, &["me", "servers", "dms", "settings", "voice", "call", "version"]);
    assert!(snap["voice"].is_null() && snap["call"].is_null());
    assert_eq!(snap["version"], "0.0.0-test");

    let created = call(&a.app, "create_server", json!({ "name": "Friends" })).await;
    assert_keys(&created, SERVER_KEYS);
    assert_keys(&created["connection"], &["state", "hostId", "hostName", "path", "rttMs"]);
    assert!(is_hex64(&created["id"]));
    assert_eq!(created["isOwner"], true);
    assert_eq!(created["ownerId"], a.user.as_str());
    let server = server_id(&created);
    let invite = call(&a.app, "create_invite", json!({ "serverId": server })).await;
    assert!(invite.as_str().unwrap().starts_with("tincan1"));

    let joined = call(&b.app, "join_server", json!({ "invite": invite })).await;
    assert_eq!(server_id(&joined), server);
    assert_eq!(joined["isOwner"], false);
    all_online(&[&a, &b], &server).await;
    settle_after_join().await;
    let members = call(&b.app, "members", json!({ "serverId": server })).await;
    for m in members.as_array().unwrap() {
        assert_keys(m, MEMBER_KEYS);
    }
    let alice = members.as_array().unwrap().iter().find(|m| m["userId"] == a.user.as_str()).unwrap();
    assert_eq!(alice["name"], "Alice");
    assert_eq!(alice["isOwner"], true);
    assert_eq!(alice["isHost"], true);
    let hosted = wait_for(&mut b.rx, "server-updated", 20, |s| {
        s["id"] == server.as_str() && s["connection"]["state"] == "member" && s["onlineCount"] == 2 && s["connection"]["hostName"] == "Alice"
    })
    .await;
    assert_keys(&hosted, SERVER_KEYS);
    assert_eq!(hosted["connection"]["hostId"], a.user.as_str());

    let general = text_channel(&b.app, &server).await;
    let sent = call(&a.app, "send_message", json!({ "convId": server, "channelId": general, "text": "hi bob", "attachments": [] })).await;
    assert_keys(&sent, MESSAGE_KEYS);
    assert_eq!(sent["pending"], false);
    let got = wait_for(&mut b.rx, "message-added", 20, |m| m["text"] == "hi bob").await;
    assert_keys(&got, MESSAGE_KEYS);
    assert_eq!(got["id"], sent["id"]);
    assert_eq!(got["authorId"], a.user.as_str());
    assert_eq!(got["authorName"], "Alice");
    assert_eq!(got["channelId"], general.as_str());
    assert_eq!(got["convId"], server.as_str());
    // Bob now has an unread channel.
    wait_for(&mut b.rx, "channels-updated", 10, |c| {
        c["serverId"] == server.as_str() && c["channels"].as_array().unwrap().iter().any(|x| x["id"] == general.as_str() && x["unread"] == true)
    })
    .await;
    call(&b.app, "mark_read", json!({ "convId": server, "channelId": general })).await;
    let chans = call(&b.app, "channels", json!({ "serverId": server })).await;
    assert!(chans.as_array().unwrap().iter().all(|c| c["unread"] == false));

    call(&b.app, "typing", json!({ "convId": server, "channelId": general })).await;
    let t = wait_for(&mut a.rx, "typing", 10, |_| true).await;
    assert_eq!(t, json!({ "convId": server, "channelId": general, "userId": b.user, "name": "Bob" }));

    call(&b.app, "send_message", json!({ "convId": server, "channelId": general, "text": "hi alice", "attachments": [] })).await;
    let back = wait_for(&mut a.rx, "message-added", 20, |m| m["text"] == "hi alice").await;
    assert_eq!(back["authorName"], "Bob");

    for i in 0..7 {
        call(&a.app, "send_message", json!({ "convId": server, "channelId": general, "text": format!("m{i}"), "attachments": [] })).await;
    }
    let want: Vec<String> = ["hi bob", "hi alice", "m0", "m1", "m2", "m3", "m4", "m5", "m6"].iter().map(|s| s.to_string()).collect();
    let (texts, pages) = eventually("all messages at Bob", 20, || async {
        let r = all_texts(&b.app, &server, Some(&general), 3).await;
        (r.0 == want).then_some(r)
    })
    .await;
    assert_eq!(texts, want);
    assert_eq!(pages, 3, "9 messages in pages of 3");
    assert_eq!(all_texts(&a.app, &server, Some(&general), 4).await.0, want);

    a.app.shutdown().await;
    b.app.shutdown().await;
}

/// (b) the host shuts down; the other two keep chatting and see the new host.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_leaves_and_the_rest_continue() {
    let _g = serial().await;
    let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let a = node(dirs[0].path(), "Alice").await;
    let mut b = node(dirs[1].path(), "Bob").await;
    let mut c = node(dirs[2].path(), "Cam").await;
    let server = server_id(&call(&a.app, "create_server", json!({ "name": "Club" })).await);
    let invite = call(&a.app, "create_invite", json!({ "serverId": server })).await;
    call(&b.app, "join_server", json!({ "invite": invite })).await;
    call(&c.app, "join_server", json!({ "invite": invite })).await;
    all_online(&[&a, &b, &c], &server).await;
    // Let statuses and the succession list settle.
    tokio::time::sleep(Duration::from_secs(2)).await;

    a.app.shutdown().await;
    let not_alice = |s: &Value| {
        s["id"] == server.as_str()
            && s["connection"]["hostId"].as_str().is_some_and(|h| h != a.user)
            && matches!(s["connection"]["state"].as_str(), Some("member" | "hosting"))
    };
    let sb = wait_for(&mut b.rx, "server-updated", 40, not_alice).await;
    let sc = wait_for(&mut c.rx, "server-updated", 40, not_alice).await;
    let host_b = sb["connection"]["hostId"].as_str().unwrap().to_string();
    let host_c = sc["connection"]["hostId"].as_str().unwrap().to_string();
    let host = eventually("both agree on the host", 30, || {
        let (b, c) = (b.app.clone(), c.app.clone());
        async move {
            let snap_b = call(&b, "app_snapshot", json!({})).await;
            let snap_c = call(&c, "app_snapshot", json!({})).await;
            let hb = snap_b["servers"][0]["connection"]["hostId"].as_str().map(str::to_string);
            let hc = snap_c["servers"][0]["connection"]["hostId"].as_str().map(str::to_string);
            hb.filter(|h| Some(h) == hc.as_ref())
        }
    })
    .await;
    assert!(host == b.user || host == c.user, "new host is Bob or Cam");
    assert!([host_b, host_c].iter().all(|h| *h == b.user || *h == c.user));

    let general = text_channel(&b.app, &server).await;
    call(&b.app, "send_message", json!({ "convId": server, "channelId": general, "text": "still here", "attachments": [] })).await;
    wait_for(&mut c.rx, "message-added", 30, |m| m["text"] == "still here").await;
    call(&c.app, "send_message", json!({ "convId": server, "channelId": general, "text": "me too", "attachments": [] })).await;
    wait_for(&mut b.rx, "message-added", 30, |m| m["text"] == "me too").await;
    // The new host's roster follows within its 250 ms debounce.
    eventually("Alice shown offline", 10, || async { (!online_members(&b.app, &server).await.contains(&a.user)).then_some(()) }).await;

    b.app.shutdown().await;
    c.app.shutdown().await;
}

/// (c) an attachment survives its uploader: B auto-fetches, A leaves, C joins later and
/// downloads it from B.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attachment_outlives_its_uploader() {
    let _g = serial().await;
    let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let files = tempfile::tempdir().unwrap();
    let a = node(dirs[0].path(), "Alice").await;
    let mut b = node(dirs[1].path(), "Bob").await;
    let server = server_id(&call(&a.app, "create_server", json!({ "name": "Files" })).await);
    let invite = call(&a.app, "create_invite", json!({ "serverId": server })).await;
    call(&b.app, "join_server", json!({ "invite": invite })).await;
    all_online(&[&a, &b], &server).await;
    let general = text_channel(&a.app, &server).await;

    let bytes = data(300_000, 7);
    let path: PathBuf = files.path().join("data.bin");
    std::fs::write(&path, &bytes).unwrap();
    let too_many: Vec<String> = (0..11).map(|_| path.to_string_lossy().into_owned()).collect();
    let err = call_err(&a.app, "send_message", json!({ "convId": server, "channelId": general, "text": "", "attachments": too_many })).await;
    assert_eq!(err, "You can attach up to 10 files to one message.");
    let missing = files.path().join("nope.bin").to_string_lossy().into_owned();
    let err = call_err(&a.app, "send_message", json!({ "convId": server, "channelId": general, "text": "x", "attachments": [missing] })).await;
    assert_eq!(err, "Couldn't read nope.bin. Check that the file still exists.");

    let sent = call(
        &a.app,
        "send_message",
        json!({ "convId": server, "channelId": general, "text": "a file", "attachments": [path.to_string_lossy()] }),
    )
    .await;
    let att = &sent["attachments"][0];
    assert_keys(att, &["hash", "name", "size", "mime"]);
    assert_eq!(att["name"], "data.bin");
    assert_eq!(att["size"], 300_000);
    assert_eq!(att["mime"], "application/octet-stream");
    let hash = att["hash"].as_str().unwrap().to_string();
    let mine = call(&a.app, "transfer", json!({ "hash": hash })).await;
    assert_keys(&mine, TRANSFER_KEYS);
    assert_eq!(mine["state"], "complete");
    assert_eq!(call(&a.app, "file_url", json!({ "hash": hash, "name": "data.bin" })).await, json!(format!("{FILE_BASE}{hash}")));

    let done = wait_for(&mut b.rx, "transfer-updated", 30, |t| t["hash"] == hash.as_str() && t["state"] == "complete").await;
    assert_keys(&done, TRANSFER_KEYS);
    assert_eq!(done["total"], 300_000);
    assert_eq!(call(&b.app, "transfer", json!({ "hash": hash })).await["state"], "complete");

    a.app.shutdown().await;
    wait_for(&mut b.rx, "server-updated", 40, |s| {
        s["id"] == server.as_str() && s["connection"]["hostId"] == b.user.as_str() && matches!(s["connection"]["state"].as_str(), Some("solo" | "hosting"))
    })
    .await;
    let invite2 = call(&b.app, "create_invite", json!({ "serverId": server })).await;

    let c = node(dirs[2].path(), "Cam").await;
    let idle = call(&c.app, "transfer", json!({ "hash": hash })).await;
    assert_eq!(idle["state"], "idle");
    call(&c.app, "join_server", json!({ "invite": invite2 })).await;
    let general_c = text_channel(&c.app, &server).await;
    assert_eq!(general_c, general);
    eventually("history at Cam", 20, || async {
        let page = call(&c.app, "messages", json!({ "convId": server, "channelId": general, "before": null, "limit": 10 })).await;
        page["messages"].as_array().unwrap().iter().any(|m| m["attachments"][0]["hash"] == hash.as_str()).then_some(())
    })
    .await;
    // Bob is the one online holder (Alice, the author, is gone).
    eventually("Bob counted as a peer", 20, || async {
        let t = call(&c.app, "transfer", json!({ "hash": hash })).await;
        (t["peers"] == 1).then_some(())
    })
    .await;
    let t = call(&c.app, "download", json!({ "hash": hash })).await;
    assert_keys(&t, TRANSFER_KEYS);
    eventually("download at Cam", 30, || async {
        let t = call(&c.app, "transfer", json!({ "hash": hash })).await;
        (t["state"] == "complete").then_some(())
    })
    .await;
    let (got, mime) = c.app.file_for_url(&hash).await.expect("complete blob is served");
    assert_eq!(got, bytes, "bytes equal");
    assert_eq!(mime, "application/octet-stream");
    assert!(c.app.file_for_url(&"00".repeat(32)).await.is_none());

    // open_file exports into the download folder (no program is launched in tests).
    let mut s = call(&c.app, "app_snapshot", json!({})).await["settings"].clone();
    let downloads = files.path().join("downloads");
    s["downloadDir"] = json!(downloads.to_string_lossy());
    call(&c.app, "settings_set", json!({ "settings": s })).await;
    call(&c.app, "open_file", json!({ "hash": hash, "name": "data.bin" })).await;
    let zone = PathBuf::from(format!("{}:Zone.Identifier", downloads.join("data.bin").display()));
    assert!(std::fs::read_to_string(&zone).unwrap().contains("ZoneId=3"), "the export carries the Mark of the Web");
    std::fs::remove_file(&zone).unwrap();
    call(&c.app, "open_file", json!({ "hash": hash, "name": "data.bin" })).await;
    assert_eq!(std::fs::read(downloads.join("data.bin")).unwrap(), bytes);
    assert!(!downloads.join("data (1).bin").exists(), "a second open reuses the export");
    assert!(std::fs::read_to_string(&zone).unwrap().contains("ZoneId=3"), "reusing an export marks it again");

    b.app.shutdown().await;
    c.app.shutdown().await;
}

/// Wait until `app` holds `hash`.
async fn until_complete(app: &App, hash: &str, what: &str) {
    eventually(what, 30, || async { (call(app, "transfer", json!({ "hash": hash })).await["state"] == "complete").then_some(()) }).await;
}

/// Send one file into `conv` from `app`; returns its hash.
async fn send_file(app: &App, conv: &str, channel: Option<&str>, path: &Path) -> String {
    let sent = call(app, "send_message", json!({ "convId": conv, "channelId": channel, "text": "", "attachments": [path.to_string_lossy()] })).await;
    sent["attachments"][0]["hash"].as_str().unwrap().to_string()
}

/// (c2) Blobs are served per conversation. A member of a server cannot fetch a file that
/// two other people exchanged in a DM, and a stranger who DMs me cannot fetch my server
/// files, even after posting a message that references the hash (hashes are not secret).
/// Each refusal has a positive control on the same connection. Also: an attachment that
/// declares more than 25 MB is never fetched, and peer file names are cleaned.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blobs_are_served_only_where_they_are_shared() {
    let _g = serial().await;
    let dirs: Vec<_> = (0..4).map(|_| tempfile::tempdir().unwrap()).collect();
    let files = tempfile::tempdir().unwrap();
    let mut a = node(dirs[0].path(), "Alice").await;
    let b = node(dirs[1].path(), "Bob").await;
    let c = node(dirs[2].path(), "Carol").await;
    let x = node(dirs[3].path(), "Xavier").await;
    // Carol fetches only on purpose, so the positive control below is a real download.
    let mut s = call(&c.app, "app_snapshot", json!({})).await["settings"].clone();
    s["autoFetchLimit"] = json!(0);
    call(&c.app, "settings_set", json!({ "settings": s })).await;

    let server = server_id(&call(&a.app, "create_server", json!({ "name": "Shared" })).await);
    let invite = call(&a.app, "create_invite", json!({ "serverId": server })).await;
    call(&b.app, "join_server", json!({ "invite": invite })).await;
    call(&c.app, "join_server", json!({ "invite": invite })).await;
    all_online(&[&a, &b, &c], &server).await;
    let general = text_channel(&a.app, &server).await;

    let server_file = files.path().join("server.bin");
    std::fs::write(&server_file, data(200_000, 21)).unwrap();
    let server_hash = send_file(&a.app, &server, Some(&general), &server_file).await;
    until_complete(&b.app, &server_hash, "Bob fetches the server file").await;

    // Alice and Bob exchange a file in a DM.
    a.app.test_add_addr(&b.user, &b.app.test_addrs()).unwrap();
    b.app.test_add_addr(&a.user, &a.app.test_addrs()).unwrap();
    let ab = call(&a.app, "open_dm", json!({ "userId": b.user })).await["convId"].as_str().unwrap().to_string();
    let dm_file = files.path().join("private.bin");
    std::fs::write(&dm_file, data(150_000, 22)).unwrap();
    let dm_hash = send_file(&a.app, &ab, None, &dm_file).await;
    until_complete(&b.app, &dm_hash, "Bob fetches the DM file").await;

    // Carol, a member of the server, asks Bob for the DM file directly: refused.
    let bob_addrs = b.app.test_addrs();
    assert!(!c.app.test_fetch_from(&dm_hash, &b.user, &bob_addrs, 15).await, "a server member fetched a DM-only file");
    // Referencing the hash in the server first does not open it up.
    c.app.test_post_attachment_ref(&server, Some(&general), &dm_hash, "private.bin", 150_000).await.unwrap();
    eventually("Bob indexed Carol's reference", 20, || async {
        let page = call(&b.app, "messages", json!({ "convId": server, "channelId": general, "before": null, "limit": 20 })).await;
        page["messages"].as_array().unwrap().iter().any(|m| m["attachments"][0]["hash"] == dm_hash.as_str()).then_some(())
    })
    .await;
    assert!(!c.app.test_fetch_from(&dm_hash, &b.user, &bob_addrs, 15).await, "a reference in the server shared a DM-only file");
    // Control: the server file, which Bob got from its author in the server, is served.
    assert!(c.app.test_fetch_from(&server_hash, &b.user, &bob_addrs, 20).await, "Carol could not fetch the server file from Bob");

    // A stranger DMs Alice, then asks her for the server file: refused.
    x.app.test_add_addr(&a.user, &a.app.test_addrs()).unwrap();
    a.app.test_add_addr(&x.user, &x.app.test_addrs()).unwrap();
    let a_code = call(&a.app, "app_snapshot", json!({})).await["me"]["userCode"].as_str().unwrap().to_string();
    let xa = call(&x.app, "add_contact", json!({ "code": a_code })).await["convId"].as_str().unwrap().to_string();
    call(&x.app, "send_message", json!({ "convId": xa, "channelId": null, "text": "hello", "attachments": [] })).await;
    wait_for(&mut a.rx, "message-added", 20, |m| m["text"] == "hello" && m["convId"] == xa.as_str()).await;
    let alice_addrs = a.app.test_addrs();
    assert!(!x.app.test_fetch_from(&server_hash, &a.user, &alice_addrs, 15).await, "a stranger fetched a server file");
    x.app.test_post_attachment_ref(&xa, None, &server_hash, "server.bin", 200_000).await.unwrap();
    eventually("Alice indexed the stranger's reference", 20, || async {
        let page = call(&a.app, "messages", json!({ "convId": xa, "channelId": null, "before": null, "limit": 20 })).await;
        page["messages"].as_array().unwrap().iter().any(|m| m["attachments"][0]["hash"] == server_hash.as_str()).then_some(())
    })
    .await;
    assert!(!x.app.test_fetch_from(&server_hash, &a.user, &alice_addrs, 15).await, "a DM reference shared a server file");
    // Control: a file Alice sends the stranger in their DM arrives.
    let for_x = files.path().join("for-x.bin");
    std::fs::write(&for_x, data(50_000, 23)).unwrap();
    let x_hash = send_file(&a.app, &xa, None, &for_x).await;
    until_complete(&x.app, &x_hash, "the stranger fetches what Alice sent him").await;

    // An attachment that declares more than 25 MB is not fetched, automatically or on demand.
    let big = "ab".repeat(32);
    b.app.test_post_attachment_ref(&server, Some(&general), &big, "photo\u{202E}gnp.exe", 30 * 1024 * 1024).await.unwrap();
    let shown = wait_for(&mut a.rx, "message-added", 20, |m| m["attachments"][0]["hash"] == big.as_str()).await;
    assert_eq!(shown["attachments"][0]["name"], "photognp.exe", "hidden format characters are removed from file names");
    assert_eq!(shown["attachments"][0]["mime"], "application/octet-stream", "the type follows the cleaned name");
    assert_eq!(call(&a.app, "transfer", json!({ "hash": big })).await["state"], "idle", "not auto-fetched");
    assert_eq!(call_err(&a.app, "download", json!({ "hash": big })).await, "This file is larger than 25 MB, so Tincan won't download it.");

    for n in [&a, &b, &c, &x] {
        n.app.shutdown().await;
    }
}

/// (c3) The hash index keeps every conversation that references a hash, not only the first
/// one seen. A stranger names a hash in a DM first and never serves it, so the download
/// parks; when the real sender posts the file in a server, the parked download asks there
/// and completes without a click.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_reference_wakes_a_parked_download() {
    let _g = serial().await;
    let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let files = tempfile::tempdir().unwrap();
    let a = node(dirs[0].path(), "Alice").await;
    let mut b = node(dirs[1].path(), "Bob").await;
    let x = node(dirs[2].path(), "Xavier").await;

    let server = server_id(&call(&a.app, "create_server", json!({ "name": "Shared" })).await);
    let invite = call(&a.app, "create_invite", json!({ "serverId": server })).await;
    call(&b.app, "join_server", json!({ "invite": invite })).await;
    all_online(&[&a, &b], &server).await;
    let general = text_channel(&a.app, &server).await;
    // Alice first posts the file in a server of her own (Bob is not in it), which gives the
    // test its hash.
    let notes = server_id(&call(&a.app, "create_server", json!({ "name": "Notes" })).await);
    let notes_channel = text_channel(&a.app, &notes).await;
    let path = files.path().join("report.bin");
    std::fs::write(&path, data(120_000, 31)).unwrap();
    let hash = send_file(&a.app, &notes, Some(&notes_channel), &path).await;

    // Xavier DMs Bob and names the hash first, without holding it.
    x.app.test_add_addr(&b.user, &b.app.test_addrs()).unwrap();
    b.app.test_add_addr(&x.user, &x.app.test_addrs()).unwrap();
    let b_code = call(&b.app, "app_snapshot", json!({})).await["me"]["userCode"].as_str().unwrap().to_string();
    let xb = call(&x.app, "add_contact", json!({ "code": b_code })).await["convId"].as_str().unwrap().to_string();
    call(&x.app, "send_message", json!({ "convId": xb, "channelId": null, "text": "hi", "attachments": [] })).await;
    wait_for(&mut b.rx, "message-added", 20, |m| m["text"] == "hi" && m["convId"] == xb.as_str()).await;
    x.app.test_post_attachment_ref(&xb, None, &hash, "report.bin", 120_000).await.unwrap();
    eventually("Bob's auto-fetch parks", 30, || async {
        (call(&b.app, "transfer", json!({ "hash": hash })).await["state"] == "unavailable").then_some(())
    })
    .await;

    // Alice posts the real file where Bob can see it. Nothing else changes (no roster
    // update), and nobody clicks Download.
    assert_eq!(send_file(&a.app, &server, Some(&general), &path).await, hash);
    eventually("Bob's parked download completes", 15, || async {
        (call(&b.app, "transfer", json!({ "hash": hash })).await["state"] == "complete").then_some(())
    })
    .await;

    for n in [&a, &b, &x] {
        n.app.shutdown().await;
    }
}

/// (c4) File events lost to a lagging receiver are recovered: a download that completed
/// unseen is still shared with its server (another member can fetch it from me), and the
/// UI hears that it is complete.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lost_file_events_are_recovered() {
    let _g = serial().await;
    let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let files = tempfile::tempdir().unwrap();
    let a = node(dirs[0].path(), "Alice").await;
    let mut b = node(dirs[1].path(), "Bob").await;
    let c = node(dirs[2].path(), "Carol").await;
    let mut s = call(&c.app, "app_snapshot", json!({})).await["settings"].clone();
    s["autoFetchLimit"] = json!(0);
    call(&c.app, "settings_set", json!({ "settings": s })).await;

    let server = server_id(&call(&a.app, "create_server", json!({ "name": "Shared" })).await);
    let invite = call(&a.app, "create_invite", json!({ "serverId": server })).await;
    call(&b.app, "join_server", json!({ "invite": invite })).await;
    call(&c.app, "join_server", json!({ "invite": invite })).await;
    all_online(&[&a, &b, &c], &server).await;
    let general = text_channel(&a.app, &server).await;

    b.app.test_pause_file_events(true);
    let path = files.path().join("lag.bin");
    std::fs::write(&path, data(180_000, 41)).unwrap();
    let hash = send_file(&a.app, &server, Some(&general), &path).await;
    // Bob's store completes the auto-fetch; his app sees none of its events.
    until_complete(&b.app, &hash, "Bob's store holds the file").await;
    let bob_addrs = b.app.test_addrs();
    assert!(!c.app.test_fetch_from(&hash, &b.user, &bob_addrs, 10).await, "shared although the completion was never seen");

    b.app.test_pause_file_events(false);
    wait_for(&mut b.rx, "transfer-updated", 10, |t| t["hash"] == hash.as_str() && t["state"] == "complete").await;
    assert!(c.app.test_fetch_from(&hash, &b.user, &bob_addrs, 20).await, "the recovered share lets Carol fetch it from Bob");

    for n in [&a, &b, &c] {
        n.app.shutdown().await;
    }
}

/// (d) DMs: add by user code, delivery both ways, and an offline recipient gets the
/// message when it comes back (pending -> delivered via `message-updated`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_messages_and_offline_delivery() {
    let _g = serial().await;
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = node(da.path(), "Alice").await;
    let mut b = node(db.path(), "Bob").await;
    a.app.test_add_addr(&b.user, &b.app.test_addrs()).unwrap();
    b.app.test_add_addr(&a.user, &a.app.test_addrs()).unwrap();

    let code = call(&b.app, "app_snapshot", json!({})).await["me"]["userCode"].as_str().unwrap().to_string();
    assert_eq!(call_err(&a.app, "add_contact", json!({ "code": "hello" })).await, "That isn't a Tincan ID. Paste the code that starts with tincan1u.");
    let dm = call(&a.app, "add_contact", json!({ "code": code })).await;
    assert_keys(&dm, DM_KEYS);
    assert_eq!(dm["userId"], b.user.as_str());
    let conv = dm["convId"].as_str().unwrap().to_string();
    assert!(is_hex64(&dm["convId"]));

    let first = call(&a.app, "send_message", json!({ "convId": conv, "channelId": null, "text": "hello Bob", "attachments": [] })).await;
    assert!(first["channelId"].is_null());
    let got = wait_for(&mut b.rx, "message-added", 20, |m| m["text"] == "hello Bob").await;
    assert_eq!(got["convId"], conv.as_str());
    assert!(got["channelId"].is_null());
    assert_eq!(got["authorId"], a.user.as_str());
    let dm_b = wait_for(&mut b.rx, "dm-updated", 10, |d| d["convId"] == conv.as_str()).await;
    assert_keys(&dm_b, DM_KEYS);
    assert_eq!(dm_b["userId"], a.user.as_str());

    call(&b.app, "typing", json!({ "convId": conv, "channelId": null })).await;
    let t = wait_for(&mut a.rx, "typing", 10, |_| true).await;
    assert!(t["channelId"].is_null());
    assert_eq!(t["convId"], conv.as_str());

    call(&b.app, "send_message", json!({ "convId": conv, "channelId": null, "text": "hi Alice", "attachments": [] })).await;
    wait_for(&mut a.rx, "message-added", 20, |m| m["text"] == "hi Alice").await;
    eventually("first DM delivered", 20, || async {
        let page = call(&a.app, "messages", json!({ "convId": conv, "channelId": null, "before": null, "limit": 10 })).await;
        let mine = page["messages"].as_array().unwrap().iter().find(|m| m["text"] == "hello Bob").cloned();
        mine.filter(|m| m["pending"] == false)
    })
    .await;
    // APP-CORE-10: my own message moves the DM's last message time (the DM list order),
    // with the link already up and no server roster to refresh it. Earlier refreshes settle
    // first, so the event below can only come from this send.
    let settled = Instant::now() + Duration::from_millis(1500);
    while tokio::time::timeout(settled.saturating_duration_since(Instant::now()), a.rx.recv()).await.is_ok() {}
    let again = call(&a.app, "send_message", json!({ "convId": conv, "channelId": null, "text": "one more", "attachments": [] })).await;
    wait_for(&mut a.rx, "dm-updated", 5, |d| d["convId"] == conv.as_str() && d["lastMessageAt"] == again["sentAt"]).await;

    // Q5: a contact who shares no server shows the name they announced over the DM, in the
    // DM list and as the author of their messages.
    let dm_name = |app: Arc<App>| async move { call(&app, "app_snapshot", json!({})).await["dms"][0]["name"].as_str().map(str::to_string) };
    eventually("Bob's DM name", 10, || async { (dm_name(a.app.clone()).await.as_deref() == Some("Bob")).then_some(()) }).await;
    let page = call(&a.app, "messages", json!({ "convId": conv, "channelId": null, "before": null, "limit": 10 })).await;
    let from_bob: Vec<&Value> = page["messages"].as_array().unwrap().iter().filter(|m| m["authorId"] == b.user.as_str()).collect();
    assert!(!from_bob.is_empty() && from_bob.iter().all(|m| m["authorName"] == "Bob"), "{page}");
    // A rename reaches the contact, cleaned of bidi controls (names may contain them).
    call(&b.app, "set_name", json!({ "name": "Bob\u{202E}by" })).await;
    eventually("Bob's new DM name", 10, || async { (dm_name(a.app.clone()).await.as_deref() == Some("Bobby")).then_some(()) }).await;
    call(&b.app, "set_name", json!({ "name": "Bob" })).await;
    eventually("Bob's name back", 10, || async { (dm_name(a.app.clone()).await.as_deref() == Some("Bob")).then_some(()) }).await;

    // Bob goes offline; Alice's message waits in her outbox.
    b.app.shutdown().await;
    let b_user = b.user.clone();
    drop(b);
    let waiting = call(&a.app, "send_message", json!({ "convId": conv, "channelId": null, "text": "while you were away", "attachments": [] })).await;
    assert_eq!(waiting["pending"], true);

    // Bob comes back on the same profile, on a new port. Alice's outbox dialed his old
    // address when she sent; tincan-net ignores new addresses while that dial is in flight
    // (dm.rs ensure_connect), so tell her where he is once it has timed out (4 s).
    let b2 = start(db.path()).await;
    let mut rx_b2 = b2.subscribe();
    tokio::time::sleep(Duration::from_secs(5)).await;
    a.app.test_add_addr(&b_user, &b2.test_addrs()).unwrap();
    wait_for(&mut rx_b2, "message-added", 30, |m| m["text"] == "while you were away").await;
    let delivered = wait_for(&mut a.rx, "message-updated", 30, |m| m["id"] == waiting["id"]).await;
    assert_eq!(delivered["pending"], false);
    let (texts, _) = all_texts(&b2, &conv, None, 2).await;
    assert_eq!(texts, ["hello Bob", "hi Alice", "one more", "while you were away"]);
    let snap = call(&b2, "app_snapshot", json!({})).await;
    assert_eq!(snap["me"]["name"], "Bob", "the name survives a restart");
    assert_eq!(snap["dms"][0]["userId"], a.user.as_str());

    a.app.shutdown().await;
    b2.shutdown().await;
}

/// SPEC-GAPS-2: a contact who shares no server shows online again while their DM is on
/// screen (`dm_watch`), after they restarted on a new address with nothing pending.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_watched_dm_contact_shows_online() {
    let _g = serial().await;
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = node(da.path(), "Alice").await;
    let b = node(db.path(), "Bob").await;
    a.app.test_add_addr(&b.user, &b.app.test_addrs()).unwrap();
    b.app.test_add_addr(&a.user, &a.app.test_addrs()).unwrap();
    let conv = call(&a.app, "add_contact", json!({ "code": b.user })).await["convId"].as_str().unwrap().to_string();
    call(&a.app, "send_message", json!({ "convId": conv, "channelId": null, "text": "hi", "attachments": [] })).await;
    let online = |app: Arc<App>| async move { call(&app, "app_snapshot", json!({})).await["dms"][0]["online"].as_bool() };
    eventually("Bob online", 20, || async { (online(a.app.clone()).await == Some(true)).then_some(()) }).await;
    eventually("Alice's message delivered", 20, || async {
        let page = call(&a.app, "messages", json!({ "convId": conv, "channelId": null, "before": null, "limit": 10 })).await;
        page["messages"].as_array().unwrap().iter().all(|m| m["pending"] == false).then_some(())
    })
    .await;

    // Bob restarts on a new port. Nothing is pending, so nobody dials.
    let b_user = b.user.clone();
    b.app.shutdown().await;
    drop(b);
    eventually("Bob offline", 20, || async { (online(a.app.clone()).await == Some(false)).then_some(()) }).await;
    let b2 = start(db.path()).await;
    a.app.test_add_addr(&b_user, &b2.test_addrs()).unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(online(a.app.clone()).await, Some(false), "nothing dialed Bob yet");

    // Alice opens the DM: the app keeps the connection up, and Bob shows online.
    call(&a.app, "dm_watch", json!({ "convId": conv })).await;
    eventually("Bob online while watched", 20, || async { (online(a.app.clone()).await == Some(true)).then_some(()) }).await;
    call(&a.app, "dm_watch", json!({ "convId": null })).await;
    assert_eq!(call_err(&a.app, "dm_watch", json!({ "convId": "00" })).await, "That conversation id is not valid.");

    a.app.shutdown().await;
    b2.shutdown().await;
}

/// (e) owner changes and deletions propagate.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn owner_changes_and_deletes_propagate() {
    let _g = serial().await;
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = node(da.path(), "Alice").await;
    let mut b = node(db.path(), "Bob").await;
    let server = server_id(&call(&a.app, "create_server", json!({ "name": "Old name" })).await);
    let invite = call(&a.app, "create_invite", json!({ "serverId": server })).await;
    call(&b.app, "join_server", json!({ "invite": invite })).await;
    all_online(&[&a, &b], &server).await;
    settle_after_join().await;

    // Renaming myself publishes a Profile in every server. APP-CORE-08: a second rename
    // while the first one's publish is in flight reaches the server too.
    call(&b.app, "set_name", json!({ "name": "Bob the first" })).await;
    call(&b.app, "set_name", json!({ "name": "Bobby" })).await;
    wait_for(&mut a.rx, "members-updated", 20, |m| {
        m["serverId"] == server.as_str() && m["members"].as_array().unwrap().iter().any(|x| x["userId"] == b.user.as_str() && x["name"] == "Bobby")
    })
    .await;

    call(&a.app, "rename_server", json!({ "serverId": server, "name": "New name" })).await;
    wait_for(&mut b.rx, "server-updated", 20, |s| s["id"] == server.as_str() && s["name"] == "New name").await;
    assert_eq!(call_err(&b.app, "rename_server", json!({ "serverId": server, "name": "Mine" })).await, "Only the server owner can do that.");

    let ch = call(&a.app, "create_channel", json!({ "serverId": server, "name": "random", "kind": "text" })).await;
    assert_keys(&ch, CHANNEL_KEYS);
    let ch_id = ch["id"].as_str().unwrap().to_string();
    let has = |c: &Value, name: &str| c["channels"].as_array().unwrap().iter().any(|x| x["id"] == ch_id.as_str() && x["name"] == name);
    wait_for(&mut b.rx, "channels-updated", 20, |c| has(c, "random")).await;
    call(&a.app, "rename_channel", json!({ "serverId": server, "channelId": ch_id, "name": "chat" })).await;
    wait_for(&mut b.rx, "channels-updated", 20, |c| has(c, "chat")).await;
    call(&a.app, "delete_channel", json!({ "serverId": server, "channelId": ch_id })).await;
    wait_for(&mut b.rx, "channels-updated", 20, |c| {
        c["serverId"] == server.as_str() && !c["channels"].as_array().unwrap().iter().any(|x| x["id"] == ch_id.as_str())
    })
    .await;
    let voice = call(&a.app, "create_channel", json!({ "serverId": server, "name": "Hangout", "kind": "voice" })).await;
    assert_eq!(voice["kind"], "voice");

    let general = text_channel(&a.app, &server).await;
    let from_a = call(&a.app, "send_message", json!({ "convId": server, "channelId": general, "text": "keep", "attachments": [] })).await;
    let oops = call(&b.app, "send_message", json!({ "convId": server, "channelId": general, "text": "oops", "attachments": [] })).await;
    wait_for(&mut a.rx, "message-added", 20, |m| m["id"] == oops["id"]).await;
    wait_for(&mut b.rx, "message-added", 20, |m| m["id"] == from_a["id"]).await;
    let err = call_err(&b.app, "delete_message", json!({ "convId": server, "messageId": from_a["id"] })).await;
    assert_eq!(err, "You can only delete your own messages.");
    call(&b.app, "delete_message", json!({ "convId": server, "messageId": oops["id"] })).await;
    let upd = wait_for(&mut a.rx, "message-updated", 20, |m| m["id"] == oops["id"]).await;
    assert_eq!(upd["deleted"], true);
    assert_eq!(upd["text"], "");
    let page = call(&a.app, "messages", json!({ "convId": server, "channelId": general, "before": null, "limit": 10 })).await;
    let shown = page["messages"].as_array().unwrap().iter().find(|m| m["id"] == oops["id"]).unwrap().clone();
    assert_eq!(shown["deleted"], true);

    // Leaving removes the server from the snapshot.
    call(&b.app, "leave_server", json!({ "serverId": server })).await;
    let snap = wait_for(&mut b.rx, "snapshot", 20, |s| s["servers"].as_array().unwrap().is_empty()).await;
    assert!(snap["servers"].as_array().unwrap().is_empty());

    a.app.shutdown().await;
    b.app.shutdown().await;
}

/// The profile lock: a second start on the same directory fails until shutdown.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn profile_is_locked_while_running() {
    let _g = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let app = start(dir.path()).await;
    let cfg = AppConfig {
        data_dir: dir.path().to_path_buf(),
        app_version: "t".into(),
        file_url_base: FILE_BASE.into(),
        test: TestOptions { offline_network: true, no_devices: true },
    };
    assert_eq!(App::start(cfg.clone()).await.err().as_deref(), Some("Tincan is already running with this profile."));
    let snap = call(&app, "app_snapshot", json!({})).await;
    assert!(snap["me"].is_null(), "no name yet");
    assert_eq!(call_err(&app, "create_server", json!({ "name": "x" })).await, "Pick a display name first.");
    app.shutdown().await;
    app.shutdown().await; // idempotent
    assert_eq!(call_err(&app, "app_snapshot", json!({})).await, "Tincan is shutting down.");
    let again = App::start(cfg).await.expect("restart after shutdown");
    again.shutdown().await;
}

/// History sync is coalesced: joining a server with a long history emits no per-message
/// events, then one refresh with a `snapshot`; a catch-up after a restart stays bounded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn history_sync_is_coalesced() {
    let _g = serial().await;
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = node(da.path(), "Alice").await;
    let mut b = node(db.path(), "Bob").await;
    let server = server_id(&call(&a.app, "create_server", json!({ "name": "Busy" })).await);
    let general = text_channel(&a.app, &server).await;
    for i in 0..300 {
        call(&a.app, "send_message", json!({ "convId": server, "channelId": general, "text": format!("h{i}"), "attachments": [] })).await;
    }
    let invite = call(&a.app, "create_invite", json!({ "serverId": server })).await;
    call(&b.app, "join_server", json!({ "invite": invite })).await;
    // After the sync: one refresh of the server (channels with the unread dot, members,
    // the summary), then a snapshot, and no per-message events at all.
    let (mut chans, mut members, mut summary) = (false, false, false);
    let (mut added, mut total) = (0, 0);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let ev = tokio::time::timeout(left, b.rx.recv()).await.expect("refresh after the join").expect("event");
        total += 1;
        let p = &ev.payload;
        let here = p["serverId"] == server.as_str() || p["id"] == server.as_str();
        match ev.name {
            "message-added" => added += 1,
            "channels-updated" if here => {
                chans |= p["channels"].as_array().unwrap().iter().any(|c| c["id"] == general.as_str() && c["unread"] == true)
            }
            "members-updated" if here => members |= p["members"].as_array().unwrap().len() == 2,
            "server-updated" if here => summary |= p["unread"] == true,
            "snapshot" if chans && members && summary => break,
            _ => {}
        }
    }
    eprintln!("join of a 300-message history: {added} message-added, {total} events in all");
    assert_eq!(added, 0, "no per-message events for the join's history");
    assert!(total < 40, "{total} events for a 300-message sync");
    assert_eq!(message_count(&b.app, &server, &general).await, Some(300));
    let chans = call(&b.app, "channels", json!({ "serverId": server })).await;
    assert!(chans.as_array().unwrap().iter().any(|c| c["id"] == general.as_str() && c["unread"] == true));

    // Bob is offline while 100 more messages arrive, then catches up on restart.
    b.app.shutdown().await;
    drop(b);
    for i in 0..100 {
        call(&a.app, "send_message", json!({ "convId": server, "channelId": general, "text": format!("c{i}"), "attachments": [] })).await;
    }
    let b2 = start(db.path()).await;
    let mut rx = b2.subscribe();
    let (added, total) = until_synced(&mut rx, &b2, &server, &general, 400).await;
    eprintln!("catch-up of 100 messages: {added} message-added, {total} events in all");
    assert!(added <= 40, "{added} message events for a 100-message catch-up");
    assert!(total < 80, "{total} events for a 100-message catch-up");

    a.app.shutdown().await;
    b2.shutdown().await;
}

// ---------------------------------------------------------------------------------- contract

fn interface_block<'a>(src: &'a str, name: &str) -> &'a str {
    let start = src.find(&format!("export interface {name} {{")).unwrap_or_else(|| panic!("interface {name} not found"));
    let body = &src[start..];
    let end = body.find("\n}").expect("end of interface");
    &body[..end]
}

/// Keys of a TS interface: `name:` or `"name":` at the start of a line.
fn ts_keys(block: &str) -> Vec<String> {
    block
        .lines()
        .skip(1)
        .filter_map(|l| {
            let l = l.trim();
            if l.starts_with("//") || l.starts_with("/*") || l.starts_with('*') {
                return None;
            }
            let key = l.split(':').next()?.trim().trim_matches('"');
            (!key.is_empty() && key.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_uppercase() || c == '_' || c == '-')).then(|| key.to_string())
        })
        .collect()
}

/// (f) every `types.ts` command except the shell's is dispatched, and every event the app
/// emits is an `Events` key.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn contract_matches_types_ts() {
    let _g = serial().await;
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ui/src/api/types.ts");
    let src = std::fs::read_to_string(&path).expect("read types.ts");
    let commands = ts_keys(interface_block(&src, "Commands"));
    let events = ts_keys(interface_block(&src, "Events"));
    assert!(commands.len() > 40 && events.len() > 10, "parsed {commands:?} {events:?}");
    let expected: Vec<&String> = commands.iter().filter(|c| *c != "screen_watch" && !c.starts_with("window_")).collect();

    let handled = App::handled_commands();
    for c in &expected {
        assert!(handled.contains(&c.as_str()), "{c} is not handled");
    }
    for h in &handled {
        assert!(commands.iter().any(|c| c == h), "{h} is not a types.ts command");
    }
    for e in App::event_names() {
        assert!(events.iter().any(|x| x == e), "{e} is not a types.ts event");
    }
    for e in &events {
        assert!(App::event_names().contains(&e.as_str()), "the app never emits {e}");
    }

    let dir = tempfile::tempdir().unwrap();
    let app = start(dir.path()).await;
    for c in &expected {
        let r = tokio::time::timeout(Duration::from_secs(10), app.call(c, json!({}))).await.unwrap_or_else(|_| panic!("{c} hung"));
        if let Err(e) = r {
            assert!(!e.starts_with("Unknown command"), "{c}: {e}");
        }
    }
    for c in ["screen_watch", "window_close", "nope"] {
        assert!(app.call(c, json!({})).await.unwrap_err().starts_with("Unknown command"));
    }
    // Media results without a session are well formed (no devices in tests).
    assert_eq!(call(&app, "clips", json!({})).await, json!([]));
    assert_eq!(call(&app, "audio_devices", json!({})).await, json!({ "inputs": [], "outputs": [] }));
    assert_eq!(call_err(&app, "voice_join", json!({ "serverId": "00", "channelId": "00" })).await, "That conversation id is not valid.");
    assert_eq!(call_err(&app, "clip_play", json!({ "hash": "ab".repeat(32) })).await, "not-in-voice");
    assert_eq!(call_err(&app, "screen_start", json!({ "sourceId": "test:pattern", "mode": "text" })).await, "not-in-voice");
    let sources = call(&app, "screen_sources", json!({})).await;
    assert_keys(&sources[0], &["id", "kind", "name", "width", "height"]);
    assert!(app.screen_watch(5, Box::new(|_| true)).await.is_err(), "no watching outside voice");

    // Settings keys match the TS interface and survive a round trip.
    let settings_keys = ts_keys(interface_block(&src, "Settings"));
    let snap = call(&app, "app_snapshot", json!({})).await;
    let mut want: Vec<&str> = settings_keys.iter().map(String::as_str).collect();
    want.sort();
    assert_keys(&snap["settings"], &want);
    let mut s = snap["settings"].clone();
    s["theme"] = json!("dark");
    s["allowHosting"] = json!(false);
    s["downloadDir"] = json!(dir.path().join("dl").to_string_lossy());
    let saved = call(&app, "settings_set", json!({ "settings": s })).await;
    assert_eq!(saved["theme"], "dark");
    let bad = json!({ "settings": { "customRelayUrl": "not a url" } });
    assert!(app.call("settings_set", bad).await.unwrap_err().contains("relay URL"));
    app.shutdown().await;
    let again = start(dir.path()).await;
    let snap = call(&again, "app_snapshot", json!({})).await;
    assert_eq!(snap["settings"]["theme"], "dark");
    assert_eq!(snap["settings"]["allowHosting"], false);
    again.shutdown().await;
}

/// (g) short codes: the owner's 6-character invite code (the same for every member, only
/// the owner rotates it, the old one stops admitting at once) and friend codes in
/// `add_contact`. The profiles share a parent folder, so they share `.offline-codes`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn short_invite_and_friend_codes() {
    let _g = serial().await;
    let root = tempfile::tempdir().unwrap();
    let dir = |n: &str| {
        let d = root.path().join(n);
        std::fs::create_dir_all(&d).unwrap();
        d
    };
    let a = node(&dir("a"), "Alice").await;
    let b = node(&dir("b"), "Bob").await;
    let c = node(&dir("c"), "Carol").await;
    let is_code = |v: &Value| v.as_str().is_some_and(|s| tincan_core::code::ShortCode::parse(s).is_some_and(|c| c.to_string() == s));

    let server = server_id(&call(&a.app, "create_server", json!({ "name": "Codes" })).await);
    let code = call(&a.app, "get_invite_code", json!({ "serverId": server })).await;
    assert!(is_code(&code), "{code}");
    assert_eq!(call(&a.app, "get_invite_code", json!({ "serverId": server })).await, code);
    let typed = code.as_str().unwrap().replace('-', " ").to_lowercase();
    assert_eq!(server_id(&call(&b.app, "join_server", json!({ "invite": typed })).await), server);
    all_online(&[&a, &b], &server).await;
    let seen = eventually("bob sees the code", 20, || async {
        let v = b.app.call("get_invite_code", json!({ "serverId": server })).await.ok()?;
        (!v.is_null()).then_some(v)
    })
    .await;
    assert_eq!(seen, code);
    assert!(call_err(&b.app, "rotate_invite_code", json!({ "serverId": server })).await.contains("Only the server owner"));

    let rotated = call(&a.app, "rotate_invite_code", json!({ "serverId": server })).await;
    assert!(is_code(&rotated) && rotated != code);
    let err = call_err(&c.app, "join_server", json!({ "invite": code })).await;
    assert!(!err.is_empty());
    assert_eq!(server_id(&call(&c.app, "join_server", json!({ "invite": rotated })).await), server);

    // Friend codes: made on first use, stable, rotatable; add_contact resolves them.
    let bob_code = call(&b.app, "get_friend_code", json!({})).await;
    assert!(is_code(&bob_code));
    assert_eq!(call(&b.app, "get_friend_code", json!({})).await, bob_code);
    let dm = eventually("alice adds bob by friend code", 20, || async {
        a.app.call("add_contact", json!({ "code": bob_code.as_str().unwrap().to_lowercase() })).await.ok()
    })
    .await;
    assert_eq!(dm["userId"], b.user.as_str());
    assert!(call_err(&b.app, "add_contact", json!({ "code": bob_code })).await.contains("your own code"));
    let new_code = call(&b.app, "rotate_friend_code", json!({})).await;
    assert!(is_code(&new_code) && new_code != bob_code);
    assert_eq!(call(&b.app, "get_friend_code", json!({})).await, new_code);
    let dm = eventually("carol adds bob by the new code", 20, || async { c.app.call("add_contact", json!({ "code": new_code })).await.ok() }).await;
    assert_eq!(dm["userId"], b.user.as_str());
    eventually("the old friend code stops resolving", 20, || async { c.app.call("add_contact", json!({ "code": bob_code })).await.err() }).await;
    // The long ID still works.
    let long = call(&a.app, "app_snapshot", json!({})).await["me"]["userCode"].clone();
    assert_eq!(call(&c.app, "add_contact", json!({ "code": long })).await["userId"], a.user.as_str());
    assert!(call_err(&c.app, "add_contact", json!({ "code": "hello" })).await.contains("friend code"));

    for n in [&a, &b, &c] {
        n.app.shutdown().await;
    }
}
