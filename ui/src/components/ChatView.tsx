// A text channel or a DM: header, history, composer.

import { Hash, Phone, Users } from "lucide-react";
import type { Channel, DmSummary, ServerSummary } from "../api/types";
import { startCall, toggleMembers } from "../store/actions";
import { convKey, useStore, type TypingEntry } from "../store/store";
import { Avatar, ICON, IconButton } from "./common";
import { Composer, TypingLine } from "./Composer";
import { MessageList } from "./MessageList";
import { VoiceStage } from "./VoiceView";

const NO_TYPING: TypingEntry[] = [];

/** A server text channel. Mount with key = channel id so local state resets per channel. */
export function ChannelChat({ server, channel }: { server: ServerSummary; channel: Channel }) {
  const meId = useStore((s) => s.me?.userId ?? "");
  const showMembers = useStore((s) => s.showMembers);
  const typing = useStore((s) => s.typing[convKey(server.id, channel.id)] ?? NO_TYPING);

  return (
    <>
      <header className="main-header">
        <h1 className="main-title">
          <Hash size={ICON} aria-hidden="true" className="faint" />
          <span className="truncate">{channel.name}</span>
        </h1>
        <IconButton label={showMembers ? "Hide members" : "Show members"} pressed={showMembers} onClick={toggleMembers}>
          <Users size={ICON} />
        </IconButton>
      </header>
      <MessageList
        convId={server.id}
        channelId={channel.id}
        meId={meId}
        canModerate={server.isOwner}
        pendingName={null}
        startText={`Start of #${channel.name}`}
      />
      <Composer convId={server.id} channelId={channel.id} placeholder={`Message #${channel.name}`} />
      <TypingLine names={typing.map((t) => t.name)} />
    </>
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

  return (
    <>
      <header className="main-header">
        <h1 className="main-title">
          <Avatar userId={dm.userId} name={dm.name} size={24} online={dm.online} />
          <span className="truncate">{dm.name}</span>
          <span className="main-sub">{dm.online ? "Online" : "Offline"}</span>
        </h1>
        <IconButton
          label={`Call ${dm.name}`}
          tooltip={dm.online ? `Call ${dm.name}` : `${dm.name} is offline`}
          disabled={!dm.online || busy}
          onClick={() => startCall(dm.userId)}
        >
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
