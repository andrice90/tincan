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

## Mock backend

Used automatically outside Tauri (`window.__TAURI_INTERNALS__` missing). Loaded with a dynamic import, so the production bundle only fetches it in a browser.

URL flags: `?fresh` (name picker, no servers), `?noenc` (screen_start fails with `no-hardware-encoder`), `?ring` (incoming call from Alex). Console helpers on `window.tincanMock`: `ring(name)`, `online(name)`, `offline(name)` (Riley coming online delivers pending DMs), `migrate()` (Game night host moves to Morgan), `toast(text)`.

Join errors: codes shorter than 40 chars, codes not starting with `tincan1`, `tincan1u...` contact codes, and codes containing `offline` are rejected with the matching message.

## Choices made where the docs disagree or are silent

- Clip limits shown at the add step: 20 s and 5 MB (architecture.md), not the 10 s / 2 MB in design.md. Text lives in `CLIP_LIMIT_TEXT` (`src/dialogs/SoundDialogs.tsx`).
- Mute and deafen live in the call panel only (design.md also lists a mic button in the user panel; two mute buttons side by side were redundant).
- Opening a voice room joins it when you are not already in a room or a call.
- No typing indicator for DMs: `typing` needs a non-null `channelId`.
