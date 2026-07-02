// App orchestration: device list, player, timeline, controls, keyboard. Holds the
// single source of truth and wires the pieces. View window (the bar) is decoupled
// from the loaded HLS window; we only reload the playlist when a seek lands outside
// the loaded window. For typical day-sized data the whole device loads as one window.

import { getDevices, getTimeline, getProcessing, masterUrl } from "./api.js";
import { Player } from "./player.js";
import { Timeline } from "./timeline.js";
import { Detections } from "./detections.js";
import { clockMs, dateLabel, localDateInput, DAY_MS, tzAbbr, humanDur } from "./time.js";
import { on, setPlaybackProvider } from "./store.js";
import { createPoller } from "./poll.js";

const $ = (id) => document.getElementById(id);
const WINDOW_MS = 6 * 3600 * 1000; // matches backend VIEWER_MAX_WINDOW_NANOS default
const REFRESH_MS = 6000; // how often we poll for newly-ingested footage (devices + timeline)
const FOLLOW_EPS_MS = 5000; // the view counts as "parked at the live edge" within this of latest

const state = {
  devices: [],
  device: null,
  view: { fromMs: 0, toMs: 1 },
  loaded: null,
  timeline: null,
  playing: false,
  rate: 1,
  userMuted: true,
  detMode: false,
  aiEnabled: true, // AI processing-status ribbons on the scrub bar
  seekIntent: null, // {ms, at}: optimistic playhead target while a seek is still landing
};

let player, timeline, detections, toastTimer;
let refreshPoller = null,
  lastDeviceSig = "";
let procSeq = 0, // stale-response guard for the processing-status fetch
  procDebounce = null,
  lastBadgeKey = "";

async function init() {
  const video = $("video");
  video.muted = true; // allow autoplay; user unmutes
  player = new Player(video, { onError: showError, onNotice: toast });
  timeline = new Timeline($("timeline"), {
    onSeek: (ms) => seekTo(ms, { play: true }),
    onWindowChange: (from, to) => {
      state.view = { fromMs: from, toMs: to };
      scheduleProcessingRefetch(); // pan/zoom changed the visible window
    },
  });
  detections = new Detections($("detOverlay"), video);
  detections.onError = (e) => toast("detections: " + (e?.message || e));
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
      state,
      get player() {
        return player;
      },
      get timeline() {
        return timeline;
      },
      currentMs: () => (player ? player.currentWallClockMs() : 0),
    };
  }
  lastDeviceSig = deviceSig(state.devices);
  populateDeviceSelect();
  const usable = state.devices.filter((d) => d.segmentCount > 0 && d.latestMs);
  // Deep-link from the Events feed: /?device=<id>&t=<ms> opens that camera at that instant.
  const params = new URLSearchParams(location.search);
  const linkDevice = params.get("device");
  const linkMs = Number(params.get("t"));
  if (linkDevice && isFinite(linkMs) && linkMs > 0 && state.devices.some((d) => d.id === linkDevice)) {
    selectDevice(linkDevice, { seekMs: linkMs });
  } else if (usable.length) {
    const def = usable.slice().sort((a, b) => b.segmentCount - a.segmentCount)[0];
    selectDevice(def.id);
  } else {
    showStatus("No cameras have reported footage yet.");
  }
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
  state.device = d;
  $("deviceSelect").value = id;
  setDeviceMeta(d);

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
}

function startAutoRefresh() {
  if (refreshPoller) return; // idempotent (init reaches here from two paths)
  refreshPoller = createPoller(
    async () => {
      await refreshDevices();
      // Always refresh AI status (not gated by the new-footage signature): processing
      // advances as the worker catches up on already-ingested segments.
      await refetchProcessing();
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

function seekTo(ms, { play = false } = {}) {
  if (!state.device) return;
  const { ms: snapped, movedMs } = timeline.snapInfo(ms);
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

// ---- controls ---------------------------------------------------------------

function sortedCoverage() {
  return (state.timeline?.coverage ?? []).slice().sort((a, b) => a.startMs - b.startMs);
}
function gotoNextSpan() {
  const ms = player.currentWallClockMs();
  const c = sortedCoverage().find((x) => x.startMs > ms + 500);
  if (c) seekTo(c.startMs, { play: true });
}
function gotoPrevSpan() {
  const ms = player.currentWallClockMs();
  let target = null;
  for (const x of sortedCoverage()) if (x.startMs < ms - 1500) target = x.startMs;
  if (target != null) seekTo(target, { play: true });
}

function togglePlay() {
  if (player.video.paused) player.play().catch(() => {});
  else player.pause();
}

function setRate(r) {
  state.rate = r;
  player.setRate(r);
  $("speed").value = String(r);
  // Audio garbles at high speed: mute >=4x but restore the user's choice at 1x/2x.
  player.setMuted(r >= 4 ? true : state.userMuted);
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
  $("btnLatest").onclick = () => {
    const d = state.device;
    if (d?.latestMs) seekTo(d.latestMs - 4000, { play: true });
  };
  $("btnFit").onclick = () => {
    const d = state.device;
    if (d) timeline.fit(d.earliestMs - 2000, d.latestMs + 2000);
  };
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

function wireKeys() {
  window.addEventListener("keydown", (e) => {
    if (e.metaKey || e.ctrlKey || e.altKey) return; // never eat browser/system chords
    // An open modal owns the keyboard — typing/navigating in it must not scrub the player.
    if (document.querySelector(".modal:not([hidden])")) return;
    const t = e.target;
    if (t && (t.tagName === "INPUT" || t.tagName === "SELECT" || t.tagName === "TEXTAREA")) return;
    switch (e.key) {
      case " ":
      case "k":
        e.preventDefault();
        togglePlay();
        break;
      case "ArrowLeft":
        relSeek(-5000);
        break;
      case "ArrowRight":
        relSeek(5000);
        break;
      case "j":
        relSeek(-10000);
        break;
      case "l":
        relSeek(10000);
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
