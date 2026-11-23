import { eq, test } from "../test/harness";
import { buildNameIndex, nameKey, sharesName, shortId } from "./names";

const bob = "bobbobbo" + "x".repeat(44);
const mallory = "mallory1" + "y".repeat(44);
const alice = "alicealc" + "z".repeat(44);

/** True when `fake` clashes with `real` (both directions: both get the short id). */
function clash(real: string, fake: string): boolean {
  const index = buildNameIndex([
    { userId: bob, name: real },
    { userId: mallory, name: fake },
  ]);
  const a = sharesName(index, bob, real);
  const b = sharesName(index, mallory, fake);
  if (a !== b) throw new Error(`one-sided clash for ${JSON.stringify(real)} / ${JSON.stringify(fake)}`);
  return a;
}

test("SECURITY-4: the short id matches the app's fallback name (first 8 characters)", () => {
  eq(shortId(bob), "bobbobbo");
});

test("SECURITY-4: two people with the same name both get told apart", () => {
  const index = buildNameIndex([
    { userId: bob, name: "Bob" },
    { userId: alice, name: "Alice" },
    { userId: mallory, name: "Bob" },
  ]);
  eq(sharesName(index, bob, "Bob"), true, "real Bob");
  eq(sharesName(index, mallory, "Bob"), true, "impostor");
  eq(sharesName(index, alice, "Alice"), false, "Alice");
});

test("SECURITY-4: the same person seen twice (server member and DM contact) is not a clash", () => {
  const index = buildNameIndex([
    { userId: bob, name: "Bob" },
    { userId: bob, name: "Bob" },
  ]);
  eq(sharesName(index, bob, "Bob"), false);
});

test("SECURITY-4: case, width, spacing and invisible characters do not hide a clash", () => {
  eq(clash("Bob", " BOB\u200B"), true, "zero-width space, capitals, leading space");
  eq(clash("Bob", "\uFF22ob"), true, "fullwidth B");
  eq(clash("Bob", "B\u202Eob"), true, "bidi override");
  eq(clash("Bob", "Bob\u3164"), true, "Hangul filler (renders blank)");
  eq(clash("Bob", "Bo\uFE0Fb"), true, "variation selector");
  eq(clash("Bob", "B\u00ADob"), true, "soft hyphen");
  eq(clash("Bob Smith", "Bob\u00A0\u00A0Smith"), true, "no-break spaces");
  eq(clash("Bob", "\u{1D401}ob"), true, "mathematical bold B");
});

test("SECURITY-4: letters of other scripts that look Latin clash with the Latin name", () => {
  eq(clash("Alice", "\u0410lice"), true, "Cyrillic capital A");
  eq(clash("Alice", "\u0430lic\u0435"), true, "Cyrillic small a and e");
  eq(clash("BOB", "\u0412\u041E\u0412"), true, "all-Cyrillic BOB");
  eq(clash("Bob", "\u0412\u043Eb"), true, "Cyrillic capital Ve and small o");
  eq(clash("BOB", "\u0392\u039F\u0392"), true, "Greek capitals");
  eq(clash("Hank", "\u0397ank"), true, "Greek capital eta is H");
  eq(clash("nate", "\u03B7ate"), true, "Greek small eta is n");
  eq(clash("Pam", "\u0420\u0430m"), true, "Cyrillic Er and a");
  eq(clash("Joy", "\u0408o\u0443"), true, "Cyrillic Je and u");
  eq(clash("Sam", "\u0405am"), true, "Cyrillic Dze");
  eq(clash("Ivo", "\u0406vo"), true, "Cyrillic I");
  eq(clash("hope", "\u04BBope"), true, "Cyrillic shha");
  eq(clash("Oscar", "\u0555scar"), true, "Armenian capital oh");
});

test("SECURITY-4: Latin lookalikes clash: 0 and o, 1 l i I and |", () => {
  eq(clash("Bob", "B0b"), true, "zero for o");
  eq(clash("Alice", "AIice"), true, "capital I for l");
  eq(clash("Alice", "A1ice"), true, "one for l");
  eq(clash("Alice", "A|ice"), true, "bar for l");
  eq(clash("Will", "WiII"), true, "capital I for l");
  eq(clash("Dmitri", "Dm\u0131tr\u0131"), true, "dotless i");
  eq(clash("Lars", "\u0141ars"), true, "L with stroke");
});

test("SECURITY-4: accents and overlays do not hide a clash", () => {
  eq(clash("Bob", "B\u00F3b"), true, "precomposed o acute");
  eq(clash("Bob", "Bo\u0301b"), true, "combining acute");
  eq(clash("Bob", "Bo\u0338b"), true, "combining overlay");
  eq(clash("Zoe", "Zo\u00EB"), true, "diaeresis");
});

test("SECURITY-4: different names stay apart", () => {
  eq(clash("Bob", "Rob"), false);
  eq(clash("Alice", "Alicia"), false);
  eq(clash("Sam", "Pam"), false);
  eq(clash("Bob", "Bob2"), false);
  // the mock backend's people: no short id shows up in the default demo
  const names = ["Sam", "Alex", "Jordan", "Priya", "Morgan", "Riley", "Casey", "Taylor"];
  const keys = new Set(names.map(nameKey));
  eq(keys.size, names.length, "demo names all distinct");
});

test("SECURITY-4: a name used by someone not in the index still counts", () => {
  // an author who left the server, writing under a current member's name
  const index = buildNameIndex([{ userId: bob, name: "Bob" }]);
  eq(sharesName(index, mallory, "Bob"), true);
  eq(sharesName(index, mallory, "\u0412\u043Eb"), true, "and with a lookalike");
});
