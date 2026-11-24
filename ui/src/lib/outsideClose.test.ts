import { eq, test } from "../test/harness";
import { KEEP_OPEN_SELECTOR, closesOnEscape, closesOnPointerDown, type PanelLike, type TargetLike } from "./outsideClose";

/** A fake element: `inside` the panel or not, matching `closest` for the given ancestors. */
type Fake = TargetLike & { inside: boolean };

function el(ancestors: string[], inside = false): Fake {
  const keep = KEEP_OPEN_SELECTOR.split(",").map((s) => s.trim());
  return { inside, closest: (sel: string) => (sel === KEEP_OPEN_SELECTOR && ancestors.some((a) => keep.includes(a)) ? {} : null) };
}

const panel: PanelLike<Fake> = { contains: (n) => n.inside };

test("soundboard: a click elsewhere in the window closes it", () => {
  eq(closesOnPointerDown(el(["main"]), panel), true);
  eq(closesOnPointerDown(el([".channel-row"]), panel), true);
});

test("soundboard: a click inside it keeps it open", () => {
  eq(closesOnPointerDown(el([], true), panel), false);
});

test("soundboard: its toggle button toggles instead of closing and reopening", () => {
  eq(closesOnPointerDown(el([".soundboard-toggle"]), panel), false);
});

test("soundboard: a menu, popover or dialog it opened keeps it open", () => {
  eq(closesOnPointerDown(el([".popover"]), panel), false, "a tile's right-click menu");
  eq(closesOnPointerDown(el(["dialog"]), panel), false, "Add sound, Rename");
});

test("soundboard: no target or no panel decides nothing", () => {
  eq(closesOnPointerDown(null, panel), false);
  eq(closesOnPointerDown(el(["main"]), null), false);
});

test("soundboard: Escape closes it unless a menu or dialog takes it first", () => {
  eq(closesOnEscape({ defaultPrevented: false }, false), true);
  eq(closesOnEscape({ defaultPrevented: true }, false), false);
  eq(closesOnEscape({ defaultPrevented: false }, true), false);
});
