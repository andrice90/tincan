<p align="center">
  <img src="src-tauri/icons/icon.svg" width="128" height="128" alt="Tin Can logo: a tin can sending out sound waves">
</p>

<h1 align="center">Tin Can</h1>

<p align="center">Peer-to-peer group chat, voice and screen share for Windows. No accounts, and no server to run.</p>

Tin Can is a Discord-style app where each channel's members **are** the network. The most stable member acts as host. When the host leaves, another member takes over. Messages and files are stored on each member's PC and sync automatically.

## Features

- **Servers:** text channels and voice rooms. Every voice room also has its own text chat.
- **Direct messages:** one-to-one, plus 1:1 voice calls.
- **Screen sharing:** share a screen or a window in a voice room or a call, with its audio. A window share sends that app's audio; a screen share sends everything except Tin Can. Each viewer sets the volume per sharer.
- **Voice rooms you can look into:** opening a room shows who is inside and its chat; you join with **Join voice**.
- **AI noise suppression:** an optional neural noise filter for your microphone (Settings > Voice), next to the standard one. See [AI noise suppression](#ai-noise-suppression).
- **File attachments:** files travel peer to peer and keep downloading from other members who have them after the sender goes offline.
- **Soundboard:** play short clips into a call and stop them. Others pull a clip peer to peer and become a source for it, and set soundboard volume per person and per room.
- **Short codes:** 6-character server invite codes and friend codes. The owner rotates an invite code without current members losing access.
- **Owner tools:** remove a member (they can rejoin only with an invite that still works), rename and delete channels.
- **Avatars and server icons**, with a crop window.
- **Join and leave sounds** in voice rooms, if you add the sound files when you build (see [License](#license)).
- **Two layouts:** Floating (the default: rounded cards with animated panels) and Classic (flat panels), set in Settings > Appearance.

## Install

Download `Tincan_0.2.1_x64-setup.exe` from the [0.2.1 release](../../releases/tag/v0.2.1) and run it. Releases are alpha builds, marked pre-release on GitHub.

- Windows 10 or 11, 64-bit. Screen sharing needs Windows 10 version 2004 or newer.
- The installer is per user and needs no admin rights. It is not code-signed, so SmartScreen may warn about it: choose **More info**, then **Run anyway**.
- Every member of a server needs the same version. Older builds can't read events added in newer ones, and 0.1.0 can't hear 0.2.1's soundboard or share audio.

## Using it

1. Pick a display name.
2. Create a server, then open **Invite people** and share its 6-character code. Or join someone else's server with their code.
3. To message someone directly, add them by their friend code (Settings shows yours).

By default Tin Can uses n0's public iroh relays to find peers and as a fallback when a direct connection isn't possible. Those relays are rate-limited and come with no guarantee. A custom relay URL can be set in Settings > Network.

## AI noise suppression

Settings > Voice has an **AI noise suppression** switch under **Noise suppression**. It is off by default. When on, your microphone goes through DPDFNet, a neural speech enhancement model, instead of the standard WebRTC noise filter. It removes much more background noise, such as fans and keyboard clicks.

- It runs on your PC; the model is part of the app.
- It adds about 50 ms of delay to your voice, against about 7 ms for the standard filter.
- It uses about a third of one CPU core while you are in a call (measured on an Intel i9-12900K).
- It is new in 0.2.1 and has only been tested offline on generated speech, not yet in real calls. Turn it off if your voice sounds wrong.

## Build from source

You need Rust (1.91 or newer), Node 22 and pnpm 10. Run these from the repository root unless noted.

```sh
# Rust tests (all crates)
cargo test --workspace

# UI checks
cd ui && pnpm install && pnpm typecheck && pnpm test && pnpm build

# Dev run: Vite plus the debug app, using a separate "dev" profile
cd ui && TINCAN_PROFILE=dev pnpm tauri dev

# Release exe and NSIS installer (downloads the noise suppression model first)
cd ui && pnpm tauri build
# target/release/tincan.exe
# target/release/bundle/nsis/Tincan_0.2.1_x64-setup.exe
```

- **AI noise suppression model:** `scripts/fetch-dpdfnet.ps1` downloads DPDFNet (10 MB, pinned version, checked against its sha256) into `crates/tincan-audio/models/`, which is git-ignored, and the build embeds it. `pnpm tauri build` runs the script; for other builds run it yourself. Without the model the app builds and keeps WebRTC noise suppression only.
- **Profiles:** pass `--profile <name>` or set `TINCAN_PROFILE`. Each profile keeps its own data in `%APPDATA%\Tincan\profiles\<name>`.
- **Log file:** each run writes `logs\tincan.log` in its profile folder. `TINCAN_LOG=<filter>` changes what is logged and also prints the log to the console.

## How it works

- **Stack:** a Rust core (iroh QUIC, SQLite, Opus, Windows Graphics Capture and Media Foundation H.264) inside a Tauri 2 shell, with a React UI. All audio and capture is native; the webview only draws the UI and decoded video.
- **History:** each server's history is a signed, append-only event log that every member keeps.
- **Hosting:** the host orders events, relays voice and video for the members, and is elected again whenever it leaves.
- **Encryption:** media is not end-to-end encrypted in this version, so the host can read voice and video passing through it.

The docs in [`docs/`](docs) go further:

- [`architecture.md`](docs/architecture.md): design and the security model.
- [`api.md`](docs/api.md): the UI-to-app contract.
- [`design.md`](docs/design.md): screens and visual tokens.
- [`e2e.md`](docs/e2e.md): the two-profile end-to-end run.
- [`review/`](docs/review): security review findings and their status.
- [`HANDOFF.md`](docs/HANDOFF.md): current status and open questions.

## Status

Version 0.2.1 is an early test build:

- **Tested:** two profiles on one PC (offline and online), plus Rust and UI test suites.
- **Not tested yet:**
  - hole punching across networks;
  - relay throughput for voice and video;
  - echo cancellation in a real room;
  - AI noise suppression in real calls and on slower PCs;
  - the installer on a clean machine.

## License

Tin Can is licensed under the [Apache License, Version 2.0](LICENSE).

The release and builds made with the model include DPDFNet by Ceva (`dpdfnet2_48khz_hr`, [github.com/ceva-ip/DPDFNet](https://github.com/ceva-ip/DPDFNet), [huggingface.co/Ceva-IP/DPDFNet](https://huggingface.co/Ceva-IP/DPDFNet)), also under the Apache License, Version 2.0.

The repository and the release contain no third-party sounds, so the join and leave cues in voice rooms are silent by default. To hear them, put your own sounds in `crates/tincan-app/assets/` as `voice-join.wav` and `voice-leave.wav` (WAV, up to 10 seconds) before building. The build embeds them when both files are present; that folder is git-ignored.
