// Docked above the user panel while in a voice room or a call.

import { AudioLines, HeadphoneOff, Headphones, Mic, MicOff, MonitorOff, MonitorUp, PhoneOff } from "lucide-react";
import type { PathKind } from "../api/types";
import { cancelCall, leaveVoice, openDialog, setDeafened, setMuted, setSoundboardOpen, stopShare } from "../store/actions";
import { callPanelParts } from "../store/calls";
import { useStore } from "../store/store";
import { ICON, IconButton } from "./common";

function pathLabel(path: PathKind): { text: string; cls: string } {
  switch (path) {
    case "direct":
      return { text: "Direct", cls: "conn-direct" };
    case "relay":
      return { text: "Relayed", cls: "conn-relay" };
    case "none":
      return { text: "Connecting", cls: "conn-none" };
  }
}

export function CallPanel() {
  const voice = useStore((s) => s.voice);
  const call = useStore((s) => s.call);
  const soundboardOpen = useStore((s) => s.soundboardOpen);
  const servers = useStore((s) => s.servers);
  const parts = callPanelParts(voice, call);

  // A call ringing out gets its own row, also above a room panel: cancelling it never
  // needs the room's Leave button, and leaving the room never cancels it.
  const calling = parts.calling && call && (
    <section className="call-panel" aria-label="Call">
      <div className="call-panel-top">
        <div className="call-panel-label">
          <span className="call-state">Calling</span>
          <span className="truncate muted">
            <bdi>{call.name}</bdi>
          </span>
        </div>
        <IconButton label="Cancel call" tone="danger" onClick={cancelCall}>
          <PhoneOff size={ICON} />
        </IconButton>
      </div>
    </section>
  );
  if (parts.callNoAudio && call) {
    // accepted, but no voice session is attached: still offer a way to end it
    return (
      <section className="call-panel" aria-label="Call">
        <div className="call-panel-top">
          <div className="call-panel-label">
            <span className="call-state conn-none">Connecting</span>
            <span className="truncate muted">
              Call with <bdi>{call.name}</bdi>
            </span>
          </div>
          <IconButton label="Hang up" tone="danger" onClick={leaveVoice}>
            <PhoneOff size={ICON} />
          </IconButton>
        </div>
      </section>
    );
  }
  if (!voice) return calling || null;

  const label = pathLabel(voice.path);
  const rtt = voice.rttMs !== null ? `Round trip ${voice.rttMs} ms` : undefined;
  // Room, server and contact names are peer-chosen: each is isolated from the text around it.
  const place =
    voice.kind === "room" ? (
      <>
        <bdi>{voice.name}</bdi> / <bdi>{servers.find((s) => s.id === voice.convId)?.name ?? ""}</bdi>
      </>
    ) : (
      <>
        Call with <bdi>{voice.name}</bdi>
      </>
    );
  const leaveLabel = voice.kind === "room" ? "Leave voice" : "Hang up";

  return (
    <>
      {calling}
      <section className="call-panel" aria-label="Voice">
        <div className="call-panel-top">
          <div className="call-panel-label">
            <span className={`call-state ${label.cls}`} title={rtt}>
              {label.text}
              {rtt && <span className="sr-only">, {rtt}</span>}
            </span>
            <span className="truncate muted">{place}</span>
          </div>
          <IconButton label={leaveLabel} tone="danger" onClick={leaveVoice}>
            <PhoneOff size={ICON} />
          </IconButton>
        </div>
        <div className="call-panel-buttons">
          <IconButton
            label={voice.muted ? "Unmute" : "Mute"}
            pressed={voice.muted}
            tone={voice.muted ? "off" : undefined}
            onClick={() => setMuted(!voice.muted)}
          >
            {voice.muted ? <MicOff size={ICON} /> : <Mic size={ICON} />}
          </IconButton>
          <IconButton
            label={voice.deafened ? "Undeafen" : "Deafen"}
            pressed={voice.deafened}
            tone={voice.deafened ? "off" : undefined}
            onClick={() => setDeafened(!voice.deafened)}
          >
            {voice.deafened ? <HeadphoneOff size={ICON} /> : <Headphones size={ICON} />}
          </IconButton>
          <IconButton
            label={voice.sharing ? "Stop sharing" : "Share screen"}
            pressed={voice.sharing}
            onClick={() => (voice.sharing ? stopShare() : openDialog({ kind: "screen-picker" }))}
          >
            {voice.sharing ? <MonitorOff size={ICON} /> : <MonitorUp size={ICON} />}
          </IconButton>
          <IconButton label="Soundboard" pressed={soundboardOpen} onClick={() => setSoundboardOpen(!soundboardOpen)}>
            <AudioLines size={ICON} />
          </IconButton>
        </div>
      </section>
    </>
  );
}
