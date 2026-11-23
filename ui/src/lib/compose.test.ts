import { eq, test } from "../test/harness";
import { finishSend, registerComposer, restoreFailed, type Draft } from "./compose";

test("UI-LOGIC-03: text typed while a send is in flight survives the send", () => {
  // Enter took "first" out of the box; the user typed "second" before the ack came back
  eq(finishSend({ text: "second", files: [] }, { text: "first", files: [] }, true), { text: "second", files: [] });
});

test("UI-LOGIC-03: a failed send puts its text and files back", () => {
  eq(finishSend({ text: "", files: [] }, { text: "first", files: ["a.png"] }, false), { text: "first", files: ["a.png"] });
});

test("UI-LOGIC-03: a failed send keeps what was typed since, after the failed text", () => {
  eq(finishSend({ text: "second", files: ["b.png"] }, { text: "first", files: ["a.png", "b.png"] }, false), {
    text: "first\nsecond",
    files: ["a.png", "b.png"],
  });
});

test("REGRESSION-3: a failed send reaches the composer that shows its conversation now", () => {
  const drafts = new Map<string, string>();
  const got: Draft[] = [];
  const unregister = registerComposer("general", (d) => got.push(d));
  restoreFailed("general", { text: "long message", files: ["a.png"] }, drafts);
  eq(got, [{ text: "long message", files: ["a.png"] }]);
  eq(drafts.has("general"), false, "not left where the next keystroke overwrites it");
  unregister();
});

test("REGRESSION-3: with no composer on screen the failed text waits in the draft", () => {
  const drafts = new Map<string, string>([["general", "typed since"]]);
  restoreFailed("general", { text: "long message", files: [] }, drafts);
  eq(drafts.get("general"), "long message\ntyped since");
  const empty = new Map<string, string>();
  restoreFailed("random", { text: "hi", files: [] }, empty);
  eq(empty.get("random"), "hi");
});

test("REGRESSION-3: an old composer going away does not unregister the new one", () => {
  const drafts = new Map<string, string>();
  const got: string[] = [];
  const oldOff = registerComposer("general", () => got.push("old"));
  const newOff = registerComposer("general", () => got.push("new"));
  oldOff();
  restoreFailed("general", { text: "x", files: [] }, drafts);
  eq(got, ["new"]);
  newOff();
});
