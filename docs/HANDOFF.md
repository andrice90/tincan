# Handoff

Status of the Tincan build for the next session. Updated at every milestone commit; the newest state is at the top.

## Where it stands

### Soundboard and names (branch `worktree-agent-a345a20b5a9682faf`)

- **Stop a sound:** a stop button on the tile of a sound I am playing; it stops it here and in the call at once (`clip_stop`, `VoiceSession.playingClips`).
- **Soundboard track:** clips now travel on their own audio track per member (`KIND_SOUND`, track kind 2) instead of inside the voice packets. That makes a true per-person soundboard volume possible. **Compatibility:** older builds drop these packets, so everyone in a room needs this build to hear clips.
- **Volumes:** right-click a member or a voice tile for Volume and Soundboard volume; right-click a voice room for its Soundboard volume. All saved on this PC.
- **Names:** display names only. The short id shows only when two names look alike (SECURITY-4).
- The soundboard closes on a click outside it.
- Not verified: the two-profile UI e2e was not run for this change, and nothing on real audio hardware.

### Third round (2026-11-24)

- **Floating layout, now the default:** every region is a rounded card with a gap around it, and panels slide open and closed (ease-in-out, with a reverse for each). Classic stays available in Settings > Appearance > Layout. The composer card and the account card are both 52 px tall.
- **Typing indicator:** "X is typing" shows in the chat area under the newest message, not in the composer.
- **Voice tiles:** the column count follows the number of people (`lib/tileGrid.ts`), and tiles scale from the stage size (container query units). Sliding panels or dragging a grab bar no longer rewraps them or makes them jump.
- **Join and leave sounds in voice rooms:** played locally at half gain, silent while deafened, at most one per 1.5 s. The sound files are third-party (the owner's Ovani Sound pack) and are not in the repo or the public release: `crates/tincan-app/assets/*.wav` is git-ignored, and `crates/tincan-app/build.rs` embeds them only when both files are present (otherwise the cues are silent).
- **Log file per profile:** `%APPDATA%\Tincan\profiles\<profile>\logs\tincan.log` (rotates at 4 MB, max 16 MB per run). `TINCAN_LOG` overrides the filter.
- **App icon:** `src-tauri/build.rs` now reruns when the icon changes. Before this, the exe kept the old icon.
- **Open:** the owner once saw voice stuck on "Connecting" as a solo host after an in-place upgrade. It didn't reproduce here in four scenarios, including an upgrade from the previous build. The log file is there to catch it next time.

The two-profile UI e2e passed 49/49 after each change. The installer was rebuilt at `024e63e`.

### Second round of owner requests (2026-11-23)

Added since the first handoff, all merged on main:

- **Voice click fix.** unsafe-libopus packet loss concealment emitted near full-scale bursts after quiet hybrid frames, which was the click about every second in voice. Concealment is now checked against the last frame and dropped if it bursts. Pauses fade out, and clips have 5 ms ramps.
- **UI:**
  - The member list never overlaps "(you)" or the Host and Owner tags.
  - The channel list and member list resize with a grab bar.
  - The WebView page menu is suppressed outside text fields and selections.
  - Unread channels show badges.
- **Voice room chat:** each voice room has its own message history. The chat icon on the room row opens it, and "Chat" opens a panel beside the tiles.
- **Remove member:** owner only, through `Body::MemberRemove` (index 11). The host enforces it, including a new host after migration. A removed user can rejoin only with an invite that still works: their own old invites are dead.
- **Short codes:** 6-character server invite codes that last until the owner rotates them, and 6-character friend codes, both rotatable. Each code commits to its target with argon2id, so knowing a code doesn't let anyone redirect it. Long codes stay as a fallback.
- **Avatars and server icons:** `Body::ProfileImage` (12) and `ServerIcon` (13), plus `Ctl::DmAvatar` (40). A crop dialog shows a circle or squircle outline, with drag and zoom. Images are 128x128 and at most 64 KB.
- **New logo:** a tin can angled up and right, projecting sound. Sources are in `src-tauri/icons/*.svg`, with variants for each size.

**Compatibility:** these add event and frame variants. A build from before them rejects the new events and stalls on the sender, so every machine in a server needs the new build.

Last full check: `cargo test --workspace` 531 passed, 0 failed (14 ignored). Clippy is clean. `pnpm test` 97 passed. The two-profile UI e2e passed 49/49 on the debug build.

**Not verified:**
- Short-code lookups over the real pkarr relay and DHT; only the offline folder path is tested.
- Avatar upload through the real WebView2 file picker.
- The voice click fix on real hardware.
- The e2e has no steps yet for voice room chat, removal, avatars or the resize bar.

### First handoff

Testable product (2026-11-23). Every feature in the original request is built. These run end to end between two profiles on one machine through the real UI:

- text rooms and DMs
- voice rooms
- screen share
- P2P attachments
- host migration
- the soundboard

1:1 calls and downloading an attachment while its poster is offline are covered by the in-process app tests only.

Three review rounds found security and correctness problems; all confirmed findings but two are fixed (see [`review/findings.md`](review/findings.md)). The two-profile UI e2e passes 49/49 on the debug build and on the release build. The NSIS installer builds but has never been run.

| Step | State |
|---|---|
| Core crates (core, store, net, files, audio, screen) | done, tested |
| App orchestrator (`crates/tincan-app`) and Tauri shell (`src-tauri`) | done, tested |
| Round 1 hardening of tincan-net | done |
| Round 2 fixes (app, shell, UI) | done |
| Round 3 fixes (review of the round 2 fixes) | done, except R3-OPEN-1 and R3-OPEN-2 (below); the fixes were reviewed only by their author |
| unslop-ui pass (`ui/src` and the app's toasts and errors) | done, see `notes-ui.md` |
| Two-profile UI e2e (`ui/e2e`) | 49/49, debug and release builds |
| NSIS installer (`pnpm tauri build`) | builds: `Tincan_0.1.0_x64-setup.exe`, 7.9 MB (release exe 27.8 MB); never run |
| Release idle memory | `tincan.exe` 43-46 MB working set, 9-11 MB private, 0% CPU. WebView2 adds about 370-450 MB working set over 6 processes (`e2e.md`). The 30 MB target holds for private memory, not for working set. |

Last checks on `main`:

- `cargo test --workspace`: 0 failures; the real-device and live-network tests are ignored.
- clippy `--workspace --all-targets -D warnings` is clean.
- `pnpm typecheck` and `pnpm build` are clean.
- `pnpm test`: 97 passed (after the second round).

## Build, run, test

All commands from the repository root unless noted. A per-task `CARGO_TARGET_DIR` keeps parallel builds apart (`target/` is git-ignored).

```sh
# Rust tests (everything; about 5 min on this machine)
CARGO_TARGET_DIR=target/a-int cargo test --workspace

# UI
cd ui && pnpm install && pnpm typecheck && pnpm test && pnpm build

# Dev run (Vite on :5173 plus the debug app; profile "dev")
cd ui && TINCAN_PROFILE=dev pnpm tauri dev

# Debug exe with the UI embedded (what the e2e run uses by default)
cd ui && pnpm build && cd ..
CARGO_TARGET_DIR=target/a-int cargo build -p tincan --features tauri/custom-protocol
target/a-int/debug/tincan.exe --profile alice

# Two-profile UI end-to-end run: see docs/e2e.md (TINCAN_EXE picks another exe)
TINCAN_E2E_DEPS=<folder with node_modules/playwright-core> node ui/e2e/e2e.mjs

# Release exe and installer (NSIS, per user, unsigned)
cd ui && pnpm tauri build
# exe:       target/release/tincan.exe
# installer: target/release/bundle/nsis/Tincan_0.1.0_x64-setup.exe
```

`cargo test --workspace` relinks `target/a-int/debug/tincan.exe` without the embedded UI. Rebuild with `--features tauri/custom-protocol` before an e2e run (the script detects a stale exe).

Profiles: `--profile <name>` or `TINCAN_PROFILE`, data in `%APPDATA%\Tincan\profiles\<name>`. Test switches: `TINCAN_OFFLINE_NETWORK=1` (loopback only, no relays, no firewall prompt), `TINCAN_NO_DEVICES=1` (synthetic audio, no capture), `TINCAN_LOG=<tracing filter>` (logs to stderr). Keep test profiles named `e2e-*`; `default` is the real profile.

## Docs map

- `architecture.md`: design, defaults chosen without asking, security model and residual risks, what is not verified.
- `api.md`, `ui/src/api/types.ts`: the UI to app contract.
- `design.md`: screens and tokens.
- `notes-*.md`: per-crate integration notes, kept current by the agents that changed each crate.
- `e2e.md`: the two-profile run, its timings and resource numbers.
- `review/`: confirmed review findings (`findings.md`) and the full entries per round (`round*-confirmed.json`).

## Since the last handoff

- Short codes: each server has a 6-character invite code (owner rotates it) and each user a 6-character friend code, resolved through argon2id-derived pkarr records (a shared folder offline). Four new commands; see api.md and architecture.md "Short codes". The long forms still work.
- Round 2 fixes merged, then round 3: a review of those fixes found 17 more issues, and all are fixed. The main ones were sybil admissions growing a server without bound, an attestation merge takeover, Events and DM floods, pre-auth and stranger-slot lockouts, and unproven blob sizes.
- Avatars and server icons (branch `worktree-agent-a7bdc89dd249e4e55`): `Body` 12 and 13, `Ctl::DmAvatar` (40), `BlobKind::Image`, commands `set_avatar` and `set_server_icon`, avatar and icon fields in the DTOs, a crop dialog. `Body` 11 is a placeholder to drop when `MemberRemove` merges. Every member needs this build before anyone sets an image (see `notes-tincan-core.md`).
- The contract gained one command: `dm_watch` (hold a DM connection open while it is on screen). See `api.md`.
- The e2e run found one UI race, E2E-R3-1 (the header kept the host's short id after joining). It is fixed.
- The unslop-ui pass made light copy and alignment changes. Its report is in `notes-ui.md`.
- The screen player's codec caches evict the oldest entry instead of clearing (SEC-CODEC-CACHE).
- The fix branches and agent worktrees are merged and removed. The `wip/net`, `wip/files-sec` and `wip/ui` branches are snapshots of unfinished agent work, made before the usage limit, and are superseded by main. Delete them with `git branch -D` once no longer wanted.

## Open questions for the owner

1. **R3-OPEN-2: deleting or blocking a DM.** Stranger DM spam no longer locks anyone out, but there is no way to remove it from the DM list. Proposed default:
   - Add "Delete conversation", which hides it until a new message arrives.
   - Add "Block", which drops the peer's DM events and dials and keeps a local block list.
   - Both are local only. This needs a contract command and UI.
2. **R3-OPEN-1: a malicious host flooding members with valid events.** Members budget only the new-member admissions a host relays. Budgeting all host-relayed events would also throttle legitimate history sync.
   - Proposed default: a per-author rate limit on live (not sync) events received from the host, sized well above the host's own publish budget.
3. **SECURITY-4 approach.** Lookalike names get a short-id suffix: the UI folds UTS #39 confusables in `nameKey`. Is a suffix enough, or should a verified-contact mark exist?
4. **SHELL-2: uninstall.** The "Delete app data" box removes all of `%APPDATA%\Tincan`, every profile included. Confirm that this is wanted.
5. **Relays.** n0's public relays are the default. They are rate-limited with no SLA. Should Tincan run its own relay?

## Deferred (documented, not bugs)

- NET-SECURITY-6: closing idle connections with no conversation after 30 s. Skipped: legitimate DM watchers keep such links alive and would redial at the stranger rate.
- NET-SECURITY-9: merging authors by hlc in `send_missing`. The re-request rounds before SyncComplete cover the missing events.
- RESOURCES-10, optional part: answering HEAD for `http://tincan.localhost/file/<hash>` without reading the body. This needs a size/MIME accessor on `App`.

## Next steps

1. Run the installer on a clean Windows user or VM (not this machine's real profile). Check install, first run, the firewall prompt, SmartScreen, uninstall with and without "Delete app data", and an upgrade over an existing install.
2. Two machines on different networks: WAN hole punching, relay fallback, voice quality, and screen share over a relay.
3. Decide the open questions above; R3-OPEN-2 is the most visible to users.
4. An independent review of the round 3 fixes, which so far were reviewed only by their author.

## Not verified (needs real conditions)

- The installer and its uninstall hook have never run.
- WAN hole punching, and relay throughput for voice and video.
- Echo cancellation in a real room, and a real microphone level.
- The first-run Windows Firewall prompt, and SmartScreen on an unsigned installer.
- Hardware encoders on iGPU-only machines.
- 1:1 calls through the UI (in-process tests only).
- A host crash (as opposed to a clean close) mid-call.
- A real Windows logoff with the handoff (SHELL-1).
- The round 3 merge, flood and stranger paths (in-process tests only).

## Decisions made without the owner

See "Goals and defaults" in `architecture.md`. Notable:

- n0's public iroh relays are the default (a custom relay URL is a setting).
- The installer is per user and unsigned.
- The host can see room media in plaintext: there is no end-to-end encryption in v1.
- Attachments are capped at 25 MB.
- Downloaded files get the Mark of the Web, and only an allowlist of document and media types opens directly. Anything else is shown in Explorer.
