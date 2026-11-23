// Server list rules. Pure, so they can be tested.

import type { ServerSummary } from "../api/types";

/**
 * The server list after `create_server` or `join_server` returned `summary`. The result was
 * built before the command returned, and a `server-updated` that arrived while it ran is
 * newer (for a join, the history sync can finish first and bring the host's name). The
 * backend does not send that event again, so the result only adds a server the list does
 * not hold yet. It replaces one only when that is the copy of a server I was removed from
 * and the result is the re-join.
 */
export function withCreatedServer(servers: ServerSummary[], summary: ServerSummary): ServerSummary[] {
  const old = servers.find((x) => x.id === summary.id);
  if (!old) return [...servers, summary];
  if (old.removed && !summary.removed) return servers.map((x) => (x.id === summary.id ? summary : x));
  return servers;
}
