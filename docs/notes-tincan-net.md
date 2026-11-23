# tincan-net: integration notes

Crate: `crates/tincan-net`. Rustdoc: `cargo doc -p tincan-net --no-deps`. This page covers what the app integrator needs beyond the doc comments. Part A covers sessions, election, sync and DMs; part B (the sections at the end) covers rendezvous, voice and screen media with the host SFU, and 1:1 calls.

## Wiring (app)

1. `let endpoint = tincan_net::build_endpoint(&identity, &cfg).await?;`
   - `presets::N0` for `RelayChoice::N0Default` and `Custom(urls)`; `presets::Minimal` plus `RelayMode::Disabled` for `Disabled`.
   - The media-tuned transport config (idle 10 s, keep-alive 2 s, datagram send buffer 16 KiB, receive buffer 256 KiB) applies endpoint-wide, to blobs connections too. The Router hands handlers an `Accepting`, so a per-connection `accept_with` is not possible.
   - mDNS (`"tincan1"`, cadence 5 s) when `cfg.mdns`. `Net::start` subscribes to it and dials LAN peers that are members of my servers.
2. `Files::load(.., endpoint.clone(), ..)`.
3. `let net = Net::start(endpoint, storage, identity, cfg, |b| b.accept(tincan_files::ALPN, files.protocol())).await?;`
4. Forward `net.subscribe()` events to the UI. On `RecvError::Lagged`, refresh from `conv_state` or storage. During a large first sync, thousands of `EventAdded` can arrive within a second.
5. On exit: `net.shutdown().await`. It hands off every hosted server (up to 2 s for the ack plus 1 s flush), closes DMs, then `router.shutdown()`. That closes the endpoint and runs `Files::shutdown`.

`NetConfig.display_name` is new compared to the task sketch. It is needed for the owner `Profile` of `create_server` and the `Profile` inside a `JoinRequest`. Change it with `set_display_name`. Renaming inside a server is a normal `publish(server, Body::Profile { name })`.

## Behaviour the UI should know

- `connection(server)`: `Connecting` while looking, `Member`, `Hosting` (members connected), `Solo`, `Offline`. `Offline` means "allow hosting" is off and nobody was reachable; it retries every 60 s. `ConnectionChanged` fires on state, host, path or epoch changes, and on RTT moves of at least 25 % and 10 ms.
- `publish(server, body)`:
  - As a member, it resolves after the host's `PublishAck`. The event is applied locally at that moment, and `EventAdded` fires.
  - As host, it applies at once.
  - While connecting, the body waits in a queue for up to 15 s, then fails with `NotConnected`.
  - A signed event whose ack is late returns `Timeout`. It stays queued and is re-sent after the next `Welcome`, so it may still appear later. Its seq is never reused.
- `publish(dm, body)`: stored and `EventAdded` emitted at once. Only `Message` and `Delete`; the channel is forced to `dm_channel(conv)`.
  - `dm_delivered(conv)` is the highest confirmed seq; messages with a higher seq are pending.
  - `dm_pending(conv)` is the highest unconfirmed seq (0 when all are delivered). `DmDelivered` fires when the ack advances.
- `typing(conv, channel)` works for servers and DMs (the task sketch had `server`).
- `who_has(conv, hash)`:
  - Servers: holders from the host's aggregation (`Have` announcements plus clip libraries; the host includes itself).
  - DMs: always the contact.
  - The result never includes me.
- `leave(server)` publishes `MemberLeave` (best effort, 5 s), hands off if hosting, deletes the conv, its events and its peers, and emits `Left`. The own-seq floor in kv (`net/own_seq/<conv>`) is kept on purpose, so a re-join continues the seq chain.
- An incoming DM from a stranger (someone who pasted my user code) is accepted and creates the DM conversation (`DmOpened`).
- `join(invite)` errors are user sentences:
  - "This invite has expired."
  - "This invite is not valid for this server."
  - "Couldn't reach anyone in that server. Check the invite or try again later."
  - Joining a server I'm already in returns its id.

## Protocol decisions (core types unchanged)

- **Clean handoff:** the leaving host sends `Handoff{its claim, roster}` to S1. S1 claims `epoch+1` (prev = old host), becomes host with the inherited short ids, and replies `HostMoved{its new claim, its addr}` followed by `HandoffAck{epoch}`. The old host relays that `HostMoved` to every other member, waits until they close (at most 1 s), and closes with `CLOSE_HOST_LEAVING`. A member that only sees the close code fails over by succession.
- **Failover:**
  - Successor rank i (0-based) promotes after `i * 3 s`, so S1 promotes at once. It waits 3 s longer while a higher-ranked successor answers without hosting yet, including a `Redirect` to the dead host from a successor that has not noticed the loss.
  - Members dial every higher-ranked successor at once and re-dial each every second.
  - A non-successor falls back to cold open after `len * 3 s + 4 s`.
  - Any close of the host link other than through `HostMoved`, or 10 s of silence (ticks every 2 s), counts as losing the host.
- **Cold-open race:** a member that is still seeking and receives a `Hello` from a member with a higher user id starts hosting and accepts it. The other side answers `NotHost` and dials back. Two members starting together therefore meet at once instead of both going solo after 6 s.
  - Exception: while its own dial to the last known host is still in flight, the lower id answers `NotHost` and re-dials the other member after one dial timeout. This avoids splitting a healthy server.
  - This promotion before the 6 s solo rule is an addition to architecture.md.
- **Partition merge:**
  - A host probes cached offline peers: every 60 s while solo (5 min after 30 min alone), every 5 min with members, ±20 % jitter. It also probes on any learned address or mDNS sighting.
  - A `Welcome` from another host, or a `Redirect` to one, starts the compare. The prober (already a member at the other host) sends `HostCompare`, the other replies with its own, and both run `election::merge_winner` on the two values as sent (`connected` excludes the peer).
  - The winner claims `max(epochs) + 1` and sends `HostMoved` to the loser and, as a claim refresh, to its own members. A member receiving `HostMoved` naming its current host only updates the claim.
  - The loser relays that `HostMoved` to its members, closes them with `CLOSE_REDIRECT`, and joins the winner over a fresh connection (normal `Hello`, `Welcome` and two-way `SyncHave`).
  - When two hosts probe each other at once, only the probe dialed by the lower user id goes on.
- **Sync:**
  - `send_missing` streams authors in `sync_author_order`, ascending seq, in pages of at most 256 KiB, and always ends with an empty `Events{more: false}`. The receiver uses that marker as "initial sync complete": it sends `Status{synced: true}`, which makes it eligible for succession. Live fan-out never sends an empty `Events`.
  - The host replies to a member's `SyncHave` with its own (if it lacks something) at most every 30 s, and not right after `Welcome` (it already sent one). This bounds resync ping-pong when an event is permanently rejected.
- **Publish acks:**
  - The host answers `ok: true` for an event it already stores with the same id.
  - It answers `ok: false, reason: "gap"` without buffering when a prerequisite is missing; the member retries the same signed event up to 5 times, every 500 ms.
  - Any other `ok: false` releases the seq.
  - Members publish one event at a time, and only after their own sync to the host has been sent, so the host never sees gaps from an honest author.
- **Own seqs:** a persisted floor (`net/own_seq/<conv>`) means no seq is ever signed twice, even after a crash or when the host acknowledged but local storage failed.
- **Roster addresses:** a member's address is the remote addresses of the host's paths to it; the host's own entry is `endpoint.addr()`. Members feed roster addresses into the iroh `MemoryLookup`, which blobs downloads use too, and into storage peers (for cold open). `Hello` carries no address.
- **DM ack:** after applying at least one event, the receiver replies `SyncHave{heads}`. That is the delivery confirmation and is persisted in `net/dm_ack/<conv>`. There is no reply to an empty `Events`, and none when nothing was applied, so permanent rejects cannot ping-pong. Simultaneous dials keep the connection dialed by the lower user id. An idle DM connection closes after 120 s unless it is held (`dm_hold`) or has undelivered events.

## Part B: wiring media in the app

```
let mut s = net.voice_join(server, channel).await?;          // or call_start / call_accept
engine.start_call(s.local_track, s.epoch, s.audio_out.clone())?;
// task: while let Some(p) = s.audio_in.recv().await { engine.push_packet(p) }
// task: while let Some((track, f)) = s.video_in.recv().await { channel.send(ui_video_message(track, &f)) }
let (track, frames) = net.share_start()?;                     // ScreenShare::start(source, mode, frames)
net.subscribe_track(t) / net.unsubscribe_track(t)              // screen_watch / screen_unwatch
net.sound_played(clip).await?                                  // after AudioEngine::play_clip
net.voice_leave().await                                        // also hangs up an active call
```

Events to wire:

| NetEvent | App action |
|---|---|
| `VoiceUpdated { conv }` | re-read `voice_info()` (connected, relayed -> `engine.set_relayed`, epoch -> `engine.set_epoch`, sharing, watching); emit `voice-updated` |
| `KeyframeRequested { track }` | `ScreenShare::request_keyframe()` |
| `TargetBitrate { track, bps }` | `ScreenShare::set_bitrate(bps)` |
| `LossReport { pct }` | `engine.set_loss_hint(pct)` |
| `SoundPlayed { conv, by, clip }` | "Heard in this call" (`sound-played`); fetch from `by` / `who_has` |
| `CallInvite { call }` | ring UI (it also comes as `CallUpdated`) |
| `CallUpdated { call }` | `call-updated`; on `Active` start the engine with the call's session; on `Ended` stop it |

- The channels of a `MediaSession` stay the same across host migrations. Outgoing audio is re-stamped with the current track id and epoch byte, so `set_epoch` is cosmetic and a changed short id needs no engine restart.
- `voice_join` sets `Status.voice` (reported only to the server that owns the channel); `share_start` sets `sharing`. Mute and deafen stay with `set_status`. Calls are not rooms: `Status.voice` is empty while in a call.
- `share_start` and `sound_played` outside a session fail with `NetError::NotInVoice`, which displays as `not-in-voice` (api.md).
- `call_start` returns the ringing `CallInfo` and the call's session at once; media flows once accepted. `call_accept` leaves the voice room. Joining a room hangs up an active or outgoing call but lets an incoming one ring. Track ids in calls: the lower user id has short id 0.
- `set_extra_ctl_handler` is now a tap: the media layer handles every media and call `Ctl` first.

## Part B: media design decisions

- **Transport:** everything rides the existing connections (`tincan/server/1` member <-> host, `tincan/dm/1` for calls), so audio and video share one congestion controller and datagrams go out before stream data. Media control frames go straight onto the control link's live queue: same order as session frames, never blocking.
- **Roles:** the session actors call the media hub through drop guards in `HostState` (SFU plus rendezvous publisher) and `MemberState` (media over that host connection). Every way out of a role (handoff, merge, crash failover, leave, shutdown) tears media down. Tokens and `Connection::stable_id` stop a late drop from removing its successor's registration.
- **Liveness:** media control frames (`hooks::is_extra`) no longer refresh `last_rx` on either side. Media tasks keep running when a session actor hangs; before this change a hung host's SFU kept its members from ever failing over (found by `voice_resumes_after_host_crash`).
- **Audio forwarding:** one reader task per member link (`read_many_datagrams`, batch 32). A datagram whose track id is not the sender's own voice track is dropped. Everything else goes verbatim to the other members of the sender's room, and to the host's `audio_in` when the host is in that room. The host's own packets are injected the same way. A member who is in no room is not forwarded.
- **Video forwarder** (`media/track.rs`; the same code runs on every hop: the sharer's upload with the host as only viewer, the host SFU, a call peer):
  - Groups start at keyframes. Each viewer has a writer task, one uni stream per group and a bounded queue (160 frames).
  - Drop policy: a queue over 2 frames and over 250 ms of the measured input rate, a full queue, or a frame older than 300 ms resets the viewer's stream. The viewer then skips to the next keyframe, and a keyframe is requested upstream (coalesced to 1/s per track).
  - Priorities: the newest group's stream gets 50 and the replaced one is demoted to 0 before `finish`. A raw `set_priority(group_seq)` would overtake the control stream (100) after 100 groups.
  - GOP cache: the current group from its keyframe, up to 4 s / 2 MiB. A new viewer is seeded from it at once (measured on loopback: first frame 0.3-0.5 ms after `subscribe_track`), with a keyframe request when the cached group is older than 1.5 s. A viewer's `KeyframeRequest` re-seeds it from a fresh cache, otherwise it goes upstream.
- **Receiving video:** a reader task per stream feeds one ordering task per connection. Per track it delivers one group at a time in `group_seq` order. A newer group waits until the current group's stream ends (or 200 ms / 60 frames); older groups are dropped; after a reset, deltas wait for a keyframe. `VideoOut` in front of `video_in` remembers the last timestamp per track. On a transport change it is armed: frames up to that timestamp (within 10 s) are dropped until the first newer one, so a cached group that a new host replays after a migration does not rewind the picture. Unarmed it filters nothing, so a share that restarts its timeline (tincan-screen timestamps start per share) is delivered.
- **Bitrate:** AIMD per track at the forwarding hop (`media/stats.rs`):
  - x0.7 after a second with drops or a viewer reporting loss (from the measured input rate when that is below the target); +10% after every 4 clean seconds; kept within `[300 kbps, cap]`.
  - Cap = min(4.5 Mbps, 10 Mbps host budget / all viewer streams, 800 kbps if a viewer is relayed, 1 Mbps if the sharer is relayed).
  - The target goes to the sharer as `TargetBitrate` when it moves by 5% or more (as a local event when the host or a call peer shares).
- **Reports:** every member sends a `ReceiverReport` once a second for each track it received. For voice the host keeps the worst report per track (its own uplink measurement included) and sends it to the track's owner, which emits `LossReport`. Video reports with loss count as congestion. In calls the peer's reports go straight to the owner.
- **Migration:** on every new transport the session re-sends `VoiceJoin` and its `Subscribe`s, re-attaches its share (to the new host link or into its own SFU) and asks the app for a keyframe. Measured on loopback:
  - Clean handoff: voice back after 62 ms, video after 0.49 s.
  - Host crash with real liveness: voice back after 11.96 s (failover 11.93 s).
- **Soundboard:** `SoundPlayed` goes to the host. It checks `by == sender` and sends it to each member of the sender's room over that member's control link (not `broadcast_ctl`), plus a local event when the host is in that room.
- **Calls:** `CallInvite/Accept/Decline/End/CallState` on the DM control stream, media on the DM connection (direct, no forwarding).
  - Both sides hold the DM connection (`dm_hold`) while a call exists. Ringing ends after 45 s (`Timings::call_ring`).
  - Glare (both ring each other): the call of the lower user id wins and the other side answers it. `call_start` toward someone who is ringing me answers their call.
  - A second caller is declined (busy). A replaced DM connection moves the media over; a lost one ends the call (`CallEnd::Lost`).

## Part B: rendezvous

- Code: `src/rdv.rs`. On by default (`NetConfig.rendezvous`), never with `RelayChoice::Disabled`: no network calls at all then. The n0 pkarr relay is used only with `RelayChoice::N0Default`; custom relays use the DHT only.
- Publish:
  - Resolve relay and DHT first and apply the clobber rule: another host with epoch >= mine and younger than 15 min is dialed instead (which leads to the merge). If this device still hosts 10 s later (that host was unreachable or lost the merge; the publisher task ends with the host role), it publishes anyway.
  - Then `seq = max(now_us, highest seen + 1)`, a relay PUT (serialized per process, 4 s apart) and a DHT put with `cas` = the DHT's seq (one retry without cas on a CAS mismatch). A new `n0_mainline::Dht` client per operation.
- Host schedule:
  - First publish 2 s after becoming host, then on roster changes: 30 s debounce, at least 10 s apart, only when the epoch, relay URL or online list changed.
  - Relay refresh every 5 min, DHT refresh every 30 min.
  - Re-resolve every 60 s while solo (5 min after 30 min alone, and while hosting members). A foreign host found this way is dialed (`PeerSeen`).
- Cold open resolves the record in parallel with the cached-peer dials (every `ColdOpen` seek). `join` resolves the invite's record in parallel with the bootstrap dials and dials the host and online members it names.
- Live runs (`cargo test -p tincan-net -- --ignored rendezvous_live`, random keys, nothing identifying):
  - `rendezvous_live_publish_resolve`: publish to relay and DHT 6.4-7.2 s (including the resolve before it), relay resolve 147 ms, DHT first item 350-375 ms, and a second publish landed above the first.
  - `rendezvous_live_join_through_the_record`: the host's record was resolvable 6.4 s after `create_server`; a joiner whose invite listed nobody joined through the record alone in 1.27 s.

## Timings

`src/timing.rs` holds every timer; defaults come from `tincan_core::election`. In-crate tests use `Net::start_with(.., Timings)` (pub(crate)) with shortened values, except the crash test, which uses the real 2 s tick and 10 s death.

Idle cost:
- Member: one 2 s tick timer per connected server.
- Solo host: no tick, probe timer only.
- DM: one 120 s idle timer per open connection.
- No polling loops.

## Known limits

- The SFU and the voice session take the host epoch only from role changes. After a merge, the winner's epoch bump does not reach the epoch byte (informational only).
- A sharer always uploads to its host, even when nobody watches (the host's GOP cache needs it for instant start).
- After a share stops, its track and GOP cache stay at the host until the sharer disconnects or shares again; a viewer that subscribes to that stale track id gets the old cached group.
- Without simulcast, one relayed viewer caps the share for everyone at 800 kbps.
- The proactive-migration decision is unit tested (`Proactive::evaluate`). The full 10-minute migration path was not run end to end.
- `Hello` has no address field. A host learns a member's address only from QUIC paths, and over a relay that is the relay URL.
- DM pending state is "my head > peer's ack". A message whose signature the peer rejects stays pending forever.
- After 5 "gap" answers, an in-flight event is dropped and its seq released for reuse. If an earlier host had stored it and comes back later, two events share that seq (equivocation: each replica keeps the first it saw). This needs a host to crash between storing an event and fanning it out, and the event must reach no other member.
