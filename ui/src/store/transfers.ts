// Transfer state rules. Pure, so they can be tested.

import type { Transfer } from "../api/types";

/**
 * True when a command's transfer result (download, transfer) may replace what the store
 * holds: no transfer-updated arrived while the command was in flight (the store still holds
 * `before`), and a complete file never goes back to an unfinished state.
 */
export function takeTransfer(current: Transfer | undefined, before: Transfer | undefined, next: Transfer): boolean {
  if (current !== before) return false;
  return !(current?.state === "complete" && next.state !== "complete");
}

/** Transfers to read again after a snapshot (their final event may have been dropped). */
export function staleTransfers(entries: Iterable<[string, Transfer]>): string[] {
  const out: string[] = [];
  for (const [hash, t] of entries) if (t.state !== "complete") out.push(hash);
  return out;
}
