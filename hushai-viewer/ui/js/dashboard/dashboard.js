// System dashboard: polls /api/dashboard and renders KPI tiles + cameras + background-process
// status + work queues. Mirrors the app's polling pattern (6s, busy-guard, paused while hidden).

import { getDashboard } from "../api.js";

const $ = (id) => document.getElementById(id);
const REFRESH_MS = 6000; // matches the player page's refresh cadence

let timer = null;
let busy = false;
let everLoaded = false;

// ---- small DOM + format helpers -------------------------------------------

function el(tag, props = {}, children = []) {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(props)) {
    if (k === "class") node.className = v;
    else if (k === "html") node.innerHTML = v;
    else if (k === "text") node.textContent = v;
    else if (v != null) node.setAttribute(k, v);
  }
  for (const c of [].concat(children)) {
    if (c == null) continue;
    node.appendChild(typeof c === "string" ? document.createTextNode(c) : c);
  }
  return node;
}

// "connected"/"up" -> green; "idle"/"degraded" -> amber; "offline"/"down" -> red; else grey.
function statusClass(state) {
  switch (state) {
    case "connected":
    case "up":
      return "up";
    case "idle":
    case "degraded":
      return "degraded";
    case "offline":
    case "down":
      return "down";
    default:
      return "unknown";
  }
}

function pill(state, label) {
  return el("span", { class: `pill ${statusClass(state)}`, text: label || state });
}

function agoSecs(secs) {
  if (secs == null) return "—";
  const s = Math.max(0, Math.round(secs));
  if (s < 60) return `${s}s ago`;
  if (s < 3600) return `${Math.round(s / 60)}m ago`;
  if (s < 86400) return `${Math.round(s / 3600)}h ago`;
  return `${Math.round(s / 86400)}d ago`;
}

function fmtClock(rfc3339) {
  if (!rfc3339) return "—";
  try {
    return new Date(rfc3339).toLocaleTimeString();
  } catch {
    return rfc3339;
  }
}

// ---- KPI tiles ------------------------------------------------------------

function kpi(label, valueHtml, sub, klass) {
  return el("div", { class: `kpi ${klass || ""}` }, [
    el("div", { class: "kpi-label", text: label }),
    el("div", { class: "kpi-value", html: valueHtml }),
    el("div", { class: "kpi-sub", text: sub || "" }),
  ]);
}

function renderKpis(data) {
  const root = $("kpis");
  root.replaceChildren();

  const cs = data.camera_summary || { connected: 0, idle: 0, offline: 0, total: 0 };
  const online = cs.connected + cs.idle;
  const camKlass = online === 0 ? (cs.total ? "down" : "unknown") : online === cs.total ? "up" : "degraded";
  root.appendChild(
    kpi("Cameras online", `${online}<span class="unit">/ ${cs.total}</span>`,
      `${cs.connected} live · ${cs.idle} idle · ${cs.offline} offline`, camKlass),
  );

  const svcs = data.services || [];
  const real = svcs.filter((s) => !s.optional);
  const up = real.filter((s) => s.state === "up").length;
  const bad = real.filter((s) => s.state === "down").length;
  const svcKlass = bad > 0 ? "down" : up === real.length ? "up" : "degraded";
  root.appendChild(
    kpi("Services healthy", `${up}<span class="unit">/ ${real.length}</span>`,
      bad > 0 ? `${bad} down` : "all systems go", svcKlass),
  );

  const q = data.queues || {};
  const backlog = (q.transcription?.pending || 0) + (q.transcription?.processing || 0) +
    (q.vision?.pending || 0) + (q.vision?.processing || 0);
  const errs = (q.transcription?.error || 0) + (q.vision?.error || 0);
  root.appendChild(
    kpi("Queue backlog", `${backlog.toLocaleString()}`,
      errs > 0 ? `${errs} error${errs === 1 ? "" : "s"}` : "items waiting / in-flight",
      errs > 0 ? "degraded" : backlog > 0 ? "degraded" : "up"),
  );

  const disk = svcs.find((s) => s.name === "disk");
  if (disk) {
    root.appendChild(kpi("Disk free", disk.detail.replace(" free", ""),
      "on the capture volume", statusClass(disk.state)));
  }
}

// ---- cameras --------------------------------------------------------------

function renderCameras(cameras, summary) {
  const grid = $("cameras");
  grid.replaceChildren();

  const s = summary || { connected: 0, idle: 0, offline: 0, total: 0 };
  $("cameraCount").textContent = `${s.connected + s.idle} of ${s.total} online`;

  if (!cameras.length) {
    grid.appendChild(el("div", { class: "empty muted", text: "No cameras have connected yet." }));
    return;
  }

  for (const c of cameras) {
    const media = [c.has_video ? "V" : null, c.has_audio ? "A" : null, c.has_muxed ? "M" : null].filter(Boolean);
    const card = el("div", { class: "card cam" }, [
      el("div", { class: "card-top" }, [
        el("span", { class: "card-id" }, [
          el("span", { class: `dot ${statusClass(c.state)}` }),
          el("span", { class: "card-title mono", title: c.device_id, text: c.device_id }),
        ]),
        pill(c.state),
      ]),
      el("div", { class: "card-headline" }, [`Last upload ${agoSecs(c.last_seen_age_secs)}`]),
      el("div", { class: "card-stats small muted" }, [
        el("span", {}, [`${(c.segment_count || 0).toLocaleString()} segments`]),
        el("span", {}, [`${c.session_count || 0} sessions`]),
        media.length ? el("span", { class: "badges" }, media.map((m) => el("i", { class: "badge", text: m }))) : null,
      ]),
    ]);
    grid.appendChild(card);
  }
}

// ---- services -------------------------------------------------------------

function renderServices(services) {
  const root = $("services");
  root.replaceChildren();
  const list = services || [];
  const up = list.filter((s) => !s.optional && s.state === "up").length;
  $("serviceCount").textContent = `${up} of ${list.filter((s) => !s.optional).length} healthy`;

  for (const svc of list) {
    const mutedDown = svc.optional && svc.state === "down";
    root.appendChild(
      el("div", { class: `srow${mutedDown ? " optional-down" : ""}` }, [
        el("span", { class: `dot ${statusClass(svc.state)}` }),
        el("span", { class: "srow-name" }, [
          svc.name,
          svc.optional ? el("span", { class: "muted small", text: " · optional" }) : null,
        ]),
        el("span", { class: "srow-kind muted", text: svc.kind }),
        el("span", { class: "srow-detail muted small", text: svc.detail || "", title: svc.detail || "" }),
        el("span", { class: "srow-latency muted small mono", text: svc.latency_ms != null ? `${svc.latency_ms} ms` : "" }),
        pill(svc.state),
      ]),
    );
  }
}

// ---- queues ---------------------------------------------------------------

function qstat(label, value, klass) {
  return el("div", { class: `qstat${klass ? " " + klass : ""}` }, [
    el("div", { class: "qnum", text: (value ?? 0).toLocaleString() }),
    el("div", { class: "qlabel muted small", text: label }),
  ]);
}

function queueCard(title, q) {
  q = q || {};
  return el("div", { class: "card queue" }, [
    el("div", { class: "card-top" }, [
      el("span", { class: "card-title", text: title }),
      (q.error || 0) > 0 ? pill("down", `${q.error} error`) : (q.pending || 0) > 0 ? pill("degraded", "busy") : pill("up", "idle"),
    ]),
    el("div", { class: "qstats" }, [
      qstat("pending", q.pending, (q.pending || 0) > 100 ? "warn" : null),
      qstat("processing", q.processing),
      qstat("errors", q.error, (q.error || 0) > 0 ? "down" : null),
      qstat("done 24h", q.done_recent),
    ]),
    el("div", { class: "muted small" }, [
      `oldest pending ${q.pending ? agoSecs(q.oldest_pending_age_secs) : "—"} · last activity ${agoSecs(q.max_updated_age_secs)}`,
    ]),
  ]);
}

function renderQueues(queues) {
  const root = $("queues");
  root.replaceChildren();
  root.appendChild(queueCard("Transcription (audio)", queues?.transcription));
  root.appendChild(queueCard("Vision (faces & objects)", queues?.vision));
}

// ---- poll loop ------------------------------------------------------------

async function refresh() {
  if (busy || document.hidden) return;
  busy = true;
  try {
    const data = await getDashboard();
    everLoaded = true;
    $("dashBanner").hidden = true;
    $("liveDot").classList.add("live");
    $("generatedAt").textContent = `updated ${fmtClock(data.generated_at)}`;
    renderKpis(data);
    renderCameras(data.cameras || [], data.camera_summary);
    renderServices(data.services || []);
    renderQueues(data.queues || {});
  } catch (e) {
    $("liveDot").classList.remove("live");
    $("generatedAt").textContent = "disconnected";
    const banner = $("dashBanner");
    banner.hidden = false;
    banner.className = "dash-banner error";
    banner.innerHTML = everLoaded
      ? `Lost connection to the viewer — retrying every ${REFRESH_MS / 1000}s. <span class="muted">(${String(e.message || e)})</span>`
      : `Couldn't load <b>/api/dashboard</b>. Make sure the viewer is running the latest build (restart <span class="mono">hushai-viewer</span>), then this page recovers automatically. <span class="muted">(${String(e.message || e)})</span>`;
  } finally {
    busy = false;
  }
}

function start() {
  refresh();
  if (timer) clearInterval(timer);
  timer = setInterval(refresh, REFRESH_MS);
}

document.addEventListener("visibilitychange", () => {
  if (!document.hidden) refresh(); // refresh immediately on return; interval keeps running
});

start();
