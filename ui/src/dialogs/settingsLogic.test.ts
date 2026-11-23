import type { Connection } from "../api/types";
import { eq, ok, test } from "../test/harness";
import { FETCH_LIMITS, MAX_ATTACHMENT_BYTES, connStatus, debounced, fetchLimitOptions } from "./settingsLogic";

test("UI-LOGIC-12: the auto-download menu offers nothing the app would clamp", () => {
  ok(FETCH_LIMITS.every((n) => n <= MAX_ATTACHMENT_BYTES), `limits ${FETCH_LIMITS.join(", ")}`);
  eq(FETCH_LIMITS[FETCH_LIMITS.length - 1], MAX_ATTACHMENT_BYTES, "25 MB is the top choice");
  eq(fetchLimitOptions(3 * 1024 * 1024).length, FETCH_LIMITS.length + 1, "odd stored value still listed");
});

const conn = (state: Connection["state"], path: Connection["path"], rttMs: number | null = null): Connection => ({
  state,
  hostId: null,
  hostName: null,
  path,
  rttMs,
});

test("SPEC-GAPS-9: a server this PC hosts shows how many people are connected, not No peers", () => {
  eq(connStatus(conn("hosting", "none"), 4), { text: "3 people connected", cls: "muted" });
  eq(connStatus(conn("hosting", "none"), 2).text, "1 person connected");
});

test("SPEC-GAPS-9: members keep the path and round trip", () => {
  eq(connStatus(conn("member", "direct", 24), 5), { text: "Direct, 24 ms", cls: "conn-direct" });
  eq(connStatus(conn("member", "relay"), 5), { text: "Relayed", cls: "conn-relay" });
  eq(connStatus(conn("solo", "none"), 1), { text: "No peers", cls: "faint" });
  eq(connStatus(conn("connecting", "none"), 3), { text: "No peers", cls: "faint" });
});

test("UI-LOGIC-11: closing Settings right after moving a slider still saves the value", () => {
  const saved: number[] = [];
  const pending = new Map<number, () => void>();
  let seq = 0;
  const d = debounced<number>((v) => saved.push(v), 250, {
    set: (fn) => {
      pending.set(++seq, fn);
      return seq;
    },
    clear: (t) => void pending.delete(t as number),
  });
  d.push(1.2);
  d.push(1.5);
  d.flush(); // the slider unmounts within 250 ms
  eq(saved, [1.5]);
  eq(pending.size, 0, "no timer left");
  d.flush();
  eq(saved, [1.5], "nothing pending: no second save");
});
