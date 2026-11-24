import { eq, test } from "../test/harness";
import { LAYOUT_KEY, PANEL_MOTION_MS, closeDelayMs, loadLayout, parseLayout, saveLayout } from "./layout";
import type { WidthStore } from "./panelWidth";

function memStore(init: Record<string, string> = {}): WidthStore & { data: Record<string, string> } {
  const data = { ...init };
  return {
    data,
    getItem: (k) => (k in data ? (data[k] as string) : null),
    setItem: (k, v) => {
      data[k] = v;
    },
    removeItem: (k) => {
      delete data[k];
    },
  };
}

const throwing: WidthStore = {
  getItem: () => {
    throw new Error("blocked");
  },
  setItem: () => {
    throw new Error("full");
  },
  removeItem: () => {
    throw new Error("blocked");
  },
};

test("layout: Floating unless Classic was saved", () => {
  eq(parseLayout(null), "floating");
  eq(parseLayout(""), "floating");
  eq(parseLayout("Classic"), "floating");
  eq(parseLayout("floating"), "floating");
  eq(parseLayout("classic"), "classic");
});

test("layout: load from storage, missing or throwing storage gives Floating", () => {
  eq(loadLayout(undefined), "floating");
  eq(loadLayout(throwing), "floating");
  eq(loadLayout(memStore()), "floating");
  eq(loadLayout(memStore({ [LAYOUT_KEY]: "classic" })), "classic");
  eq(loadLayout(memStore({ [LAYOUT_KEY]: "cards" })), "floating");
});

test("layout: Classic is saved, Floating is saved as no key", () => {
  const s = memStore();
  saveLayout(s, "classic");
  eq(s.data, { [LAYOUT_KEY]: "classic" });
  eq(loadLayout(s), "classic");
  saveLayout(s, "floating");
  eq(s.data, {});
  eq(loadLayout(s), "floating");
  // blocked or full storage is not an error
  saveLayout(throwing, "classic");
  saveLayout(undefined, "classic");
});

test("layout: close delay only in Floating with motion allowed", () => {
  eq(closeDelayMs("classic", false), 0);
  eq(closeDelayMs("classic", true), 0);
  eq(closeDelayMs("floating", true), 0);
  const d = closeDelayMs("floating", false);
  eq(d > PANEL_MOTION_MS, true, "content outlives the transition");
  eq(d < PANEL_MOTION_MS * 2, true, "but not by much");
});
