// Tincan two-profile end-to-end run, driven only through the two UIs over CDP.
//
//   node e2e.mjs [--keep] [--online]
//
// Starts tincan.exe --profile e2e-a (CDP 9301) and --profile e2e-b (CDP 9302) with
// TINCAN_OFFLINE_NETWORK=1 (loopback only, no relays, no firewall prompt; --online drops it),
// then runs flows a-f from docs/e2e.md, the idle resource check, and closes everything.
// --keep leaves the instances open when a step fails. See docs/e2e.md for the environment.
// The one step that is not a webview action: the native Open dialog (attachment, sound) is
// answered with Win32 messages (pick.ps1), because CDP cannot reach it.

import { spawn } from "node:child_process";
import fs from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { decodePng, makePng, makeWav, meanColor, near } from "./media.mjs";
import {
  OUT,
  REPO,
  answerFileDialog,
  launch,
  log,
  now,
  processStats,
  psCommand,
  shot,
  sleep,
  waitPage,
  waitText,
  wipeProfile,
} from "./lib.mjs";

const args = new Set(process.argv.slice(2));
const KEEP = args.has("--keep");
const OFFLINE = !args.has("--online");
const PORTS = { "e2e-a": 9301, "e2e-b": 9302 };
const NAMES = { "e2e-a": "Alice", "e2e-b": "Bob" };
const SERVER = "E2E Server";
const PATTERN_TITLE = "tincan-e2e-pattern";

const results = { started: new Date().toISOString(), offline: OFFLINE, timings: {}, checks: [], resources: null };
const media = path.join(OUT, "media");
const children = new Set();
const logFiles = [];

function check(name, ok, detail = "") {
  results.checks.push({ name, ok, detail });
  log(ok ? "PASS" : "FAIL", name, detail);
  if (!ok) throw new Error(`check failed: ${name} ${detail}`);
}

function timing(name, ms) {
  results.timings[name] = Math.round(ms);
  log("time", name, `${Math.round(ms)} ms`);
}

function save() {
  fs.writeFileSync(path.join(OUT, "results.json"), JSON.stringify(results, null, 2));
}

// ------------------------------------------------------------------ UI helpers

/** The dialog currently open in a page. */
const dlg = (inst) => inst.page.locator("dialog[open]");

async function clickButton(inst, name, scope = inst.page) {
  await scope.getByRole("button", { name, exact: true }).first().click();
}

async function errorToasts(inst) {
  return inst.page.$$eval(".toast.is-error", (els) => els.map((e) => e.textContent.trim()));
}

async function noErrorToast(inst, step) {
  const errs = await errorToasts(inst);
  check(`${step}: no error toast on ${inst.name}`, errs.length === 0, errs.join(" | "));
}

async function sideSub(inst) {
  return inst.page.$eval(".side-header .side-sub", (e) => e.textContent.trim()).catch(() => "");
}

/** Types into the composer one key at a time (sends typing) and returns before Enter. */
async function typeInComposer(inst, text, delay = 35) {
  const box = inst.page.locator("textarea.composer-input");
  await box.click();
  await box.pressSequentially(text, { delay });
}

/**
 * Sends `text` from `from` and returns the UI-to-UI latency: from the Enter keydown in the
 * sender's page to the DOM insertion in the receiver's page, both stamped in the pages
 * (performance.timeOrigin + now(): one system clock, both processes on this machine).
 */
async function sendAndSee(from, to, text) {
  await typeInComposer(from, text, 5);
  await to.page.evaluate((t) => {
    const has = () => [...document.querySelectorAll(".msg-text")].some((e) => e.textContent.includes(t));
    window.__e2eSeen = new Promise((resolve) => {
      const mo = new MutationObserver(() => {
        if (has()) {
          mo.disconnect();
          resolve(performance.timeOrigin + performance.now());
        }
      });
      mo.observe(document.body, { childList: true, subtree: true, characterData: true });
    });
  }, text);
  await from.page.evaluate(() => {
    window.__e2eSent = null;
    document.querySelector("textarea.composer-input").addEventListener(
      "keydown",
      (e) => {
        if (e.key === "Enter") window.__e2eSent = performance.timeOrigin + performance.now();
      },
      { once: true, capture: true },
    );
  });
  await from.page.locator("textarea.composer-input").press("Enter");
  const seen = await Promise.race([to.page.evaluate(() => window.__e2eSeen), sleep(15000).then(() => null)]);
  if (seen === null) throw new Error(`${to.name}: "${text}" did not arrive within 15 s`);
  const sent = await from.page.evaluate(() => window.__e2eSent);
  return seen - sent;
}

async function openServer(inst) {
  await inst.page.locator(`nav.rail button[title="${SERVER}"]`).click();
  await waitText(inst, ".side-header .side-title", SERVER);
}

async function openChannel(inst, name) {
  await inst.page.locator("button.channel-row", { hasText: name }).first().click();
}

async function membersOnline(inst) {
  return inst.page.$$eval("aside.members .member-group:not(.is-offline) .member-name", (els) => els.map((e) => e.textContent.trim()));
}

/** Clicks a button that opens the native Open dialog and answers it with `file`. */
async function pickWith(inst, clickFn, file) {
  const answered = answerFileDialog(inst.pid, file);
  await clickFn();
  const out = await answered;
  log(inst.name, "file dialog", out);
  check(`native Open dialog answered on ${inst.name}`, out.startsWith("OK"), out);
}

// ------------------------------------------------------------------ setup

function makeMedia() {
  fs.mkdirSync(media, { recursive: true });
  const png = path.join(media, "e2e-attach.png");
  // four vertical stripes: red, green, blue, white
  const stripes = [
    [220, 40, 40],
    [40, 200, 80],
    [40, 80, 220],
    [240, 240, 240],
  ];
  fs.writeFileSync(png, makePng(96, 64, (x) => stripes[Math.floor(x / 24)]));
  const wav = path.join(media, "e2e-chime.wav");
  fs.writeFileSync(wav, makeWav({ seconds: 2, amplitude: 0.05 }));
  return { png: png.replaceAll("/", "\\"), wav: wav.replaceAll("/", "\\") };
}

function startPattern() {
  const ps1 = path.join(path.dirname(fileURLToPath(import.meta.url)), "pattern.ps1");
  const proc = spawn("powershell.exe", ["-NoProfile", "-ExecutionPolicy", "Bypass", "-STA", "-WindowStyle", "Hidden", "-File", ps1], {
    stdio: "ignore",
  });
  children.add(proc);
  return proc;
}

async function waitWindow(pid, title, ms = 20000) {
  const end = now() + ms;
  while (now() < end) {
    const t = await psCommand(`(Get-Process -Id ${pid} -ErrorAction SilentlyContinue).MainWindowTitle`).catch(() => "");
    if (t === title) return true;
    await sleep(300);
  }
  return false;
}

async function start(name) {
  const inst = await launch(name, PORTS[name], { offline: OFFLINE });
  children.add(inst.proc);
  logFiles.push({ name, file: inst.logFile });
  inst.exited.then(() => children.delete(inst.proc));
  // the exe must embed the current ui/dist (a build without custom-protocol loads Vite instead)
  const want = fs.readFileSync(path.join(REPO, "ui", "dist", "index.html"), "utf8").match(/assets\/index-[\w-]+\.js/)?.[0];
  const have = await inst.page.evaluate(() => [...document.scripts].map((s) => s.src).join(" "));
  check(`${name} runs the current ui/dist bundle`, !!want && have.includes(want), `${want} vs ${have}`);
  return inst;
}

/** Closes an instance with its title bar Close button (window_close) and waits for exit. */
async function closeViaUi(inst) {
  const t = now();
  await inst.page.locator(".titlebar-btn.is-close").click();
  const code = await Promise.race([inst.exited, sleep(15000).then(() => "timeout")]);
  inst.browser.close().catch(() => {});
  return { code, ms: now() - t };
}

// ------------------------------------------------------------------ flows

async function flowA(a, b) {
  for (const inst of [a, b]) {
    await inst.page.waitForSelector("#welcome-name", { timeout: 20000 });
    await inst.page.fill("#welcome-name", NAMES[inst.name]);
    await clickButton(inst, "Continue");
  }
  for (const inst of [a, b]) await waitText(inst, ".user-panel-name", NAMES[inst.name]);
  await Promise.all([shot(a, "a1-name"), shot(b, "a1-name")]);

  // A creates a server
  await clickButton(a, "Create server");
  await dlg(a).locator("#name-input").fill(SERVER);
  let t = now();
  await dlg(a).locator("button[type=submit]").click();
  await waitText(a, ".side-header .side-title", SERVER);
  await waitText(a, ".side-header .side-sub", "Only you are online");
  timing("create_server_ui", now() - t);

  // invite dialog: read the short code (the owner's first one is made on open: KDF + publish)
  await a.page.locator("button.side-header-btn").click();
  await a.page.getByRole("menuitem", { name: "Invite people" }).click();
  t = now();
  await waitPage(a, () => /^[2-9A-HJ-NP-Z]{3}-[2-9A-HJ-NP-Z]{3}$/.test(document.querySelector("#invite-code")?.value ?? ""), null, {
    timeout: 30000,
    what: "short invite code",
  });
  timing("invite_code_shown", now() - t);
  const invite = await a.page.inputValue("#invite-code");
  check("short invite code shown", /^[2-9A-HJ-NP-Z]{3}-[2-9A-HJ-NP-Z]{3}$/.test(invite), invite);
  await shot(a, "a2-invite");
  await clickButton(a, "Done", dlg(a));

  // B joins with it (typed in lowercase without the dash: input is forgiving)
  await clickButton(b, "Join with invite");
  await dlg(b).locator("#invite-input").fill(invite.replace("-", "").toLowerCase());
  t = now();
  await dlg(b).locator("button[type=submit]").click();
  await waitText(b, ".side-header .side-title", SERVER, { timeout: 30000 });
  timing("join_until_server_shown", now() - t);
  await waitPage(
    b,
    () => {
      const on = [...document.querySelectorAll("aside.members .member-group:not(.is-offline) .member-name")].map((e) => e.textContent);
      return on.some((n) => n.startsWith("Alice")) && on.some((n) => n.startsWith("Bob"));
    },
    null,
    { timeout: 30000, what: "both online in B's member list" },
  );
  timing("join_until_both_online_on_b", now() - t);
  await waitPage(
    a,
    () => {
      const on = [...document.querySelectorAll("aside.members .member-group:not(.is-offline) .member-name")].map((e) => e.textContent);
      return on.some((n) => n.startsWith("Alice")) && on.some((n) => n.startsWith("Bob"));
    },
    null,
    { timeout: 30000, what: "both online in A's member list" },
  );
  timing("join_until_both_online_on_a", now() - t);
  check("A's member list shows both online", true, (await membersOnline(a)).join(", "));
  check("B's member list shows both online", true, (await membersOnline(b)).join(", "));
  check("B sees who hosts", /Hosted by Alice/.test(await sideSub(b)), await sideSub(b));
  await Promise.all([shot(a, "a3-members"), shot(b, "a3-members")]);
  await noErrorToast(a, "a");
  await noErrorToast(b, "a");
}

async function flowB(a, b, files) {
  // typing line on B while A types
  await typeInComposer(a, "Hello from Alice", 60);
  const typingMs = await waitText(b, ".typing-line", "Alice is typing", { timeout: 8000 });
  check("B shows Alice's typing line", true, `${Math.round(typingMs)} ms after the last key`);
  await shot(b, "b1-typing");
  let t = now();
  await a.page.locator("textarea.composer-input").press("Enter");
  await waitText(b, ".msg-text", "Hello from Alice");
  timing("message_a_to_b_first", now() - t);

  await typeInComposer(b, "Hi Alice, Bob here", 60);
  await waitText(a, ".typing-line", "Bob is typing", { timeout: 8000 });
  check("A shows Bob's typing line", true);
  t = now();
  await b.page.locator("textarea.composer-input").press("Enter");
  await waitText(a, ".msg-text", "Hi Alice, Bob here");
  timing("message_b_to_a_first", now() - t);

  const lat = { ab: [], ba: [] };
  for (let i = 1; i <= 10; i++) {
    lat.ab.push(await sendAndSee(a, b, `ping a${i}`));
    lat.ba.push(await sendAndSee(b, a, `pong b${i}`));
  }
  const med = (xs) => xs.slice().sort((x, y) => x - y)[Math.floor(xs.length / 2)];
  timing("message_latency_a_to_b_median", med(lat.ab));
  timing("message_latency_b_to_a_median", med(lat.ba));
  results.latencySamples = { ab: lat.ab.map(Math.round), ba: lat.ba.map(Math.round) };
  // the typing line clears once the message arrived
  await waitPage(b, () => !document.querySelector(".typing-line")?.textContent.trim(), null, { what: "typing line cleared" });

  // attachment: A picks a PNG through the native dialog and sends it
  await pickWith(a, () => a.page.getByRole("button", { name: "Attach files" }).click(), files.png);
  await waitText(a, ".composer-file", "e2e-attach.png");
  await typeInComposer(a, "Here is a picture", 5);
  t = now();
  await a.page.locator("textarea.composer-input").press("Enter");
  await waitText(b, ".msg-text", "Here is a picture");
  const imgMs = await waitPage(
    b,
    () => [...document.querySelectorAll(".image-preview img")].some((i) => i.alt === "e2e-attach.png" && i.complete && i.naturalWidth > 0),
    null,
    { timeout: 30000, what: "B's inline image preview" },
  );
  timing("attachment_until_preview_on_b", now() - t);
  const dims = await b.page.$eval('.image-preview img[alt="e2e-attach.png"]', (i) => ({ w: i.naturalWidth, h: i.naturalHeight, src: i.src }));
  check("B shows the PNG inline", dims.w === 96 && dims.h === 64, JSON.stringify(dims));
  check("preview comes from the tincan file scheme", dims.src.startsWith("http://tincan.localhost/file/"), dims.src);
  void imgMs;
  await Promise.all([shot(a, "b2-attachment"), shot(b, "b2-attachment")]);
  await noErrorToast(a, "b");
  await noErrorToast(b, "b");
}

async function flowC(a, b) {
  // B copies its friend code with the UI's "Copy my friend code" (clipboard write captured in
  // the page so the real clipboard is left alone), cross-checked against get_friend_code.
  await b.page.getByRole("button", { name: /^Direct messages/ }).click();
  await b.page.locator(".side-header").getByRole("button", { name: "Add contact" }).click();
  await b.page.evaluate(() => {
    window.__e2eCopied = [];
    navigator.clipboard.writeText = async (t) => {
      window.__e2eCopied.push(t);
    };
  });
  // enabled once the dialog has the code (made and published on first use)
  await clickButton(b, "Copy my friend code", dlg(b));
  await waitText(b, "dialog[open] button", "Copied");
  const code = await b.page.evaluate(() => window.__e2eCopied[0]);
  const apiCode = await b.page.evaluate(async () => window.__TAURI_INTERNALS__.invoke("api", { cmd: "get_friend_code", args: {} }));
  check("Copy my friend code gives B's friend code", code === apiCode && /^[2-9A-HJ-NP-Z]{3}-[2-9A-HJ-NP-Z]{3}$/.test(code), code);
  await clickButton(b, "Cancel", dlg(b));

  // A adds B by that code. B's record is published in the background right after the code
  // is made, so a lookup that comes first can miss it: resubmit on that error for a while.
  await a.page.getByRole("button", { name: /^Direct messages/ }).click();
  await a.page.locator(".side-header").getByRole("button", { name: "Add contact" }).click();
  await dlg(a).locator("#contact-code").fill(code);
  let t = now();
  for (let tries = 0; ; tries++) {
    await dlg(a).locator("button[type=submit]").click();
    await waitPage(
      a,
      () => document.querySelector(".main-header .main-title")?.textContent?.includes("Bob") || !!document.querySelector("dialog[open] .dialog-error"),
      null,
      { timeout: 30000, what: "A's add-contact result" },
    );
    const err = await a.page.$eval("dialog[open] .dialog-error", (e) => e.textContent).catch(() => null);
    if (!err) break;
    if (tries >= 10) check("A adds Bob by friend code", false, err);
    await sleep(1500);
  }
  timing("add_contact_by_friend_code", now() - t);
  await waitText(a, ".main-header .main-title", "Bob");
  check("A's DM with Bob is open", true, await a.page.$eval(".main-header .main-title", (e) => e.textContent));

  await typeInComposer(a, "DM hello from Alice", 5);
  t = now();
  await a.page.locator("textarea.composer-input").press("Enter");
  await waitText(b, "button.dm-row", "Alice", { timeout: 15000 });
  await b.page.locator("button.dm-row", { hasText: "Alice" }).click();
  await waitText(b, ".msg-text", "DM hello from Alice");
  timing("dm_a_to_b_including_open", now() - t);
  await waitPage(a, () => !document.querySelector(".msg-text.is-pending"), null, { what: "A's DM delivered (not pending)" });

  await typeInComposer(b, "DM reply from Bob", 40);
  await waitText(a, ".typing-line", "Bob is typing", { timeout: 8000 });
  check("DM typing line on A", true);
  t = now();
  await b.page.locator("textarea.composer-input").press("Enter");
  await waitText(a, ".msg-text", "DM reply from Bob");
  timing("dm_b_to_a", now() - t);
  await Promise.all([shot(a, "c1-dm"), shot(b, "c1-dm")]);
  await noErrorToast(a, "c");
  await noErrorToast(b, "c");
}

async function joinLounge(inst) {
  await openServer(inst);
  await openChannel(inst, "Lounge");
  await inst.page.waitForSelector('section[aria-label="Voice"]', { timeout: 15000 });
  // two instances share one microphone and one set of speakers: mute at once
  await inst.page.getByRole("button", { name: "Mute", exact: true }).click();
  await inst.page.waitForSelector('section[aria-label="Voice"] button[aria-label="Unmute"]');
}

async function flowD(a, b, files) {
  const t = now();
  await joinLounge(a);
  await joinLounge(b);
  for (const inst of [a, b]) {
    await waitPage(
      inst,
      () => {
        const names = [...document.querySelectorAll('ul[aria-label="In Lounge"] .room-person')].map((e) => e.textContent);
        return names.some((n) => n.includes("Alice")) && names.some((n) => n.includes("Bob"));
      },
      null,
      { timeout: 15000, what: "both in Lounge" },
    );
  }
  timing("voice_both_in_room", now() - t);
  const path_ = await b.page.$eval('section[aria-label="Voice"] .call-state', (e) => e.textContent.trim());
  check("voice path shown", ["Direct", "Relayed"].some((p) => path_.startsWith(p)), path_);
  await Promise.all([shot(a, "d1-voice"), shot(b, "d1-voice")]);

  // A adds the WAV and plays it
  await a.page.getByRole("button", { name: "Soundboard", exact: true }).click();
  await a.page.waitForSelector('section[aria-label="Soundboard"]');
  await pickWith(a, () => a.page.locator("button.clip-add").click(), files.wav);
  await dlg(a).locator("#sound-name").waitFor();
  const clipName = await dlg(a).locator("#sound-name").inputValue();
  check("Add sound dialog names the clip after the file", clipName === "e2e-chime", clipName);
  await dlg(a).locator("button[type=submit]").click();
  await a.page.waitForSelector('button[aria-label="Play e2e-chime"]', { timeout: 15000 });
  const meta = await a.page.$eval('button[aria-label="Play e2e-chime"] .clip-meta', (e) => e.textContent.trim());
  check("clip added with its length", /^(1\.9|2\.0|2\.1) s$/.test(meta), meta);

  await b.page.getByRole("button", { name: "Soundboard", exact: true }).click();
  await b.page.waitForSelector('section[aria-label="Soundboard"]');
  let t2 = now();
  await a.page.click('button[aria-label="Play e2e-chime"]');
  await waitText(b, 'section[aria-label="Soundboard"] h3', "Heard in this call", { timeout: 10000 });
  await waitText(b, ".clip-tile.is-heard", "e2e-chime");
  timing("sound_played_until_heard_on_b", now() - t2);
  const from = await b.page.$eval(".clip-tile.is-heard .clip-meta", (e) => e.textContent.trim());
  check("heard tile says who played it", from === "From Alice", from);
  await shot(b, "d2-heard");

  t2 = now();
  await b.page.click('button[aria-label="Save e2e-chime to your sounds"]');
  await b.page.waitForSelector('button[aria-label="Play e2e-chime"]', { timeout: 20000 });
  timing("clip_pull_on_b", now() - t2);
  await sleep(2200); // let A's clip finish before B plays
  await b.page.click('button[aria-label="Play e2e-chime"]');
  await waitPage(b, () => document.querySelector('button[aria-label="Play e2e-chime"]')?.classList.contains("is-playing"), null, {
    what: "B's tile playing",
  });
  // A owns the clip too: B's play reaches A as sound-played and lights A's tile
  await waitPage(a, () => document.querySelector('button[aria-label="Play e2e-chime"]')?.classList.contains("is-playing"), null, {
    timeout: 5000,
    what: "A sees B's play",
  });
  check("B played the pulled clip and A saw it", true);
  await Promise.all([shot(a, "d3-played"), shot(b, "d3-played")]);
  await noErrorToast(a, "d");
  await noErrorToast(b, "d");
  for (const inst of [a, b]) await inst.page.getByRole("button", { name: "Close soundboard" }).click();
}

async function flowE(a, b) {
  const pattern = startPattern();
  check("pattern window open", await waitWindow(pattern.pid, PATTERN_TITLE), PATTERN_TITLE);
  await sleep(500);

  await a.page.getByRole("button", { name: "Share screen", exact: true }).click();
  const card = dlg(a).locator("button.source-card", { has: a.page.locator(".source-name", { hasText: PATTERN_TITLE }) });
  await card.first().waitFor({ timeout: 15000 });
  check("pattern window listed once", (await card.count()) === 1, String(await card.count()));
  await card.click();
  check("pattern window selected", (await card.getAttribute("aria-checked")) === "true");
  // nothing else may be picked (the picker pre-selects the first display)
  const pickedNames = await dlg(a).locator('button.source-card[aria-checked="true"] .source-name').allTextContents();
  check("only the pattern window is picked", pickedNames.length === 1 && pickedNames[0] === PATTERN_TITLE, pickedNames.join(","));
  let t = now();
  await dlg(a).locator("button[type=submit]").click();
  await a.page.waitForSelector('section[aria-label="Voice"] button[aria-label="Stop sharing"]', { timeout: 15000 });
  timing("screen_start_ui", now() - t);
  await a.page.waitForSelector("dialog[open]", { state: "detached", timeout: 5000 });
  check("share dialog closed after Share", true);

  // B opens the share from its tile
  const watch = b.page.getByRole("button", { name: "Watch Alice's screen" });
  await watch.waitFor({ timeout: 15000 });
  t = now();
  await watch.click();
  await waitPage(
    b,
    () => {
      const c = document.querySelector('canvas[aria-label="Alice\'s screen"]');
      return c && c.width > 16 && !document.querySelector(".screen-status");
    },
    null,
    { timeout: 20000, what: "first decoded frame on B" },
  );
  timing("watch_until_first_frame", now() - t);
  await sleep(1500);

  // read the colours from a CDP screenshot of B's window
  const geo = await b.page.$eval('canvas[aria-label="Alice\'s screen"]', (c) => {
    const r = c.getBoundingClientRect();
    return { x: r.x, y: r.y, w: r.width, h: r.height, cw: c.width, ch: c.height, dpr: devicePixelRatio };
  });
  const file = await shot(b, "e1-watching");
  await shot(a, "e1-sharing");
  const img = decodePng(fs.readFileSync(file));
  const scale = Math.min(geo.w / geo.cw, geo.h / geo.ch);
  const dw = geo.cw * scale;
  const dh = geo.ch * scale;
  const ox = (geo.x + (geo.w - dw) / 2) * geo.dpr;
  const oy = (geo.y + (geo.h - dh) / 2) * geo.dpr;
  const at = (fx, fy) => meanColor(img, Math.round(ox + fx * dw * geo.dpr), Math.round(oy + fy * dh * geo.dpr), 10);
  const samples = {
    red: at(0.2, 0.3),
    green: at(0.8, 0.3),
    blue: at(0.2, 0.85),
    yellow: at(0.8, 0.85),
  };
  results.screenSamples = { geo, samples };
  log("screen samples", JSON.stringify(samples), JSON.stringify(geo));
  check("B's canvas shows red top left", near(samples.red, [255, 0, 0]), JSON.stringify(samples.red));
  check("B's canvas shows green top right", near(samples.green, [0, 255, 0]), JSON.stringify(samples.green));
  check("B's canvas shows blue bottom left", near(samples.blue, [0, 0, 255]), JSON.stringify(samples.blue));
  check("B's canvas shows yellow bottom right", near(samples.yellow, [255, 255, 0]), JSON.stringify(samples.yellow));
  check("canvas has the window's aspect", Math.abs(geo.cw / geo.ch - 656 / 519) < 0.12, `${geo.cw}x${geo.ch}`);

  // stop sharing; B's stage goes back to tiles
  t = now();
  await a.page.locator('section[aria-label="Voice"] button[aria-label="Stop sharing"]').click();
  await waitPage(b, () => !document.querySelector('canvas[aria-label="Alice\'s screen"]'), null, { what: "share gone on B" });
  timing("screen_stop_until_gone_on_b", now() - t);
  await noErrorToast(a, "e");
  await noErrorToast(b, "e");
  return pattern;
}

async function flowF(a, b) {
  // leave voice so the run measures text and presence recovery
  for (const inst of [a, b]) {
    await inst.page.locator('section[aria-label="Voice"]').getByRole("button", { name: "Leave voice" }).click();
    await inst.page.waitForSelector('section[aria-label="Voice"]', { state: "detached" });
    await openChannel(inst, "general");
    await inst.page.waitForSelector("textarea.composer-input");
  }
  const subA = await sideSub(a);
  const subB = await sideSub(b);
  log("host lines", subA, "|", subB);
  const host = subA === "Hosted by you" ? a : subB === "Hosted by you" ? b : null;
  check("one instance says it hosts", host !== null, `${subA} | ${subB}`);
  const other = host === a ? b : a;
  check("the other names the host", (await sideSub(other)) === `Hosted by ${NAMES[host.name]}`, await sideSub(other));

  // the host leaves through window_close (title bar Close)
  const t = now();
  const closing = closeViaUi(host);
  const recovered = await waitPage(
    other,
    () => ["Only you are online", "Hosted by you"].includes(document.querySelector(".side-header .side-sub")?.textContent.trim()),
    null,
    { timeout: 30000, what: "survivor hosting" },
  );
  timing("migration_until_survivor_hosts", recovered);
  const closed = await closing;
  timing("host_close_until_exit", closed.ms);
  check("closed host exited cleanly", closed.code === 0, String(closed.code));
  const away = ["Sent while the host was away 1", "Sent while the host was away 2"];
  for (const text of away) {
    await typeInComposer(other, text, 5);
    await other.page.locator("textarea.composer-input").press("Enter");
    await waitText(other, ".msg-text", text);
  }
  timing("migration_until_survivor_sent", now() - t);
  check("survivor keeps chatting", true, await sideSub(other));
  await shot(other, "f1-survivor");
  await noErrorToast(other, "f");

  // restart the closed instance with the same profile and port
  const t2 = now();
  const back = await start(host.name);
  await back.page.waitForSelector(".user-panel-name", { timeout: 20000 });
  await waitText(back, ".side-header .side-sub", `Hosted by ${NAMES[other.name]}`, { timeout: 30000 });
  timing("restart_until_member_again", now() - t2);
  for (const text of away) await waitText(back, ".msg-text", text, { timeout: 30000 });
  timing("restart_until_missed_messages_shown", now() - t2);
  check("restarted instance shows the messages sent while it was closed", true);
  await waitPage(
    other,
    (n) => [...document.querySelectorAll("aside.members .member-group:not(.is-offline) .member-name")].some((e) => e.textContent.startsWith(n)),
    NAMES[host.name],
    { timeout: 20000, what: "returning member online" },
  );
  const lat = await sendAndSee(back, other, "Back again");
  timing("after_rejoin_message_latency", lat);
  // read markers survive the restart: the DM read before closing is not unread again
  const dmLabel = await back.page.locator("nav.rail button[title='Direct messages']").getAttribute("aria-label");
  check("restarted instance keeps its DM read", dmLabel === "Direct messages", dmLabel);
  await Promise.all([shot(back, "f2-rejoined"), shot(other, "f2-rejoined")]);
  await noErrorToast(back, "f");
  return host === a ? [back, b] : [a, back];
}

async function resourceCheck(a, b) {
  for (const inst of [a, b]) await inst.browser.close().catch(() => {});
  log("idle 60 s with CDP disconnected");
  await sleep(60000);
  const [ra, rb] = await Promise.all([processStats(a.pid, 10000), processStats(b.pid, 10000)]);
  results.resources = { [a.name]: ra, [b.name]: rb };
  results.resourceTotals = {};
  for (const [n, rows] of Object.entries(results.resources)) {
    for (const r of rows) {
      log(n, r.name, r.role, r.pid, `ws ${r.workingSetMB} MB`, `priv ${r.privateMB} MB`, `thr ${r.threads}`, `cpu ${r.cpuPct}%`);
    }
    const sum = (k) => +rows.reduce((s, r) => s + (r[k] ?? 0), 0).toFixed(2);
    results.resourceTotals[n] = { processes: rows.length, workingSetMB: sum("workingSetMB"), privateMB: sum("privateMB"), threads: sum("threads"), cpuPct: sum("cpuPct") };
    log(n, "total", JSON.stringify(results.resourceTotals[n]));
  }
  save();
}

async function reconnect(inst) {
  const { chromium } = await import("./lib.mjs");
  inst.browser = await chromium.connectOverCDP(`http://127.0.0.1:${inst.port}`);
  inst.page = inst.browser.contexts().flatMap((c) => c.pages()).find((p) => p.url().startsWith("http://tauri.localhost"));
  return inst;
}

// ------------------------------------------------------------------ main

let a;
let b;
let pattern;
let failed = null;
try {
  wipeProfile("e2e-a");
  wipeProfile("e2e-b");
  const files = makeMedia();
  [a, b] = await Promise.all([start("e2e-a"), start("e2e-b")]);
  log("started", a.pid, b.pid);
  await flowA(a, b);
  save();
  await flowB(a, b, files);
  save();
  await flowC(a, b);
  save();
  await flowD(a, b, files);
  save();
  pattern = await flowE(a, b);
  save();
  [a, b] = await flowF(a, b);
  save();
  await resourceCheck(a, b);
  await reconnect(a);
  await reconnect(b);
  log("all flows passed");
} catch (e) {
  failed = e;
  log("FAILED", e.stack ?? e);
  for (const inst of [a, b]) {
    if (inst?.page && !inst.page.isClosed()) await shot(inst, "zz-failure").catch(() => {});
  }
} finally {
  results.finished = new Date().toISOString();
  results.passed = failed === null;
  results.error = failed ? String(failed.message ?? failed) : null;
  results.console = { a: a?.console ?? [], b: b?.console ?? [] };
  save();
  if (!KEEP || failed === null) {
    for (const inst of [a, b]) {
      if (!inst || inst.proc.exitCode !== null) continue;
      const r = await closeViaUi(inst).catch((e) => ({ code: String(e) }));
      log("closed", inst.name, JSON.stringify(r));
      if (inst.proc.exitCode === null) inst.proc.kill();
    }
    for (const c of children) if (c.exitCode === null) c.kill();
  } else {
    log("--keep: leaving instances open", a?.pid, b?.pid);
  }
}
// What the instances wrote to stderr (TINCAN_LOG): warnings, errors, audio device use.
results.logSummary = logFiles.map(({ name, file }) => {
  const lines = fs.existsSync(file) ? fs.readFileSync(file, "utf8").split(/\r?\n/) : [];
  const pick = (re) => lines.filter((l) => re.test(l));
  return {
    name,
    file: path.basename(file),
    warnings: pick(/\sWARN\s/).slice(0, 20),
    errors: pick(/\sERROR\s/).slice(0, 20),
    micOpened: pick(/microphone opened/).length,
    micClosed: pick(/microphone closed/).length,
    speakerOpened: pick(/speaker opened/).length,
    speakerClosed: pick(/speaker closed/).length,
  };
});
for (const l of results.logSummary) {
  log("log", l.name, `warn ${l.warnings.length} error ${l.errors.length} mic ${l.micOpened}/${l.micClosed} speaker ${l.speakerOpened}/${l.speakerClosed}`);
}
save();
process.exit(failed ? 1 : 0);
