import { eq, test } from "../test/harness";
import type { TypingEntry } from "./store";
import { typingTracker } from "./typing";

/** A fake clock with timers. */
function fakeClock() {
  let t = 0;
  let seq = 0;
  const timers = new Map<number, { at: number; fn: () => void }>();
  return {
    now: () => t,
    setTimer: (fn: () => void, ms: number) => {
      const id = ++seq;
      timers.set(id, { at: t + ms, fn });
      return id;
    },
    clearTimer: (id: unknown) => {
      timers.delete(id as number);
    },
    /** Moves time forward, firing due timers in order. */
    advance(ms: number) {
      const end = t + ms;
      for (;;) {
        let next: [number, { at: number; fn: () => void }] | null = null;
        for (const e of timers) if (e[1].at <= end && (!next || e[1].at < next[1].at)) next = e;
        if (!next) break;
        timers.delete(next[0]);
        t = next[1].at;
        next[1].fn();
      }
      t = end;
    },
  };
}

const SHOW = 6000;

function setup() {
  const clock = fakeClock();
  const lists = new Map<string, TypingEntry[]>();
  const tracker = typingTracker({ ...clock, get: (k) => lists.get(k), set: (k, l) => void lists.set(k, l) });
  const typed = (userId: string) => tracker.typed("c", { userId, name: userId, until: clock.now() + SHOW });
  const names = () => (lists.get("c") ?? []).map((e) => e.name);
  return { clock, typed, names };
}

test("UI-LOGIC-06: someone who stopped typing drops off while another keeps typing", () => {
  const { clock, typed, names } = setup();
  typed("alice");
  typed("bob");
  // Bob keeps typing: one event every 3 s
  for (let i = 0; i < 4; i++) {
    clock.advance(3000);
    typed("bob");
  }
  // 12 s in: Alice's entry ran out at 6 s
  eq(names(), ["bob"]);
});

test("UI-LOGIC-06: the list empties once everyone stopped", () => {
  const { clock, typed, names } = setup();
  typed("alice");
  clock.advance(2000);
  typed("bob");
  clock.advance(4100);
  eq(names(), ["bob"], "Alice gone at 6 s");
  clock.advance(2000);
  eq(names(), [], "Bob gone at 8 s");
});

test("UI-LOGIC-06: a typer who keeps typing stays listed", () => {
  const { clock, typed, names } = setup();
  for (let i = 0; i < 6; i++) {
    typed("alice");
    clock.advance(3000);
  }
  eq(names(), ["alice"]);
});
