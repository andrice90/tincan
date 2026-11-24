// Open and close state of a panel whose content must stay on screen while it closes.
// The content (`kept`) is the last value shown: when the value goes away (I left voice,
// the member list was hidden) the panel keeps rendering it until the close transition is
// over, then unmounts it. Each close gets a generation, so the timer of an earlier close
// never unmounts a panel that was opened again meanwhile.

export type Phase = "closed" | "open" | "closing";

export interface Presence<T> {
  phase: Phase;
  /** What to render: the current value while open, the last one while closing. */
  kept: T | null;
  /** Bumped at each close; a finished timer only counts for its own close. */
  gen: number;
}

export function closedPresence<T>(): Presence<T> {
  return { phase: "closed", kept: null, gen: 0 };
}

/** A presence that starts open on `value` (or closed when it is null). */
export function initialPresence<T>(value: T | null): Presence<T> {
  return value === null ? closedPresence<T>() : { phase: "open", kept: value, gen: 0 };
}

/**
 * The presence after the value became `value`. Returns `p` itself when nothing changed,
 * so it can run on every render. `delayMs` 0 closes at once (Classic, reduced motion).
 * `same` tells a new value that shows the same thing from a change.
 */
export function presenceStep<T>(
  p: Presence<T>,
  value: T | null,
  delayMs: number,
  same: (a: T, b: T) => boolean = Object.is,
): Presence<T> {
  if (value !== null) {
    if (p.phase === "open" && p.kept !== null && same(p.kept, value)) return p;
    return { phase: "open", kept: value, gen: p.gen };
  }
  if (p.phase === "open") {
    if (delayMs <= 0) return { phase: "closed", kept: null, gen: p.gen + 1 };
    return { phase: "closing", kept: p.kept, gen: p.gen + 1 };
  }
  if (p.phase === "closing" && delayMs <= 0) return { phase: "closed", kept: null, gen: p.gen };
  return p;
}

/** The close timer of generation `gen` ran out: unmount, unless the panel opened or closed again since. */
export function presenceDone<T>(p: Presence<T>, gen: number): Presence<T> {
  if (p.phase !== "closing" || p.gen !== gen) return p;
  return { phase: "closed", kept: null, gen: p.gen };
}
