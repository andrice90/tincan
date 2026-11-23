import { eq, ok, test } from "../test/harness";
import {
  centredOffset,
  clampOffset,
  clampZoom,
  coverScale,
  cropRect,
  cropSide,
  encodeUnderLimit,
  MAX_ZOOM,
  nudge,
  zoomOffset,
  type Frame,
} from "./crop";

const F: Frame = { view: 280, margin: 20 };

test("crop: zoom 1 makes the short side cover the shape", () => {
  eq(cropSide(F), 240);
  eq(coverScale({ w: 480, h: 960 }, F), 0.5, "portrait");
  eq(coverScale({ w: 1200, h: 600 }, F), 0.4, "landscape");
  eq(coverScale({ w: 60, h: 60 }, F), 4, "a small image is scaled up");
  eq(clampZoom(0.2), 1);
  eq(clampZoom(9), MAX_ZOOM);
  eq(clampZoom(Number.NaN), 1);
  eq(clampZoom(2.5), 2.5);
});

test("crop: the centred image covers the shape and crops its middle", () => {
  const img = { w: 1200, h: 600 };
  const s = coverScale(img, F);
  const o = centredOffset(img, s, F);
  eq(clampOffset(o, img, s, F), o, "already valid");
  eq(cropRect(o, s, F), { sx: 300, sy: 0, size: 600 });
});

test("crop: dragging never uncovers the shape", () => {
  const img = { w: 1000, h: 500 };
  const s = coverScale(img, F) * 2; // 0.96
  // Too far right and down: the image's left and top edges stop at the shape.
  eq(clampOffset({ x: 300, y: 300 }, img, s, F), { x: 20, y: 20 });
  // Too far left and up: the right and bottom edges stop at the shape.
  const o = clampOffset({ x: -5000, y: -5000 }, img, s, F);
  eq(o, { x: 260 - 1000 * s, y: 260 - 500 * s });
  const r = cropRect(o, s, F);
  ok(Math.abs(r.sx + r.size - 1000) < 1e-9 && Math.abs(r.sy + r.size - 500) < 1e-9, "the crop ends at the image corner");
  // Every clamped offset gives a crop inside the image.
  for (const p of [{ x: 0, y: 0 }, { x: -200, y: 15 }, { x: 99, y: -999 }]) {
    const c = cropRect(clampOffset(p, img, s, F), s, F);
    ok(c.sx >= 0 && c.sy >= 0 && c.sx + c.size <= 1000 + 1e-9 && c.sy + c.size <= 500 + 1e-9, `inside for ${JSON.stringify(p)}`);
  }
});

test("crop: zooming keeps the focus point and stays covered", () => {
  const img = { w: 800, h: 800 };
  const s1 = coverScale(img, F);
  const o1 = centredOffset(img, s1, F);
  const s2 = s1 * 2;
  const o2 = zoomOffset(o1, img, s1, s2, F);
  // The image pixel under the viewport centre is the same before and after.
  const under = (o: { x: number; y: number }, s: number) => ((F.view / 2 - o.x) / s).toFixed(6);
  eq(under(o2, s2), under(o1, s1));
  // Zooming out at a corner focus is clamped back over the shape.
  const out = zoomOffset({ x: -400, y: -400 }, img, s2, s1, F, { x: 0, y: 0 });
  eq(out, clampOffset(out, img, s1, F));
  eq(cropRect(out, s1, F).size, 800);
});

test("crop: arrow keys nudge, Shift moves further", () => {
  eq(nudge("ArrowLeft", false), { x: 4, y: 0 });
  eq(nudge("ArrowDown", true), { x: 0, y: -16 });
  eq(nudge("Enter", false), null);
});

test("encode: WebP at falling quality until it fits", async () => {
  const calls: string[] = [];
  const r = await encodeUnderLimit(async (side, type, q) => {
    calls.push(`${type}@${side}:${q}`);
    return { type, bytes: new Uint8Array(q !== undefined && q <= 0.7 ? 1000 : 70_000) };
  });
  eq([r.type, r.side, r.bytes.length], ["image/webp", 128, 1000]);
  eq(calls, ["image/webp@128:0.9", "image/webp@128:0.8", "image/webp@128:0.7"]);
});

test("encode: PNG at falling sizes when WebP is not available", async () => {
  const sides: number[] = [];
  const r = await encodeUnderLimit(async (side, type) => {
    if (type === "image/webp") return { type: "image/png", bytes: new Uint8Array(10) };
    sides.push(side);
    return { type, bytes: new Uint8Array(side * side * 4 + 100) };
  });
  eq([r.type, r.side], ["image/png", 112]);
  eq(sides, [128, 112], "128x128 RGBA is just over 64 KB");
});

test("encode: an error when nothing fits", async () => {
  let threw = false;
  try {
    await encodeUnderLimit(async (_side, type) => ({ type, bytes: new Uint8Array(100_000) }));
  } catch {
    threw = true;
  }
  ok(threw, "rejects");
});
