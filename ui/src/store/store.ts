// A small external store read through useSyncExternalStore. Components select
// the slice they need; the state is replaced immutably, so a component only
// re-renders when its own slice changes. High-frequency values (speaking flags,
// transfer progress, playing clips) live in keyed stores so one update wakes
// only the components that watch that key.

import { useSyncExternalStore } from "react";
import type {
  CallInfo,
  Channel,
  ChannelId,
  ChannelKind,
  Clip,
  ConvId,
  DmSummary,
  Me,
  Member,
  Message,
  ServerId,
  ServerSummary,
  Settings,
  Transfer,
  UserId,
  BlobHash,
  VoiceSession,
} from "../api/types";
import { buildAvatarIndex } from "../lib/avatars";
import { browserStorage, loadLayout, type Layout } from "../lib/layout";
import { buildNameIndex, sharesName, type NameIndex } from "../lib/names";

/** What the main area shows. */
export type View =
  | { kind: "home" }
  | { kind: "server"; serverId: ServerId; channelId: ChannelId | null }
  | { kind: "dms"; convId: ConvId | null };

/** Settings dialog tabs. */
export type SettingsTab = "profile" | "voice" | "files" | "network" | "appearance";

/** The open modal dialog, if any. */
export type DialogState =
  | { kind: "create-server" }
  | { kind: "join-server" }
  | { kind: "invite"; serverId: ServerId }
  | { kind: "rename-server"; serverId: ServerId }
  | { kind: "leave-server"; serverId: ServerId }
  | { kind: "kick-member"; serverId: ServerId; userId: UserId; name: string }
  | { kind: "create-channel"; serverId: ServerId; channelKind: ChannelKind }
  | { kind: "rename-channel"; serverId: ServerId; channelId: ChannelId }
  | { kind: "delete-channel"; serverId: ServerId; channelId: ChannelId }
  | { kind: "delete-message"; message: Message }
  | { kind: "settings"; tab: SettingsTab }
  | { kind: "screen-picker" }
  | { kind: "add-contact" }
  | { kind: "add-sound"; path: string }
  | { kind: "rename-sound"; clip: Clip }
  /** Crop a picked image for my avatar or a server's icon; `back` reopens after it closes. */
  | { kind: "crop-image"; file: File; target: CropTarget; back: DialogState | null };

/** What a cropped image becomes. */
export type CropTarget = { kind: "avatar" } | { kind: "server-icon"; serverId: ServerId };

/** Messages loaded for one conversation (server channel or DM). */
export interface ConvMessages {
  /** Oldest first, including deleted ones (the UI skips those). */
  list: Message[];
  /** Cursor for the next older page; null once the start is loaded. */
  cursor: string | null;
  loaded: boolean;
  loadingOlder: boolean;
}

/** Someone typing in a channel. */
export interface TypingEntry {
  userId: UserId;
  name: string;
  until: number;
}

/** A transient notice. */
export interface Toast {
  id: number;
  kind: "info" | "error";
  text: string;
}

/** The whole UI state. */
export interface State {
  ready: boolean;
  /** Set when the first snapshot failed to load. */
  fatal: string | null;
  me: Me | null;
  version: string;
  servers: ServerSummary[];
  dms: DmSummary[];
  settings: Settings | null;
  voice: VoiceSession | null;
  call: CallInfo | null;
  channels: Record<ServerId, Channel[]>;
  members: Record<ServerId, Member[]>;
  messages: Record<string, ConvMessages>;
  typing: Record<string, TypingEntry[]>;
  clips: Clip[];
  view: View;
  lastChannel: Record<ServerId, ChannelId>;
  lastDm: ConvId | null;
  showMembers: boolean;
  soundboardOpen: boolean;
  /** The chat panel beside a voice room's tiles is open. */
  roomChatPanel: boolean;
  /** Voice room whose stage is shown in full window, by channel or conv id. */
  fullStage: boolean;
  /** Classic or Floating window layout, saved per profile (lib/layout.ts). */
  layout: Layout;
  dialog: DialogState | null;
  toasts: Toast[];
}

const initial: State = {
  ready: false,
  fatal: null,
  me: null,
  version: "",
  servers: [],
  dms: [],
  settings: null,
  voice: null,
  call: null,
  channels: {},
  members: {},
  messages: {},
  typing: {},
  clips: [],
  view: { kind: "home" },
  lastChannel: {},
  lastDm: null,
  showMembers: true,
  soundboardOpen: false,
  roomChatPanel: false,
  fullStage: false,
  layout: loadLayout(browserStorage()),
  dialog: null,
  toasts: [],
};

let state: State = initial;
const listeners = new Set<() => void>();

function subscribe(cb: () => void): () => void {
  listeners.add(cb);
  return () => {
    listeners.delete(cb);
  };
}

/** Current state, for actions and event handlers. */
export function getState(): State {
  return state;
}

/** Replaces the state with `fn(state)` and notifies subscribers if it changed. */
export function update(fn: (s: State) => State): void {
  const next = fn(state);
  if (next === state) return;
  state = next;
  for (const l of listeners) l();
}

/** Shallow-merges `patch` into the state. */
export function patch(p: Partial<State>): void {
  update((s) => ({ ...s, ...p }));
}

/**
 * Reads a slice of the state. The selector must return a stable value
 * (a slice of the state or a primitive), never a freshly built object.
 */
export function useStore<T>(selector: (s: State) => T): T {
  return useSyncExternalStore(
    subscribe,
    () => selector(state),
    () => selector(state),
  );
}

let nameMemo: { members: State["members"]; dms: State["dms"]; me: State["me"]; index: NameIndex } | null = null;

/**
 * Display names of everyone the UI knows (all server members, DM contacts, me), rebuilt when
 * those lists change.
 */
function nameIndex(s: State): NameIndex {
  if (nameMemo && nameMemo.members === s.members && nameMemo.dms === s.dms && nameMemo.me === s.me) return nameMemo.index;
  const people: { userId: UserId; name: string }[] = [];
  for (const list of Object.values(s.members)) people.push(...list);
  people.push(...s.dms);
  if (s.me) people.push(s.me);
  nameMemo = { members: s.members, dms: s.dms, me: s.me, index: buildNameIndex(people) };
  return nameMemo.index;
}

/** True when someone else the UI knows uses the same display name as this person. */
export function useSharesName(userId: UserId, name: string): boolean {
  return useStore((s) => sharesName(nameIndex(s), userId, name));
}

let avatarMemo: { members: State["members"]; dms: State["dms"]; me: State["me"]; voice: State["voice"]; index: Map<UserId, string> } | null =
  null;

/**
 * Avatar URL per user from every list that carries one (server members, DMs, voice
 * participants, me). An image only reaches a list once the app holds and checked it.
 */
export function avatarIndex(s: Pick<State, "members" | "dms" | "me" | "voice">): Map<UserId, string> {
  if (avatarMemo && avatarMemo.members === s.members && avatarMemo.dms === s.dms && avatarMemo.me === s.me && avatarMemo.voice === s.voice) {
    return avatarMemo.index;
  }
  const index = buildAvatarIndex(s.members, s.dms, s.voice?.participants ?? [], s.me);
  avatarMemo = { members: s.members, dms: s.dms, me: s.me, voice: s.voice, index };
  return index;
}

/** The avatar URL of a user, or null to show the coloured initial. */
export function useAvatarUrl(userId: UserId): string | null {
  return useStore((s) => avatarIndex(s).get(userId) ?? null);
}

/** Key for the messages and typing maps. */
export function convKey(convId: ConvId, channelId: ChannelId | null): string {
  return `${convId}:${channelId ?? ""}`;
}

// ------------------------------------------------------------------ keyed stores

/** A map whose entries can be watched one key at a time. */
export class KeyedStore<V> {
  private values = new Map<string, V>();
  private watchers = new Map<string, Set<() => void>>();

  /** Current value for `key`. */
  get(key: string): V | undefined {
    return this.values.get(key);
  }

  /** Sets `key` and wakes its watchers when the value changed. */
  set(key: string, value: V): void {
    if (this.values.get(key) === value) return;
    this.values.set(key, value);
    const w = this.watchers.get(key);
    if (w) for (const cb of w) cb();
  }

  /** Every key and value, for a resync. */
  entries(): IterableIterator<[string, V]> {
    return this.values.entries();
  }

  /** Removes `key` and wakes its watchers. */
  delete(key: string): void {
    if (!this.values.has(key)) return;
    this.values.delete(key);
    const w = this.watchers.get(key);
    if (w) for (const cb of w) cb();
  }

  /** Watches one key. */
  watch(key: string, cb: () => void): () => void {
    let w = this.watchers.get(key);
    if (!w) {
      w = new Set();
      this.watchers.set(key, w);
    }
    w.add(cb);
    return () => {
      w.delete(cb);
      if (w.size === 0) this.watchers.delete(key);
    };
  }
}

/** Speaking flags by user id. */
export const speakingStore = new KeyedStore<boolean>();
/** Transfer state by blob hash. */
export const transferStore = new KeyedStore<Transfer>();
/** Clips currently playing, by hash: the time playback ends. */
export const playingStore = new KeyedStore<number>();

function useKeyed<V>(store: KeyedStore<V>, key: string): V | undefined {
  return useSyncExternalStore(
    (cb) => store.watch(key, cb),
    () => store.get(key),
    () => store.get(key),
  );
}

/** True while `userId` is speaking. */
export function useSpeaking(userId: UserId): boolean {
  return useKeyed(speakingStore, userId) ?? false;
}

/** The latest transfer state for a blob, if known. */
export function useTransferState(hash: BlobHash): Transfer | undefined {
  return useKeyed(transferStore, hash);
}

/** True while the clip is playing. */
export function usePlaying(hash: BlobHash): boolean {
  return useKeyed(playingStore, hash) !== undefined;
}
