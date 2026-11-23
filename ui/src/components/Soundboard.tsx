// Soundboard panel, opened from the call panel while in voice.

import { Download, Pencil, Trash2, X } from "lucide-react";
import { memo, useEffect } from "react";
import { errorText, pickFiles } from "../api/client";
import type { Clip } from "../api/types";
import { formatSeconds } from "../lib/format";
import { fetchClip, openDialog, playClip, pushToast, removeClip, setSoundboardOpen } from "../store/actions";
import { usePlaying, useStore } from "../store/store";
import { ICON_SM, IconButton } from "./common";
import { Menu, useMenu } from "./Menu";

/** Audio files the clip picker offers. */
export const CLIP_EXTENSIONS = ["mp3", "wav", "ogg", "flac", "m4a", "opus"];

const OwnedTile = memo(function OwnedTile({ clip, disabled }: { clip: Clip; disabled: boolean }) {
  const playing = usePlaying(clip.hash);
  const menu = useMenu();
  return (
    <>
      <button
        type="button"
        className={playing ? "clip-tile is-playing" : "clip-tile"}
        disabled={disabled}
        aria-label={`Play ${clip.name}`}
        aria-pressed={playing}
        onClick={() => playClip(clip)}
        onContextMenu={menu.openAt}
      >
        <span className="clip-name truncate">{clip.name}</span>
        <span className="clip-meta">{formatSeconds(clip.durationMs)}</span>
      </button>
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
    </>
  );
});

const HeardTile = memo(function HeardTile({ clip, fromName }: { clip: Clip; fromName: string | null }) {
  const playing = usePlaying(clip.hash);
  const t = clip.transfer;
  const downloading = t?.state === "downloading" || t?.state === "queued";
  const pct = t && t.total > 0 ? Math.floor((t.have / t.total) * 100) : 0;
  return (
    <div className={playing ? "clip-tile is-heard is-playing" : "clip-tile is-heard"}>
      <span className="clip-name truncate">{clip.name}</span>
      <span className="clip-meta truncate">{fromName ? `From ${fromName}` : formatSeconds(clip.durationMs)}</span>
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

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setSoundboardOpen(false);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
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
    <section className="soundboard" aria-label="Soundboard">
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
