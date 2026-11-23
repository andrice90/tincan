// Screen share viewer. Inside Tauri it opens a raw Channel with screen_watch and
// decodes H.264 with WebCodecs; in a plain browser it draws a test pattern.
//
// Message layout from Rust: [u8 flags (bit0 key)][u16 BE track][u32 BE ts 90 kHz][Annex-B access unit].

import { useEffect, useRef, useState } from "react";
import { errorText, frameChannel, invoke, isTauri, type FrameChannel } from "../api/client";
import { codecFromAnnexB } from "./h264";
import { isWindowHidden } from "./windowState";

const HEADER = 7;

type Status = { kind: "waiting" } | { kind: "playing" } | { kind: "error"; text: string };

/** Decodes one track into a canvas. Created per mount, never shared. */
class ScreenPlayer {
  private decoder: VideoDecoder | null = null;
  private codec: string | null = null;
  private needKey = true;
  private latest: VideoFrame | null = null;
  private raf = 0;
  private channel: FrameChannel | null = null;
  private stopped = false;
  private playing = false;
  private lastTs = -1;
  private wraps = 0;
  private readonly ctx: CanvasRenderingContext2D | null;

  constructor(
    private readonly canvas: HTMLCanvasElement,
    private readonly track: number,
    private readonly onStatus: (s: Status) => void,
  ) {
    this.ctx = canvas.getContext("2d", { alpha: false, desynchronized: true });
  }

  async start(): Promise<void> {
    const ch = await frameChannel(this.onMessage);
    if (this.stopped) {
      ch.close();
      return;
    }
    this.channel = ch;
    try {
      await invoke("screen_watch", { track: this.track, onFrame: ch.ipc });
    } catch (err) {
      if (!this.stopped) this.onStatus({ kind: "error", text: errorText(err) });
    }
  }

  stop(): void {
    if (this.stopped) return;
    this.stopped = true;
    if (this.raf) cancelAnimationFrame(this.raf);
    this.raf = 0;
    this.latest?.close();
    this.latest = null;
    this.closeDecoder();
    this.channel?.close();
    this.channel = null;
    invoke("screen_unwatch", { track: this.track }).catch(() => {
      // the share may already be gone
    });
  }

  /** Synchronous on purpose: decode order must follow arrival order. */
  private onMessage = (buf: ArrayBuffer): void => {
    if (this.stopped || !(buf instanceof ArrayBuffer) || buf.byteLength <= HEADER) return;
    const view = new DataView(buf);
    const key = (view.getUint8(0) & 1) === 1;
    if (view.getUint16(1) !== this.track) return;
    const ts90 = view.getUint32(3);
    const au = new Uint8Array(buf, HEADER);

    if (key) {
      const codec = codecFromAnnexB(au);
      if (codec && (codec !== this.codec || !this.decoder)) {
        if (!this.configure(codec)) return;
      }
    }
    const dec = this.decoder;
    if (!dec || dec.state !== "configured") return;
    if (this.needKey && !key) return;
    try {
      dec.decode(new EncodedVideoChunk({ type: key ? "key" : "delta", timestamp: this.micros(ts90), data: au }));
      this.needKey = false;
    } catch (err) {
      console.warn("screen decode failed", err);
      this.resetDecoder();
    }
  };

  /** 90 kHz u32 clock (wraps every 13 h) to monotonic microseconds. */
  private micros(ts90: number): number {
    if (this.lastTs >= 0 && ts90 < this.lastTs && this.lastTs - ts90 > 0x80000000) this.wraps++;
    this.lastTs = ts90;
    return Math.round(((this.wraps * 0x100000000 + ts90) * 100) / 9);
  }

  private configure(codec: string): boolean {
    this.closeDecoder();
    try {
      const dec = new VideoDecoder({ output: this.onFrame, error: this.onError });
      dec.configure({ codec, hardwareAcceleration: "prefer-hardware", optimizeForLatency: true });
      this.decoder = dec;
      this.codec = codec;
      this.needKey = true;
      return true;
    } catch (err) {
      this.decoder = null;
      this.codec = null;
      this.onStatus({ kind: "error", text: "This screen share uses a video format this PC can't decode." });
      console.warn("VideoDecoder configure failed", codec, err);
      return false;
    }
  }

  private closeDecoder(): void {
    const d = this.decoder;
    this.decoder = null;
    if (d && d.state !== "closed") {
      try {
        d.close();
      } catch {
        // already closed by an error
      }
    }
  }

  /** After an error the decoder is closed: build a new one at the next keyframe. */
  private resetDecoder(): void {
    this.closeDecoder();
    this.codec = null;
    this.needKey = true;
  }

  private onError = (err: DOMException): void => {
    console.warn("screen decoder error", err);
    this.decoder = null;
    this.codec = null;
    this.needKey = true;
  };

  private onFrame = (frame: VideoFrame): void => {
    if (this.stopped || isWindowHidden()) {
      frame.close();
      return;
    }
    this.latest?.close();
    this.latest = frame;
    if (!this.raf) this.raf = requestAnimationFrame(this.draw);
  };

  private draw = (): void => {
    this.raf = 0;
    const f = this.latest;
    this.latest = null;
    if (!f) return;
    if (this.ctx) {
      if (this.canvas.width !== f.displayWidth || this.canvas.height !== f.displayHeight) {
        this.canvas.width = f.displayWidth;
        this.canvas.height = f.displayHeight;
      }
      this.ctx.drawImage(f, 0, 0);
    }
    f.close();
    if (!this.playing) {
      this.playing = true;
      this.onStatus({ kind: "playing" });
    }
  };
}

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
    void player.start();
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
