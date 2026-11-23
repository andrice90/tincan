// Pure parts of the Settings dialog, so they can be tested.

import type { Connection } from "../api/types";

const MB = 1024 * 1024;

/** The app clamps autoFetchLimit to this (MAX_ATTACHMENT_BYTES in tincan-app). */
export const MAX_ATTACHMENT_BYTES = 25 * MB;

/** Auto-download choices. Nothing above the app's clamp: attachments can't be larger anyway. */
export const FETCH_LIMITS = [0, 5 * MB, 10 * MB, 25 * MB];

/** The choices of the auto-download menu, with an odd stored value included. */
export function fetchLimitOptions(current: number): number[] {
  return FETCH_LIMITS.includes(current) ? FETCH_LIMITS : [...FETCH_LIMITS, current].sort((a, b) => a - b);
}

/**
 * The connection column of Settings > Network for one server. A hosting PC has no path of
 * its own (the app reports "none"), so it shows how many others are connected instead.
 */
export function connStatus(c: Connection, onlineCount: number): { text: string; cls: string } {
  if (c.state === "hosting") {
    const n = Math.max(0, onlineCount - 1);
    if (n > 0) return { text: n === 1 ? "1 person connected" : `${n} people connected`, cls: "muted" };
  }
  const path = c.path === "direct" ? "Direct" : c.path === "relay" ? "Relayed" : null;
  if (!path) return { text: "No peers", cls: "faint" };
  return { text: c.rttMs !== null ? `${path}, ${c.rttMs} ms` : path, cls: c.path === "direct" ? "conn-direct" : "conn-relay" };
}

/** Calls `fn` with the last value `ms` after the last change; flush() runs a pending call now. */
export function debounced<T>(
  fn: (v: T) => void,
  ms: number,
  timers: { set(fn: () => void, ms: number): unknown; clear(t: unknown): void },
): { push(v: T): void; flush(): void } {
  let timer: unknown = null;
  let last: T | undefined;
  let has = false;
  return {
    push(v) {
      last = v;
      has = true;
      if (timer !== null) timers.clear(timer);
      timer = timers.set(() => {
        timer = null;
        has = false;
        fn(v);
      }, ms);
    },
    flush() {
      if (timer === null) return;
      timers.clear(timer);
      timer = null;
      if (has) {
        has = false;
        fn(last as T);
      }
    },
  };
}
