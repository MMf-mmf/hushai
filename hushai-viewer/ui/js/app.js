// App orchestration: device list, player, timeline, controls, keyboard. Holds the
// single source of truth and wires the pieces. View window (the bar) is decoupled
// from the loaded HLS window; we only reload the playlist when a seek lands outside
// the loaded window. For typical day-sized data the whole device loads as one window.

import {
  getDevices,
  getTimeline,
  getProcessing,
  getEvents,
  getEventFeed,
  ackDelivery,
  masterUrl,
} from "./api.js";
import { Player } from "./player.js";
import { Timeline } from "./timeline.js";
import { Detections } from "./detections.js";
import { initEventsOverlay, initAlertBell } from "./events-overlay.js";
import { initExportBar } from "./export-range.js";
import * as thumbs from "./thumbs.js";
import { clock, clockMs, dateLabel, localDateInput, DAY_MS, tzAbbr, humanDur } from "./time.js";
import { on, setPlaybackProvider } from "./store.js";
import { createPoller } from "./poll.js";
import { wireModal } from "./modal.js";

const $ = (id) => document.getElementById(id);
const WINDOW_MS = 6 * 3600 * 1000; // matches backend VIEWER_MAX_WINDOW_NANOS default
const REFRESH_MS = 6000; // how often we poll for newly-ingested footage (devices + timeline)
const FOLLOW_EPS_MS = 5000; // the view counts as "parked at the live edge" within this of latest
const RATES = [0.25, 0.5, 1, 2, 4, 8]; // the <'/'>' speed ladder (and the #speed options)
const LIVE_BEHIND_MS = 4000; // go-live seeks land this far behind latest (encode/ingest headroom)
const LIVE_NEAR_MS = 10_000; // playhead within this of latest counts as "watching live"
const LIVE_EDGE_FRESH_MS = 30_000; // latest footage younger than this draws the live-edge cap
const FOLLOW_RELOAD_EPS_MS = 8000; // follow-live reloads once playback rides this close to the loaded edge

const state = {
  devices: [],
  device: null,
  view: { fromMs: 0, toMs: 1 },
  loaded: null,
  timeline: null,
  events: new Map(), // event id -> event, for the selected camera (lane + drawer share it)
  playing: false,
  rate: 1,
  userMuted: true,
  detMode: false,
  aiEnabled: true, // AI processing-status ribbons on the scrub bar
  seekIntent: null, // {ms, at}: optimistic playhead target while a seek is still landing
  followLive: false, // LIVE pill engaged: auto-chase newest footage as it lands
  exportMode: false, // ✂ clip export: timeline drags select a range, export bar shown
};

const EVENTS_CAP = 3000; // soft cap on the in-memory event map (oldest dropped)
const PREVIEW_W = 168; // #tlPreview outer width — matches its CSS

let player, timeline, detections, toastTimer, helpModal;
let eventsOverlay = null,
  alertBell = null,
  exportBar = null;
let refreshPoller = null,
  lastDeviceSig = "";
let evSeq = 0, // stale-response guard for the events fetches (bumped per device switch)
  eventsMaxCreatedMs = 0, // newest createdMs seen — the incremental fetch's `since`
  refreshTickN = 0; // the alert feed refreshes on every other auto-refresh tick
let procSeq = 0, // stale-response guard for the processing-status fetch
  procDebounce = null,
  lastBadgeKey = "",
  lastA11yTick = 0; // 1 Hz throttle for ARIA slider values + LIVE pill + live-edge cap

async function init() {
  const video = $("video");
  video.muted = true; // allow autoplay; user unmutes
  player = new Player(video, { onError: showError, onNotice: toast });
  timeline = new Timeline($("timeline"), {
    // Alt-drag scrubs pass exact:true so the raw millisecond survives (no gap-snap).
    onSeek: (ms, opts = {}) => seekTo(ms, { play: true, exact: !!opts.exact }),
    onWindowChange: (from, to) => {
      state.view = { fromMs: from, toMs: to };
      scheduleProcessingRefetch(); // pan/zoom changed the visible window
    },
    onHover: (ms) => onTimelineHover(ms), // drives the #tlPreview thumbnail
    // Export mode: drag-made/adjusted selections feed the export bar's readout + href.
    onSelectionChange: (sel) => exportBar?.setRange(sel?.fromMs ?? null, sel?.toMs ?? null),
    // A marker on the events lane jumps the player to that moment.
    onEventClick: (ev) => seekTo(ev.startMs, { play: true }),
  });
  // Events drawer (rail next to the stage) + topbar alert bell. Data flows from here:
  // the drawer gets the selected camera's events pushed (shared with the timeline lane)
  // and only fetches for itself in its all-cameras scope.
  eventsOverlay = initEventsOverlay({
    onSeek: (ms) => seekTo(ms, { play: true }),
    onSelectDevice: (id, ms) => selectDevice(id, { seekMs: ms }),
    fetchEvents: getEvents,
  });
  // Export bar (floats above the scrub bar): renders the export-mode selection and
  // hands out the MP4 download link. Same thin coupling as the events overlay — it
  // only renders; mode + selection live here / in the timeline.
  exportBar = initExportBar({
    getDevice: () => state.device,
    isCovered: (ms) => timeline.isCovered(ms),
    onExit: () => setExportMode(false),
  });
  alertBell = initAlertBell({
    fetchFeed: () => getEventFeed({ limit: 100 }),
    ack: ackDelivery,
    onView: (item) => {
      if (item.eventStartMs == null) return;
      if (
        item.deviceId &&
        item.deviceId !== state.device?.id &&
        state.devices.some((d) => d.id === item.deviceId)
      ) {
        selectDevice(item.deviceId, { seekMs: item.eventStartMs });
      } else {
        seekTo(item.eventStartMs, { play: true });
      }
    },
  });
  detections = new Detections($("detOverlay"), video);
  detections.onError = (e) => toast("detections: " + (e?.message || e));
  // The loaded HLS window ran dry: auto-advance across gaps / follow the live edge.
  video.addEventListener("ended", onVideoEnded);
  wireControls();
  wireKeys();
  startTicker();
  // Chat citation click -> jump the video here (switching device if needed).
  on("seekToCitation", (e) => seekToCitation(e.detail));
  // Chat pulls "what's on screen right now" (camera + wall-clock playhead) per send, so the
  // backend can scope deictic questions ("who was speaking in this clip") to the open video.
  // The closure reads live state, so registering before a device is selected is fine.
  setPlaybackProvider(() => {
    const ms = player ? player.currentWallClockMs() : 0;
    if (!state.device || !isFinite(ms) || ms <= 0) return null;
    return { deviceId: state.device.id, playheadMs: ms };
  });
  $("tzLabel").textContent = tzAbbr();

  try {
    state.devices = await getDevices();
  } catch (e) {
    showError("Could not load devices: " + e.message);
    startAutoRefresh(); // keep polling so we recover automatically once the backend is reachable
    return;
  }
  // Small debug handle for local automated verification (this is a localhost-only tool),
  // gated to localhost so a LAN/tunnel deploy doesn't hand app internals to any visitor.
  if (["localhost", "127.0.0.1", "::1"].includes(location.hostname)) {
    window.viewerDebug = {
      seekTo,
      setMode,
      setProcessingEnabled,
      goLive,
      state,
      get player() {
        return player;
      },
      get timeline() {
        return timeline;
      },
      currentMs: () => (player ? player.currentWallClockMs() : 0),
      events: () => state.events.size,
      exportRange: () => timeline.getSelection(),
      setExportMode,
    };
  }
  lastDeviceSig = deviceSig(state.devices);
  populateDeviceSelect();
  const usable = state.devices.filter((d) => d.segmentCount > 0 && d.latestMs);
  // Deep-link from the Events feed / omni-search: /?device=<id>[&t=<ms>] opens that
  // camera, at that instant when `t` is given, else at its default landing spot.
  const params = new URLSearchParams(location.search);
  const linkDevice = params.get("device");
  const linkMs = Number(params.get("t"));
  if (linkDevice && state.devices.some((d) => d.id === linkDevice)) {
    selectDevice(linkDevice, { seekMs: isFinite(linkMs) && linkMs > 0 ? linkMs : null });
  } else if (usable.length) {
    const def = usable.slice().sort((a, b) => b.segmentCount - a.segmentCount)[0];
    selectDevice(def.id);
  } else {
    showStatus("No cameras have reported footage yet.");
  }
  alertBell?.refresh(); // badge appears promptly; the poller keeps it fresh after this
  startAutoRefresh(); // pick up new footage / new cameras on their own, no manual reload
}

function populateDeviceSelect() {
  const sel = $("deviceSelect");
  sel.innerHTML = "";
  for (const d of state.devices) {
    const opt = document.createElement("option");
    opt.value = d.id;
    const span = d.earliestMs && d.latestMs ? humanDur(d.latestMs - d.earliestMs) : "—";
    // Prefer the operator-assigned friendly name (set on the Files page); fall back to the id.
    opt.textContent = `${d.displayName || d.id} · ${d.segmentCount} segs · ${span}`;
    sel.appendChild(opt);
  }
  sel.onchange = () => selectDevice(sel.value);
}

async function selectDevice(id, { seekMs = null } = {}) {
  const d = state.devices.find((x) => x.id === id);
  if (!d) return;
  state.followLive = false; // switching cameras is manual navigation — drop live-follow
  setExportMode(false); // a selection's times belong to the old camera — don't carry it over
  state.device = d;
  $("deviceSelect").value = id;
  setDeviceMeta(d);
  // Events belong to the camera: drop the old set immediately (no stale markers while
  // the new fetch is in flight), then load this camera's recent events.
  evSeq++;
  state.events.clear();
  eventsMaxCreatedMs = 0;
  pushEvents();
  eventsOverlay?.setDeviceId(id);
  refetchEvents();

  const earliest = d.earliestMs ?? Date.now();
  const latest = d.latestMs ?? Date.now();
  timeline.setBounds(earliest, latest);
  const pad = Math.max((latest - earliest) * 0.02, 2000);
  state.view = { fromMs: earliest - pad, toMs: latest + pad };
  timeline.setWindow(state.view.fromMs, state.view.toMs);
  $("dateInput").value = localDateInput(latest);
  $("dayLabel").textContent = dateLabel(latest);

  await refetchTimeline();
  refetchProcessing();
  // A citation deep-link supplies its target so we open the window there directly
  // (instead of the first run) — no visible double-jump. Otherwise open on the first
  // video so the user lands on a picture, falling back to the first run / device start.
  const startMs = seekMs ?? firstVideoMs() ?? state.timeline?.coverage?.[0]?.startMs ?? earliest;
  loadWindowAround(startMs, { seekMs: startMs, play: true });
}

// Jump the player to a chat citation. Switches device first (opening the window at the
// cited moment) when the citation is on a different camera; otherwise reuses the normal
// seek path (window reload + snap-to-covered + toast all handled by seekTo).
async function seekToCitation({ deviceId, ms }) {
  if (!ms || !isFinite(ms)) return;
  if (deviceId && deviceId !== state.device?.id && state.devices.some((d) => d.id === deviceId)) {
    await selectDevice(deviceId, { seekMs: ms });
  } else {
    seekTo(ms, { play: true });
  }
}

async function refetchTimeline() {
  const d = state.device;
  try {
    state.timeline = await getTimeline(d.id, d.earliestMs, d.latestMs);
    timeline.setData({
      coverage: state.timeline.coverage,
      sessionBoundariesMs: state.timeline.sessionBoundariesMs,
    });
    if (!state.timeline.coverage.length) showStatus("No footage in range.");
    else hideStatus();
  } catch (e) {
    showError("timeline: " + e.message);
  }
}

// AI processing-status ribbons. Fetched for the *visible* window (cheap) and refreshed on
// its own — independent of the device-signature short-circuit in refreshDevices(), because
// processing advances as the worker catches up even when no new footage arrives.
async function refetchProcessing() {
  const d = state.device;
  if (!d || !state.aiEnabled) return;
  const seq = ++procSeq;
  try {
    const p = await getProcessing(d.id, state.view.fromMs, state.view.toMs);
    if (seq !== procSeq) return; // superseded by a newer device/window
    timeline.setProcessing({ audio: p.audio, vision: p.vision });
  } catch {
    // transient (backend restart / window change mid-flight) — next tick retries
  }
}

// Debounce window-change-driven refetches so dragging the scrub bar doesn't spam the API.
function scheduleProcessingRefetch() {
  if (!state.aiEnabled) return;
  clearTimeout(procDebounce);
  procDebounce = setTimeout(refetchProcessing, 250);
}

// ---- events (timeline lane + drawer share state.events) ----------------------

// Fold fetched events into state.events; returns whether anything new landed and
// advances the incremental-fetch watermark (max createdMs seen).
function mergeEvents(list) {
  let changed = false;
  for (const ev of list) {
    if (!state.events.has(ev.id)) changed = true;
    state.events.set(ev.id, ev);
    if (ev.createdMs > eventsMaxCreatedMs) eventsMaxCreatedMs = ev.createdMs;
  }
  // Soft cap: a long-lived tab on a busy camera shouldn't grow without bound.
  if (state.events.size > EVENTS_CAP) {
    const sorted = [...state.events.values()].sort((a, b) => a.startMs - b.startMs);
    for (const ev of sorted.slice(0, state.events.size - EVENTS_CAP)) state.events.delete(ev.id);
  }
  return changed;
}

// One push point: the timeline lane and the events drawer always see the same list.
function pushEvents() {
  const list = [...state.events.values()];
  timeline.setEvents(list);
  eventsOverlay?.setEvents(list);
}

// Full fetch on device select (recent 500). Guarded by evSeq so a fast camera switch
// can't land a stale camera's events.
async function refetchEvents() {
  const d = state.device;
  if (!d) return;
  const seq = evSeq;
  try {
    const list = await getEvents({ deviceId: d.id, limit: 500 });
    if (seq !== evSeq) return; // superseded by a newer device selection
    if (mergeEvents(list)) pushEvents();
  } catch {
    // transient — the 6s incremental tick doubles as the retry
  }
}

// Incremental merge folded into the 6s auto-refresh: only events created since the
// watermark (or a bounded recent slice when we have nothing yet).
async function refetchEventsIncremental() {
  const d = state.device;
  if (!d) return;
  const seq = evSeq;
  try {
    const list = await getEvents({
      deviceId: d.id,
      sinceMs: eventsMaxCreatedMs || undefined,
      limit: 200,
    });
    if (seq !== evSeq) return;
    if (mergeEvents(list)) pushEvents();
  } catch {
    // transient — next tick retries
  }
}

function setDeviceMeta(d) {
  const kinds = [d.hasVideo && "video", d.hasAudio && "audio", d.hasMuxed && "muxed"]
    .filter(Boolean)
    .join(" + ");
  $("deviceMeta").textContent = `${d.segmentCount} segments · ${d.sessionCount} sessions · ${kinds}`;
}

// ---- live auto-refresh ------------------------------------------------------
// New footage is ingested continuously, but the page used to fetch the device list and
// timeline only once at load — so anything captured afterward stayed invisible until a
// manual reload. We poll on an interval and fold in whatever is new: the timeline grows,
// the selected camera's range extends, and a camera that first reports while the page is
// open gets adopted automatically. We deliberately never touch the player or the user's
// zoom/pan — we only slide the view to follow the live edge when it's already parked there.

// A fingerprint that changes exactly when there is new footage to show: a later live edge,
// more segments, or a new camera. Lets the poll do nothing (no DOM churn) when nothing changed.
function deviceSig(devices) {
  return devices.map((d) => `${d.id}:${d.segmentCount}:${d.latestMs ?? 0}`).join("|");
}

async function refreshDevices() {
  let devices;
  try {
    devices = await getDevices();
  } catch {
    return; // transient (e.g. backend restart) — try again next tick
  }
  const sig = deviceSig(devices);
  if (sig === lastDeviceSig) return; // nothing new since the last poll
  lastDeviceSig = sig;
  state.devices = devices;
  populateDeviceSelect();
  if (state.device) $("deviceSelect").value = state.device.id;

  // Empty-state recovery: footage (or the very first camera) just appeared.
  if (!state.device) {
    const usable = devices.filter((d) => d.segmentCount > 0 && d.latestMs);
    if (usable.length) {
      hideStatus();
      const def = usable.slice().sort((a, b) => b.segmentCount - a.segmentCount)[0];
      selectDevice(def.id);
    }
    return;
  }

  // Fold fresh data into the currently-selected camera.
  const fresh = devices.find((d) => d.id === state.device.id);
  if (!fresh) return; // selected camera vanished (shouldn't happen) — leave the view alone
  const prevLatest = state.device.latestMs ?? 0;
  state.device = fresh;
  setDeviceMeta(fresh);

  const earliest = fresh.earliestMs ?? Date.now();
  const latest = fresh.latestMs ?? Date.now();
  timeline.setBounds(earliest, latest);
  // Only chase the live edge if the view is already parked at it; if the user has panned or
  // zoomed into older footage, keep their window put and just let the coverage band grow.
  if (latest > prevLatest && state.view.toMs >= prevLatest - FOLLOW_EPS_MS) {
    const pad = Math.max((latest - earliest) * 0.02, 2000);
    state.view = { fromMs: earliest - pad, toMs: latest + pad };
    timeline.setWindow(state.view.fromMs, state.view.toMs);
  }
  await refetchTimeline();
  maybeFollowLive(fresh);
}

// Follow loop (piggybacks the 6s device poll): when new footage lands beyond the
// loaded HLS window and playback is riding its edge, reload the window at the live
// edge and keep rolling. The playlist is a fixed from..to window, so following MUST
// reload — a plain seek near the old edge would just park on the last old fragment.
function maybeFollowLive(fresh) {
  if (!state.followLive || !fresh?.latestMs) return;
  const target = fresh.latestMs - LIVE_BEHIND_MS;
  const lr = player.loadedRangeMs();
  if (!lr) {
    seekTo(target, { play: true, fromLive: true });
    return;
  }
  const ms = player.currentWallClockMs();
  const riding = !(Number.isFinite(ms) && ms > 0) || lr.toMs - ms < FOLLOW_RELOAD_EPS_MS;
  if (fresh.latestMs > lr.toMs + 1000 && riding) {
    // Continue from the playhead when it's already near the edge (seamless); a stale
    // playhead (hidden tab, big backlog) jumps straight to just-behind-latest.
    const at = Math.max(Number.isFinite(ms) && ms > 0 ? ms : 0, target);
    state.seekIntent = { ms: at, at: performance.now() };
    timeline.setPlayhead(at);
    loadWindowAround(at, { seekMs: at, play: true });
  }
}

function startAutoRefresh() {
  if (refreshPoller) return; // idempotent (init reaches here from two paths)
  refreshPoller = createPoller(
    async () => {
      await refreshDevices();
      // Always refresh AI status (not gated by the new-footage signature): processing
      // advances as the worker catches up on already-ingested segments.
      await refetchProcessing();
      // New events for the open camera (incremental since the createdMs watermark).
      await refetchEventsIncremental();
      // The alert feed moves slower than footage — every other tick is plenty.
      if (refreshTickN++ % 2 === 0) await alertBell?.refresh();
    },
    { intervalMs: REFRESH_MS },
  );
  // First tick after one interval (matching the old setInterval cadence). The poller
  // busy-guards, pauses while the tab is hidden, and re-syncs the moment it's focused again.
  refreshPoller.start({ immediate: false });
}

// The continuous coverage span (recorded run) containing `ms`, or null if `ms` is in a gap.
function spanContaining(ms) {
  return (state.timeline?.coverage ?? []).find((c) => ms >= c.startMs && ms <= c.endMs) ?? null;
}
// The coverage span nearest to `ms` (used when `ms` lands in a gap or outside all runs).
function nearestSpan(ms) {
  let best = null,
    bestD = Infinity;
  for (const c of state.timeline?.coverage ?? []) {
    const d = ms < c.startMs ? c.startMs - ms : ms > c.endMs ? ms - c.endMs : 0;
    if (d < bestD) {
      bestD = d;
      best = c;
    }
  }
  return best;
}
// Earliest moment that actually has VIDEO, so we open on a picture rather than an
// audio-only stretch (e.g. a session that recorded sound before the camera started).
function firstVideoMs() {
  const vs = (state.timeline?.spans ?? []).filter((s) => s.kind === "video");
  return vs.length ? Math.min(...vs.map((s) => s.startMs)) : null;
}

// The HLS window to load around `centerMs`. We load ONE continuous run (a single coverage
// span), NOT the whole device range. A window that straddles a gap or an audio-only stretch
// desyncs hls.js's separate audio rendition from the video track (audio "found no media" ->
// the frame freezes on play). Very long runs are capped to WINDOW_MS around the playhead
// (the backend clamps to the same max independently).
function computeWindow(centerMs) {
  const d = state.device;
  const span = spanContaining(centerMs) ?? nearestSpan(centerMs);
  let from = span ? span.startMs : d.earliestMs;
  let to = span ? span.endMs : d.latestMs;
  if (to - from > WINDOW_MS) {
    from = Math.max(from, Math.min(centerMs - WINDOW_MS / 2, to - WINDOW_MS));
    to = from + WINDOW_MS;
  }
  return { fromMs: from, toMs: to };
}

function loadWindowAround(centerMs, { seekMs = centerMs, play = true } = {}) {
  const d = state.device;
  state.loaded = computeWindow(centerMs);
  player.load(masterUrl(d.id, state.loaded.fromMs, state.loaded.toMs), {
    seekMs,
    autoplay: play,
  });
  state.playing = play;
  updatePlayBtn();
  // Keep detection boxes in step with the (re)loaded video window. A far seek reloads
  // the window here, so this is also the natural refetch trigger.
  if (state.detMode) detections.setWindow(d.id, state.loaded.fromMs, state.loaded.toMs);
}

// Switch between plain Video and the Detections overlay. The same video keeps playing.
function setMode(on) {
  state.detMode = on;
  $("modeVideo").classList.toggle("active", !on);
  $("modeVideo").setAttribute("aria-selected", String(!on));
  $("modeDet").classList.toggle("active", on);
  $("modeDet").setAttribute("aria-selected", String(on));
  $("modeToggle").classList.toggle("pinned", on);
  detections.setActive(on);
  if (on && state.device && state.loaded) {
    detections.setWindow(state.device.id, state.loaded.fromMs, state.loaded.toMs);
  }
}

const AI_GLYPH = { done: "✓", processing: "⟳", pending: "◌", error: "✕", skipped: "∅" };

// Live "AI status under the playhead" badge in the topbar. Reports the worst of the two
// lanes with the active modality's verb. Diffed by a key so the DOM is only touched on change.
function updateAiBadge(ms) {
  const badge = $("aiStatus");
  if (!badge) return;
  if (!state.aiEnabled) {
    if (lastBadgeKey !== "off") {
      badge.hidden = true;
      lastBadgeKey = "off";
    }
    return;
  }
  const { audio, vision } = timeline.statusAt(ms);
  const a = audio?.status ?? null;
  const v = vision?.status ?? null;
  const key = `${a}|${v}`;
  if (key === lastBadgeKey) return;
  lastBadgeKey = key;
  if (!a && !v) {
    badge.hidden = true; // no AI lane here (gap / outside footage)
    return;
  }
  // worst-state precedence: error > processing > pending > done > skipped
  // (skipped ranks below done: a content-gate no-op should never mask real work/failures).
  const rank = { error: 4, processing: 3, pending: 2, done: 1, skipped: 0 };
  const worst = [a, v].filter(Boolean).sort((x, y) => rank[y] - rank[x])[0];
  let text;
  if (worst === "error") text = "Processing failed";
  else if (worst === "processing")
    text =
      a === "processing" && v === "processing"
        ? "Processing…"
        : a === "processing"
          ? "Transcribing…"
          : "Analyzing video…";
  else if (worst === "pending") text = "Not yet processed";
  else if (worst === "skipped") text = "Skipped (static/silent)";
  else text = "Processed";

  badge.hidden = false;
  badge.className = `ai-badge is-${worst}`;
  badge.querySelector(".ai-ico").textContent = AI_GLYPH[worst];
  badge.querySelector(".ai-txt").textContent = text;
}

// Toggle the AI processing-status layer (ribbons + badge + polling).
function setProcessingEnabled(on) {
  state.aiEnabled = on;
  timeline.setProcessingEnabled(on);
  const cb = $("aiStatusToggle");
  if (cb) cb.checked = on;
  if (on) refetchProcessing();
  else lastBadgeKey = ""; // let the next tick hide the badge
}

function seekTo(ms, { play = false, exact = false, fromLive = false } = {}) {
  if (!state.device) return;
  // Single live-follow choke point: any manual seek/scrub/jump drops the follow.
  // The follow loop's own seeks pass fromLive so chasing the edge doesn't un-follow.
  if (!fromLive) state.followLive = false;
  // Alt-precision (exact) seeks skip gap-snap entirely and land on the raw millisecond.
  const { ms: snapped, movedMs } = exact ? { ms, movedMs: 0 } : timeline.snapInfo(ms);
  // Sub-second snaps happen on every gap-edge click — only narrate jumps a human would notice.
  if (Math.abs(movedMs) >= 1000)
    toast(`No footage here — skipping ${humanDur(Math.abs(movedMs))} ${movedMs > 0 ? "ahead" : "back"}`);
  // Optimistic playhead: show the landing spot immediately and let the ticker hold it
  // until the player reports a nearby position (see startTicker) — otherwise the playhead
  // sits stale at the old position while a far seek reloads the HLS window.
  timeline.setPlayhead(snapped);
  state.seekIntent = { ms: snapped, at: performance.now() };
  if (!state.loaded || snapped < state.loaded.fromMs || snapped > state.loaded.toMs) {
    loadWindowAround(snapped, { seekMs: snapped, play: true });
  } else {
    player.seekToWallClock(snapped);
    if (play) {
      player.play().catch(() => {});
      state.playing = true;
      updatePlayBtn();
    }
  }
}

// ---- hover preview thumbnail --------------------------------------------------
// Driven by the timeline's onHover: over covered footage on a video-capable camera,
// float #tlPreview above the bar at the hover x with a HH:MM:SS label and a 2s-bucket
// still (thumbs.js debounces + caches). Hidden on hover-out, during any drag/pinch
// gesture, over gaps, and on audio-only cameras.

function hideTimelinePreview() {
  const box = $("tlPreview");
  if (box && !box.hidden) box.hidden = true;
  thumbs.cancel();
}

function onTimelineHover(ms) {
  const box = $("tlPreview");
  if (!box) return;
  const d = state.device;
  const show =
    ms != null &&
    d != null &&
    (d.hasVideo || d.hasMuxed) &&
    !timeline.isDragging() &&
    timeline.isCovered(ms);
  if (!show) {
    hideTimelinePreview();
    return;
  }
  const canvas = $("timeline");
  const bar = canvas.parentElement; // .timelinebar (the positioning context)
  const x = canvas.offsetLeft + timeline.xOf(ms);
  const maxLeft = Math.max(4, bar.clientWidth - PREVIEW_W - 4);
  box.style.left = `${Math.min(Math.max(x - PREVIEW_W / 2, 4), maxLeft)}px`;
  box.querySelector("span").textContent = clock(ms);
  box.hidden = false;
  thumbs.request(d.id, ms, (url) => {
    if (box.hidden) return; // hover already left while the still was loading
    const img = box.querySelector("img");
    if (url) {
      if (img.dataset.url !== url) {
        img.src = url;
        img.dataset.url = url;
      }
      img.hidden = false;
    } else {
      img.hidden = true; // no frame there — keep the time label alone
    }
  });
}

// ---- export mode (✂ button / E key) ------------------------------------------
// Select a range on the scrub bar, download it as an MP4. The timeline owns the
// selection gestures (setSelectMode/setSelection); export-range.js owns the bar UI;
// this section owns the mode itself and the i/o in-out keys.

// Where the UI believes the playhead is: the player's report, or the optimistic seek
// target / drawn playhead while a far seek is still landing (same fallback as relSeek).
function uiPlayheadMs() {
  const ms = player.currentWallClockMs();
  return Number.isFinite(ms) && ms > 0 ? ms : (state.seekIntent?.ms ?? timeline.playheadMs);
}

function pushSelectionToBar() {
  const sel = timeline.getSelection();
  exportBar?.setRange(sel?.fromMs ?? null, sel?.toMs ?? null);
}

// Enter/leave export mode. Entering flips the timeline to select gestures and seeds a
// grabbable selection around the playhead when none exists; leaving clears both the
// selection and the bar (per-use ranges shouldn't linger into the next session).
function setExportMode(on) {
  if (state.exportMode === on) return;
  state.exportMode = on;
  const btn = $("exportToggle");
  if (btn) {
    btn.classList.toggle("active", on);
    btn.setAttribute("aria-pressed", String(on));
  }
  timeline.setSelectMode(on);
  if (on) {
    if (!timeline.getSelection()) {
      const seed = seedSelection();
      if (seed) timeline.setSelection(seed.fromMs, seed.toMs);
    }
    pushSelectionToBar();
    exportBar?.setVisible(true);
  } else {
    timeline.setSelection(null);
    exportBar?.setRange(null);
    exportBar?.setVisible(false);
  }
}

// ±30s around the playhead, clamped into the device's footage bounds.
function seedSelection() {
  const d = state.device;
  if (!d) return null;
  const lo = d.earliestMs ?? timeline.boundsFrom;
  const hi = d.latestMs ?? timeline.boundsTo;
  const raw = uiPlayheadMs();
  const center = Math.min(Math.max(Number.isFinite(raw) ? raw : lo, lo), hi);
  const fromMs = Math.max(center - 30_000, lo);
  const toMs = Math.min(center + 30_000, hi);
  return toMs - fromMs >= 1000 ? { fromMs, toMs } : null;
}

// i/o (export mode): set the clip's in/out edge at the playhead. With no selection —
// or when the playhead has crossed the opposite edge — the other end re-anchors 60s
// away (i → [playhead, +60s], o → [−60s, playhead]), clamped into footage bounds.
function setSelectionPoint(edge) {
  const d = state.device;
  const raw = uiPlayheadMs();
  if (!d || !Number.isFinite(raw)) return;
  const lo = d.earliestMs ?? timeline.boundsFrom;
  const hi = d.latestMs ?? timeline.boundsTo;
  const ms = Math.min(Math.max(raw, lo), hi);
  const sel = timeline.getSelection();
  let fromMs, toMs;
  if (edge === "in") {
    fromMs = ms;
    toMs = sel && sel.toMs > ms ? sel.toMs : Math.min(ms + 60_000, hi);
  } else {
    toMs = ms;
    fromMs = sel && sel.fromMs < ms ? sel.fromMs : Math.max(ms - 60_000, lo);
  }
  if (toMs - fromMs < 500) return; // playhead pinned at a footage bound — nothing to select
  timeline.setSelection(fromMs, toMs);
  pushSelectionToBar();
}

// ---- controls ---------------------------------------------------------------

function sortedCoverage() {
  return (state.timeline?.coverage ?? []).slice().sort((a, b) => a.startMs - b.startMs);
}
// Start of the first coverage span after `ms` (null when nothing lies ahead).
function nextSpanStart(ms) {
  return sortedCoverage().find((x) => x.startMs > ms + 500)?.startMs ?? null;
}
function gotoNextSpan() {
  const t = nextSpanStart(player.currentWallClockMs());
  if (t != null) seekTo(t, { play: true });
}
function gotoPrevSpan() {
  const ms = player.currentWallClockMs();
  let target = null;
  for (const x of sortedCoverage()) if (x.startMs < ms - 1500) target = x.startMs;
  if (target != null) seekTo(target, { play: true });
}

// The loaded window ran out. Three cases: the run continues past the window cap
// (reload forward), a later run exists (narrate the gap and auto-advance), or nothing
// lies ahead (hold at the live edge when following, else just stay ended).
function onVideoEnded() {
  const reported = player.currentWallClockMs();
  const ms =
    Number.isFinite(reported) && reported > 0
      ? reported
      : (state.seekIntent?.ms ?? timeline.playheadMs ?? 0);
  if (!state.device || !ms) return;
  if (spanContaining(ms + 2000)) {
    // The run continues but the loaded window was capped mid-span: keep rolling.
    loadWindowAround(ms + 1000, { seekMs: ms, play: true });
    return;
  }
  const next = nextSpanStart(ms);
  if (next != null) {
    toast(`Gap ${humanDur(next - ms)} — continuing`);
    // Auto-advance is not a manual seek: keep the follow when it's on.
    seekTo(next, { play: true, fromLive: state.followLive });
  } else if (state.followLive && state.device.latestMs) {
    // Hold at the live edge; the 6s poll extends the window as footage lands.
    seekTo(state.device.latestMs - LIVE_BEHIND_MS, { play: true, fromLive: true });
  }
}

// The LIVE pill / Shift+L: jump just behind the newest footage and start following.
function goLive() {
  const d = state.device;
  if (!d?.latestMs) return;
  state.followLive = true;
  seekTo(d.latestMs - LIVE_BEHIND_MS, { play: true, fromLive: true });
  updateLivePill(d.latestMs - LIVE_BEHIND_MS);
}

// Solid red LIVE only while actually following at the edge; dimmed GO LIVE otherwise.
function updateLivePill(ms) {
  const btn = $("btnLive");
  if (!btn) return;
  const latest = state.device?.latestMs ?? 0;
  const live =
    state.followLive && latest > 0 && Number.isFinite(ms) && ms > 0 && latest - ms < LIVE_NEAR_MS;
  btn.classList.toggle("is-live", live);
  btn.textContent = live ? "LIVE" : "GO LIVE";
}

// The timeline's live-edge cap only makes sense while footage is actually arriving.
function updateLiveEdge() {
  const latest = state.device?.latestMs ?? 0;
  timeline.setLiveEdge(latest && Date.now() - latest < LIVE_EDGE_FRESH_MS ? latest : null);
}

// Screen-reader mirror of the canvas slider (role=slider on #timeline), 1 Hz.
function updateAria(ms) {
  const el = $("timeline");
  const d = state.device;
  if (!el || !d) return;
  el.setAttribute("aria-valuemin", String(Math.round((d.earliestMs ?? 0) / 1000)));
  el.setAttribute("aria-valuemax", String(Math.round((d.latestMs ?? 0) / 1000)));
  if (Number.isFinite(ms) && ms > 0) {
    el.setAttribute("aria-valuenow", String(Math.round(ms / 1000)));
    el.setAttribute(
      "aria-valuetext",
      `${clock(ms)}, ${timeline.isCovered(ms) ? "recorded" : "gap"}`,
    );
  }
}

function togglePlay() {
  if (player.video.paused) {
    player.play().catch(() => {});
  } else {
    player.pause();
    state.followLive = false; // a manual pause drops live-follow
  }
}

function setRate(r) {
  state.rate = r;
  player.setRate(r);
  $("speed").value = String(r);
  // Audio garbles at high speed: mute >=4x but restore the user's choice at <=2x
  // (slow-mo included — 0.25x/0.5x keep sound).
  player.setMuted(r >= 4 ? true : state.userMuted);
}

// <'/'>' walk the RATES ladder (0.25x..8x) with a toast naming the new rate.
function cycleRate(dir) {
  let i = RATES.indexOf(state.rate);
  if (i < 0) i = RATES.findIndex((r) => r >= state.rate); // off-ladder: snap to the next step up
  if (i < 0) i = RATES.length - 1;
  const next = RATES[Math.min(Math.max(i + dir, 0), RATES.length - 1)];
  setRate(next);
  toast(`Speed ${next}×`);
}

function setMuted(m) {
  state.userMuted = m;
  player.setMuted(m || state.rate >= 4);
  $("btnMute").textContent = m ? "🔇" : "🔊";
}

function relSeek(deltaMs) {
  // The player reports 0/NaN mid-reload; fall back to where the UI says we are so the
  // arrow keys never go dead while a far seek is still loading its window.
  const ms = player.currentWallClockMs();
  const base = Number.isFinite(ms) && ms > 0 ? ms : (state.seekIntent?.ms ?? timeline.playheadMs);
  if (!Number.isFinite(base) || !Number.isFinite(deltaMs)) return;
  seekTo(base + deltaMs, { play: false });
}

function jumpToDay(dayStartMs) {
  state.view = { fromMs: dayStartMs, toMs: dayStartMs + DAY_MS };
  timeline.fit(state.view.fromMs, state.view.toMs);
  $("dateInput").value = localDateInput(dayStartMs);
  $("dayLabel").textContent = dateLabel(dayStartMs);
  seekTo(dayStartMs, { play: true });
}

function wireControls() {
  $("btnPlay").onclick = togglePlay;
  $("btnPrev").onclick = gotoPrevSpan;
  $("btnNext").onclick = gotoNextSpan;
  $("speed").onchange = (e) => setRate(Number(e.target.value));
  $("btnMute").onclick = () => setMuted(!state.userMuted);
  $("volume").oninput = (e) => {
    player.setVolume(Number(e.target.value));
    if (Number(e.target.value) > 0 && state.userMuted) setMuted(false);
  };
  $("btnLive").onclick = goLive;
  $("btnFit").onclick = fitAll;
  $("exportToggle").onclick = () => setExportMode(!state.exportMode);
  // Shortcut cheat-sheet (?): a static dialog, so wireModal covers Esc/backdrop/focus.
  helpModal = wireModal($("helpModal"));
  $("helpClose").onclick = () => helpModal.close();
  $("zoomIn").onclick = () => timeline.zoom(1 / 1.6);
  $("zoomOut").onclick = () => timeline.zoom(1.6);
  $("btnFull").onclick = () => {
    const el = $("viewport");
    if (document.fullscreenElement) document.exitFullscreen();
    else el.requestFullscreen?.();
  };
  $("dateInput").onchange = (e) => {
    const [y, m, d] = e.target.value.split("-").map(Number);
    if (y) jumpToDay(new Date(y, m - 1, d).getTime());
  };
  $("dayPrev").onclick = () => stepDay(-1);
  $("dayNext").onclick = () => stepDay(1);
  $("modeVideo").onclick = () => setMode(false);
  $("modeDet").onclick = () => setMode(true);
  const aiToggle = $("aiStatusToggle");
  if (aiToggle) {
    aiToggle.checked = state.aiEnabled;
    aiToggle.onchange = (e) => setProcessingEnabled(e.target.checked);
  }
  const aiInfoBtn = $("aiInfoBtn");
  const aiInfoPop = $("aiInfoPop");
  if (aiInfoBtn && aiInfoPop) {
    const closePop = () => {
      aiInfoPop.hidden = true;
      aiInfoBtn.setAttribute("aria-expanded", "false");
    };
    aiInfoBtn.onclick = (e) => {
      e.stopPropagation();
      const willOpen = aiInfoPop.hidden;
      aiInfoPop.hidden = !willOpen;
      aiInfoBtn.setAttribute("aria-expanded", String(willOpen));
    };
    // Click-outside / Escape dismiss.
    document.addEventListener("click", (e) => {
      if (!aiInfoPop.hidden && !aiInfoPop.contains(e.target) && e.target !== aiInfoBtn) closePop();
    });
    document.addEventListener("keydown", (e) => {
      if (e.key === "Escape") closePop();
    });
  }
}

function stepDay(dir) {
  const cur = $("dateInput").value;
  if (!cur) return;
  const [y, m, d] = cur.split("-").map(Number);
  jumpToDay(new Date(y, m - 1, d + dir).getTime());
}

// Zoom the bar out to the device's whole footage range (btnFit + the `0` key).
function fitAll() {
  const d = state.device;
  if (d) timeline.fit(d.earliestMs - 2000, d.latestMs + 2000);
}

function wireKeys() {
  window.addEventListener("keydown", (e) => {
    if (e.metaKey || e.ctrlKey || e.altKey) return; // never eat browser/system chords
    // An open modal owns the keyboard — typing/navigating in it must not scrub the
    // player. Exception: `?` still toggles the cheat-sheet closed from inside itself.
    if (document.querySelector(".modal:not([hidden])")) {
      if (e.key === "?" && helpModal?.isOpen()) {
        e.preventDefault();
        helpModal.close();
      }
      return;
    }
    const t = e.target;
    if (t && (t.tagName === "INPUT" || t.tagName === "SELECT" || t.tagName === "TEXTAREA")) return;
    switch (e.key) {
      case " ":
      case "k":
        e.preventDefault();
        togglePlay();
        break;
      case "ArrowLeft":
        relSeek(e.shiftKey ? -60000 : -5000);
        break;
      case "ArrowRight":
        relSeek(e.shiftKey ? 60000 : 5000);
        break;
      case "j":
        relSeek(-10000);
        break;
      case "l":
        relSeek(10000);
        break;
      case "L": // Shift+L
        goLive();
        break;
      case ",": // step one frame back (pauses)
        player.stepFrame(-1);
        state.followLive = false;
        break;
      case ".": // step one frame forward (pauses)
        player.stepFrame(1);
        state.followLive = false;
        break;
      case "<": // Shift+, — slower
        cycleRate(-1);
        break;
      case ">": // Shift+. — faster
        cycleRate(1);
        break;
      case "[":
        gotoPrevSpan();
        break;
      case "]":
        gotoNextSpan();
        break;
      case "m":
        setMuted(!state.userMuted);
        break;
      case "f":
        $("btnFull").click();
        break;
      case "d":
        setMode(!state.detMode);
        break;
      case "a":
        setProcessingEnabled(!state.aiEnabled);
        break;
      case "e":
        setExportMode(!state.exportMode);
        break;
      case "i": // export mode: clip in-point at the playhead
        if (state.exportMode) setSelectionPoint("in");
        break;
      case "o": // export mode: clip out-point at the playhead
        if (state.exportMode) setSelectionPoint("out");
        break;
      case "Escape":
        // Mid-drag, Esc belongs to the timeline (it cancels the gesture — its handler
        // runs after this one, so the drag is still visible here); otherwise exit.
        if (state.exportMode && !timeline.isDragging()) setExportMode(false);
        break;
      case "Home":
        if (state.device) seekTo(state.device.earliestMs, { play: true });
        break;
      case "End":
        if (state.device) seekTo(state.device.latestMs - 4000, { play: true });
        break;
      case "+":
      case "=":
        timeline.zoom(1 / 1.6);
        break;
      case "-":
        timeline.zoom(1.6);
        break;
      case "0":
        fitAll();
        break;
      case "1":
        setRate(1);
        break;
      case "2":
        setRate(2);
        break;
      case "3":
        setRate(4);
        break;
      case "4":
        setRate(8);
        break;
      case "?":
        e.preventDefault();
        helpModal?.open();
        break;
    }
  });
}

// ---- playhead ticker --------------------------------------------------------

function startTicker() {
  const loop = () => {
    if (player && state.device) {
      const reported = player.currentWallClockMs();
      // While a seek is in flight, prefer its optimistic target over the player's report:
      // hold it until the player lands within 2s of the intent (satisfied) or the intent
      // goes stale (4s — e.g. the seek failed), then fall back to the reported position.
      let ms = reported;
      const intent = state.seekIntent;
      if (intent) {
        const stale = performance.now() - intent.at > 4000;
        const satisfied = isFinite(reported) && Math.abs(reported - intent.ms) < 2000;
        if (stale || satisfied) state.seekIntent = null;
        else ms = intent.ms;
      }
      if (ms && isFinite(ms) && ms > 0) {
        timeline.setPlayhead(ms);
        $("readout").textContent = clockMs(ms);
        if (state.detMode) detections.onTick(ms);
        updateAiBadge(ms);
      }
      // 1 Hz side-channel: ARIA slider values, LIVE pill state, live-edge cap freshness.
      const nowTick = performance.now();
      if (nowTick - lastA11yTick >= 1000) {
        lastA11yTick = nowTick;
        updateAria(ms);
        updateLivePill(ms);
        updateLiveEdge();
      }
      // Advance the 'processing' shimmer (cheap no-op unless something is processing on screen).
      timeline.tickAnim(performance.now());
      const paused = player.video.paused;
      if (paused === state.playing) {
        state.playing = !paused;
        updatePlayBtn();
      }
    }
    requestAnimationFrame(loop);
  };
  requestAnimationFrame(loop);
}

function updatePlayBtn() {
  const b = $("btnPlay");
  if (b) {
    b.textContent = state.playing ? "⏸" : "▶";
    $("recDot").classList.toggle("live", state.playing);
  }
}

// ---- status / toast ---------------------------------------------------------

function showStatus(msg) {
  const el = $("status");
  el.textContent = msg;
  el.style.display = "flex";
}
function hideStatus() {
  $("status").style.display = "none";
}
function showError(msg) {
  const el = $("status");
  // textContent (not innerHTML) so an error string carrying server/user-derived text — a device id
  // or an echoed query in a 4xx message — can't inject HTML/script into the admin console.
  const div = document.createElement("div");
  div.textContent = msg;
  el.replaceChildren(div);
  el.style.display = "flex";
}
function toast(msg) {
  const el = $("toast");
  el.textContent = msg;
  el.classList.add("show");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => el.classList.remove("show"), 1400);
}

init().catch((e) => showError("Failed to start viewer: " + (e?.message || e)));
