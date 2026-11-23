// Whether the app window is minimized. WebView2 keeps the page "visible" and
// rAF running when the Tauri window is minimized, so video pauses from the
// window's own state instead of the Page Visibility API.

import { isTauri } from "../api/client";

let hidden = false;
let started = false;
const listeners = new Set<(hidden: boolean) => void>();

function setHidden(next: boolean): void {
  if (next === hidden) return;
  hidden = next;
  for (const cb of listeners) cb(next);
}

function start(): void {
  if (started) return;
  started = true;
  if (!isTauri) {
    const sync = () => setHidden(document.visibilityState === "hidden");
    document.addEventListener("visibilitychange", sync);
    sync();
    return;
  }
  void import("@tauri-apps/api/window")
    .then(async ({ getCurrentWindow }) => {
      const w = getCurrentWindow();
      const sync = () => {
        w.isMinimized().then(setHidden, () => setHidden(false));
      };
      sync();
      await w.onResized(sync);
      await w.onFocusChanged(sync);
    })
    .catch((err: unknown) => console.warn("window state unavailable", err));
}

/** True while the window is minimized (Tauri) or the tab is hidden (browser). */
export function isWindowHidden(): boolean {
  start();
  return hidden;
}

/** Calls `cb` when the window is minimized or restored. Returns the unsubscribe. */
export function onWindowHidden(cb: (hidden: boolean) => void): () => void {
  start();
  listeners.add(cb);
  return () => {
    listeners.delete(cb);
  };
}
