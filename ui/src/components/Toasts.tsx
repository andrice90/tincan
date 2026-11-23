// Transient notices from "toast" events and failed actions.

import { X } from "lucide-react";
import { dismissToast } from "../store/actions";
import { useStore } from "../store/store";

export function Toasts() {
  const toasts = useStore((s) => s.toasts);
  return (
    <div className="toasts" aria-live="polite">
      {toasts.map((t) => (
        <div key={t.id} className={t.kind === "error" ? "toast is-error" : "toast"} role={t.kind === "error" ? "alert" : "status"}>
          <span>{t.text}</span>
          <button type="button" className="icon-btn icon-btn-sm" aria-label="Dismiss" title="Dismiss" onClick={() => dismissToast(t.id)}>
            <X size={14} />
          </button>
        </div>
      ))}
    </div>
  );
}
