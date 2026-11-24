// What the shell's persistent slots show: the composer under the content, and the panel
// on the right. They live outside the keyed channel views so they can animate closed
// after the view they belong to is gone (lib/presence.ts keeps the last answer).

import type { Channel, ChannelId, ConvId, DmSummary, ServerId, ServerSummary } from "../api/types";
import type { Layout } from "../lib/layout";
import type { View } from "./store";

export interface ComposerSpot {
  convId: ConvId;
  channelId: ChannelId | null;
  placeholder: string;
  /** I was removed from the server: a note with "Join again" in place of the composer. */
  removed: boolean;
  /** Same conversation, placeholder and removal: the same composer. */
  sig: string;
}

function spot(convId: ConvId, channelId: ChannelId | null, placeholder: string, removed: boolean): ComposerSpot {
  return { convId, channelId, placeholder, removed, sig: `${convId}\n${channelId ?? ""}\n${placeholder}\n${removed}` };
}

/**
 * The composer for the main area: a text channel, a voice room's chat page, or a DM.
 * A voice room's tiles have none (the chat panel beside them has its own).
 */
export function composerSpot(
  view: View,
  servers: readonly ServerSummary[],
  channels: Record<ServerId, Channel[] | undefined>,
  dms: readonly DmSummary[],
): ComposerSpot | null {
  if (view.kind === "dms") {
    const dm = view.convId ? dms.find((d) => d.convId === view.convId) : undefined;
    return dm ? spot(dm.convId, null, `Message ${dm.name}`, false) : null;
  }
  if (view.kind !== "server" || !view.channelId) return null;
  const server = servers.find((s) => s.id === view.serverId);
  const channel = channels[view.serverId]?.find((c) => c.id === view.channelId);
  if (!server || !channel) return null;
  if (channel.kind === "text") return spot(server.id, channel.id, `Message #${channel.name}`, server.removed === true);
  if (view.chat) return spot(server.id, channel.id, `Message ${channel.name}`, server.removed === true);
  return null;
}

export type RightPanel =
  | { kind: "members"; serverId: ServerId; sig: string }
  | { kind: "room-chat"; serverId: ServerId; channelId: ChannelId; sig: string };

/**
 * The right panel: the member list beside a chat page (a text channel or a voice room's
 * chat page), or in Floating the voice room's chat beside its tiles. Classic keeps that
 * chat inside the room view, under its header.
 */
export function rightPanel(
  view: View,
  channels: Record<ServerId, Channel[] | undefined>,
  showMembers: boolean,
  roomChatPanel: boolean,
  layout: Layout,
): RightPanel | null {
  if (view.kind !== "server" || !view.channelId) return null;
  const channel = channels[view.serverId]?.find((c) => c.id === view.channelId);
  if (!channel) return null;
  if (channel.kind === "voice" && !view.chat) {
    if (layout !== "floating" || !roomChatPanel) return null;
    return { kind: "room-chat", serverId: view.serverId, channelId: channel.id, sig: `chat\n${view.serverId}\n${channel.id}` };
  }
  if (!showMembers) return null;
  return { kind: "members", serverId: view.serverId, sig: `members\n${view.serverId}` };
}
