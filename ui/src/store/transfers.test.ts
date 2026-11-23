import type { Transfer } from "../api/types";
import { eq, test } from "../test/harness";
import { staleTransfers, takeTransfer } from "./transfers";

const t = (state: Transfer["state"], have = 0): Transfer => ({ hash: "h", state, have, total: 100, peers: 1 });

test("UI-LOGIC-10: a download result never overwrites a newer transfer-updated", () => {
  const before = t("idle");
  const event = t("complete", 100);
  // the event arrived while the command was in flight
  eq(takeTransfer(event, before, t("queued")), false);
  // nothing arrived: the command's state is the newest
  eq(takeTransfer(before, before, t("queued")), true);
});

test("UI-LOGIC-10: complete never goes back to an unfinished state", () => {
  const done = t("complete", 100);
  eq(takeTransfer(done, done, t("downloading", 50)), false);
  eq(takeTransfer(done, done, t("complete", 100)), true);
});

test("UI-LOGIC-14: after a lag snapshot every unfinished transfer is read again", () => {
  const entries: [string, Transfer][] = [
    ["a", t("downloading", 30)],
    ["b", t("complete", 100)],
    ["c", t("queued")],
    ["d", t("idle")],
  ];
  eq(staleTransfers(entries), ["a", "c", "d"]);
});
