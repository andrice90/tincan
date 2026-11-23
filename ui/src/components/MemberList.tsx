// Right panel: who is in the server, online first.

import { MessageSquare, Phone } from "lucide-react";
import { memo, useMemo } from "react";
import type { Member, ServerId } from "../api/types";
import { messageUser, startCall } from "../store/actions";
import { useStore } from "../store/store";
import { ICON_SM, SpeakingAvatar } from "./common";
import { Menu, useMenu, type MenuItem } from "./Menu";

const EMPTY: Member[] = [];

const MemberRow = memo(function MemberRow({ m, isMe, canCall }: { m: Member; isMe: boolean; canCall: boolean }) {
  const menu = useMenu();
  const items: MenuItem[] = [
    { label: "Message", icon: <MessageSquare size={ICON_SM} />, onSelect: () => messageUser(m.userId) },
    { label: "Call", icon: <Phone size={ICON_SM} />, onSelect: () => startCall(m.userId), disabled: !m.online || !canCall },
  ];
  const content = (
    <>
      <SpeakingAvatar userId={m.userId} name={m.name} size={32} />
      <span className="member-name truncate">{isMe ? `${m.name} (you)` : m.name}</span>
      {m.isHost && <span className="member-tag">Host</span>}
      {m.isOwner && <span className="member-tag">Owner</span>}
    </>
  );
  return (
    <li>
      {isMe ? (
        <div className="member-row">{content}</div>
      ) : (
        <button
          type="button"
          className="member-row"
          aria-haspopup="menu"
          onClick={(e) => menu.openBelow(e.currentTarget, "end")}
          onContextMenu={menu.openAt}
        >
          {content}
        </button>
      )}
      {menu.anchor && <Menu anchor={menu.anchor} items={items} onClose={menu.close} label={`${m.name} menu`} />}
    </li>
  );
});

export function MemberList({ serverId }: { serverId: ServerId }) {
  const members = useStore((s) => s.members[serverId] ?? EMPTY);
  const meId = useStore((s) => s.me?.userId);
  const busy = useStore((s) => s.call !== null || s.voice?.kind === "call");
  const [online, offline] = useMemo(() => {
    const byName = (a: Member, b: Member) => a.name.localeCompare(b.name);
    return [members.filter((m) => m.online).sort(byName), members.filter((m) => !m.online).sort(byName)];
  }, [members]);

  return (
    <aside className="members" aria-label="Members">
      {online.length > 0 && (
        <section className="member-group">
          <h3>
            Online <span className="faint">{online.length}</span>
          </h3>
          <ul>
            {online.map((m) => (
              <MemberRow key={m.userId} m={m} isMe={m.userId === meId} canCall={!busy} />
            ))}
          </ul>
        </section>
      )}
      {offline.length > 0 && (
        <section className="member-group is-offline">
          <h3>
            Offline <span className="faint">{offline.length}</span>
          </h3>
          <ul>
            {offline.map((m) => (
              <MemberRow key={m.userId} m={m} isMe={m.userId === meId} canCall={!busy} />
            ))}
          </ul>
        </section>
      )}
    </aside>
  );
}
