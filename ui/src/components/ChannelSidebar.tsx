// Channel list for one server: header with host state and server menu, text
// channels, voice rooms with the people inside.

import { ChevronDown, Hash, HeadphoneOff, Image as ImageIcon, ImageOff, LogOut, MessageSquare, MicOff, Monitor, Pencil, Plus, Trash2, UserPlus, Volume2 } from "lucide-react";
import { memo, useMemo, useRef, type MouseEvent, type ReactNode } from "react";
import type { Channel, Connection, Member, ServerSummary, VoiceSession } from "../api/types";
import { openDialog, openRoomChat, pickImageFor, removeServerIcon, selectChannel } from "../store/actions";
import { roomChatMode } from "../store/chatView";
import { useStore } from "../store/store";
import { unreadBadge } from "../lib/unread";
import { ICON_SM, IconButton, PersonName, SpeakingAvatar, UnreadBadge } from "./common";
import { Menu, useMenu, type MenuItem } from "./Menu";

/** One line of host state for the header. */
export function hostLine(c: Connection, meId: string | undefined): string {
  switch (c.state) {
    case "connecting":
      return "Connecting";
    case "offline":
      return "Offline";
    case "solo":
      return "Only you are online";
    case "hosting":
      return "Hosted by you";
    case "member":
      if (c.hostId && c.hostId === meId) return "Hosted by you";
      return c.hostName ? `Hosted by ${c.hostName}` : "Connected";
  }
}

const EMPTY_CHANNELS: Channel[] = [];
const EMPTY_MEMBERS: Member[] = [];

export function ChannelSidebar({ server }: { server: ServerSummary }) {
  const channels = useStore((s) => s.channels[server.id] ?? EMPTY_CHANNELS);
  const members = useStore((s) => s.members[server.id] ?? EMPTY_MEMBERS);
  const view = useStore((s) => s.view);
  const voice = useStore((s) => s.voice);
  const meId = useStore((s) => s.me?.userId);
  const menu = useMenu();
  const headerRef = useRef<HTMLButtonElement>(null);

  const selected = view.kind === "server" ? view.channelId : null;
  const sorted = useMemo(() => channels.slice().sort((a, b) => a.position - b.position), [channels]);
  const text = sorted.filter((c) => c.kind === "text");
  const rooms = sorted.filter((c) => c.kind === "voice");
  const conn = server.connection;

  const items: MenuItem[] = [
    { label: "Invite people", icon: <UserPlus size={ICON_SM} />, onSelect: () => openDialog({ kind: "invite", serverId: server.id }) },
  ];
  if (server.isOwner) {
    items.push(
      { label: "Rename server", icon: <Pencil size={ICON_SM} />, onSelect: () => openDialog({ kind: "rename-server", serverId: server.id }) },
      { label: "Change icon", icon: <ImageIcon size={ICON_SM} />, onSelect: () => pickImageFor({ kind: "server-icon", serverId: server.id }) },
    );
    if (server.icon) {
      items.push({ label: "Remove icon", icon: <ImageOff size={ICON_SM} />, onSelect: () => removeServerIcon(server.id) });
    }
    items.push(
      {
        label: "Create channel",
        icon: <Plus size={ICON_SM} />,
        onSelect: () => openDialog({ kind: "create-channel", serverId: server.id, channelKind: "text" }),
      },
    );
  }
  items.push("separator", {
    label: "Leave server",
    icon: <LogOut size={ICON_SM} />,
    danger: true,
    onSelect: () => openDialog({ kind: "leave-server", serverId: server.id }),
  });

  return (
    <>
      <button
        ref={headerRef}
        type="button"
        className="side-header side-header-btn"
        aria-haspopup="menu"
        aria-expanded={menu.anchor !== null}
        onClick={() => headerRef.current && menu.openBelow(headerRef.current)}
      >
        <span className="side-header-text">
          <span className="side-title truncate">
            <bdi>{server.name}</bdi>
          </span>
          <span className={conn.state === "offline" || conn.state === "connecting" ? "side-sub is-warn" : "side-sub"}>
            {hostLine(conn, meId)}
          </span>
        </span>
        <ChevronDown size={ICON_SM} aria-hidden="true" />
      </button>
      {menu.anchor && <Menu anchor={menu.anchor} items={items} onClose={menu.close} label={`${server.name} menu`} />}

      <div className="side-scroll">
        <ChannelGroup title="Text channels" server={server} kind="text">
          {text.map((c) => (
            <ChannelRow key={c.id} channel={c} server={server} selected={c.id === selected} />
          ))}
        </ChannelGroup>
        <ChannelGroup title="Voice rooms" server={server} kind="voice">
          {rooms.map((c) => (
            <VoiceRoomRow key={c.id} channel={c} server={server} selected={c.id === selected} members={members} voice={voice} />
          ))}
        </ChannelGroup>
      </div>
    </>
  );
}

function ChannelGroup({
  title,
  server,
  kind,
  children,
}: {
  title: string;
  server: ServerSummary;
  kind: "text" | "voice";
  children: ReactNode;
}) {
  return (
    <section className="channel-group" aria-label={title}>
      <div className="channel-group-head">
        <h3>{title}</h3>
        {server.isOwner && (
          <IconButton
            small
            label={kind === "text" ? "Create text channel" : "Create voice room"}
            onClick={() => openDialog({ kind: "create-channel", serverId: server.id, channelKind: kind })}
          >
            <Plus size={ICON_SM} />
          </IconButton>
        )}
      </div>
      <ul className="channel-list">{children}</ul>
    </section>
  );
}

function useChannelMenu(server: ServerSummary, channel: Channel): { onContextMenu?: (e: MouseEvent) => void; node: ReactNode } {
  const menu = useMenu();
  if (!server.isOwner) return { node: null };
  const room = channel.kind === "voice";
  const items: MenuItem[] = [
    {
      label: room ? "Rename voice room" : "Rename channel",
      icon: <Pencil size={ICON_SM} />,
      onSelect: () => openDialog({ kind: "rename-channel", serverId: server.id, channelId: channel.id }),
    },
    {
      label: room ? "Delete voice room" : "Delete channel",
      icon: <Trash2 size={ICON_SM} />,
      danger: true,
      onSelect: () => openDialog({ kind: "delete-channel", serverId: server.id, channelId: channel.id }),
    },
  ];
  return {
    onContextMenu: menu.openAt,
    node: menu.anchor && <Menu anchor={menu.anchor} items={items} onClose={menu.close} label={`${channel.name} menu`} />,
  };
}

const ChannelRow = memo(function ChannelRow({ channel, server, selected }: { channel: Channel; server: ServerSummary; selected: boolean }) {
  const ctx = useChannelMenu(server, channel);
  const badge = unreadBadge(channel.unread, selected);
  return (
    <li>
      <button
        type="button"
        className={badge ? "channel-row is-unread" : "channel-row"}
        aria-current={selected ? "page" : undefined}
        onClick={() => selectChannel(server.id, channel)}
        onContextMenu={ctx.onContextMenu}
      >
        <Hash size={ICON_SM} aria-hidden="true" />
        <span className="truncate">
          <bdi>{channel.name}</bdi>
        </span>
        {badge && <UnreadBadge badge={badge} />}
      </button>
      {ctx.node}
    </li>
  );
});

interface RoomPerson {
  userId: string;
  name: string;
  muted: boolean;
  deafened: boolean;
  sharing: boolean;
}

function VoiceRoomRow({
  channel,
  server,
  selected,
  members,
  voice,
}: {
  channel: Channel;
  server: ServerSummary;
  selected: boolean;
  members: Member[];
  voice: VoiceSession | null;
}) {
  const ctx = useChannelMenu(server, channel);
  // the dot stays until the room's chat itself is on screen (its tiles alone don't count)
  const chatShown = useStore((s) => roomChatMode(s.view, channel, s.roomChatPanel) !== null);
  const people: RoomPerson[] = useMemo(() => {
    if (voice?.kind === "room" && voice.channelId === channel.id) {
      return voice.participants.map((p) => ({
        userId: p.userId,
        name: p.name,
        muted: p.muted,
        deafened: p.deafened,
        sharing: p.screenTrack !== null,
      }));
    }
    return members
      .filter((m) => m.online && m.voiceChannelId === channel.id)
      .map((m) => ({ userId: m.userId, name: m.name, muted: m.muted, deafened: m.deafened, sharing: m.sharing }));
  }, [voice, members, channel.id]);

  const badge = unreadBadge(channel.unread, chatShown);
  const unread = badge !== null;

  return (
    <li>
      <div className="channel-row-wrap">
        <button
          type="button"
          className={unread ? "channel-row is-unread" : "channel-row"}
          aria-current={selected ? "page" : undefined}
          onClick={() => selectChannel(server.id, channel)}
          onContextMenu={ctx.onContextMenu}
        >
          <Volume2 size={ICON_SM} aria-hidden="true" />
          <span className="truncate">
            <bdi>{channel.name}</bdi>
          </span>
          {badge && <UnreadBadge badge={badge} />}
        </button>
        <IconButton small className="room-chat-btn" label={`Open chat for ${channel.name}`} onClick={() => openRoomChat(server.id, channel)}>
          <MessageSquare size={ICON_SM} />
        </IconButton>
      </div>
      {ctx.node}
      {people.length > 0 && (
        <ul className="room-people" aria-label={`In ${channel.name}`}>
          {people.map((p) => (
            <li key={p.userId} className="room-person">
              <SpeakingAvatar userId={p.userId} name={p.name} size={20} />
              <span className="truncate">
                <PersonName userId={p.userId} name={p.name} />
              </span>
              <span className="room-person-icons">
                {p.sharing && <Monitor size={14} role="img" aria-label="Sharing screen" />}
                {p.deafened ? <HeadphoneOff size={14} role="img" aria-label="Deafened" /> : p.muted && <MicOff size={14} role="img" aria-label="Muted" />}
              </span>
            </li>
          ))}
        </ul>
      )}
    </li>
  );
}
