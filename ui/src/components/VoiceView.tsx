// Voice room view and the shared call stage: participant tiles, and while
// someone shares a screen, that share on the stage with tiles in a strip.

import { HeadphoneOff, Maximize2, MessageSquare, MicOff, Minimize2, Monitor, Volume2 } from "lucide-react";
import { memo, useEffect, useState, type MouseEvent } from "react";
import type { Channel, ChannelId, Member, ServerId, ServerSummary, VoiceParticipant, VoiceSession } from "../api/types";
import { ScreenView } from "../media/ScreenView";
import { joinVoice, messageUser, setFullStage, stopShare, toggleRoomChat } from "../store/actions";
import type { TilePerson } from "../store/calls";
import { useStore } from "../store/store";
import { ChannelMessages } from "./ChatView";
import { ICON, ICON_SM, IconButton, PersonName, SpeakingAvatar } from "./common";
import { Popover, useMenu } from "./Menu";
import { PersonVolumes } from "./VolumeSliders";
import { tileGrid } from "../lib/tileGrid";
import type { CSSProperties } from "react";

/** The tile grid's columns and rows as CSS variables (see `.tiles` in app.css). */
function gridStyle(n: number): CSSProperties {
  const { cols, rows } = tileGrid(n);
  return { "--cols": cols, "--rows": rows } as CSSProperties;
}

function VolumePopover({ p, anchor, onClose }: { p: TilePerson; anchor: NonNullable<ReturnType<typeof useMenu>["anchor"]>; onClose: () => void }) {
  return (
    <Popover anchor={anchor} onClose={onClose} label={`${p.name} options`} className="volume-pop">
      <span className="menu-head-name truncate">
        <PersonName userId={p.userId} name={p.name} />
      </span>
      <PersonVolumes userId={p.userId} known={p} />
      <button
        type="button"
        className="menu-item"
        onClick={() => {
          onClose();
          messageUser(p.userId);
        }}
      >
        <MessageSquare size={ICON_SM} />
        <span>Message</span>
      </button>
    </Popover>
  );
}

const Tile = memo(function Tile({
  p,
  isMe,
  small,
  onPick,
  picked,
  sharing = p.screenTrack !== null,
}: {
  p: TilePerson;
  isMe: boolean;
  small?: boolean;
  onPick?: (track: number) => void;
  picked?: boolean;
  /** Overrides the share marker when the track is not known (rooms you are not in). */
  sharing?: boolean;
}) {
  const menu = useMenu();
  const pickable = sharing && onPick !== undefined && !isMe;
  const onContextMenu = (e: MouseEvent) => {
    if (!isMe) menu.openAt(e);
  };
  const cls = ["tile", small ? "tile-sm" : "", picked ? "is-picked" : "", pickable ? "is-pickable" : ""].filter(Boolean).join(" ");
  const body = (
    <>
      <SpeakingAvatar userId={p.userId} name={p.name} size={small ? 40 : 64} />
      <span className="tile-name truncate">
        <PersonName userId={p.userId} name={p.name} you={isMe} />
      </span>
      <span className="tile-icons">
        {sharing && <Monitor size={ICON_SM} role="img" aria-label="Sharing screen" />}
        {p.deafened ? (
          <HeadphoneOff size={ICON_SM} role="img" aria-label="Deafened" />
        ) : (
          p.muted && <MicOff size={ICON_SM} role="img" aria-label="Muted" />
        )}
      </span>
    </>
  );
  return (
    <>
      {pickable ? (
        <button
          type="button"
          className={cls}
          aria-pressed={picked}
          aria-label={`Watch ${p.name}'s screen`}
          title={`Watch ${p.name}'s screen`}
          onClick={() => p.screenTrack !== null && onPick?.(p.screenTrack)}
          onContextMenu={onContextMenu}
        >
          {body}
        </button>
      ) : (
        <div className={cls} onContextMenu={onContextMenu}>
          {body}
        </div>
      )}
      {menu.anchor && <VolumePopover p={p} anchor={menu.anchor} onClose={menu.close} />}
    </>
  );
});

/** Tiles and the screen share stage for a joined room or call. */
export function VoiceStage({ voice }: { voice: VoiceSession }) {
  const me = useStore((s) => s.me);
  const meId = me?.userId;
  const full = useStore((s) => s.fullStage);
  const [picked, setPicked] = useState<number | null>(null);

  const participants: VoiceParticipant[] =
    me && !voice.participants.some((p) => p.userId === me.userId)
      ? [
          {
            userId: me.userId,
            name: me.name,
            muted: voice.muted,
            deafened: voice.deafened,
            speaking: false,
            avatar: me.avatar,
            screenTrack: null,
            volume: 1,
            soundboardVolume: 1,
          },
          ...voice.participants,
        ]
      : voice.participants;

  const sharers = participants.filter((p) => p.screenTrack !== null);
  const others = sharers.filter((p) => p.userId !== meId);
  const stagePerson =
    sharers.find((p) => p.screenTrack === picked) ?? others[0] ?? sharers.find((p) => p.userId === meId) ?? null;
  const iShare = voice.sharing && !sharers.some((p) => p.userId === meId);

  useEffect(() => {
    if (!full) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setFullStage(false);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [full]);

  useEffect(() => {
    if (!stagePerson && full) setFullStage(false);
  }, [stagePerson, full]);

  if (!stagePerson && !iShare) {
    return (
      <div className="tiles" style={gridStyle(participants.length)}>
        {participants.map((p) => (
          <Tile key={p.userId} p={p} isMe={p.userId === meId} />
        ))}
      </div>
    );
  }

  const mine = !stagePerson || stagePerson.userId === meId;
  return (
    <div className="stage-layout">
      <div
        className={full ? "stage is-full" : "stage"}
        onDoubleClick={() => !mine && setFullStage(!full)}
      >
        {mine ? (
          <div className="stage-self">
            <p>You're sharing your screen</p>
            <button type="button" className="btn" onClick={stopShare}>
              Stop sharing
            </button>
          </div>
        ) : (
          <>
            {stagePerson.screenTrack !== null && (
              <ScreenView key={stagePerson.screenTrack} track={stagePerson.screenTrack} label={`${stagePerson.name}'s screen`} />
            )}
            <span className="stage-label">
              <bdi>{stagePerson.name}</bdi>'s screen
            </span>
            <IconButton
              className="stage-full-btn"
              label={full ? "Exit full window" : "Full window"}
              onClick={() => setFullStage(!full)}
            >
              {full ? <Minimize2 size={ICON} /> : <Maximize2 size={ICON} />}
            </IconButton>
          </>
        )}
      </div>
      <div className="strip">
        {participants.map((p) => (
          <Tile
            key={p.userId}
            p={p}
            isMe={p.userId === meId}
            small
            onPick={setPicked}
            picked={p.screenTrack !== null && p.screenTrack === stagePerson?.screenTrack}
          />
        ))}
      </div>
    </div>
  );
}

const EMPTY_MEMBERS: Member[] = [];

/** The main area for a voice room: live stage when joined, who is inside otherwise. */
export function VoiceRoomView({ server, channel }: { server: ServerSummary; channel: Channel }) {
  const voice = useStore((s) => s.voice);
  const call = useStore((s) => s.call);
  const members = useStore((s) => s.members[server.id] ?? EMPTY_MEMBERS);
  const joined = voice?.kind === "room" && voice.channelId === channel.id;
  const inside = members.filter((m) => m.online && m.voiceChannelId === channel.id);
  const chatOpen = useStore((s) => s.roomChatPanel);
  // Floating shows the chat panel as the right card (the shell's right slot).
  const inlineChat = useStore((s) => s.layout !== "floating");
  const unread = channel.unread && !chatOpen;

  return (
    <>
      <header className="main-header">
        <h1 className="main-title">
          <Volume2 size={ICON} aria-hidden="true" className="faint" />
          <span className="truncate">
            <bdi>{channel.name}</bdi>
          </span>
        </h1>
        <div className="main-actions">
          {!joined && (
            <button type="button" className="btn btn-primary btn-sm" disabled={call !== null} onClick={() => joinVoice(server.id, channel.id)}>
              Join voice
            </button>
          )}
          <button
            type="button"
            className={unread ? "btn btn-ghost btn-sm room-chat-toggle is-unread" : "btn btn-ghost btn-sm room-chat-toggle"}
            aria-pressed={chatOpen}
            onClick={toggleRoomChat}
          >
            Chat
            {unread && <span className="sr-only">, unread messages</span>}
          </button>
        </div>
      </header>
      <div className="voice-split">
        <div className="voice-body">
          {joined && voice ? (
            <VoiceStage voice={voice} />
          ) : inside.length === 0 ? (
            <p className="voice-empty">No one is here yet.</p>
          ) : (
            <div className="tiles" style={gridStyle(inside.length)}>
              {inside.map((m) => (
                <Tile
                  key={m.userId}
                  p={{ userId: m.userId, name: m.name, muted: m.muted, deafened: m.deafened, speaking: m.speaking, avatar: m.avatar, screenTrack: null, volume: null, soundboardVolume: null }}
                  isMe={false}
                  sharing={m.sharing}
                />
              ))}
            </div>
          )}
        </div>
        {chatOpen && inlineChat && <RoomChat server={server} channel={channel} />}
      </div>
    </>
  );
}

function RoomChat({ server, channel }: { server: ServerSummary; channel: Channel }) {
  return (
    <section className="room-chat" aria-label={`Chat in ${channel.name}`}>
      <ChannelMessages server={server} channel={channel} />
    </section>
  );
}

/** A voice room's chat panel as the Floating right card, found by id (it may outlive the room view). */
export function RoomChatPanel({ serverId, channelId }: { serverId: ServerId; channelId: ChannelId }) {
  const server = useStore((s) => s.servers.find((x) => x.id === serverId));
  const channel = useStore((s) => s.channels[serverId]?.find((c) => c.id === channelId));
  if (!server || !channel) return null;
  return <RoomChat server={server} channel={channel} />;
}
