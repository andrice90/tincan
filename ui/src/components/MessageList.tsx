// Scrollable message history: date dividers, author groups (same author within
// 5 minutes), hover actions, load older on scroll to top, stick to bottom.

import { Check, Copy, Trash2 } from "lucide-react";
import { memo, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { copyText } from "../api/client";
import type { ChannelId, ConvId, Message, UserId } from "../api/types";
import { dayStart, formatDay, formatFull, formatTime } from "../lib/format";
import { anchorStep } from "../lib/scroll";
import { loadOlder, openDialog } from "../store/actions";
import { convKey, useStore } from "../store/store";
import { Attachment } from "./Attachment";
import { Avatar, ICON_SM, PersonName } from "./common";
import { TypingLine } from "./Composer";

const GROUP_MS = 5 * 60_000;
const LOAD_OLDER_PX = 400;
const STICK_PX = 48;
const USER_SCROLL_MS = 600;

type Row =
  | { kind: "day"; key: string; label: string }
  | { kind: "msg"; key: string; m: Message; head: boolean; lastPending: boolean };

function buildRows(list: Message[]): Row[] {
  const rows: Row[] = [];
  let prev: Message | null = null;
  const now = Date.now();
  for (const m of list) {
    if (m.deleted) continue;
    const newDay = !prev || dayStart(prev.sentAt) !== dayStart(m.sentAt);
    if (newDay) rows.push({ kind: "day", key: `day-${dayStart(m.sentAt)}`, label: formatDay(m.sentAt, now) });
    const head = newDay || !prev || prev.authorId !== m.authorId || m.sentAt - prev.sentAt > GROUP_MS;
    rows.push({ kind: "msg", key: m.id, m, head, lastPending: m.pending });
    // one "waiting" line per run of pending messages, under the last one
    if (m.pending && prev?.pending && !head) {
      const before = rows[rows.length - 2];
      if (before?.kind === "msg") before.lastPending = false;
    }
    prev = m;
  }
  return rows;
}

function CopyButton({ text }: { text: string }) {
  const [done, setDone] = useState(false);
  useEffect(() => {
    if (!done) return;
    const t = setTimeout(() => setDone(false), 1500);
    return () => clearTimeout(t);
  }, [done]);
  return (
    <button
      type="button"
      className="icon-btn icon-btn-sm"
      aria-label={done ? "Copied" : "Copy text"}
      title={done ? "Copied" : "Copy text"}
      onClick={() => {
        void copyText(text).then((ok) => setDone(ok));
      }}
    >
      {done ? <Check size={ICON_SM} /> : <Copy size={ICON_SM} />}
    </button>
  );
}

interface ItemProps {
  m: Message;
  head: boolean;
  canDelete: boolean;
  pendingName: string | null;
}

const MessageItem = memo(function MessageItem({ m, head, canDelete, pendingName }: ItemProps) {
  const hasActions = m.text !== "" || canDelete;
  return (
    <article className={head ? "msg msg-head" : "msg"} aria-label={`${m.authorName}, ${formatTime(m.sentAt)}`}>
      {head ? (
        <Avatar userId={m.authorId} name={m.authorName} size={32} />
      ) : (
        <time className="msg-gutter-time" dateTime={new Date(m.sentAt).toISOString()} title={formatFull(m.sentAt)}>
          {formatTime(m.sentAt)}
        </time>
      )}
      <div className="msg-body">
        {head && (
          <div className="msg-meta">
            <span className="msg-author">
              <PersonName userId={m.authorId} name={m.authorName} />
            </span>
            <time className="msg-time" dateTime={new Date(m.sentAt).toISOString()} title={formatFull(m.sentAt)}>
              {formatTime(m.sentAt)}
            </time>
          </div>
        )}
        {m.text !== "" && <div className={m.pending ? "msg-text is-pending" : "msg-text"}>{m.text}</div>}
        {m.attachments.length > 0 && (
          <div className="msg-attachments">
            {m.attachments.map((a) => (
              <Attachment key={a.hash} a={a} />
            ))}
          </div>
        )}
        {m.pending && pendingName && <div className="msg-pending">Waiting for {pendingName} to come online</div>}
      </div>
      {hasActions && (
        <div className="msg-actions">
          {m.text !== "" && <CopyButton text={m.text} />}
          {canDelete && (
            <button
              type="button"
              className="icon-btn icon-btn-sm is-danger"
              aria-label="Delete message"
              title="Delete message"
              onClick={() => openDialog({ kind: "delete-message", message: m })}
            >
              <Trash2 size={ICON_SM} />
            </button>
          )}
        </div>
      )}
    </article>
  );
});

interface ListProps {
  convId: ConvId;
  channelId: ChannelId | null;
  meId: UserId;
  /** Server owners may delete anyone's message. */
  canModerate: boolean;
  /** DM partner, for pending labels. */
  pendingName: string | null;
  /** Shown at the start of the history. */
  startText: string;
}

const NO_TYPING: readonly { name: string }[] = [];

export function MessageList({ convId, channelId, meId, canModerate, pendingName, startText }: ListProps) {
  const key = convKey(convId, channelId);
  const conv = useStore((s) => s.messages[key]);
  const typing = useStore((s) => s.typing[key] ?? NO_TYPING);
  const scroller = useRef<HTMLDivElement>(null);
  const content = useRef<HTMLDivElement>(null);
  const stick = useRef(true);
  /** Last wheel, key or touch input on the list. */
  const intent = useRef(0);
  /** True while a pointer (scrollbar drag) is down on the list. */
  const dragging = useRef(false);
  /** Distance from the bottom captured before older messages are prepended. */
  const anchor = useRef<number | null>(null);

  const list = conv?.list;
  const rows = useMemo(() => (list ? buildRows(list) : []), [list]);
  const firstKey = rows.find((r) => r.kind === "msg")?.key;
  const lastRow = rows[rows.length - 1];
  const lastKey = lastRow?.key;
  const lastMine = lastRow?.kind === "msg" && lastRow.m.authorId === meId;
  const prevLast = useRef<string | undefined>(undefined);
  const prevFirst = useRef<string | undefined>(undefined);

  const requestOlder = () => {
    const el = scroller.current;
    if (!el || !conv || conv.cursor === null || conv.loadingOlder) return;
    anchor.current = el.scrollHeight - el.scrollTop;
    void loadOlder(convId, channelId).then(() => {
      // nothing was prepended (error or empty page): forget the anchor
      requestAnimationFrame(() => {
        anchor.current = null;
      });
    });
  };

  // Keep the reading position when older messages arrive, else follow the bottom.
  useLayoutEffect(() => {
    const el = scroller.current;
    if (!el) return;
    // my own new message always brings the view to the bottom
    if (lastKey !== prevLast.current && lastMine && prevLast.current !== undefined) stick.current = true;
    prevLast.current = lastKey;
    const step = anchorStep(anchor.current, firstKey, prevFirst.current);
    prevFirst.current = firstKey;
    if (step === "apply" && anchor.current !== null) {
      el.scrollTop = el.scrollHeight - anchor.current;
      anchor.current = null;
      return;
    }
    if (stick.current) el.scrollTop = el.scrollHeight;
    // rows changed below the older page still loading: keep its anchor exact
    if (step === "recapture") anchor.current = el.scrollHeight - el.scrollTop;
  }, [rows, firstKey, lastKey, lastMine]);

  // Images and cards change height after render; stay pinned to the bottom when we were there.
  useEffect(() => {
    const el = scroller.current;
    const inner = content.current;
    if (!el || !inner) return;
    const ro = new ResizeObserver(() => {
      if (!stick.current) return;
      el.scrollTop = el.scrollHeight;
      // an older page still loading (a short first page): keep its anchor at the new bottom,
      // or the growth would be lost once the anchor is applied
      if (anchor.current !== null) anchor.current = el.scrollHeight - el.scrollTop;
    });
    ro.observe(inner);
    // the viewport shrinks when the composer grows or a call stage appears
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  // A short first page: fetch more until the view can scroll.
  useEffect(() => {
    const el = scroller.current;
    if (!el || !conv?.loaded || conv.cursor === null || conv.loadingOlder) return;
    if (el.scrollHeight <= el.clientHeight + LOAD_OLDER_PX) requestOlder();
    // requestOlder reads the latest conv; re-run only when the page state changes.
  }, [conv?.loaded, conv?.cursor, conv?.loadingOlder]);

  const onScroll = () => {
    const el = scroller.current;
    if (!el) return;
    const top = el.scrollTop;
    const nearBottom = el.scrollHeight - top - el.clientHeight < STICK_PX;
    // Only the user unpins the view. Scroll events caused by layout (images decoding,
    // cards changing size) arrive late and would otherwise unpin a pinned view.
    const byUser = dragging.current || performance.now() - intent.current < USER_SCROLL_MS;
    if (byUser) stick.current = nearBottom;
    else if (nearBottom) stick.current = true;
    if (top < LOAD_OLDER_PX) requestOlder();
  };

  const markIntent = () => {
    intent.current = performance.now();
  };

  useEffect(() => {
    const up = () => {
      dragging.current = false;
    };
    window.addEventListener("pointerup", up);
    return () => window.removeEventListener("pointerup", up);
  }, []);

  return (
    <div
      className="msg-scroller"
      ref={scroller}
      onScroll={onScroll}
      onWheel={markIntent}
      onTouchMove={markIntent}
      onKeyDown={markIntent}
      onPointerDown={() => {
        dragging.current = true;
        markIntent();
      }}
    >
      <div className="msg-content" ref={content}>
        {conv?.loaded && conv.cursor === null && <p className="msg-start">{startText}</p>}
        {conv?.loadingOlder && <p className="msg-loading">Loading older messages</p>}
        {rows.map((r) =>
          r.kind === "day" ? (
            <div key={r.key} className="day-divider" role="separator" aria-label={r.label}>
              <span>{r.label}</span>
            </div>
          ) : (
            <MessageItem
              key={r.key}
              m={r.m}
              head={r.head}
              canDelete={r.m.authorId === meId || canModerate}
              pendingName={r.lastPending ? pendingName : null}
            />
          ),
        )}
        {/* Under the newest message; the content's ResizeObserver keeps a pinned view at the bottom. */}
        <TypingLine names={typing.map((t) => t.name)} />
      </div>
    </div>
  );
}
