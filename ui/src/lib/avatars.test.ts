import type { DmSummary, Me, Member, VoiceParticipant } from "../api/types";
import { eq, test } from "../test/harness";
import { buildAvatarIndex } from "./avatars";

const URL = (h: string) => `http://tincan.localhost/file/${h}`;

function member(userId: string, avatar: string | null): Member {
  return { userId, name: userId, online: true, isHost: false, isOwner: false, voiceChannelId: null, muted: false, deafened: false, sharing: false, speaking: false, avatar };
}

test("avatars: every list contributes, missing ones show the initial", () => {
  const dm: DmSummary = { convId: "c", userId: "riley", name: "Riley", online: false, unread: false, lastMessageAt: null, avatar: URL("bb") };
  const p: VoiceParticipant = { userId: "casey", name: "Casey", muted: false, deafened: false, speaking: false, avatar: URL("cc"), screenTrack: null, volume: 1, soundboardVolume: 1, shareVolume: 1 };
  const idx = buildAvatarIndex({ s1: [member("alex", URL("aa")), member("jordan", null)] }, [dm], [p], null);
  eq(idx.get("alex"), URL("aa"));
  eq(idx.get("riley"), URL("bb"), "a DM-only contact");
  eq(idx.get("casey"), URL("cc"), "a voice participant");
  eq(idx.has("jordan"), false, "no picture yet");
});

test("avatars: mine wins and a removal shows at once", () => {
  const me: Me = { userId: "sam", userCode: "tincan1usam", name: "Sam", avatar: URL("new") };
  eq(buildAvatarIndex({ s: [member("sam", URL("old"))] }, [], [], me).get("sam"), URL("new"));
  eq(buildAvatarIndex({ s: [member("sam", URL("old"))] }, [], [], { ...me, avatar: null }).has("sam"), false);
});
