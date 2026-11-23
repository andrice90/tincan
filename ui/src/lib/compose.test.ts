import { eq, test } from "../test/harness";
import { finishSend } from "./compose";

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
