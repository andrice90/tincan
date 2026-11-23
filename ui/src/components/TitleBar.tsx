// Custom title bar (window decorations are off). data-tauri-drag-region="deep"
// makes the whole bar drag the window and double-click toggle maximize (Tauri's
// drag script does both); the buttons are clickable elements, so they don't drag.

import { Copy, Minus, Square, X } from "lucide-react";
import { useEffect, useState } from "react";
import { isTauri } from "../api/client";
import { windowClose, windowMinimize, windowToggleMaximize } from "../store/actions";

/** True while the Tauri window is maximized. Always false in a browser. */
function useMaximized(): boolean {
  const [max, setMax] = useState(false);
  useEffect(() => {
    if (!isTauri) return;
    let off: (() => void) | null = null;
    let cancelled = false;
    void import("@tauri-apps/api/window").then(async ({ getCurrentWindow }) => {
      const w = getCurrentWindow();
      const sync = () => {
        w.isMaximized().then(
          (m) => {
            if (!cancelled) setMax(m);
          },
          () => {},
        );
      };
      sync();
      const un = await w.onResized(sync);
      if (cancelled) un();
      else off = un;
    });
    return () => {
      cancelled = true;
      off?.();
    };
  }, []);
  return max;
}

export function TitleBar() {
  const maximized = useMaximized();
  return (
    <header className="titlebar" data-tauri-drag-region="deep">
      <div className="titlebar-title">
        <img src="/icon.svg" alt="" width={16} height={16} draggable={false} />
        <span>Tincan</span>
      </div>
      <div className="titlebar-buttons">
        <button type="button" className="titlebar-btn" aria-label="Minimize" title="Minimize" onClick={windowMinimize}>
          <Minus size={16} strokeWidth={1.5} />
        </button>
        <button
          type="button"
          className="titlebar-btn"
          aria-label={maximized ? "Restore" : "Maximize"}
          title={maximized ? "Restore" : "Maximize"}
          onClick={windowToggleMaximize}
        >
          {maximized ? <Copy size={14} strokeWidth={1.5} /> : <Square size={13} strokeWidth={1.5} />}
        </button>
        <button type="button" className="titlebar-btn is-close" aria-label="Close" title="Close" onClick={windowClose}>
          <X size={16} strokeWidth={1.5} />
        </button>
      </div>
    </header>
  );
}
