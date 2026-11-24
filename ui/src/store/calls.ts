// Pure decisions for voice rooms and calls, kept apart from the store so they can be tested.

import type { CallInfo, VoiceParticipant, VoiceSession } from "../api/types";

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

/**
 * A tile's person. `volume` is null when it is not known: tiles of a room you have not
 * joined (the app reports saved volumes only for joined rooms and calls).
 */
export type TilePerson = Omit<VoiceParticipant, "volume"> & { volume: number | null };

/**
 * Where the per-user volume slider starts: this session's choice, else the saved volume.
 * Null when neither is known: then no slider is shown, so a click on it can never replace
 * the saved volume with a made-up 100%.
 */
export function initialVolume(chosen: number | undefined, p: TilePerson): number | null {
  return chosen ?? p.volume;
}

/** Send the slider's value only when it differs from the one last sent (or shown at open). */
export function shouldSend(last: number, current: number): boolean {
  return Math.round(last * 100) !== Math.round(current * 100);
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

/** What a voice room's view shows: its live stage once joined, else who is inside and a Join button. */
export interface RoomStage {
  /** I am in this room: the live stage (tiles, screen share). The call panel holds the controls. */
  joined: boolean;
  /** "Join voice" is shown: viewing a room I am not in. Only this button sends `voice_join`. */
  join: boolean;
  /** Join is disabled: a call is ringing or active, so it would have to be ended first. */
  joinDisabled: boolean;
}

/**
 * Opening a room only shows it. Viewing never joins: no audio, no mic, no call panel for it.
 * `removed`: I was removed from the server, so there is nothing to join.
 */
export function roomStage(voice: VoiceSession | null, call: CallInfo | null, channelId: string, removed: boolean): RoomStage {
  const joined = voice?.kind === "room" && voice.channelId === channelId;
  return { joined, join: !joined && !removed, joinDisabled: call !== null };
}
