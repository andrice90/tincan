import type { Channel, DmSummary, ServerSummary } from "../api/types";
import { eq, test } from "../test/harness";
import { composerSpot, rightPanel } from "./dock";

const server = { id: "s1", name: "Game night", removed: false } as ServerSummary;
const removed = { id: "s2", name: "Book club", removed: true } as ServerSummary;
const text: Channel = { id: "general", serverId: "s1", name: "general", kind: "text", position: 0, unread: false };
const room: Channel = { id: "lounge", serverId: "s1", name: "Lounge", kind: "voice", position: 1, unread: false };
const bookText: Channel = { id: "books", serverId: "s2", name: "books", kind: "text", position: 0, unread: false };
const channels = { s1: [text, room], s2: [bookText] };
const dm = { convId: "d1", userId: "u1", name: "Alex" } as DmSummary;
const servers = [server, removed];

test("dock: the composer goes with a text channel, a room's chat page and a DM", () => {
  const t = composerSpot({ kind: "server", serverId: "s1", channelId: "general" }, servers, channels, [dm]);
  eq(t && [t.convId, t.channelId, t.placeholder, t.removed], ["s1", "general", "Message #general", false]);
  const page = composerSpot({ kind: "server", serverId: "s1", channelId: "lounge", chat: true }, servers, channels, [dm]);
  eq(page && [page.channelId, page.placeholder], ["lounge", "Message Lounge"]);
  const d = composerSpot({ kind: "dms", convId: "d1" }, servers, channels, [dm]);
  eq(d && [d.convId, d.channelId, d.placeholder], ["d1", null, "Message Alex"]);
});

test("dock: no composer on a room's tiles, home, an empty DM list or a channel not loaded", () => {
  eq(composerSpot({ kind: "server", serverId: "s1", channelId: "lounge" }, servers, channels, [dm]), null);
  eq(composerSpot({ kind: "home" }, servers, channels, [dm]), null);
  eq(composerSpot({ kind: "dms", convId: null }, servers, channels, [dm]), null);
  eq(composerSpot({ kind: "dms", convId: "gone" }, servers, channels, [dm]), null);
  eq(composerSpot({ kind: "server", serverId: "s1", channelId: null }, servers, channels, [dm]), null);
  eq(composerSpot({ kind: "server", serverId: "s3", channelId: "x" }, servers, channels, [dm]), null);
});

test("dock: a removed server's composer becomes the removed note, and the sig tells them apart", () => {
  const r = composerSpot({ kind: "server", serverId: "s2", channelId: "books" }, servers, channels, []);
  eq(r?.removed, true);
  const a = composerSpot({ kind: "server", serverId: "s1", channelId: "general" }, servers, channels, []);
  const b = composerSpot({ kind: "server", serverId: "s1", channelId: "general" }, servers, channels, []);
  eq(a?.sig === b?.sig, true, "same channel, same sig");
  const renamed = composerSpot(
    { kind: "server", serverId: "s1", channelId: "general" },
    servers,
    { s1: [{ ...text, name: "lobby" }] },
    [],
  );
  eq(a?.sig === renamed?.sig, false, "a rename changes the placeholder");
});

test("dock: the member list goes with chat pages when shown", () => {
  const onText = rightPanel({ kind: "server", serverId: "s1", channelId: "general" }, channels, true, false, "classic");
  eq(onText?.kind, "members");
  eq(rightPanel({ kind: "server", serverId: "s1", channelId: "general" }, channels, false, false, "floating"), null);
  const page = rightPanel({ kind: "server", serverId: "s1", channelId: "lounge", chat: true }, channels, true, true, "floating");
  eq(page?.kind, "members", "a room's chat page shows members, not a second chat");
  eq(rightPanel({ kind: "dms", convId: "d1" }, channels, true, true, "floating"), null);
  eq(rightPanel({ kind: "home" }, channels, true, true, "floating"), null);
});

test("dock: a room's chat panel is the right card in Floating only", () => {
  const tiles = { kind: "server", serverId: "s1", channelId: "lounge" } as const;
  eq(rightPanel(tiles, channels, true, true, "classic"), null, "Classic keeps it under the room header");
  eq(rightPanel(tiles, channels, true, false, "floating"), null, "closed panel");
  const chat = rightPanel(tiles, channels, false, true, "floating");
  eq(chat && chat.kind === "room-chat" ? [chat.serverId, chat.channelId] : null, ["s1", "lounge"]);
  eq(rightPanel(tiles, channels, true, false, "classic"), null, "no members beside the tiles");
});
