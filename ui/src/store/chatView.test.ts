import type { Channel } from "../api/types";
import { eq, test } from "../test/harness";
import { roomChatMode, shownChat } from "./chatView";
import type { View } from "./store";

const srv = "s1";
const general: Channel = { id: "g", serverId: srv, name: "general", kind: "text", position: 0, unread: false };
const lounge: Channel = { id: "l", serverId: srv, name: "Lounge", kind: "voice", position: 0, unread: true };
const channels = { [srv]: [general, lounge] };

const at = (channelId: string | null, chat?: boolean): View => ({ kind: "server", serverId: srv, channelId, chat });

test("chat view: a text channel always shows its chat", () => {
  eq(shownChat(at("g"), channels, false), { convId: srv, channelId: "g" });
  eq(shownChat(at("g"), channels, true), { convId: srv, channelId: "g" });
});

test("chat view: a voice room's tiles alone show no chat", () => {
  eq(shownChat(at("l"), channels, false), null);
  eq(roomChatMode(at("l"), lounge, false), null);
});

test("chat view: a voice room's chat page and panel both show its chat", () => {
  eq(shownChat(at("l", true), channels, false), { convId: srv, channelId: "l" });
  eq(roomChatMode(at("l", true), lounge, false), "page");
  eq(shownChat(at("l"), channels, true), { convId: srv, channelId: "l" });
  eq(roomChatMode(at("l"), lounge, true), "panel");
  // the page wins over an open panel
  eq(roomChatMode(at("l", true), lounge, true), "page");
});

test("chat view: the room's mode only applies while that room is on screen", () => {
  eq(roomChatMode(at("g"), lounge, true), null, "another channel");
  eq(roomChatMode({ kind: "server", serverId: "other", channelId: "l", chat: true }, lounge, true), null, "another server");
  eq(roomChatMode({ kind: "home" }, lounge, true), null, "home");
  eq(roomChatMode(at("g", true), general, true), null, "text channels have no room mode");
});

test("chat view: DMs, home and unknown channels", () => {
  eq(shownChat({ kind: "dms", convId: "d1" }, channels, false), { convId: "d1", channelId: null });
  eq(shownChat({ kind: "dms", convId: null }, channels, false), null);
  eq(shownChat({ kind: "home" }, channels, true), null);
  eq(shownChat(at(null), channels, true), null);
  eq(shownChat(at("gone", true), channels, true), null);
  eq(shownChat(at("l", true), {}, true), null, "channels not loaded yet");
});
