// Omni-search palette: press "/" (or Cmd/Ctrl+K) to jump to a camera, a moment in time,
// a license plate, or a person/voice — or hand the query to the AI chat. Self-contained
// like confirm.js: the modal DOM is built lazily and appended to <body>, so a page adopts
// the palette with a single side-effect import. Groups requery as you type (200ms
// debounce); ↑/↓ walk every row across groups, Enter activates, Esc/backdrop close
// (behavior + focus trap via modal.js).

import {
  getDevices,
  getEvents,
  getPersons,
  getSpeakers,
  searchPlates,
  samplePlateUrl,
} from "../api.js";
import { wireModal } from "../modal.js";
import { toast } from "../toast.js";
import { el } from "../dom.js";
import { chatAsk } from "../store.js";

const DEBOUNCE_MS = 200;
const CACHE_MS = 60_000;
const DAY_MS = 24 * 3600 * 1000;
const LIVE_BEHIND_MS = 4000; // land just behind a camera's newest footage (matches app.js)

let modal = null,
  input,
  results,
  ctl;
let rows = []; // flat [{node, activate}] in visual order, across all groups
let hi = -1; // highlighted row index (↑/↓ + hover)
let seq = 0; // stale-async-query guard
let debounceT = null;

// ---- cached lookups -----------------------------------------------------------
// Devices back the Cameras group on every keystroke; people/voices are fetched lazily
// on the first character and refreshed on the same 60s clock.

const caches = new Map(); // key -> { at, data }
async function cached(key, fetcher) {
  const hit = caches.get(key);
  if (hit && performance.now() - hit.at < CACHE_MS) return hit.data;
  const data = await fetcher();
  caches.set(key, { at: performance.now(), data });
  return data;
}

// ---- deep links -----------------------------------------------------------------
// app.js's deep-link path needs BOTH ?device and ?t (a lone ?device is ignored and the
// default camera wins — verified in app.js init), so every link here carries a time.

function deepLink(deviceId, ms) {
  return `/?device=${encodeURIComponent(deviceId)}&t=${Math.round(ms)}`;
}

// The latest materialized /v1/events row for a subject (person/plate/speaker id) as a
// deep link, or null. The endpoint orders start DESC, so limit:1 IS the newest sighting.
async function latestSightingLink(subjectId) {
  const evs = await getEvents({ subjectId, limit: 1 });
  const ev = evs?.[0];
  return ev && ev.deviceId && ev.startMs ? deepLink(ev.deviceId, ev.startMs) : null;
}

async function openLatestSighting(subjectId, noneMsg) {
  let link;
  try {
    link = await latestSightingLink(subjectId);
  } catch {
    toast("Couldn't look up sightings.", { kind: "error" });
    return;
  }
  if (link) location.href = link;
  else toast(noneMsg);
}

// ---- "jump to time" parser -------------------------------------------------------
// Tiny on purpose: "YYYY-MM-DD [HH:MM]", "HH:MM", "today 5pm", "yesterday 17:30",
// "5:30pm". Local time. Returns wall-clock ms, or null when the query isn't time-shaped.
export function parseWhen(raw) {
  const q = (raw || "").trim().toLowerCase();
  if (!q) return null;
  let dayMs = null;
  let rest = q;

  const kw = rest.match(/^(today|yesterday)\b\s*(.*)$/);
  if (kw) {
    const d = new Date();
    d.setHours(0, 0, 0, 0);
    dayMs = d.getTime() - (kw[1] === "yesterday" ? DAY_MS : 0);
    rest = kw[2];
  } else {
    const iso = rest.match(/^(\d{4})-(\d{1,2})-(\d{1,2})\b\s*(.*)$/);
    if (iso) {
      const d = new Date(+iso[1], +iso[2] - 1, +iso[3]);
      // Reject rollover (e.g. 2026-13-45 silently becoming next year).
      if (d.getMonth() !== +iso[2] - 1 || d.getDate() !== +iso[3]) return null;
      dayMs = d.getTime();
      rest = iso[4];
    }
  }

  rest = rest.replace(/^at\s+/, "");
  let timeMs = null;
  if (rest) {
    const t = rest.match(/^(\d{1,2})(?::(\d{2}))?\s*(am|pm)?$/);
    if (!t) return null; // trailing junk — not a time query
    let h = +t[1];
    const min = t[2] ? +t[2] : 0;
    if (min > 59) return null;
    if (t[3]) {
      if (h < 1 || h > 12) return null;
      if (t[3] === "pm" && h !== 12) h += 12;
      if (t[3] === "am" && h === 12) h = 0;
    } else {
      if (h > 23) return null;
      // A bare number ("17") is too ambiguous to hijack — require :mm, am/pm, or a date.
      if (!t[2] && dayMs == null) return null;
    }
    timeMs = (h * 60 + min) * 60_000;
  }

  if (dayMs == null && timeMs == null) return null;
  if (dayMs == null) {
    const d = new Date();
    d.setHours(0, 0, 0, 0);
    dayMs = d.getTime();
  }
  return dayMs + (timeMs ?? 0);
}

// ---- rows -----------------------------------------------------------------------

function makeRow({ icon, thumb, label, sub, hint, activate }) {
  const node = el(
    "div",
    { class: "omni-row" },
    thumb ?? (icon ? el("span", { class: "omni-ico", text: icon }) : null),
    el("span", { class: "omni-label", text: label }),
    sub ? el("span", { class: "omni-sub", text: sub }) : null,
    hint ? el("span", { class: "omni-hint", text: hint }) : null,
  );
  let busy = false; // async activations (sighting lookups) must not double-fire
  const row = { node };
  row.activate = async () => {
    if (busy) return;
    busy = true;
    try {
      await activate();
    } finally {
      busy = false;
    }
  };
  node.addEventListener("click", row.activate);
  node.addEventListener("mouseenter", () => setHi(rows.indexOf(row)));
  return row;
}

function cameraRow(d) {
  const name = d.displayName || d.id;
  return makeRow({
    icon: "🎥",
    label: name,
    sub: d.displayName ? d.id : null,
    hint: "camera",
    activate: () => {
      const u = new URLSearchParams({ device: d.id });
      if (d.latestMs) u.set("t", String(Math.round(Math.max(d.latestMs - LIVE_BEHIND_MS, d.earliestMs ?? 0))));
      location.href = "/?" + u.toString();
    },
  });
}

function timeRow(ms) {
  const label = new Date(ms).toLocaleString(undefined, {
    weekday: "short",
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
  return makeRow({
    icon: "⏱",
    label: `Jump to ${label}`,
    hint: "time",
    activate: () => {
      // Keep the current camera: the URL's ?device= wins; on the viewer page fall back
      // to the picker's selection. (Follow-up: seek in-page via the store instead of a
      // full navigation when already on the viewer.)
      const device =
        new URLSearchParams(location.search).get("device") ||
        document.getElementById("deviceSelect")?.value ||
        null;
      if (device) location.href = deepLink(device, ms);
      else location.href = `/?t=${Math.round(ms)}`;
    },
  });
}

function plateRow(p) {
  const label = (p.display_name || "").trim() || (p.plate_text || "").trim() || "Unreadable plate";
  const thumb = el("img", { class: "omni-thumb", alt: "", loading: "lazy", src: samplePlateUrl(p.plate_id) });
  thumb.addEventListener("error", () => thumb.classList.add("is-missing"));
  const n = p.n_sightings ?? p.n_samples ?? 0;
  const sightings = `${n} sighting${n === 1 ? "" : "s"}`;
  return makeRow({
    thumb,
    label,
    sub: p.display_name && p.plate_text ? `${p.plate_text} · ${sightings}` : sightings,
    hint: "latest sighting",
    activate: () => openLatestSighting(p.plate_id, "No sightings recorded"),
  });
}

function personRow(p) {
  const n = p.n_sightings ?? 0;
  return makeRow({
    icon: "👤",
    label: p.display_name,
    sub: `${n} sighting${n === 1 ? "" : "s"}`,
    hint: "latest sighting",
    activate: () => openLatestSighting(p.person_id, "No sightings recorded"),
  });
}

function speakerRow(s) {
  return makeRow({
    icon: "🎙",
    label: s.display_name,
    sub: `${s.n_samples ?? 0} samples`,
    hint: "voice",
    // Voice sightings exist as events (subject_type "speaker") but older captures may
    // predate the events layer — fall back to pointing at the Voices modal.
    activate: () => openLatestSighting(s.speaker_id, "Open Voices to manage"),
  });
}

function askRow(q) {
  return makeRow({
    icon: "✦",
    label: `Ask: ${q}`,
    hint: "AI chat",
    activate: () => {
      // On the viewer page the chat pane subscribes to the store's chatAsk event;
      // elsewhere hand off via /?ask= (chat-pane.js consumes it and cleans the URL).
      if (document.querySelector(".chat-pane")) {
        ctl.close();
        chatAsk(q);
      } else {
        location.href = "/?ask=" + encodeURIComponent(q);
      }
    },
  });
}

// ---- query + render ---------------------------------------------------------------

const named = (x) => x.display_name && String(x.display_name).trim() !== "";

async function query(q) {
  const my = ++seq;
  const ql = q.toLowerCase();

  const [devices, plates, persons, speakers] = await Promise.all([
    cached("devices", getDevices).catch(() => []),
    ql.length >= 2 ? searchPlates(q).catch(() => []) : Promise.resolve([]),
    ql ? cached("persons", getPersons).catch(() => []) : Promise.resolve([]),
    ql ? cached("speakers", getSpeakers).catch(() => []) : Promise.resolve([]),
  ]);
  if (my !== seq || !modal || modal.hidden) return; // superseded or closed mid-flight

  const cams = devices
    .filter((d) => !ql || (d.displayName || d.id).toLowerCase().includes(ql) || d.id.toLowerCase().includes(ql))
    .slice(0, 6)
    .map(cameraRow);

  const when = parseWhen(q);

  const plateRows = (plates || [])
    .filter((p) => !p.archived)
    .slice(0, 6)
    .map(plateRow);

  // Client-side substring over the named catalog (unnamed entries have nothing to match).
  const people = (persons || [])
    .filter((p) => !p.archived && named(p) && p.display_name.toLowerCase().includes(ql))
    .slice(0, 5)
    .map(personRow);
  const voices = (speakers || [])
    .filter((s) => !s.archived && named(s) && s.display_name.toLowerCase().includes(ql))
    .slice(0, 5)
    .map(speakerRow);

  render(
    [
      ["Cameras", cams],
      ["Jump to time", when != null ? [timeRow(when)] : []],
      ["Plates", plateRows],
      ["People & Voices", [...people, ...voices]],
      ["Ask the AI", q ? [askRow(q)] : []],
    ],
    q,
  );
}

function render(groups, q) {
  rows = [];
  hi = -1;
  const frag = [];
  for (const [title, rs] of groups) {
    if (!rs.length) continue;
    frag.push(
      el("div", { class: "omni-group" }, el("div", { class: "omni-group-title", text: title }), rs.map((r) => r.node)),
    );
    rows.push(...rs);
  }
  if (!rows.length) {
    results.replaceChildren(
      el("div", {
        class: "omni-empty muted",
        text: q
          ? "No matches."
          : "Type to search cameras, plates, people & voices — or a time like “yesterday 5pm”.",
      }),
    );
    return;
  }
  results.replaceChildren(...frag);
  setHi(0);
}

function setHi(i) {
  if (hi >= 0 && rows[hi]) rows[hi].node.classList.remove("hi");
  hi = i;
  if (hi >= 0 && rows[hi]) {
    rows[hi].node.classList.add("hi");
    rows[hi].node.scrollIntoView({ block: "nearest" });
  }
}

function moveHi(dir) {
  if (!rows.length) return;
  setHi((hi + dir + rows.length) % rows.length);
}

// ---- modal shell --------------------------------------------------------------------

function build() {
  if (modal) return;
  input = el("input", {
    class: "omni-input",
    type: "text",
    placeholder: "Search cameras, times, plates, people… or ask the AI",
    autocomplete: "off",
    spellcheck: "false",
    "aria-label": "Search everything",
  });
  results = el("div", { class: "omni-results" });
  const foot = el(
    "div",
    { class: "omni-foot muted" },
    el("span", {}, el("kbd", { text: "↑↓" }), " navigate"),
    el("span", {}, el("kbd", { text: "↵" }), " open"),
    el("span", {}, el("kbd", { text: "esc" }), " close"),
  );
  const card = el(
    "div",
    { class: "modal-card omni-card" },
    el("div", { class: "omni-input-row" }, el("span", { class: "omni-glyph", text: "⌕" }), input),
    results,
    foot,
  );
  modal = el("div", { class: "modal omni-modal", hidden: true }, card);
  modal.setAttribute("aria-label", "Search");
  document.body.appendChild(modal);

  ctl = wireModal(modal, {
    initialFocus: input,
    onOpen: () => {
      input.value = "";
      query("");
    },
  });

  input.addEventListener("input", () => {
    clearTimeout(debounceT);
    debounceT = setTimeout(() => query(input.value.trim()), DEBOUNCE_MS);
  });
  input.addEventListener("keydown", (e) => {
    if (e.key === "ArrowDown") {
      e.preventDefault();
      moveHi(1);
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      moveHi(-1);
    } else if (e.key === "Enter") {
      e.preventDefault();
      (rows[hi] ?? rows[0])?.activate();
    }
  });
}

function open() {
  build();
  if (!ctl.isOpen()) ctl.open();
}

// Global hotkeys, mirroring app.js's guards (which this module cannot edit): never fire
// while another dialog owns the screen, and "/" must keep typing plain in form fields.
document.addEventListener("keydown", (e) => {
  const isK = (e.key === "k" || e.key === "K") && (e.metaKey || e.ctrlKey) && !e.altKey && !e.shiftKey;
  const isSlash = e.key === "/" && !e.metaKey && !e.ctrlKey && !e.altKey;
  if (!isK && !isSlash) return;
  const openModalEl = document.querySelector(".modal:not([hidden])");
  if (openModalEl && openModalEl !== modal) return; // another dialog owns the keyboard
  if (isSlash) {
    if (openModalEl) return; // the palette itself is open — "/" just types into it
    const t = e.target;
    if (t && (t.tagName === "INPUT" || t.tagName === "SELECT" || t.tagName === "TEXTAREA" || t.isContentEditable))
      return;
    e.preventDefault();
    open();
  } else {
    e.preventDefault(); // keep the browser's own Cmd/Ctrl+K (search bar) out of it
    if (openModalEl === modal && ctl?.isOpen()) ctl.close(); // Cmd/Ctrl+K toggles
    else open();
  }
});
