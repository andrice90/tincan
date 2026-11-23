// The soundboard's clip list. Pure, so it can be tested.

import type { Clip } from "../api/types";

/** Heard clips the app keeps per session (MAX_HEARD in tincan-app media/clips.rs). */
export const MAX_HEARD = 50;

/**
 * The clip list after a sound-played event, mirroring the app's heard list: a clip I don't
 * own moves to the end (newest last) and only the newest MAX_HEARD of them stay, so the
 * soundboard never shows a tile the app has forgotten and a flood of announcements stays
 * bounded. Owned clips are listed first by the app and are never dropped here.
 */
export function addPlayedClip(clips: Clip[], clip: Clip): Clip[] {
  if (clip.owned) return clips.some((c) => c.hash === clip.hash) ? clips : [...clips, clip];
  const rest = clips.filter((c) => c.hash !== clip.hash);
  let heard = rest.reduce((n, c) => (c.owned ? n : n + 1), 0) + 1;
  const out: Clip[] = [];
  for (const c of rest) {
    if (!c.owned && heard > MAX_HEARD) {
      heard--;
      continue;
    }
    out.push(c);
  }
  out.push(clip);
  return out;
}
