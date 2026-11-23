import type { Message } from "../api/types";
import { eq, ok, test } from "../test/harness";
import { OFFSCREEN_MAX, applyLive, keepOnSnapshot, mergeNewest, olderPageApplies, prependOlder, refreshSpan } from "./messages";
import type { ConvMessages } from "./store";

function msg(n: number, extra: Partial<Message> = {}): Message {
  return {
    id: `m${n}`,
    convId: "srv",
    channelId: "ch",
    authorId: "alice",
    authorName: "Alice",
    text: `message ${n}`,
    attachments: [],
    sentAt: 1000 + n,
    deleted: false,
    pending: false,
    ...extra,
  };
}

const range = (from: number, to: number) => Array.from({ length: to - from + 1 }, (_, i) => msg(from + i));
const ids = (list: Message[]) => list.map((m) => m.id);
const entry = (list: Message[], cursor: string | null, extra: Partial<ConvMessages> = {}): ConvMessages => ({
  list,
  cursor,
  loaded: true,
  loadingOlder: false,
  ...extra,
});

// ------------------------------------------------------------------ UI-LOGIC-02

test("UI-LOGIC-02: a message that arrives while the first page loads is kept", () => {
  const c = applyLive(undefined, msg(31), { loading: true, shown: true });
  ok(c !== null && c !== undefined, "an unloaded entry is created");
  eq(c && ids(c.list), ["m31"]);
  eq(c?.loaded, false, "still loading");
  // without a load in flight nothing is cached (the next view loads a page)
  eq(applyLive(undefined, msg(31), { loading: false, shown: true }), undefined);
});

test("UI-LOGIC-02: the first page merges with live messages, newest last", () => {
  const page = { messages: range(2, 30), cursor: "c2" };
  const merged = mergeNewest(page, [msg(31)]);
  eq(ids(merged.list), ids(range(2, 31)));
  eq(merged.cursor, "c2");
});

test("UI-LOGIC-02: delivery and deletion seen live are not undone by an older page copy", () => {
  const page = { messages: [msg(1, { pending: true }), msg(2)], cursor: null };
  const merged = mergeNewest(page, [msg(1, { pending: false }), msg(2, { deleted: true })]);
  eq(merged.list.map((m) => [m.pending, m.deleted]), [
    [false, false],
    [false, true],
  ]);
});

test("UI-LOGIC-02: a live update for a message older than the page is dropped, not appended", () => {
  const page = { messages: range(10, 39), cursor: "c10" };
  const merged = mergeNewest(page, [msg(3, { deleted: true }), msg(40)]);
  eq(ids(merged.list), [...ids(range(10, 39)), "m40"]);
  // when the page reaches the start, nothing is older than it
  const all = mergeNewest({ messages: range(10, 12), cursor: null }, [msg(3)]);
  eq(ids(all.list), ["m3", "m10", "m11", "m12"]);
});

// ------------------------------------------------------------------ UI-LOGIC-04

test("UI-LOGIC-04: an older page is only prepended to the entry it was asked for", () => {
  const c = entry(range(41, 70), "c41");
  ok(olderPageApplies(c, "c41", true), "same entry");
  ok(!olderPageApplies(c, "c11", true), "deep page landing on a fresh newest page");
  ok(!olderPageApplies(c, "c41", false), "read before a snapshot");
  ok(!olderPageApplies(undefined, "c41", true), "entry dropped");
});

test("UI-LOGIC-04: an older page goes in front and keeps sent order", () => {
  const c = entry([msg(5), ...range(41, 70)], "c41", { loadingOlder: true });
  const next = prependOlder(c, { messages: range(11, 40), cursor: "c11" });
  eq(ids(next.list).slice(0, 3), ["m5", "m11", "m12"]);
  eq(next.list.length, 61);
  eq([next.cursor, next.loadingOlder], ["c11", false]);
});

// ------------------------------------------------------------------ UI-LOGIC-05

test("UI-LOGIC-05: a snapshot keeps only the cache of the conversation on screen", () => {
  const messages = { "a:1": entry(range(1, 3), null), "b:2": entry(range(4, 6), null) };
  eq(Object.keys(keepOnSnapshot(messages, "a:1")), ["a:1"]);
  eq(Object.keys(keepOnSnapshot(messages, null)), []);
  eq(Object.keys(keepOnSnapshot(messages, "c:3")), []);
});

test("UI-LOGIC-05: re-reading the kept span keeps the first row and its cursor", () => {
  // scrolled back: 41..100 cached; a delete and a DM ack were missed; 101..103 arrived since
  const cached = entry(range(41, 100), "c41");
  const fresh = [...range(30, 103)].map((m) => (m.id === "m50" ? { ...m, deleted: true } : m));
  const next = refreshSpan(cached, { messages: fresh, cursor: "c30" }, "m41");
  ok(next !== null, "kept");
  eq(next && ids(next.list)[0], "m41", "first row unchanged");
  eq(next?.cursor, "c41", "old cursor still points before m41");
  eq(next && next.list.length, 63);
  eq(next?.list.find((m) => m.id === "m50")?.deleted, true, "missed delete applied");
});

test("UI-LOGIC-05: a message inserted into the kept span (partition merge) appears", () => {
  const cached = entry(range(41, 60), "c41");
  const inserted = msg(50, { id: "late", sentAt: 1050.5 });
  const fresh = [...range(41, 60), inserted].sort((a, b) => a.sentAt - b.sentAt);
  const next = refreshSpan(cached, { messages: fresh, cursor: "c41b" }, "m41");
  ok(next?.list.some((m) => m.id === "late") ?? false, "inserted message shown");
});

test("UI-LOGIC-05: when the span can't be matched the entry is replaced, never patched", () => {
  const cached = entry(range(41, 60), "c41");
  const next = refreshSpan(cached, { messages: range(500, 529), cursor: "c500" }, "m41");
  eq(next && ids(next.list), ids(range(500, 529)));
  eq(next?.cursor, "c500");
});

test("UI-LOGIC-05: live messages since the refresh started are kept, loadingOlder of a replaced entry is reset", () => {
  const cached = entry([...range(41, 60), msg(61)], "c41", { loadingOlder: true });
  const kept = refreshSpan(cached, { messages: range(41, 60), cursor: "c40" }, "m41");
  eq(kept && ids(kept.list).slice(-2), ["m60", "m61"], "live message after the read");
  eq(kept?.loadingOlder, true, "same cursor: the older page in flight still applies");
  const replaced = refreshSpan(cached, { messages: range(500, 529), cursor: "c500" }, "m41");
  eq(replaced?.loadingOlder, false, "new cursor: nothing in flight applies");
});

test("UI-LOGIC-05: a cache that held the whole history takes the fresh page whole", () => {
  const cached = entry(range(1, 10), null);
  const next = refreshSpan(cached, { messages: [msg(0), ...range(1, 10)], cursor: null }, "m1");
  eq(next && ids(next.list)[0], "m0");
  eq(next?.cursor, null);
});

// ------------------------------------------------------------------ UI-LOGIC-15

test("UI-LOGIC-15: a large cache of a conversation off screen is dropped instead of grown", () => {
  const big = entry(range(1, OFFSCREEN_MAX), "c1");
  eq(applyLive(big, msg(OFFSCREEN_MAX + 1), { loading: false, shown: false }), null, "dropped");
  const onScreen = applyLive(big, msg(OFFSCREEN_MAX + 1), { loading: false, shown: true });
  eq(onScreen && onScreen.list.length, OFFSCREEN_MAX + 1, "on screen keeps growing");
  const small = applyLive(entry(range(1, 5), null), msg(6), { loading: false, shown: false });
  eq(small && small.list.length, 6, "small off-screen cache appended");
  // an update to a message already cached is applied in place
  const upd = applyLive(big, msg(5, { deleted: true }), { loading: false, shown: false });
  eq(upd?.list[4].deleted, true);
});
