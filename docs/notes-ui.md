# UI notes (ui/)

Vite 8 + React 19 + TypeScript 6.0 app. `pnpm install`, `pnpm dev` (port 5173, strict, COOP/COEP headers), `pnpm build` (tsc --noEmit then vite build to `ui/dist`).

## What src-tauri must provide

- `build.devUrl = "http://localhost:5173"`, `build.frontendDist = "../ui/dist"`, `beforeDevCommand = "pnpm --dir ../ui dev"`, `beforeBuildCommand = "pnpm --dir ../ui build"`.
- Window `decorations: false`. The UI draws its own 32 px title bar with `data-tauri-drag-region="deep"`; Tauri's drag script then drags on mousedown and toggles maximize on double-click. That needs `core:window:allow-start-dragging` (not in `core:window:default` in tauri 2.12.7; `internal-toggle-maximize` is).
- `app.security.headers` with `Cross-Origin-Opener-Policy: same-origin` and `Cross-Origin-Embedder-Policy: require-corp` (same as the Vite dev server).
- `tauri-plugin-dialog` registered, capability `dialog:allow-open` (attachments, clip files, download folder picker).
- Capabilities used by `@tauri-apps/api/window`: `core:window:allow-is-maximized`, `core:window:allow-is-minimized`, plus `core:event:default` (resize and focus listeners). The title bar buttons call the `window_minimize` / `window_toggle_maximize` / `window_close` commands, not the window API.
- `file_url` must return a same-origin URL (asset protocol or custom scheme): with COEP `require-corp` a cross-origin image without CORP will not load.
- `screen_watch.onFrame` is a `Channel<ArrayBuffer>`; send `InvokeResponseBody::Raw` with `tincan_core::media::ui_video_message`.

## Tests

Pure UI logic has dependency-free tests: from `ui/`, run `node --experimental-strip-types --no-warnings src/test/run.mjs` (Node 22.6 or later). `src/test/run.mjs` loads every `*.test.ts` under `src` with Node's type stripping (a resolve hook adds the `.ts` that Vite-style imports leave out) and runs what they registered with `src/test/harness.ts`. A tested module may only have type imports: no React, no `api/client`, no `.tsx`, no enums or parameter properties. That is why the decisions live in small modules:

| Module | Rules |
|---|---|
| `store/calls.ts` | which command Leave voice / Hang up sends, which parts the call panel shows, where the per-user volume slider starts |
| `store/messages.ts` | message cache merges (first page, live events, older pages, snapshot refresh, off-screen cap) |
| `store/typing.ts` | the typing list and its sweep timer (timers injected) |
| `store/transfers.ts` | when a command's transfer result may replace the store's, which transfers a snapshot re-reads |
| `store/clips.ts` | heard clip cap, the clip picker's formats |
| `lib/names.ts` | short ids, name clashes and the lookalike fold |
| `lib/compose.ts` | the composer box around a send |
| `lib/scroll.ts` | the load-older scroll anchor |
| `dialogs/settingsLogic.ts` | auto-download choices, Settings > Network status, the volume slider debounce |
| `media/screenLogic.ts` | per-track watch/unwatch ordering, decoder fallback, backpressure |

`package.json` has no `test` script yet (it is outside `ui/src`).

## Mock backend

Used automatically outside Tauri (`window.__TAURI_INTERNALS__` missing). Loaded with a dynamic import, so the production bundle only fetches it in a browser.

URL flags: `?fresh` (name picker, no servers), `?noenc` (screen_start fails with `no-hardware-encoder`), `?ring` (incoming call from Alex), `?noanswer` (outgoing calls ring for 45 s, then "X didn't answer."). Console helpers on `window.tincanMock`:

- `ring(name)`, `online(name)`, `offline(name)` (Riley coming online delivers pending DMs), `migrate()` (Game night host moves to Morgan), `toast(text)`.
- For scripted checks: `state()` (the mock's voice and call), `addMember(name, server?)` (a name clash), `soundFlood(count)` (heard clips nobody owns), `snapshot()`, `delay(command, ms, reply?)` (latency before the command runs, or with `reply` after it ran, so events sent meanwhile arrive before its result), `post(name, text, channel?)` and `typing(name, channel?)` (Game night channels), `finishQuietly(fileName)` (a download completes with its final event dropped).

The mock follows the app where it matters to the UI: a hosted server has `path: "none"` and no RTT, `settings_set` clamps volumes to 0..2 and `autoFetchLimit` to 25 MB, `clip_add` takes MP3, WAV, OGG, FLAC and M4A only, `messages` pages are at most 200, and participants carry `volume` (Jordan is saved at 30%).

Join errors: codes shorter than 40 chars, codes not starting with `tincan1`, `tincan1u...` contact codes, and codes containing `offline` are rejected with the matching message.

## Message cache (store/actions.ts, store/messages.ts)

- Pages are per conversation (`convKey`). Live `message-added` / `message-updated` are applied to a loaded cache; delivery (`pending` to false) and deletion only move one way, whichever copy arrives first.
- A message that arrives while the conversation's first page is in flight is kept in an unloaded entry and merged with the page by id (one-way rule). A cached message older than the page is dropped unless the page reaches the start, so a stray update never lands at the end of the list.
- `markSeen`: someone else's message on screen marks the conversation read at once (leading edge, then at most once a second during a burst), and only while it is still on screen.
- Snapshot (event-lag rule): pages read before it may have holes, so every cache is dropped except the conversation on screen, which is re-read from the newest message back to its oldest cached one (up to 200 per read, at most 5 more pages). When the read contains that oldest message (or reaches the start of the conversation), the cache keeps it as the first row, with its cursor, and the reading position survives. A catch-up too long to reach it replaces the cache with the first read only (up to 200 messages, like a plain reload), not the up to 1200 rows of the extended read. A re-read that fails drops the kept cache and loads the newest page, as for any other conversation.
- A generation counter (`cacheGen`, bumped by each snapshot after the first) is part of the load keys: a load from before a snapshot never blocks one after it, and its result is discarded. An older page is prepended only to the entry and cursor it was asked for, in the same generation.
- A conversation off screen whose cache reached 300 messages is dropped instead of grown; the next visit loads its newest page.

## Calls and voice

- Leave voice in a room sends `voice_leave` even while a call rings out from it; the ring survives and the call panel shows its own "Calling <name>" row with Cancel call, above the room panel. Only a call (or a ring with no room) is hung up with `call_hangup`.
- An active call with no voice session attached shows "Call with <name>" with Hang up, so it can always be ended.
- Calling someone (DM header or member list) opens the DM with them first: the call stage, their screen share and the per-user volume menu are there.
- The per-user volume slider starts at the volume picked in this session, else `VoiceParticipant.volume`, else 100%. `volume` is optional in `types.ts` until the app sends it.

## Names

Display names are chosen by each peer and are not unique. When two people the UI knows (members of any server, DM contacts) use names that look the same, both are shown with their short id (the first 8 characters of the user id, the app's fallback name) after the name: member list, DM list, DM header, message authors, voice tiles, room lists, incoming call. The member menu and the voice tile popover always show "ID <short id>". Peer names and ids render in `<bdi>`, so a name's bidi controls can't reorder the id, and a long name is cut short with an ellipsis while the id after it stays whole (`.person` is an inline flex row). Nothing changes in text e2e matches unless two names clash.

`nameKey` decides what "look the same" means: NFKD (width, ligatures, math letters), then default-ignorable and format characters (zero-width, bidi controls, variation selectors, Hangul fillers, soft hyphen) and combining marks (accents, overlays) are removed, Cyrillic, Greek, Armenian and a few Latin letters that look like a Latin letter are mapped to it (a UTS #39 confusables subset, mapped before lowercasing so Greek capital eta is H and small eta is n), then lowercase, 0 to o, and 1, l, i, I and | to l, and runs of spaces to one. It folds too much on purpose: two different names that fold together only get an extra short id. My own name is in the index too, so a contact named like me gets the id. Results are cached (bounded, names come from peers).

## Screen share (media/ScreenPlayer.ts)

- A new codec string is probed with `VideoDecoder.isConfigSupported` (prefer-hardware, then no-preference) before it is configured; frames wait, in order, for the answer. Headless Edge reports an unsupported config through the error callback as `OperationError` ("Unsupported configuration"), not `NotSupportedError`, and decodes `avc1.f4...` (High 4:4:4) only without hardware, so the decision never looks at the error name.
- A decoder that fails before its first frame is retried in software; three failures without a frame show "This screen share uses a video format this PC can't decode." Errors after frames rebuild the decoder at the next keyframe. A later codec that decodes replaces the message with the video. Probe results are cached per codec string, at most 32 (the string comes from the sharer's SPS).
- Delta frames are skipped while `decodeQueueSize` is over 8 (the view waits for the next keyframe, at most one GOP).
- `screen_watch` and `screen_unwatch` run one after another per track (`keyedSerial`), so an unwatch never overtakes its watch, and a player only unwatches when it subscribed.
- Minimized (`onWindowHidden`), the player unwatches and closes its decoder; on restore it watches again on a new channel and starts at the host's cached keyframe.

## Choices made where the docs disagree or are silent

- Clip limits shown at the add step: 20 s and 5 MB (architecture.md), not the 10 s / 2 MB in design.md. Text lives in `CLIP_LIMIT_TEXT` (`src/dialogs/SoundDialogs.tsx`).
- Mute and deafen live in the call panel only (design.md also lists a mic button in the user panel; two mute buttons side by side were redundant).
- Opening a voice room joins it when you are not already in a room or a call.
- Voice room chat:
  - The chat button on a room's row (`Open chat for <room>`) opens the chat page (`View.chat`), which never joins voice; clicking the row itself keeps the old behaviour.
  - The room header's "Chat" button toggles a 340px panel beside the tiles (`State.roomChatPanel`, kept across rooms for the session). `.voice-split` has `contain: inline-size` so the tile strip scrolls instead of widening the window.
  - `store/chatView.ts` (`shownChat`, `roomChatMode`) is the one answer to "which chat is on screen". Loading, `mark_read`, `markSeen` and the snapshot cache all use it, so a room's chat is never marked read while only its tiles show. The row's unread state (bold, sr-only "unread messages") and the Chat button's follow the same rule.
  - The member list shows beside a room's chat page, not beside its tiles.
  - Known gap: a full-window stage (`.stage.is-full`) covers the open panel, but the panel still counts as on screen, so messages arriving then are marked read (the DM call stage has the same gap). The soundboard popover floats over the left of the main area and can overlap the panel in a narrow window.
- DM typing works: `typing` takes `channelId: null` for a DM and the app sends `typing` events with `channelId: null`, shown under the DM composer.
- Settings > Network shows "N people connected" for a server this PC hosts (the app reports no path while hosting).
- The auto-download menu stops at 25 MB, the app's attachment limit.
- The composer empties when a send starts. A failed send puts its text back in front of anything typed since; an Enter pressed during a send is sent when it finishes.
- The soundboard keeps at most 50 heard clips, newest last, like the app.

## unslop-ui pass

2026-11-23, unslop-ui with `--surface` over every screen, dialog, menu and media view in `ui/src`, checked against the mock backend with before and after screenshots (headless Edge, dark and light, 1280x800 and 900x640). Verdict: light. The copy and surface already followed design.md: no eyebrows, numbering, arrows, em dashes, gradients, glows, heavy weights or decorative motion. Spacing is on the 4px scale; transitions stay at design.md's 120 ms.

| Rule | Found | Fixed | Kept |
|---|---|---|---|
| U3 subtitle reflex | 1 | | 1 |
| U6 status strings | 3 | 1 | 2 |
| U7 labeling everything | 3 | 1 | 2 |
| U9 badges | 1 | | 1 |
| U10 status dots | 2 | | 2 |
| U11 decorative icons | 3 | | 3 |
| U14 saying it twice | 1 | 1 | |
| U18 off-scale spacing | 1 | 1 | |
| U25 edges | 2 | 2 | |
| unslop 11 synonym cycling | 3 | 3 | |

Changed:

- DM sidebar empty state: the main pane already says "No direct messages yet.", so the sidebar drops "No conversations yet." and keeps "Message someone from a server's member list, or add a contact by their ID." (unslop 11, two names for one fact shown twice).
- Voice room context menu: "Rename channel" and "Delete channel" become "Rename voice room" and "Delete voice room"; the delete dialog's button for a room becomes "Delete voice room" (it said "Delete channel" under "The room is removed for everyone."). Text channels keep "channel" (unslop 11).
- Your own screen on the stage: the "Your screen" label under "You're sharing your screen" is gone (U7). Someone else's share keeps its name label and its canvas name.
- File card while downloading: "48%" next to "8.9 MB of 18.4 MB" and the bar is gone; the bar now runs to the card's edge (U14). A queued download with holders online says "2 people online have it" instead of "queued", since the disabled Queued button says that (U6).
- Add contact: the ghost "Copy my ID" button is set back by its padding so its label starts on the dialog's text line (U25, `.btn-ghost.foot-start`).
- Settings > Network: the rows share one set of columns (`subgrid`), so the host lines start on one edge and the statuses end on one edge (U25).
- Rail unread dot: centred in the 8px gap under the icon (`bottom: -7px`, was 1px off; U18).

Kept on purpose:

- Welcome: heading "Pick a display name" over the label "Display name" (U7). The field keeps its visible label, and the heading is the first-launch screen's only heading (design.md, screen 1).
- Profile: "People add you as a contact with this ID." (U7). The label doesn't say what the ID is for.
- Screen picker: "Sharper, up to 15 fps" and "Smoother, up to 30 fps" (U3). They tell the two modes apart; the numbers match architecture.md.
- Constraints: "Anyone with this invite can join for 7 days." (`DEFAULT_INVITE_TTL_MS`), "Sounds can be up to 20 seconds and 5 MB.", "Leave empty to use the public relays."
- "3.1 MB, 2 people online have it" on file cards and "Lounge / Game night" in the call panel (U6): both facts are used.
- Rail unread dots and avatar online dots (U10), Host and Owner tags (U9), Direct and Relayed colours: all bound to real state.
- Icons (U11): menus of actions where every item has one, the # and speaker icons that tell text channels from voice rooms, file type icons on file cards.
- Placeholders "tincan1...", "tincan1u...", "https://relay.example.com" and "new-channel" show a format (U7).

No string that `ui/e2e` selects on changed.

Tasks for the owner:

- [x] `crates/tincan-app`: the toasts and error texts the UI shows come from the app (mirrored in `src/api/mock.ts`) and were outside this pass. Checked afterwards: one passive toast reworded ("Audio stopped working, so Tincan restarted it.").
- [x] `docs/HANDOFF.md`: mark "unslop-ui pass on the UI" done.
