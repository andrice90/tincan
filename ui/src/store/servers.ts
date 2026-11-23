// Server list rules. Pure, so they can be tested.

import type { ServerSummary } from "../api/types";

/**
 * The server list after `create_server` or `join_server` returned `summary`. The result was
 * built before the command returned, and a `server-updated` that arrived while it ran is
 * newer (for a join, the history sync can finish first and bring the host's name). The
 * backend does not send that event again, so the result only adds a server the list does
 * not hold yet; it never replaces one.
 */
export function withCreatedServer(servers: ServerSummary[], summary: ServerSummary): ServerSummary[] {
  return servers.some((x) => x.id === summary.id) ? servers : [...servers, summary];
}
