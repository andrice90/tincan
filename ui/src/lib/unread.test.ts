import { eq, test } from "../test/harness";
import { unreadBadge, unreadLabel } from "./unread";

test("unread badge: none when read or on screen", () => {
  eq(unreadBadge(false, false), null);
  eq(unreadBadge(true, true), null);
  eq(unreadBadge(true, true, 5), null);
});

test("unread badge: a dot without a count", () => {
  eq(unreadBadge(true, false), { kind: "dot" });
  eq(unreadBadge(true, false, null), { kind: "dot" });
  eq(unreadBadge(true, false, 0), { kind: "dot" });
  eq(unreadBadge(true, false, Number.NaN), { kind: "dot" });
});

test("unread badge: the count, capped at 99+", () => {
  eq(unreadBadge(true, false, 1), { kind: "count", text: "1" });
  eq(unreadBadge(true, false, 99), { kind: "count", text: "99" });
  eq(unreadBadge(true, false, 100), { kind: "count", text: "99+" });
  eq(unreadBadge(true, false, 2.7), { kind: "count", text: "2" });
});

test("unread badge: screen reader text", () => {
  eq(unreadLabel({ kind: "dot" }), ", unread messages");
  eq(unreadLabel({ kind: "count", text: "1" }), ", 1 unread message");
  eq(unreadLabel({ kind: "count", text: "99+" }), ", 99+ unread messages");
});
