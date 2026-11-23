// Bottom of the sidebar: who I am and the settings button.

import { Settings } from "lucide-react";
import { openDialog } from "../store/actions";
import { useStore } from "../store/store";
import { ICON, IconButton, SpeakingAvatar } from "./common";

export function UserPanel() {
  const me = useStore((s) => s.me);
  if (!me) return null;
  return (
    <section className="user-panel" aria-label="You">
      <SpeakingAvatar userId={me.userId} name={me.name} size={32} />
      <span className="user-panel-name truncate">{me.name}</span>
      <IconButton label="Settings" onClick={() => openDialog({ kind: "settings", tab: "profile" })}>
        <Settings size={ICON} />
      </IconButton>
    </section>
  );
}
