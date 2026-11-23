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
2. **"Copy my friend code"**: the button is clicked, but `navigator.clipboard.writeText` is replaced in the page beforehand. The code is captured there and the user's real clipboard is left alone. The captured code is cross-checked against one `get_friend_code` call, which returns the code already made.
3. Process statistics come from PowerShell (`Get-Process`, `Win32_Process`).

## Flows and checks

| Flow | Driven through the UI | Checked |
|---|---|---|
| a | Both pick a name on the Welcome screen. A creates "E2E Server", opens Invite people and reads the short code from the dialog. B joins it with "Join with invite", typed in lowercase without the dash. | A 6-character `ABC-DEF` code (made on first open; offline it resolves through `profiles/.offline-codes`). Both member lists show Alice and Bob under Online. B's header says "Hosted by Alice". No error toasts. |
| b | A types slowly, B types slowly, then 10 timed messages each way. A attaches `e2e-attach.png` (96x64, four stripes) through the paperclip and the native dialog, then sends. | "Alice is typing" on B and "Bob is typing" on A. The typing line clears after the message. Every message arrives. B shows an inline `img` with naturalWidth 96 and naturalHeight 64, loaded from `http://tincan.localhost/file/<hash>`. |
| c | B: Direct messages, Add contact, "Copy my friend code". A: Add contact, pastes the code (resubmitted while B's record is not written yet), sends a DM. B opens the new conversation row and types and sends a reply. | The code equals `get_friend_code` and has the `ABC-DEF` form. A's message is no longer pending once delivered. DM typing line. Messages both ways. |
| d | Both open the Lounge voice room (opening it joins) and press Mute. A opens Soundboard, then Add sound with `e2e-chime.wav`. A plays it. B opens Soundboard and presses Save on the heard tile, then plays the saved clip. | Both are listed under Lounge on both sides. The voice panel shows "Direct". The Add sound dialog proposes "e2e-chime" and the tile shows "2.0 s". B sees "Heard in this call" with "e2e-chime, From Alice". After Save, B's tile is a playable owned clip. B's play lights A's tile (A got `sound-played`). |
| e | The pattern window opens. A: Share screen, picks the `tincan-e2e-pattern` card, Share. B clicks "Watch Alice's screen". | Exactly one card with that title. It is the only selected card (the picker preselects a display). The dialog closes and "Stop sharing" appears. B's canvas gets frames. In a CDP screenshot of B, the four quadrant centres of the drawn video are (255,1,0), (0,255,1), (0,0,255) and (255,255,0), each within 60 of red, green, blue and yellow. The canvas has the window's aspect (642x512). Stop sharing removes it on B. |
| f | Both leave voice and open #general. The host (read from the header, "Hosted by you") presses the title bar Close (`window_close`). The survivor sends two messages. The closed profile is started again with the same port and arguments. | The host exits with code 0. The survivor's header becomes "Only you are online". The restarted instance shows "Hosted by Bob" and both messages sent while it was closed. Bob's member list shows Alice online again. A message after the rejoin arrives. The DM that was read before closing is still read (read markers survive a restart). |
| idle | CDP disconnected, both instances in #general, nobody in voice, 60 s idle. CPU is then sampled over 10 s. | Numbers below. |

## Results

Last run: 2026-11-23, after the round 2 fixes and the round 3 review fixes (`docs/review/findings.md`), debug build (`dev` profile) with the embedded UI, started from the repository root. All 49 checks passed, in two runs in a row after bug 4 below was fixed. The first run after the round 3 fixes had failed "B sees who hosts" (bug 4). No instance wrote a warning or error to its log (`TINCAN_LOG=warn,tincan_audio=info`). Both instances that joined voice opened and closed the microphone and speaker exactly once: on joining voice, and on leaving it. The restarted instance never joined voice and opened no device. The numbers below come from the first of the two passing runs. The run before the round 2 fixes was within a few ms of them, except the host exit (219 ms then).

### Timings (loopback, offline network)

Waits poll every 50 ms. Message latencies are stamped in the pages themselves: from the Enter keydown in the sender's page to the DOM insertion in the receiver's page (a MutationObserver), using `performance.timeOrigin + now()` in both. Both pages share the machine's clock.

| What | Time |
|---|---|
| Create server (click to "Only you are online") | 83 ms |
| Join: click "Join server" to the server shown on B | 79 ms |
| Join: to both online in B's member list / in A's | 81 ms / 201 ms |
| Message latency A to B, median of 10 | 4 ms |
| Message latency B to A, median of 10 | 3 ms |
| Attachment (180 B PNG): Enter to B's inline preview loaded, auto-fetched | 78 ms |
| DM A to B, including B opening the new conversation / DM B to A | 26 ms / 11 ms |
| Voice: both listed in Lounge on both sides | 332 ms |
| Sound played on A to "Heard in this call" on B | 16 ms |
| Clip pull on B (Save to playable tile) | 839 ms |
| Screen share: Share click to "Stop sharing" | 331 ms |
| Watch click to first decoded frame on B | 18 ms |
| Stop sharing to the share gone on B | 319 ms |
| Host migration: Close click to survivor "Only you are online" | 162 ms |
| Host migration: Close click to the host process exiting (clean handoff) | 91 ms |
| Close click to the survivor having sent two messages (includes typing) | 1138 ms |
| Restart: launch to "Hosted by Bob" / to the missed messages shown | 446 ms / 449 ms |
| First message after the rejoin | 4 ms |

### Resources after 60 s idle

Debug build, both instances in a server text channel, not in voice, CDP disconnected. CPU% is of one core, from two CPU-time samples 10 s apart. Windows counts CPU time in 15.6 ms ticks, so over 10 s the resolution is about 0.16%. Working sets double-count shared pages, so the totals overstate real memory. The WebView2 processes are the browser process and its children, matched by parent process. Each cell gives working set, private bytes, threads and CPU.

| Process | e2e-a (restarted, never in voice since) | e2e-b (was in voice earlier) |
|---|---|---|
| tincan.exe | 57.4 MB, 9.4 MB, 14, 0% | 58.2 MB, 11.6 MB, 19, 0% |
| WebView2 browser | 129.1 MB, 32.9 MB, 56, 0.15% | 126.6 MB, 36.1 MB, 51, 0.3% |
| WebView2 GPU | 77.2 MB, 74.2 MB, 62, 0% | 113.4 MB, 102.1 MB, 63, 0% |
| WebView2 renderer | 85.2 MB, 36.8 MB, 25, 0% | 116.2 MB, 61.2 MB, 29, 0% |
| WebView2 utility (2) | 40.9 + 22.7 MB, 11.2 + 7.3 MB, 24 + 11, 0% | 42.7 + 22 MB, 11.5 + 7.2 MB, 24 + 10, 0.15% |
| WebView2 crashpad | 15.8 MB, 3.3 MB, 12, 0% | 15.8 MB, 3.3 MB, 12, 0% |
| Total (7 processes) | 428 MB working set, 175 MB private, 204 threads, 0.15% | 495 MB working set, 233 MB private, 208 threads, 0.45% |

- The Rust process idles at 0% CPU with 9-12 MB of private memory.
- **The architecture target of under 30 MB RSS for the idle Rust core is not met by working set.** The release build (below) idles at 43-46 MB working set with 9-11 MB of private bytes. The rest is the 28 MB exe image and shared DLL pages that the working set counts. Private memory is well under 30 MB.
- B's 5 extra threads are likely its audio engine, which stays parked after the first voice use (by design, per the app notes). Its devices were closed, as the logs show.
- B's larger GPU and renderer processes are likely left over from decoding the screen share. This was not measured separately.

### Release build

Run on 2026-11-23 against `target/release/tincan.exe` from `pnpm tauri build` (`TINCAN_EXE`, offline network, real audio devices). The exe was built from commit 3a32e6e; the next commit changed only one toast's wording, and the installer was then rebuilt from it. All 49 checks passed on the first run, and no instance logged a warning or error.

Most timings were within 5 ms of the debug run above. The exceptions:

| What | Release | Debug |
|---|---|---|
| Watch click to first decoded frame | 64 ms | 18 ms |
| Voice: both in the room | 299 ms | 332 ms |
| Join: both online on A | 196 ms | 201 ms |

After 60 s idle, with the same setup as above:

| Process | e2e-a | e2e-b |
|---|---|---|
| tincan.exe | 43.3 MB, 8.9 MB, 14, 0% | 46.1 MB, 10.8 MB, 18, 0% |
| Total (7 processes) | 416 MB working set, 176 MB private, 202 threads, 4.6% | 495 MB working set, 237 MB private, 214 threads, 1.1% |

e2e-a's 4.6% CPU came from two WebView2 processes during the sample: the browser process at 3.6% and a utility process at 1%. tincan.exe was at 0%. The single 10 s sample can't tell a background task from a steady cost.

## Bugs found and fixed

The app and the shell met for the first time in this run. They needed no glue changes: the bundle, IPC, events, the `tincan` file scheme, raw video frames over the Channel, the native dialog and the close sequence all worked against the real backend. The run found two UI bugs and one gap in diagnostics:

1. **A delivered DM kept showing "Waiting for Bob to come online".** On loopback the DM ack (`message-updated`, `pending: false`) arrived before the `send_message` result (`pending: true`), and `putMessage` replaced the newer state with the older one. The fix: `putMessage` (`ui/src/store/actions.ts`) treats `pending` and `deleted` as one-way. A command result can no longer bring back a pending or deleted state that an event already cleared.
2. **A conversation stayed unread when the view changed right after a message arrived.** The UI marked a conversation read only when the backend's debounced unread flag (100 ms) came back while the conversation was still on screen. Leaving within that window left a DM or channel unread, and that survived restarts. The fix: `markSeen` in `actions.ts` calls `mark_read` as soon as someone else's message lands in the conversation on screen, then once more at the end of a burst if the conversation is still on screen, at most once per second per conversation.
3. **No way to see backend logs.** The shell had no tracing subscriber, so device and network warnings were invisible. The fix: `TINCAN_LOG=<EnvFilter>` (`src-tauri/src/main.rs`) writes tracing output to stderr. When unset, nothing is installed. The e2e run captures it per instance under `logs/`.
4. **B's header kept "Hosted by <short id>" after joining** (found after the round 2 and 3 fixes, in one run of three). `join_server` returns a `ServerSummary` built when the join returns, before the history sync brought the host's Profile. Since round 2 the bulk window ends at `SyncComplete`, so the sync's `server-updated` (with "Alice") can reach the UI before the command result. `addServer` then replaced the newer entry with the result, and the backend never sends the same `server-updated` again, so the header stayed wrong until the summary changed for another reason. The check read the header 55 ms after the member list showed both names and still saw the short id. The fix: a `create_server` or `join_server` result only adds a server the list does not hold yet (`withCreatedServer` in `ui/src/store/servers.ts`), the same rule as command results for transfers (UI-LOGIC-10).

## Not verified

- **Default networking:** relays, mDNS, rendezvous and the n0 pkarr/DHT records (`--online`) were not run. The a-int `tincan.exe` has no firewall rule, and the run would raise a Windows Firewall prompt that someone has to answer. Two machines, NAT and relayed paths were not tested either.
- **Audio content:** both instances were muted, so speaking rings and voice packets were not checked through the UI. The microphone and speaker did open and close (logs).
  - Whether B actually heard the clip was not checked.
  - A separate 3 s mic test (Settings, Voice) opened the real microphone, but the meter stayed at 0. The room may have been quiet or the mic muted; no one spoke.
  - Echo cancellation with two instances on one set of speakers was not tested.
- **Calls:** the 1:1 call flow (ring, accept, hang up) and screen sharing inside a call were not part of this run.
- **Host migration:** only the clean handoff via `window_close` was tested. A host crash (killed process), migration while in voice or while sharing, and proactive migration were not.
- **"Copy" and "Copy my friend code":** the real clipboard write was not tested; the in-page write was captured instead.
- **Window and build:**
  - Window dragging, minimize and maximize were not exercised.
  - No release or NSIS build was run.
  - Only the debug build was measured for resources.
- **File transfers:** the attachment was 180 bytes, so download progress, large files, the 25 MB limit and resuming a partial download were not exercised in this run (the app tests cover several of them in-process).
- **`open_file`:** clicking an attachment opens Explorer, so the script never clicks one.
- **Screen share sources:** only one window source at 15 fps text mode, on this machine's GPU, was tested. Display sources, motion mode, a closing shared window and a machine without an encoder were not.
