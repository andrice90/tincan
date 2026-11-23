import { eq, test } from "../test/harness";
import { buildNameIndex, sharesName, shortId } from "./names";

const bob = "bobbobbo" + "x".repeat(44);
const mallory = "mallory1" + "y".repeat(44);
const alice = "alicealc" + "z".repeat(44);

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
  const index = buildNameIndex([
    { userId: bob, name: "Bob" },
    { userId: mallory, name: " BOB​" },
  ]);
  eq(sharesName(index, bob, "Bob"), true, "real Bob");
  const wide = buildNameIndex([
    { userId: bob, name: "Bob" },
    { userId: mallory, name: "Ｂob" },
  ]);
  eq(sharesName(wide, mallory, "Ｂob"), true, "fullwidth B");
});

test("SECURITY-4: a name used by someone not in the index still counts", () => {
  // an author who left the server, writing under a current member's name
  const index = buildNameIndex([{ userId: bob, name: "Bob" }]);
  eq(sharesName(index, mallory, "Bob"), true);
});
