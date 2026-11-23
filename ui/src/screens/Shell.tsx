// Main window layout: rail, sidebar (channel or DM list, call panel, user
// panel), content, optional member list.

import { CallPanel } from "../components/CallPanel";
import { ChannelSidebar } from "../components/ChannelSidebar";
import { ChannelChat, DmChat } from "../components/ChatView";
import { DmSidebar } from "../components/DmSidebar";
import { MemberList } from "../components/MemberList";
import { ResizeHandle, usePanelWidth } from "../components/ResizeHandle";
import { ServerRail } from "../components/ServerRail";
import { Soundboard } from "../components/Soundboard";
import { UserPanel } from "../components/UserPanel";
import { VoiceRoomView } from "../components/VoiceView";
import { SIDEBAR_BOUNDS, SIDEBAR_KEY } from "../lib/panelWidth";
import { useStore, type State } from "../store/store";
import { Home } from "./Home";

function currentServer(s: State) {
  const v = s.view;
  return v.kind === "server" ? s.servers.find((x) => x.id === v.serverId) : undefined;
}

function currentDm(s: State) {
  const v = s.view;
  return v.kind === "dms" && v.convId ? s.dms.find((d) => d.convId === v.convId) : undefined;
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
  const showMembers = useStore((s) => s.showMembers);
  // The member list goes with a chat page: a text channel or a voice room's chat page.
  const chatPage = useStore((s) => {
    const v = s.view;
    if (v.kind !== "server") return false;
    const kind = s.channels[v.serverId]?.find((c) => c.id === v.channelId)?.kind;
    return kind === "text" || (kind === "voice" && v.chat === true);
  });
  const soundboardOpen = useStore((s) => s.soundboardOpen && s.voice !== null);
  const sideWidth = usePanelWidth(SIDEBAR_KEY, SIDEBAR_BOUNDS);

  return (
    <div className="shell">
      <ServerRail />
      <aside className="sidebar" style={{ width: sideWidth.width }}>
        {server ? <ChannelSidebar server={server} /> : <DmSidebar />}
        <div className="sidebar-foot">
          <CallPanel />
          <UserPanel />
        </div>
        <ResizeHandle panel={sideWidth} bounds={SIDEBAR_BOUNDS} edge="right" label="Resize sidebar" />
      </aside>
      <main className="main">
        <MainContent />
        {soundboardOpen && <Soundboard />}
      </main>
      {view.kind === "server" && server && showMembers && chatPage && <MemberList serverId={server.id} />}
    </div>
  );
}
