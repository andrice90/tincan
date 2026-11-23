// Runs the UI's pure-logic tests without any dependency:
//   node --experimental-strip-types --no-warnings src/test/run.mjs   (from ui/, Node 22.6+)
// Imports inside src are extensionless (Vite style), so a resolve hook retries them with ".ts".

import { readdirSync } from "node:fs";
import { register } from "node:module";
import { join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

register(
  "data:text/javascript," +
    encodeURIComponent(`
export async function resolve(spec, ctx, next) {
  try {
    return await next(spec, ctx);
  } catch (err) {
    if ((spec.startsWith("./") || spec.startsWith("../")) && !/\\.[cm]?[jt]sx?$/.test(spec)) return next(spec + ".ts", ctx);
    throw err;
  }
}`),
);

const src = fileURLToPath(new URL("..", import.meta.url));
const files = readdirSync(src, { recursive: true })
  .map(String)
  .filter((f) => f.endsWith(".test.ts"))
  .sort();
for (const f of files) await import(pathToFileURL(join(src, f)).href);
const { runAll } = await import(pathToFileURL(join(src, "test", "harness.ts")).href);
const failed = await runAll((line) => console.log(line));
process.exitCode = failed === 0 ? 0 : 1;
