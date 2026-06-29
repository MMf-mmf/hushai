// Time helpers. The API speaks Unix *nanoseconds* (UTC); the UI works in
// milliseconds (well within JS safe-integer range) and renders in the admin's
// local timezone. ns/1e6 keeps ~sub-microsecond error — irrelevant at video scale.

export const nsToMs = (ns) => Number(ns) / 1e6;
// ms -> ns as a plain integer string (no exponent for values < 1e21).
export const msToNsStr = (ms) => String(Math.round(ms * 1e6));

const pad = (n, w = 2) => String(n).padStart(w, "0");

export function clock(ms) {
  const d = new Date(ms);
  return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
}

export function clockMs(ms) {
  const d = new Date(ms);
  return `${clock(ms)}.${pad(d.getMilliseconds(), 3)}`;
}

export function dateLabel(ms) {
  const d = new Date(ms);
  return d.toLocaleDateString(undefined, {
    weekday: "short",
    year: "numeric",
    month: "short",
    day: "numeric",
  });
}

// "YYYY-MM-DD" in LOCAL time (for the <input type=date> and day stepping).
export function localDateInput(ms) {
  const d = new Date(ms);
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}

// Local midnight (ms) for the local day containing `ms`.
export function startOfLocalDay(ms) {
  const d = new Date(ms);
  d.setHours(0, 0, 0, 0);
  return d.getTime();
}

export const DAY_MS = 24 * 3600 * 1000;

export function tzAbbr() {
  try {
    const parts = new Intl.DateTimeFormat(undefined, { timeZoneName: "short" }).formatToParts(
      new Date(),
    );
    return parts.find((p) => p.type === "timeZoneName")?.value ?? "";
  } catch {
    return "";
  }
}

// Human duration for tooltips/labels, e.g. "1h 12m", "3m 4s", "12s".
export function humanDur(ms) {
  const s = Math.round(ms / 1000);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${s % 60}s`;
  const h = Math.floor(m / 60);
  return `${h}h ${m % 60}m`;
}

// Human byte size, e.g. "0 B", "934 KB", "2.1 GB" (binary units; mirrors the backend's human_bytes).
export function humanBytes(bytes) {
  const n = Number(bytes) || 0;
  const units = ["B", "KB", "MB", "GB", "TB", "PB"];
  let v = n;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i += 1;
  }
  return i === 0 ? `${n} B` : `${v.toFixed(1)} ${units[i]}`;
}
