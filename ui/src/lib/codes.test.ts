import { eq, test } from "../test/harness";
import { parseShortCode, randomShortCode } from "./codes";

test("short code: any case, with or without the dash or spaces", () => {
  eq(parseShortCode("K7Q-M2X"), "K7Q-M2X");
  eq(parseShortCode("k7qm2x"), "K7Q-M2X");
  eq(parseShortCode(" k7q m2x "), "K7Q-M2X");
  eq(parseShortCode("K-7-Q-M-2-X"), "K7Q-M2X");
});

test("short code: wrong length or lookalike characters are not codes", () => {
  eq(parseShortCode("K7Q-M2"), null);
  eq(parseShortCode("K7Q-M2XY"), null);
  eq(parseShortCode("K0Q-M1X"), null);
  eq(parseShortCode("KOQ-MIX"), null);
  eq(parseShortCode("tincan1abcdefghijklmnopqrstuvwxyz"), null);
  eq(parseShortCode(""), null);
});

test("short code: random codes parse back to themselves", () => {
  for (let i = 0; i < 50; i++) {
    const c = randomShortCode();
    eq(parseShortCode(c), c);
  }
});
