// Message cache rules, kept pure so they can be tested. The store caches pages per
// conversation and appends live events. Events can be dropped (a lagged event stream, or the
// app's bulk windows, which send no message events and end in a snapshot), so the rules are:
// - a page is a contiguous storage read and wins over nothing it doesn't cover;
// - delivery (pending -> false) and deletion only move one way;
// - a snapshot drops every cache except the conversation on screen, which is re-read over
//   the whole span it shows, so it ends with no holes and the same first row.

import type { Message, MessagePage } from "../api/types";
import type { ConvMessages } from "./store";

/** A conversation off screen whose cache reached this many messages is dropped, not grown. */
export const OFFSCREEN_MAX = 300;

/** The app's cap on `messages` page size (MAX_PAGE in tincan-app commands.rs). */
export const MAX_PAGE = 200;

/** `base` with delivery and deletion already seen on `other` kept. */
export function oneWay(base: Message, other: Message): Message {
  return { ...base, pending: base.pending && other.pending, deleted: base.deleted || other.deleted };
}

function bySentAt(a: Message, b: Message): number {
  return a.sentAt - b.sentAt;
}

/**
 * The newest page merged with what the cache holds. Page copies win, except that delivery
 * and deletion seen live are kept. A cached message the page doesn't have is kept when it is
 * within the page's span (it arrived after the read); one older than the page is outside what
 * the page shows and is dropped, unless the page reaches the start of the conversation.
 */
export function mergeNewest(page: MessagePage, live: Message[]): { list: Message[]; cursor: string | null } {
  const byId = new Map(live.map((m) => [m.id, m]));
  const list = page.messages.map((m) => {
    const l = byId.get(m.id);
    return l ? oneWay(m, l) : m;
  });
  const inPage = new Set(page.messages.map((m) => m.id));
  const start = page.messages[0]?.sentAt ?? -Infinity;
  const extra = live.filter((m) => !inPage.has(m.id) && (page.cursor === null || m.sentAt >= start));
  if (extra.length > 0) {
    list.push(...extra);
    list.sort(bySentAt);
  }
  return { list, cursor: page.cursor };
}

/**
 * A live message-added or message-updated applied to a conversation's cache:
 * the new entry, `undefined` for "nothing cached, nothing to do", or `null` for "drop the cache".
 * `loading`: the conversation's first page is in flight, so a message that arrives now is kept
 * for the merge instead of being lost. `shown`: the conversation is on screen.
 */
export function applyLive(c: ConvMessages | undefined, m: Message, opts: { loading: boolean; shown: boolean }): ConvMessages | null | undefined {
  if (!c) return opts.loading ? { list: [m], cursor: null, loaded: false, loadingOlder: false } : undefined;
  const i = c.list.findIndex((x) => x.id === m.id);
  if (i >= 0) {
    const list = c.list.slice();
    // A command result can arrive after the event that already moved it (a DM acked within
    // milliseconds), so never undo delivery or deletion.
    list[i] = oneWay(m, c.list[i]);
    return { ...c, list };
  }
  // off screen, a big cache is cheaper to reload on the next visit than to keep growing
  if (!opts.shown && c.loaded && c.list.length >= OFFSCREEN_MAX) return null;
  const list = [...c.list, m];
  // keep display order when a late event arrives out of order
  if (list.length > 1 && list[list.length - 2].sentAt > m.sentAt) list.sort(bySentAt);
  return { ...c, list };
}

/**
 * The kept cache of the conversation on screen after a snapshot, given a fresh read from the
 * newest message back to at least `coverId` (the oldest message the cache held when the
 * read started). When the read contains it, the cache keeps that first row and its cursor,
 * plus anything an older page put in front since; everything from it on comes from the read.
 * Otherwise the read replaces the cache.
 */
export function refreshSpan(c: ConvMessages, page: MessagePage, coverId: string | null): ConvMessages {
  const at = coverId === null ? -1 : page.messages.findIndex((m) => m.id === coverId);
  const ci = coverId === null ? -1 : c.list.findIndex((m) => m.id === coverId);
  if (at >= 0 && ci >= 0 && c.cursor !== null) {
    const fresh = mergeNewest({ messages: page.messages.slice(at), cursor: c.cursor }, c.list.slice(ci));
    return { list: [...c.list.slice(0, ci), ...fresh.list], cursor: c.cursor, loaded: true, loadingOlder: c.loadingOlder };
  }
  const merged = mergeNewest(page, c.list);
  return { list: merged.list, cursor: merged.cursor, loaded: true, loadingOlder: false };
}

/** True when the read contains `coverId` or reaches the start of the conversation. */
export function pageCovers(page: MessagePage, coverId: string | null): boolean {
  return coverId === null || page.cursor === null || page.messages.some((m) => m.id === coverId);
}

/**
 * Which read a refresh installs: the whole read when it reached the cached span (or the start
 * of the conversation), so the cache keeps its first row. A catch-up too long to reach it
 * replaces the cache anyway; then only the first read is installed, like a plain reload,
 * instead of up to a thousand rows the view would render at once.
 */
export function refreshRead(first: MessagePage, extended: MessagePage, coverId: string | null): MessagePage {
  return pageCovers(extended, coverId) ? extended : first;
}

/** How many messages to ask for when re-reading a cache of `cached` messages. */
export function refreshLimit(cached: number, pageSize: number): number {
  return Math.min(MAX_PAGE, Math.max(pageSize, cached + pageSize));
}

/**
 * An older page goes in front of the cache only if it was asked for this entry's current
 * cursor and no snapshot came in between; otherwise it could leave a hole.
 */
export function olderPageApplies(c: ConvMessages | undefined, before: string, sameGen: boolean): boolean {
  return c !== undefined && sameGen && c.cursor === before;
}

/** An older page put in front of the cache (ids already cached are skipped). */
export function prependOlder(c: ConvMessages, page: MessagePage): ConvMessages {
  const have = new Set(c.list.map((m) => m.id));
  const older = page.messages.filter((m) => !have.has(m.id));
  const list = [...older, ...c.list];
  // a live message older than the cached page sits in front of it; keep sent order
  if (older.length > 0 && c.list.length > 0 && older[older.length - 1].sentAt > c.list[0].sentAt) list.sort(bySentAt);
  return { ...c, list, cursor: page.cursor, loadingOlder: false };
}

/** The caches a snapshot keeps: only the conversation on screen. */
export function keepOnSnapshot(messages: Record<string, ConvMessages>, shown: string | null): Record<string, ConvMessages> {
  const c = shown === null ? undefined : messages[shown];
  return c && shown !== null ? { [shown]: c } : {};
}
