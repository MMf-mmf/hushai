// Cameras grid: one poster tile per device, refreshed every 10s, plus a single shared
// live "peek" hls.js player promoted into at most ONE tile at a time — N always-live
// players would cost N ffmpeg remuxes forever, posters + one promoted player stays cheap
// at any camera count (plan C2). Tiles are keyed and updated in place so a running peek
// survives the poll ticks; clicking a tile deep-links into the viewer (/?device=&t=).

import { getDevices, getEventFeed, posterUrl, masterUrl } from "../api.js";
import { el, emptyState, renderBanner } from "../dom.js";
import { createPoller } from "../poll.js";
import { initTopbar, setLive, setUpdated } from "../nav.js";
import { toast } from "../toast.js";
import { Player } from "../player.js";
import { clock } from "../time.js";

const $ = (id) => document.getElementById(id);
const REFRESH_MS = 10_000; // posters, device list, and alert counts all ride this tick

// Latest-footage recency → status; the same thresholds the dashboard's camera states use
// (<15s live, <5m idle, else offline) so the two pages never disagree about a camera.
const LIVE_MS = 15_000;
const IDLE_MS = 5 * 60_000;

// Peek plays the last ~60s of footage, entered near the live edge. The 4s lag mirrors
// the click-through deep-link (`t = latest - 4000`) so peek and viewer land on the same
// moment, and keeps the seek off the still-uploading newest segment.
const PEEK_WINDOW_MS = 60_000;
const PEEK_EDGE_LAG_MS = 4_000;

let everLoaded = false;
const tiles = new Map(); // deviceId -> tile record (see makeTile)
let peek = null; // { deviceId, player, video } — at most one site-wide
let unacked = new Map(); // deviceId -> unacknowledged feed-alert count

// ---- derivations ------------------------------------------------------------

function statusOf(latestMs, nowMs) {
  if (latestMs == null) return { klass: "unknown", label: "no footage" };
  const age = nowMs - latestMs;
  if (age < LIVE_MS) return { klass: "up", label: "live" };
  if (age < IDLE_MS) return { klass: "degraded", label: "idle" };
  return { klass: "down", label: "offline" };
}

function agoLabel(latestMs, nowMs) {
  if (latestMs == null) return "never seen";
  const s = Math.max(0, Math.round((nowMs - latestMs) / 1000));
  if (s < 60) return `last seen ${s}s ago`;
  if (s < 3600) return `last seen ${Math.round(s / 60)}m ago`;
  if (s < 86400) return `last seen ${Math.round(s / 3600)}h ago`;
  return `last seen ${Math.round(s / 86400)}d ago`;
}

// The viewer honors /?device=<id>&t=<ms> (see app.js init) — land 4s shy of the newest
// footage so playback starts on a fully uploaded segment.
function viewerHref(d) {
  const dev = `/?device=${encodeURIComponent(d.id)}`;
  return d.latestMs != null ? `${dev}&t=${Math.round(d.latestMs - PEEK_EDGE_LAG_MS)}` : dev;
}

// Poster with a cache-buster so every tick fetches a fresh frame (the endpoint serves
// max-age=5; the extra param defeats any in-between cache). Tolerates posterUrl ever
// growing its own query string.
function bustPoster(rec) {
  const base = posterUrl(rec.device.id);
  rec.img.src = `${base}${base.includes("?") ? "&" : "?"}_=${Date.now()}`;
}

// ---- live peek (the single shared player) ------------------------------------

function stopPeek() {
  if (!peek) return;
  const rec = tiles.get(peek.deviceId);
  peek.player.destroy();
  peek.video.remove();
  peek = null;
  if (rec) {
    syncFrame(rec);
    bustPoster(rec); // come back to a current frame, not the pre-peek one
  }
}

function startPeek(rec) {
  const d = rec.device;
  if (d.latestMs == null) {
    toast("No footage yet for this camera.", { kind: "info" });
    return;
  }
  stopPeek(); // at most one live player site-wide
  const video = el("video", { class: "peek-video", playsinline: true });
  video.muted = true; // autoplay policy: peeks start muted
  rec.frame.appendChild(video);
  const player = new Player(video, {
    onError: (msg) => {
      toast(msg, { kind: "error" });
      stopPeek();
    },
    onNotice: (msg) => toast(msg, { kind: "info" }),
  });
  peek = { deviceId: d.id, player, video };
  const toMs = d.latestMs;
  const fromMs = toMs - PEEK_WINDOW_MS;
  player.load(masterUrl(d.id, fromMs, toMs), {
    seekMs: Math.max(fromMs, toMs - PEEK_EDGE_LAG_MS),
    autoplay: true,
  });
  syncFrame(rec);
}

function togglePeek(rec) {
  if (peek && peek.deviceId === rec.device.id) stopPeek();
  else startPeek(rec);
}

// ---- tiles --------------------------------------------------------------------

// Reconcile the frame's three faces (poster / "no video yet" / peek video) + the ▶/✕
// toggle from the current state. Single choke point so the faces can't drift.
function syncFrame(rec) {
  const peeking = !!peek && peek.deviceId === rec.device.id;
  rec.root.classList.toggle("peeking", peeking);
  rec.img.hidden = peeking || !rec.posterOk;
  rec.noVideo.hidden = peeking || rec.posterOk;
  rec.btn.textContent = peeking ? "✕" : "▶";
  const label = peeking ? "Stop live peek" : "Live peek";
  rec.btn.title = label;
  rec.btn.setAttribute("aria-label", `${label} — ${rec.device.displayName || rec.device.id}`);
}

function makeTile(d) {
  const img = el("img", { class: "poster", alt: "" });
  const noVideo = el("div", { class: "no-video muted", text: "no video yet", hidden: true });
  const btn = el("button", { class: "peek-btn", type: "button", text: "▶" });
  const frame = el("div", { class: "frame" }, img, noVideo, btn);
  // A stretched link (not a wrapping <a>) keeps the markup valid with a button inside
  // the tile, while the whole tile still middle-clicks / cmd-clicks like a link.
  const link = el("a", { class: "tile-link", href: viewerHref(d) });
  const nameEl = el("span", { class: "cam-name" });
  const chip = el("span", { class: "alert-chip", hidden: true });
  const dot = el("span", { class: "dot" });
  const stateEl = el("span", { class: "pill" });
  const agoEl = el("span", { class: "cam-ago muted small" });
  const root = el(
    "div",
    { class: "cam-tile" },
    link,
    frame,
    el(
      "div",
      { class: "cam-meta" },
      el("div", { class: "cam-name-row" }, nameEl, chip),
      el("div", { class: "cam-status-row" }, dot, stateEl, agoEl),
    ),
  );
  const rec = { device: d, root, link, frame, img, noVideo, btn, nameEl, chip, dot, stateEl, agoEl, posterOk: false };
  img.addEventListener("load", () => {
    rec.posterOk = true;
    syncFrame(rec);
  });
  img.addEventListener("error", () => {
    rec.posterOk = false;
    syncFrame(rec);
  });
  btn.addEventListener("click", (e) => {
    e.preventDefault();
    e.stopPropagation();
    togglePeek(rec);
  });
  // Click-through ends the peek (navigation tears it down anyway; this also covers
  // modified clicks that open a new tab and leave this page running).
  link.addEventListener("click", stopPeek);
  syncFrame(rec);
  return rec;
}

function updateTile(rec, d, nowMs) {
  rec.device = d;
  const friendly = d.displayName || d.id;
  rec.nameEl.textContent = friendly;
  rec.nameEl.title = d.id; // keep the raw id discoverable, like the dashboard cards
  rec.nameEl.classList.toggle("mono", !d.displayName);
  rec.link.href = viewerHref(d);
  rec.link.setAttribute("aria-label", `Open ${friendly} in the viewer`);
  const st = statusOf(d.latestMs, nowMs);
  rec.dot.className = `dot ${st.klass}`;
  rec.stateEl.className = `pill ${st.klass}`;
  rec.stateEl.textContent = st.label;
  rec.agoEl.textContent = agoLabel(d.latestMs, nowMs);
  const n = unacked.get(d.id) || 0;
  rec.chip.hidden = n === 0;
  rec.chip.textContent = String(n);
  rec.chip.title = `${n} unacknowledged alert${n === 1 ? "" : "s"}`;
  // Refresh the poster except while this tile is the live peek (the img is hidden and
  // the player is already showing fresher frames than any poster).
  if (!peek || peek.deviceId !== d.id) bustPoster(rec);
}

function render(devs) {
  const grid = $("grid");
  const nowMs = Date.now();
  if (!devs.length) {
    stopPeek();
    tiles.clear();
    grid.replaceChildren(emptyState("No cameras have connected yet."));
    return;
  }
  if (!tiles.size) grid.replaceChildren(); // clear a previous empty-state block
  const seen = new Set();
  for (const d of devs) {
    seen.add(d.id);
    let rec = tiles.get(d.id);
    if (!rec) {
      rec = makeTile(d);
      tiles.set(d.id, rec);
      grid.appendChild(rec.root);
    }
    updateTile(rec, d, nowMs);
  }
  for (const [id, rec] of tiles) {
    if (!seen.has(id)) {
      if (peek && peek.deviceId === id) stopPeek();
      rec.root.remove();
      tiles.delete(id);
    }
  }
}

// ---- data + poll loop -----------------------------------------------------------

// Alert-chip counts: one feed fetch per tick, unacknowledged items bucketed by device.
// Best-effort — a feed hiccup must not blank the camera grid, so failures keep the last
// counts and only warn.
async function refreshFeed() {
  try {
    const feed = await getEventFeed({ limit: 100 });
    const m = new Map();
    for (const item of feed) {
      if (item.acknowledged || !item.deviceId) continue;
      m.set(item.deviceId, (m.get(item.deviceId) || 0) + 1);
    }
    unacked = m;
  } catch (e) {
    console.warn("cameras: event feed unavailable", e);
  }
}

async function refresh() {
  try {
    const [devs] = await Promise.all([getDevices(), refreshFeed()]);
    everLoaded = true;
    $("camBanner").hidden = true;
    setLive(true);
    setUpdated(`updated ${clock(Date.now())}`);
    render(devs);
  } catch (e) {
    setLive(false);
    setUpdated("disconnected");
    // Text-only banner: e.message can echo server output and must never be parsed as HTML.
    renderBanner($("camBanner"), {
      mode: "error",
      message: everLoaded
        ? `Lost connection to the viewer — retrying every ${REFRESH_MS / 1000}s.`
        : "Couldn't load the camera list. Make sure hushai-viewer is running, then this page recovers automatically.",
      detail: String(e.message || e),
    });
  }
}

// createPoller supplies the busy-guard, the hidden-tab pause (posters stop refetching in
// background tabs), and the refocus refresh. refresh() renders its own error state.
const poller = createPoller(refresh, { intervalMs: REFRESH_MS });

initTopbar({ section: "cameras" });
poller.start();

// Debug handle for local automated verification, gated to localhost so a LAN/tunnel
// deploy doesn't hand page internals to any visitor (same pattern as viewerDebug).
if (["localhost", "127.0.0.1", "::1"].includes(location.hostname)) {
  window.camerasDebug = {
    deviceCount: () => tiles.size,
    peek: (id) => {
      const rec = tiles.get(id);
      if (rec) togglePeek(rec);
      return !!rec;
    },
    activePeek: () => (peek ? peek.deviceId : null),
  };
}
