# Handoff

Status of the Tincan build for the next session. Updated at every milestone commit; the newest state is at the top.

## Where it stands

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
- `pnpm test`: 62 passed.

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
