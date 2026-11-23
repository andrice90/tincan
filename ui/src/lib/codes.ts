// Short codes (server invite code, friend code): 6 characters shown as ABC-DEF.
// Mirrors ShortCode::parse in crates/tincan-core/src/code.rs.

/** The code alphabet: no 0/O/1/I lookalikes. */
export const CODE_ALPHABET = "23456789ABCDEFGHJKLMNPQRSTUVWXYZ";
const CODE_LEN = 6;

/**
 * Typed or pasted input as a short code in "ABC-DEF" form, or null when it isn't one.
 * Any case; dashes and whitespace are ignored.
 */
export function parseShortCode(input: string): string | null {
  const s = input.replace(/[\s-]/g, "").toUpperCase();
  if (s.length !== CODE_LEN || [...s].some((ch) => !CODE_ALPHABET.includes(ch))) return null;
  return `${s.slice(0, 3)}-${s.slice(3)}`;
}

/** A random code in "ABC-DEF" form. */
export function randomShortCode(): string {
  const bytes = crypto.getRandomValues(new Uint8Array(CODE_LEN));
  const s = [...bytes].map((b) => CODE_ALPHABET[b % CODE_ALPHABET.length]).join("");
  return `${s.slice(0, 3)}-${s.slice(3)}`;
}
