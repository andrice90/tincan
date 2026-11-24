// Main window layout: rail, sidebar (channel or DM list, then the account card with the
// call panel and user panel), content with the composer slot under it, and the right slot
// (member list, or in Floating a voice room's chat). Classic draws these as flat panels;
// Floating (data-layout on <html>) draws each as a card. The slots stay mounted so their
// contents can animate closed (components/Presence.tsx); in Classic they close at once.

import type { CSSProperties } from "react";
import { CallPanel, useCallState } from "../components/CallPanel";
import { ChannelSidebar } from "../components/ChannelSidebar";
import { ChannelChat, DmChat, DockedComposer } from "../components/ChatView";
import { DmSidebar } from "../components/DmSidebar";
import { MemberList } from "../components/MemberList";
import { usePresence, type Shown } from "../components/Presence";
import { ResizeHandle, usePanelWidth } from "../components/ResizeHandle";
import { ServerRail } from "../components/ServerRail";
import { Soundboard } from "../components/Soundboard";
import { UserPanel } from "../components/UserPanel";
import { RoomChatPanel, VoiceRoomView } from "../components/VoiceView";
import { MEMBERS_BOUNDS, MEMBERS_KEY, SIDEBAR_BOUNDS, SIDEBAR_KEY } from "../lib/panelWidth";
import { composerSpot, rightPanel } from "../store/dock";
import { useStore, type State } from "../store/store";
import { Home } from "./Home";

/** The voice room chat as the Floating right card; Classic's panel beside the tiles is 340 px too. */
const ROOM_CHAT_WIDTH = 340;
/** What the 800 px minimum window leaves the room chat card next to the narrowest sidebar and content. */
const ROOM_CHAT_MIN = 256;

function currentServer(s: State) {
  const v = s.view;
  return v.kind === "server" ? s.servers.find((x) => x.id === v.serverId) : undefined;
}

function currentDm(s: State) {
  const v = s.view;
  return v.kind === "dms" && v.convId ? s.dms.find((d) => d.convId === v.convId) : undefined;
}

const sameSig = (a: { sig: string }, b: { sig: string }) => a.sig === b.sig;

/** `data-state` of a slot: open, closing (content kept for the transition), or closed. */
function slotState(p: Shown<unknown>): "open" | "closing" | undefined {
  if (p.open) return "open";
  return p.closing ? "closing" : undefined;
}

function MainContent() {
  const view = useStore((s) => s.view);
  const server = useStore(currentServer);
  const channels = useStore((s) => (s.view.kind === "server" ? s.channels[s.view.serverId] : undefined));
  const dm = useStore(currentDm);
  const hasDms = useStore((s) => s.dms.length > 0);

  if (view.kind === "home") return <Home />;
  if (view.kind === "dms") {
    if (dm) return <DmChat key={dm.convId} dm={dm} />;
    return (
      <div className="center-screen">
        <p className="muted">{hasDms ? "Pick a conversation." : "No direct messages yet."}</p>
      </div>
    );
  }
  if (!server) return null;
  const channel = channels?.find((c) => c.id === view.channelId);
  if (!channel) {
    return (
      <div className="center-screen">
        <p className="muted">{channels ? "This server has no channels yet." : "Loading channels"}</p>
      </div>
    );
  }
  return channel.kind === "voice" && !view.chat ? (
    <VoiceRoomView key={channel.id} server={server} channel={channel} />
  ) : (
    <ChannelChat key={channel.id} server={server} channel={channel} />
  );
}

export function Shell() {
  const view = useStore((s) => s.view);
  const server = useStore(currentServer);
  const servers = useStore((s) => s.servers);
  const channels = useStore((s) => s.channels);
  const dms = useStore((s) => s.dms);
  const showMembers = useStore((s) => s.showMembers);
  const roomChatPanel = useStore((s) => s.roomChatPanel);
  const layout = useStore((s) => s.layout);
  const soundboardOpen = useStore((s) => s.soundboardOpen && s.voice !== null);
  const sideWidth = usePanelWidth(SIDEBAR_KEY, SIDEBAR_BOUNDS);
  const membersWidth = usePanelWidth(MEMBERS_KEY, MEMBERS_BOUNDS);

  const composer = usePresence(composerSpot(view, servers, channels, dms), sameSig);
  const right = usePresence(rightPanel(view, channels, showMembers, roomChatPanel, layout), sameSig);
  const voice = usePresence(useCallState());

  const rightPanelNow = right.kept;
  const roomChat = rightPanelNow?.kind === "room-chat";
  const rightStyle = {
    "--right-w": `${roomChat ? ROOM_CHAT_WIDTH : membersWidth.width}px`,
    "--right-min": `${roomChat ? ROOM_CHAT_MIN : MEMBERS_BOUNDS.min}px`,
  } as CSSProperties;

  return (
    <div className="shell">
      <ServerRail />
      <aside className="sidebar" style={{ width: sideWidth.width }}>
        <div className="side-card">{server ? <ChannelSidebar server={server} /> : <DmSidebar />}</div>
        <div className="sidebar-foot">
          <div className="voice-slot" data-state={slotState(voice)}>
            <div className="voice-slot-inner" inert={voice.closing}>
              {voice.kept && <CallPanel voice={voice.kept.voice} call={voice.kept.call} />}
            </div>
          </div>
          <UserPanel />
        </div>
        <ResizeHandle panel={sideWidth} bounds={SIDEBAR_BOUNDS} edge="right" label="Resize sidebar" />
      </aside>
      <main className="main">
        <div className="content-card">
          <MainContent />
          {soundboardOpen && <Soundboard />}
        </div>
        <div className="composer-slot" data-state={slotState(composer)}>
          <div className="composer-slot-inner" inert={composer.closing}>
            {composer.kept && <DockedComposer spot={composer.kept} />}
          </div>
        </div>
      </main>
      <div className="right-slot" data-state={slotState(right)} style={rightStyle}>
        {right.open && rightPanelNow?.kind === "members" && (
          <ResizeHandle panel={membersWidth} bounds={MEMBERS_BOUNDS} edge="left" label="Resize member list" />
        )}
        <div className="right-card" inert={right.closing}>
          {rightPanelNow?.kind === "members" && <MemberList serverId={rightPanelNow.serverId} />}
          {rightPanelNow?.kind === "room-chat" && <RoomChatPanel serverId={rightPanelNow.serverId} channelId={rightPanelNow.channelId} />}
        </div>
      </div>
    </div>
  );
}
