// "X is typing" bookkeeping: who is listed per conversation and when they drop off.
// Timers and storage are passed in, so the rules can be tested with a fake clock.

import type { TypingEntry } from "./store";

export interface TypingDeps {
  now(): number;
  setTimer(fn: () => void, ms: number): unknown;
  get(key: string): TypingEntry[] | undefined;
  set(key: string, list: TypingEntry[]): void;
}

/**
 * Tracks typers per conversation key and removes each one when their entry runs out. One
 * sweep timer per conversation, armed for the earliest entry: a new typing event never
 * pushes it back, so someone who stopped drops off while another person keeps typing.
 */
export function typingTracker(deps: TypingDeps): { typed(key: string, entry: TypingEntry): void } {
  const timers = new Map<string, unknown>();
  const arm = (key: string, list: TypingEntry[] | undefined): void => {
    if (!list || list.length === 0) return;
    const next = Math.min(...list.map((x) => x.until)) - deps.now();
    timers.set(key, deps.setTimer(() => sweep(key), Math.max(50, next)));
  };
  const sweep = (key: string): void => {
    timers.delete(key);
    const list = deps.get(key);
    if (!list) return;
    const t = deps.now();
    const live = list.filter((x) => x.until > t);
    if (live.length !== list.length) deps.set(key, live);
    arm(key, live);
  };
  return {
    typed(key, entry) {
      const list = [...(deps.get(key) ?? []).filter((t) => t.userId !== entry.userId), entry];
      deps.set(key, list);
      if (!timers.has(key)) arm(key, list);
    },
  };
}
