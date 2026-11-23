// Typed access to the Rust app. Inside Tauri this goes through @tauri-apps/api;
// in a plain browser it goes to the in-browser mock backend (loaded on demand,
// so the production bundle never pays for it).

import type { CommandName, Commands, EventName, Events } from "./types";

type Args<K extends CommandName> = Commands[K]["args"];
type Result<K extends CommandName> = Commands[K]["result"];

/** True when running inside the Tauri webview. */
export const isTauri: boolean =
  typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

/** The backend surface both the real bridge and the mock implement. */
export interface Backend {
  invoke(name: string, args: unknown): Promise<unknown>;
  /** Registers a listener; resolves to its unsubscribe function. */
  listen(name: string, cb: (payload: unknown) => void): Promise<() => void>;
}

let backendPromise: Promise<Backend> | null = null;

function backend(): Promise<Backend> {
  if (!backendPromise) {
    backendPromise = isTauri ? tauriBackend() : import("./mock").then((m) => m.mockBackend);
  }
  return backendPromise;
}

/** Commands the Tauri shell implements itself; see `tauriBackend`. */
const SHELL_COMMANDS: ReadonlySet<string> = new Set(["screen_watch", "window_minimize", "window_toggle_maximize", "window_close"]);

async function tauriBackend(): Promise<Backend> {
  const [core, event] = await Promise.all([
    import("@tauri-apps/api/core"),
    import("@tauri-apps/api/event"),
  ]);
  return {
    // The shell registers `screen_watch` (it carries a Channel) and the window commands as
    // their own Tauri commands; every other command goes through the generic `api` command,
    // which hands `cmd` and the camelCase `args` object to tincan-app unchanged.
    invoke: (name, args) =>
      SHELL_COMMANDS.has(name)
        ? core.invoke(name, args as Record<string, unknown>)
        : core.invoke("api", { cmd: name, args: args ?? {} }),
    listen: (name, cb) => event.listen(name, (e) => cb(e.payload)),
  };
}

/** Error strings the Rust side uses as machine-readable codes. */
export type ErrorCode = "no-hardware-encoder" | "not-in-voice" | "deafened";

/** Turns a rejected command into a string that is safe to show. */
export function errorText(err: unknown): string {
  const raw = typeof err === "string" ? err : err instanceof Error ? err.message : "";
  switch (raw) {
    case "no-hardware-encoder":
      return "Screen sharing isn't available on this PC. It has no hardware video encoder.";
    case "not-in-voice":
      return "Join a voice room first.";
    case "deafened":
      return "Undeafen to use the soundboard.";
    case "":
      return "Something went wrong. Try again.";
    default:
      return raw;
  }
}

/** The raw error string of a rejected command, for matching error codes. */
export function errorCode(err: unknown): string {
  return typeof err === "string" ? err : err instanceof Error ? err.message : "";
}

/** Calls a Rust command. Rejects with the command's user-facing error string. */
export async function invoke<K extends CommandName>(name: K, args: Args<K>): Promise<Result<K>> {
  const b = await backend();
  return (await b.invoke(name, args)) as Result<K>;
}

/**
 * Subscribes to a backend event. Returns a synchronous unsubscribe that also
 * cancels a registration that is still in flight, so React StrictMode double
 * mounting never leaves a duplicate listener behind.
 */
export function listen<E extends EventName>(name: E, cb: (payload: Events[E]) => void): () => void {
  let unlisten: (() => void) | null = null;
  let cancelled = false;
  backend()
    .then((b) => b.listen(name, (p) => cb(p as Events[E])))
    .then((u) => {
      if (cancelled) u();
      else unlisten = u;
    })
    .catch((err: unknown) => console.error(`listen ${name} failed`, err));
  return () => {
    cancelled = true;
    if (unlisten) {
      unlisten();
      unlisten = null;
    }
  };
}

/** A receiver for raw video messages from `screen_watch`. */
export interface FrameChannel {
  /** The value to pass as `onFrame`. */
  readonly ipc: unknown;
  /** Stops delivery to the handler. */
  close(): void;
}

/** Creates a Tauri Channel that delivers raw ArrayBuffers to `onFrame`. Tauri only. */
export async function frameChannel(onFrame: (buf: ArrayBuffer) => void): Promise<FrameChannel> {
  const core = await import("@tauri-apps/api/core");
  let open = true;
  const ch = new core.Channel<ArrayBuffer>((msg) => {
    if (open) onFrame(msg);
  });
  return {
    ipc: ch,
    close() {
      open = false;
      ch.onmessage = () => {};
    },
  };
}

/** Opens the native file picker. Mock mode returns fake paths. */
export async function pickFiles(opts: {
  multiple: boolean;
  filters?: { name: string; extensions: string[] }[];
  directory?: boolean;
}): Promise<string[]> {
  if (!isTauri) {
    const m = await import("./mock");
    return m.fakePick(opts);
  }
  const dialog = await import("@tauri-apps/plugin-dialog");
  const picked = await dialog.open({
    multiple: opts.multiple,
    directory: opts.directory ?? false,
    filters: opts.filters,
  });
  if (picked === null) return [];
  return Array.isArray(picked) ? picked : [picked];
}

/** Copies text to the clipboard. */
export async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    return false;
  }
}
