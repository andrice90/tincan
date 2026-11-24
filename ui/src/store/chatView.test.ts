import type { Channel } from "../api/types";
import { eq, test } from "../test/harness";
import { roomChatClick, roomChatShown, roomOnScreen, shownChat } from "./chatView";
import type { View } from "./store";

const srv = "s1";
const general: Channel = { id: "g", serverId: srv, name: "general", kind: "text", position: 0, unread: false };
const lounge: Channel = { id: "l", serverId: srv, name: "Lounge", kind: "voice", position: 0, unread: true };
const den: Channel = { id: "d", serverId: srv, name: "Den", kind: "voice", position: 1, unread: false };
const channels = { [srv]: [general, lounge, den] };

const at = (channelId: string | null): View => ({ kind: "server", serverId: srv, channelId });

test("chat view: a text channel always shows its chat", () => {
  eq(shownChat(at("g"), channels, false), { convId: srv, channelId: "g" });
  eq(shownChat(at("g"), channels, true), { convId: srv, channelId: "g" });
});

test("read rule: a voice room's tiles alone show no chat, so it is not marked read", () => {
  eq(shownChat(at("l"), channels, false), null);
  eq(roomChatShown(at("l"), lounge, false), false);
});

test("read rule: a voice room's chat counts while its panel is open on that room", () => {
  eq(shownChat(at("l"), channels, true), { convId: srv, channelId: "l" });
  eq(roomChatShown(at("l"), lounge, true), true);
  // the panel flag outlives the room: on another room it shows that room's chat only
  eq(roomChatShown(at("d"), lounge, true), false, "another room on screen");
  eq(shownChat(at("d"), channels, true), { convId: srv, channelId: "d" });
});

test("chat view: the room's chat only applies while that room is on screen", () => {
  eq(roomChatShown(at("g"), lounge, true), false, "another channel");
  eq(roomChatShown({ kind: "server", serverId: "other", channelId: "l" }, lounge, true), false, "another server");
  eq(roomChatShown({ kind: "home" }, lounge, true), false, "home");
  eq(roomChatShown({ kind: "dms", convId: "x" }, lounge, true), false, "DMs");
  eq(roomOnScreen(at("g"), general), false, "text channels are not rooms");
});

test("chat button: on the room on screen it toggles the panel, like the header's Chat button", () => {
  eq(roomChatClick(at("l"), lounge, false), { open: true, navigate: false });
  eq(roomChatClick(at("l"), lounge, true), { open: false, navigate: false });
});

test("chat button: on another room or page it opens that room with the panel open", () => {
  eq(roomChatClick(at("d"), lounge, true), { open: true, navigate: true }, "from another room, panel open");
  eq(roomChatClick(at("d"), lounge, false), { open: true, navigate: true }, "from another room, panel closed");
  eq(roomChatClick(at("g"), lounge, false), { open: true, navigate: true }, "from a text channel");
  eq(roomChatClick({ kind: "dms", convId: null }, lounge, true), { open: true, navigate: true }, "from DMs");
});

test("chat view: DMs, home and unknown channels", () => {
  eq(shownChat({ kind: "dms", convId: "d1" }, channels, false), { convId: "d1", channelId: null });
  eq(shownChat({ kind: "dms", convId: null }, channels, false), null);
  eq(shownChat({ kind: "home" }, channels, true), null);
  eq(shownChat(at(null), channels, true), null);
  eq(shownChat(at("gone"), channels, true), null);
  eq(shownChat(at("l"), {}, true), null, "channels not loaded yet");
});
