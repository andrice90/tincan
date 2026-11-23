// Main window layout: rail, sidebar (channel or DM list, call panel, user
// panel), content, optional member list.

import { CallPanel } from "../components/CallPanel";
import { ChannelSidebar } from "../components/ChannelSidebar";
import { ChannelChat, DmChat } from "../components/ChatView";
import { DmSidebar } from "../components/DmSidebar";
import { MemberList } from "../components/MemberList";
import { ServerRail } from "../components/ServerRail";
import { Soundboard } from "../components/Soundboard";
import { UserPanel } from "../components/UserPanel";
import { VoiceRoomView } from "../components/VoiceView";
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
  return channel.kind === "voice" ? (
    <VoiceRoomView key={channel.id} server={server} channel={channel} />
  ) : (
    <ChannelChat key={channel.id} server={server} channel={channel} />
  );
}

export function Shell() {
  const view = useStore((s) => s.view);
  const server = useStore(currentServer);
  const showMembers = useStore((s) => s.showMembers);
  const channelKind = useStore((s) => {
    const v = s.view;
    if (v.kind !== "server") return null;
    return s.channels[v.serverId]?.find((c) => c.id === v.channelId)?.kind ?? null;
  });
  const soundboardOpen = useStore((s) => s.soundboardOpen && s.voice !== null);

  return (
    <div className="shell">
      <ServerRail />
      <aside className="sidebar">
        {server ? <ChannelSidebar server={server} /> : <DmSidebar />}
        <div className="sidebar-foot">
          <CallPanel />
          <UserPanel />
        </div>
      </aside>
      <main className="main">
        <MainContent />
        {soundboardOpen && <Soundboard />}
      </main>
      {view.kind === "server" && server && showMembers && channelKind === "text" && <MemberList serverId={server.id} />}
    </div>
  );
}
