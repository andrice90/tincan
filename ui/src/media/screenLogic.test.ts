import { eq, test } from "../test/harness";
import { MAX_DECODE_QUEUE, MAX_FAILS_WITHOUT_FRAMES, afterDecoderError, keyedSerial, pickAccel, skipForBackpressure } from "./screenLogic";

const tick = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

test("RESOURCES-13: an unwatch sent while the watch is in flight runs after it", async () => {
  const serial = keyedSerial<number>();
  const log: string[] = [];
  const watch = serial(5, async () => {
    log.push("watch start");
    await tick(30);
    log.push("watch done");
  });
  const unwatch = serial(5, async () => {
    log.push("unwatch");
  });
  await Promise.all([watch, unwatch]);
  eq(log, ["watch start", "watch done", "unwatch"]);
});

test("RESOURCES-13: a failed watch does not block the unwatch, other tracks don't wait", async () => {
  const serial = keyedSerial<number>();
  const log: string[] = [];
  const watch = serial(5, async () => {
    await tick(30);
    throw new Error("gone");
  }).catch(() => log.push("watch failed"));
  const other = serial(6, async () => void log.push("track 6"));
  const unwatch = serial(5, async () => void log.push("unwatch 5"));
  await Promise.all([watch, other, unwatch]);
  eq(log, ["track 6", "watch failed", "unwatch 5"]);
});

test("UI-LOGIC-08: a codec only software can decode is configured for software", () => {
  // Edge: isConfigSupported(avc1.f4001f) is false with prefer-hardware, true with no-preference
  eq(pickAccel(false, true), "no-preference");
  eq(pickAccel(true, true), "prefer-hardware");
  eq(pickAccel(false, false), null);
});

test("UI-LOGIC-08: a hardware decoder that fails before any frame is retried in software", () => {
  eq(afterDecoderError("prefer-hardware", false, 1), "retry-software");
});

test("UI-LOGIC-08: repeated failures without a frame end in the can't-decode message", () => {
  eq(afterDecoderError("no-preference", false, 1), "reset");
  eq(afterDecoderError("no-preference", false, MAX_FAILS_WITHOUT_FRAMES), "unsupported");
  // after frames were decoded, an error is a mid-stream decode error
  eq(afterDecoderError("prefer-hardware", true, 0), "reset");
});

test("RESOURCES-8: delta frames are skipped while the decoder is behind", () => {
  eq(skipForBackpressure(false, MAX_DECODE_QUEUE + 1), true);
  eq(skipForBackpressure(false, 2), false);
  eq(skipForBackpressure(true, MAX_DECODE_QUEUE + 10), false, "a keyframe resets the backlog");
});
