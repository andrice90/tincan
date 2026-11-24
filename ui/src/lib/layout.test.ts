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

test("layout: Classic unless Floating was saved", () => {
  eq(parseLayout(null), "classic");
  eq(parseLayout(""), "classic");
  eq(parseLayout("Floating"), "classic");
  eq(parseLayout("classic"), "classic");
  eq(parseLayout("floating"), "floating");
});

test("layout: load from storage, missing or throwing storage gives Classic", () => {
  eq(loadLayout(undefined), "classic");
  eq(loadLayout(throwing), "classic");
  eq(loadLayout(memStore()), "classic");
  eq(loadLayout(memStore({ [LAYOUT_KEY]: "floating" })), "floating");
  eq(loadLayout(memStore({ [LAYOUT_KEY]: "cards" })), "classic");
});

test("layout: Floating is saved, Classic is saved as no key", () => {
  const s = memStore();
  saveLayout(s, "floating");
  eq(s.data, { [LAYOUT_KEY]: "floating" });
  eq(loadLayout(s), "floating");
  saveLayout(s, "classic");
  eq(s.data, {});
  eq(loadLayout(s), "classic");
  // blocked or full storage is not an error
  saveLayout(throwing, "floating");
  saveLayout(undefined, "floating");
});

test("layout: close delay only in Floating with motion allowed", () => {
  eq(closeDelayMs("classic", false), 0);
  eq(closeDelayMs("classic", true), 0);
  eq(closeDelayMs("floating", true), 0);
  const d = closeDelayMs("floating", false);
  eq(d > PANEL_MOTION_MS, true, "content outlives the transition");
  eq(d < PANEL_MOTION_MS * 2, true, "but not by much");
});
