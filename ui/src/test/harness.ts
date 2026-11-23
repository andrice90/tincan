// A tiny test registry for the pure UI logic. No dependencies: `run.mjs` loads the
// `*.test.ts` files with Node's type stripping and runs what they registered.
// Tests import only modules that are plain TypeScript with type-only imports.

interface Case {
  name: string;
  fn: () => void | Promise<void>;
}

const cases: Case[] = [];

/** Registers a test case. */
export function test(name: string, fn: () => void | Promise<void>): void {
  cases.push({ name, fn });
}

function show(v: unknown): string {
  return JSON.stringify(v, (_k, x: unknown) => (x instanceof Set ? [...x] : x instanceof Map ? [...x] : x));
}

/** Fails unless `actual` and `expected` serialise to the same JSON. */
export function eq(actual: unknown, expected: unknown, what = "value"): void {
  const a = show(actual);
  const e = show(expected);
  if (a !== e) throw new Error(`${what}: expected ${e}, got ${a}`);
}

/** Fails unless `cond` is true. */
export function ok(cond: boolean, what: string): void {
  if (!cond) throw new Error(what);
}

/** Runs every registered case in order. Returns the number of failures. */
export async function runAll(log: (line: string) => void): Promise<number> {
  let failed = 0;
  for (const c of cases) {
    try {
      await c.fn();
      log(`ok    ${c.name}`);
    } catch (err) {
      failed++;
      log(`FAIL  ${c.name}\n      ${err instanceof Error ? err.message : String(err)}`);
    }
  }
  log(`${cases.length - failed} passed, ${failed} failed`);
  return failed;
}
