// Wording for screen share audio: what the picker's switch shares, and what the sharer is
// told is going out.

import type { ScreenSource, ShareAudio } from "../api/types";

/** The note under the picker's "Share audio" switch for the picked source. */
export function shareAudioOption(kind: ScreenSource["kind"] | null): string {
  return kind === "window" ? "Audio from this app only" : "Everything your PC plays, except Tincan";
}

/** The sharer's line under "You're sharing your screen". */
export function shareAudioNote(audio: ShareAudio): string {
  switch (audio) {
    case "app":
      return "With the app's audio";
    case "system":
      return "With your PC's audio, except Tincan";
    case "off":
      return "Without audio";
  }
}
