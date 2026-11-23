// Direct message conversations, newest first.

import { UserPlus } from "lucide-react";
import { useMemo } from "react";
import { openDialog, openDms } from "../store/actions";
import { useStore } from "../store/store";
import { Avatar, ICON_SM, IconButton, PersonName } from "./common";

export function DmSidebar() {
  const dms = useStore((s) => s.dms);
  const view = useStore((s) => s.view);
  const selected = view.kind === "dms" ? view.convId : null;
  const sorted = useMemo(() => dms.slice().sort((a, b) => (b.lastMessageAt ?? 0) - (a.lastMessageAt ?? 0)), [dms]);

  return (
    <>
      <div className="side-header">
        <span className="side-title">Direct messages</span>
        <IconButton small label="Add contact" onClick={() => openDialog({ kind: "add-contact" })}>
          <UserPlus size={ICON_SM} />
        </IconButton>
      </div>
      <div className="side-scroll">
        {sorted.length === 0 ? (
          <p className="side-empty">No conversations yet. Message someone from a server's member list, or add a contact by their ID.</p>
        ) : (
          <ul className="dm-list">
            {sorted.map((d) => {
              const unread = d.unread && d.convId !== selected;
              return (
                <li key={d.convId}>
                  <button
                    type="button"
                    className={unread ? "dm-row is-unread" : "dm-row"}
                    aria-current={d.convId === selected ? "page" : undefined}
                    onClick={() => openDms(d.convId)}
                  >
                    <Avatar userId={d.userId} name={d.name} size={32} online={d.online} />
                    <span className="truncate">
                      <PersonName userId={d.userId} name={d.name} />
                    </span>
                    <span className="sr-only">
                      {d.online ? ", online" : ", offline"}
                      {unread ? ", unread messages" : ""}
                    </span>
                  </button>
                </li>
              );
            })}
          </ul>
        )}
      </div>
    </>
  );
}
