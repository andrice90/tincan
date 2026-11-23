# Handoff

Status of the Tincan build for the next session. Updated at every milestone commit; the newest state is at the top.

## Where it stands

Work in progress (2026-11-23). Every feature in the original request is built and runs end to end between two profiles on one machine. Two review rounds found security and correctness problems. Round 1 (tincan-net and tincan-core: host takeover, DoS, trust in the host) is fixed. Round 2 (tincan-app, the shell and the UI) is being fixed now. Its status is tracked per finding in [`review/findings.md`](review/findings.md).

| Step | State |
|---|---|
| Core crates (core, store, net, files, audio, screen) | done, tested |
| App orchestrator (`crates/tincan-app`) and Tauri shell (`src-tauri`) | done, tested |
| Two-profile UI end-to-end run (`ui/e2e`) | 49/49 checks passed (before the round 2 fixes) |
| Round 1 hardening of tincan-net | done |
| Round 2 fixes (app, shell, UI) | in progress, see `review/findings.md` |
| unslop-ui pass on the UI | pending |
| NSIS installer (`pnpm tauri build`) | pending |

## Build, run, test

All commands from the repository root unless noted. A per-task `CARGO_TARGET_DIR` keeps parallel builds apart (`target/` is git-ignored).

```sh
# Rust tests (everything; about 5 min on this machine)
CARGO_TARGET_DIR=target/a-int cargo test --workspace

# UI
cd ui && pnpm install && pnpm typecheck && pnpm build

# Dev run (Vite on :5173 plus the debug app; profile "dev")
cd ui && TINCAN_PROFILE=dev pnpm tauri dev

# Debug exe with the UI embedded (what the e2e run uses)
cd ui && pnpm build && cd ..
CARGO_TARGET_DIR=target/a-int cargo build -p tincan --features tauri/custom-protocol
target/a-int/debug/tincan.exe --profile alice

# Two-profile UI end-to-end run: see docs/e2e.md
TINCAN_E2E_DEPS=<folder with node_modules/playwright-core> node ui/e2e/e2e.mjs

# Installer (NSIS, per user, unsigned)
cd ui && pnpm tauri build
```

`cargo test --workspace` relinks `target/a-int/debug/tincan.exe` without the embedded UI. Rebuild with `--features tauri/custom-protocol` before an e2e run (the script detects a stale exe).

Profiles: `--profile <name>` or `TINCAN_PROFILE`, data in `%APPDATA%\Tincan\profiles\<name>`. Test switches: `TINCAN_OFFLINE_NETWORK=1` (loopback only, no relays, no firewall prompt), `TINCAN_NO_DEVICES=1` (synthetic audio, no capture), `TINCAN_LOG=<tracing filter>` (logs to stderr).

## Docs map

- `architecture.md`: design, defaults chosen without asking, security model, what is not verified.
- `api.md`, `ui/src/api/types.ts`: the UI to app contract.
- `design.md`: screens and tokens.
- `notes-*.md`: per-crate integration notes, written by the agents that built each crate.
- `e2e.md`: the two-profile run, its timings and resource numbers.
- `review/`: confirmed review findings and their status.

## Resuming the in-flight work

The round 2 fix workflow (run `wf_3e0c42f2-14e`, script under `~/.claude/projects/D--code-projects-tincan/<session>/workflows/scripts/tincan-fix-round2-wf_3e0c42f2-14e.js`) was running when the usage limit hit. Its four fixers work in git worktrees and leave branches `fix/net`, `fix/files-sec`, `fix/ui`, `fix/shell` (check `git branch --list fix/*`). If the workflow cannot be resumed, merge those branches into main yourself, then do the remaining steps below. Also open: `ui/src/lib/names.ts` `nameKey` (on `fix/ui`) does not fold lookalike letters (Cyrillic "А" vs Latin "A"), so the duplicate-name suffix can be dodged; fold a UTS #39 confusables subset there, with a test.

## Next steps

1. Finish the round 2 fixes, highest severity first, then run a review of the fixes.
2. unslop-ui pass on every screen (copy and surface), keeping `ui/e2e` selectors in step.
3. Rerun the full test suite and the two-profile e2e.
4. Build the NSIS installer, smoke-test the release exe, measure release idle memory against the 30 MB target.

## Not verified (needs real conditions)

WAN hole punching, relay throughput for voice and video, echo cancellation in a real room, the first-run Windows Firewall prompt, SmartScreen on an unsigned installer, hardware encoders on iGPU-only machines, a real microphone level, 1:1 calls through the UI, a host crash (as opposed to a clean close) mid-call.

## Decisions made without the owner

See "Goals and defaults" in `architecture.md`. Notable: n0's public iroh relays are the default (rate-limited hobby tier, no SLA; a custom relay URL is a setting), the installer is per user and unsigned, and the host can see room media in plaintext (no end-to-end encryption in v1).
