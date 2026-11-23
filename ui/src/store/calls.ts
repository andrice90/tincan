// Pure decisions for voice rooms and calls, kept apart from the store so they can be tested.

import type { CallInfo, VoiceSession } from "../api/types";

/** The command the call panel's red button sends. */
export type LeaveCommand = "voice_leave" | "call_hangup" | null;

/**
 * Which command leaves what the user is in. A room is left with `voice_leave` even while a
 * call rings out from it: hanging up would only cancel the ring and keep the room's mic live
 * (the backend leaves the room alone and sends no voice-updated). The ringing call survives
 * `voice_leave` and gets its own Cancel button.
 */
export function leaveCommand(voice: VoiceSession | null, call: CallInfo | null): LeaveCommand {
  if (voice?.kind === "room") return "voice_leave";
  if (voice?.kind === "call" || call) return "call_hangup";
  return null;
}

/** Which parts the call panel shows. */
export interface CallPanelParts {
  /** "Calling <name>" with a Cancel button, also above a room panel. */
  calling: boolean;
  /** The room or call panel with mute, deafen, share and leave. */
  voice: boolean;
  /** An active call whose voice session is not attached (yet, or attaching failed): Hang up only. */
  callNoAudio: boolean;
}

export function callPanelParts(voice: VoiceSession | null, call: CallInfo | null): CallPanelParts {
  return {
    calling: call?.state === "ringing-out" && voice?.kind !== "call",
    voice: voice !== null,
    callNoAudio: voice === null && call?.state === "active",
  };
}
