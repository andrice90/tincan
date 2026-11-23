// Telling people with the same display name apart. Display names are chosen by each peer
// and are not unique, so when two people the UI knows use the same name, both are shown
// with their short id. Pure, so it can be tested.

import type { UserId } from "../api/types";

/** Display name (compared loosely) to the users who use it. */
export type NameIndex = Map<string, Set<UserId>>;

/** The first 8 characters of the user id, as the app shows a person without a name. */
export function shortId(userId: UserId): string {
  return userId.slice(0, 8);
}

// zero-width, bidi control and other invisible format characters
const INVISIBLE = /[­؜᠎​-‏‪-‮⁠-⁯﻿]/g;

/** How two names are compared: width, case, spacing and invisible characters don't count. */
export function nameKey(name: string): string {
  return name.normalize("NFKC").replace(INVISIBLE, "").replace(/\s+/g, " ").trim().toLowerCase();
}

/** Builds the index from every person the UI knows (server members, DM contacts). */
export function buildNameIndex(people: Iterable<{ userId: UserId; name: string }>): NameIndex {
  const index: NameIndex = new Map();
  for (const p of people) {
    const key = nameKey(p.name);
    let set = index.get(key);
    if (!set) {
      set = new Set();
      index.set(key, set);
    }
    set.add(p.userId);
  }
  return index;
}

/** True when someone else uses the same display name as `userId`. */
export function sharesName(index: NameIndex, userId: UserId, name: string): boolean {
  const set = index.get(nameKey(name));
  return set !== undefined && (set.size > 1 || !set.has(userId));
}
