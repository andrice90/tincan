// Add and rename soundboard clips.

import { useState } from "react";
import type { Clip } from "../api/types";
import { Modal } from "../components/Modal";
import { baseName, stripExtension } from "../lib/format";
import { addClip, closeDialog, renameClip } from "../store/actions";
import { useSubmit } from "./useSubmit";

/** Clip limits enforced by the audio engine (docs/architecture.md, Soundboard). */
export const CLIP_LIMIT_TEXT = "Sounds can be up to 20 seconds and 5 MB.";

const CLIP_NAME_MAX = 32;

export function AddSoundDialog({ path }: { path: string }) {
  const [name, setName] = useState(() => stripExtension(baseName(path)).slice(0, CLIP_NAME_MAX));
  const s = useSubmit(() => addClip(path, name.trim()), closeDialog);
  return (
    <Modal
      title="Add sound"
      onClose={closeDialog}
      onSubmit={s.submit}
      footer={
        <>
          <button type="button" className="btn btn-ghost" onClick={closeDialog}>
            Cancel
          </button>
          <button type="submit" className="btn btn-primary" disabled={s.busy || !name.trim()}>
            Add sound
          </button>
        </>
      }
    >
      <div className="field">
        <span className="field-label">File</span>
        <span className="truncate muted" title={path}>
          {baseName(path)}
        </span>
      </div>
      <div className="field">
        <label className="field-label" htmlFor="sound-name">
          Name
        </label>
        <input
          id="sound-name"
          className="input"
          value={name}
          maxLength={CLIP_NAME_MAX}
          autoFocus
          onChange={(e) => {
            setName(e.target.value);
            s.setError(null);
          }}
        />
        <p className="field-note">{CLIP_LIMIT_TEXT}</p>
      </div>
      {s.error && (
        <p className="error-text dialog-error" role="alert">
          {s.error}
        </p>
      )}
    </Modal>
  );
}

export function RenameSoundDialog({ clip }: { clip: Clip }) {
  const [name, setName] = useState(clip.name);
  const s = useSubmit(() => renameClip(clip.hash, name.trim()), closeDialog);
  return (
    <Modal
      title="Rename sound"
      onClose={closeDialog}
      onSubmit={s.submit}
      footer={
        <>
          <button type="button" className="btn btn-ghost" onClick={closeDialog}>
            Cancel
          </button>
          <button type="submit" className="btn btn-primary" disabled={s.busy || !name.trim()}>
            Save
          </button>
        </>
      }
    >
      <div className="field">
        <label className="field-label" htmlFor="sound-rename">
          Name
        </label>
        <input
          id="sound-rename"
          className="input"
          value={name}
          maxLength={CLIP_NAME_MAX}
          autoFocus
          onChange={(e) => {
            setName(e.target.value);
            s.setError(null);
          }}
        />
      </div>
      {s.error && (
        <p className="error-text dialog-error" role="alert">
          {s.error}
        </p>
      )}
    </Modal>
  );
}
