// Grab bar on a side panel's edge that faces the content: drag, arrow keys, Home and End
// resize; a double click resets. The width is saved when a drag ends or a key changes it.

import { useCallback, useRef, useState, type KeyboardEvent, type PointerEvent } from "react";
import {
  clampWidth,
  dragWidth,
  keyWidth,
  loadWidth,
  saveWidth,
  type Edge,
  type PanelBounds,
  type WidthStore,
} from "../lib/panelWidth";

function storage(): WidthStore | undefined {
  try {
    return window.localStorage;
  } catch {
    return undefined;
  }
}

export interface PanelWidth {
  width: number;
  /** Applies a width now; `save` also stores it. */
  set: (width: number, save: boolean) => void;
}

/** A panel width read once from storage at mount, so the first paint already has it. */
export function usePanelWidth(key: string, bounds: PanelBounds): PanelWidth {
  const [width, setWidth] = useState(() => loadWidth(storage(), key, bounds));
  const set = useCallback(
    (w: number, save: boolean) => {
      const next = clampWidth(w, bounds);
      setWidth(next);
      if (save) saveWidth(storage(), key, next, bounds);
    },
    [key, bounds],
  );
  return { width, set };
}

interface ResizeHandleProps {
  panel: PanelWidth;
  bounds: PanelBounds;
  edge: Edge;
  label: string;
}

/** A focusable vertical separator that resizes `panel`. Place it inside the panel. */
export function ResizeHandle({ panel, bounds, edge, label }: ResizeHandleProps) {
  const drag = useRef<{ startX: number; startWidth: number; last: number; pointerId: number } | null>(null);
  const [dragging, setDragging] = useState(false);

  const onPointerDown = (e: PointerEvent<HTMLDivElement>) => {
    if (e.button !== 0) return;
    e.preventDefault();
    e.currentTarget.setPointerCapture(e.pointerId);
    // Start from the width on screen: in a narrow window the panel may be narrower than saved.
    const shown = e.currentTarget.parentElement?.getBoundingClientRect().width;
    const start = shown ? clampWidth(shown, bounds) : panel.width;
    drag.current = { startX: e.clientX, startWidth: start, last: start, pointerId: e.pointerId };
    setDragging(true);
  };

  const onPointerMove = (e: PointerEvent<HTMLDivElement>) => {
    const d = drag.current;
    if (!d || d.pointerId !== e.pointerId) return;
    const w = dragWidth(d.startWidth, d.startX, e.clientX, edge, bounds);
    if (w === d.last) return;
    d.last = w;
    panel.set(w, false);
  };

  const end = (e: PointerEvent<HTMLDivElement>) => {
    const d = drag.current;
    if (!d || d.pointerId !== e.pointerId) return;
    drag.current = null;
    setDragging(false);
    if (d.last !== d.startWidth) panel.set(d.last, true);
  };

  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    const w = keyWidth(e.key, panel.width, edge, bounds);
    if (w === null) return;
    e.preventDefault();
    panel.set(w, true);
  };

  return (
    <div
      className={`resize-handle is-${edge}${dragging ? " is-dragging" : ""}`}
      role="separator"
      aria-orientation="vertical"
      aria-label={label}
      aria-valuenow={panel.width}
      aria-valuemin={bounds.min}
      aria-valuemax={bounds.max}
      tabIndex={0}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={end}
      onPointerCancel={end}
      onDoubleClick={() => panel.set(bounds.initial, true)}
      onKeyDown={onKeyDown}
    />
  );
}
