// Which conversation's messages are on screen. A text channel always shows its chat; a voice
// room shows its chat as the whole page (opened from the sidebar's chat button) or as a panel
// next to the tiles (the room header's Chat button). Loading, unread and mark_read all follow
// this one answer, so a room's chat is never marked read while only its tiles are showing.

import type { Channel, ChannelId, ConvId } from "../api/types";
import type { View } from "./store";

/** How a voice room's chat is shown: the whole page, a panel beside the tiles, or not at all. */
export type RoomChatMode = "page" | "panel" | null;

/** The chat mode of voice room `channel` when it is the channel in `view`. */
export function roomChatMode(view: View, channel: Channel, panelOpen: boolean): RoomChatMode {
  if (channel.kind !== "voice") return null;
  if (view.kind !== "server" || view.serverId !== channel.serverId || view.channelId !== channel.id) return null;
  if (view.chat) return "page";
  return panelOpen ? "panel" : null;
}

/** The conversation whose messages are visible in the main area, if any. */
export function shownChat(
  view: View,
  channels: Record<string, Channel[] | undefined>,
  panelOpen: boolean,
): { convId: ConvId; channelId: ChannelId | null } | null {
  if (view.kind === "server" && view.channelId) {
    const ch = channels[view.serverId]?.find((c) => c.id === view.channelId);
    if (!ch) return null;
    if (ch.kind === "voice" && roomChatMode(view, ch, panelOpen) === null) return null;
    return { convId: view.serverId, channelId: ch.id };
  }
  if (view.kind === "dms" && view.convId) return { convId: view.convId, channelId: null };
  return null;
}
