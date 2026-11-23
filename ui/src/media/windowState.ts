// Whether the app window is minimized. WebView2 keeps the page "visible" and
// rAF running when the Tauri window is minimized, so video drawing pauses from
// the window's own state instead of the Page Visibility API.

import { isTauri } from "../api/client";

let hidden = false;
let started = false;

function start(): void {
  if (started) return;
  started = true;
  if (!isTauri) {
    const sync = () => {
      hidden = document.visibilityState === "hidden";
    };
    document.addEventListener("visibilitychange", sync);
    sync();
    return;
  }
  void import("@tauri-apps/api/window")
    .then(async ({ getCurrentWindow }) => {
      const w = getCurrentWindow();
      const sync = () => {
        w.isMinimized().then(
          (m) => {
            hidden = m;
          },
          () => {
            hidden = false;
          },
        );
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
