import { eq, test } from "../test/harness";
import { shareAudioNote, shareAudioOption } from "./shareAudio";

test("share audio: the picker names what its switch shares", () => {
  eq(shareAudioOption("window"), "Audio from this app only");
  eq(shareAudioOption("display"), "Everything your PC plays, except Tincan");
  eq(shareAudioOption(null), "Everything your PC plays, except Tincan");
});

test("share audio: the sharer is told what goes out", () => {
  eq(shareAudioNote("app"), "With the app's audio");
  eq(shareAudioNote("system"), "With your PC's audio, except Tincan");
  eq(shareAudioNote("off"), "Without audio");
});
