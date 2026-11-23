// Formatting helpers shared by the UI and the mock backend.

/** "18.4 MB", "512 KB", "12 bytes". */
export function formatBytes(n: number): string {
  if (n < 1024) return n === 1 ? "1 byte" : `${n} bytes`;
  const units = ["KB", "MB", "GB", "TB"];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  const text = v >= 100 ? v.toFixed(0) : v.toFixed(1).replace(/\.0$/, "");
  return `${text} ${units[i]}`;
}

/** "1.8 s" for clip durations. */
export function formatSeconds(ms: number): string {
  const s = ms / 1000;
  return `${s < 10 ? s.toFixed(1) : Math.round(s)} s`;
}

const timeFmt = new Intl.DateTimeFormat(undefined, { hour: "numeric", minute: "2-digit" });
const dayFmt = new Intl.DateTimeFormat(undefined, { weekday: "long", month: "long", day: "numeric" });
const dayYearFmt = new Intl.DateTimeFormat(undefined, {
  weekday: "long",
  month: "long",
  day: "numeric",
  year: "numeric",
});
const fullFmt = new Intl.DateTimeFormat(undefined, { dateStyle: "full", timeStyle: "short" });

/** "9:41 PM". */
export function formatTime(ms: number): string {
  return timeFmt.format(ms);
}

/** Full date and time, for timestamp tooltips. */
export function formatFull(ms: number): string {
  return fullFmt.format(ms);
}

/** Start of the local day containing `ms`. */
export function dayStart(ms: number): number {
  const d = new Date(ms);
  d.setHours(0, 0, 0, 0);
  return d.getTime();
}

/** "Today", "Yesterday", "Saturday, November 21". */
export function formatDay(ms: number, now = Date.now()): string {
  const today = dayStart(now);
  const day = dayStart(ms);
  if (day === today) return "Today";
  if (day === dayStart(today - 1)) return "Yesterday";
  const sameYear = new Date(ms).getFullYear() === new Date(now).getFullYear();
  return (sameYear ? dayFmt : dayYearFmt).format(ms);
}

/** Up to two initials from a display name. */
export function initials(name: string): string {
  const parts = name.trim().split(/\s+/).filter(Boolean);
  if (parts.length === 0) return "?";
  const first = Array.from(parts[0])[0] ?? "";
  const second = parts.length > 1 ? (Array.from(parts[parts.length - 1])[0] ?? "") : "";
  return (first + second).toUpperCase();
}

/** Stable small integer from a string id, for avatar colours. */
export function hashIndex(id: string, mod: number): number {
  let h = 2166136261;
  for (let i = 0; i < id.length; i++) {
    h ^= id.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return (h >>> 0) % mod;
}

/** The last path segment of a Windows or POSIX path. */
export function baseName(path: string): string {
  const parts = path.split(/[\\/]/);
  return parts[parts.length - 1] || path;
}

/** File name without its extension. */
export function stripExtension(name: string): string {
  const i = name.lastIndexOf(".");
  return i > 0 ? name.slice(0, i) : name;
}
