import type { CallInfo, VoiceSession } from "../api/types";
import { eq, test } from "../test/harness";
import { callPanelParts, initialVolume, leaveCommand } from "./calls";

function voice(kind: "room" | "call"): VoiceSession {
  return {
    kind,
    convId: "conv",
    channelId: kind === "room" ? "lounge" : null,
    name: "Lounge",
    participants: [],
    muted: false,
    deafened: false,
    sharing: false,
    path: "direct",
    rttMs: null,
  };
}

function call(state: CallInfo["state"]): CallInfo {
  return { callId: "c1", userId: "bob", name: "Bob", state };
}

test("APP-MEDIA-1: Leave voice in a room leaves the room even while a call rings out", () => {
  eq(leaveCommand(voice("room"), call("ringing-out")), "voice_leave");
  eq(leaveCommand(voice("room"), null), "voice_leave");
});

test("APP-MEDIA-1: Hang up ends a call, with or without a session attached", () => {
  eq(leaveCommand(voice("call"), call("active")), "call_hangup");
  eq(leaveCommand(null, call("active")), "call_hangup");
  eq(leaveCommand(null, call("ringing-out")), "call_hangup");
  eq(leaveCommand(null, null), null);
});

test("APP-MEDIA-1: a ringing call shows its own Cancel row above the room panel", () => {
  eq(callPanelParts(voice("room"), call("ringing-out")), { calling: true, voice: true, callNoAudio: false });
  eq(callPanelParts(null, call("ringing-out")), { calling: true, voice: false, callNoAudio: false });
  eq(callPanelParts(voice("room"), null), { calling: false, voice: true, callNoAudio: false });
  // a ringing-in call is answered in its own dialog
  eq(callPanelParts(voice("room"), call("ringing-in")), { calling: false, voice: true, callNoAudio: false });
});

test("UI-LOGIC-09: an active call without a voice session still gets a Hang up panel", () => {
  eq(callPanelParts(null, call("active")), { calling: false, voice: false, callNoAudio: true });
  eq(callPanelParts(voice("call"), call("active")), { calling: false, voice: true, callNoAudio: false });
  eq(callPanelParts(null, null), { calling: false, voice: false, callNoAudio: false });
});

test("UI-LOGIC-13: the volume slider starts at the saved volume, not 100%", () => {
  const p = { userId: "alex", name: "Alex", muted: false, deafened: false, speaking: false, screenTrack: null, volume: 0.3 };
  eq(initialVolume(undefined, p), 0.3, "saved by the app");
  eq(initialVolume(0.8, p), 0.8, "moved in this session");
  eq(initialVolume(undefined, { ...p, volume: undefined }), 1, "app without the field");
});
