// Pure decisions of the screen share player, so they can be tested without WebCodecs.

/** How a decoder is configured. */
export type Accel = "prefer-hardware" | "no-preference";

/**
 * Runs async operations one after another per key. screen_watch and screen_unwatch for one
 * track are separate commands the app may run in any order, so an unwatch sent while a watch
 * is in flight could overtake it and leave a sink streaming to a dead channel. Chained per
 * track, each one waits for the previous to finish (also across players of the same track).
 */
export function keyedSerial<K>(): <T>(key: K, op: () => Promise<T>) => Promise<T> {
  const tails = new Map<K, Promise<void>>();
  return <T>(key: K, op: () => Promise<T>): Promise<T> => {
    const prev = tails.get(key) ?? Promise.resolve();
    const result = prev.then(op);
    const tail = result.then(
      () => {},
      () => {},
    );
    tails.set(key, tail);
    void tail.then(() => {
      if (tails.get(key) === tail) tails.delete(key);
    });
    return result;
  };
}

/** The acceleration to configure with, from isConfigSupported answers; null: can't decode. */
export function pickAccel(hardware: boolean, software: boolean): Accel | null {
  return hardware ? "prefer-hardware" : software ? "no-preference" : null;
}

/** Decoder failures without a single frame before a codec counts as undecodable here. */
export const MAX_FAILS_WITHOUT_FRAMES = 3;

/**
 * What to do after the decoder closed with an error. A hardware decoder that fails before
 * producing a frame is retried in software. Repeated failures without a frame mean this PC
 * can't decode the share: say so instead of rebuilding at every keyframe. Anything else is
 * a decode error mid-stream: rebuild at the next keyframe.
 */
export function afterDecoderError(accel: Accel, producedFrames: boolean, failsWithoutFrames: number): "retry-software" | "unsupported" | "reset" {
  if (producedFrames) return "reset";
  if (accel === "prefer-hardware") return "retry-software";
  return failsWithoutFrames >= MAX_FAILS_WITHOUT_FRAMES ? "unsupported" : "reset";
}

/** Delta frames are skipped while the decoder is this far behind (then wait for a keyframe). */
export const MAX_DECODE_QUEUE = 8;

/** True when a frame should be skipped because the decoder can't keep up. */
export function skipForBackpressure(key: boolean, queueSize: number): boolean {
  return !key && queueSize > MAX_DECODE_QUEUE;
}
