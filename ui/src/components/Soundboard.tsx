// Soundboard panel, opened from the call panel while in voice.

import { Download, Pencil, Square, Trash2, X } from "lucide-react";
import { memo, useEffect, useRef } from "react";
import { errorText, pickFiles } from "../api/client";
import type { Clip } from "../api/types";
import { formatSeconds } from "../lib/format";
import { closesOnEscape, closesOnPointerDown } from "../lib/outsideClose";
import { fetchClip, openDialog, playClip, pushToast, removeClip, setSoundboardOpen, stopClip } from "../store/actions";
import { isMyClipPlaying } from "../store/calls";
import { CLIP_EXTENSIONS } from "../store/clips";
import { usePlaying, useStore } from "../store/store";
import { ICON_SM, IconButton } from "./common";
import { Menu, useMenu } from "./Menu";

const OwnedTile = memo(function OwnedTile({ clip, disabled }: { clip: Clip; disabled: boolean }) {
  // Mine: the app reports it until it ends or I stop it. Someone else playing it lights the
  // tile too, with no stop.
  const mine = useStore((s) => isMyClipPlaying(s.voice, clip.hash));
  const playing = usePlaying(clip.hash) || mine;
  const menu = useMenu();
  return (
    <div className="clip-cell">
      <button
        type="button"
        className={playing ? "clip-tile is-playing" : "clip-tile"}
        disabled={disabled}
        aria-label={`Play ${clip.name}`}
        aria-pressed={playing}
        onClick={() => playClip(clip)}
        onContextMenu={menu.openAt}
      >
        <span className="clip-name truncate">
        <bdi>{clip.name}</bdi>
      </span>
        <span className="clip-meta">{formatSeconds(clip.durationMs)}</span>
      </button>
      {mine && (
        <IconButton small className="clip-stop" label={`Stop ${clip.name}`} onClick={() => stopClip(clip)}>
          <Square size={ICON_SM} fill="currentColor" />
        </IconButton>
      )}
      {menu.anchor && (
        <Menu
          anchor={menu.anchor}
          label={`${clip.name} menu`}
          onClose={menu.close}
          items={[
            { label: "Rename", icon: <Pencil size={ICON_SM} />, onSelect: () => openDialog({ kind: "rename-sound", clip }) },
            { label: "Remove", icon: <Trash2 size={ICON_SM} />, danger: true, onSelect: () => removeClip(clip.hash) },
          ]}
        />
      )}
    </div>
  );
});

const HeardTile = memo(function HeardTile({ clip, fromName }: { clip: Clip; fromName: string | null }) {
  const playing = usePlaying(clip.hash);
  const t = clip.transfer;
  const downloading = t?.state === "downloading" || t?.state === "queued";
  const pct = t && t.total > 0 ? Math.floor((t.have / t.total) * 100) : 0;
  return (
    <div className={playing ? "clip-tile is-heard is-playing" : "clip-tile is-heard"}>
      <span className="clip-name truncate">
        <bdi>{clip.name}</bdi>
      </span>
      <span className="clip-meta truncate">
        {fromName ? (
          <>
            From <bdi>{fromName}</bdi>
          </>
        ) : (
          formatSeconds(clip.durationMs)
        )}
      </span>
      {downloading ? (
        <span
          className="clip-progress"
          role="progressbar"
          aria-label={`Saving ${clip.name}`}
          aria-valuemin={0}
          aria-valuemax={100}
          aria-valuenow={pct}
        >
          <span style={{ width: `${pct}%` }} />
        </span>
      ) : (
        <IconButton small className="clip-save" label={`Save ${clip.name} to your sounds`} onClick={() => fetchClip(clip.hash)}>
          <Download size={ICON_SM} />
        </IconButton>
      )}
    </div>
  );
});

export function Soundboard() {
  const clips = useStore((s) => s.clips);
  const voice = useStore((s) => s.voice);
  const membersByServer = useStore((s) => s.members);
  const dms = useStore((s) => s.dms);
  const deafened = voice?.deafened ?? false;
  const ref = useRef<HTMLElement>(null);

  // Closes on Escape and on a click outside it (see lib/outsideClose.ts for what counts).
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && closesOnEscape(e, document.querySelector("dialog[open]") !== null)) setSoundboardOpen(false);
    };
    const onDown = (e: PointerEvent) => {
      if (closesOnPointerDown<Element>(e.target instanceof Element ? e.target : null, ref.current)) setSoundboardOpen(false);
    };
    window.addEventListener("keydown", onKey);
    document.addEventListener("pointerdown", onDown, true);
    return () => {
      window.removeEventListener("keydown", onKey);
      document.removeEventListener("pointerdown", onDown, true);
    };
  }, []);

  const owned = clips.filter((c) => c.owned);
  const heard = clips.filter((c) => !c.owned);

  const nameOf = (id: string | null): string | null => {
    if (!id) return null;
    for (const list of Object.values(membersByServer)) {
      const m = list.find((x) => x.userId === id);
      if (m) return m.name;
    }
    return dms.find((d) => d.userId === id)?.name ?? null;
  };

  const add = async () => {
    try {
      const [path] = await pickFiles({ multiple: false, filters: [{ name: "Sounds", extensions: CLIP_EXTENSIONS }] });
      if (path) openDialog({ kind: "add-sound", path });
    } catch (err) {
      pushToast("error", errorText(err));
    }
  };

  return (
    <section ref={ref} className="soundboard" aria-label="Soundboard">
      <div className="soundboard-head">
        <h2>Soundboard</h2>
        <IconButton small label="Close soundboard" onClick={() => setSoundboardOpen(false)}>
          <X size={ICON_SM} />
        </IconButton>
      </div>
      {deafened && <p className="soundboard-note">Undeafen to play sounds.</p>}
      <h3>Your sounds</h3>
      <div className="clip-grid">
        {owned.map((c) => (
          <OwnedTile key={c.hash} clip={c} disabled={deafened} />
        ))}
        <button type="button" className="clip-tile clip-add" onClick={() => void add()}>
          <span className="clip-name">Add sound</span>
        </button>
      </div>
      {heard.length > 0 && (
        <>
          <h3>Heard in this call</h3>
          <div className="clip-grid">
            {heard.map((c) => (
              <HeardTile key={c.hash} clip={c} fromName={nameOf(c.heardFrom)} />
            ))}
          </div>
        </>
      )}
    </section>
  );
}
