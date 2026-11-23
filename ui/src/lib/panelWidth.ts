// Widths of the resizable side panels: bounds, clamping, keyboard steps and the saved
// value. Storage is passed in so the rules can be tested without a browser. Each Tauri
// profile has its own WebView2 data folder, so a plain localStorage key is per profile.

/** Size limits of one panel, in CSS pixels. */
export interface PanelBounds {
  min: number;
  max: number;
  /** Today's fixed width, used until the user drags and after a reset. */
  initial: number;
  /** Arrow key step. */
  step: number;
}

/** The part of `Storage` the width needs. */
export interface WidthStore {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
  removeItem(key: string): void;
}

/** Which edge of the panel the grab bar sits on (the one facing the content). */
export type Edge = "right" | "left";

/** Channel and DM list. Matches `--side-w` before it was resizable. */
export const SIDEBAR_BOUNDS: PanelBounds = { min: 200, max: 420, initial: 240, step: 16 };

/**
 * Member list. Not narrower than its old fixed width: below 220 px the "(you)", Host and
 * Owner tags leave no room for the name.
 */
export const MEMBERS_BOUNDS: PanelBounds = { min: 220, max: 360, initial: 220, step: 16 };

export const SIDEBAR_KEY = "tincan.sidebarWidth";
export const MEMBERS_KEY = "tincan.membersWidth";

/** A whole-pixel width inside the bounds; anything not a finite number gives the initial width. */
export function clampWidth(width: number, b: PanelBounds): number {
  if (!Number.isFinite(width)) return b.initial;
  return Math.min(b.max, Math.max(b.min, Math.round(width)));
}

/** The saved width, or the initial one when nothing usable was saved. */
export function parseWidth(raw: string | null, b: PanelBounds): number {
  if (raw === null || !/^\s*\d+(\.\d+)?\s*$/.test(raw)) return b.initial;
  return clampWidth(Number(raw), b);
}

/** Reads the saved width. Storage that throws (blocked, private mode) gives the initial width. */
export function loadWidth(store: WidthStore | undefined, key: string, b: PanelBounds): number {
  if (!store) return b.initial;
  try {
    return parseWidth(store.getItem(key), b);
  } catch {
    return b.initial;
  }
}

/** Saves a width; the initial width is saved as no value, so a reset follows later default changes. */
export function saveWidth(store: WidthStore | undefined, key: string, width: number, b: PanelBounds): void {
  if (!store) return;
  const w = clampWidth(width, b);
  try {
    if (w === b.initial) store.removeItem(key);
    else store.setItem(key, String(w));
  } catch {
    // Storage full or blocked: the width still applies for this session.
  }
}

/**
 * The width after a key press on the grab bar, or null when the key does nothing.
 * Arrows move the bar the way they point, so on a left-edge bar ArrowLeft widens.
 */
export function keyWidth(key: string, width: number, edge: Edge, b: PanelBounds): number | null {
  const grow = edge === "right" ? 1 : -1;
  switch (key) {
    case "ArrowRight":
      return clampWidth(width + grow * b.step, b);
    case "ArrowLeft":
      return clampWidth(width - grow * b.step, b);
    case "Home":
      return b.min;
    case "End":
      return b.max;
    default:
      return null;
  }
}

/** The width while dragging: the start width moved by how far the pointer went, toward the content. */
export function dragWidth(startWidth: number, startX: number, x: number, edge: Edge, b: PanelBounds): number {
  const delta = x - startX;
  return clampWidth(startWidth + (edge === "right" ? delta : -delta), b);
}
