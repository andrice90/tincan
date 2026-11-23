// A file attached to a message: a card with size, holders and progress, or an
// inline image once the file is on this PC.

import { File, FileArchive, FileImage, FileMusic, FilePlay, FileText } from "lucide-react";
import { memo, useCallback, useEffect, useState } from "react";
import type { Attachment as AttachmentT, Transfer } from "../api/types";
import { formatBytes } from "../lib/format";
import { downloadFile, ensureTransfer, fileUrl, openFile } from "../store/actions";
import { useTransferState } from "../store/store";

function FileIcon({ mime }: { mime: string }) {
  const size = 24;
  if (mime.startsWith("image/")) return <FileImage size={size} />;
  if (mime.startsWith("video/")) return <FilePlay size={size} />;
  if (mime.startsWith("audio/")) return <FileMusic size={size} />;
  if (mime.startsWith("text/") || mime === "application/pdf" || mime.includes("document")) return <FileText size={size} />;
  if (mime.includes("zip") || mime.includes("compressed") || mime.includes("tar")) return <FileArchive size={size} />;
  return <File size={size} />;
}

function holders(peers: number): string {
  if (peers === 0) return "No one online has it";
  if (peers === 1) return "1 person online has it";
  return `${peers} people online have it`;
}

function Meta({ a, t }: { a: AttachmentT; t: Transfer | undefined }) {
  if (!t) return <span className="file-meta">{formatBytes(a.size)}</span>;
  switch (t.state) {
    case "downloading":
      return (
        <span className="file-meta">
          {formatBytes(t.have)} of {formatBytes(t.total || a.size)}
        </span>
      );
    case "queued":
      return (
        <span className="file-meta">
          {formatBytes(a.size)}, {t.peers === 0 ? "waiting for someone who has it" : "queued"}
        </span>
      );
    case "failed":
      return <span className="file-meta is-error">Download failed. {holders(t.peers)}</span>;
    case "unavailable":
      return (
        <span className="file-meta">
          {formatBytes(a.size)}, {holders(0).toLowerCase()}
        </span>
      );
    case "complete":
      return (
        <span className="file-meta">
          {formatBytes(a.size)}, {t.peers === 0 ? "only you have it" : holders(t.peers).toLowerCase()}
        </span>
      );
    case "idle":
      return (
        <span className="file-meta">
          {formatBytes(a.size)}, {holders(t.peers).toLowerCase()}
        </span>
      );
  }
}

function Action({ a, t }: { a: AttachmentT; t: Transfer | undefined }) {
  const state = t?.state ?? "idle";
  if (state === "complete") {
    return (
      <button type="button" className="btn btn-sm" onClick={() => openFile(a.hash, a.name)}>
        Open
      </button>
    );
  }
  if (state === "downloading") {
    const pct = t && t.total > 0 ? Math.floor((t.have / t.total) * 100) : 0;
    return <span className="file-pct">{pct}%</span>;
  }
  if (state === "queued") {
    return (
      <button type="button" className="btn btn-sm" disabled>
        Queued
      </button>
    );
  }
  return (
    <button type="button" className="btn btn-sm" onClick={() => downloadFile(a.hash)}>
      {state === "failed" ? "Retry" : "Download"}
    </button>
  );
}

function ImagePreview({ a, onFail }: { a: AttachmentT; onFail: () => void }) {
  const [url, setUrl] = useState<string | null>(null);
  const [loaded, setLoaded] = useState(false);
  useEffect(() => {
    let live = true;
    fileUrl(a.hash, a.name).then(
      (u) => {
        if (!live) return;
        if (u) setUrl(u);
        else onFail();
      },
      () => live && onFail(),
    );
    return () => {
      live = false;
    };
  }, [a.hash, a.name, onFail]);
  if (!url) return <div className="image-preview is-loading" />;
  return (
    <button
      type="button"
      className={loaded ? "image-preview" : "image-preview is-loading"}
      title={a.name}
      onClick={() => openFile(a.hash, a.name)}
    >
      {/* Off-screen images in a long history are not read from the app until scrolled near;
          the placeholder keeps its size until the image has decoded. */}
      <img src={url} alt={a.name} loading="lazy" decoding="async" onLoad={() => setLoaded(true)} onError={onFail} />
    </button>
  );
}

/** One attachment of a message. */
export const Attachment = memo(function Attachment({ a }: { a: AttachmentT }) {
  const t = useTransferState(a.hash);
  const [imageFailed, setImageFailed] = useState(false);
  const onImageFail = useCallback(() => setImageFailed(true), []);
  useEffect(() => ensureTransfer(a.hash), [a.hash]);

  const isImage = a.mime.startsWith("image/");
  if (isImage && t?.state === "complete" && !imageFailed) {
    return <ImagePreview a={a} onFail={onImageFail} />;
  }

  const pct = t && t.state === "downloading" && t.total > 0 ? (t.have / t.total) * 100 : null;
  return (
    <div className="file-card">
      <span className="file-icon">
        <FileIcon mime={a.mime} />
      </span>
      <div className="file-info">
        <span className="file-name truncate" title={a.name}>
          <bdi>{a.name}</bdi>
        </span>
        <Meta a={a} t={t} />
        {pct !== null && (
          <span
            className="file-progress"
            role="progressbar"
            aria-label={`Downloading ${a.name}`}
            aria-valuemin={0}
            aria-valuemax={100}
            aria-valuenow={Math.floor(pct)}
          >
            <span style={{ width: `${pct}%` }} />
          </span>
        )}
      </div>
      <Action a={a} t={t} />
    </div>
  );
});
