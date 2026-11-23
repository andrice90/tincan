// Message input: Enter sends, Shift+Enter adds a line, files attach through the picker.

import { Paperclip, X } from "lucide-react";
import { useLayoutEffect, useRef, useState, type KeyboardEvent } from "react";
import { errorText, pickFiles } from "../api/client";
import type { ChannelId, ConvId } from "../api/types";
import { baseName } from "../lib/format";
import { sendMessage, sendTyping } from "../store/actions";
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

  useLayoutEffect(() => {
    const el = area.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, MAX_HEIGHT)}px`;
  }, [text]);

  const change = (v: string) => {
    setText(v);
    drafts.set(key, v);
    setError(null);
    if (v.trim()) sendTyping(convId, channelId);
  };

  const send = async () => {
    if (sending || (!text.trim() && files.length === 0)) return;
    setSending(true);
    setError(null);
    try {
      await sendMessage(convId, channelId, text.trim(), files);
      setText("");
      setFiles([]);
      drafts.delete(key);
    } catch (err) {
      setError(errorText(err));
    } finally {
      setSending(false);
      area.current?.focus();
    }
  };

  const onKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
      e.preventDefault();
      void send();
    }
  };

  const attach = async () => {
    try {
      const picked = await pickFiles({ multiple: true });
      if (picked.length > 0) setFiles((f) => [...f, ...picked.filter((p) => !f.includes(p))]);
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
                  onClick={() => setFiles((f) => f.filter((x) => x !== p))}
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
  let text = "";
  if (names.length === 1) text = `${names[0]} is typing`;
  else if (names.length === 2) text = `${names[0]} and ${names[1]} are typing`;
  else if (names.length > 2) text = "Several people are typing";
  return (
    <div className="typing-line" aria-live="polite">
      {text}
    </div>
  );
}
