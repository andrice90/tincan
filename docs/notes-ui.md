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
| `lib/names.ts` | short ids and name clashes |
| `lib/compose.ts` | the composer box around a send |
| `lib/scroll.ts` | the load-older scroll anchor |
| `dialogs/settingsLogic.ts` | auto-download choices, Settings > Network status, the volume slider debounce |
| `media/screenLogic.ts` | per-track watch/unwatch ordering, decoder fallback, backpressure |

`package.json` has no `test` script yet (it is outside `ui/src`).

## Mock backend

Used automatically outside Tauri (`window.__TAURI_INTERNALS__` missing). Loaded with a dynamic import, so the production bundle only fetches it in a browser.

URL flags: `?fresh` (name picker, no servers), `?noenc` (screen_start fails with `no-hardware-encoder`), `?ring` (incoming call from Alex), `?noanswer` (outgoing calls ring for 45 s, then "X didn't answer."). Console helpers on `window.tincanMock`:

- `ring(name)`, `online(name)`, `offline(name)` (Riley coming online delivers pending DMs), `migrate()` (Game night host moves to Morgan), `toast(text)`.
- For scripted checks: `state()` (the mock's voice and call), `addMember(name, server?)` (a name clash), `soundFlood(count)` (heard clips nobody owns), `snapshot()`, `delay(command, ms)`, `post(name, text, channel?)` and `typing(name, channel?)` (Game night channels), `finishQuietly(fileName)` (a download completes with its final event dropped).

The mock follows the app where it matters to the UI: a hosted server has `path: "none"` and no RTT, `settings_set` clamps volumes to 0..2 and `autoFetchLimit` to 25 MB, `clip_add` takes MP3, WAV, OGG, FLAC and M4A only, `messages` pages are at most 200, and participants carry `volume` (Jordan is saved at 30%).

Join errors: codes shorter than 40 chars, codes not starting with `tincan1`, `tincan1u...` contact codes, and codes containing `offline` are rejected with the matching message.

## Message cache (store/actions.ts, store/messages.ts)

- Pages are per conversation (`convKey`). Live `message-added` / `message-updated` are applied to a loaded cache; delivery (`pending` to false) and deletion only move one way, whichever copy arrives first.
- A message that arrives while the conversation's first page is in flight is kept in an unloaded entry and merged with the page by id (one-way rule). A cached message older than the page is dropped unless the page reaches the start, so a stray update never lands at the end of the list.
- `markSeen`: someone else's message on screen marks the conversation read at once (leading edge, then at most once a second during a burst), and only while it is still on screen.
- Snapshot (event-lag rule): pages read before it may have holes, so every cache is dropped except the conversation on screen, which is re-read from the newest message back to its oldest cached one (up to 200 per read, at most 5 more pages). When the read contains that oldest message, the cache keeps it as the first row, with its cursor; otherwise the read replaces the cache. The reading position survives.
- A generation counter (`cacheGen`, bumped by each snapshot after the first) is part of the load keys: a load from before a snapshot never blocks one after it, and its result is discarded. An older page is prepended only to the entry and cursor it was asked for, in the same generation.
- A conversation off screen whose cache reached 300 messages is dropped instead of grown; the next visit loads its newest page.

## Calls and voice

- Leave voice in a room sends `voice_leave` even while a call rings out from it; the ring survives and the call panel shows its own "Calling <name>" row with Cancel call, above the room panel. Only a call (or a ring with no room) is hung up with `call_hangup`.
- An active call with no voice session attached shows "Call with <name>" with Hang up, so it can always be ended.
- Calling someone (DM header or member list) opens the DM with them first: the call stage, their screen share and the per-user volume menu are there.
- The per-user volume slider starts at the volume picked in this session, else `VoiceParticipant.volume`, else 100%. `volume` is optional in `types.ts` until the app sends it.

## Names

Display names are chosen by each peer and are not unique. When two people the UI knows (members of any server, DM contacts) use the same name, compared without case, width, spacing or invisible characters, both are shown with their short id (the first 8 characters of the user id, the app's fallback name) after the name: member list, DM list, DM header, message authors, voice tiles, room lists, incoming call. The member menu and the voice tile popover always show "ID <short id>". Peer names and ids render in `<bdi>`, so a name's bidi controls can't reorder the id. Nothing changes in text e2e matches unless two names clash.

## Screen share (media/ScreenPlayer.ts)

- A new codec string is probed with `VideoDecoder.isConfigSupported` (prefer-hardware, then no-preference) before it is configured; frames wait, in order, for the answer. Headless Edge reports an unsupported config through the error callback as `OperationError` ("Unsupported configuration"), not `NotSupportedError`, and decodes `avc1.f4...` (High 4:4:4) only without hardware, so the decision never looks at the error name.
- A decoder that fails before its first frame is retried in software; three failures without a frame show "This screen share uses a video format this PC can't decode." Errors after frames rebuild the decoder at the next keyframe.
- Delta frames are skipped while `decodeQueueSize` is over 8 (the view waits for the next keyframe, at most one GOP).
- `screen_watch` and `screen_unwatch` run one after another per track (`keyedSerial`), so an unwatch never overtakes its watch, and a player only unwatches when it subscribed.
- Minimized (`onWindowHidden`), the player unwatches and closes its decoder; on restore it watches again on a new channel and starts at the host's cached keyframe.

## Choices made where the docs disagree or are silent

- Clip limits shown at the add step: 20 s and 5 MB (architecture.md), not the 10 s / 2 MB in design.md. Text lives in `CLIP_LIMIT_TEXT` (`src/dialogs/SoundDialogs.tsx`).
- Mute and deafen live in the call panel only (design.md also lists a mic button in the user panel; two mute buttons side by side were redundant).
- Opening a voice room joins it when you are not already in a room or a call.
- DM typing works: `typing` takes `channelId: null` for a DM and the app sends `typing` events with `channelId: null`, shown under the DM composer.
- Settings > Network shows "N people connected" for a server this PC hosts (the app reports no path while hosting).
- The auto-download menu stops at 25 MB, the app's attachment limit.
- The composer empties when a send starts. A failed send puts its text back in front of anything typed since; an Enter pressed during a send is sent when it finishes.
- The soundboard keeps at most 50 heard clips, newest last, like the app.
