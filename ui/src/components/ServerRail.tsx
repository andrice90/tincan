// Left rail: direct messages, servers, add a server.

import { MessagesSquare, Plus } from "lucide-react";
import { memo } from "react";
import type { ServerSummary } from "../api/types";
import { initials } from "../lib/format";
import { openDms, openHome, selectServer } from "../store/actions";
import { useStore } from "../store/store";

const ServerButton = memo(function ServerButton({ server, selected }: { server: ServerSummary; selected: boolean }) {
  return (
    <button
      type="button"
      className="rail-btn"
      aria-current={selected ? "page" : undefined}
      aria-label={server.unread ? `${server.name}, unread messages` : server.name}
      title={server.name}
      onClick={() => selectServer(server.id)}
    >
      {initials(server.name)}
      {server.unread && !selected && <span className="rail-unread" />}
    </button>
  );
});

export function ServerRail() {
  const servers = useStore((s) => s.servers);
  const view = useStore((s) => s.view);
  const dmUnread = useStore((s) => s.dms.some((d) => d.unread));
  const selectedServer = view.kind === "server" ? view.serverId : null;

  return (
    <nav className="rail" aria-label="Servers">
      <button
        type="button"
        className="rail-btn"
        aria-current={view.kind === "dms" ? "page" : undefined}
        aria-label={dmUnread ? "Direct messages, unread messages" : "Direct messages"}
        title="Direct messages"
        onClick={() => openDms()}
      >
        <MessagesSquare size={20} />
        {dmUnread && view.kind !== "dms" && <span className="rail-unread" />}
      </button>
      <div className="rail-divider" />
      {servers.map((s) => (
        <ServerButton key={s.id} server={s} selected={s.id === selectedServer} />
      ))}
      <button
        type="button"
        className="rail-btn rail-add"
        aria-current={view.kind === "home" ? "page" : undefined}
        aria-label="Add a server"
        title="Add a server"
        onClick={openHome}
      >
        <Plus size={20} />
      </button>
    </nav>
  );
}
