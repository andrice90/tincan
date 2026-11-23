// Shared helpers for the Tincan two-profile end-to-end run: launching instances with
// WebView2 remote debugging, connecting Playwright over CDP, screenshots, waits and
// process statistics. Everything the run does to an instance goes through its UI.

import { spawn, execFile } from "node:child_process";
import { createRequire } from "node:module";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
// Inside the repository (ui/e2e) the repo root is two levels up.
const inRepo = fs.existsSync(path.join(here, "..", "..", "Cargo.toml"));

// playwright-core is not a ui/ dependency: TINCAN_E2E_DEPS names a folder whose
// node_modules has it (npm i playwright-core in any scratch folder).
const require = createRequire(path.join(process.env.TINCAN_E2E_DEPS || here, "package.json"));
let pw;
try {
  pw = require("playwright-core");
} catch {
  console.error("playwright-core not found. Run `npm i playwright-core` in a scratch folder and set TINCAN_E2E_DEPS to it.");
  process.exit(2);
}
export const { chromium } = pw;

export const REPO = process.env.TINCAN_REPO || (inRepo ? path.resolve(here, "..", "..") : "D:/code-projects/tincan");
export const EXE = process.env.TINCAN_EXE || path.join(REPO, "target", "a-int", "debug", "tincan.exe");
/** Logs, screenshots, generated media and results.json (never inside the repository). */
export const OUT = process.env.TINCAN_E2E_OUT || (inRepo ? path.join(os.tmpdir(), "tincan-e2e") : here);
export const SHOTS = path.join(OUT, "shots");
export const PROFILES = path.join(process.env.APPDATA, "Tincan", "profiles");

export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
export const now = () => performance.now();

let t0 = now();
/** Timestamped log line. */
export function log(...args) {
  const s = ((now() - t0) / 1000).toFixed(2).padStart(7);
  console.log(`[${s}]`, ...args);
}

/** Removes a test profile folder. Only names starting with e2e- are accepted. */
export function wipeProfile(name) {
  if (!/^e2e-[a-z0-9-]+$/.test(name)) throw new Error(`refusing to delete profile ${name}`);
  fs.rmSync(path.join(PROFILES, name), { recursive: true, force: true });
}

async function cdpReady(port, ms) {
  const end = now() + ms;
  while (now() < end) {
    try {
      const r = await fetch(`http://127.0.0.1:${port}/json/version`);
      if (r.ok) return true;
    } catch {
      // not up yet
    }
    await sleep(150);
  }
  return false;
}

/**
 * Starts `tincan.exe --profile <name>` with remote debugging on `port`, connects over CDP
 * and returns { name, port, proc, pid, browser, page, logs }.
 */
export async function launch(name, port, { offline = true } = {}) {
  fs.mkdirSync(path.join(OUT, "logs"), { recursive: true });
  const logFile = path.join(OUT, "logs", `${name}-${Date.now()}.log`);
  const fd = fs.openSync(logFile, "a");
  const env = {
    ...process.env,
    WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${port}`,
    RUST_BACKTRACE: "1",
    // warnings from every crate plus audio device open/close, into logs/<profile>-<ms>.log
    TINCAN_LOG: process.env.TINCAN_LOG ?? "warn,tincan_audio=info",
  };
  if (offline) env.TINCAN_OFFLINE_NETWORK = "1";
  else delete env.TINCAN_OFFLINE_NETWORK;
  delete env.TINCAN_NO_DEVICES;
  delete env.TINCAN_PROFILE;
  const started = now();
  // detached: no console window for the debug build, and no job object tying it to node
  const proc = spawn(EXE, ["--profile", name], { env, stdio: ["ignore", fd, fd], detached: true });
  fs.closeSync(fd);
  const exited = new Promise((r) => proc.once("exit", (code) => r(code)));
  if (!(await cdpReady(port, 30000))) throw new Error(`${name}: no CDP on port ${port}`);
  const browser = await chromium.connectOverCDP(`http://127.0.0.1:${port}`);
  let page = null;
  const end = now() + 20000;
  while (!page && now() < end) {
    for (const ctx of browser.contexts()) {
      for (const p of ctx.pages()) if (/^http:\/\/tauri\.localhost/.test(p.url())) page = p;
    }
    if (!page) await sleep(100);
  }
  if (!page) throw new Error(`${name}: no Tincan page`);
  const inst = { name, port, proc, pid: proc.pid, browser, page, logFile, exited, console: [], started };
  page.on("console", (m) => {
    const line = `${m.type()}: ${m.text()}`;
    inst.console.push(line);
    if (m.type() === "error" || m.type() === "warning") log(`${name} console`, line);
  });
  page.on("pageerror", (e) => {
    inst.console.push(`pageerror: ${e.message}`);
    log(`${name} pageerror`, e.message);
  });
  await page.waitForLoadState("domcontentloaded");
  return inst;
}

/** CDP screenshot of the instance's webview only (never the desktop). */
export async function shot(inst, label) {
  fs.mkdirSync(SHOTS, { recursive: true });
  const file = path.join(SHOTS, `${label}-${inst.name}.png`);
  await inst.page.screenshot({ path: file });
  return file;
}

/** Waits until `fn` (run in the page) returns truthy; returns the elapsed ms. */
export async function waitPage(inst, fn, arg, { timeout = 15000, what = "condition" } = {}) {
  const start = now();
  try {
    await inst.page.waitForFunction(fn, arg, { timeout, polling: 50 });
  } catch (e) {
    throw new Error(`${inst.name}: timed out after ${timeout} ms waiting for ${what}`);
  }
  return now() - start;
}

/** Waits until some element matching `selector` contains `text`. */
export function waitText(inst, selector, text, opts = {}) {
  return waitPage(
    inst,
    ([sel, t]) => [...document.querySelectorAll(sel)].some((e) => e.textContent.includes(t)),
    [selector, text],
    { what: `"${text}" in ${selector}`, ...opts },
  );
}

/** Runs a PowerShell script file with arguments; resolves to stdout. */
export function powershell(file, args = [], { timeout = 60000 } = {}) {
  return new Promise((resolve, reject) => {
    execFile(
      "powershell.exe",
      ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", file, ...args],
      { timeout, windowsHide: true },
      (err, stdout, stderr) => (err ? reject(new Error(`${path.basename(file)}: ${err.message} ${stdout} ${stderr}`)) : resolve(stdout.trim())),
    );
  });
}

/** Runs a PowerShell command string; resolves to stdout. */
export function psCommand(command, { timeout = 60000 } = {}) {
  return new Promise((resolve, reject) => {
    execFile("powershell.exe", ["-NoProfile", "-Command", command], { timeout, windowsHide: true, maxBuffer: 8 << 20 }, (err, stdout, stderr) =>
      err ? reject(new Error(`${err.message} ${stderr}`)) : resolve(stdout.trim()),
    );
  });
}

/** Fills the native Open dialog of `pid` with `file` (Win32 messages, see pick.ps1). */
export function answerFileDialog(pid, file) {
  return powershell(path.join(here, "pick.ps1"), ["-ProcId", String(pid), "-Path", file]);
}

// One sample of every process: parent links and the WebView2 role from Win32_Process,
// memory, threads and CPU time from Get-Process.
const SAMPLE_PS = `
$cim = @{}
Get-CimInstance Win32_Process | ForEach-Object { $cim[[int]$_.ProcessId] = $_ }
Get-Process | ForEach-Object {
  $c = $cim[$_.Id]
  $role = ''
  if ($c -and $c.CommandLine -match '--type=([a-z-]+)') { $role = $Matches[1] }
  elseif ($c -and $_.ProcessName -eq 'msedgewebview2') { $role = 'browser' }
  [pscustomobject]@{
    ProcessId = $_.Id
    ParentProcessId = if ($c) { [int]$c.ParentProcessId } else { 0 }
    Name = $_.ProcessName
    Role = $role
    WorkingSet = $_.WorkingSet64
    Private = $_.PrivateMemorySize64
    Threads = $_.Threads.Count
    CpuMs = try { $_.TotalProcessorTime.TotalMilliseconds } catch { 0 }
  }
} | ConvertTo-Json -Compress`;

/**
 * Process statistics for a tincan.exe and every process below it (WebView2), from two
 * samples `ms` apart: working set, private bytes, threads and CPU% of one core.
 */
export async function processStats(rootPid, ms = 10000) {
  const sample = async () => JSON.parse(await psCommand(SAMPLE_PS));
  const tree = (list) => {
    const keep = new Map();
    const want = new Set([rootPid]);
    let grew = true;
    while (grew) {
      grew = false;
      for (const p of list) {
        if (!want.has(p.ProcessId) && want.has(p.ParentProcessId)) {
          want.add(p.ProcessId);
          grew = true;
        }
      }
    }
    for (const p of list) if (want.has(p.ProcessId)) keep.set(p.ProcessId, p);
    return keep;
  };
  const a = tree(await sample());
  const ta = now();
  await sleep(ms);
  const b = tree(await sample());
  const dt = (now() - ta) / 1000;
  const rows = [];
  for (const [pid, p] of b) {
    const prev = a.get(pid);
    rows.push({
      pid,
      parent: p.ParentProcessId,
      name: p.Name,
      role: p.Role || (p.Name === "tincan" ? "app" : ""),
      workingSetMB: +(Number(p.WorkingSet) / 1048576).toFixed(1),
      privateMB: +(Number(p.Private) / 1048576).toFixed(1),
      threads: p.Threads,
      cpuPct: prev ? +(((Number(p.CpuMs) - Number(prev.CpuMs)) / 1000 / dt) * 100).toFixed(2) : null,
    });
  }
  return rows;
}
