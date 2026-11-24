// Pick a display or window to share, and how to encode it.

import { useEffect, useState } from "react";
import { errorCode, errorText, invoke } from "../api/client";
import type { ScreenSource, ShareMode } from "../api/types";
import { Modal } from "../components/Modal";
import { shareAudioOption } from "../lib/shareAudio";
import { closeDialog, startShare } from "../store/actions";

type Phase = { kind: "loading" } | { kind: "ready"; sources: ScreenSource[] } | { kind: "no-encoder" } | { kind: "error"; text: string };

function SourceList({
  title,
  sources,
  picked,
  onPick,
}: {
  title: string;
  sources: ScreenSource[];
  picked: string | null;
  onPick: (id: string) => void;
}) {
  if (sources.length === 0) return null;
  return (
    <section className="source-group" aria-label={title}>
      <h3>{title}</h3>
      <div className="source-grid" role="radiogroup" aria-label={title}>
        {sources.map((s) => (
          <button
            key={s.id}
            type="button"
            role="radio"
            aria-checked={picked === s.id}
            className="source-card"
            onClick={() => onPick(s.id)}
          >
            <span className="source-name truncate">{s.name}</span>
            <span className="source-size">
              {s.width} × {s.height}
            </span>
          </button>
        ))}
      </div>
    </section>
  );
}

export function ScreenPickerDialog() {
  const [phase, setPhase] = useState<Phase>({ kind: "loading" });
  const [picked, setPicked] = useState<string | null>(null);
  const [mode, setMode] = useState<ShareMode>("text");
  const [audio, setAudio] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    invoke("screen_sources", {}).then(
      (sources) => {
        if (!live) return;
        setPhase({ kind: "ready", sources });
        setPicked(sources.find((s) => s.kind === "display")?.id ?? sources[0]?.id ?? null);
      },
      (err: unknown) => {
        if (!live) return;
        setPhase(errorCode(err) === "no-hardware-encoder" ? { kind: "no-encoder" } : { kind: "error", text: errorText(err) });
      },
    );
    return () => {
      live = false;
    };
  }, []);

  const share = async () => {
    if (!picked || busy) return;
    setBusy(true);
    setError(null);
    try {
      await startShare(picked, mode, audio);
      closeDialog();
    } catch (err) {
      if (errorCode(err) === "no-hardware-encoder") setPhase({ kind: "no-encoder" });
      else setError(errorText(err));
    } finally {
      setBusy(false);
    }
  };

  if (phase.kind === "no-encoder") {
    return (
      <Modal
        title="Screen sharing isn't available"
        onClose={closeDialog}
        footer={
          <button type="button" className="btn btn-primary" onClick={closeDialog}>
            Close
          </button>
        }
      >
        <p className="muted">
          This PC has no hardware video encoder that Tincan can use. You can still watch other people's screens.
        </p>
      </Modal>
    );
  }

  const sources = phase.kind === "ready" ? phase.sources : [];
  const pickedKind = sources.find((s) => s.id === picked)?.kind ?? null;
  return (
    <Modal
      title="Share your screen"
      onClose={closeDialog}
      onSubmit={() => void share()}
      wide
      footer={
        <>
          <button type="button" className="btn btn-ghost" onClick={closeDialog}>
            Cancel
          </button>
          <button type="submit" className="btn btn-primary" disabled={!picked || busy}>
            Share
          </button>
        </>
      }
    >
      {phase.kind === "loading" && <p className="muted">Looking for screens and windows</p>}
      {phase.kind === "error" && (
        <p className="error-text" role="alert">
          {phase.text}
        </p>
      )}
      {phase.kind === "ready" && sources.length === 0 && <p className="muted">No screens or windows to share.</p>}
      <SourceList title="Displays" sources={sources.filter((s) => s.kind === "display")} picked={picked} onPick={setPicked} />
      <SourceList title="Windows" sources={sources.filter((s) => s.kind === "window")} picked={picked} onPick={setPicked} />
      {phase.kind === "ready" && sources.length > 0 && (
        <div className="field share-mode">
          <span className="field-label" id="mode-label">
            Optimize for
          </span>
          <div className="mode-options" role="radiogroup" aria-labelledby="mode-label">
            <button type="button" role="radio" aria-checked={mode === "text"} className="mode-option" onClick={() => setMode("text")}>
              <span className="mode-name">Text</span>
              <span className="mode-note">Sharper, up to 15 fps</span>
            </button>
            <button type="button" role="radio" aria-checked={mode === "motion"} className="mode-option" onClick={() => setMode("motion")}>
              <span className="mode-name">Motion</span>
              <span className="mode-note">Smoother, up to 30 fps</span>
            </button>
          </div>
        </div>
      )}
      {phase.kind === "ready" && sources.length > 0 && (
        <label className="switch-row share-audio" htmlFor="share-audio">
          <span className="share-audio-text">
            <span>Share audio</span>
            <span className="mode-note">{shareAudioOption(pickedKind)}</span>
          </span>
          <input
            id="share-audio"
            type="checkbox"
            role="switch"
            className="switch"
            checked={audio}
            onChange={(e) => setAudio(e.target.checked)}
          />
        </label>
      )}
      {error && (
        <p className="error-text dialog-error" role="alert">
          {error}
        </p>
      )}
    </Modal>
  );
}
