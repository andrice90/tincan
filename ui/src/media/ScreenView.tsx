// Screen share viewer. Inside Tauri it plays the track with ScreenPlayer; in a plain browser
// it draws a test pattern.

import { useEffect, useRef, useState } from "react";
import { isTauri } from "../api/client";
import { ScreenPlayer, type Status } from "./ScreenPlayer";

/** Static test pattern for mock mode. */
function drawTestPattern(canvas: HTMLCanvasElement, track: number): void {
  canvas.width = 1280;
  canvas.height = 720;
  const ctx = canvas.getContext("2d");
  if (!ctx) return;
  const bars = ["#c0c0c0", "#c0c000", "#00c0c0", "#00c000", "#c000c0", "#c00000", "#0000c0"];
  const w = canvas.width / bars.length;
  bars.forEach((c, i) => {
    ctx.fillStyle = c;
    ctx.fillRect(Math.floor(i * w), 0, Math.ceil(w), 480);
  });
  ctx.fillStyle = "#101214";
  ctx.fillRect(0, 480, canvas.width, 240);
  ctx.fillStyle = "#e9eaec";
  ctx.font = "600 40px 'Segoe UI', sans-serif";
  ctx.fillText(`Test pattern, track ${track}`, 48, 610);
}

interface Props {
  track: number;
  /** Accessible name, such as "Alex's screen". */
  label: string;
}

/** Renders a remote screen share track. Watches on mount, unwatches on unmount. */
export function ScreenView({ track, label }: Props) {
  const canvas = useRef<HTMLCanvasElement>(null);
  const [status, setStatus] = useState<Status>({ kind: "waiting" });

  useEffect(() => {
    const el = canvas.current;
    if (!el) return;
    setStatus({ kind: "waiting" });
    if (!isTauri) {
      drawTestPattern(el, track);
      setStatus({ kind: "playing" });
      return;
    }
    if (typeof VideoDecoder === "undefined") {
      setStatus({ kind: "error", text: "This PC can't decode screen shares." });
      return;
    }
    const player = new ScreenPlayer(el, track, setStatus);
    player.start();
    return () => player.stop();
  }, [track]);

  return (
    <div className="screen-view">
      <canvas ref={canvas} role="img" aria-label={label} />
      {status.kind === "waiting" && <p className="screen-status">Waiting for the first frame</p>}
      {status.kind === "error" && (
        <p className="screen-status" role="alert">
          {status.text}
        </p>
      )}
    </div>
  );
}
