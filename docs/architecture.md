# Tincan architecture (v1)

Locked 2026-11-22 from the research in `docs/research/` (every version and API below was compiled on this machine by a probe; sources in `docs/research/probes/`).

## Goals and defaults

- Windows 10/11 desktop. No accounts, nobody runs a server. One setup step: pick a display name.
- Small footprint: one Rust process plus WebView2. Rust core target < 30 MB RSS idle, < 5% of one core in a 6-person voice call.
- Defaults chosen without asking (change here if wrong):
  - Server size target: up to 50 members, up to 10 in one voice room, one or two screen shares at a time.
  - New members receive full history (Discord behaviour).
  - One device per identity.
  - Any member can create invites. Only the server owner can create, rename or delete channels.
  - Attachments up to 25 MB are auto-fetched by every online member so they stay available after the uploader leaves (setting, 0 disables). Larger files are fetched on demand.
  - Soundboard clips: assumption, max 20 s and 5 MB per clip (decoded length check), to keep a clip from hijacking a call.
  - Soundboard plays while your mic is muted; it is disabled while deafened.
  - Relays: n0's public iroh relays by default (development and hobby tier, rate-limited, no SLA). A custom relay URL is a setting. This is the one caveat to "no servers": a small self-hosted `iroh-relay` or n0 Pro ($19/month) is recommended before a public release.
  - Installer: NSIS, per-user (no admin). Windows Firewall asks once on first run; direct paths need "Allow".
- Out of scope for v1 (room left in the event enum): kick, ban, roles, reactions, edits, replies, avatars, multi-device, group end-to-end encryption beyond QUIC, store-and-forward DM mailboxes, system audio in screen share, camera video.

## Stack (pinned)

| Piece | Crate / package | Version |
|---|---|---|
| App shell | tauri, tauri-build, @tauri-apps/cli, @tauri-apps/api | 2.12.7, 2.7.6, 2.12.6, 2.12.5 |
| UI | React 19, Vite 8, TypeScript 6, lucide-react | from create-tauri-app 4.7.5 react-ts |
| Transport | iroh (QUIC, hole punching, relays) | 1.5 |
| Files | iroh-blobs (BLAKE3 verified, resumable, multi-provider) | =0.105.0 |
| LAN discovery | iroh-mdns-address-lookup | =0.6.1 |
| Rendezvous | iroh-dns (pkarr SignedPacket), n0-mainline (BEP44 DHT) | =1.5.0, =0.8.0 |
| Audio I/O | cpal (WASAPI) | 0.18 |
| Echo cancel, noise suppression, AGC | sonora (pure Rust WebRTC APM) | 0.3 |
| Opus | unsafe-libopus (pure Rust libopus 1.3.1) behind `OpusCodec` trait | 0.2 |
| Clip decode, resample | symphonia, rubato | 0.6, 5 |
| Screen capture + H.264 | moq-video (`capture` feature: Windows.Graphics.Capture + Media Foundation hardware encoder) | =0.1.23 |
| Persistence | rusqlite (`bundled`) | latest 0.3x |
| Wire encoding | postcard + serde | 1.x |
| Crypto | iroh-base keys (ed25519), blake3, chacha20poly1305 | 1.5, 1.8, 0.11 |

Licences are permissive (MIT/Apache-2.0, BSD-3-Clause). `unsafe-libopus` is stale (last release 2024-11); it sits behind a trait so a maintained Opus can replace it.

## Process model

```
tincan.exe (Rust)                                  WebView2 (UI only)
 ├─ tokio runtime, 3 worker threads                 ├─ React app
 ├─ tincan-net    iroh Endpoint + Router            ├─ WebCodecs VideoDecoder (screen share viewing)
 │                 ALPN tincan/server/1             └─ canvas rendering
 │                 ALPN tincan/dm/1
 ├─ tincan-files  iroh-blobs FsStore, ALPN /iroh-bytes/4
 ├─ tincan-audio  cpal threads, APM, Opus, jitter buffers, mixer
 ├─ tincan-screen WGC capture + MF H.264 thread
 └─ tincan-store  SQLite (one file per profile)
        Tauri commands / events (JSON)  +  Tauri Channel (raw bytes, video only)
```

All audio and capture are native. The webview never touches the microphone or the screen: this avoids the WebView2 getDisplayMedia deadlock (WebView2Feedback #5763), the unverified echo-cancellation path for webview playback, and the WebView2 permission prompts. The webview only decodes and draws received video.

`TOKIO_WORKER_THREADS=2` is set at the top of `main` before any thread exists (iroh-blobs' FsStore builds its own runtime sized by it). The app's own runtime is built explicitly with 3 workers and handed to Tauri via `tauri::async_runtime::set`.

WebView2 does not throttle a minimized Tauri window on Windows (measured). `visibilitychange` never fires on minimize, so the UI follows Tauri window events (minimized state), not Page Visibility. While the window is minimized a watched screen share is unwatched (`screen_unwatch`, the decoder closed), so nothing is received, copied over IPC or decoded; it is watched again on restore and starts at a keyframe.

Exit sequence:

- Closing the window hides it at once, then runs `App::shutdown` (media, then the host handoff alongside the file store flush) for at most 5 s before the process exits.
- Windows logoff, shutdown and Restart Manager end the session through `RunEvent::Exit`: the shell runs `App::shutdown` there, blocking, with a 4 s cap (`src-tauri/src/exit.rs`, `main.rs` `end_session`).
- One instance per profile (`src-tauri/src/instance.rs`): a second launch of an open profile focuses the first window and exits with code 0. A launch while the profile is still closing waits for the old process instead of failing.
- The uninstaller's "delete the application data" box removes `%APPDATA%\Tincan` (every profile; `src-tauri/windows/hooks.nsh`).

### Profiles (multi-instance testing)

`tincan.exe --profile <name>` (or env `TINCAN_PROFILE`) selects `%APPDATA%\Tincan\profiles\<name>\` for the SQLite file, identity key, blob store and the WebView2 data directory. Default profile is `default`. Two profiles on one machine are two independent identities.

## Crates and ownership

```
Cargo.toml            workspace
crates/tincan-core    pure: ids, events, signing, validation, HLC, wire messages, media headers,
                      sync vectors, election scoring, invites, Storage trait + MemStorage
crates/tincan-store   SqliteStorage (implements core::Storage) + app tables
crates/tincan-net     endpoint, router, server sessions (join, sync, fan-out, election, migration,
                      merge), rendezvous, DMs, 1:1 calls, media transport + host SFU forwarding
crates/tincan-files   iroh-blobs store, attachments, holder queries, prefetch, upload throttle,
                      soundboard clip fetch
crates/tincan-audio   audio engine, Opus trait, jitter buffer, mixer, clip decode and playback
crates/tincan-screen  source listing, WGC capture, H.264 encode, keyframe/bitrate control
crates/tincan-app     orchestrator: profiles, identity, wiring of all crates, the UI API as plain
                      async methods + an event stream of DTOs (mirrors ui/src/api/types.ts),
                      end-to-end tests with two in-process instances
src-tauri             thin Tauri shell: commands -> tincan-app, events -> emit, video Channel,
                      window, bundling
ui                    React UI (runs in a browser against a mock backend for development)
```

Dependency rule: `core` depends on nothing internal. `store`, `net`, `files`, `audio`, `screen` depend only on `core`. Only `tincan-app` wires them together; `src-tauri` depends on `tincan-app`. Cross-crate communication goes through plain types in `core` and tokio channels.

Router wiring: `tincan-net` builds the iroh `Endpoint` (`build_endpoint`), `tincan-files` loads its blob store with that endpoint, then `tincan-net` spawns the `Router` with its own ALPNs plus extra protocol handlers passed in by the app (the blobs protocol).

## Identity and keys

- `UserId` = iroh `EndpointId` (ed25519 public key, 32 bytes). Displayed as z-base-32 (`PublicKey::to_z32`). Secret key in the profile dir (`identity.key`, 32 bytes).
- Every event is signed by its author with the same key: `sig = ed25519(sk, "tincan-ev1" || postcard(Event))`. `EventId = blake3(postcard(Event))`.
- `ServerId` = EventId of the owner-signed `Genesis` event. The Genesis event itself carries `server = [0; 32]` (it cannot contain its own hash); every later event carries the real ServerId.
- `server_secret`: 32 random bytes, given to members at admission. Sub-keys via `blake3::derive_key(context, server_secret || server_id)`:
  - `"tincan v1 rendezvous signing key"` -> ed25519 seed for the rendezvous record (rdv_sk / rdv_pk)
  - `"tincan v1 rendezvous record encryption key"` -> rdv_read_key (XChaCha20-Poly1305)
  - `"tincan v1 hello mac key"` -> hello_key

## Connections

| ALPN | Who dials | Carries |
|---|---|---|
| `tincan/server/1` | member -> current host of that server | control+sync bi stream, audio datagrams, video uni streams |
| `tincan/dm/1` | either side of a DM | DM log sync, 1:1 call signaling, call audio datagrams and video streams |
| `/iroh-bytes/4` (iroh_blobs::ALPN) | downloader -> holder | file and clip transfer |

One connection per member-host link carries everything for that server, so audio and video share one congestion controller. noq writes queued datagrams before stream data in every packet (verified in noq-proto source), so audio is not stuck behind video. Media-tuned transport config (dial and accept, `ConnectOptions::with_transport_config` / `Incoming::accept_with`):

- `max_idle_timeout` 10 s, `keep_alive_interval` 2 s
- `datagram_send_buffer_size` 16 KiB (about 150 ms of fan-out audio; the 1 MiB default holds seconds of stale audio)
- `datagram_receive_buffer_size` 256 KiB
- congestion controller: default (Cubic). BBR3 is an A/B experiment later.

Every dial is wrapped in a 4 s app-level timeout (a dial to an offline id otherwise hangs 30 s, measured).

Stream priorities on the server connection: control stream 100, video GOP streams 0..50.

## Framing

Control streams: `u32 little-endian length || postcard(Ctl)`, max 1 MiB per frame. Defined in `tincan_core::wire`.

## Events (replicated history)

Each server's state is a grow-only set of signed events. Every member stores all of them, so any member can become host and the host holds no unique state.

```
Event { v: u8, server: ServerId, author: UserId, seq: u64, hlc: u64, body: Body }
Body:
  Genesis       { name, owner, nonce }
  Profile       { name }
  ChannelCreate { name, kind: Text|Voice, position }
  ChannelUpdate { channel, name?, position?, deleted? }      owner only
  ServerUpdate  { name? }                                    owner only
  Message       { channel, text, attachments: [Attachment] }
  Delete        { target }                                   author of target, or owner
  InviteCreate  { invite_pk, expires_ms, max_uses }
  InviteRevoke  { invite_pk }
  MemberAdmit   { user, invite_pk, from_seq }
  MemberLeave   { last_seq }
Attachment { hash, size, name, mime }
```

- `seq` is per author per server, contiguous from 1. Events are stored only when contiguous; out-of-order events wait in a small pending buffer.
- `hlc` = (unix ms << 16) | 16-bit logical counter. Display order is `(hlc, event_id)`. Events more than 5 minutes in the future are buffered.
- Validation (every member runs it locally, deterministic): signature, author's seq/hlc strictly increasing, author admitted (Genesis owner, or a `MemberAdmit` naming them, and not after their `MemberLeave.last_seq`), body-specific authority rules above.
- Sync: on connect both sides send `SyncHave { per author: highest contiguous seq }` (about 40 B per author). Each side streams what the other lacks per author in ascending seq order (receivers store only contiguous runs), in pages of at most 256 KiB. Pages go on the control stream between live messages; at the target scale (50 members) a full history is a few MB.
- Live path: member `Publish{event}` -> host validates, stores, `PublishAck`, fans out `Events` to everyone else. The host's own events take the same path internally.
- `VvDigest` every 60 s catches anything missed.

## Presence and ephemeral state (host-aggregated, re-announced)

Members re-announce everything ephemeral in `Hello` and on change: `Status` (uptime, AC power, voice room, sharing, muted/deafened), held blob hashes (`Have`), soundboard clips in their library. Held blobs and clips are announced per server: a host learns only the ones shared with its server (`Net::set_announced`), and DM contacts get no `Have` list. The host aggregates them into the `Roster` it broadcasts. After a migration everyone re-announces to the new host, so nothing is lost when a host dies.

`Roster { epoch, rev, online: [RosterEntry], succession: [UserId; <=4] }`
`RosterEntry { user, short_id: u16, addr: AddrHint, since_ms, voice: Option<ChannelId>, muted, deafened, sharing }`
The host fills `voice` from its SFU (what the member joined with `VoiceJoin`), not from the member's `Status`.

`short_id` is assigned by the host, carried in the roster, and kept by the next host (it reuses the roster it inherited), so media track ids survive migration.

## Host election and migration

Cold open (per joined server, while the app runs):
1. In parallel: dial the last known host, the succession list and up to 6 recently seen members (cached `AddrHint`s, staggered 150 ms, 4 s timeout each); LAN mDNS (`service_name = "tincan1"`, cadence 5 s, intersect with members); resolve the rendezvous record (n0 pkarr relay GET, about 150 ms, and mainline DHT, 0.5-3 s).
2. Any member that answers replies `Welcome` (if it is host) or `Redirect` (points to the host).
3. Nobody reached after 6 s: become solo host with `epoch = last_seen_epoch + 1` and publish the rendezvous record.
4. While solo: retry step 1 every 60 s, backing off to 5 min after 30 min alone.

Liveness: `Tick` every 2 s both ways. Host suspected at 6 s of silence, dead at 10 s. A QUIC close with app code `HOST_LEAVING` (1) switches immediately.

Succession: the host ranks online members and puts the top 4 in the roster. Score (higher wins, tie -> lower UserId):

```
score = 400 * direct_path        (host sees a selected direct path to them: conn.paths() is_selected && is_ip)
      + 200 * on_ac_power
      + 200 * min(uptime, 2h) / 2h
      +  50 * (1 - min(rtt, 300ms) / 300ms)
```

Relay-only members are excluded from succession while any member with a direct path exists. `net_report` is not used (it is behind an unstable iroh feature).

Clean handoff (app exit, system suspend, "stop hosting"): host sends `Handoff{claim(epoch+1, S1), roster}` to S1, waits up to 2 s for `HandoffAck`, broadcasts `HostMoved`, closes with `HOST_LEAVING`. Members dial S1.

Crash failover: members keep the succession list. On host death, S1 promotes itself to `epoch+1` and accepts connections; members dial S1, and if S1 is unreachable for 3 s, S2, and so on. S_k promotes itself only after k x 3 s without reaching any higher-ranked successor.

Proactive migration ("most stable member hosts"): allowed only when no voice room is active, the best candidate's score has been at least 30% higher for 10 minutes, and 30 minutes have passed since the last migration. Implemented as a clean handoff.

Epochs: `SignedHostClaim { server, epoch, host, prev, issued_ms }` signed by the claiming host. A member follows a new claim only from its current host (`HostMoved`) or after losing its host. Unsolicited higher-epoch claims are ignored. Every claim must also be at most the epoch ceiling (one epoch per second of unix time plus 1 000 000, 5 min of skew allowed), which every node computes without history.

Partition merge (two solo hosts is the common case, given the 6 s solo timeout): triggered when a dial, a `Redirect` or the rendezvous record reveals another host for the same server. The hosts exchange `HostCompare2{claim, attests}`, where each attestation is signed by a connected member for that host and epoch. Winner = max(verified attested members, epoch, -UserId). The loser hands its members over only if the winner proved at least as many members as the loser has connected (re-checked when the winner's `HostMoved` arrives). The winner keeps its claim if its epoch is higher or the loser proved no members; otherwise it claims the loser's epoch + 1. The loser sends `HostMoved(winner's claim)` to its members and becomes a member of the winner. Everyone resyncs with `SyncHave`.

## Security model

### Threat model

- Members are admitted by invite and hold `server_secret`, but none of them is trusted. Any single member may run a modified client.
- Non-members can reach only the ALPNs. They fail the `Hello` MAC and the join checks. Strangers can open a DM connection, which stores nothing until they send a valid first message.
- Events are signed by their authors and validated by every member, so nobody (the host included) can forge or alter another member's events.
- Hosting is not a privilege. Any member can host legitimately by being online first or by ranking as successor. The host forwards media in plaintext (no end-to-end encryption beyond QUIC, by design).

### What a malicious member must not be able to do

1. Take over hosting from a live host that has other members connected.
2. Degrade or disrupt other members' media or sessions beyond what an honest bad network link would.
3. Make the host or other members do unbounded work or hold unbounded memory.
4. Impersonate another member in any control message.
5. Make honest members diverge.

### How this holds

- Takeover: partition merges count only member-signed attestations (fresh, unique, current members, not the peer's own links). A host with members to hand over counts only established attesters: members whose admission this replica stored at least 24 h ago by its own clock (recorded per member in kv; members known before this was recorded count as long ago). Members admitted just now, by the peer or through members it admitted, never count. A host with members yields only to a peer that proved at least as many, re-checked with the established count when the move arrives. A solo host counts every valid attestation, so it still rejoins a young server. Self-reported counts and scores are ignored, a bare compare never moves the host's epoch, epochs are bounded by a shared ceiling, and compares are rate limited. A successor that never promotes delays a failover by at most `host_dead + stagger`.
- Disruption and unbounded work: every input a member controls is bounded per member (see notes-tincan-net.md "Hardening"). That covers typing, status and roster, publishes, syncs (one in flight per link), holder queries, held-blob lists, clips, soundboard, admissions, merge compares, attestations, audio datagrams, video uploads, subscriptions and keyframe requests, pending buffers, video receive state, QUIC stream windows and pre-auth connections. Relayed chatter can use at most half of another member's queue and never gets anyone disconnected. Budgets that protect shared resources (admissions, joins) are keyed per member, so one member cannot spend everyone's. Every admission of a new member is budgeted on every path (joins, publishes, own and relayed `Events`, probe links) and charged to the member that answers for its author: the nearest admitter up the chain that this replica stored before the host role started or at least 24 h ago, so keys admitted just now never bring budgets of their own. A host-wide budget bounds new members over everyone, and members bound how many new members a host can make them apply. New events in `Events` frames are charged per sender in bytes, and a sender's own events against its own budget, so `Events` is no way around the publish budget. DM events are budgeted per contact, tightly in a stranger's unanswered conversation.
- Impersonation: the host relays `Typing` and `SoundPlayed` only with `by == sender`. Receivers accept them only for current members listed online (rooms) or from the peer (calls and DMs). Attestations, claims and events are signed by whom they name.
- Divergence: my seq is never moved by a host and never reused once a signed event left the device. Rule-breaking events are rejected identically everywhere (deterministic validation). History over a budget is held back and asked for again later, never dropped, and an author's chain stays contiguous. An author signing two events for one seq is detected, and the proof is kept. Epoch checks need no local history, so no honest node can be locked out by how far the epoch moved while it was away.

### What the host can and cannot do

- It can: see and drop everything it relays (messages it fans out, plaintext media), delay or refuse publishes (censor), choose the order and timing of fan-out, show a wrong online list and succession for its members, push the epoch up to the ceiling, and refuse to yield in a merge it lost (keeping the members it already has).
- It cannot: forge or alter events, move my seqs, make me sign two events for one seq, admit members past the invite rules, impersonate members to my app beyond "is typing" / "played a clip" for members that are online, put non-members into my stored peers or short-id inheritance, lock anyone out through the epoch, or make me do unbounded work.

### Residual risks

- Sybils: any member can mint invites and admit throwaway identities (its budget: 20 at once, then one a minute per host role; 60, then one per 10 s, over everyone). A member that admitted keys at least 24 h ago (pre-aged sybils) can still use them as attesters and as members with budgets of their own, so a member that prepared at least as many as the live host has connected members can still win a merge. So can identities that join through another member's invite that was shared publicly: their admission chain leads to that member, not to the attacker. A member admitted during the current host role and less than 24 h ago spends its admitter's budget, so a new member's flood can delay joins through its admitter's invites (often the owner's) for a while.
- A server whose members were all first seen less than 24 h ago does not merge two partitions that both have members: each host counts none of the other's attesters. They merge once the members are established (or when one side has no members left).
- Two hosts with 0 attested members still compare by epoch and id, which is equivalent to "online first" because neither side moves attested members.
- Attestations are accepted up to 12 min old (members' clocks may be off by minutes), so a host can still count members that left it within the last ~12 minutes (but not ones now connected to the comparing host).
- Two hosts whose membership views differ may both think they won a round; the merge waits for a later round after their histories synced.
- A rogue host can keep the members it already holds by ignoring a merge it lost, and it can censor them.
- Equivocation is detected, not resolved. Replicas may show different versions of one seq until a proof-carrying resolution exists.
- Events arriving in bulk from a member are still signature-checked on the session actor (known events are skipped first, and a bad signature drops the sender).
- Ex-members keep `server_secret`, so they can pass the `Hello` MAC (but not the membership check) and write the rendezvous record until secret rotation exists.
- A malicious host can push the epoch up to the ceiling. Every node accepts that, so nobody is locked out. A node whose clock is more than 5 min behind refuses such claims until its clock catches up (at most its clock error), and an honest successor just above the ceiling waits for it the same way.
- Admission and compare budgets are per host role: they reset when a host stops and starts hosting again. Admissions held back on a probe link are not asked for again on that link; they come with a later sync (the next comparison, or the other side's members).
- A flood of fresh identities can push real joiners and strangers out of the 32 unknown pre-auth slots before they send their first frame (members and contacts I wrote to have their own 64, and a full pool drops a waiter of the peer holding the most).
- Stranger identities that keep reconnecting can use up the stranger connection rate (10 a minute), delaying first contact from real strangers. A full stranger pool (8) closes its oldest connection to make room, so idle holders lock nobody out. Unanswered stranger conversations older than 3 days no longer count toward the cap of 32, but nothing deletes them yet (there is no DM delete or block). Contacts and members of shared servers are not affected.
- Blob transfer throughput under the bounded QUIC windows (16 MiB per connection, 8 bidi streams) has not been measured.

### Files and peer text (tincan-app)

Admitted members and DM contacts are untrusted, hashes are not secret, and peer-supplied names and text are untrusted for the app and the UI.

- Blob access is decided per (peer, hash). A peer may fetch a blob from me only if it is shared with a conversation that peer is in right now. A blob becomes shared with a conversation when I send it there, play the clip in its voice room or call, download it from a peer that conversation named (an author of a reference there, or a holder its host listed), or when someone who may already fetch it from me reposts it in a server (a repost in a DM shares nothing). A message that only references a hash grants nothing. At most 16 shares are kept per hash; past that a DM share goes before a server share.
- A partial download found at start resumes only from where it came from: an attachment from the conversations of its references, a clip pull from the conversation it was played in. A partial of unknown origin is not resumed, so its hash is never shown to every host and contact.
- Each server's host learns only the held blobs and clips shared with that server. A host is still trusted to list real holders, so a host that also references a hash can get a blob I download from the holder it named shared with its server.
- Size caps: attachments 25 MiB, clips 5 MiB, checked against the proven size (`TooLarge`, shown as a failed download, and remembered). A provider's size header is only a claim: when it is over a cap, the provider is asked for the last chunk first, which proves the real size; one that cannot prove it fails only its own attempt. Larger declared sizes are never queued; `download` refuses them with "This file is larger than 25 MB, so Tincan won't download it."
- Opening a file exports it under a cleaned name (no hidden format or bidi characters, the real extension kept) and writes the Mark of the Web (`Zone.Identifier`, `ZoneId=3`). Only documents, media and images whose bytes match their extension are opened with the default app (the path passed to Explorer quoted as one token, and file names without commas); everything else is revealed in Explorer.
- Peer names (members, DM contacts, servers, channels, files, clips) have control and hidden format characters removed before they reach the UI, which also isolates them with `<bdi>` and shows a short id next to names that look alike. A DM contact's name is the one they announced over the DM.
- A host's refusal text (`NetError::RejectedByPeer`) is quoted as the host's words, never shown as Tincan's own sentence.

## Rendezvous record

A hint for cold joiners when every cached peer is stale. Never trusted on its own (dialers verify via `Hello`, `SignedHostClaim` and the member list).

- `RdvRecord { v, epoch, host, host_relay, published_ms, online: [UserId; <=12] }`, postcard, sealed with XChaCha20-Poly1305 under rdv_read_key, base64url, chunked into 240-char TXT strings under `_tincan`, as a pkarr `SignedPacket` signed by rdv_sk (889 B measured, max 1104 B).
- Published to the n0 pkarr relay (optional accelerator) and to mainline DHT as a BEP44 mutable item. Write rule: resolve first, then `seq = max(now_us, seen_seq + 1)` with `cas = seen_seq`.
- The host publishes on becoming host and on roster changes (30 s debounce, at least 10 s apart), refreshes the relay copy every 5 min and the DHT copy every 30 min, and re-resolves every 5 min to detect a foreign host.

## Invites and joining

`Invite { v, server, name_hint, invite_seed: [u8;16], rdv_pk, rdv_read_key, bootstrap: [UserId; <=2] + AddrHints, expires_ms }`, encoded `tincan1` + base32(postcard). The invite never contains `server_secret`.

1. Any member authors `InviteCreate{invite_pk = public(derive_key("tincan v1 invite key", seed)), expires_ms, max_uses}` and shares the code.
2. Joiner dials bootstrap peers and resolves the rendezvous record, then sends the host `JoinRequest{server, invite_pk, profile, ts_ms, invite_sig}` where `invite_sig = ed25519(invite_sk, "tincan-join1" || server || joiner || host || ts_ms)`.
3. Host checks the invite (exists, not revoked, not expired, uses below max_uses best-effort), authors `MemberAdmit`, replies `JoinAccepted{server_secret, genesis, admit}`. Normal `Hello` and sync follow. Joins are rate limited per invite and per invite creator (refused with `RejectCode::Busy`).

`Hello { server, nonce, mac = keyed_blake3(hello_key, server || client || host || nonce), epoch_seen, next_seq_hint, status, have_hashes }` proves membership to any host.

## Direct messages and 1:1 calls

- ALPN `tincan/dm/1`, dialed by UserId (iroh resolves addresses via n0 DNS; cached `AddrHint`s from shared servers are tried too).
- A DM log uses the same `Event` format with `server = blake3("tincan dm" || sorted(a, b))` and only `Message`/`Delete` bodies. Both sides store it; `SyncHave` on every connect.
- Offline delivery v1: the sender's outbox shows the message as pending and delivers when the contact is seen online in any shared server's roster, or on retry (30 s, 1, 2, 5, 15 min backoff, 4 s dial timeout).
- Calls: `CallInvite / CallAccept / CallDecline / CallEnd` on the DM control stream; media rides the same connection with the same framing. No host, no forwarding; the call ends when either side leaves.
- Adding a contact: from a shared server's member list, or by pasting `tincan1u` + z32 user id ("Copy my ID" in the user panel).

## Files (attachments and soundboard clips)

- iroh-blobs `FsStore` in the profile dir, `ImportMode::TryReference` for local files (no copy), named tags `att-<hash>` / `clip-<hash>` so GC keeps them.
- Provider events: `connected: Intercept` (only members of a shared server or DM contacts may connect), then every get and observe request is checked per (peer, hash) (see "Files and peer text" in the security model). While in voice, uploads are capped at a fixed 256 KiB/s (128 KiB/s while sharing): nothing measures the uplink.
- Holder index: members announce held hashes in `Hello`/`Have`, each server only the ones shared with it; the host keeps `hash -> online holders` and answers `WhoHas{hash}` with `Holders{hash, users, addrs}`. Downloads use the online authors first, then holders from the host, then offline authors.
- Downloader pool `connect_timeout` 4 s. A failed download is re-queued when a new holder comes online. `router.shutdown()` on exit so partial progress survives restarts (a hard kill loses the last partial chunk).
- Auto-prefetch: every online member fetches attachments up to 25 MB when the message arrives, so the file survives the uploader leaving first.
- Opening a file exports it to `Downloads\Tincan\` with the Mark of the Web, then opens it with the default app only when it is a document, media or an image whose bytes match its extension; anything else is revealed in Explorer.

## Media

### Audio (native, tincan-audio)

- Capture/playback: cpal WASAPI shared mode, 48 kHz mono f32, 10 ms processing frames, rubato resampling when the device rate differs.
- Processing: sonora `AudioProcessing` with echo canceller, noise suppression, AGC2 and high-pass. The render reference is the exact post-volume mix sent to the speaker (`process_render_f32`).
- Encode: Opus 48 kHz mono, 20 ms frames, VOIP/voice, VBR 32 kbps (16-48), complexity 7, in-band FEC on, packet-loss-perc from receiver reports (5-20), DTX on (off while a clip plays).
- Audio datagram, 12-byte big-endian header, forwarded verbatim by the host:

```
[0]     0xA1 (audio v1)
[1]     flags: bit0 speaking, bit1 talkspurt start, bit2 FEC present, bit3 soundboard active
[2..4]  track_id u16 = short_id * 4 + kind (kind 0 = voice)
[4..6]  seq u16 (+1 per packet; DTX does not consume seq)
[6..10] ts u32 (48 kHz sample clock, random origin)
[10]    level 0..127 (-dBov)
[11]    host epoch low byte
[12..]  Opus packet
```

- Jitter buffer per track: min 40 ms, start 60 ms, max 200 ms (300 ms on relayed paths). Target = max(min, P95 of relative arrival delay over 2 s + 1 frame). Grow on underrun, shrink by dropping one frame in silence when over target + 20 ms for 1 s.
- Loss: decode FEC from the next packet if buffered, else Opus PLC for up to 5 frames, then fade to silence.
- Mixer: per-user gain, sum, soft limiter. UI sounds (join/leave chimes) go through the same mixer so the echo canceller sees them.
- Speaking: sender sets bit0 from post-APM energy with 250 ms hangover; receivers light the ring after 2 speaking packets and hold 300 ms. The engine emits `Speaking{track_id, speaking}` changes only.
- Mute: stop sending voice datagrams. Deafen: mute + stop playout.

### Soundboard

- Library: clips the user added (file picker) or pulled from others. Stored as blobs; metadata (hash, name, duration) in SQLite.
- Play: the engine decodes the clip (symphonia), resamples to 48 kHz mono once and caches the PCM. While playing, clip samples are mixed into the outgoing stream after noise suppression and AGC (so they are not gated or squashed), DTX and voice gating are off, and flag bit3 is set. The clip is also mixed into local playout, so the player hears it and the echo canceller has it as reference.
- Announce: the player sends `SoundPlayed{clip: ClipInfo}` to the voice room via the host; the host forwards it to the room. Listeners see it under "Heard in this call" and can pull it (holders = the player plus anyone who has it). Once pulled it is in their library and they seed it.

### Screen share (native, tincan-screen)

- Sources: displays and windows via `moq_video::capture::{displays, windows}`. Tincan draws its own picker.
- Capture: Windows.Graphics.Capture (frames arrive only on change, so every stage handles variable frame rate). Requested output size is ignored by WGC; the encoder input is downscaled when wider than 1920.
- Encode: Media Foundation hardware H.264, Annex-B with SPS/PPS in band, CBR, low-latency mode, no B-frames, GOP 4 s plus forced IDR on request (`Encoder::cut`), live bitrate (`Encoder::set_bitrate`).
  - Text mode (default): up to 15 fps, 1.2 Mbps target, 2.5 Mbps cap.
  - Motion mode: up to 30 fps, 3 Mbps target, 4.5 Mbps cap.
  - Cap = min(mode cap, host_uplink_budget / viewers) with host_uplink_budget = 10 Mbps default.
  - No hardware encoder: screen share is unavailable on that machine and the UI says so (openh264 is a later opt-in feature).
- Video transport: one QUIC uni stream per GOP, `set_priority(group_seq)` before the first write.
  - Stream header: `0xB1, track_id u16, group_seq u32, codec u8 (1 = H.264 Annex-B), flags u8`
  - Frames: `varint len | ts u32 (90 kHz) | fflags u8 (bit0 keyframe) | access unit`
- Host forwarder per viewer: bounded queue; if a viewer's queue exceeds max(2 frames, 250 ms) or a frame is older than 300 ms, reset that viewer's group stream, skip to the next keyframe and send a coalesced `KeyframeRequest` upstream (at most 1/s per track). The host caches the current GOP (bounded 4 s / 2 MB) so a new viewer starts immediately.
- Viewing: viewers `Subscribe{track_id}` / `Unsubscribe`. Received access units cross to the webview over a raw Tauri Channel: `[u8 flags][u16 track_id][u32 ts][AU bytes]`; the UI decodes with `VideoDecoder({codec: 'avc1.' + SPS profile/level, hardwareAcceleration: 'prefer-hardware', optimizeForLatency: true})` without a description, in a synchronous `onmessage`, and draws the latest frame on rAF while the window is visible.

### Host SFU

The host forwards each voice datagram to every other member in the same voice room (no decode), injects its own engine's packets the same way, and hands members' packets to its own engine. Video goes only to subscribers. A relayed viewer is capped at 800 kbps and a relayed sharer at 1 Mbps.

## App API

Commands and events between Rust and the UI are defined in `docs/api.md` and mirrored in `ui/src/api/types.ts`. The UI ships a mock backend (`ui/src/api/mock.ts`) so it runs in a plain browser during development.

## Storage (SQLite, per profile)

Tables: `identity`, `settings` (key/value JSON), `servers` (id, name, secret, genesis, my_next_seq, last_epoch, last_host, joined_ms), `events` (id, server, author, seq, hlc, kind, channel, body blob, sig, deleted), `peers` (server, user, addr_hint, last_seen_ms), `profiles` (user, name, updated_hlc), `dm_contacts`, `outbox`, `clips` (hash, name, duration_ms, size, added_ms), `downloads` (hash, state).

## Testing

- Unit tests in `core` for validation, ordering, sync diffs, election scoring, invites, framing, media headers.
- In-process multi-endpoint tests in `net` (relays disabled, loopback addresses): join and sync, clean handoff, host crash, two solo hosts merging, ephemeral state restored after migration, DM offline outbox delivery.
- `files` tests: download with the uploader offline from a partial and a complete holder; resume after restart.
- `audio` tests with pluggable source/sink: synthetic or WAV in, WAV out, two engines through an in-memory "network" with loss and jitter.
- Two profiles on one machine for end-to-end runs.

## Not verified yet (needs two machines on different networks)

WAN hole punching rate, relay throughput for voice and video, echo cancellation quality in a real room (sonora 0.3), the first-run firewall prompt being dismissed, SmartScreen on an unsigned installer, hardware encoder coverage on iGPU-only machines.
