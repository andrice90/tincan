// The window layout: Classic (flat panels, the default) or Floating (every region a
// card with a gap around it, panels that slide open and closed). Saved in localStorage
// like the panel widths, so it is per profile. Storage is passed in for the tests.

import type { WidthStore } from "./panelWidth";

export type Layout = "classic" | "floating";

export const LAYOUT_KEY = "tincan.layout";

/** Settings > Appearance order. */
export const LAYOUTS: readonly Layout[] = ["classic", "floating"];

/**
 * How long a Floating panel takes to open or close, in ms. Matches `--motion-panel` in
 * tokens.css: the close delay below unmounts the content only after the CSS transition.
 */
export const PANEL_MOTION_MS = 220;

/** Margin after the transition before closing content is unmounted. */
const CLOSE_SLACK_MS = 40;

/** The saved layout; anything else is Classic. */
export function parseLayout(raw: string | null): Layout {
  return raw === "floating" ? "floating" : "classic";
}

/** Reads the saved layout. Missing or throwing storage gives Classic. */
export function loadLayout(store: WidthStore | undefined): Layout {
  if (!store) return "classic";
  try {
    return parseLayout(store.getItem(LAYOUT_KEY));
  } catch {
    return "classic";
  }
}

/** Saves the layout; Classic, the default, is saved as no value. */
export function saveLayout(store: WidthStore | undefined, layout: Layout): void {
  if (!store) return;
  try {
    if (layout === "classic") store.removeItem(LAYOUT_KEY);
    else store.setItem(LAYOUT_KEY, layout);
  } catch {
    // Storage full or blocked: the layout still applies for this session.
  }
}

/**
 * How long closing content stays mounted. Classic keeps today's instant panels, and
 * reduced motion is instant in both layouts: 0 unmounts at once.
 */
export function closeDelayMs(layout: Layout, reducedMotion: boolean): number {
  if (layout !== "floating" || reducedMotion) return 0;
  return PANEL_MOTION_MS + CLOSE_SLACK_MS;
}

/** `localStorage`, or undefined where it is missing or blocked. */
export function browserStorage(): WidthStore | undefined {
  try {
    return (globalThis as { localStorage?: WidthStore }).localStorage;
  } catch {
    return undefined;
  }
}
