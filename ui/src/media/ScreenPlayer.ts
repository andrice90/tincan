// Screen share player: subscribes to a track with screen_watch and decodes its H.264 with
// WebCodecs into a canvas. Tauri only (it needs a raw Channel).
//
// Message layout from Rust: [u8 flags (bit0 key)][u16 BE track][u32 BE ts 90 kHz][Annex-B access unit].

import { errorText, frameChannel, invoke, type FrameChannel } from "../api/client";
import { codecFromAnnexB } from "./h264";
import { afterDecoderError, keyedSerial, pickAccel, skipForBackpressure, type Accel } from "./screenLogic";
import { isWindowHidden, onWindowHidden } from "./windowState";

const HEADER = 7;
/** Access units held while a new codec is probed (a few seconds of video). */
const MAX_HELD = 240;
const UNDECODABLE = "This screen share uses a video format this PC can't decode.";

export type Status = { kind: "waiting" } | { kind: "playing" } | { kind: "error"; text: string };

/** screen_watch and screen_unwatch, one after another per track, across players. */
const perTrack = keyedSerial<number>();

/** How each codec string decodes on this PC (null: it can't), probed once per run. */
const support = new Map<string, Promise<Accel | null>>();

async function supported(codec: string, hardwareAcceleration: Accel): Promise<boolean> {
  try {
    return (await VideoDecoder.isConfigSupported({ codec, hardwareAcceleration, optimizeForLatency: true })).supported === true;
  } catch {
    return false;
  }
}

function decoderSupport(codec: string): Promise<Accel | null> {
  let p = support.get(codec);
  if (!p) {
    // A well-formed but unsupported config does not throw in configure(): the decoder closes
    // later through its error callback. Ask first, so it is never configured at all.
    p = (async () => {
      const hw = await supported(codec, "prefer-hardware");
      return pickAccel(hw, hw || (await supported(codec, "no-preference")));
    })();
    support.set(codec, p);
  }
  return p;
}

interface Unit {
  key: boolean;
  ts90: number;
  au: Uint8Array;
}

/** Decodes one track into a canvas. Created per mount, never shared. */
export class ScreenPlayer {
  private decoder: VideoDecoder | null = null;
  private codec: string | null = null;
  private accel: Accel = "prefer-hardware";
  private needKey = true;
  private latest: VideoFrame | null = null;
  private raf = 0;
  private channel: FrameChannel | null = null;
  private stopped = false;
  private playing = false;
  private lastTs = -1;
  private wraps = 0;
  /** Per codec string: how to configure it, null when this PC can't decode it. */
  private accelFor = new Map<string, Accel | null>();
  /** The codec being probed; its access units wait in `held`, in order. */
  private probing: string | null = null;
  private held: Unit[] = [];
  /** Frames out of the current decoder, and decoder failures without any frame per codec. */
  private frames = 0;
  private fails = new Map<string, number>();
  private offHidden: (() => void) | null = null;
  private readonly ctx: CanvasRenderingContext2D | null;

  constructor(
    private readonly canvas: HTMLCanvasElement,
    private readonly track: number,
    private readonly onStatus: (s: Status) => void,
  ) {
    this.ctx = canvas.getContext("2d", { alpha: false, desynchronized: true });
  }

  start(): void {
    // Minimized, nothing is drawn, so nothing is received either: unwatch, and watch again on
    // restore (the host keeps the current GOP, so playback resumes from a keyframe).
    this.offHidden = onWindowHidden((hidden) => (hidden ? this.pause() : void this.watch()));
    if (!isWindowHidden()) void this.watch();
  }

  stop(): void {
    if (this.stopped) return;
    this.stopped = true;
    this.offHidden?.();
    this.offHidden = null;
    if (this.raf) cancelAnimationFrame(this.raf);
    this.raf = 0;
    this.latest?.close();
    this.latest = null;
    this.held = [];
    this.closeDecoder();
    this.unwatch();
  }

  /** Opens a channel and subscribes to the track. */
  private async watch(): Promise<void> {
    if (this.stopped || this.channel) return;
    const ch = await frameChannel(this.onMessage);
    if (this.stopped || this.channel || isWindowHidden()) {
      ch.close();
      return;
    }
    this.channel = ch;
    this.needKey = true;
    try {
      await perTrack(this.track, () => invoke("screen_watch", { track: this.track, onFrame: ch.ipc }));
    } catch (err) {
      if (!this.stopped && this.channel === ch) this.onStatus({ kind: "error", text: errorText(err) });
    }
  }

  /**
   * Stops receiving, if this player subscribed. Queued behind its own screen_watch, and only
   * sent by a subscribed player, so it never removes the sink of a newer player of the track.
   */
  private unwatch(): void {
    const ch = this.channel;
    if (!ch) return;
    this.channel = null;
    ch.close();
    perTrack(this.track, () => invoke("screen_unwatch", { track: this.track })).catch(() => {
      // the share may already be gone
    });
  }

  private pause(): void {
    if (this.stopped || !this.channel) return;
    this.unwatch();
    this.held = [];
    this.probing = null;
    this.resetDecoder();
  }

  private onMessage = (buf: ArrayBuffer): void => {
    if (this.stopped || !(buf instanceof ArrayBuffer) || buf.byteLength <= HEADER) return;
    const view = new DataView(buf);
    if (view.getUint16(1) !== this.track) return;
    this.feed({ key: (view.getUint8(0) & 1) === 1, ts90: view.getUint32(3), au: new Uint8Array(buf, HEADER) });
  };

  /** Synchronous on purpose: decode order must follow arrival order. */
  private feed(u: Unit): void {
    if (this.probing !== null) {
      if (this.held.length < MAX_HELD) this.held.push(u);
      return;
    }
    if (u.key) {
      const codec = codecFromAnnexB(u.au);
      if (codec && (codec !== this.codec || !this.decoder)) {
        const accel = this.accelFor.get(codec);
        if (accel === undefined) {
          this.probe(codec, u);
          return;
        }
        // can't decode it: the error is on screen, ignore the share's keyframes
        if (accel === null) return;
        if (!this.configure(codec, accel)) return;
      }
    }
    const dec = this.decoder;
    if (!dec || dec.state !== "configured") return;
    if (this.needKey && !u.key) return;
    // the decoder can't keep up (software decode, a large share): skip to the next keyframe
    // instead of queueing without bound
    if (skipForBackpressure(u.key, dec.decodeQueueSize)) {
      this.needKey = true;
      return;
    }
    try {
      dec.decode(new EncodedVideoChunk({ type: u.key ? "key" : "delta", timestamp: this.micros(u.ts90), data: u.au }));
      this.needKey = false;
    } catch (err) {
      console.warn("screen decode failed", err);
      this.resetDecoder();
    }
  }

  /** Asks whether a new codec decodes here, holding its frames until the answer. */
  private probe(codec: string, first: Unit): void {
    this.probing = codec;
    this.held = [first];
    void decoderSupport(codec).then((accel) => {
      this.accelFor.set(codec, accel);
      if (this.stopped || this.probing !== codec) return;
      this.probing = null;
      const held = this.held;
      this.held = [];
      if (accel === null) {
        this.onStatus({ kind: "error", text: UNDECODABLE });
        return;
      }
      for (const u of held) this.feed(u);
    });
  }

  /** 90 kHz u32 clock (wraps every 13 h) to monotonic microseconds. */
  private micros(ts90: number): number {
    if (this.lastTs >= 0 && ts90 < this.lastTs && this.lastTs - ts90 > 0x80000000) this.wraps++;
    this.lastTs = ts90;
    return Math.round(((this.wraps * 0x100000000 + ts90) * 100) / 9);
  }

  private configure(codec: string, accel: Accel): boolean {
    this.closeDecoder();
    try {
      const dec: VideoDecoder = new VideoDecoder({ output: this.onFrame, error: (err) => this.onError(dec, err) });
      dec.configure({ codec, hardwareAcceleration: accel, optimizeForLatency: true });
      this.decoder = dec;
      this.codec = codec;
      this.accel = accel;
      this.frames = 0;
      this.needKey = true;
      return true;
    } catch (err) {
      this.decoder = null;
      this.codec = null;
      this.accelFor.set(codec, null);
      this.onStatus({ kind: "error", text: UNDECODABLE });
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

  private onError(dec: VideoDecoder, err: DOMException): void {
    console.warn("screen decoder error", err);
    if (dec !== this.decoder) return;
    const codec = this.codec;
    this.decoder = null;
    this.codec = null;
    this.needKey = true;
    if (!codec || this.stopped) return;
    const fails = this.frames > 0 ? 0 : (this.fails.get(codec) ?? 0) + 1;
    this.fails.set(codec, fails);
    // Chromium reports an unsupported config as OperationError, and a hardware-only failure
    // the same way: decide from whether this decoder ever produced a frame, not the name.
    switch (afterDecoderError(this.accel, this.frames > 0, fails)) {
      case "retry-software":
        this.accelFor.set(codec, "no-preference");
        break;
      case "unsupported":
        this.accelFor.set(codec, null);
        this.onStatus({ kind: "error", text: UNDECODABLE });
        break;
      case "reset":
        break;
    }
  }

  private onFrame = (frame: VideoFrame): void => {
    this.frames++;
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
