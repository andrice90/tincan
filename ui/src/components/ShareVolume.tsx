// Screen share audio volume: a slider per sharer (saved by the app per sharer), shown in the
// stage's volume button and the stage's right-click menu, and inside `PersonVolumes` (member
// list and tile menus) while that person shares.

import { Volume2, VolumeX } from "lucide-react";
import { useRef, useState, type MouseEvent } from "react";
import type { UserId } from "../api/types";
import { setShareVolume } from "../store/actions";
import { shouldSend } from "../store/calls";
import { useStore } from "../store/store";
import { ICON } from "./common";
import { Popover, useMenu, type MenuAnchor } from "./Menu";

/** Someone sharing their screen, as the share volume controls need them. */
export interface Sharer {
  userId: UserId;
  name: string;
  /** Their saved share volume (0..2); undefined when the app did not report one. */
  shareVolume?: number;
}

/** Share volume picked in this session (0..2); the app's saved value seeds it. */
const chosen = new Map<UserId, number>();

/** Where the slider starts: this session's choice, else the saved value, else 100%. */
function startOf(p: Sharer): number {
  return chosen.get(p.userId) ?? p.shareVolume ?? 1;
}

/** The share volume slider for one sharer. */
export function ShareVolumeSlider({ p }: { p: Sharer }) {
  const start = startOf(p);
  const [v, setV] = useState(start);
  const sent = useRef(start);
  const id = `share-vol-${p.userId}`;
  const commit = () => {
    if (!shouldSend(sent.current, v)) return;
    sent.current = v;
    setShareVolume(p.userId, v);
  };
  return (
    <>
      <label className="field-label" htmlFor={id}>
        Screen share volume
      </label>
      <div className="volume-row">
        <input
          id={id}
          type="range"
          min={0}
          max={200}
          step={5}
          value={Math.round(v * 100)}
          onChange={(e) => {
            const next = Number(e.target.value) / 100;
            setV(next);
            chosen.set(p.userId, next);
          }}
          onPointerUp={commit}
          onKeyUp={commit}
        />
        <span className="volume-value">{Math.round(v * 100)}%</span>
      </div>
    </>
  );
}

/** A popover with only the share volume. */
export function ShareVolumePopover({ p, anchor, onClose }: { p: Sharer; anchor: MenuAnchor; onClose: () => void }) {
  return (
    <Popover anchor={anchor} onClose={onClose} label={`${p.name}'s screen share volume`} className="volume-pop">
      <ShareVolumeSlider p={p} />
    </Popover>
  );
}

/**
 * The stage's share volume: a button over the picture, and the same slider on a right click
 * anywhere on the stage (`onContextMenu`). Deafened shows a crossed-out speaker.
 */
export function useStageVolume(p: Sharer | null) {
  const menu = useMenu();
  const deafened = useStore((s) => s.voice?.deafened ?? false);
  const label = deafened ? "Screen share volume (you're deafened)" : "Screen share volume";
  const button =
    p === null ? null : (
      <button
        type="button"
        className="icon-btn stage-volume-btn"
        aria-label={label}
        title={label}
        onClick={(e: MouseEvent<HTMLButtonElement>) => menu.openBelow(e.currentTarget, "end")}
      >
        {deafened ? <VolumeX size={ICON} /> : <Volume2 size={ICON} />}
      </button>
    );
  const popover = p !== null && menu.anchor ? <ShareVolumePopover p={p} anchor={menu.anchor} onClose={menu.close} /> : null;
  const onContextMenu = (e: MouseEvent) => {
    if (p !== null) menu.openAt(e);
  };
  return { button, popover, onContextMenu };
}
