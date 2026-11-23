import type { Clip } from "../api/types";
import { eq, test } from "../test/harness";
import { CLIP_EXTENSIONS, MAX_HEARD, addPlayedClip } from "./clips";

function clip(hash: string, owned: boolean, name = hash): Clip {
  return { hash, name, durationMs: 1000, size: 1000, owned, heardFrom: owned ? null : "peer", transfer: null };
}

const hashes = (list: Clip[]) => list.map((c) => c.hash);

test("APP-MEDIA-3: heard clips are capped at the app's 50, oldest dropped first", () => {
  let list: Clip[] = [clip("mine1", true), clip("mine2", true)];
  for (let i = 0; i < 120; i++) list = addPlayedClip(list, clip(`h${i}`, false));
  eq(MAX_HEARD, 50, "cap");
  eq(list.filter((c) => !c.owned).length, 50, "heard count");
  eq(hashes(list).slice(0, 2), ["mine1", "mine2"], "owned clips kept");
  eq(hashes(list)[2], "h70", "oldest kept heard clip");
  eq(hashes(list)[51], "h119", "newest heard clip last");
});

test("APP-MEDIA-3: a clip heard again moves to the end, like the app's list", () => {
  let list: Clip[] = [clip("mine", true), clip("a", false), clip("b", false), clip("c", false)];
  list = addPlayedClip(list, clip("a", false, "renamed"));
  eq(hashes(list), ["mine", "b", "c", "a"]);
  eq(list[3].name, "renamed", "fresh copy");
});

test("APP-MEDIA-3: an owned clip never counts against the cap and is not duplicated", () => {
  const list = [clip("mine", true), clip("a", false)];
  eq(addPlayedClip(list, clip("mine", true)), list, "unchanged");
  eq(hashes(addPlayedClip(list, clip("new-owned", true))), ["mine", "a", "new-owned"]);
});

test("SPEC-GAPS-8: the Add sound picker offers only formats the app can decode", () => {
  // tincan-audio decodes with symphonia (mp3, aac/m4a, wav, ogg vorbis, flac); it has no Opus decoder
  eq(CLIP_EXTENSIONS, ["mp3", "wav", "ogg", "flac", "m4a"]);
});
