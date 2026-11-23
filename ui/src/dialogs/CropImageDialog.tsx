// Crop a picked image to the avatar circle or the server icon's rounded square, then encode
// it to 128x128 WebP (PNG when WebP is unavailable) under 64 KB and send it to the app.

import { useEffect, useId, useRef, useState, type KeyboardEvent, type PointerEvent } from "react";
import { errorText } from "../api/client";
import { Modal } from "../components/Modal";
import {
  centredOffset,
  clampOffset,
  clampZoom,
  coverScale,
  cropRect,
  encodeUnderLimit,
  MAX_ZOOM,
  nudge,
  zoomOffset,
  type Frame,
  type Point,
  type Size,
} from "../lib/crop";
import { closeDialog, openDialog, setAvatar, setServerIcon } from "../store/actions";
import type { CropTarget, DialogState } from "../store/store";

/** The viewport: 280px, the shape 240px inside it. */
const FRAME: Frame = { view: 280, margin: 20 };
/** The server rail's corner radius (12px) over its 44px button, as a share of the side. */
const SQUIRCLE = 12 / 44;

/** Render the crop at `side` x `side` and encode it. */
async function renderCrop(img: HTMLImageElement, offset: Point, scale: number, side: number, type: string, quality?: number) {
  const canvas = document.createElement("canvas");
  canvas.width = side;
  canvas.height = side;
  const ctx = canvas.getContext("2d");
  if (!ctx) throw new Error("Couldn't prepare the image. Try again.");
  ctx.imageSmoothingQuality = "high";
  const r = cropRect(offset, scale, FRAME);
  ctx.drawImage(img, r.sx, r.sy, r.size, r.size, 0, 0, side, side);
  const blob = await new Promise<Blob | null>((resolve) => canvas.toBlob(resolve, type, quality));
  if (!blob) throw new Error("Couldn't prepare the image. Try again.");
  return { type: blob.type, bytes: new Uint8Array(await blob.arrayBuffer()) };
}

export function CropImageDialog({ file, target, back }: { file: File; target: CropTarget; back: DialogState | null }) {
  const [url, setUrl] = useState<string | null>(null);
  const [img, setImg] = useState<HTMLImageElement | null>(null);
  const [zoom, setZoom] = useState(1);
  const [offset, setOffset] = useState<Point>({ x: 0, y: 0 });
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const viewRef = useRef<HTMLDivElement>(null);
  const drag = useRef<{ id: number; x: number; y: number; from: Point } | null>(null);
  const maskId = useId();
  const avatar = target.kind === "avatar";

  useEffect(() => {
    const u = URL.createObjectURL(file);
    setUrl(u);
    const im = new Image();
    im.onload = () => {
      const size = { w: im.naturalWidth, h: im.naturalHeight };
      setImg(im);
      setZoom(1);
      setOffset(centredOffset(size, coverScale(size, FRAME), FRAME));
    };
    im.onerror = () => setError("Tincan can't open that file. Pick a PNG, JPEG, WebP or GIF image.");
    im.src = u;
    return () => {
      // Revoking the URL makes a load still in flight fail: that is not the file's fault.
      im.onload = null;
      im.onerror = null;
      URL.revokeObjectURL(u);
    };
  }, [file]);

  const size: Size | null = img ? { w: img.naturalWidth, h: img.naturalHeight } : null;
  const scale = size ? coverScale(size, FRAME) * zoom : 1;

  // Kept in a ref so the wheel listener (added once, not passive) sees the latest values.
  const live = useRef({ size, zoom, offset });
  live.current = { size, zoom, offset };

  const zoomTo = (next: number, focus?: Point) => {
    const { size: s, zoom: z, offset: o } = live.current;
    if (!s) return;
    const nz = clampZoom(next);
    const base = coverScale(s, FRAME);
    setOffset(zoomOffset(o, s, base * z, base * nz, FRAME, focus));
    setZoom(nz);
  };

  useEffect(() => {
    const el = viewRef.current;
    if (!el) return;
    const onWheel = (e: WheelEvent) => {
      e.preventDefault();
      const r = el.getBoundingClientRect();
      zoomTo(live.current.zoom * Math.exp(-e.deltaY * 0.002), { x: e.clientX - r.left, y: e.clientY - r.top });
    };
    el.addEventListener("wheel", onWheel, { passive: false });
    return () => el.removeEventListener("wheel", onWheel);
    // Added once: zoomTo reads everything through `live`.
  }, []);

  const moveTo = (o: Point) => {
    if (size) setOffset(clampOffset(o, size, scale, FRAME));
  };

  const onPointerDown = (e: PointerEvent<HTMLDivElement>) => {
    if (!size || e.button !== 0) return;
    e.currentTarget.setPointerCapture(e.pointerId);
    drag.current = { id: e.pointerId, x: e.clientX, y: e.clientY, from: offset };
  };
  const onPointerMove = (e: PointerEvent<HTMLDivElement>) => {
    const d = drag.current;
    if (!d || d.id !== e.pointerId) return;
    moveTo({ x: d.from.x + e.clientX - d.x, y: d.from.y + e.clientY - d.y });
  };
  const endDrag = (e: PointerEvent<HTMLDivElement>) => {
    if (drag.current?.id === e.pointerId) drag.current = null;
  };
  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    const d = nudge(e.key, e.shiftKey);
    if (d) {
      e.preventDefault();
      moveTo({ x: offset.x + d.x, y: offset.y + d.y });
    } else if (e.key === "+" || e.key === "=") {
      e.preventDefault();
      zoomTo(zoom * 1.1);
    } else if (e.key === "-") {
      e.preventDefault();
      zoomTo(zoom / 1.1);
    }
  };

  const close = () => (back ? openDialog(back) : closeDialog());

  const save = async () => {
    if (!img || busy) return;
    setBusy(true);
    setError(null);
    try {
      const out = await encodeUnderLimit((side, type, quality) => renderCrop(img, offset, scale, side, type, quality));
      if (target.kind === "avatar") await setAvatar(out.bytes);
      else await setServerIcon(target.serverId, out.bytes);
      close();
    } catch (err) {
      setError(err instanceof Error ? err.message : errorText(err));
      setBusy(false);
    }
  };

  const side = FRAME.view - 2 * FRAME.margin;
  const radius = avatar ? side / 2 : side * SQUIRCLE;
  const shape = <rect x={FRAME.margin} y={FRAME.margin} width={side} height={side} rx={radius} ry={radius} />;

  return (
    <Modal
      title={avatar ? "Crop avatar" : "Crop server icon"}
      onClose={close}
      onSubmit={() => void save()}
      className="modal-crop"
      footer={
        <>
          <button type="button" className="btn btn-ghost" onClick={close}>
            Cancel
          </button>
          <button type="submit" className="btn btn-primary" disabled={!img || busy}>
            {busy ? "Saving" : "Save"}
          </button>
        </>
      }
    >
      <div
        ref={viewRef}
        className="crop-view"
        style={{ width: FRAME.view, height: FRAME.view }}
        tabIndex={0}
        role="group"
        aria-label="Image position. Drag or use the arrow keys to move it."
        data-autofocus
        data-testid="crop-view"
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={endDrag}
        onPointerCancel={endDrag}
        onKeyDown={onKeyDown}
      >
        {url && size && (
          <img
            className="crop-img"
            src={url}
            alt=""
            draggable={false}
            style={{ width: size.w * scale, height: size.h * scale, transform: `translate(${offset.x}px, ${offset.y}px)` }}
          />
        )}
        <svg className="crop-mask" viewBox={`0 0 ${FRAME.view} ${FRAME.view}`} aria-hidden="true">
          <defs>
            <mask id={maskId}>
              <rect width={FRAME.view} height={FRAME.view} fill="white" />
              <g fill="black">{shape}</g>
            </mask>
          </defs>
          <rect width={FRAME.view} height={FRAME.view} className="crop-dim" mask={`url(#${maskId})`} />
          <g className="crop-outline">{shape}</g>
        </svg>
      </div>
      <div className="field crop-zoom">
        <label className="field-label" htmlFor="crop-zoom">
          Zoom
        </label>
        <input
          id="crop-zoom"
          type="range"
          min={1}
          max={MAX_ZOOM}
          step={0.01}
          value={zoom}
          disabled={!img}
          onChange={(e) => zoomTo(Number(e.target.value))}
        />
      </div>
      {error && (
        <p className="error-text" role="alert">
          {error}
        </p>
      )}
    </Modal>
  );
}
