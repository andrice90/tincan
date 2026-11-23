// Message input: Enter sends, Shift+Enter adds a line, files attach through the picker.

import { Paperclip, X } from "lucide-react";
import { useEffect, useLayoutEffect, useRef, useState, type KeyboardEvent, type ReactNode } from "react";
import { errorText, pickFiles } from "../api/client";
import type { ChannelId, ConvId } from "../api/types";
import { finishSend, type Draft } from "../lib/compose";
import { baseName } from "../lib/format";
import { pushToast, sendMessage, sendTyping } from "../store/actions";
import { convKey } from "../store/store";
import { ICON, IconButton } from "./common";

const MAX_HEIGHT = 200;

/** Unsent text per conversation, kept while switching channels. */
const drafts = new Map<string, string>();

interface Props {
  convId: ConvId;
  channelId: ChannelId | null;
  placeholder: string;
}

export function Composer({ convId, channelId, placeholder }: Props) {
  const key = convKey(convId, channelId);
  const [text, setText] = useState(() => drafts.get(key) ?? "");
  const [files, setFiles] = useState<string[]>([]);
  const [sending, setSending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const area = useRef<HTMLTextAreaElement>(null);
  /** What the box holds right now, for the end of a send. */
  const box = useRef<Draft>({ text, files });
  const busy = useRef(false);
  /** Enter was pressed during a send: send the box when it finishes. */
  const queued = useRef(false);
  const mounted = useRef(false);

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  const setBox = (d: Draft) => {
    box.current = d;
    setText(d.text);
    setFiles(d.files);
    if (d.text) drafts.set(key, d.text);
    else drafts.delete(key);
  };

  useLayoutEffect(() => {
    const el = area.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, MAX_HEIGHT)}px`;
  }, [text]);

  const change = (v: string) => {
    box.current = { ...box.current, text: v };
    setText(v);
    drafts.set(key, v);
    setError(null);
    if (v.trim()) sendTyping(convId, channelId);
  };

  // The box is emptied when the send starts, so typing during a slow send (a host ack, a
  // file import) goes into an empty box and is never wiped. A failed send puts its text back.
  const send = async (): Promise<void> => {
    const sent = box.current;
    if (!sent.text.trim() && sent.files.length === 0) return;
    if (busy.current) {
      queued.current = true;
      return;
    }
    busy.current = true;
    setSending(true);
    setError(null);
    setBox({ text: "", files: [] });
    let ok = false;
    try {
      await sendMessage(convId, channelId, sent.text.trim(), sent.files);
      ok = true;
    } catch (err) {
      queued.current = false;
      if (mounted.current) setError(errorText(err));
      else pushToast("error", errorText(err));
    }
    busy.current = false;
    if (!mounted.current) {
      // switched away during the send: keep a failed text as the draft
      if (!ok) drafts.set(key, finishSend({ text: drafts.get(key) ?? "", files: [] }, sent, false).text);
      return;
    }
    setBox(finishSend(box.current, sent, ok));
    setSending(false);
    area.current?.focus();
    if (queued.current) {
      queued.current = false;
      await send();
    }
  };

  const addFiles = (picked: string[]) => {
    const cur = box.current.files;
    setBox({ ...box.current, files: [...cur, ...picked.filter((p) => !cur.includes(p))] });
  };

  const removeFile = (p: string) => setBox({ ...box.current, files: box.current.files.filter((x) => x !== p) });

  const onKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
      e.preventDefault();
      void send();
    }
  };

  const attach = async () => {
    try {
      const picked = await pickFiles({ multiple: true });
      if (picked.length > 0) addFiles(picked);
    } catch (err) {
      setError(errorText(err));
    }
    area.current?.focus();
  };

  return (
    <div className="composer-wrap">
      {error && (
        <p className="composer-error" role="alert">
          {error}
        </p>
      )}
      <div className="composer">
        {files.length > 0 && (
          <ul className="composer-files" aria-label="Files to send">
            {files.map((p) => (
              <li key={p} className="composer-file">
                <span className="truncate" title={p}>
                  {baseName(p)}
                </span>
                <button
                  type="button"
                  className="icon-btn icon-btn-sm"
                  aria-label={`Remove ${baseName(p)}`}
                  title="Remove"
                  onClick={() => removeFile(p)}
                >
                  <X size={14} />
                </button>
              </li>
            ))}
          </ul>
        )}
        <div className="composer-row">
          <IconButton label="Attach files" onClick={() => void attach()} disabled={sending}>
            <Paperclip size={ICON} />
          </IconButton>
          <textarea
            ref={area}
            className="composer-input"
            rows={1}
            value={text}
            placeholder={placeholder}
            aria-label={placeholder}
            onChange={(e) => change(e.target.value)}
            onKeyDown={onKeyDown}
            maxLength={4000}
          />
        </div>
      </div>
    </div>
  );
}

/** "Jordan is typing" line under the composer. Space is reserved so the layout never jumps. */
export function TypingLine({ names }: { names: string[] }) {
  // Names are peer-chosen: each is isolated so its direction can't reorder the sentence.
  let text: ReactNode = "";
  if (names.length === 1)
    text = (
      <>
        <bdi>{names[0]}</bdi> is typing
      </>
    );
  else if (names.length === 2)
    text = (
      <>
        <bdi>{names[0]}</bdi> and <bdi>{names[1]}</bdi> are typing
      </>
    );
  else if (names.length > 2) text = "Several people are typing";
  return (
    <div className="typing-line" aria-live="polite">
      {text}
    </div>
  );
}
