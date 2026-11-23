// No server selected: create one or join with an invite.

import { openDialog } from "../store/actions";
import { useStore } from "../store/store";

export function Home() {
  const hasServers = useStore((s) => s.servers.length > 0);
  return (
    <div className="center-screen">
      <div className="center-card home-card">
        <h1>{hasServers ? "Add a server" : "You're not in any servers yet"}</h1>
        <div className="home-actions">
          <button type="button" className="btn btn-primary" onClick={() => openDialog({ kind: "create-server" })}>
            Create server
          </button>
          <button type="button" className="btn" onClick={() => openDialog({ kind: "join-server" })}>
            Join with invite
          </button>
        </div>
      </div>
    </div>
  );
}
