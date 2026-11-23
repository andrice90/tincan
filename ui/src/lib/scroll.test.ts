import { eq, test } from "../test/harness";
import { anchorStep } from "./scroll";

test("UI-LOGIC-07: a new message at the bottom does not use up the load-older anchor", () => {
  eq(anchorStep(1200, "m41", "m41"), "recapture");
});

test("UI-LOGIC-07: the older page landing applies the anchor", () => {
  eq(anchorStep(1200, "m11", "m41"), "apply");
});

test("UI-LOGIC-07: nothing to do without an anchor", () => {
  eq(anchorStep(null, "m11", "m41"), "none");
  eq(anchorStep(null, "m41", "m41"), "none");
});
