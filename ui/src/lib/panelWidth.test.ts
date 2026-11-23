import { eq, ok, test } from "../test/harness";
import {
  MEMBERS_BOUNDS,
  SIDEBAR_BOUNDS,
  clampWidth,
  dragWidth,
  keyWidth,
  loadWidth,
  parseWidth,
  saveWidth,
  type WidthStore,
} from "./panelWidth";

const B = SIDEBAR_BOUNDS;

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

test("panel width: defaults match the old fixed widths", () => {
  eq(B.initial, 240);
  eq(MEMBERS_BOUNDS.initial, 220);
  ok(B.min <= B.initial && B.initial <= B.max, "sidebar initial inside bounds");
  ok(MEMBERS_BOUNDS.min <= MEMBERS_BOUNDS.initial && MEMBERS_BOUNDS.initial <= MEMBERS_BOUNDS.max, "members initial inside bounds");
});

test("panel width: clamps and rounds", () => {
  eq(clampWidth(100, B), 200);
  eq(clampWidth(999, B), 420);
  eq(clampWidth(300.6, B), 301);
  eq(clampWidth(Number.NaN, B), 240);
  eq(clampWidth(Number.POSITIVE_INFINITY, B), 240);
});

test("panel width: parses saved values, garbage gives the initial width", () => {
  eq(parseWidth(null, B), 240);
  eq(parseWidth("", B), 240);
  eq(parseWidth("wide", B), 240);
  eq(parseWidth("300px", B), 240);
  eq(parseWidth("-50", B), 240);
  eq(parseWidth("1e3", B), 240);
  eq(parseWidth("300", B), 300);
  eq(parseWidth("5000", B), 420);
  eq(parseWidth("150", B), 200);
});

test("panel width: load and save round trip", () => {
  const s = memStore();
  eq(loadWidth(s, "k", B), 240);
  saveWidth(s, "k", 333, B);
  eq(s.data.k, "333");
  eq(loadWidth(s, "k", B), 333);
  saveWidth(s, "k", 9999, B);
  eq(loadWidth(s, "k", B), 420);
});

test("panel width: saving the initial width (a reset) removes the key", () => {
  const s = memStore({ k: "300" });
  saveWidth(s, "k", 240, B);
  eq(s.data, {});
  eq(loadWidth(s, "k", B), 240);
});

test("panel width: storage that throws or is missing never breaks the layout", () => {
  eq(loadWidth(throwing, "k", B), 240);
  saveWidth(throwing, "k", 300, B);
  eq(loadWidth(undefined, "k", B), 240);
  saveWidth(undefined, "k", 300, B);
});

test("panel width: arrow keys follow the arrow, Home and End jump to the bounds", () => {
  eq(keyWidth("ArrowRight", 240, "right", B), 256);
  eq(keyWidth("ArrowLeft", 240, "right", B), 224);
  eq(keyWidth("ArrowLeft", 220, "left", MEMBERS_BOUNDS), 236);
  eq(keyWidth("ArrowRight", 236, "left", MEMBERS_BOUNDS), 220);
  eq(keyWidth("ArrowRight", 420, "right", B), 420);
  eq(keyWidth("ArrowLeft", 205, "right", B), 200);
  eq(keyWidth("Home", 300, "right", B), 200);
  eq(keyWidth("End", 300, "right", B), 420);
  eq(keyWidth("Enter", 300, "right", B), null);
  eq(keyWidth("ArrowUp", 300, "right", B), null);
});

test("panel width: drag moves toward the content side", () => {
  eq(dragWidth(240, 500, 500, "right", B), 240);
  eq(dragWidth(240, 500, 560, "right", B), 300);
  eq(dragWidth(240, 500, 100, "right", B), 200);
  eq(dragWidth(220, 900, 860, "left", MEMBERS_BOUNDS), 260);
  eq(dragWidth(220, 900, 950, "left", MEMBERS_BOUNDS), 220);
});
