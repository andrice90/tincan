import type { ServerSummary } from "../api/types";
import { eq, test } from "../test/harness";
import { withCreatedServer } from "./servers";

function server(id: string, hostName: string | null): ServerSummary {
  return {
    id,
    name: "E2E Server",
    ownerId: "alice",
    isOwner: false,
    connection: { state: "member", hostId: "alice", hostName, path: "direct", rttMs: 1 },
    unread: false,
    onlineCount: 2,
    icon: null,
    removed: false,
  };
}

test("join_server's result never replaces a server-updated that came while it ran", () => {
  // the sync finished first: the event knows the host's name, the result did not yet
  const fromEvent = server("s1", "Alice");
  const list = withCreatedServer([fromEvent], server("s1", "w67ra3gh"));
  eq(list, [fromEvent]);
});

test("joining again replaces the copy of a server I was removed from", () => {
  const gone = { ...server("s1", null), removed: true };
  const back = server("s1", "Alice");
  eq(withCreatedServer([gone], back), [back]);
  // a removed copy never replaces a live entry
  eq(withCreatedServer([back], gone), [back]);
});

test("join_server's result adds a server the list does not hold yet", () => {
  const other = server("s0", "Bob");
  const joined = server("s1", "w67ra3gh");
  eq(withCreatedServer([other], joined), [other, joined]);
  eq(withCreatedServer([], joined), [joined]);
});
