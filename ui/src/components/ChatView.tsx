// A text channel, a voice room's chat, or a DM: header, history, composer.

import { useEffect } from "react";
import { Hash, Phone, Users, Volume2 } from "lucide-react";
import type { Channel, DmSummary, ServerSummary } from "../api/types";
import { joinVoice, openDialog, startCall, toggleMembers, watchDm } from "../store/actions";
import { convKey, useStore, type TypingEntry } from "../store/store";
import { Avatar, ICON, IconButton, PersonName } from "./common";
import { Composer, TypingLine } from "./Composer";
import { MessageList } from "./MessageList";
import { VoiceStage } from "./VoiceView";

const NO_TYPING: TypingEntry[] = [];

/** History, composer and typing line of a server channel; a voice room's chat uses it too. */
export function ChannelMessages({ server, channel }: { server: ServerSummary; channel: Channel }) {
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
      {server.removed ? (
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
      <ChannelMessages server={server} channel={channel} />
    </>
  );
}

/** In place of the composer once the owner removed me: the history stays readable. */
function RemovedNote() {
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

/** A direct message conversation, with the call stage on top while in a call with them. Mount with key = conv id. */
export function DmChat({ dm }: { dm: DmSummary }) {
  const meId = useStore((s) => s.me?.userId ?? "");
  const voice = useStore((s) => s.voice);
  const call = useStore((s) => s.call);
  const inCall = voice?.kind === "call" && voice.convId === dm.convId;
  const busy = call !== null || voice?.kind === "call";
  const typing = useStore((s) => s.typing[convKey(dm.convId, null)] ?? NO_TYPING);

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
      <Composer convId={dm.convId} channelId={null} placeholder={`Message ${dm.name}`} />
      <TypingLine names={typing.map((t) => t.name)} />
    </>
  );
}
