// Pure maths and encoding policy for the avatar and server icon crop dialog. No DOM: the
// dialog passes sizes in, and encoding takes the canvas work as functions.
//
// Coordinates: the viewport is a `view` x `view` square. The final shape (circle or
// squircle) sits inside it with `margin` on every side, so the crop square is
// [margin, margin + side] on both axes. The image is drawn with its top-left corner at
// `offset` (viewport pixels) and `scale` viewport pixels per image pixel.

export interface Size {
  w: number;
  h: number;
}

export interface Point {
  x: number;
  y: number;
}

export interface Frame {
  /** Viewport side (px). */
  view: number;
  /** Space between the viewport edge and the crop shape (px). */
  margin: number;
}

/** Side of the crop square inside the viewport. */
export function cropSide(f: Frame): number {
  return f.view - 2 * f.margin;
}

/** Largest zoom on the slider (times the smallest). */
export const MAX_ZOOM = 4;

/** The scale at zoom 1: the image's short side just covers the crop square. */
export function coverScale(img: Size, f: Frame): number {
  return cropSide(f) / Math.min(img.w, img.h);
}

/** Zoom kept within 1..MAX_ZOOM (1 keeps the shape covered). */
export function clampZoom(zoom: number): number {
  if (!Number.isFinite(zoom)) return 1;
  return Math.min(MAX_ZOOM, Math.max(1, zoom));
}

/** Keep the drawn image covering the whole crop square. */
export function clampOffset(o: Point, img: Size, scale: number, f: Frame): Point {
  const side = cropSide(f);
  const lo = (len: number) => f.margin + side - len * scale;
  const clamp = (v: number, min: number, max: number) => Math.min(max, Math.max(min, v));
  return { x: clamp(o.x, lo(img.w), f.margin), y: clamp(o.y, lo(img.h), f.margin) };
}

/** The offset that centres the image on the crop square. */
export function centredOffset(img: Size, scale: number, f: Frame): Point {
  const c = f.view / 2;
  return { x: c - (img.w * scale) / 2, y: c - (img.h * scale) / 2 };
}

/**
 * New offset after changing the scale while keeping `focus` (a viewport point, by default
 * the centre) over the same image pixel, clamped.
 */
export function zoomOffset(o: Point, img: Size, from: number, to: number, f: Frame, focus?: Point): Point {
  const p = focus ?? { x: f.view / 2, y: f.view / 2 };
  const k = to / from;
  return clampOffset({ x: p.x - (p.x - o.x) * k, y: p.y - (p.y - o.y) * k }, img, to, f);
}

/** The square of the source image (image pixels) that the crop shape shows. */
export function cropRect(o: Point, scale: number, f: Frame): { sx: number; sy: number; size: number } {
  return { sx: (f.margin - o.x) / scale, sy: (f.margin - o.y) / scale, size: cropSide(f) / scale };
}

/** Arrow-key nudge in viewport pixels (Shift moves further), or null for other keys. */
export function nudge(key: string, shift: boolean): Point | null {
  const d = shift ? 16 : 4;
  switch (key) {
    case "ArrowLeft":
      return { x: d, y: 0 };
    case "ArrowRight":
      return { x: -d, y: 0 };
    case "ArrowUp":
      return { x: 0, y: d };
    case "ArrowDown":
      return { x: 0, y: -d };
    default:
      return null;
  }
}

// ------------------------------------------------------------------ encoding

/** Largest file the app accepts. */
export const MAX_IMAGE_BYTES = 64 * 1024;
/** Side of the image sent to the app. */
export const OUTPUT_SIDE = 128;
/** WebP qualities tried in order until the file fits. */
export const WEBP_QUALITIES = [0.9, 0.8, 0.7, 0.55, 0.4, 0.25];
/** PNG sides tried when WebP is not available (PNG has no quality setting). */
export const PNG_SIDES = [128, 112, 96, 80, 64];

export interface Encoded {
  type: "image/webp" | "image/png";
  side: number;
  bytes: Uint8Array;
}

/**
 * Encode the crop under `limit` bytes: WebP at falling quality, then (when the encoder gives
 * no WebP, or no quality fits) PNG at falling sizes. `encode(side, type, quality)` renders
 * the crop at `side` x `side` and returns the bytes and the type actually produced (a
 * canvas returns PNG for a type it cannot encode). Throws when nothing fits.
 */
export async function encodeUnderLimit(
  encode: (side: number, type: "image/webp" | "image/png", quality?: number) => Promise<{ type: string; bytes: Uint8Array }>,
  limit = MAX_IMAGE_BYTES,
): Promise<Encoded> {
  for (const q of WEBP_QUALITIES) {
    const r = await encode(OUTPUT_SIDE, "image/webp", q);
    if (r.type !== "image/webp") break;
    if (r.bytes.length <= limit) return { type: "image/webp", side: OUTPUT_SIDE, bytes: r.bytes };
  }
  for (const side of PNG_SIDES) {
    const r = await encode(side, "image/png");
    if (r.type === "image/png" && r.bytes.length <= limit) return { type: "image/png", side, bytes: r.bytes };
  }
  throw new Error("Couldn't make that image small enough. Try another one.");
}
