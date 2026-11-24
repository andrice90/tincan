// Keeps a panel's content mounted while its Floating close transition runs (lib/presence.ts).
// The wrapper element stays mounted and animates on `open`; `kept` is what goes inside it.

import { useEffect, useState, useSyncExternalStore } from "react";
import { closeDelayMs } from "../lib/layout";
import { initialPresence, presenceDone, presenceStep, type Presence } from "../lib/presence";
import { useStore } from "../store/store";

const REDUCE = "(prefers-reduced-motion: reduce)";

function subscribeReduce(cb: () => void): () => void {
  const mq = window.matchMedia(REDUCE);
  mq.addEventListener("change", cb);
  return () => mq.removeEventListener("change", cb);
}

/** The OS asks for no motion. */
export function useReducedMotion(): boolean {
  return useSyncExternalStore(
    subscribeReduce,
    () => window.matchMedia(REDUCE).matches,
    () => false,
  );
}

/** How long closing content stays mounted: 0 in Classic and under reduced motion. */
export function useCloseDelay(): number {
  const layout = useStore((s) => s.layout);
  const reduced = useReducedMotion();
  return closeDelayMs(layout, reduced);
}

export interface Shown<T> {
  /** The wrapper is open: true while `value` is set, false while closing and closed. */
  open: boolean;
  /** Render this: the value, or while closing the last one. Null once closed. */
  kept: T | null;
  closing: boolean;
}

/** Open while `value` is set; after it goes null the last value stays until the close is over. */
export function usePresence<T>(value: T | null, same?: (a: T, b: T) => boolean): Shown<T> {
  const delay = useCloseDelay();
  const [p, setP] = useState<Presence<T>>(() => initialPresence(value));
  // Derived during render, so the frame that closes the wrapper still has the content.
  const next = presenceStep(p, value, delay, same);
  if (next !== p) setP(next);

  const { phase, gen } = next;
  useEffect(() => {
    if (phase !== "closing") return;
    const t = window.setTimeout(() => setP((cur) => presenceDone(cur, gen)), delay);
    return () => window.clearTimeout(t);
  }, [phase, gen, delay]);

  return { open: next.phase === "open", kept: next.kept, closing: next.phase === "closing" };
}
