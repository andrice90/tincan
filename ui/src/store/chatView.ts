// Which conversation's messages are on screen. A text channel always shows its chat; a voice
// room shows its tiles, and its chat only in the panel beside them (the room header's Chat
// button or the chat button on its sidebar row). Loading, unread and mark_read all follow
// this one answer, so a room's chat is never marked read while only its tiles are showing.

import type { Channel, ChannelId, ConvId } from "../api/types";
import type { View } from "./store";

/** Voice room `channel` is on screen. */
export function roomOnScreen(view: View, channel: Channel): boolean {
  return channel.kind === "voice" && view.kind === "server" && view.serverId === channel.serverId && view.channelId === channel.id;
}

/** The chat of voice room `channel` is on screen: the room is, and its chat panel is open. */
export function roomChatShown(view: View, channel: Channel, panelOpen: boolean): boolean {
  return panelOpen && roomOnScreen(view, channel);
}

/**
 * The chat button on a voice room's sidebar row. On the room already on screen it does what
 * the header's Chat button does: toggle the panel. On any other room it opens that room with
 * the panel open (toggling there could close a panel that was already open).
 */
export function roomChatClick(view: View, channel: Channel, panelOpen: boolean): { open: boolean; navigate: boolean } {
  if (roomOnScreen(view, channel)) return { open: !panelOpen, navigate: false };
  return { open: true, navigate: true };
}

/** The conversation whose messages are visible, if any. */
export function shownChat(
  view: View,
  channels: Record<string, Channel[] | undefined>,
  panelOpen: boolean,
): { convId: ConvId; channelId: ChannelId | null } | null {
  if (view.kind === "server" && view.channelId) {
    const ch = channels[view.serverId]?.find((c) => c.id === view.channelId);
    if (!ch) return null;
    if (ch.kind === "voice" && !roomChatShown(view, ch, panelOpen)) return null;
    return { convId: view.serverId, channelId: ch.id };
  }
  if (view.kind === "dms" && view.convId) return { convId: view.convId, channelId: null };
  return null;
}
