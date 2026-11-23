// Voice room view and the shared call stage: participant tiles, and while
// someone shares a screen, that share on the stage with tiles in a strip.

import { HeadphoneOff, Maximize2, MessageSquare, MicOff, Minimize2, Monitor, Volume2 } from "lucide-react";
import { memo, useEffect, useRef, useState, type MouseEvent } from "react";
import type { Channel, Member, ServerSummary, UserId, VoiceParticipant, VoiceSession } from "../api/types";
import { ScreenView } from "../media/ScreenView";
import { joinVoice, messageUser, setFullStage, setUserVolume, stopShare, toggleRoomChat } from "../store/actions";
import { initialVolume, shouldSend, type TilePerson } from "../store/calls";
import { useStore } from "../store/store";
import { ChannelMessages } from "./ChatView";
import { ICON, ICON_SM, IconButton, PersonId, PersonName, SpeakingAvatar } from "./common";
import { Popover, useMenu } from "./Menu";

/** Per-user playback volume picked in this session (0..2); the app's saved value seeds it. */
const volumes = new Map<UserId, number>();

function VolumeSlider({ p, start }: { p: TilePerson; start: number }) {
  const [v, setV] = useState(start);
  // The value last sent (the one shown at open until then): a release or key that changed
  // nothing sends nothing.
  const sent = useRef(start);
  const id = `vol-${p.userId}`;
  const commit = () => {
    if (!shouldSend(sent.current, v)) return;
    sent.current = v;
    setUserVolume(p.userId, v);
  };
  return (
    <>
      <label className="field-label" htmlFor={id}>
        Volume for <bdi>{p.name}</bdi>
      </label>
      <div className="volume-row">
        <input
          id={id}
          type="range"
          min={0}
          max={200}
          step={5}
          value={Math.round(v * 100)}
          onChange={(e) => {
            const next = Number(e.target.value) / 100;
            setV(next);
            volumes.set(p.userId, next);
          }}
          onPointerUp={commit}
          onKeyUp={commit}
        />
        <span className="volume-value">{Math.round(v * 100)}%</span>
      </div>
    </>
  );
}

function VolumePopover({ p, anchor, onClose }: { p: TilePerson; anchor: NonNullable<ReturnType<typeof useMenu>["anchor"]>; onClose: () => void }) {
  const start = initialVolume(volumes.get(p.userId), p);
  return (
    <Popover anchor={anchor} onClose={onClose} label={`${p.name} options`} className="volume-pop">
      <PersonId userId={p.userId} />
      {start !== null && <VolumeSlider p={p} start={start} />}
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
            screenTrack: null,
            volume: 1,
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
      <div className="tiles">
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
            <div className="tiles">
              {inside.map((m) => (
                <Tile
                  key={m.userId}
                  p={{ userId: m.userId, name: m.name, muted: m.muted, deafened: m.deafened, speaking: m.speaking, screenTrack: null, volume: null }}
                  isMe={false}
                  sharing={m.sharing}
                />
              ))}
            </div>
          )}
        </div>
        {chatOpen && (
          <section className="room-chat" aria-label={`Chat in ${channel.name}`}>
            <ChannelMessages server={server} channel={channel} />
          </section>
        )}
      </div>
    </>
  );
}
