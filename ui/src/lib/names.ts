// Telling people with the same display name apart. Display names are chosen by each peer
// and are not unique, so when two people the UI knows use names that look the same, both
// are shown with their short id. Pure, so it can be tested.

import type { UserId } from "../api/types";

/** Display name (compared loosely) to the users who use it. */
export type NameIndex = Map<string, Set<UserId>>;

/** The first 8 characters of the user id, as the app shows a person without a name. */
export function shortId(userId: UserId): string {
  return userId.slice(0, 8);
}

/** Zero-width, bidi control, variation selector, Hangul filler and other invisible characters. */
const INVISIBLE = /[\p{Default_Ignorable_Code_Point}\p{Cf}]/gu;

/** Combining marks: accents, overlays and enclosing marks on a base letter. */
const MARKS = /[\p{Mn}\p{Me}]/gu;

/**
 * Letters of other scripts that look like a Latin letter, after the UTS #39 confusables list
 * (a subset: Cyrillic, Greek, Armenian and Latin lookalikes). Mapped before lowercasing, so a
 * capital maps by its own shape (Greek capital eta is H, small eta is n).
 */
const LOOKALIKES = new Map<string, string>([
  // Cyrillic capitals
  ["\u0410", "A"], ["\u0412", "B"], ["\u0415", "E"], ["\u041A", "K"], ["\u041C", "M"],
  ["\u041D", "H"], ["\u041E", "O"], ["\u0420", "P"], ["\u0421", "C"], ["\u0422", "T"],
  ["\u0423", "Y"], ["\u0425", "X"], ["\u042C", "b"], ["\u0405", "S"], ["\u0406", "I"],
  ["\u0408", "J"], ["\u0474", "V"], ["\u04AE", "Y"], ["\u04BA", "H"], ["\u04C0", "I"],
  ["\u0500", "d"], ["\u051A", "Q"], ["\u051C", "W"],
  // Cyrillic small letters
  ["\u0430", "a"], ["\u0435", "e"], ["\u043E", "o"], ["\u0440", "p"], ["\u0441", "c"],
  ["\u0443", "y"], ["\u0445", "x"], ["\u0455", "s"], ["\u0456", "i"], ["\u0458", "j"],
  ["\u0475", "v"], ["\u04AF", "y"], ["\u04BB", "h"], ["\u04CF", "l"], ["\u0501", "d"],
  ["\u051B", "q"], ["\u051D", "w"],
  // Greek capitals
  ["\u0391", "A"], ["\u0392", "B"], ["\u0395", "E"], ["\u0396", "Z"], ["\u0397", "H"],
  ["\u0399", "I"], ["\u039A", "K"], ["\u039C", "M"], ["\u039D", "N"], ["\u039F", "O"],
  ["\u03A1", "P"], ["\u03A4", "T"], ["\u03A5", "Y"], ["\u03A7", "X"], ["\u03DC", "F"],
  ["\u03F9", "C"], ["\u037F", "J"],
  // Greek small letters
  ["\u03B1", "a"], ["\u03B3", "y"], ["\u03B7", "n"], ["\u03B9", "i"], ["\u03BD", "v"],
  ["\u03BF", "o"], ["\u03C1", "p"], ["\u03C5", "u"], ["\u03F2", "c"], ["\u03F3", "j"],
  // Armenian
  ["\u054D", "U"], ["\u054F", "S"], ["\u0555", "O"], ["\u0566", "q"], ["\u0570", "h"],
  ["\u0578", "n"], ["\u057D", "u"], ["\u0581", "g"], ["\u0585", "o"],
  // Latin letters that don't decompose to their lookalike
  ["\u00D0", "D"], ["\u00D8", "O"], ["\u00F8", "o"], ["\u0110", "D"], ["\u0111", "d"],
  ["\u0126", "H"], ["\u0127", "h"], ["\u0131", "i"], ["\u0141", "L"], ["\u0142", "l"],
  ["\u0166", "T"], ["\u0167", "t"], ["\u01C0", "l"], ["\u0237", "j"], ["\u0251", "a"],
  ["\u0261", "g"], ["\u0269", "i"],
]);

/** After lowercasing: digits and marks that pass for letters (0 and o; 1, l, i, I and |). */
const LATIN_FOLD = /[01i|]/g;

/** nameKey results, bounded: names come from peers. */
const keyCache = new Map<string, string>();
const KEY_CACHE_MAX = 2000;

/**
 * How two names are compared. Width, case, spacing, invisible characters and accents don't
 * count, and letters that look alike count as the same letter, so Alice with a Cyrillic A,
 * "AIice" with a capital I and "B0b" with a zero clash with "Alice" and "Bob". Folding too
 * much only shows an extra short id; folding too little lets a lookalike pass as someone.
 */
export function nameKey(name: string): string {
  const hit = keyCache.get(name);
  if (hit !== undefined) return hit;
  let s = name.normalize("NFKD").replace(INVISIBLE, "").replace(MARKS, "");
  let mapped = "";
  for (const ch of s) mapped += LOOKALIKES.get(ch) ?? ch;
  s = mapped
    .toLowerCase()
    .replace(LATIN_FOLD, (c) => (c === "0" ? "o" : "l"))
    .replace(/\s+/g, " ")
    .trim();
  if (keyCache.size >= KEY_CACHE_MAX) keyCache.clear();
  keyCache.set(name, s);
  return s;
}

/** Builds the index from every person the UI knows (server members, DM contacts, me). */
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

/** True when someone else uses a name that looks the same as `userId`'s. */
export function sharesName(index: NameIndex, userId: UserId, name: string): boolean {
  const set = index.get(nameKey(name));
  return set !== undefined && (set.size > 1 || !set.has(userId));
}
