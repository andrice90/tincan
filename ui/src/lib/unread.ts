// What an unread badge on a sidebar row shows. The contract carries an unread flag per
// channel and DM, not a count, so today every badge is a dot; a count is shown when the
// data has one.

/** A dot, or the count as text ("99+" past 99). */
export type BadgeContent = { kind: "dot" } | { kind: "count"; text: string };

/** Largest count shown as a number. */
export const BADGE_MAX = 99;

/**
 * The badge for a row, or null for none: nothing when the row is read or is the one on
 * screen, a count when one is known and positive, else a dot.
 */
export function unreadBadge(unread: boolean, current: boolean, count?: number | null): BadgeContent | null {
  if (!unread || current) return null;
  if (count === undefined || count === null || !Number.isFinite(count) || count < 1) return { kind: "dot" };
  const n = Math.floor(count);
  return { kind: "count", text: n > BADGE_MAX ? `${BADGE_MAX}+` : String(n) };
}

/** The screen reader text after the row's name. */
export function unreadLabel(badge: BadgeContent): string {
  if (badge.kind === "dot") return ", unread messages";
  return badge.text === "1" ? ", 1 unread message" : `, ${badge.text} unread messages`;
}
