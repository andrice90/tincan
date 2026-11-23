# Two-profile end-to-end run

This run checks the real app on one Windows machine. Two `tincan.exe` instances (profiles `e2e-a` "Alice" and `e2e-b` "Bob") run the real `tincan-app` backend and the real UI bundle. A node script drives both UIs over the Chrome DevTools Protocol (CDP) of their WebView2 windows. The script lives in [`ui/e2e/`](../ui/e2e/):

| File | What it does |
|---|---|
| `e2e.mjs` | Flows a-f, the idle resource check, the results file |
| `lib.mjs` | Launching an instance with remote debugging, the Playwright CDP connection, screenshots, waits, process statistics |
| `media.mjs` | Generates the PNG attachment and the WAV clip; a small PNG decoder for checking screenshots |
| `pattern.ps1` | The window that gets shared: four saturated quadrants plus a moving white square, titled `tincan-e2e-pattern` |
| `pick.ps1` | Answers the native Open dialog of one Tincan process |

## Running it

Requirements: Windows 10/11 with WebView2, node 22, and a hardware H.264 encoder for flow e. playwright-core is not a `ui/` dependency, so install it in any scratch folder.

```sh
# 1. UI bundle and a debug build that embeds it (without the feature the exe loads localhost:5173)
cd ui && pnpm build && cd ..
CARGO_TARGET_DIR=target/a-int cargo build -p tincan --features tauri/custom-protocol

# 2. playwright-core, outside the repository
mkdir -p "$TEMP/tincan-e2e-deps" && (cd "$TEMP/tincan-e2e-deps" && npm i playwright-core)

# 3. the run
TINCAN_E2E_DEPS="$TEMP/tincan-e2e-deps" node ui/e2e/e2e.mjs            # add --keep to leave instances open on a failure
```

Environment variables:

| Variable | Default | Meaning |
|---|---|---|
| `TINCAN_E2E_DEPS` | (required) | Folder whose `node_modules` has playwright-core |
| `TINCAN_E2E_OUT` | `%TEMP%\tincan-e2e` | Where `logs/`, `shots/`, `media/` and `results.json` go |
| `TINCAN_EXE` | `target/a-int/debug/tincan.exe` | The exe under test |
| `TINCAN_LOG` | `warn,tincan_audio=info` | Passed to the instances (shell diagnostics to stderr, see below) |

The script checks that each instance serves the current `ui/dist` bundle (script hash), so a stale exe fails at once.

### What it does to the machine

- It deletes `%APPDATA%\Tincan\profiles\e2e-a` and `e2e-b` (only names starting with `e2e-` are accepted), then starts both instances with `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9301` and `9302`. At the end it closes them with the title bar Close button and leaves both profile folders in place.
- `TINCAN_OFFLINE_NETWORK=1` is set: loopback only, with no relays, mDNS or rendezvous, and no Windows Firewall prompt. `--online` drops it (see "Not verified").
- Real audio devices are opened, because `TINCAN_NO_DEVICES` would also turn off real screen capture. Both instances mute right after joining voice, since they share one microphone and one set of speakers. The clip is a quiet 2 s tone (523 Hz at -26 dBFS).
- A 640x480 WinForms window titled `tincan-e2e-pattern` opens at (60, 60) for flow e and is closed at the end.
- Screenshots are CDP captures of the two webviews only. The screen picker is not captured, because it lists the titles of other windows.

### Steps that are not webview clicks

The script drives each flow by clicking and typing in the two webviews, with these exceptions:

1. **Native Open dialog** (attachment, "Add sound"): CDP cannot reach it. `pick.ps1` finds the dialog window of that process, sets the file name box with `WM_SETTEXT` and presses Open with `WM_COMMAND IDOK`. It sends no keyboard or mouse input, so it works without focus. This still exercises the real `plugin:dialog|open` path and its capability. (UI Automation sees these classic controls only as panes, so the script uses plain Win32 calls.)
2. **"Copy my ID"**: the button is clicked, but `navigator.clipboard.writeText` is replaced in the page beforehand. The code is captured there and the user's real clipboard is left alone. The captured code is cross-checked against one `app_snapshot` call, which is read-only.
3. Process statistics come from PowerShell (`Get-Process`, `Win32_Process`).

## Flows and checks

| Flow | Driven through the UI | Checked |
|---|---|---|
| a | Both pick a name on the Welcome screen. A creates "E2E Server", opens Invite people and reads the code from the dialog. B joins it with "Join with invite". | A 284-char `tincan1...` code. Both member lists show Alice and Bob under Online. B's header says "Hosted by Alice". No error toasts. |
| b | A types slowly, B types slowly, then 10 timed messages each way. A attaches `e2e-attach.png` (96x64, four stripes) through the paperclip and the native dialog, then sends. | "Alice is typing" on B and "Bob is typing" on A. The typing line clears after the message. Every message arrives. B shows an inline `img` with naturalWidth 96 and naturalHeight 64, loaded from `http://tincan.localhost/file/<hash>`. |
| c | B: Direct messages, Add contact, "Copy my ID". A: Add contact, pastes the code, sends a DM. B opens the new conversation row and types and sends a reply. | The code equals `me.userCode` and starts with `tincan1u`. A's message is no longer pending once delivered. DM typing line. Messages both ways. |
| d | Both open the Lounge voice room (opening it joins) and press Mute. A opens Soundboard, then Add sound with `e2e-chime.wav`. A plays it. B opens Soundboard and presses Save on the heard tile, then plays the saved clip. | Both are listed under Lounge on both sides. The voice panel shows "Direct". The Add sound dialog proposes "e2e-chime" and the tile shows "2.0 s". B sees "Heard in this call" with "e2e-chime, From Alice". After Save, B's tile is a playable owned clip. B's play lights A's tile (A got `sound-played`). |
| e | The pattern window opens. A: Share screen, picks the `tincan-e2e-pattern` card, Share. B clicks "Watch Alice's screen". | Exactly one card with that title. It is the only selected card (the picker preselects a display). The dialog closes and "Stop sharing" appears. B's canvas gets frames. In a CDP screenshot of B, the four quadrant centres of the drawn video are (255,1,0), (0,255,1), (0,0,255) and (255,255,0), each within 60 of red, green, blue and yellow. The canvas has the window's aspect (642x512). Stop sharing removes it on B. |
| f | Both leave voice and open #general. The host (read from the header, "Hosted by you") presses the title bar Close (`window_close`). The survivor sends two messages. The closed profile is started again with the same port and arguments. | The host exits with code 0. The survivor's header becomes "Only you are online". The restarted instance shows "Hosted by Bob" and both messages sent while it was closed. Bob's member list shows Alice online again. A message after the rejoin arrives. The DM that was read before closing is still read (read markers survive a restart). |
| idle | CDP disconnected, both instances in #general, nobody in voice, 60 s idle. CPU is then sampled over 10 s. | Numbers below. |

## Results

Last run: 2026-11-23, debug build (`dev` profile) with the embedded UI, started from `ui/e2e/`. All 49 checks passed. It was the fifth passing run in a row since the first UI fix below, and the only one after the last code change (the on-screen re-check in `markSeen`). No instance wrote a warning or error to its log (`TINCAN_LOG=warn,tincan_audio=info`). Both instances that joined voice opened and closed the microphone and speaker exactly once: on joining voice, and on leaving it (the speaker 0.5 s later). The restarted instance never joined voice and opened no device. The numbers below come from that run. The earlier passing runs were within a few ms of them, except the host exit, which varied from 92 to 219 ms.

### Timings (loopback, offline network)

Waits poll every 50 ms. Message latencies are stamped in the pages themselves: from the Enter keydown in the sender's page to the DOM insertion in the receiver's page (a MutationObserver), using `performance.timeOrigin + now()` in both. Both pages share the machine's clock.

| What | Time |
|---|---|
| Create server (click to "Only you are online") | 81 ms |
| Join: click "Join server" to the server shown on B | 77 ms |
| Join: to both online in B's member list / in A's | 78 ms / 205 ms |
| Message latency A to B, median of 10 (samples 2-4 ms) | 3 ms |
| Message latency B to A, median of 10 (samples 2-3 ms) | 3 ms |
| Attachment (180 B PNG): Enter to B's inline preview loaded, auto-fetched | 63 ms |
| DM A to B, including B opening the new conversation / DM B to A | 74 ms / 19 ms |
| Voice: both listed in Lounge on both sides | 302 ms |
| Sound played on A to "Heard in this call" on B | 18 ms |
| Clip pull on B (Save to playable tile) | 849 ms |
| Screen share: Share click to "Stop sharing" | 334 ms |
| Watch click to first decoded frame on B | 24 ms |
| Stop sharing to the share gone on B | 323 ms |
| Host migration: Close click to survivor "Only you are online" | 162 ms |
| Host migration: Close click to the host process exiting (clean handoff) | 219 ms |
| Close click to the survivor having sent two messages (includes typing) | 1213 ms |
| Restart: launch to "Hosted by Bob" / to the missed messages shown | 416 ms / 418 ms |
| First message after the rejoin | 4 ms |

### Resources after 60 s idle

Debug build, both instances in a server text channel, not in voice, CDP disconnected. CPU% is of one core, from two CPU-time samples 10 s apart. Windows counts CPU time in 15.6 ms ticks, so over 10 s the resolution is about 0.16%. Working sets double-count shared pages, so the totals overstate real memory. The WebView2 processes are the browser process and its children, matched by parent process. Each cell gives working set, private bytes, threads and CPU.

| Process | e2e-a (restarted, never in voice since) | e2e-b (was in voice earlier) |
|---|---|---|
| tincan.exe | 54.1 MB, 9.1 MB, 13, 0.15% | 57.1 MB, 11.2 MB, 17, 0% |
| WebView2 browser | 126.6 MB, 32.5 MB, 57, 0% | 126.6 MB, 36.2 MB, 51, 0.15% |
| WebView2 GPU | 72.5 MB, 74.1 MB, 58, 0.3% | 118.3 MB, 109.2 MB, 63, 0.3% |
| WebView2 renderer | 83.6 MB, 34.9 MB, 26, 0.3% | 116.4 MB, 60.7 MB, 26, 0.15% |
| WebView2 utility (2) | 40.5 + 22.1 MB, 11.2 + 7.2 MB, 24 + 10, 0% | 42.6 + 22.1 MB, 11.6 + 8.2 MB, 26 + 10, 0% |
| WebView2 crashpad | 15.7 MB, 3.2 MB, 11, 0% | 15.7 MB, 3.2 MB, 11, 0% |
| Total (7 processes) | 415 MB working set, 172 MB private, 199 threads, 0.75% | 499 MB working set, 240 MB private, 204 threads, 0.6% |

- The Rust process idles at about 0% CPU with 9-11 MB of private memory.
- **The architecture target of under 30 MB RSS for the idle Rust core is not demonstrated.** The debug build's working set is 54-57 MB. Given the 9-11 MB of private bytes, that is likely mostly the debug exe's image pages, but no release build was measured.
- B's 4 extra threads are likely its audio engine, which stays parked after the first voice use (by design, per the app notes). Its devices were closed, as the logs show.
- B's larger GPU and renderer processes are likely left over from decoding the screen share. This was not measured separately.

## Bugs found and fixed

The app and the shell met for the first time in this run. They needed no glue changes: the bundle, IPC, events, the `tincan` file scheme, raw video frames over the Channel, the native dialog and the close sequence all worked against the real backend. The run found two UI bugs and one gap in diagnostics:

1. **A delivered DM kept showing "Waiting for Bob to come online".** On loopback the DM ack (`message-updated`, `pending: false`) arrived before the `send_message` result (`pending: true`), and `putMessage` replaced the newer state with the older one. The fix: `putMessage` (`ui/src/store/actions.ts`) treats `pending` and `deleted` as one-way. A command result can no longer bring back a pending or deleted state that an event already cleared.
2. **A conversation stayed unread when the view changed right after a message arrived.** The UI marked a conversation read only when the backend's debounced unread flag (100 ms) came back while the conversation was still on screen. Leaving within that window left a DM or channel unread, and that survived restarts. The fix: `markSeen` in `actions.ts` calls `mark_read` as soon as someone else's message lands in the conversation on screen, then once more at the end of a burst if the conversation is still on screen, at most once per second per conversation.
3. **No way to see backend logs.** The shell had no tracing subscriber, so device and network warnings were invisible. The fix: `TINCAN_LOG=<EnvFilter>` (`src-tauri/src/main.rs`) writes tracing output to stderr. When unset, nothing is installed. The e2e run captures it per instance under `logs/`.

## Not verified

- **Default networking:** relays, mDNS, rendezvous and the n0 pkarr/DHT records (`--online`) were not run. The a-int `tincan.exe` has no firewall rule, and the run would raise a Windows Firewall prompt that someone has to answer. Two machines, NAT and relayed paths were not tested either.
- **Audio content:** both instances were muted, so speaking rings and voice packets were not checked through the UI. The microphone and speaker did open and close (logs).
  - Whether B actually heard the clip was not checked.
  - A separate 3 s mic test (Settings, Voice) opened the real microphone, but the meter stayed at 0. The room may have been quiet or the mic muted; no one spoke.
  - Echo cancellation with two instances on one set of speakers was not tested.
- **Calls:** the 1:1 call flow (ring, accept, hang up) and screen sharing inside a call were not part of this run.
- **Host migration:** only the clean handoff via `window_close` was tested. A host crash (killed process), migration while in voice or while sharing, and proactive migration were not.
- **"Copy invite" and "Copy my ID":** the real clipboard write was not tested; the in-page write was captured instead.
- **Window and build:**
  - Window dragging, minimize and maximize were not exercised.
  - No release or NSIS build was run.
  - Only the debug build was measured for resources.
- **File transfers:** the attachment was 180 bytes, so download progress, large files, the 25 MB limit and resuming a partial download were not exercised in this run (the app tests cover several of them in-process).
- **`open_file`:** clicking an attachment opens Explorer, so the script never clicks one.
- **Screen share sources:** only one window source at 15 fps text mode, on this machine's GPU, was tested. Display sources, motion mode, a closing shared window and a machine without an encoder were not.
