# tincan-app: integration notes

Crate: `crates/tincan-app`. The public surface in `src/lib.rs` is the frozen shell contract (`App::start`, `call`, `subscribe`, `screen_watch`, `file_for_url`, `shutdown`). Additions are `pub mod dto` (the JSON types) and the `#[doc(hidden)]` test hooks listed below. Every `types.ts` command and event is implemented; media (voice rooms, calls, screen share, soundboard, devices) lives in `src/media/`.

## Module map

| File | What it holds |
|---|---|
| `lib.rs` | Frozen surface: `App` wraps `Arc<Core>`, plus the test hooks |
| `app.rs` | `Core`: every subsystem and cache, `start` and `shutdown`, conversation membership for the access check, what to announce, `set_name` and Profile publishing, `net_err` |
| `index.rs` | The hash index (`HashInfo`: references and shares per hash), its kv encoding, the bounded cache, and `Access` / `Policy`, the per (peer, hash) rule the blobs provider applies |
| `profile.rs` | Profile directory layout, the lock file, `identity.key`, the display-name kv key |
| `settings.rs` | `Settings` defaults, validation (`normalize`), load and save |
| `dto.rs` | Every `types.ts` interface (`serde(rename_all = "camelCase")`, exact enum strings) |
| `emit.rs` | `Ev` (every `Events` name) and `Emitter` (bounded broadcast of `UiEvent`) |
| `dispatch.rs` | `App::call`: argument structs, routing, `CORE_COMMANDS` |
| `commands.rs` | Servers, channels, messages, DMs and settings commands; cursor and id parsing |
| `transfers.rs` | Attachment import, hash index bookkeeping (references, shares, download nominations), auto-fetch, the fetch worker, `transfer` / `download` / `open_file` / `file_url` / `file_for_url`, resume, lag recovery |
| `views.rs` | `ConvView` (cached slice of `ConvState`), DTO builders, `refresh` with dedup |
| `pump.rs` | The event pump: `NetEvent`/`FileEvent` to UI events, debounce and bulk coalescing |
| `shell.rs` | MIME types, safe export names, peer text clean-up, unique paths, the open or reveal decision, the Mark of the Web |
| `media/mod.rs` | `Media`: state, `COMMANDS`, command routing, the hooks the rest of the app calls, lock order |
| `media/engine.rs` | The `AudioEngine` slot, its event forwarder, restart after the engine thread dies, the synthetic test microphone, `decode_clip` under `catch_unwind` |
| `media/session.rs` | Attach and detach of a `MediaSession`, `reconcile`, audio and video pumps, `VoiceSession` and `CallInfo` DTOs, calls, mute and deafen, per-user volume, devices, mic test |
| `media/clips.rs` | Soundboard library, heard clips, pulling a heard clip, announcing the library |
| `media/screen.rs` | Screen sources, share start and stop (tincan-screen or the synthetic pattern), `screen_watch` and `screen_unwatch` |

## Profile directory

`tincan.db` (`SqliteStorage`, held as `Arc<SqliteStorage>`), `identity.key` (32 bytes, written through a temp file on first start), `blobs/` (`Files::load`) and `tincan.lock`.

- The lock is `File::try_lock` on `tincan.lock`. It is held until `App::shutdown`, not until process exit, so a test can reopen the same profile in-process.
- A second start fails with "Tincan is already running with this profile."
- The display name lives in kv `app/display_name`. It is not in the settings document, because the UI's `Settings` has no name and `settings_set` would erase it.
- Settings are one JSON document in the store's settings table.
- Other kv keys:
  - `app/att/<hash>`: the hash index record (`index.rs`): up to 8 references (conv, DM flag, author, name, declared size, mime) and up to 16 conversations the blob is shared with. Written whenever a message referencing the hash is seen (pages, live events, history sync), a file is sent, a download completes, or a clip is played. A record from before this format (one origin) reads as one reference plus a share with that conversation.
  - `app/refused/<hash>`: the size an attachment turned out to have when tincan-files refused it (`TooLarge`). Auto-fetch and `download` never ask for it again.
  - `app/export/<hash>`: the exported path, so `open_file` reuses an export that still exists with the right size.

## Startup

`App::start` runs these steps in order:

1. Lock the profile, load the identity, open the database, load settings and the name.
2. `build_endpoint`, with the network config chosen as follows:
   - Relay: `RelayChoice::Disabled` under `offline_network`, else `Custom` from `customRelayUrl`, else `N0Default`.
   - mDNS and rendezvous: off under `offline_network`.
   - Bind address: `127.0.0.1:0` under `offline_network`, which also avoids firewall prompts in tests.
   - If a stored custom relay is rejected, the endpoint is rebuilt with the default relays. A toast says so after the UI's first `app_snapshot`.
3. `Files::load(blobs, endpoint, policy)`. The policy is `index::Policy` over `Arc<RwLock<Access>>` (see Blob access below). It starts empty, so nobody can connect until step 5 fills in the conversations. Then the shares of every held blob are read from kv (`load_shares`).
   - Conversation membership is rebuilt from the cached views on startup, joins and leaves.
   - It is rebuilt at once on a `Members` change, so a new member can fetch right after joining.
4. `Net::start(.., |b| b.accept(tincan_files::ALPN, files.protocol()))`. Then subscribe to both event streams. The `Have` list is set in step 5 (`update_policy` calls `announce`).
5. Warm every view and DM delivery mark. Spawn the pump and the fetch worker. Read `files.partial_downloads()` for resuming later.

`TOKIO_WORKER_THREADS=2` is the shell's job (set in `main` before any runtime exists). Tests do not set it.

## Shutdown

`App::shutdown` runs once; concurrent callers wait (`tokio::sync::OnceCell`). Steps, each bounded:

1. `media.shutdown` (3 s): marks media closed, stops my share (capture thread joined on a blocking thread), detaches the session, drops every watcher and drops the audio engine (its thread is joined on a blocking thread). The call and the room themselves end in `net.shutdown`.
2. `net.shutdown`, which does the clean handoff and router shutdown, closing files (10 s budget; about 3 s in practice).
3. `files.shutdown` (idempotent, 5 s).
4. Abort the pump and the worker, release the lock.

After shutdown every command returns "Tincan is shutting down." and `file_for_url` returns `None`.

## Dispatch

`App::call(cmd, args)` deserializes `args` into a per-command struct (null counts as `{}`).

- A bad shape returns "Invalid arguments for <cmd>: ..." (only a UI bug can cause it).
- `CORE_COMMANDS` (dispatch.rs) and `media::COMMANDS` together are every `types.ts` command except `screen_watch` and `window_*`. `App::handled_commands()` returns both lists, and the contract test checks them against `types.ts`.
- Unknown names return "Unknown command: <name>".

Rules from api.md and the task:

- Ids and cursors:
  - Ids are lowercase hex; users are z-base-32 (`UserId::to_z32`).
  - The cursor is `<hlc>:<eventIdHex>` of the oldest message on the page. It is null when there is nothing older (the page fetches `limit + 1`; `limit` is clamped to 1..200).
  - Pages are oldest first.
- DMs:
  - `channelId` is null in every DTO. Internally the channel is `dm_channel(conv)`, and storage pages DMs with `channel = None`.
  - `pending` is `seq > net.dm_delivered(conv)` for my own DM messages.
- `sentAt` is `hlc_ms(hlc)`. A deleted message keeps its row with an empty `text` and `attachments`.
- Unread:
  - From the store's read markers.
  - A server is unread when any visible text channel is.
  - `mark_read` sets the marker to the newest message's hlc.
- `Member`:
  - Built from the view, plus the roster when the connection is `member`, `hosting` or `solo`; otherwise only I am online.
  - `isHost` comes from `net.connection`, `speaking` from `Media::is_speaking`.
  - Sorted with online members first, then by name.
- `ServerSummary.onlineCount` is the roster size (me included), or 1 without a current roster. `hostName` is the host's name in that server.
- Errors are short sentences, for example:
  - "Only the server owner can do that."
  - "Pick a display name first."
  - "You can attach up to 10 files to one message."
  - "<name> is larger than 25 MB. Send a smaller file."
  - "That isn't a Tincan ID. Paste the code that starts with tincan1u."

  Errors from tincan-net are passed through, except `NetError::Core` and `Endpoint`, which become generic sentences.

## Event mapping and throttling (pump.rs)

| Source | UI event |
|---|---|
| `EventAdded` + `Change::Message` | `message-added` at once, then an unread refresh |
| `EventAdded` + `Change::Deleted` | `message-updated` (the stored event, `deleted: true`) |
| `ConvUpdated` (`ServerRenamed`, `Channels`, `Members`) | view invalidated, then `server-updated` / `channels-updated` / `members-updated` after the debounce |
| `RosterUpdated`, `ConnectionChanged` | `server-updated`, `members-updated`, DM online states; holders re-queried for parked downloads |
| `Typing` | `typing` (`channelId` null for DMs), never for my own typing |
| `DmDelivered` | `message-updated` (`pending: false`) for my DM messages in the newly acked seq range (at most 200) |
| `DmOpened`, `DmLinkUp/Down` | `dm-updated` |
| `Joined`, `Left` | `snapshot` (plus the refreshes) |
| `Error` | `toast` (error) |
| `FileEvent::Transfer` | `transfer-updated`; state changes persisted in the downloads table |
| `FileEvent::Completed` (attachment) | `net.have_add(hash)`, so I show up as a holder |
| media `NetEvent`s (plus `RosterUpdated` and `ConnectionChanged`, which the pump also handles), every `FileEvent` | `Media::on_net_event` / `on_file_event` (see Media below) |
| a lagged network receiver | also `Media::on_lagged` (one reconcile) |

Throttling:

- **Debounce:** non-message changes mark a conversation dirty. One flush 100 ms later recomputes the marked DTOs. Each is emitted only if its JSON differs from the last one sent (`Core::sent`, keyed per DTO and conversation).
- **Bulk ingest:** tincan-net does not surface its sync-complete marker, so the pump detects bulk itself. A conversation goes bulk on any of:
  - more than 8 messages of one conversation in one drained batch (the pump drains with `try_recv` after each `recv`);
  - more than 40 messages in one second;
  - a window armed by `join_server`: before `Net::join`, and again for 1 s after it returns, since the history sync starts right after the `Welcome`.

  While bulk:
  - No `message-added` / `message-updated` events go out.
  - The attachment index and auto-fetch still run.
  - State changes are deferred.

  After 400 ms without messages, one refresh of everything for that conversation is emitted, plus one `snapshot` if anything was suppressed. The snapshot makes the UI drop and reload its message pages (actions.ts `applySnapshot`).
- **Measured in `history_sync_is_coalesced`:**
  - Joining a 300-message history emits no `message-added`. Then one `channels-updated` (with the unread dot), one `members-updated`, one `server-updated` and a `snapshot` follow; the test requires each of them.
  - A 100-message catch-up after a restart emits no `message-added` and ends in a snapshot. The count passes with room under the 40 the test allows.
- **Trade-off:** a live message in the first second after a join reaches the UI through that snapshot, not as `message-added`.
- **Lag:** if the network receiver lags, everything is marked dirty and a `snapshot` is emitted.
- **Timers:** the only one is the debounce/quiet deadline, armed while something is pending. The UI broadcast holds 1024 events.

## Files

- **Send:**
  - `send_message` attachments are absolute paths: at most 10 files, 25 MiB each, no folders.
  - Each is imported (`Files::import`, TryReference) and becomes `Attachment { hash, size, name (file name, at most 255 chars), mime by extension }`.
  - The hash is indexed with me as the author and shared with the conversation it was sent to, which also announces it when that is a server.
- **Hash index** (`index.rs`, kv `app/att/<hash>`, a cache of 10 000 hashes that evicts the oldest):
  - Every message the app sees adds a reference (conversation, author, declared name, size and MIME). At most 8 per hash and 2 authors per conversation; past 8 the oldest DM reference goes first (anyone can open a DM; joining a server takes an invite), else the oldest. Nothing is ever cleared all at once.
  - A download asks `who_has` in every conversation that references the hash and tries every author. Name and size shown for hash-only commands come from the first reference. `download` refuses a hash whose smallest declared size is over 25 MiB.
  - A reference grants no access (see Blob access).
  - A new reference to a hash whose download is parked queues a holder lookup: at once for a server reference, under the 10 s throttle for a DM reference.
- **Size caps:** tincan-files refuses an attachment over 25 MiB and a clip over 5 MiB as soon as the real size is known (`TransferState::TooLarge`, shown as `failed`), and drops its tags so the partial data is freed. The app records the refusal (`app/refused/<hash>`). Declared sizes over the cap are never queued (auto-fetch, `download`, resume).
- **Auto-fetch:**
  - Incoming attachments that declare at most `autoFetchLimit` (default 25 MiB, 0 = off, never past 25 MiB) and that are not mine, not held, not requested and not refused are pushed on `AutoQueue` (1024 hashes). It is served newest first and drops the oldest when full: a history sync streams the oldest messages first, and the newest are on screen and the likeliest to lose their holders.
  - One fetch worker, one item at a time. Holder lookups (`Peers`), parked-download retries (`Requery`) and resumes go through a separate bounded channel (1024) that the worker serves before any auto-fetch.
  - For each item the worker runs `who_has` in each referencing conversation, then `files.download(hash, Attachment, providers)`.
  - Provider order: online authors, then the other holders, then offline authors last (a dead first provider costs the 4 s connect timeout).
  - History attachments are auto-fetched too (architecture.md: every member keeps attachments available).
- **Parked downloads:** an `unavailable` download is re-queried when rosters change or a DM link comes up. The 10 s per-hash throttle is checked before queueing (`requery_due`), and only providers online now are added (`online_only`); with none, the download is not woken.
- **Resume:** `partial_downloads()` is read at start. The first time any server reaches member, hosting or solo, or a DM link comes up, each entry is queued with its indexed references. Entries the queue has no room for wait for the next connection. An entry without references (for example a clip) asks every conversation's `who_has`.
- **Lost file events:** when the pump's file receiver lags, `on_files_lagged` finishes the shares of downloads that completed meanwhile (`Files::provided_by` names who delivered them), re-announces, re-reads every tracked transfer and open download record through `on_transfer` (which fixes the records and the UI), and finishes clip pulls whose blob is complete.
- **`transfer`:**
  - `idle` when tincan-files has no record and the blob is not held.
  - `peers` counts online holders from the last `who_has` (plus authors when online), never me.
  - If a hash has no holder information yet, the worker looks it up and emits `transfer-updated`.
- **`download`:** errors with "This file isn't available on this device." for a hash the app never saw, and "This file is larger than 25 MB, so Tincan won't download it." for one over the cap.
- **`open_file`:** see Blob access below for the open or reveal rule.
  - Exports to `downloadDir` (default `%USERPROFILE%\Downloads\Tincan`) under `safe_file_name`. If the name is taken, it uses `name (n).ext`. A previous export that still exists with the right size is reused.
  - Writes the Mark of the Web on the exported file every time (new or reused export).
  - With `TestOptions::no_devices` it exports and marks but never launches a program.
- **URLs:**
  - `file_url` returns `file_url_base + hashHex` once the blob is complete.
  - `file_for_url` reads at most 25 MiB and hands the store's buffer over without a second copy (`Vec::from(Bytes)`). The MIME is sniffed from magic bytes for common images, else the type of the cleaned file name if that is a raster image, audio or video (never the peer's declared type). Everything else, SVG and HTML included, is served as `application/octet-stream`, so nothing runs script on the app origin.

## Blob access

Admitted members and DM contacts are untrusted, and hashes are not secret: tincan-net sends one `Have` list to every host and DM peer, and anyone who can post can name any hash in a message. So knowing or naming a hash grants nothing.

- **The rule** (`Access::allow_blob`, asked by tincan-files for every get and observe request): a peer may fetch hash H from me only if H is *shared* with a conversation that peer is in right now: a current member of that server, or the peer of that DM. A peer must also be in at least one of my conversations to open a blobs connection at all (`allow_peer`). Get requests for child ranges (hash sequences) and get-many requests are refused in tincan-files, so an allowed blob cannot be used to pull others.
- **What shares H with conversation C** (persisted in the index record, at most 16 per hash):
  - I sent H in C.
  - I played clip H in C's voice room or call (`clip_play` shares before announcing).
  - I downloaded H and the peer that delivered the last verified bytes (`FileEvent::Completed.from`) was named by C: an author of a reference to H in C, or a holder C's host listed in `who_has` (`Core::nominations`, kept per download in flight). Clip pulls nominate the player and the holders of the session's conversation.
  - Someone reposted H in C while they may already fetch H from me: they could pass the bytes on anyway.
- **What does not:** a message in C that only references H. This is stricter than "a hash belongs to every conversation that references it", which would let a member of server S post the hash of a DM file in S and then fetch it from me.
- **Announcements:** `Core::announce` sets the `Have` list and the clip library announcement to held blobs that are shared with a server I am in. A blob shared only with DMs is never announced (DM `who_has` always names the contact). tincan-net still sends that one list to every host and every DM peer, so a server's host also learns hashes shared with my other servers; it cannot fetch them. Per-conversation announcements need a tincan-net API.
- **Known gaps:** a host is trusted to list real holders, so a host that also references H can get a blob I download from the holder it named shared with its server. A download whose bytes came from two providers is credited to the last one.

### Open or reveal (`shell::open_action`, unit-tested)

`open_file` decides on the final exported name (after `safe_file_name` and `unique_path`, never the peer's raw name) and the file's first 16 bytes:

- **Opened with the default app:** `pdf`, `txt`, and audio and video (`mp3`, `wav`, `ogg`, `oga`, `opus`, `flac`, `m4a`, `mp4`, `m4v`, `webm`, `mov`), plus the images `png`, `jpg`, `jpeg`, `gif`, `webp` and `bmp` when the sniffed type matches the extension.
- **Revealed** with `explorer.exe /select,"<path>"` (passed as a raw argument; normal quoting would make it one token): everything else (programs, scripts, shortcuts, HTML applications, archives, office files, unknown types), an image whose bytes are not that image type (a GIF polyglot named `.png`), and any file whose Mark of the Web could not be written (FAT and exFAT drives have no streams).
- **Mark of the Web:** every export gets a `Zone.Identifier` stream with `ZoneId=3`, so SmartScreen, Attachment Manager and Office Protected View treat it as downloaded from the Internet, also when the user opens it later from Explorer.
- **Export names** (`safe_file_name`): no directories; control and hidden format characters dropped (U+200B-U+200F, U+202A-U+202E, U+2060-U+2069, U+061C, U+FEFF); whitespace runs, no-break spaces included, fold to one space; Windows reserved characters become `_`; at most 200 characters, cut from the stem so the last extension (1 to 16 characters, no spaces) is kept; trailing dots and spaces trimmed after the cut; Windows device names (CON, PRN, AUX, NUL, CONIN$, CONOUT$, COM0-9, LPT0-9 and the superscript 1-3 forms) get a `_` prefix; an empty result is `file`.

## Peer text

- **Names in DTOs:** peer display names (members, authors, DM and call names, typing, voice participants, `sound-played`), contact names, server and channel names, attachment names and clip names have control characters and the hidden format characters above removed (`shell::clean_text`, `views::display`). Attachment names also fold whitespace runs (`views::file_display`), like the export. The attachment MIME in the DTO is the type of the cleaned file name, not the sender's claim, so the UI's icon and image preview match what `open_file` will do. Stored names stay as published, because commands compare against them.
- Removing U+200C and U+200D also splits emoji joined with ZWJ and changes some Persian and Indic text. The task asked for the whole range.
- **Host refusals** (`net_err`): `NetError::Rejected` text that came from a host is shown as `The host refused this: "<text>"`, with hidden characters removed, whitespace folded and the text cut to 200 characters. Exact matches of the sentences tincan-net writes itself and of an honest host's join refusals (invite not valid, revoked, used up, clock checks) are shown as they are. A tincan-net variant for peer text would replace the string match.

## Settings

- `settings_set` validates the input:
  - Volumes must be finite; they are clamped to 0..2.
  - `autoFetchLimit` is clamped to 25 MiB.
  - `downloadDir` must be an absolute path (an empty value means the default).
  - `customRelayUrl` must be http(s) with a host; an empty value means null.
  - Empty device ids become null.
- It then persists the document and applies what it can:
  - `allowHosting`: `net.set_allow_hosting`, live.
  - `autoFetchLimit` and `downloadDir`: read on use.
  - `customRelayUrl`: needs a new endpoint, so a toast says "Restart Tincan to use the new relay setting." (under `offline_network` the toast says relays are off).
  - Devices, volumes (0..2 maps to the engine's gain directly), noise suppression and echo cancellation go to `Media::apply_settings`, which applies them to a running engine at once (`set_devices`, `set_input_gain`, `set_output_gain`, `set_processing`). An engine created later reads the current settings. Under `no_devices` the device ids are ignored.
  - `theme` is the UI's.

## Names and DMs

- **`set_name`:**
  - Validates 1 to 100 characters and persists the name.
  - Calls `net.set_display_name`.
  - For each server where my Profile name differs, publishes `Body::Profile` in a background task (one in flight per server), then emits `snapshot`.
  - The same check runs whenever a server becomes connected, so a rename made while offline still reaches each server.
- **Contacts:**
  - `add_contact` accepts `tincan1u...` or a bare z32 id (`parse_user_code`), then runs `open_dm`.
  - `open_dm` calls `net.dm_open`, saves a contact record and adds the DM to the access state: the peer may open a blobs connection, and fetches only what is shared with that DM.
  - An incoming DM from a stranger arrives as `DmOpened` and gets the same treatment.
- **`DmSummary.name`:** in order, a server display name, the contact record, or the first 8 characters of the z32 id. DMs carry no profile, so a contact you share no server with stays a short id.

## Media

`src/media/` holds `Media` (one per `Core`). The dispatcher routes the 21 media commands to `Media::call`; `App::screen_watch` goes to `Media::screen_watch`.

| Hook | Called from |
|---|---|
| `Media::start` | `Core::start`, after `Core` exists: loads per-user volumes and pending clip pulls, announces the clip library |
| `Media::on_net_event` | the pump: `VoiceUpdated`, `KeyframeRequested`, `TargetBitrate`, `SoundPlayed`, `LossReport`, `CallInvite`, `CallUpdated`, and also `RosterUpdated` and `ConnectionChanged` |
| `Media::on_lagged` | `Pump::lagged` |
| `Media::on_file_event` | every `FileEvent` (clip pulls) |
| `Media::apply_settings` | `settings_set` |
| `Media::voice_snapshot` / `call_snapshot` | `AppSnapshot.voice` / `.call` |
| `Media::is_speaking` | `Member.speaking`, `VoiceParticipant.speaking` |
| `Media::shutdown` | first step of `App::shutdown` |

Lock order inside media: `st` (the state mutex), then the engine slot, then the watcher map. Nothing in media calls `Core::refresh` or `Core::snapshot` while holding `st`, because those call `is_speaking` and `voice_snapshot`. No media lock is held across an `.await` (the public futures must stay `Send`).

### The audio engine (`engine.rs`)

- One `tincan_audio::AudioEngine` per profile, created on first use (`voice_join`, a call, `mic_test`) from the current settings, mute state and volumes. It parks with no work when idle, so it is kept until shutdown.
- `TestOptions::no_devices`: `AudioIo::Custom` with a synthetic microphone (silence, or speech-like bursts after `App::test_synthetic_voice(true)`) and `NullSink`. No device is opened.
- Engine events, mapped by a forwarder task:
  - `Speaking { track }`: user via the session's short-id map (`track_short_id`; roster short ids in rooms, the call short ids in calls), then `speaking`.
  - `LocalSpeaking`: `speaking` for me.
  - `MicLevel`: `mic-level`.
  - `DeviceError`: an error `toast`.
- Neither `speaking` nor its changes send `members-updated` or `voice-updated`. Both DTOs read the current flags, so the UI's speaking store stays right when they are sent for other reasons.
- **Engine death:** the engine's event sender lives only on its thread, so the forwarder seeing the channel close outside shutdown means the thread died (a panic unwound it).
  - The app drops the dead handle, lights every speaking ring off, starts a fresh engine and, if a session is attached, calls `start_call` again with the kept `audio_out`, local track and epoch. A toast says "Audio stopped working and was restarted."
  - At most 3 restarts per 60 s; after that the toast asks to rejoin voice or restart Tincan.
  - The audio pump looks up the current engine per batch, so received packets go to the new engine.
  - `test_crash_audio_engine` makes the synthetic microphone panic on the engine thread; `voice_room_audio_flows_both_ways` checks the restart and that audio keeps flowing.
  - Malformed Opus input, two unit tests:
    - `opus_decoder_survives_malformed_packets` drives the engine's decoder (`LibOpus`) directly with 6000 payloads: random bytes, and real packets bit-flipped, truncated or extended. `decode`, `decode_fec` and `conceal` returned errors or audio, never a panic, and the output stayed finite.
    - `garbage_packets_do_not_kill_the_engine` is a smoke test of the engine path (routing, jitter buffer) with 1200 random payloads in about 1 s. The engine plays one frame per track per 20 ms, so it decodes only a fraction of them.
  - unsafe-libopus is translated C, so a hostile packet is more likely undefined behaviour than a panic. The restart covers panics only.
- **Clip decoding:** `decode_clip` runs `AudioEngine::decode_clip` (max 20 000 ms) on a blocking thread under `catch_unwind`. A panic counts as "not a sound". Every decode path uses it: `clip_add`, a pulled clip, and playing a clip whose PCM is not cached.

### Sessions (`session.rs`)

- `voice_join` / `call_accept` / `call_start` get a `MediaSession` from net. Attaching it:
  - `engine.start_call(session.local_track, session.epoch, session.audio_out)`;
  - a task pushes `audio_in` into the engine (`recv_many`, batch 32);
  - a task delivers `video_in` to the `screen_watch` sinks (`ui_video_message`);
  - `files.set_upload_limit(256 KiB/s)` (128 KiB/s while I share). No uplink measurement exists, so the "50% of uplink" rule is a fixed cap.
- Detaching stops the engine's call side and clears speaking flags, watchers, my share, the heard clips (`clips-updated`), the PCM cache and the upload limit.
- **`reconcile`** decides attach and detach from `Net::voice_info()` and `Net::call_info()`, because net emits `VoiceUpdated` / `CallUpdated` before the command gets its session back. It runs on `VoiceUpdated`, `CallUpdated`, a lagged stream and after every session command.
  - An attached room is stale when `voice_info` names another conversation or channel. An attached call is stale unless `call_info` is active with the same peer (keyed by peer: glare rewrites the call id).
  - Session commands increment `switching`; while it is non-zero, reconcile does not detach (no `voice-updated: null` flicker between leaving and joining).
  - It also follows host migrations: `set_epoch` and `set_relayed` on change (`VoiceInfo.epoch` / `relayed`). The short-id map is refreshed from the roster.
- **Calls:**
  - `call_start` returns the ringing `CallInfo`; its session waits in `pending_call` and is attached by reconcile once the call is active. If net returns an already active call (they were ringing me), it attaches at once.
  - The peer's short id is `0` when the peer's id is lower than mine, else `1`.
  - `call-updated` carries the `CallInfo` while ringing or active and `null` once ended, as the mock does. A notice is shown when it is news: "X declined the call.", "X didn't answer.", "Missed call from X.", "The call with X was lost."
  - `call_decline` with an unknown id returns null.
- **`VoiceSession`:**
  - Rooms: participants are me (local mute, deafen and share state) plus the roster entries in my room, sorted by name. The roster is used only while the server's connection is member, hosting or solo.
  - Calls: participants are me and the peer, with `peer_muted`, `peer_deafened` and `peer_sharing`.
  - `path` is `none` until media flows, then `relay` or `direct` (a member uses its connection's path). `Connection.path` is `None` while hosting, which the UI would show as "Connecting".
  - `rttMs` is the member's host RTT. It is null when hosting and in calls (net exposes no call RTT).
  - `voice-updated` is sent when the JSON changes: on `VoiceUpdated`, roster and connection changes of my server, `CallUpdated`, mute or deafen changes, share changes.
- **Mute and deafen** follow `actions.ts`: unmuting clears deafen, deafening sets muted, undeafening leaves muted. They go to the engine (`set_muted`, `set_deafened`) and to `net.set_status`. The state persists across sessions within a run.
- **`set_user_volume`:** 0..2, persisted in kv `app/user_volumes`. It is applied as `set_track_volume(track_id(short, KIND_VOICE))` whenever the session's short-id map changes. Members who leave the room get `remove_track`.
- **`audio_devices`:** `AudioEngine::devices()` on a blocking thread; empty lists with `no_devices`.
- **`mic_test`:** `engine.mic_test(on)`; the engine sends `mic-level` about 20 times per second.

### Soundboard (`clips.rs`)

- **Library:** the store's `clips` table plus `BlobKind::Clip` blobs. Clips are announced with `net.set_clips` at start and after every add, remove, rename, pull or play, so the host's holder index lists me. Only complete clips shared with a server I am in are announced: a clip is shared with a server when I play it in one of its voice rooms, or pull it there (Blob access).
- **`clips` and `clips-updated`:** the library (`owned: true`), then the clips heard in this session that I don't own (`owned: false`, `heardFrom`, `transfer` while pulling). At most 50 heard clips; the list is cleared when the session ends.
- **`clip_add`:**
  - The name is trimmed, 1 to 32 characters (the UI's limit); an empty name uses the file stem.
  - The file is refused when it is a folder or larger than 5 MiB.
  - It is decoded (max 20 s), then imported with `import_bytes` (copied: the user may move the original), then saved.
  - Errors: "X is longer than 20 seconds. Pick a shorter sound.", "X is larger than 5 MB. Pick a smaller sound.", "X isn't a sound Tincan can play. Pick an MP3, WAV, OGG, FLAC or M4A file.", "Couldn't read X. Check that the file still exists."
  - Adding a clip that is already in the library returns the existing entry.
- **`clip_play`:**
  - Not in voice gives `not-in-voice`; deafened gives `deafened` (checked before and after decoding).
  - A clip not in the library: "Save that sound before playing it."
  - The PCM comes from the cache (at most 4 clips or 2 M samples, dropped on leaving voice) or a fresh decode.
  - `engine.play_clip`, then the clip is shared with the session's conversation (the server of the room, or the DM of the call), then `net.sound_played(ClipInfo)`. Playing works while muted.
- **Incoming `SoundPlayed`:**
  - Only for my attached session.
  - Announcements over 5 MiB or about 20 s are ignored. Peer names are cleaned of control and hidden format characters and cut to 32 characters.
  - Emits `sound-played { userId, name, clip }`; `clip.owned` says whether it is in my library. A clip I don't own is remembered as heard.
  - The clip is **not** played locally: the player's engine already mixes it into its voice stream (`AF_SOUNDBOARD`), so a local copy would play it twice.
- **`clip_fetch`:**
  - Records the pull (kv `app/clip_fetches`, so it survives a restart), then `files.download(hash, Clip, [player, ...who_has(conv)])`.
  - A blob already held as an attachment is tagged as a clip instead of downloaded.
  - Progress and states re-send `clips-updated`. tincan-files stops a clip download past 5 MiB (`TooLarge`); the app then forgets the pull and says so.
  - On `FileEvent::Completed { kind: Clip }` for a recorded pull, the blob is read and decoded. If it is valid it goes into the library and is announced, so I serve it while I keep it. Otherwise the clip tag is dropped and a toast says so.
  - `Completed` events of clips that were not pulled are ignored, because `clip_add`'s import fires one before its row exists.
- **`clip_remove`** drops the row and the clip tag (an attachment tag keeps the bytes). **`clip_rename`** errors with "That sound was removed." for a missing clip.

### Screen share (`screen.rs`)

- **`screen_sources`:** `tincan_screen::sources()` (inside tokio). With `no_devices` it returns only the synthetic source `test:pattern`, plus the real ones after `test_allow_screen_capture`.
- **`screen_start`:**
  - Needs a session (`not-in-voice`).
  - `hardware_encoder_available` is asked once on a blocking thread; without an encoder the error is `no-hardware-encoder`.
  - A running share is stopped first. Then `net.share_start()` and `ScreenShare::start(source, mode, frames)`.
  - Start errors: `no-hardware-encoder` from `Error::code`, else short sentences (closed or minimized window, permission, Windows version).
  - Returns my screen track.
  - The share's `ended()` is watched. A closed window gives an info toast ("The shared window was closed, so screen sharing stopped."), a failure an error toast. A watch error (the thread died without a reason) counts as a failure.
  - `KeyframeRequested` and `TargetBitrate` go to the single active share (its track id can change with the short id).
- **Synthetic pattern (`no_devices`):** 15 fps of Annex-B-shaped access units (AUD, SPS, PPS, IDR for keyframes), a keyframe every 60 frames or on request, `try_send` with a forced keyframe after a drop. It exercises the same app path without a GPU.
- **Stopping:** every `ScreenShare` is stopped on a blocking thread (`stop` and `Drop` join the capture thread): on `screen_stop`, a replaced share, detach, an ended share and shutdown.
- **`screen_watch`:**
  - Needs a session. The track must be shared in it right now (the roster's `sharing` in my room, the call's `peer_sharing`, or my own share), else "That screen share has ended.".
  - This refuses the stale GOP cache the host keeps for a stopped share.
  - A sink replacing another one for the same track resubscribes, so it starts at a keyframe.
- **Watchers end** on `screen_unwatch`, when the sink returns false (then `unsubscribe_track`), when the track leaves the roster or `peer_sharing`, and on detach.

## Test hooks (`#[doc(hidden)]`)

- `App::handled_commands()` and `App::event_names()`, for the contract test.
- `App::test_addrs()`: the loopback addresses of the instance's IPv4 sockets.
- `App::test_add_addr(user_z32, addrs)`: calls `net.add_addr_hint`, which also wakes waiting DMs and sessions.
- `App::test_synthetic_voice(on)`: the `no_devices` microphone sends speech-like bursts (600 ms on, 400 ms off) instead of silence, so speaking flags and packets flow.
- `App::test_crash_audio_engine()`: the `no_devices` microphone panics on its next read, killing the engine thread.
- `App::test_allow_screen_capture()`: real screen capture despite `no_devices` (audio stays synthetic). Only the ignored hardware test uses it.
- `App::test_media_counters()`: `{ audioIn, videoOut, engineRestarts, engineRunning }` (audio datagrams handed to the engine, video messages delivered to sinks).
- `App::test_fetch_from(hash, user, addrs, secs)`: fetch a blob straight from a peer over the blobs protocol, as a modified client could (no message, index or holder lookup).
- `App::test_post_attachment_ref(conv, channel, hash, name, size)`: publish a message whose attachment claims a hash without holding the bytes.
- `App::test_pause_file_events(paused)`: the pump drops every file event while paused; resuming runs the lag recovery.

Joins need no hook, because invites carry the inviter's loopback address.

## Tests

- `cargo test -p tincan-app` runs the unit tests in each module, `tests/e2e.rs` and `tests/media.rs`. Cargo runs the two test binaries one after the other; inside each, tests are serialized with a mutex because each uses real protocol timers. The whole run takes 45 to 60 s (e2e about 17 s, media 28 to 41 s).
- `create_join_chat_and_page` (a):
  - The snapshot shape.
  - create, invite and join; members online; `server-updated` with the host.
  - Messages both ways as `message-added`; unread and `mark_read`; `typing`.
  - 9 messages paged 3 at a time (3 pages, oldest first).
- `host_leaves_and_the_rest_continue` (b): the host shuts down; both others report the same new host and keep chatting.
- `attachment_outlives_its_uploader` (c):
  - Attachment errors; A sends 300 kB and B auto-fetches it.
  - A shuts down and B hosts solo; C joins with B's invite.
  - C sees `peers == 1`, downloads it, and gets identical bytes from `file_for_url`.
  - `open_file` exports once into a temp folder, with `ZoneId=3` in its `Zone.Identifier` stream, and marks a reused export again.
- `blobs_are_served_only_where_they_are_shared` (c2):
  - A server member cannot fetch from Bob a file Alice and Bob exchanged in a DM, also after posting a message in the server that references its hash. The server file is served to her on the same path (control).
  - A stranger who DMs Alice cannot fetch her server file, also after referencing it in the DM. A file Alice sends him in the DM arrives (control).
  - An attachment declaring 30 MB is not auto-fetched and `download` refuses it; its name loses the RLO and its MIME follows the cleaned name.
- `a_new_reference_wakes_a_parked_download` (c3): a stranger names a hash in a DM and never serves it, so Bob's auto-fetch parks. When Alice posts the file in their server, the download completes with no roster change and no click.
- `lost_file_events_are_recovered` (c4): Bob's pump drops file events while his auto-fetch completes, so the file is not shared; after the lag recovery the UI gets `complete` and Carol can fetch it from Bob.
- `direct_messages_and_offline_delivery` (d):
  - `add_contact` by code; DMs and DM typing both ways.
  - B shuts down; A's message is pending. B restarts on the same profile, gets the message, and A gets `message-updated` with `pending: false`.
  - The name and the DM list survive the restart.
- `owner_changes_and_deletes_propagate` (e):
  - A later `set_name` reaches the other member.
  - `rename_server`, create, rename and delete channel, and `delete_message` propagate.
  - Owner-only and own-message errors; `leave_server` gives a `snapshot` without the server.
- `history_sync_is_coalesced`: the bulk measurements above.
- `public_futures_are_send` (type-checked only): the futures of `start`, `call`, `screen_watch`, `file_for_url` and `shutdown` are `Send`, as the shell spawns them on Tauri's runtime.
- `profile_is_locked_while_running`: the lock, idempotent shutdown, restart.
- `contract_matches_types_ts` (f):
  - Parses `ui/src/api/types.ts` at test time.
  - Every command except `screen_watch` / `window_*` is handled, and every Events key is emittable and vice versa.
  - Every command called with `{}` never returns "Unknown command".
  - Settings keys equal the TS interface and round-trip across a restart.
  - Media without a session: `clips` is `[]`, `audio_devices` is empty, `clip_play` and `screen_start` give `not-in-voice`, `screen_watch` fails.

`tests/media.rs` (`no_devices`, `offline_network`, real timings):

- `voice_room_audio_flows_both_ways` (a):
  - Two members in the default voice room; `voice-updated` on both lists both with a path.
  - Each gets `speaking` for the other, and each engine receives at least 20 packets in 2 s.
  - The snapshot carries `voice`; `members` shows `voiceChannelId`.
  - Deafen and unmute reach the other side as `voice-updated` with the UI's mute model; `set_user_volume` works and validates.
  - A crashed engine thread is restarted (toast, `engineRestarts == 1`). After it, Alice gets a fresh `speaking` for Bob (the new engine decodes) and Bob keeps receiving packets (only Alice's new engine sends to him).
  - `voice_leave` gives `voice-updated: null` here and a one-person session there; the mic test emits `mic-level`.
- `voice_resumes_after_the_host_leaves` (b):
  - Three in voice; the host shuts down.
  - Both others get `voice-updated` with just the two of them and a path.
  - Once the new host is known, their snapshots' `voice` says the same.
  - After 1.5 s of silence (rings off, old events dropped), both get a fresh `speaking` for the other, and both engines keep receiving packets.
- `call_rings_connects_and_hangs_up` (c):
  - `call_start` returns `ringing-out`; the callee gets `call-updated` `ringing-in` (also in its snapshot).
  - `call_accept` returns a `call` session with both; the caller gets `active` and `voice-updated`.
  - `speaking` and audio both ways.
  - `call_hangup` gives `call-updated: null` and `voice-updated: null` on both sides (in either order: net emits `VoiceUpdated` first).
  - A second call is declined and the caller gets "... declined the call."
- `soundboard_clips_travel_between_members` (d):
  - `clip_add` refuses a 25 s WAV, a 5 MiB + 1 byte file, 30 kB of noise named `.mp3`, a missing file and a 40-character name, all without a panic.
  - A 2 s WAV is added (about 2000 ms).
  - `not-in-voice` outside voice, `deafened` while deafened; it plays while muted.
  - B gets `sound-played` (owned false, heardFrom A), the clip in `clips`, and audio; `clip_fetch` ends in `clips-updated` with it owned, same name and duration.
  - A shuts down; C joins with B's invite and joins voice. B plays, C pulls it from B (the only holder).
  - Rename and remove work; renaming a removed clip errors.
- `screen_share_reaches_a_watcher` (e):
  - `display:0` is refused under `no_devices`; the synthetic pattern starts and B's `voice-updated` shows A's `screenTrack`.
  - Watching an unshared track fails.
  - B's sink gets at least 20 raw messages: the first a keyframe with the right track and an Annex-B start code, timestamps increasing.
  - `screen_stop` clears `screenTrack` and drops B's sink.
  - Re-sharing in motion mode works, `screen_unwatch` drops the sink, and leaving voice ends the share.
- `real_screen_share_of_the_primary_display` (`#[ignore]`, run with `cargo test -p tincan-app --test media -- --ignored real_screen_share`):
  - Shares `display:0` for 2 s through tincan-screen; with no encoder it checks `no-hardware-encoder` instead.
  - Run once on this machine (hardware encoder present, 3440x1440 primary display): `screen_start` took 261 ms, and B's sink got 32 messages in 2 s (2 keyframes, 285 kB), the first an IDR with SPS `avc1.640028`.

Unit tests in `media/`:

- `decode_limits_and_garbage`: 2 s WAV ok, 21 s refused, noise refused, truncated input does not panic.
- `opus_decoder_survives_malformed_packets` and `garbage_packets_do_not_kill_the_engine` (above).
- Clip names, the bounded PCM cache, the bounded heard list.
- The synthetic access units parse as Annex-B keyframes and deltas.
- Screen errors map to the api.md strings.

## Decisions

- **Join window:** a live message during the join window reaches the UI through the refresh `snapshot` instead of `message-added` (no flood, slight delay).
- **History attachments:** auto-fetched like live ones, one at a time through the worker.
- **`no_devices`:** doubles as "headless" for `open_file` (no program is launched).
- **Name collision:** the module is `app.rs`, not `core.rs`, so that `crate::core` does not shadow the `core` crate in macro expansions.
- **Restarting networking:** no in-process restart for a new relay URL; a toast asks for a restart instead.
- **Heard clips are not replayed:** listeners already hear a clip inside the player's voice stream. Playing it again locally (as a literal reading of the task would do) would play it twice, the copies offset by the network and jitter-buffer delay (not measured).
- **`call-updated: null` on end**, with a notice only when the end is news (declined, no answer, missed, lost).
- **Fixed upload cap in voice** (256 KiB/s, 128 KiB/s while sharing) instead of "50% of measured uplink": nothing measures the uplink.
- **The synthetic screen pattern** (`test:pattern`) exists only under `no_devices`, so the share and watch path is tested without a GPU.
- **One engine per profile, lazy:** created on first voice use and kept (it costs nothing parked). It is replaced only after its thread dies.

## Real-app end-to-end run

[`docs/e2e.md`](e2e.md) covers running the real `tincan.exe` as two profiles, driven through the UI over CDP (flows: join, messages and typing, attachment preview, DMs, voice and soundboard, sharing a window, host migration, idle resources). The script is in `ui/e2e/`. The crate and the shell needed no glue changes. The run turned up three fixes outside this crate:

- `ui/src/store/actions.ts` `putMessage`: `pending` and `deleted` only move one way. On loopback the DM ack (`message-updated`, `pending: false`) arrives before the `send_message` result, which used to bring "Waiting for ..." back.
- `ui/src/store/actions.ts` `markSeen`: another member's message that lands in the conversation on screen is marked read at once, then once more at the end of a burst if it is still on screen (at most once per second per conversation). Before this, leaving the view within the 100 ms unread debounce left the conversation unread for good.
- `src-tauri`: `TINCAN_LOG=<EnvFilter>` sends tracing output from every crate to stderr. With it unset, no subscriber is installed. The e2e run uses `warn,tincan_audio=info`, which logs device open and close.
