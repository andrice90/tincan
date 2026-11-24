// A text channel, a voice room's chat, or a DM: header, history, composer.

import { useEffect } from "react";
import { Hash, Phone, Users, Volume2 } from "lucide-react";
import type { Channel, DmSummary, ServerSummary } from "../api/types";
import { joinVoice, openDialog, startCall, toggleMembers, watchDm } from "../store/actions";
import type { ComposerSpot } from "../store/dock";
import { convKey, useStore, type TypingEntry } from "../store/store";
import { Avatar, ICON, IconButton, PersonName } from "./common";
import { Composer, TypingLine } from "./Composer";
import { MessageList } from "./MessageList";
import { VoiceStage } from "./VoiceView";

const NO_TYPING: TypingEntry[] = [];

/**
 * History of a server channel, with its composer and typing line unless `composer` is
 * false (a channel page: the shell's composer slot has them). A voice room's chat panel
 * keeps its own.
 */
export function ChannelMessages({ server, channel, composer = true }: { server: ServerSummary; channel: Channel; composer?: boolean }) {
  const meId = useStore((s) => s.me?.userId ?? "");
  const typing = useStore((s) => s.typing[convKey(server.id, channel.id)] ?? NO_TYPING);
  const room = channel.kind === "voice";
  return (
    <>
      <MessageList
        convId={server.id}
        channelId={channel.id}
        meId={meId}
        canModerate={server.isOwner}
        pendingName={null}
        startText={room ? `Start of the chat in ${channel.name}` : `Start of #${channel.name}`}
      />
      {!composer ? null : server.removed ? (
        <RemovedNote />
      ) : (
        <>
          <Composer convId={server.id} channelId={channel.id} placeholder={room ? `Message ${channel.name}` : `Message #${channel.name}`} />
          <TypingLine names={typing.map((t) => t.name)} />
        </>
      )}
    </>
  );
}

/**
 * A server text channel, or a voice room's chat as the whole page (opened from the sidebar,
 * without joining voice). Mount with key = channel id so local state resets per channel.
 */
export function ChannelChat({ server, channel }: { server: ServerSummary; channel: Channel }) {
  const showMembers = useStore((s) => s.showMembers);
  const room = channel.kind === "voice";
  const joined = useStore((s) => s.voice?.kind === "room" && s.voice.channelId === channel.id);
  const inCall = useStore((s) => s.call !== null);

  return (
    <>
      <header className="main-header">
        <h1 className="main-title">
          {room ? <Volume2 size={ICON} aria-hidden="true" className="faint" /> : <Hash size={ICON} aria-hidden="true" className="faint" />}
          <span className="truncate">
            <bdi>{channel.name}</bdi>
          </span>
        </h1>
        <div className="main-actions">
          {room && !joined && !server.removed && (
            <button type="button" className="btn btn-primary btn-sm" disabled={inCall} onClick={() => joinVoice(server.id, channel.id)}>
              Join voice
            </button>
          )}
          <IconButton label={showMembers ? "Hide members" : "Show members"} pressed={showMembers} onClick={toggleMembers}>
            <Users size={ICON} />
          </IconButton>
        </div>
      </header>
      <ChannelMessages server={server} channel={channel} composer={false} />
    </>
  );
}

/** The composer slot's content: the composer and typing line of the conversation on screen. */
export function DockedComposer({ spot }: { spot: ComposerSpot }) {
  const typing = useStore((s) => s.typing[convKey(spot.convId, spot.channelId)] ?? NO_TYPING);
  if (spot.removed) return <RemovedNote />;
  return (
    <>
      <Composer key={convKey(spot.convId, spot.channelId)} convId={spot.convId} channelId={spot.channelId} placeholder={spot.placeholder} />
      <TypingLine names={typing.map((t) => t.name)} />
    </>
  );
}

/** In place of the composer once the owner removed me: the history stays readable. */
export function RemovedNote() {
  return (
    <div className="composer-wrap">
      <div className="removed-note" role="status">
        <span>You were removed from this server.</span>
        <button type="button" className="btn btn-ghost" onClick={() => openDialog({ kind: "join-server" })}>
          Join again
        </button>
      </div>
    </div>
  );
}

/**
 * A direct message conversation, with the call stage on top while in a call with them.
 * Its composer is in the shell's composer slot. Mount with key = conv id.
 */
export function DmChat({ dm }: { dm: DmSummary }) {
  const meId = useStore((s) => s.me?.userId ?? "");
  const voice = useStore((s) => s.voice);
  const call = useStore((s) => s.call);
  const inCall = voice?.kind === "call" && voice.convId === dm.convId;
  const busy = call !== null || voice?.kind === "call";

  // While this DM is on screen the app keeps the connection to them up.
  useEffect(() => {
    watchDm(dm.convId);
    return () => watchDm(null);
  }, [dm.convId]);

  return (
    <>
      <header className="main-header">
        <h1 className="main-title">
          <Avatar userId={dm.userId} name={dm.name} size={24} online={dm.online} />
          <span className="truncate">
            <PersonName userId={dm.userId} name={dm.name} />
          </span>
          <span className="main-sub">{dm.online ? "Online" : "Offline"}</span>
        </h1>
        <IconButton label={`Call ${dm.name}`} disabled={busy} onClick={() => startCall(dm.userId)}>
          <Phone size={ICON} />
        </IconButton>
      </header>
      {inCall && voice && (
        <div className="call-stage">
          <VoiceStage voice={voice} />
        </div>
      )}
      <MessageList
        convId={dm.convId}
        channelId={null}
        meId={meId}
        canModerate={false}
        pendingName={dm.name}
        startText={`Start of your conversation with ${dm.name}`}
      />
    </>
  );
}
