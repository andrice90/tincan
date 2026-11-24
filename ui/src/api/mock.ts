// In-browser fake backend. Implements every command in `Commands` against
// believable sample data and emits the same events as the Rust app, so the UI
// is fully clickable with `pnpm dev` in a plain browser.
//
// URL flags: `?fresh` starts at the name picker with no servers, `?noenc`
// makes screen sharing fail with "no-hardware-encoder", `?ring` rings an
// incoming call shortly after start, `?noanswer` leaves outgoing calls ringing,
// `?removed` starts with Sam removed from Book club.
// `window.tincanMock` has helpers for the console: ring(), online(name),
// offline(name), migrate(), toast(text), state().

import type { Backend } from "./client";
import type {
  AppSnapshot,
  AudioDevices,
  CallInfo,
  Channel,
  BlobHash,
  ChannelId,
  ChannelKind,
  Clip,
  CommandName,
  Commands,
  ConvId,
  DmSummary,
  EventName,
  Events,
  Me,
  Member,
  Message,
  ScreenSource,
  ServerId,
  ServerSummary,
  Settings,
  Transfer,
  UserId,
  VoiceParticipant,
  VoiceSession,
} from "./types";
import { parseShortCode, randomShortCode } from "../lib/codes";
import { baseName, stripExtension } from "../lib/format";

// ------------------------------------------------------------------ ids

const Z32 = "ybndrfg8ejkmcpqxot1uwisza345h769";
const B32 = "abcdefghijklmnopqrstuvwxyz234567";
const HEX = "0123456789abcdef";

function seeded(seed: string): () => number {
  let h = 1779033703 ^ seed.length;
  for (let i = 0; i < seed.length; i++) {
    h = Math.imul(h ^ seed.charCodeAt(i), 3432918353);
    h = (h << 13) | (h >>> 19);
  }
  return () => {
    h = (h + 0x6d2b79f5) | 0;
    let t = Math.imul(h ^ (h >>> 15), 1 | h);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

function idFrom(seed: string, alphabet: string, len: number): string {
  const r = seeded(seed);
  let out = "";
  for (let i = 0; i < len; i++) out += alphabet[Math.floor(r() * alphabet.length)];
  return out;
}

const userId = (seed: string): UserId => idFrom(`user:${seed}`, Z32, 52);
const hexId = (seed: string): string => idFrom(`hex:${seed}`, HEX, 64);
let idCounter = 0;
const freshHex = (): string => hexId(`fresh:${Date.now()}:${idCounter++}`);

// ------------------------------------------------------------------ flags

const params = new URLSearchParams(typeof location === "undefined" ? "" : location.search);
const FLAG_FRESH = params.has("fresh");
const FLAG_NOENC = params.has("noenc");
const FLAG_RING = params.has("ring");
const FLAG_NOANSWER = params.has("noanswer");
const FLAG_REMOVED = params.has("removed");

// ------------------------------------------------------------------ people

interface Person {
  id: UserId;
  name: string;
  online: boolean;
}

const people = {
  sam: { id: userId("sam"), name: "Sam", online: true },
  alex: { id: userId("alex"), name: "Alex", online: true },
  jordan: { id: userId("jordan"), name: "Jordan", online: true },
  priya: { id: userId("priya"), name: "Priya", online: true },
  morgan: { id: userId("morgan"), name: "Morgan", online: true },
  riley: { id: userId("riley"), name: "Riley", online: false },
  casey: { id: userId("casey"), name: "Casey", online: false },
  taylor: { id: userId("taylor"), name: "Taylor", online: true },
} satisfies Record<string, Person>;

type PersonKey = keyof typeof people;
const allPeople: Person[] = Object.values(people);
const personById = (id: UserId): Person | undefined => allPeople.find((p) => p.id === id);

// ------------------------------------------------------------------ state

interface MockServer {
  summary: ServerSummary;
  channels: Channel[];
  members: Member[];
}

interface FileInfo {
  name: string;
  mime: string;
  image: string | null;
}

const MB = 1024 * 1024;
const now = Date.now();
const MIN = 60_000;

const st = {
  me: null as Me | null,
  servers: [] as MockServer[],
  dms: [] as DmSummary[],
  messages: new Map<string, Message[]>(),
  transfers: new Map<string, Transfer>(),
  files: new Map<string, FileInfo>(),
  clips: [] as Clip[],
  settings: {
    inputDeviceId: null,
    outputDeviceId: null,
    noiseSuppression: true,
    echoCancellation: true,
    inputVolume: 1,
    outputVolume: 1,
    autoFetchLimit: 25 * MB,
    downloadDir: "C:\\Users\\Sam\\Downloads\\Tincan",
    allowHosting: true,
    customRelayUrl: null,
    theme: "system",
  } as Settings,
  voice: null as VoiceSession | null,
  call: null as CallInfo | null,
  selfMuted: false,
  selfDeafened: false,
  mutedBeforeDeafen: false,
  /** Screen tracks of people currently sharing, by user. */
  screenTracks: new Map<UserId, number>(),
  /** Per-user playback volumes (0..2), as saved by set_user_volume. Jordan starts quieter. */
  volumes: new Map<UserId, number>([[people.jordan.id, 0.3]]),
  /** Per-user soundboard volumes (0..2), as saved by set_user_soundboard_volume. */
  soundVolumes: new Map<UserId, number>(),
  /** Per-voice-room soundboard volumes (0..2), as saved by set_room_soundboard_volume. */
  roomSoundVolumes: new Map<ChannelId, number>(),
  /** My clips playing now, by hash: the timer that ends each. */
  playing: new Map<BlobHash, ReturnType<typeof setTimeout>>(),
  heardDemoDone: false,
  /** Short invite code per server ("ABC-DEF"); none until the owner made one. */
  inviteCodes: new Map<ServerId, string>(),
  /** My friend code; made on first get_friend_code. */
  friendCode: null as string | null,
  /** Codes that were rotated away: they no longer resolve. */
  retiredCodes: new Set<string>(),
};

const convKey = (convId: ConvId, channelId: ChannelId | null) => `${convId}:${channelId ?? ""}`;

// ------------------------------------------------------------------ events

const listeners = new Map<string, Set<(p: unknown) => void>>();

function emit<E extends EventName>(name: E, payload: Events[E]): void {
  const set = listeners.get(name);
  if (!set || set.size === 0) return;
  const copy = structuredClone(payload);
  setTimeout(() => {
    for (const cb of set) cb(copy);
  }, 0);
}

function toast(kind: "info" | "error", text: string): void {
  emit("toast", { kind, text });
}

// ------------------------------------------------------------------ sample files

function svgUrl(svg: string): string {
  return `data:image/svg+xml;charset=utf-8,${encodeURIComponent(svg)}`;
}

function scoreboardImage(): string {
  const rows = [
    ["Jordan", "41", "12"],
    ["Alex", "38", "15"],
    ["Sam", "33", "14"],
    ["Priya", "29", "18"],
    ["Morgan", "17", "22"],
  ];
  const lines = rows
    .map(
      (r, i) =>
        `<text x="40" y="${118 + i * 40}" fill="#e9eaec">${r[0]}</text>` +
        `<text x="360" y="${118 + i * 40}" fill="#e9eaec" text-anchor="end">${r[1]}</text>` +
        `<text x="480" y="${118 + i * 40}" fill="#a4a9b0" text-anchor="end">${r[2]}</text>`,
    )
    .join("");
  return svgUrl(
    `<svg xmlns="http://www.w3.org/2000/svg" width="520" height="320" font-family="Segoe UI, sans-serif" font-size="20">` +
      `<rect width="520" height="320" fill="#1d2127"/>` +
      `<text x="40" y="56" fill="#e0784f" font-size="24" font-weight="600">Final scoreboard</text>` +
      `<text x="360" y="82" fill="#6e747c" font-size="14" text-anchor="end">Kills</text>` +
      `<text x="480" y="82" fill="#6e747c" font-size="14" text-anchor="end">Deaths</text>` +
      lines +
      `</svg>`,
  );
}

function catImage(): string {
  return svgUrl(
    `<svg xmlns="http://www.w3.org/2000/svg" width="360" height="240">` +
      `<rect width="360" height="240" fill="#2a3a4a"/>` +
      `<circle cx="180" cy="140" r="70" fill="#c9a27a"/>` +
      `<path d="M120 95 L135 40 L160 82 Z M240 95 L225 40 L200 82 Z" fill="#c9a27a"/>` +
      `<circle cx="155" cy="130" r="9" fill="#1a1c1f"/><circle cx="205" cy="130" r="9" fill="#1a1c1f"/>` +
      `<path d="M170 160 Q180 170 190 160" stroke="#1a1c1f" stroke-width="4" fill="none"/>` +
      `</svg>`,
  );
}

function placeholderImage(name: string): string {
  return svgUrl(
    `<svg xmlns="http://www.w3.org/2000/svg" width="480" height="270" font-family="Segoe UI, sans-serif">` +
      `<rect width="480" height="270" fill="#283038"/>` +
      `<rect x="24" y="24" width="432" height="222" rx="8" fill="none" stroke="#6e747c" stroke-width="2"/>` +
      `<text x="240" y="142" fill="#a4a9b0" font-size="18" text-anchor="middle">${name.replace(/[<&>]/g, "")}</text>` +
      `</svg>`,
  );
}

function mimeFor(name: string): string {
  const ext = name.toLowerCase().split(".").pop() ?? "";
  const map: Record<string, string> = {
    png: "image/png",
    jpg: "image/jpeg",
    jpeg: "image/jpeg",
    gif: "image/gif",
    webp: "image/webp",
    pdf: "application/pdf",
    zip: "application/zip",
    mp4: "video/mp4",
    mkv: "video/x-matroska",
    mp3: "audio/mpeg",
    wav: "audio/wav",
    ogg: "audio/ogg",
    txt: "text/plain",
    docx: "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    sav: "application/octet-stream",
  };
  return map[ext] ?? "application/octet-stream";
}

interface FileSpec {
  name: string;
  size: number;
  state: Transfer["state"];
  peers: number;
  have?: number;
  image?: string;
}

function addFile(spec: FileSpec) {
  const hash = hexId(`file:${spec.name}`);
  const mime = mimeFor(spec.name);
  st.files.set(hash, { name: spec.name, mime, image: spec.image ?? null });
  const have = spec.state === "complete" ? spec.size : (spec.have ?? 0);
  st.transfers.set(hash, { hash, state: spec.state, have, total: spec.size, peers: spec.peers });
  return { hash, name: spec.name, size: spec.size, mime };
}

// ------------------------------------------------------------------ sample data

type Line = [PersonKey, number, string, FileSpec?];

function addMessages(convId: ConvId, channelId: ChannelId | null, base: number, lines: Line[]): void {
  const list = st.messages.get(convKey(convId, channelId)) ?? [];
  lines.forEach(([who, offsetMin, text, file], i) => {
    const p = people[who];
    list.push({
      id: hexId(`msg:${convId}:${channelId}:${base}:${i}`),
      convId,
      channelId,
      authorId: p.id,
      authorName: p.name,
      text,
      attachments: file ? [addFile(file)] : [],
      sentAt: base + offsetMin * MIN + i * 1000,
      deleted: false,
      pending: false,
    });
  });
  list.sort((a, b) => a.sentAt - b.sentAt);
  st.messages.set(convKey(convId, channelId), list);
}

/** Sample images (data URLs stand in for `http://tincan.localhost/file/<hash>`). */
const SAMPLE_AVATARS = new Map<UserId, string>();
const SAMPLE_ICON = sampleImage("#3f7a7a", "#e0784f", true);

/** A small SVG as a data URL: two overlapping shapes on a coloured ground. */
function sampleImage(bg: string, fg: string, square = false): string {
  const shape = square ? `<rect x="34" y="30" width="60" height="68" rx="10" fill="${fg}"/>` : `<circle cx="64" cy="50" r="24" fill="${fg}"/><rect x="26" y="82" width="76" height="60" rx="30" fill="${fg}"/>`;
  const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="128" height="128" viewBox="0 0 128 128"><rect width="128" height="128" fill="${bg}"/>${shape}</svg>`;
  return `data:image/svg+xml;base64,${btoa(svg)}`;
}

/** Bytes from the UI as a data URL, the way the app serves an uploaded image. */
function bytesUrl(bytes: number[]): string {
  const isPng = bytes[0] === 0x89 && bytes[1] === 0x50;
  const isWebp = bytes[8] === 0x57 && bytes[9] === 0x45 && bytes[10] === 0x42 && bytes[11] === 0x50;
  if (!isPng && !isWebp) fail("That isn't a PNG or WebP image.");
  if (bytes.length > 64 * 1024) fail("That image is larger than 64 KB. Pick a smaller one.");
  let s = "";
  for (const b of bytes) s += String.fromCharCode(b);
  return `data:${isPng ? "image/png" : "image/webp"};base64,${btoa(s)}`;
}

function member(p: Person, extra: Partial<Member> = {}): Member {
  return {
    userId: p.id,
    name: p.name,
    online: p.online,
    isHost: false,
    isOwner: false,
    voiceChannelId: null,
    muted: false,
    deafened: false,
    sharing: false,
    speaking: false,
    avatar: SAMPLE_AVATARS.get(p.id) ?? null,
    ...extra,
  };
}

function channel(serverId: ServerId, name: string, kind: ChannelKind, position: number, unread = false): Channel {
  return { id: hexId(`ch:${serverId}:${name}`), serverId, name, kind, position, unread };
}

function seed(): void {
  SAMPLE_AVATARS.set(people.jordan.id, sampleImage("#4f6d8f", "#e9eaec"));
  SAMPLE_AVATARS.set(people.alex.id, sampleImage("#8a5a44", "#f3c9a8"));
  st.me = { userId: people.sam.id, userCode: `tincan1u${people.sam.id}`, name: "Sam", avatar: null };

  // Game night: Sam owns it, Jordan hosts it right now.
  const gn = hexId("server:game-night");
  const gnGeneral = channel(gn, "general", "text", 0);
  const gnClips = channel(gn, "clips", "text", 1, true);
  const gnLounge = channel(gn, "Lounge", "voice", 2, true);
  const gnSquad = channel(gn, "Squad up", "voice", 3);
  st.screenTracks.set(people.alex.id, 5);
  st.servers.push({
    summary: {
      id: gn,
      name: "Game night",
      ownerId: people.sam.id,
      isOwner: true,
      connection: { state: "member", hostId: people.jordan.id, hostName: "Jordan", path: "direct", rttMs: 24 },
      unread: true,
      onlineCount: 5,
      icon: SAMPLE_ICON,
      removed: false,
    },
    channels: [gnGeneral, gnClips, gnLounge, gnSquad],
    members: [
      member(people.sam, { isOwner: true }),
      member(people.alex, { voiceChannelId: gnLounge.id, sharing: true }),
      member(people.jordan, { isHost: true, voiceChannelId: gnLounge.id, speaking: true }),
      member(people.priya, { voiceChannelId: gnLounge.id, muted: true }),
      member(people.morgan),
      member(people.riley),
      member(people.casey),
    ],
  });

  const day = 24 * 60 * MIN;
  addMessages(gn, gnGeneral.id, now - 3 * day - 5 * 60 * MIN, [
    ["jordan", 0, "Made a server for us so we stop losing stuff in the group chat"],
    ["sam", 3, "Finally"],
    ["alex", 6, "nice, joined"],
    ["priya", 14, "is voice in here too?"],
    ["jordan", 15, "Yep, Lounge. Screen share works too"],
    ["morgan", 40, "testing testing"],
    ["casey", 62, "Hi all"],
    ["riley", 95, "This is way nicer than the old setup"],
  ]);
  addMessages(gn, gnGeneral.id, now - day - 6 * 60 * MIN, [
    ["jordan", 0, "Who's around tonight?"],
    ["jordan", 0, "Thinking we finally finish the campaign"],
    ["alex", 3, "I'm in after 8"],
    ["priya", 7, "same, need to eat first"],
    ["sam", 9, "I'll set up the lobby. Bringing snacks for once"],
    ["morgan", 18, "Can someone send the house rules again? I lost the doc"],
    ["jordan", 20, "Here you go", { name: "house-rules-v2.pdf", size: 1.2 * MB, state: "complete", peers: 4 }],
    ["morgan", 21, "thanks"],
    ["alex", 122, "ok online now"],
    ["alex", 122, "voice is open whenever"],
    ["priya", 133, "joining in 2"],
    ["morgan", 134, "Map pack for the custom lobby, it's big so grab it before we start", {
      name: "custom-map-pack.zip",
      size: 48.6 * MB,
      state: "idle",
      peers: 2,
    }],
    ["jordan", 136, "downloading"],
    ["morgan", 149, "that last round was rigged"],
    ["alex", 149, "skill issue"],
    ["priya", 150, "lol"],
    ["jordan", 218, "Final scoreboard", {
      name: "scoreboard.png",
      size: 412 * 1024,
      state: "complete",
      peers: 5,
      image: scoreboardImage(),
    }],
    ["sam", 219, "We need a rematch"],
    ["morgan", 219, "rematch friday"],
    ["alex", 253, "gn all"],
    ["riley", 285, "Sorry I missed it, work ran late"],
    ["riley", 285, "Friday works for me"],
  ]);
  addMessages(gn, gnGeneral.id, now - 3 * 60 * MIN, [
    ["jordan", 0, "morning. anyone tried the new patch?"],
    ["priya", 5, "yeah, they nerfed the grappling hook"],
    ["priya", 5, "rip"],
    ["alex", 30, "clip from last night, the clutch at the end", {
      name: "clutch-round-14.mp4",
      size: 18.4 * MB,
      state: "downloading",
      peers: 2,
      have: 6.2 * MB,
    }],
    ["morgan", 32, "how did that even work"],
    ["alex", 33, "don't ask"],
    ["casey", 60, "Old save from the last campaign if anyone wants it", {
      name: "campaign-act2.sav",
      size: 3.1 * MB,
      state: "unavailable",
      peers: 0,
    }],
    ["jordan", 120, "friday 8pm then?"],
    ["sam", 122, "Works for me"],
    ["priya", 140, "works"],
    ["morgan", 145, "I'll bring the extra controller"],
    ["jordan", 168, "Lounge is open if anyone wants to warm up"],
    ["alex", 170, "sharing my screen, setting up the lobby"],
    ["priya", 175, "omw"],
  ]);
  addMessages(gn, gnClips.id, now - 20 * 60 * MIN, [
    ["alex", 0, "best of last night", { name: "highlights.mp4", size: 22.3 * MB, state: "complete", peers: 3 }],
    ["jordan", 12, "this one is cursed", {
      name: "cat-reaction.gif",
      size: 860 * 1024,
      state: "complete",
      peers: 4,
      image: catImage(),
    }],
    ["priya", 15, "the cat is me every round"],
    ["morgan", 600, "more clips tonight?"],
  ]);
  // A voice room's chat: links and notes dropped while people talk.
  addMessages(gn, gnLounge.id, now - 12 * MIN, [
    ["jordan", 0, "lobby code is 4471"],
    ["alex", 2, "can you hear the game audio through my share?"],
    ["priya", 3, "yes, a bit loud"],
  ]);

  // Book club: Riley owns it, Sam is hosting because Riley is offline.
  const bc = hexId("server:book-club");
  const bcGeneral = channel(bc, "general", "text", 0, true);
  const bcList = channel(bc, "reading-list", "text", 1);
  const bcRoom = channel(bc, "Reading room", "voice", 2);
  st.servers.push({
    summary: {
      id: bc,
      name: "Book club",
      ownerId: people.riley.id,
      isOwner: false,
      connection: FLAG_REMOVED
        ? { state: "offline", hostId: null, hostName: null, path: "none", rttMs: null }
        : { state: "hosting", hostId: people.sam.id, hostName: "Sam", path: "none", rttMs: null },
      unread: true,
      onlineCount: FLAG_REMOVED ? 1 : 2,
      icon: null,
      removed: FLAG_REMOVED,
    },
    channels: [bcGeneral, bcList, bcRoom],
    members: [
      ...(FLAG_REMOVED ? [] : [member(people.sam, { isHost: true })]),
      member(people.riley, { isOwner: true }),
      member(people.taylor),
      member(people.casey),
    ],
  });
  addMessages(bc, bcGeneral.id, now - 2 * day, [
    ["riley", 0, "Next pick: The Left Hand of Darkness"],
    ["taylor", 30, "Started it last night, the first chapter is slow"],
    ["sam", 45, "It picks up around chapter 4"],
    ["casey", 300, "My notes so far", { name: "chapter-notes.docx", size: 84 * 1024, state: "complete", peers: 2 }],
    ["riley", 1500, "Meeting Sunday?"],
    ["taylor", 2700, "Sunday works. Halfway through"],
  ]);
  addMessages(bc, bcList.id, now - 9 * day, [
    ["riley", 0, "Ideas for next month go here"],
    ["taylor", 60, "Piranesi"],
    ["sam", 90, "Project Hail Mary"],
  ]);

  // DMs
  const dmAlex = hexId("dm:alex");
  const dmRiley = hexId("dm:riley");
  st.dms.push(
    { convId: dmAlex, userId: people.alex.id, name: "Alex", online: true, unread: false, lastMessageAt: now - 50 * MIN, avatar: SAMPLE_AVATARS.get(people.alex.id) ?? null },
    { convId: dmRiley, userId: people.riley.id, name: "Riley", online: false, unread: false, lastMessageAt: now - 25 * MIN, avatar: null },
  );
  addMessages(dmAlex, null, now - 4 * 60 * MIN, [
    ["alex", 0, "you still have my controller from last week?"],
    ["sam", 4, "Yes, bringing it Friday"],
    ["alex", 5, "legend"],
    ["alex", 180, "patch notes if you haven't seen them", {
      name: "patch-1.4-notes.txt",
      size: 14 * 1024,
      state: "complete",
      peers: 1,
    }],
    ["sam", 190, "Grappling hook nerf hurts"],
  ]);
  addMessages(dmRiley, null, now - 26 * 60 * MIN, [
    ["riley", 0, "Can you host book club on Sunday? I'm travelling"],
    ["sam", 20, "Sure, I'll keep my PC on"],
    ["sam", 1535, "Are you coming Friday too?"],
  ]);
  const rileyMsgs = st.messages.get(convKey(dmRiley, null));
  const last = rileyMsgs?.[rileyMsgs.length - 1];
  if (last) last.pending = true;

  // Soundboard
  const owned: [string, number, number][] = [
    ["Airhorn", 1800, 58_000],
    ["Drumroll", 3200, 101_000],
    ["Sad trombone", 2600, 84_000],
    ["Bruh", 900, 21_000],
    ["Applause", 4500, 142_000],
    ["Wow", 1200, 33_000],
  ];
  for (const [name, durationMs, size] of owned) {
    st.clips.push({ hash: hexId(`clip:${name}`), name, durationMs, size, owned: true, heardFrom: null, transfer: null });
  }
  st.clips.push(
    {
      hash: hexId("clip:Victory fanfare"),
      name: "Victory fanfare",
      durationMs: 5100,
      size: 162_000,
      owned: false,
      heardFrom: people.jordan.id,
      transfer: { hash: hexId("clip:Victory fanfare"), state: "idle", have: 0, total: 162_000, peers: 2 },
    },
    {
      hash: hexId("clip:Boing"),
      name: "Boing",
      durationMs: 700,
      size: 18_000,
      owned: false,
      heardFrom: people.alex.id,
      transfer: { hash: hexId("clip:Boing"), state: "idle", have: 0, total: 18_000, peers: 1 },
    },
  );
}

if (!FLAG_FRESH) seed();

// ------------------------------------------------------------------ helpers

class CmdError extends Error {}
function fail(text: string): never {
  throw new CmdError(text);
}

function server(id: ServerId): MockServer {
  return st.servers.find((s) => s.summary.id === id) ?? fail("That server isn't on this PC anymore.");
}

function requireOwner(s: MockServer, what: string): void {
  if (!s.summary.isOwner) fail(`Only the server owner can ${what}.`);
}

function meId(): UserId {
  return st.me?.userId ?? fail("Pick a display name first.");
}

function snapshot(): AppSnapshot {
  return {
    me: st.me,
    servers: st.servers.map((s) => s.summary),
    dms: st.dms,
    settings: st.settings,
    voice: st.voice,
    call: st.call,
    version: "0.1.0",
  };
}

function refreshOnline(s: MockServer): void {
  s.summary.onlineCount = s.members.filter((m) => m.online).length;
}

function emitMembers(s: MockServer): void {
  emit("members-updated", { serverId: s.summary.id, members: s.members });
}

function emitChannels(s: MockServer): void {
  emit("channels-updated", { serverId: s.summary.id, channels: s.channels });
}

function participantOf(m: { userId: UserId; name: string; muted: boolean; deafened: boolean; speaking: boolean }): VoiceParticipant {
  return {
    userId: m.userId,
    name: m.name,
    muted: m.muted,
    deafened: m.deafened,
    speaking: m.speaking,
    avatar: m.userId === st.me?.userId ? st.me.avatar : (SAMPLE_AVATARS.get(m.userId) ?? null),
    screenTrack: st.screenTracks.get(m.userId) ?? null,
    volume: m.userId === st.me?.userId ? 1 : (st.volumes.get(m.userId) ?? 1),
    soundboardVolume: m.userId === st.me?.userId ? 1 : (st.soundVolumes.get(m.userId) ?? 1),
  };
}

/** My clips stop when I leave voice. */
function stopPlaying(): void {
  for (const t of st.playing.values()) clearTimeout(t);
  st.playing.clear();
}

/** Rebuilds the voice session from the member list (rooms) or the call peer (calls). */
function rebuildVoice(): void {
  const v = st.voice;
  if (!v) return;
  const me = st.me;
  if (!me) return;
  v.muted = st.selfMuted;
  v.deafened = st.selfDeafened;
  v.sharing = st.screenTracks.has(me.userId);
  v.playingClips = [...st.playing.keys()].sort();
  if (v.kind === "room") {
    const s = st.servers.find((x) => x.summary.id === v.convId);
    if (!s) return;
    v.participants = s.members.filter((m) => m.online && m.voiceChannelId === v.channelId).map(participantOf);
  } else {
    const peer = st.dms.find((d) => d.convId === v.convId);
    const prev = v.participants.find((p) => p.userId !== me.userId);
    v.participants = [
      participantOf({ userId: me.userId, name: me.name, muted: st.selfMuted, deafened: st.selfDeafened, speaking: false }),
    ];
    if (peer) {
      v.participants.push(
        participantOf({
          userId: peer.userId,
          name: peer.name,
          muted: false,
          deafened: false,
          speaking: prev?.speaking ?? false,
        }),
      );
    }
  }
}

function setMyVoiceState(): void {
  const me = st.me;
  if (!me) return;
  for (const s of st.servers) {
    const m = s.members.find((x) => x.userId === me.userId);
    if (!m) continue;
    const inRoom = st.voice?.kind === "room" && st.voice.convId === s.summary.id;
    const next = {
      voiceChannelId: inRoom ? (st.voice?.channelId ?? null) : null,
      muted: inRoom ? st.selfMuted : false,
      deafened: inRoom ? st.selfDeafened : false,
      sharing: inRoom ? st.screenTracks.has(me.userId) : false,
    };
    if (
      m.voiceChannelId !== next.voiceChannelId ||
      m.muted !== next.muted ||
      m.deafened !== next.deafened ||
      m.sharing !== next.sharing
    ) {
      Object.assign(m, next);
      if (!inRoom) m.speaking = false;
      emitMembers(s);
    }
  }
}

function voiceChanged(): void {
  setMyVoiceState();
  rebuildVoice();
  emit("voice-updated", st.voice);
}

// ------------------------------------------------------------------ timers

let speakTimer: ReturnType<typeof setInterval> | null = null;

/** Flips speaking flags of people in voice rooms. Runs only while someone is in voice. */
function speakingTick(): void {
  let anyone = false;
  const meUser = st.me?.userId;
  for (const s of st.servers) {
    for (const m of s.members) {
      if (!m.online || m.voiceChannelId === null) continue;
      anyone = true;
      const canSpeak = !m.muted && !m.deafened && (m.userId !== meUser || st.voice !== null);
      const next = canSpeak ? (m.speaking ? Math.random() > 0.35 : Math.random() > 0.72) : false;
      if (next !== m.speaking) {
        m.speaking = next;
        emit("speaking", { userId: m.userId, speaking: next });
      }
    }
  }
  if (st.voice?.kind === "call") {
    anyone = true;
    for (const p of st.voice.participants) {
      const canSpeak = p.userId === meUser ? !st.selfMuted : true;
      const next = canSpeak ? (p.speaking ? Math.random() > 0.35 : Math.random() > 0.72) : false;
      if (next !== p.speaking) {
        p.speaking = next;
        emit("speaking", { userId: p.userId, speaking: next });
      }
    }
  }
  if (!anyone && speakTimer !== null) {
    clearInterval(speakTimer);
    speakTimer = null;
  }
}

function ensureSpeakingTimer(): void {
  if (speakTimer === null) speakTimer = setInterval(speakingTick, 700);
}

let transferTimer: ReturnType<typeof setInterval> | null = null;

function transferTick(): void {
  let active = false;
  for (const t of st.transfers.values()) {
    if (t.state === "queued" && t.peers > 0) {
      t.state = "downloading";
      emit("transfer-updated", t);
      active = true;
    } else if (t.state === "downloading") {
      active = true;
      t.have = Math.min(t.total, t.have + Math.max(64 * 1024, t.total / 40) * (0.6 + Math.random() * 0.8));
      if (t.have >= t.total) {
        t.have = t.total;
        t.state = "complete";
      }
      emit("transfer-updated", t);
    }
  }
  for (const c of st.clips) {
    const t = c.transfer;
    if (!t || t.state !== "downloading") continue;
    active = true;
    t.have = Math.min(t.total, t.have + t.total / 6);
    if (t.have >= t.total) {
      t.state = "complete";
      c.owned = true;
    }
    emit("transfer-updated", t);
    emit("clips-updated", st.clips);
  }
  if (!active && transferTimer !== null) {
    clearInterval(transferTimer);
    transferTimer = null;
  }
}

function ensureTransferTimer(): void {
  if (transferTimer === null) transferTimer = setInterval(transferTick, 400);
}

let micTimer: ReturnType<typeof setInterval> | null = null;
let micLevel = 0;

function micTick(): void {
  const talking = Math.sin(Date.now() / 900) > -0.2;
  const target = talking ? 0.35 + Math.random() * 0.5 : Math.random() * 0.06;
  micLevel = micLevel * 0.5 + target * 0.5;
  emit("mic-level", { level: Math.min(1, micLevel) });
}

// ------------------------------------------------------------------ replies

const replies = [
  "lol",
  "agreed",
  "wait what",
  "on my way to voice",
  "good call",
  "same here",
  "I'm down",
];
let replyIndex = 0;

function simulateReply(s: MockServer, channelId: ChannelId): void {
  const who = people.jordan;
  const base = { convId: s.summary.id, channelId, userId: who.id, name: who.name };
  setTimeout(() => emit("typing", base), 700);
  setTimeout(() => emit("typing", base), 3700);
  setTimeout(() => {
    const list = st.messages.get(convKey(s.summary.id, channelId));
    if (!list) return;
    const msg: Message = {
      id: freshHex(),
      convId: s.summary.id,
      channelId,
      authorId: who.id,
      authorName: who.name,
      text: replies[replyIndex++ % replies.length],
      attachments: [],
      sentAt: Date.now(),
      deleted: false,
      pending: false,
    };
    list.push(msg);
    emit("message-added", msg);
    const ch = s.channels.find((c) => c.id === channelId);
    if (ch && !ch.unread) {
      ch.unread = true;
      s.summary.unread = true;
      emitChannels(s);
      emit("server-updated", s.summary);
    }
  }, 5200);
}

function heardDemo(): void {
  if (st.heardDemoDone || st.voice?.kind !== "room") return;
  st.heardDemoDone = true;
  const clip: Clip = {
    hash: hexId("clip:Rimshot"),
    name: "Rimshot",
    durationMs: 1400,
    size: 41_000,
    owned: false,
    heardFrom: people.jordan.id,
    transfer: { hash: hexId("clip:Rimshot"), state: "idle", have: 0, total: 41_000, peers: 1 },
  };
  if (!st.clips.some((c) => c.hash === clip.hash)) st.clips.push(clip);
  emit("sound-played", { userId: people.jordan.id, name: "Jordan", clip });
  emit("clips-updated", st.clips);
}

// ------------------------------------------------------------------ presence helpers (console)

function setOnline(name: string, online: boolean): void {
  const p = allPeople.find((x) => x.name.toLowerCase() === name.toLowerCase());
  if (!p) return;
  p.online = online;
  for (const s of st.servers) {
    const m = s.members.find((x) => x.userId === p.id);
    if (!m) continue;
    m.online = online;
    if (!online) {
      m.voiceChannelId = null;
      m.speaking = false;
    }
    refreshOnline(s);
    emitMembers(s);
    emit("server-updated", s.summary);
  }
  const dm = st.dms.find((d) => d.userId === p.id);
  if (dm) {
    dm.online = online;
    emit("dm-updated", dm);
    if (online) {
      for (const m of st.messages.get(convKey(dm.convId, null)) ?? []) {
        if (m.pending) {
          m.pending = false;
          emit("message-updated", m);
        }
      }
    }
  }
  if (st.voice) voiceChanged();
}

function ringFrom(p: Person): void {
  if (st.call || !st.me) return;
  st.call = { callId: freshHex(), userId: p.id, name: p.name, state: "ringing-in" };
  emit("call-updated", st.call);
  const id = st.call.callId;
  setTimeout(() => {
    if (st.call?.callId === id && st.call.state === "ringing-in") {
      st.call = null;
      emit("call-updated", null);
      toast("info", `Missed call from ${p.name}`);
    }
  }, 30_000);
}

function dmWith(p: Person): DmSummary {
  let dm = st.dms.find((d) => d.userId === p.id);
  if (!dm) {
    dm = { convId: hexId(`dm:${p.name.toLowerCase()}`), userId: p.id, name: p.name, online: p.online, unread: false, lastMessageAt: null, avatar: SAMPLE_AVATARS.get(p.id) ?? null };
    st.dms.push(dm);
    st.messages.set(convKey(dm.convId, null), []);
  }
  return dm;
}

function endCall(): void {
  const hadVoice = st.voice?.kind === "call";
  st.call = null;
  emit("call-updated", null);
  if (hadVoice) {
    stopPlaying();
    st.voice = null;
    st.screenTracks.delete(meId());
    emit("voice-updated", null);
  }
}

function startCallVoice(dm: DmSummary): VoiceSession {
  if (st.voice?.kind === "room") leaveRoom();
  st.voice = {
    kind: "call",
    convId: dm.convId,
    channelId: null,
    name: dm.name,
    participants: [],
    muted: st.selfMuted,
    deafened: st.selfDeafened,
    sharing: false,
    path: "relay",
    rttMs: 88,
    playingClips: [],
  };
  rebuildVoice();
  ensureSpeakingTimer();
  emit("voice-updated", st.voice);
  return st.voice;
}

function leaveRoom(): void {
  const me = st.me;
  if (me) st.screenTracks.delete(me.userId);
  stopPlaying();
  st.voice = null;
  setMyVoiceState();
  emit("voice-updated", null);
}

// ------------------------------------------------------------------ commands

type Handlers = { [K in CommandName]: (args: Commands[K]["args"]) => Commands[K]["result"] | Promise<Commands[K]["result"]> };

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

const handlers: Handlers = {
  app_snapshot: () => snapshot(),

  set_name: ({ name }) => {
    const n = name.trim();
    if (!n) fail("Enter a display name.");
    if (n.length > 32) fail("Keep your name to 32 characters or fewer.");
    if (st.me) {
      st.me.name = n;
    } else {
      st.me = { userId: people.sam.id, userCode: `tincan1u${people.sam.id}`, name: n, avatar: null };
    }
    people.sam.name = n;
    for (const s of st.servers) {
      const m = s.members.find((x) => x.userId === st.me?.userId);
      if (m) {
        m.name = n;
        emitMembers(s);
      }
    }
    emit("snapshot", snapshot());
    return st.me;
  },

  create_server: ({ name }) => {
    const n = name.trim();
    if (!n) fail("Enter a server name.");
    const me = st.me ?? fail("Pick a display name first.");
    const id = freshHex();
    const s: MockServer = {
      summary: {
        id,
        name: n,
        ownerId: me.userId,
        isOwner: true,
        connection: { state: "solo", hostId: me.userId, hostName: me.name, path: "none", rttMs: null },
        unread: false,
        onlineCount: 1,
        icon: null,
        removed: false,
      },
      channels: [channel(id, "general", "text", 0), channel(id, "Lounge", "voice", 1)],
      members: [member({ id: me.userId, name: me.name, online: true }, { isHost: true, isOwner: true })],
    };
    st.servers.push(s);
    for (const c of s.channels) st.messages.set(convKey(id, c.id), []);
    emit("snapshot", snapshot());
    return s.summary;
  },

  create_invite: ({ serverId }) => {
    server(serverId);
    return `tincan1${idFrom(`invite:${serverId}:${Date.now()}`, B32, 118)}`;
  },

  get_invite_code: async ({ serverId }) => {
    const s = server(serverId);
    const have = st.inviteCodes.get(serverId);
    if (have || !s.summary.isOwner) return have ?? null;
    await sleep(700);
    const code = randomShortCode();
    st.inviteCodes.set(serverId, code);
    return code;
  },

  rotate_invite_code: async ({ serverId }) => {
    const s = server(serverId);
    requireOwner(s, "rotate the invite code");
    await sleep(700);
    const old = st.inviteCodes.get(serverId);
    if (old) st.retiredCodes.add(old);
    const code = randomShortCode();
    st.inviteCodes.set(serverId, code);
    return code;
  },

  join_server: async ({ invite }) => {
    const short = parseShortCode(invite);
    if (short) {
      await sleep(900);
      for (const [id, c] of st.inviteCodes) {
        if (c === short) fail(`You're already in ${server(id).summary.name}.`);
      }
      if (st.retiredCodes.has(short) || short.startsWith("X")) fail("No server uses that code. Check it, or ask for a new one.");
    }
    const code = short ? `tincan1short${short}` : invite.trim().toLowerCase();
    if (code.startsWith("tincan1u")) fail("That's someone's Tincan ID, not a server invite. Add them as a contact instead.");
    if (!code.startsWith("tincan1")) fail("That isn't a Tincan invite. Invite codes start with tincan1.");
    if (!short && code.length < 40) fail("That invite is cut off. Copy the whole code and paste it again.");
    await sleep(1200);
    if (code.includes("offline")) fail("Couldn't reach anyone in that server. Check the invite or try again later.");
    const me = st.me ?? fail("Pick a display name first.");
    // Any invite takes a removed member back into the server it was removed from.
    const removed = st.servers.find((s) => s.summary.removed);
    if (removed) {
      const host = removed.members.find((m) => m.online) ?? removed.members[0];
      removed.summary.removed = false;
      removed.summary.connection = { state: "member", hostId: host?.userId ?? null, hostName: host?.name ?? null, path: "direct", rttMs: 31 };
      removed.members = [member({ id: me.userId, name: me.name, online: true }), ...removed.members];
      refreshOnline(removed);
      emit("snapshot", snapshot());
      return removed.summary;
    }
    const existing = st.servers.find((s) => s.summary.name === "Movie night");
    if (existing) fail("You're already in Movie night.");
    const id = hexId(`server:movie-night:${code}`);
    const s: MockServer = {
      summary: {
        id,
        name: "Movie night",
        ownerId: people.morgan.id,
        isOwner: false,
        connection: { state: "member", hostId: people.morgan.id, hostName: "Morgan", path: "relay", rttMs: 92 },
        unread: false,
        onlineCount: 3,
        icon: null,
        removed: false,
      },
      channels: [channel(id, "general", "text", 0), channel(id, "Theater", "voice", 1)],
      members: [
        member({ id: me.userId, name: me.name, online: true }),
        member(people.morgan, { isHost: true, isOwner: true }),
        member(people.taylor),
      ],
    };
    st.servers.push(s);
    st.inviteCodes.set(id, randomShortCode());
    addMessages(id, s.channels[0].id, Date.now() - 50 * MIN, [
      ["morgan", 0, "Movie pick for Saturday?"],
      ["taylor", 12, "Something short, I'm tired"],
    ]);
    emit("snapshot", snapshot());
    return s.summary;
  },

  leave_server: ({ serverId }) => {
    server(serverId);
    if (st.voice?.kind === "room" && st.voice.convId === serverId) leaveRoom();
    st.servers = st.servers.filter((s) => s.summary.id !== serverId);
    emit("snapshot", snapshot());
    return null;
  },

  set_avatar: async ({ bytes }) => {
    const me = st.me ?? fail("Pick a display name first.");
    const url = bytes ? bytesUrl(bytes) : null;
    await sleep(200);
    me.avatar = url;
    for (const s of st.servers) {
      const m = s.members.find((x) => x.userId === me.userId);
      if (m) {
        m.avatar = url;
        emitMembers(s);
      }
    }
    if (st.voice) {
      rebuildVoice();
      emit("voice-updated", st.voice);
    }
    emit("snapshot", snapshot());
    return me;
  },

  set_server_icon: async ({ serverId, bytes }) => {
    const s = server(serverId);
    requireOwner(s, "change its icon");
    const url = bytes ? bytesUrl(bytes) : null;
    await sleep(200);
    s.summary.icon = url;
    emit("server-updated", s.summary);
    return s.summary;
  },

  kick_member: ({ serverId, userId }) => {
    const s = server(serverId);
    if (!s.summary.isOwner) fail("Only the server owner can do that.");
    if (userId === meId()) fail("You can't remove yourself. Leave the server instead.");
    const m = s.members.find((x) => x.userId === userId) ?? fail("They're not in this server anymore.");
    s.members = s.members.filter((x) => x.userId !== userId);
    refreshOnline(s);
    emitMembers(s);
    emit("server-updated", s.summary);
    if (st.voice?.kind === "room" && st.voice.convId === serverId) {
      rebuildVoice();
      emit("voice-updated", st.voice);
    }
    toast("info", `${m.name} was removed from ${s.summary.name}.`);
    return null;
  },

  rename_server: ({ serverId, name }) => {
    const s = server(serverId);
    requireOwner(s, "rename it");
    const n = name.trim();
    if (!n) fail("Enter a server name.");
    s.summary.name = n;
    emit("server-updated", s.summary);
    return null;
  },

  channels: ({ serverId }) => server(serverId).channels,

  create_channel: ({ serverId, name, kind }) => {
    const s = server(serverId);
    requireOwner(s, "create channels");
    const n = name.trim();
    if (!n) fail("Enter a channel name.");
    const c: Channel = {
      id: freshHex(),
      serverId,
      name: n,
      kind,
      position: s.channels.reduce((m, x) => Math.max(m, x.position), -1) + 1,
      unread: false,
    };
    s.channels.push(c);
    st.messages.set(convKey(serverId, c.id), []);
    emitChannels(s);
    return c;
  },

  rename_channel: ({ serverId, channelId, name }) => {
    const s = server(serverId);
    requireOwner(s, "rename channels");
    const c = s.channels.find((x) => x.id === channelId) ?? fail("That channel was deleted.");
    const n = name.trim();
    if (!n) fail("Enter a channel name.");
    c.name = n;
    emitChannels(s);
    if (st.voice?.channelId === channelId) {
      st.voice.name = n;
      emit("voice-updated", st.voice);
    }
    return null;
  },

  delete_channel: ({ serverId, channelId }) => {
    const s = server(serverId);
    requireOwner(s, "delete channels");
    if (st.voice?.channelId === channelId) leaveRoom();
    s.channels = s.channels.filter((c) => c.id !== channelId);
    emitChannels(s);
    return null;
  },

  members: ({ serverId }) => server(serverId).members,

  messages: ({ convId, channelId, before, limit }) => {
    const list = st.messages.get(convKey(convId, channelId)) ?? [];
    const end = before === null ? list.length : Math.max(0, Number.parseInt(before, 10) || 0);
    // the app clamps the page size to 1..200
    const start = Math.max(0, end - Math.min(200, Math.max(1, limit)));
    return { messages: list.slice(start, end), cursor: start > 0 ? String(start) : null };
  },

  send_message: async ({ convId, channelId, text, attachments }) => {
    const me = st.me ?? fail("Pick a display name first.");
    if (!text.trim() && attachments.length === 0) fail("Type a message or attach a file.");
    const dm = channelId === null ? st.dms.find((d) => d.convId === convId) : undefined;
    let s: MockServer | undefined;
    if (channelId !== null) {
      s = server(convId);
      if (s.summary.removed) fail("You were removed from this server.");
      if (!s.channels.some((c) => c.id === channelId)) fail("That channel was deleted.");
    } else if (!dm) {
      fail("That conversation isn't on this PC anymore.");
    }
    if (attachments.length > 0) await sleep(250);
    const atts = attachments.map((path) => {
      const name = baseName(path);
      const size = Math.round((0.2 + seeded(path)() * 8) * MB);
      const isImage = mimeFor(name).startsWith("image/");
      const a = addFile({
        name,
        size,
        state: "complete",
        peers: 0,
        image: isImage ? placeholderImage(name) : undefined,
      });
      const peers = s ? s.members.filter((m) => m.online && m.userId !== me.userId).length : dm?.online ? 1 : 0;
      if (peers > 0) {
        setTimeout(() => {
          const t = st.transfers.get(a.hash);
          if (!t) return;
          t.peers = peers;
          emit("transfer-updated", t);
        }, 2500);
      }
      return a;
    });
    const msg: Message = {
      id: freshHex(),
      convId,
      channelId,
      authorId: me.userId,
      authorName: me.name,
      text: text.trim(),
      attachments: atts,
      sentAt: Date.now(),
      deleted: false,
      pending: dm ? !dm.online : false,
    };
    const key = convKey(convId, channelId);
    const list = st.messages.get(key) ?? [];
    list.push(msg);
    st.messages.set(key, list);
    emit("message-added", msg);
    if (dm) {
      dm.lastMessageAt = msg.sentAt;
      emit("dm-updated", dm);
    }
    if (s && channelId && s.summary.name === "Game night" && s.channels[0]?.id === channelId) {
      simulateReply(s, channelId);
    }
    return msg;
  },

  delete_message: ({ convId, messageId }) => {
    const me = meId();
    for (const [key, list] of st.messages) {
      if (!key.startsWith(`${convId}:`)) continue;
      const m = list.find((x) => x.id === messageId);
      if (!m) continue;
      const owner = st.servers.find((s) => s.summary.id === convId)?.summary.isOwner ?? false;
      if (m.authorId !== me && !owner) fail("You can only delete your own messages.");
      m.deleted = true;
      m.text = "";
      m.attachments = [];
      emit("message-updated", m);
      return null;
    }
    return fail("That message is already gone.");
  },

  mark_read: ({ convId, channelId }) => {
    if (channelId === null) {
      const dm = st.dms.find((d) => d.convId === convId);
      if (dm?.unread) {
        dm.unread = false;
        emit("dm-updated", dm);
      }
      return null;
    }
    const s = st.servers.find((x) => x.summary.id === convId);
    const c = s?.channels.find((x) => x.id === channelId);
    if (!s || !c) return null;
    if (c.unread) {
      c.unread = false;
      emitChannels(s);
    }
    const unread = s.channels.some((x) => x.unread);
    if (unread !== s.summary.unread) {
      s.summary.unread = unread;
      emit("server-updated", s.summary);
    }
    return null;
  },

  typing: () => null,

  transfer: ({ hash }) => st.transfers.get(hash) ?? { hash, state: "idle", have: 0, total: 0, peers: 0 },

  download: ({ hash }) => {
    const t = st.transfers.get(hash) ?? fail("That file isn't known here.");
    if (t.state === "complete" || t.state === "downloading") return t;
    t.state = t.peers > 0 ? "downloading" : "queued";
    emit("transfer-updated", t);
    ensureTransferTimer();
    return t;
  },

  open_file: ({ hash, name }) => {
    const t = st.transfers.get(hash);
    if (!t || t.state !== "complete") fail("Download the file first.");
    toast("info", `Opened ${name} from ${st.settings.downloadDir}`);
    return null;
  },

  file_url: ({ hash }) => {
    const t = st.transfers.get(hash);
    if (!t || t.state !== "complete") return null;
    return st.files.get(hash)?.image ?? null;
  },

  open_dm: ({ userId }) => {
    const p = personById(userId) ?? fail("Couldn't find that person.");
    if (p.id === st.me?.userId) fail("That's you.");
    const dm = dmWith(p);
    emit("dm-updated", dm);
    return dm;
  },

  dm_watch: () => null,

  add_contact: async ({ code }) => {
    const raw = code.trim();
    const short = parseShortCode(raw);
    let id: UserId;
    if (short) {
      await sleep(600);
      if (short === st.friendCode) fail("That's your own code. Enter the code of the person you want to message.");
      if (st.retiredCodes.has(short) || short.startsWith("X")) fail("No one uses that friend code. Check it, or ask for a new one.");
      id = userId(`friend:${short}`);
    } else {
      id = raw.toLowerCase().startsWith("tincan1u") ? raw.slice(8) : raw;
      if (id.length !== 52 || [...id].some((ch) => !Z32.includes(ch))) {
        fail("That isn't a friend code or a Tincan ID. A friend code has 6 characters, like K7Q-M2X.");
      }
    }
    if (id === st.me?.userId) fail("That's your own code. Enter the code of the person you want to message.");
    let p = personById(id);
    if (!p) {
      p = { id, name: "Drew", online: false };
      allPeople.push(p);
    }
    const dm = dmWith(p);
    emit("dm-updated", dm);
    return dm;
  },

  get_friend_code: () => {
    meId();
    st.friendCode ??= randomShortCode();
    return st.friendCode;
  },

  rotate_friend_code: async () => {
    meId();
    await sleep(300);
    if (st.friendCode) st.retiredCodes.add(st.friendCode);
    st.friendCode = randomShortCode();
    return st.friendCode;
  },

  voice_join: async ({ serverId, channelId }) => {
    const s = server(serverId);
    if (s.summary.removed) fail("You were removed from this server.");
    const c = s.channels.find((x) => x.id === channelId && x.kind === "voice") ?? fail("That voice room was deleted.");
    if (st.call) endCall();
    await sleep(150);
    if (st.voice?.kind === "room") {
      const me = meId();
      st.screenTracks.delete(me);
    }
    st.voice = {
      kind: "room",
      convId: serverId,
      channelId: c.id,
      name: c.name,
      participants: [],
      muted: st.selfMuted,
      deafened: st.selfDeafened,
      sharing: false,
      path: s.summary.connection.path === "none" ? "direct" : s.summary.connection.path,
      rttMs: s.summary.connection.rttMs,
      playingClips: [],
    };
    setMyVoiceState();
    rebuildVoice();
    emit("voice-updated", st.voice);
    ensureSpeakingTimer();
    setTimeout(heardDemo, 8000);
    return st.voice;
  },

  voice_leave: () => {
    if (st.voice?.kind === "room") leaveRoom();
    return null;
  },

  call_start: ({ userId }) => {
    const p = personById(userId) ?? fail("Couldn't find that person.");
    if (!p.online) fail(`${p.name} is offline. Try again when they're online.`);
    if (st.call) fail("Hang up your current call first.");
    const dm = dmWith(p);
    const call: CallInfo = { callId: freshHex(), userId: p.id, name: p.name, state: "ringing-out" };
    st.call = call;
    emit("call-updated", call);
    setTimeout(
      () => {
        if (st.call?.callId !== call.callId) return;
        if (FLAG_NOANSWER) {
          st.call = null;
          emit("call-updated", null);
          toast("info", `${p.name} didn't answer.`);
          return;
        }
        st.call = { ...call, state: "active" };
        emit("call-updated", st.call);
        startCallVoice(dm);
      },
      FLAG_NOANSWER ? 45_000 : 2500,
    );
    return call;
  },

  call_accept: ({ callId }) => {
    if (!st.call || st.call.callId !== callId) fail("That call has ended.");
    const call = st.call as CallInfo;
    const p = personById(call.userId) ?? fail("That call has ended.");
    st.call = { ...call, state: "active" };
    emit("call-updated", st.call);
    return startCallVoice(dmWith(p));
  },

  call_decline: ({ callId }) => {
    if (st.call?.callId === callId) {
      st.call = null;
      emit("call-updated", null);
    }
    return null;
  },

  call_hangup: () => {
    if (st.call || st.voice?.kind === "call") endCall();
    return null;
  },

  set_muted: ({ muted }) => {
    st.selfMuted = muted;
    if (!muted && st.selfDeafened) st.selfDeafened = false;
    if (st.voice) voiceChanged();
    return null;
  },

  set_deafened: ({ deafened }) => {
    if (deafened && !st.selfDeafened) {
      st.mutedBeforeDeafen = st.selfMuted;
      st.selfMuted = true;
    } else if (!deafened && st.selfDeafened) {
      st.selfMuted = st.mutedBeforeDeafen;
    }
    st.selfDeafened = deafened;
    if (st.voice) voiceChanged();
    return null;
  },

  set_user_volume: ({ userId, volume }) => {
    if (!(volume >= 0 && volume <= 2)) fail("Volume must be between 0% and 200%.");
    st.volumes.set(userId, volume);
    if (st.voice) voiceChanged();
    return null;
  },

  set_user_soundboard_volume: ({ userId, volume }) => {
    if (!(volume >= 0 && volume <= 2)) fail("Volume must be between 0% and 200%.");
    st.soundVolumes.set(userId, volume);
    if (st.voice) voiceChanged();
    return null;
  },

  user_volume: ({ userId }) => ({ volume: st.volumes.get(userId) ?? 1, soundboardVolume: st.soundVolumes.get(userId) ?? 1 }),

  room_soundboard_volume: ({ channelId }) => st.roomSoundVolumes.get(channelId) ?? 1,

  set_room_soundboard_volume: ({ channelId, volume }) => {
    if (!(volume >= 0 && volume <= 2)) fail("Volume must be between 0% and 200%.");
    st.roomSoundVolumes.set(channelId, volume);
    return null;
  },

  screen_sources: async () => {
    await sleep(200);
    const sources: ScreenSource[] = [
      { id: "display:0", kind: "display", name: "Display 1", width: 2560, height: 1440 },
      { id: "display:1", kind: "display", name: "Display 2", width: 1920, height: 1080 },
      { id: "window:1", kind: "window", name: "Steam", width: 1280, height: 800 },
      { id: "window:2", kind: "window", name: "Visual Studio Code", width: 1600, height: 1000 },
      { id: "window:3", kind: "window", name: "Spotify", width: 1200, height: 760 },
      { id: "window:4", kind: "window", name: "Notepad", width: 900, height: 640 },
    ];
    return sources;
  },

  screen_start: async ({ sourceId }) => {
    const me = meId();
    if (!st.voice) fail("not-in-voice");
    await sleep(300);
    if (FLAG_NOENC) fail("no-hardware-encoder");
    if (!sourceId) fail("Pick a screen or window to share.");
    const track = 9;
    st.screenTracks.set(me, track);
    voiceChanged();
    return track;
  },

  screen_stop: () => {
    const me = meId();
    if (st.screenTracks.delete(me)) voiceChanged();
    return null;
  },

  screen_watch: () => null,
  screen_unwatch: () => null,

  clips: () => st.clips,

  clip_add: async ({ path, name }) => {
    const n = name.trim() || stripExtension(baseName(path));
    if (!/\.(mp3|wav|ogg|flac|m4a)$/i.test(path)) {
      fail(`${baseName(path)} isn't a sound Tincan can play. Pick an MP3, WAV, OGG, FLAC or M4A file.`);
    }
    await sleep(400);
    const hash = hexId(`clip:${path}:${Date.now()}`);
    const clip: Clip = { hash, name: n, durationMs: 2300, size: 74_000, owned: true, heardFrom: null, transfer: null };
    st.clips.push(clip);
    emit("clips-updated", st.clips);
    return clip;
  },

  clip_remove: ({ hash }) => {
    st.clips = st.clips.filter((c) => c.hash !== hash);
    emit("clips-updated", st.clips);
    return null;
  },

  clip_rename: ({ hash, name }) => {
    const c = st.clips.find((x) => x.hash === hash) ?? fail("That sound was removed.");
    const n = name.trim();
    if (!n) fail("Enter a name for the sound.");
    c.name = n;
    emit("clips-updated", st.clips);
    return null;
  },

  clip_play: ({ hash }) => {
    const me = st.me ?? fail("Pick a display name first.");
    if (!st.voice) fail("not-in-voice");
    if (st.selfDeafened) fail("deafened");
    const c = st.clips.find((x) => x.hash === hash && x.owned) ?? fail("Save that sound before playing it.");
    const prev = st.playing.get(hash);
    if (prev) clearTimeout(prev);
    st.playing.set(
      hash,
      setTimeout(() => {
        st.playing.delete(hash);
        if (st.voice) voiceChanged();
      }, c.durationMs),
    );
    voiceChanged();
    emit("sound-played", { userId: me.userId, name: me.name, clip: c });
    return null;
  },

  clip_stop: ({ hash }) => {
    const t = st.playing.get(hash);
    if (t) {
      clearTimeout(t);
      st.playing.delete(hash);
      if (st.voice) voiceChanged();
    }
    return null;
  },

  clip_fetch: ({ hash }) => {
    const c = st.clips.find((x) => x.hash === hash) ?? fail("That sound is no longer available.");
    if (c.owned) return c;
    c.transfer = { hash, state: "downloading", have: 0, total: c.size, peers: c.transfer?.peers ?? 1 };
    emit("clips-updated", st.clips);
    ensureTransferTimer();
    return c;
  },

  settings_set: ({ settings }) => {
    const relay = settings.customRelayUrl?.trim() ?? "";
    if (relay && !/^https?:\/\/[^\s/]+/.test(relay)) fail("Enter a relay URL that starts with https://");
    const vol = (v: number) => (Number.isFinite(v) ? Math.min(2, Math.max(0, v)) : 1);
    st.settings = {
      ...settings,
      inputVolume: vol(settings.inputVolume),
      outputVolume: vol(settings.outputVolume),
      autoFetchLimit: Math.min(25 * MB, Math.max(0, Math.floor(settings.autoFetchLimit))),
      customRelayUrl: relay || null,
    };
    return st.settings;
  },

  audio_devices: (): AudioDevices => ({
    inputs: [
      { id: "in:default", name: "Microphone (Realtek High Definition Audio)", isDefault: true },
      { id: "in:hyperx", name: "Headset Microphone (HyperX Cloud II)", isDefault: false },
      { id: "in:yeti", name: "Microphone (Yeti Stereo Microphone)", isDefault: false },
    ],
    outputs: [
      { id: "out:default", name: "Speakers (Realtek High Definition Audio)", isDefault: true },
      { id: "out:hyperx", name: "Headphones (HyperX Cloud II)", isDefault: false },
      { id: "out:monitor", name: "LG ULTRAGEAR (NVIDIA High Definition Audio)", isDefault: false },
    ],
  }),

  mic_test: ({ on }) => {
    if (on && micTimer === null) micTimer = setInterval(micTick, 50);
    if (!on && micTimer !== null) {
      clearInterval(micTimer);
      micTimer = null;
      micLevel = 0;
    }
    return null;
  },

  window_minimize: () => null,
  window_toggle_maximize: () => null,
  window_close: () => null,
};

// ------------------------------------------------------------------ backend

/** Extra latency per command, set with tincanMock.delay(): before it runs, or after (`reply`). */
const delays = new Map<string, number>();
const replyDelays = new Map<string, number>();

/** The mock backend used outside Tauri. */
export const mockBackend: Backend = {
  async invoke(name, args) {
    const h = (handlers as Record<string, ((a: unknown) => unknown) | undefined>)[name];
    if (!h) throw `Unknown command ${name}`;
    await sleep(15 + (delays.get(name) ?? 0));
    try {
      const result = await h(structuredClone(args ?? {}));
      const copy = result === undefined ? null : structuredClone(result);
      // the command ran; its result reaches the UI later (events sent meanwhile arrive first)
      if (replyDelays.has(name)) await sleep(replyDelays.get(name) ?? 0);
      return copy;
    } catch (err) {
      // Tauri rejects with the command's error string, so do the same.
      throw err instanceof CmdError ? err.message : String(err);
    }
  },
  async listen(name, cb) {
    let set = listeners.get(name);
    if (!set) {
      set = new Set();
      listeners.set(name, set);
    }
    set.add(cb);
    return () => {
      set.delete(cb);
    };
  },
};

let pickCount = 0;

/** Fake file picker results for mock mode. */
export function fakePick(opts: { multiple: boolean; filters?: { name: string; extensions: string[] }[]; directory?: boolean }): string[] {
  pickCount++;
  if (opts.directory) return ["C:\\Users\\Sam\\Documents\\Tincan downloads"];
  const audio = opts.filters?.some((f) => f.extensions.includes("mp3")) ?? false;
  if (audio) {
    const sounds = ["vine-boom.mp3", "record-scratch.wav", "crickets.ogg", "ding.mp3"];
    return [`C:\\Users\\Sam\\Music\\Sound effects\\${sounds[pickCount % sounds.length]}`];
  }
  const files = [
    "C:\\Users\\Sam\\Pictures\\Screenshots\\lobby-setup.png",
    "C:\\Users\\Sam\\Documents\\tournament-bracket.pdf",
    "C:\\Users\\Sam\\Videos\\Captures\\ace-round.mp4",
  ];
  const first = files[pickCount % files.length];
  if (!opts.multiple) return [first];
  return pickCount % 2 === 0 ? [first, files[(pickCount + 1) % files.length]] : [first];
}

// ------------------------------------------------------------------ console helpers

declare global {
  interface Window {
    tincanMock?: {
      ring(name?: string): void;
      online(name: string): void;
      offline(name: string): void;
      migrate(): void;
      toast(text: string): void;
      /** What the mock backend thinks, for scripted checks. */
      state(): { voice: VoiceSession | null; call: CallInfo | null };
      /** Adds an online member named `name` to a server (default Game night), for name clashes. */
      addMember(name: string, server?: string): void;
      /** Plays `count` different clips nobody owns into my voice session. */
      soundFlood(count: number): void;
      /** Emits a snapshot, as the app does after a lag or a bulk catch-up. */
      snapshot(): void;
      /** Adds extra latency to one command (0 to clear), before it runs or, with `reply`, after. */
      delay(command: string, ms: number, reply?: boolean): void;
      /** Someone posts a message in a Game night channel. */
      post(name: string, text: string, channel?: string): void;
      /** Someone types once in a Game night channel. */
      typing(name: string, channel?: string): void;
      /** Finishes a file download without sending transfer-updated (a dropped event). */
      finishQuietly(fileName: string): void;
    };
  }
}

if (typeof window !== "undefined") {
  window.tincanMock = {
    ring: (name = "Alex") => {
      const p = allPeople.find((x) => x.name.toLowerCase() === name.toLowerCase());
      if (p) ringFrom(p);
    },
    online: (name) => setOnline(name, true),
    offline: (name) => setOnline(name, false),
    migrate: () => {
      const s = st.servers.find((x) => x.summary.name === "Game night");
      if (!s) return;
      for (const m of s.members) m.isHost = m.userId === people.morgan.id;
      s.summary.connection = { state: "member", hostId: people.morgan.id, hostName: "Morgan", path: "relay", rttMs: 71 };
      emit("server-updated", s.summary);
      emitMembers(s);
      toast("info", "Morgan is now hosting Game night");
    },
    toast: (text) => toast("info", text),
    state: () => structuredClone({ voice: st.voice, call: st.call }),
    addMember: (name, serverName = "Game night") => {
      const s = st.servers.find((x) => x.summary.name === serverName);
      if (!s) return;
      const p: Person = { id: userId(`extra:${name}:${idCounter++}`), name, online: true };
      allPeople.push(p);
      s.members.push(member(p));
      refreshOnline(s);
      emitMembers(s);
      emit("server-updated", s.summary);
    },
    snapshot: () => emit("snapshot", snapshot()),
    delay: (command, ms, reply = false) => {
      const map = reply ? replyDelays : delays;
      if (ms > 0) map.set(command, ms);
      else map.delete(command);
    },
    post: (name, text, channelName = "general") => {
      const s = st.servers.find((x) => x.summary.name === "Game night");
      const ch = s?.channels.find((c) => c.name === channelName);
      const p = allPeople.find((x) => x.name.toLowerCase() === name.toLowerCase());
      if (!s || !ch || !p) return;
      const msg: Message = {
        id: freshHex(),
        convId: s.summary.id,
        channelId: ch.id,
        authorId: p.id,
        authorName: p.name,
        text,
        attachments: [],
        sentAt: Date.now(),
        deleted: false,
        pending: false,
      };
      const key = convKey(s.summary.id, ch.id);
      const list = st.messages.get(key) ?? [];
      list.push(msg);
      st.messages.set(key, list);
      emit("message-added", msg);
      // unread like the app: the UI's mark_read clears it when the chat is on screen
      if (!ch.unread) {
        ch.unread = true;
        s.summary.unread = true;
        emitChannels(s);
        emit("server-updated", s.summary);
      }
    },
    typing: (name, channelName = "general") => {
      const s = st.servers.find((x) => x.summary.name === "Game night");
      const ch = s?.channels.find((c) => c.name === channelName);
      const p = allPeople.find((x) => x.name.toLowerCase() === name.toLowerCase());
      if (!s || !ch || !p) return;
      emit("typing", { convId: s.summary.id, channelId: ch.id, userId: p.id, name: p.name });
    },
    finishQuietly: (fileName) => {
      for (const [hash, f] of st.files) {
        if (f.name !== fileName) continue;
        const t = st.transfers.get(hash);
        if (t) Object.assign(t, { state: "complete", have: t.total });
      }
    },
    soundFlood: (count) => {
      const by = people.jordan;
      for (let i = 0; i < count; i++) {
        const hash = freshHex();
        const clip: Clip = { hash, name: `Clip ${i + 1}`, durationMs: 500, size: 9_000, owned: false, heardFrom: by.id, transfer: null };
        emit("sound-played", { userId: by.id, name: by.name, clip });
      }
    },
  };
  if (FLAG_RING) setTimeout(() => ringFrom(people.alex), 1500);
  if (!FLAG_FRESH) {
    ensureSpeakingTimer();
    // the seeded "downloading" attachment keeps progressing
    ensureTransferTimer();
  }
}
