// Everything that talks to the backend: event wiring and user actions.

import { errorText, invoke, listen } from "../api/client";
import type {
  AppSnapshot,
  BlobHash,
  Channel,
  ChannelId,
  ChannelKind,
  Clip,
  ConvId,
  Message,
  ServerId,
  ServerSummary,
  Settings,
  ShareMode,
  Transfer,
  UserId,
  VoiceSession,
} from "../api/types";
import { leaveCommand } from "./calls";
import { addPlayedClip } from "./clips";
import { withCreatedServer } from "./servers";
import { staleTransfers, takeTransfer } from "./transfers";
import { typingTracker } from "./typing";
import { MAX_PAGE, applyLive, keepOnSnapshot, mergeNewest, olderPageApplies, pageCovers, prependOlder, refreshLimit, refreshRead, refreshSpan } from "./messages";
import {
  convKey,
  getState,
  patch,
  playingStore,
  speakingStore,
  transferStore,
  update,
  type DialogState,
  type State,
  type View,
} from "./store";

/** Messages fetched per page. */
export const PAGE_SIZE = 30;
const TYPING_SHOW_MS = 6000;
const TYPING_SEND_EVERY_MS = 3000;

// ------------------------------------------------------------------ toasts

let toastSeq = 0;

/** Shows a transient notice. */
export function pushToast(kind: "info" | "error", text: string): void {
  const id = ++toastSeq;
  update((s) => ({ ...s, toasts: [...s.toasts.slice(-3), { id, kind, text }] }));
  setTimeout(() => dismissToast(id), kind === "error" ? 8000 : 5000);
}

/** Removes a notice. */
export function dismissToast(id: number): void {
  update((s) => (s.toasts.some((t) => t.id === id) ? { ...s, toasts: s.toasts.filter((t) => t.id !== id) } : s));
}

/** Runs a command for its side effect and shows a failure as a toast. */
function run(p: Promise<unknown>): void {
  p.catch((err: unknown) => pushToast("error", errorText(err)));
}

// ------------------------------------------------------------------ helpers

function upsert<T>(list: T[], item: T, same: (a: T) => boolean): T[] {
  const i = list.findIndex(same);
  if (i < 0) return [...list, item];
  const copy = list.slice();
  copy[i] = item;
  return copy;
}

function seedSpeaking(v: VoiceSession | null): void {
  if (!v) return;
  for (const p of v.participants) speakingStore.set(p.userId, p.speaking);
}

function firstTextChannel(channels: Channel[] | undefined): ChannelId | null {
  if (!channels) return null;
  const text = channels.filter((c) => c.kind === "text").sort((a, b) => a.position - b.position);
  return text[0]?.id ?? null;
}

/** Picks a valid view after servers or channels changed. */
function repairView(s: State): View {
  const v = s.view;
  if (v.kind !== "server") return v;
  if (!s.servers.some((x) => x.id === v.serverId)) {
    const next = s.servers[0];
    return next ? { kind: "server", serverId: next.id, channelId: s.lastChannel[next.id] ?? firstTextChannel(s.channels[next.id]) } : { kind: "home" };
  }
  const chans = s.channels[v.serverId];
  if (!chans) return v;
  if (v.channelId === null || !chans.some((c) => c.id === v.channelId)) {
    const id = firstTextChannel(chans);
    return id === v.channelId ? v : { ...v, channelId: id };
  }
  return v;
}

// ------------------------------------------------------------------ loading

const inflight = new Set<string>();

async function once(key: string, fn: () => Promise<void>): Promise<void> {
  if (inflight.has(key)) return;
  inflight.add(key);
  try {
    await fn();
  } finally {
    inflight.delete(key);
  }
}

/** Loads the channel and member lists of a server. */
export function loadServer(serverId: ServerId): void {
  run(
    once(`server:${serverId}`, async () => {
      const [channels, members] = await Promise.all([
        invoke("channels", { serverId }),
        invoke("members", { serverId }),
      ]);
      for (const m of members) speakingStore.set(m.userId, m.speaking);
      update((s) => {
        const next = {
          ...s,
          channels: { ...s.channels, [serverId]: channels },
          members: { ...s.members, [serverId]: members },
        };
        return { ...next, view: repairView(next) };
      });
      afterViewChange();
    }),
  );
}

/**
 * Generation of the message caches. A snapshot after the first one bumps it: pages read
 * before it may have holes (dropped events, bulk windows), so they are not merged after it.
 * It is part of the load keys, so a load from before a snapshot never blocks one after it.
 */
let cacheGen = 0;
const msgsKey = (key: string): string => `msgs:${cacheGen}:${key}`;

/** The conversation in the main area, if it is a text channel or a DM. */
function shownConv(s: State): { convId: ConvId; channelId: ChannelId | null; key: string } | null {
  const v = s.view;
  if (v.kind === "server" && v.channelId) {
    const ch = s.channels[v.serverId]?.find((c) => c.id === v.channelId);
    return ch?.kind === "text" ? { convId: v.serverId, channelId: ch.id, key: convKey(v.serverId, ch.id) } : null;
  }
  if (v.kind === "dms" && v.convId) return { convId: v.convId, channelId: null, key: convKey(v.convId, null) };
  return null;
}

function shownKey(s: State): string | null {
  return shownConv(s)?.key ?? null;
}

/** Loads the newest page of a conversation if it isn't loaded yet. */
export function ensureMessages(convId: ConvId, channelId: ChannelId | null): void {
  const key = convKey(convId, channelId);
  if (getState().messages[key]?.loaded) return;
  const gen = cacheGen;
  run(
    once(msgsKey(key), async () => {
      const page = await invoke("messages", { convId, channelId, before: null, limit: PAGE_SIZE });
      // a snapshot came in between: its own load runs under the new generation
      if (gen !== cacheGen) return;
      update((s) => {
        // messages that arrived while the page was in flight are merged, not lost
        const merged = mergeNewest(page, s.messages[key]?.list ?? []);
        return {
          ...s,
          messages: { ...s.messages, [key]: { list: merged.list, cursor: merged.cursor, loaded: true, loadingOlder: false } },
        };
      });
    }),
  );
}

/** Older pages read past the newest one when re-reading a long cache after a snapshot. */
const REFRESH_EXTRA_PAGES = 5;

/**
 * Re-reads the cached span of the conversation on screen after a snapshot, from the newest
 * message back to the oldest cached one, and merges it (refreshSpan). The view keeps its rows
 * and reading position instead of being dropped and reloaded at the bottom.
 */
function refreshMessages(convId: ConvId, channelId: ChannelId | null): void {
  const key = convKey(convId, channelId);
  const gen = cacheGen;
  let failed = false;
  run(
    once(msgsKey(key), async () => {
      const cached = getState().messages[key];
      if (!cached?.loaded) return;
      const coverId = cached.list[0]?.id ?? null;
      try {
        const first = await invoke("messages", { convId, channelId, before: null, limit: refreshLimit(cached.list.length, PAGE_SIZE) });
        let page = first;
        for (let i = 0; i < REFRESH_EXTRA_PAGES && page.cursor !== null && !pageCovers(page, coverId); i++) {
          if (gen !== cacheGen) return;
          const older = await invoke("messages", { convId, channelId, before: page.cursor, limit: MAX_PAGE });
          page = { messages: [...older.messages, ...page.messages], cursor: older.cursor };
        }
        if (gen !== cacheGen) return;
        const read = refreshRead(first, page, coverId);
        update((s) => {
          const c = s.messages[key];
          if (!c) return s;
          const next = refreshSpan(c, read, coverId);
          if (!next.loadingOlder) olderReq.delete(key);
          return { ...s, messages: { ...s.messages, [key]: next } };
        });
      } catch (err) {
        failed = true;
        throw err;
      }
    }).finally(() => {
      if (!failed || gen !== cacheGen) return;
      // the kept cache may have holes and could not be re-read: drop it like any other
      update((s) => {
        if (!s.messages[key]) return s;
        const messages = { ...s.messages };
        delete messages[key];
        return { ...s, messages };
      });
      if (shownKey(getState()) === key) ensureMessages(convId, channelId);
    }),
  );
}

/** The loadOlder call that set a conversation's loadingOlder flag, by cache key. */
const olderReq = new Map<string, number>();
let olderSeq = 0;

/** Loads the next older page. Resolves when done (or immediately if nothing to load). */
export async function loadOlder(convId: ConvId, channelId: ChannelId | null): Promise<void> {
  const key = convKey(convId, channelId);
  const conv = getState().messages[key];
  if (!conv || conv.cursor === null || conv.loadingOlder) return;
  const before = conv.cursor;
  const gen = cacheGen;
  const token = ++olderSeq;
  olderReq.set(key, token);
  update((s) => {
    const c = s.messages[key];
    return c ? { ...s, messages: { ...s.messages, [key]: { ...c, loadingOlder: true } } } : s;
  });
  /** Clears loadingOlder unless another call owns it now (after a snapshot replaced the entry). */
  const done = (s: State): State => {
    const c = s.messages[key];
    if (olderReq.get(key) !== token || !c?.loadingOlder) return s;
    return { ...s, messages: { ...s.messages, [key]: { ...c, loadingOlder: false } } };
  };
  try {
    const page = await invoke("messages", { convId, channelId, before, limit: PAGE_SIZE });
    update((s) => {
      const c = s.messages[key];
      // only in front of the entry it was asked for: a deep page landing on a reloaded
      // newest page, or one read before a snapshot, would leave a hole
      if (!c || !olderPageApplies(c, before, gen === cacheGen)) return done(s);
      return { ...s, messages: { ...s.messages, [key]: prependOlder(c, page) } };
    });
  } catch (err) {
    update(done);
    pushToast("error", errorText(err));
  } finally {
    if (olderReq.get(key) === token) olderReq.delete(key);
  }
}

function putMessage(m: Message): void {
  const key = convKey(m.convId, m.channelId);
  const loading = inflight.has(msgsKey(key));
  update((s) => {
    const typing = s.typing[key];
    const typingNext = typing?.some((t) => t.userId === m.authorId)
      ? { ...s.typing, [key]: typing.filter((t) => t.userId !== m.authorId) }
      : s.typing;
    const c = s.messages[key];
    const next = applyLive(c, m, { loading, shown: shownKey(s) === key });
    let messages = s.messages;
    if (next === null) {
      messages = { ...s.messages };
      delete messages[key];
    } else if (next !== undefined) {
      messages = { ...s.messages, [key]: next };
    }
    if (messages === s.messages && typingNext === s.typing) return s;
    return { ...s, typing: typingNext, messages };
  });
}

/** Marks the conversation shown in the main area as read if the backend says it is unread. */
function markCurrentRead(): void {
  const s = getState();
  const v = s.view;
  if (v.kind === "server" && v.channelId) {
    const ch = s.channels[v.serverId]?.find((c) => c.id === v.channelId);
    if (ch?.kind === "text" && ch.unread) {
      const serverId = v.serverId;
      const channelId = v.channelId;
      update((st) => ({
        ...st,
        channels: {
          ...st.channels,
          [serverId]: (st.channels[serverId] ?? []).map((c) => (c.id === channelId ? { ...c, unread: false } : c)),
        },
      }));
      run(invoke("mark_read", { convId: serverId, channelId }));
    }
  } else if (v.kind === "dms" && v.convId) {
    const convId = v.convId;
    const dm = s.dms.find((d) => d.convId === convId);
    if (dm?.unread) {
      update((st) => ({ ...st, dms: st.dms.map((d) => (d.convId === convId ? { ...d, unread: false } : d)) }));
      run(invoke("mark_read", { convId, channelId: null }));
    }
  }
}

/** At most one `mark_read` per conversation in this window while messages keep arriving. */
const READ_EVERY_MS = 1000;
const seenMarks = new Map<string, { last: number; timer: ReturnType<typeof setTimeout> | null }>();

/**
 * Someone else's message was put on screen: mark that conversation read right away
 * (leading edge, then once more at the end of a burst). Waiting for the backend's unread
 * flag instead would leave the conversation unread when the view changes within its debounce.
 */
function markSeen(m: Message): void {
  if (m.authorId === getState().me?.userId) return;
  const shown = (): boolean => {
    const v = getState().view;
    return v.kind === "server" ? v.serverId === m.convId && v.channelId === m.channelId : v.kind === "dms" && v.convId === m.convId;
  };
  if (!shown()) return;
  const key = convKey(m.convId, m.channelId);
  let mark = seenMarks.get(key);
  if (!mark) {
    mark = { last: 0, timer: null };
    seenMarks.set(key, mark);
  }
  if (mark.timer) return;
  const entry = mark;
  const fire = () => {
    entry.timer = null;
    // mark_read covers everything stored so far: skip it once the conversation left the
    // screen, or a message that arrived after that would count as read unseen
    if (!shown()) return;
    entry.last = Date.now();
    run(invoke("mark_read", { convId: m.convId, channelId: m.channelId }));
  };
  const wait = entry.last + READ_EVERY_MS - Date.now();
  if (wait <= 0) fire();
  else entry.timer = setTimeout(fire, wait);
}

/** Loads what the current view needs and marks it read. */
function afterViewChange(): void {
  const v = getState().view;
  if (v.kind === "server") {
    const ch = getState().channels[v.serverId]?.find((c) => c.id === v.channelId);
    if (ch?.kind === "text") ensureMessages(v.serverId, ch.id);
  } else if (v.kind === "dms" && v.convId) {
    ensureMessages(v.convId, null);
  }
  markCurrentRead();
}

function applySnapshot(snap: AppSnapshot): void {
  const wasReady = getState().ready;
  if (wasReady) cacheGen++;
  seedSpeaking(snap.voice);
  update((s) => {
    const next: State = {
      ...s,
      ready: true,
      fatal: null,
      me: snap.me,
      version: snap.version,
      servers: snap.servers,
      dms: snap.dms,
      settings: snap.settings,
      voice: snap.voice,
      call: snap.call,
    };
    if (!wasReady && snap.servers.length > 0 && s.view.kind === "home") {
      next.view = { kind: "server", serverId: snap.servers[0].id, channelId: null };
    }
    next.view = repairView(next);
    // A snapshot can follow dropped events (the shell sends one when its event stream lagged,
    // the app after a bulk window), so cached pages may have holes: drop them, except the
    // conversation on screen, which is re-read below so its reading position survives.
    if (wasReady) next.messages = keepOnSnapshot(s.messages, shownKey(next));
    return next;
  });
  if (wasReady) {
    const shown = shownConv(getState());
    if (shown && getState().messages[shown.key]?.loaded) refreshMessages(shown.convId, shown.channelId);
    resyncTransfers();
  }
  for (const srv of snap.servers) loadServer(srv.id);
  if (!wasReady && snap.me) {
    run(invoke("clips", {}).then((clips) => patch({ clips })));
  }
  afterViewChange();
}

// ------------------------------------------------------------------ init

const typing = typingTracker({
  now: Date.now,
  setTimer: (fn, ms) => setTimeout(fn, ms),
  get: (key) => getState().typing[key],
  set: (key, list) => update((s) => ({ ...s, typing: { ...s.typing, [key]: list } })),
});

function onTyping(e: { convId: ConvId; channelId: ChannelId | null; userId: UserId; name: string }): void {
  if (e.userId === getState().me?.userId) return;
  typing.typed(convKey(e.convId, e.channelId), { userId: e.userId, name: e.name, until: Date.now() + TYPING_SHOW_MS });
}

/** Wires backend events into the store and loads the first snapshot. Returns a cleanup. */
export function init(): () => void {
  const offs = [
    listen("snapshot", applySnapshot),
    listen("server-updated", (srv) => {
      update((s) => ({ ...s, servers: upsert(s.servers, srv, (x) => x.id === srv.id) }));
      if (srv.unread) markCurrentRead();
    }),
    listen("channels-updated", ({ serverId, channels }) => {
      update((s) => {
        const next = { ...s, channels: { ...s.channels, [serverId]: channels } };
        return { ...next, view: repairView(next) };
      });
      afterViewChange();
    }),
    listen("members-updated", ({ serverId, members }) => {
      for (const m of members) speakingStore.set(m.userId, m.speaking);
      update((s) => ({ ...s, members: { ...s.members, [serverId]: members } }));
    }),
    listen("message-added", (m) => {
      putMessage(m);
      markSeen(m);
    }),
    listen("message-updated", putMessage),
    listen("typing", onTyping),
    listen("dm-updated", (dm) => {
      update((s) => ({ ...s, dms: upsert(s.dms, dm, (x) => x.convId === dm.convId) }));
      if (dm.unread) markCurrentRead();
    }),
    listen("voice-updated", (voice) => {
      seedSpeaking(voice);
      update((s) => ({
        ...s,
        voice,
        soundboardOpen: voice ? s.soundboardOpen : false,
        fullStage: voice ? s.fullStage : false,
      }));
    }),
    listen("speaking", ({ userId, speaking }) => speakingStore.set(userId, speaking)),
    listen("call-updated", (call) => patch({ call })),
    listen("transfer-updated", (t) => transferStore.set(t.hash, t)),
    listen("clips-updated", (clips) => patch({ clips })),
    listen("sound-played", ({ clip }) => {
      markPlaying(clip.hash, clip.durationMs);
      update((s) => {
        const clips = addPlayedClip(s.clips, clip);
        return clips === s.clips ? s : { ...s, clips };
      });
    }),
    listen("toast", ({ kind, text }) => pushToast(kind, text)),
  ];
  invoke("app_snapshot", {})
    .then(applySnapshot)
    .catch((err: unknown) => patch({ ready: true, fatal: errorText(err) }));
  return () => {
    for (const off of offs) off();
  };
}

// ------------------------------------------------------------------ navigation

/** Opens a server at its last channel. */
export function selectServer(serverId: ServerId): void {
  const s = getState();
  const channelId = s.lastChannel[serverId] ?? firstTextChannel(s.channels[serverId]);
  patch({ view: { kind: "server", serverId, channelId } });
  if (!s.channels[serverId]) loadServer(serverId);
  afterViewChange();
}

/** Opens a channel. Opening a voice room joins it when you are not already talking. */
export function selectChannel(serverId: ServerId, channel: Channel): void {
  update((s) => ({
    ...s,
    view: { kind: "server", serverId, channelId: channel.id },
    lastChannel: { ...s.lastChannel, [serverId]: channel.id },
  }));
  afterViewChange();
  if (channel.kind === "voice") {
    const s = getState();
    if (!s.voice && !s.call) joinVoice(serverId, channel.id);
  }
}

/** Opens the DM list, at a conversation if given. */
export function openDms(convId: ConvId | null = getState().lastDm): void {
  const s = getState();
  const recent = s.dms.reduce<(typeof s.dms)[number] | null>(
    (best, d) => (!best || (d.lastMessageAt ?? 0) > (best.lastMessageAt ?? 0) ? d : best),
    null,
  );
  const valid = convId && s.dms.some((d) => d.convId === convId) ? convId : (recent?.convId ?? null);
  patch({ view: { kind: "dms", convId: valid }, lastDm: valid ?? s.lastDm });
  afterViewChange();
}

/** Shows the home screen (create or join a server). */
export function openHome(): void {
  patch({ view: { kind: "home" } });
}

/** Opens a DM with a user, creating it if needed. */
export function messageUser(userId: UserId): void {
  run(
    invoke("open_dm", { userId }).then((dm) => {
      update((s) => ({ ...s, dms: upsert(s.dms, dm, (x) => x.convId === dm.convId) }));
      openDms(dm.convId);
    }),
  );
}

/** Adds a contact from a pasted ID and opens the conversation. Rejects with error text. */
export async function addContact(code: string): Promise<void> {
  const dm = await invoke("add_contact", { code });
  update((s) => ({ ...s, dms: upsert(s.dms, dm, (x) => x.convId === dm.convId) }));
  openDms(dm.convId);
}

export function openDialog(dialog: DialogState): void {
  patch({ dialog });
}

export function closeDialog(): void {
  patch({ dialog: null });
}

export function toggleMembers(): void {
  update((s) => ({ ...s, showMembers: !s.showMembers }));
}

export function setSoundboardOpen(open: boolean): void {
  patch({ soundboardOpen: open });
}

export function setFullStage(on: boolean): void {
  patch({ fullStage: on });
}

// ------------------------------------------------------------------ profile and servers

/** Sets the display name. Rejects with error text. */
export async function setName(name: string): Promise<void> {
  const me = await invoke("set_name", { name });
  const first = getState().me === null;
  patch({ me });
  if (first) {
    run(invoke("clips", {}).then((clips) => patch({ clips })));
  }
}

function addServer(summary: ServerSummary): void {
  update((s) => ({ ...s, servers: withCreatedServer(s.servers, summary) }));
  loadServer(summary.id);
  selectServer(summary.id);
}

/** Creates a server and opens it. Rejects with error text. */
export async function createServer(name: string): Promise<void> {
  addServer(await invoke("create_server", { name }));
}

/** Joins a server from an invite and opens it. Rejects with error text. */
export async function joinServer(invite: string): Promise<void> {
  addServer(await invoke("join_server", { invite }));
}

/** Leaves a server. Rejects with error text. */
export async function leaveServer(serverId: ServerId): Promise<void> {
  await invoke("leave_server", { serverId });
  update((s) => {
    const next = { ...s, servers: s.servers.filter((x) => x.id !== serverId) };
    return { ...next, view: repairView(next) };
  });
  afterViewChange();
}

/** Removes a member (owner only). Rejects with error text. */
export async function kickMember(serverId: ServerId, userId: UserId): Promise<void> {
  await invoke("kick_member", { serverId, userId });
  update((s) => ({
    ...s,
    members: { ...s.members, [serverId]: (s.members[serverId] ?? []).filter((m) => m.userId !== userId) },
  }));
}

/** Renames a server. Rejects with error text. */
export async function renameServer(serverId: ServerId, name: string): Promise<void> {
  await invoke("rename_server", { serverId, name });
  update((s) => ({ ...s, servers: s.servers.map((x) => (x.id === serverId ? { ...x, name: name.trim() } : x)) }));
}

/** Creates a channel and opens it. Rejects with error text. */
export async function createChannel(serverId: ServerId, name: string, kind: ChannelKind): Promise<void> {
  const ch = await invoke("create_channel", { serverId, name, kind });
  update((s) => ({
    ...s,
    channels: { ...s.channels, [serverId]: upsert(s.channels[serverId] ?? [], ch, (x) => x.id === ch.id) },
  }));
  selectChannel(serverId, ch);
}

/** Renames a channel. Rejects with error text. */
export async function renameChannel(serverId: ServerId, channelId: ChannelId, name: string): Promise<void> {
  await invoke("rename_channel", { serverId, channelId, name });
  update((s) => ({
    ...s,
    channels: {
      ...s.channels,
      [serverId]: (s.channels[serverId] ?? []).map((c) => (c.id === channelId ? { ...c, name: name.trim() } : c)),
    },
  }));
}

/** Deletes a channel. Rejects with error text. */
export async function deleteChannel(serverId: ServerId, channelId: ChannelId): Promise<void> {
  await invoke("delete_channel", { serverId, channelId });
  update((s) => {
    const next = {
      ...s,
      channels: { ...s.channels, [serverId]: (s.channels[serverId] ?? []).filter((c) => c.id !== channelId) },
    };
    return { ...next, view: repairView(next) };
  });
  afterViewChange();
}

// ------------------------------------------------------------------ messages

/** Sends a message. Rejects with error text. */
export async function sendMessage(convId: ConvId, channelId: ChannelId | null, text: string, attachments: string[]): Promise<void> {
  const m = await invoke("send_message", { convId, channelId, text, attachments });
  putMessage(m);
}

/** Deletes a message. Rejects with error text. */
export async function deleteMessage(m: Message): Promise<void> {
  await invoke("delete_message", { convId: m.convId, messageId: m.id });
  putMessage({ ...m, deleted: true });
}

const lastTypingSent = new Map<string, number>();

/** Tells others you are typing, at most every few seconds. channelId null for DMs. */
export function sendTyping(convId: ConvId, channelId: ChannelId | null): void {
  const key = convKey(convId, channelId);
  const t = Date.now();
  if ((lastTypingSent.get(key) ?? 0) + TYPING_SEND_EVERY_MS > t) return;
  lastTypingSent.set(key, t);
  invoke("typing", { convId, channelId }).catch(() => {
    // typing is best effort
  });
}

// ------------------------------------------------------------------ files

/** Sets a command's transfer result unless a newer transfer-updated arrived while it ran. */
function setTransfer(hash: BlobHash, before: Transfer | undefined, t: Transfer): void {
  if (takeTransfer(transferStore.get(hash), before, t)) transferStore.set(hash, t);
}

/** Reads every unfinished transfer again: a lag snapshot may follow a dropped final event. */
function resyncTransfers(): void {
  for (const hash of staleTransfers(transferStore.entries())) {
    const before = transferStore.get(hash);
    invoke("transfer", { hash }).then(
      (t) => setTransfer(hash, before, t),
      () => {
        // best effort; the next event or a click on the card updates it
      },
    );
  }
}

/** Fetches the transfer state of a blob once, if nothing is known yet. */
export function ensureTransfer(hash: BlobHash): void {
  if (transferStore.get(hash)) return;
  run(
    once(`transfer:${hash}`, async () => {
      const t = await invoke("transfer", { hash });
      if (!transferStore.get(hash)) transferStore.set(hash, t);
    }),
  );
}

/** Starts downloading a file. */
export function downloadFile(hash: BlobHash): void {
  const before = transferStore.get(hash);
  run(invoke("download", { hash }).then((t) => setTransfer(hash, before, t)));
}

/** Opens a downloaded file with the shell. */
export function openFile(hash: BlobHash, name: string): void {
  run(invoke("open_file", { hash, name }));
}

const urlCache = new Map<BlobHash, string>();

/** A webview-loadable URL for a downloaded image, cached per hash. */
export async function fileUrl(hash: BlobHash, name: string): Promise<string | null> {
  const hit = urlCache.get(hash);
  if (hit) return hit;
  const url = await invoke("file_url", { hash, name });
  if (url) urlCache.set(hash, url);
  return url;
}

// ------------------------------------------------------------------ voice and calls

/** Joins a voice room. */
export function joinVoice(serverId: ServerId, channelId: ChannelId): void {
  run(
    invoke("voice_join", { serverId, channelId }).then((voice) => {
      seedSpeaking(voice);
      patch({ voice });
    }),
  );
}

/** Leaves the voice room or hangs up the call. A call ringing out from a room keeps ringing. */
export function leaveVoice(): void {
  const s = getState();
  const cmd = leaveCommand(s.voice, s.call);
  if (cmd === "voice_leave") {
    run(invoke("voice_leave", {}).then(() => patch({ voice: null, soundboardOpen: false, fullStage: false })));
  } else if (cmd === "call_hangup") {
    run(invoke("call_hangup", {}).then(() => patch({ voice: null, call: null, soundboardOpen: false, fullStage: false })));
  }
}

/** Cancels the call that is ringing out, without leaving a voice room. */
export function cancelCall(): void {
  run(invoke("call_hangup", {}).then(() => patch({ call: null })));
}

function patchVoice(p: Partial<VoiceSession>): void {
  update((s) => (s.voice ? { ...s, voice: { ...s.voice, ...p } } : s));
}

/** Mutes or unmutes the microphone. */
export function setMuted(muted: boolean): void {
  const prev = getState().voice;
  patchVoice(muted ? { muted } : { muted, deafened: false });
  invoke("set_muted", { muted }).catch((err: unknown) => {
    if (prev) patchVoice({ muted: prev.muted, deafened: prev.deafened });
    pushToast("error", errorText(err));
  });
}

/** Deafens (mutes playback and the mic) or undeafens. */
export function setDeafened(deafened: boolean): void {
  const prev = getState().voice;
  patchVoice(deafened ? { deafened, muted: true } : { deafened });
  invoke("set_deafened", { deafened }).catch((err: unknown) => {
    if (prev) patchVoice({ muted: prev.muted, deafened: prev.deafened });
    pushToast("error", errorText(err));
  });
}

/** Starts sharing a screen or window. Rejects with the raw error (callers map it). */
export async function startShare(sourceId: string, mode: ShareMode): Promise<void> {
  await invoke("screen_start", { sourceId, mode });
  patchVoice({ sharing: true });
}

/** Stops sharing. */
export function stopShare(): void {
  run(invoke("screen_stop", {}).then(() => patchVoice({ sharing: false })));
}

/**
 * Rings a user and opens the conversation with them: the call stage (their screen share,
 * per-user volume) lives there, also when the call was started from a member list.
 */
export function startCall(userId: UserId): void {
  run(
    invoke("open_dm", { userId })
      .then((dm) => {
        update((s) => ({ ...s, dms: upsert(s.dms, dm, (x) => x.convId === dm.convId) }));
        openDms(dm.convId);
        return invoke("call_start", { userId });
      })
      .then((call) => patch({ call })),
  );
}

let dmWatchChain: Promise<unknown> = Promise.resolve();

/**
 * Tells the app which DM is on screen (null when none), so it keeps the connection to that
 * contact up: their presence stays right and calling them works after an idle or a restart.
 * Calls run one after the other, so leaving one DM never overtakes opening the next.
 */
export function watchDm(convId: ConvId | null): void {
  dmWatchChain = dmWatchChain.then(() => invoke("dm_watch", { convId })).catch(() => undefined);
}

/** Accepts the incoming call. */
export function acceptCall(callId: string): void {
  run(
    invoke("call_accept", { callId }).then((voice) => {
      seedSpeaking(voice);
      const s = getState();
      patch({ voice, call: s.call ? { ...s.call, state: "active" } : s.call });
      const dm = s.dms.find((d) => d.convId === voice.convId);
      if (dm) openDms(dm.convId);
    }),
  );
}

/** Declines the incoming call. */
export function declineCall(callId: string): void {
  patch({ call: null });
  run(invoke("call_decline", { callId }));
}

/** Sets how loud one person is for me (0..2). */
export function setUserVolume(userId: UserId, volume: number): void {
  run(invoke("set_user_volume", { userId, volume }));
}

// ------------------------------------------------------------------ soundboard

const playTimers = new Map<BlobHash, ReturnType<typeof setTimeout>>();

function markPlaying(hash: BlobHash, durationMs: number): void {
  const end = Date.now() + durationMs;
  playingStore.set(hash, end);
  const prev = playTimers.get(hash);
  if (prev) clearTimeout(prev);
  playTimers.set(
    hash,
    setTimeout(() => {
      playTimers.delete(hash);
      playingStore.delete(hash);
    }, durationMs),
  );
}

/** Plays a clip into the call. */
export function playClip(clip: Clip): void {
  invoke("clip_play", { hash: clip.hash }).then(
    () => markPlaying(clip.hash, clip.durationMs),
    (err: unknown) => pushToast("error", errorText(err)),
  );
}

/** Pulls a heard clip into the library. */
export function fetchClip(hash: BlobHash): void {
  run(
    invoke("clip_fetch", { hash }).then((clip) =>
      update((s) => ({ ...s, clips: upsert(s.clips, clip, (c) => c.hash === clip.hash) })),
    ),
  );
}

/** Adds a sound from a file. Rejects with error text. */
export async function addClip(path: string, name: string): Promise<void> {
  const clip = await invoke("clip_add", { path, name });
  update((s) => ({ ...s, clips: upsert(s.clips, clip, (c) => c.hash === clip.hash) }));
}

/** Renames a sound. Rejects with error text. */
export async function renameClip(hash: BlobHash, name: string): Promise<void> {
  await invoke("clip_rename", { hash, name });
  update((s) => ({ ...s, clips: s.clips.map((c) => (c.hash === hash ? { ...c, name: name.trim() } : c)) }));
}

/** Removes a sound from the library. */
export function removeClip(hash: BlobHash): void {
  run(invoke("clip_remove", { hash }).then(() => update((s) => ({ ...s, clips: s.clips.filter((c) => c.hash !== hash) }))));
}

// ------------------------------------------------------------------ settings and window

/** Saves a settings change (optimistic). Rejects with error text after reverting. */
export async function saveSettings(change: Partial<Settings>): Promise<void> {
  const prev = getState().settings;
  if (!prev) return;
  const next = { ...prev, ...change };
  patch({ settings: next });
  try {
    const saved = await invoke("settings_set", { settings: next });
    patch({ settings: saved });
  } catch (err) {
    patch({ settings: prev });
    throw errorText(err);
  }
}

export function windowMinimize(): void {
  run(invoke("window_minimize", {}));
}

export function windowToggleMaximize(): void {
  run(invoke("window_toggle_maximize", {}));
}

export function windowClose(): void {
  run(invoke("window_close", {}));
}
