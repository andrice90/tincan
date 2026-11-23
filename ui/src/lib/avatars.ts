// Which picture to show for a user: every list that carries an avatar URL, merged.

import type { DmSummary, Me, Member, UserId, VoiceParticipant } from "../api/types";

/**
 * Avatar URL per user from server members, DMs, voice participants and me. Mine wins, and
 * after I remove mine it is gone at once (other lists may still carry the old URL).
 */
export function buildAvatarIndex(
  members: Record<string, Member[]>,
  dms: DmSummary[],
  participants: VoiceParticipant[],
  me: Me | null,
): Map<UserId, string> {
  const index = new Map<UserId, string>();
  const add = (userId: UserId, url: string | null | undefined) => {
    if (url) index.set(userId, url);
  };
  for (const list of Object.values(members)) for (const m of list) add(m.userId, m.avatar);
  for (const d of dms) add(d.userId, d.avatar);
  for (const p of participants) add(p.userId, p.avatar);
  if (me) {
    if (me.avatar) index.set(me.userId, me.avatar);
    else index.delete(me.userId);
  }
  return index;
}
