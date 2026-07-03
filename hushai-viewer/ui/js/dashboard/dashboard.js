// System dashboard: polls /api/dashboard and renders KPI tiles + cameras + background-process
// status + work queues. Mirrors the app's polling pattern (6s, busy-guard, paused while hidden).

import { getDashboard, getAudit } from "../api.js";
import { chartPalette, cssVar } from "../theme.js";
import { el, renderBanner, emptyState, errorState } from "../dom.js";
import { createPoller } from "../poll.js";
import { initTopbar, setLive, setUpdated } from "../nav.js";
import { clock, startOfLocalDay } from "../time.js";
import "../search/omni.js"; // "/" or Cmd+K global search palette

const $ = (id) => document.getElementById(id);
const REFRESH_MS = 6000; // matches the player page's refresh cadence

let everLoaded = false;

// ---- small format helpers --------------------------------------------------

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

// First 8 chars of a segment uuid — enough to correlate with logs / the processing view.
function shortId(id) {
  return id ? `${id.slice(0, 8)}…` : "—";
}

function truncate(s, n) {
  s = String(s || "");
  return s.length > n ? `${s.slice(0, n - 1)}…` : s;
}

// The single most-recent error across both queues (smallest age), or null. Drives the KPI
// subtitle so the headline says *what* broke, not just how many.
function freshestError(queues) {
  const all = [
    ...((queues?.transcription?.recent_errors) || []),
    ...((queues?.vision?.recent_errors) || []),
  ];
  if (!all.length) return null;
  return all.reduce((best, e) => (e.age_secs < best.age_secs ? e : best));
}

// ---- KPI tiles ------------------------------------------------------------

// `value` is an el() child (string or node) or an array of them — never an HTML string.
function kpi(label, value, sub, klass) {
  return el("div", { class: `kpi ${klass || ""}` }, [
    el("div", { class: "kpi-label", text: label }),
    el("div", { class: "kpi-value" }, value),
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
    kpi("Cameras online", [`${online}`, el("span", { class: "unit", text: `/ ${cs.total}` })],
      `${cs.connected} live · ${cs.idle} idle · ${cs.offline} offline`, camKlass),
  );

  const svcs = data.services || [];
  const real = svcs.filter((s) => !s.optional);
  const up = real.filter((s) => s.state === "up").length;
  const bad = real.filter((s) => s.state === "down").length;
  const svcKlass = bad > 0 ? "down" : up === real.length ? "up" : "degraded";
  root.appendChild(
    kpi("Services healthy", [`${up}`, el("span", { class: "unit", text: `/ ${real.length}` })],
      bad > 0 ? `${bad} down` : "all systems go", svcKlass),
  );

  const q = data.queues || {};
  const backlog = (q.transcription?.pending || 0) + (q.transcription?.processing || 0) +
    (q.vision?.pending || 0) + (q.vision?.processing || 0);
  const errs = (q.transcription?.error || 0) + (q.vision?.error || 0);
  // Say what the error was, not just how many: lead with the freshest message, then id + age.
  let errSub = errs > 0 ? `${errs} error${errs === 1 ? "" : "s"}` : "items waiting / in-flight";
  if (errs > 0) {
    const fe = freshestError(q);
    if (fe) errSub = `${truncate(fe.last_error, 48)} · ${shortId(fe.segment_id)} · ${agoSecs(fe.age_secs)}`;
  }
  root.appendChild(
    kpi("Queue backlog", `${backlog.toLocaleString()}`, errSub,
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
    // Prefer the operator-assigned friendly name; keep the raw id discoverable via the title.
    const friendly = c.display_name || c.device_id;
    const card = el("div", { class: "card cam" }, [
      el("div", { class: "card-top" }, [
        el("span", { class: "card-id" }, [
          el("span", { class: `dot ${statusClass(c.state)}` }),
          el("span", { class: c.display_name ? "card-title" : "card-title mono", title: c.device_id, text: friendly }),
        ]),
        pill(c.state),
      ]),
      el("div", { class: "card-headline" }, [`Last upload ${agoSecs(c.last_seen_age_secs)}`]),
      el("div", { class: "card-stats small muted" }, [
        el("span", {}, [`${(c.segment_count || 0).toLocaleString()} segments`]),
        el("span", {}, [`${c.session_count || 0} sessions`]),
        media.length ? el("span", { class: "badges" }, media.map((m) => el("i", { class: "badge", text: m }))) : null,
      ]),
      // Quick jump to the file-management page focused on this device (rename / retention / delete).
      el("div", { class: "card-foot" }, [
        el("a", {
          class: "navlink small",
          href: `/manage.html?device=${encodeURIComponent(c.device_id)}`,
          text: "Manage files ›",
        }),
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

// An expandable list of the recent error rows (segment id · message · age · ×attempts). Native
// <details> so it needs no extra JS; collapsed by default to keep the card compact. Returns null
// (skipped by el's child filter) when the queue has no errors.
function errorList(recent) {
  if (!recent || !recent.length) return null;
  const rows = recent.map((e) =>
    el("li", { class: "qerr" }, [
      el("span", { class: "qerr-id mono small", title: e.segment_id, text: shortId(e.segment_id) }),
      el("span", { class: "qerr-msg", title: e.last_error, text: e.last_error || "(no message)" }),
      el("span", { class: "qerr-meta muted small", text: `${agoSecs(e.age_secs)} · ×${e.attempts}` }),
    ]),
  );
  return el("details", { class: "qerrors" }, [
    el("summary", { class: "small" }, [`recent errors (${recent.length})`]),
    el("ul", { class: "qerr-list" }, rows),
  ]);
}

// Hint-audit trust check: of the sampled hint-skips graded by the worker in the last 24h,
// how often did the worker's own gate DISAGREE (i.e. find real content the device hints called
// static/silent)? Warn loudly past 10% with a meaningful sample — miscalibrated hints silently
// defer real content until an operator reprocesses.
function auditWarning(q) {
  const graded = (q.audit_agree_recent || 0) + (q.audit_disagree_recent || 0);
  if (graded < 20) return null;
  const ratio = (q.audit_disagree_recent || 0) / graded;
  if (ratio <= 0.1) return null;
  return el("div", { class: "qerr-msg down small" }, [
    `⚠ device hints disagree with worker gates on ${Math.round(ratio * 100)}% of audited skips — ` +
      `check hint calibration or set INGEST_HINT_GATE_ENABLED=false`,
  ]);
}

// One-line explainer when most of the last 24h was skipped: an idle/static camera makes the
// queue look "quiet", which is healthy — say so instead of letting it read as a stall.
function skipExplainer(q) {
  const skipped = q.skipped_recent || 0;
  const total = skipped + (q.done_recent || 0);
  if (!total || skipped / total <= 0.5) return null;
  const viaHints = q.skipped_by_hint_recent || 0;
  return el("div", { class: "muted small" }, [
    `content mostly static/silent: AI processing skipped for ${skipped.toLocaleString()} of ` +
      `${total.toLocaleString()} segments in 24h (${viaHints.toLocaleString()} pre-skipped by device hints); ` +
      `recordings are stored & playable, skipped segments can be reprocessed`,
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
      qstat("skipped 24h", q.skipped_recent),
    ]),
    el("div", { class: "muted small" }, [
      `oldest pending ${q.pending ? agoSecs(q.oldest_pending_age_secs) : "—"} · last activity ${agoSecs(q.max_updated_age_secs)}`,
    ]),
    skipExplainer(q),
    auditWarning(q),
    errorList(q.recent_errors),
  ]);
}

function renderQueues(queues) {
  const root = $("queues");
  root.replaceChildren();
  root.appendChild(queueCard("Transcription (audio)", queues?.transcription));
  root.appendChild(queueCard("Vision (faces & objects)", queues?.vision));
}

// ---- load test (hushai-loadtest) ------------------------------------------

// Shown only while a benchmark is publishing live.json (the `loadtest` field is otherwise absent).
function renderLoadtest(lt) {
  const section = $("loadtestSection");
  if (!lt) {
    if (section) section.hidden = true;
    return;
  }
  section.hidden = false;
  const sat = lt.saturation_n != null ? `${lt.saturation_n} cameras` : "—";
  $("loadtestStatus").textContent = lt.running
    ? `running · ${lt.current_cameras}/${lt.max_cameras} cams`
    : "complete";
  $("loadtestSummary").textContent =
    `Profile: ${lt.profile} · saturation: ${sat}` + (lt.running ? " · ramping…" : "");
  drawLoadtestChart($("loadtestChart"), lt);
}

// Dependency-free multi-line chart: camera-count (x) vs per-series-normalized signals (y),
// with a dashed vertical marker at the saturation knee. Each series is scaled to its own max
// (shown in the legend) so differing units share the plot.
function drawLoadtestChart(canvas, lt) {
  if (!canvas || !canvas.getContext) return;
  const ctx = canvas.getContext("2d");
  const W = canvas.width, H = canvas.height;
  ctx.clearRect(0, 0, W, H);
  const pts = (lt.points || []).filter((p) => p && p.n != null);
  if (!pts.length) return;

  const pad = { l: 44, r: 12, t: 26, b: 28 };
  const x0 = pad.l, x1 = W - pad.r, y0 = H - pad.b, y1 = pad.t;
  const nMin = Math.min(...pts.map((p) => p.n));
  const nMax = Math.max(lt.max_cameras || 0, ...pts.map((p) => p.n));
  const xOf = (n) => x0 + (nMax === nMin ? 0 : (n - nMin) / (nMax - nMin)) * (x1 - x0);
  const yOf = (v) => y0 - v * (y0 - y1); // v normalized 0..1

  const [cLag, cQueue, cTput, cCpu, cGpu] = chartPalette();
  const cAxis = cssVar("--chart-axis", "#3a3a3a");
  const cLabel = cssVar("--chart-label", "#888");
  ctx.strokeStyle = cAxis;
  ctx.lineWidth = 1;
  ctx.beginPath();
  ctx.moveTo(x0, y1); ctx.lineTo(x0, y0); ctx.lineTo(x1, y0);
  ctx.stroke();
  ctx.fillStyle = cLabel;
  ctx.font = "11px system-ui, sans-serif";
  ctx.fillText(String(nMin), x0 - 3, y0 + 16);
  ctx.fillText(String(nMax), x1 - 14, y0 + 16);
  ctx.fillText("cameras", (x0 + x1) / 2 - 22, y0 + 16);

  if (lt.saturation_n != null) {
    const xs = xOf(lt.saturation_n);
    ctx.strokeStyle = cCpu;
    ctx.setLineDash([5, 4]);
    ctx.beginPath(); ctx.moveTo(xs, y1); ctx.lineTo(xs, y0); ctx.stroke();
    ctx.setLineDash([]);
    ctx.fillStyle = cCpu;
    ctx.fillText(`saturation N=${lt.saturation_n}`, Math.min(xs + 4, x1 - 92), y0 - 4);
  }

  const series = [
    { k: "audio_oldest_pending_age_s", label: "lag s", color: cLag },
    { k: "audio_queue_depth", label: "queue", color: cQueue },
    { k: "audio_throughput_seg_per_s", label: "tput", color: cTput },
    { k: "worker_cpu_pct", label: "CPU%", color: cCpu },
    { k: "gpu_active_pct", label: "GPU%", color: cGpu },
  ];
  let legendX = x0 + 6;
  for (const s of series) {
    const vals = pts.map((p) => (p[s.k] == null ? null : Number(p[s.k])));
    const max = Math.max(0, ...vals.filter((v) => v != null));
    if (max <= 0) continue;
    ctx.strokeStyle = s.color;
    ctx.lineWidth = 2;
    ctx.beginPath();
    let started = false;
    pts.forEach((p, i) => {
      const v = vals[i];
      if (v == null) return;
      const X = xOf(p.n), Y = yOf(v / max);
      if (!started) { ctx.moveTo(X, Y); started = true; } else ctx.lineTo(X, Y);
    });
    ctx.stroke();
    pts.forEach((p, i) => {
      const v = vals[i];
      if (v == null) return;
      const X = xOf(p.n), Y = yOf(v / max);
      ctx.fillStyle = p.keeping_up ? s.color : cLag;
      ctx.beginPath(); ctx.arc(X, Y, 2.5, 0, Math.PI * 2); ctx.fill();
    });
    const tag = `${s.label} (≤${max.toFixed(max < 10 ? 1 : 0)})`;
    ctx.fillStyle = s.color;
    ctx.fillRect(legendX, y1 - 12, 9, 9);
    ctx.fillStyle = cLabel;
    ctx.fillText(tag, legendX + 12, y1 - 4);
    legendX += 12 + ctx.measureText(tag).width + 16;
  }
}

// ---- audit trail (roadmap C6) ----------------------------------------------
// Read-only view over the append-only /v1/audit log (newest first). Loaded on init and on
// Apply/⟳ only — NOT on the 6s poll: an audit list that rewrites itself under the reader
// (and resets the "Load more" depth) would be unusable. Seq-guarded like events.js so a slow
// response started under old filters can't clobber a newer render.

const AUDIT_LIMITS = [100, 250, 500, 1000]; // backend clamps at 1000
let auditLimit = AUDIT_LIMITS[0];
let auditSeq = 0;
// Filter vocab accumulated across loads (only grows), so narrowing a filter never strips the
// datalist/select of the values you'd need to widen it again.
const auditActors = new Set();
const auditActions = new Set();
const auditTargetTypes = new Set();

// <input type="date"> value ("YYYY-MM-DD") → ms at LOCAL midnight. new Date("YYYY-MM-DD")
// would parse as UTC midnight and skew the filter by the tz offset.
function localDateToMs(v) {
  if (!v) return undefined;
  const [y, m, d] = v.split("-").map(Number);
  if (!isFinite(y) || !isFinite(m) || !isFinite(d)) return undefined;
  return new Date(y, m - 1, d).getTime();
}

// HH:MM:SS for today's entries; prefixed with the date (and year when it differs) otherwise.
function auditWhen(ms) {
  const t = clock(ms);
  if (startOfLocalDay(ms) === startOfLocalDay(Date.now())) return t;
  const d = new Date(ms);
  const opts = d.getFullYear() === new Date().getFullYear()
    ? { month: "short", day: "numeric" }
    : { year: "numeric", month: "short", day: "numeric" };
  return `${d.toLocaleDateString(undefined, opts)} ${t}`;
}

// "type/shortId" (target ids are free text — device names, uuids — so only truncate long ones).
function auditTargetLabel(r) {
  if (!r.targetType && !r.targetId) return "—";
  const id = r.targetId ? (r.targetId.length > 10 ? `${r.targetId.slice(0, 8)}…` : r.targetId) : "";
  return id ? `${r.targetType || "?"}/${id}` : r.targetType;
}

// 2xx → up, 4xx/5xx → down, anything else (3xx, or a non-HTTP event with no status) → unknown.
function auditStatusPill(status) {
  const cls = status >= 200 && status < 300 ? "up" : status >= 400 ? "down" : "unknown";
  return el("span", { class: `pill ${cls}`, text: status != null ? String(status) : "—" });
}

function auditRow(r) {
  const targetFull = r.targetId ? `${r.targetType || "?"}/${r.targetId}` : r.targetType || "";
  const reqFull = [r.method, r.path].filter(Boolean).join(" ");
  // Non-empty detail (JSON) surfaces via the action's tooltip; the row stays one line.
  const detail = r.detail && typeof r.detail === "object" && Object.keys(r.detail).length
    ? JSON.stringify(r.detail)
    : null;
  return el("div", { class: "srow audit-row" }, [
    el("span", { class: "audit-when mono", text: auditWhen(r.tsMs), title: new Date(r.tsMs).toLocaleString() }),
    el("span", { class: "audit-actor", text: r.actor || "—", title: r.actor || "" }),
    el("span", { class: "audit-ip muted mono", text: r.ip || "" }),
    el("strong", { class: "audit-action", text: r.action || "—", title: detail }),
    el("span", { class: "audit-target", text: auditTargetLabel(r), title: targetFull }),
    el("span", { class: "audit-path muted small", text: reqFull, title: reqFull }),
    auditStatusPill(r.status),
  ]);
}

// Rebuild the actor/action datalists + target-type select from the accumulated vocab. The
// select keeps its current value (options only ever grow, so it's always still present).
function refreshAuditFilterOptions() {
  const fill = (id, values) =>
    $(id).replaceChildren(...[...values].sort().map((v) => el("option", { value: v })));
  fill("aActorList", auditActors);
  fill("aActionList", auditActions);
  const sel = $("aTargetType");
  const cur = sel.value;
  sel.replaceChildren(
    el("option", { value: "", text: "Any target" }),
    ...[...auditTargetTypes].sort().map((v) => el("option", { value: v, text: v })),
  );
  sel.value = cur; // still present: the vocab set only grows
}

async function renderAudit() {
  const box = $("audit");
  const seq = ++auditSeq;
  const requested = auditLimit;
  let rows;
  try {
    rows = await getAudit({
      actor: $("aActor").value.trim() || undefined,
      action: $("aAction").value.trim() || undefined,
      targetType: $("aTargetType").value || undefined,
      sinceMs: localDateToMs($("aSince").value),
      limit: requested,
    });
  } catch (e) {
    if (seq === auditSeq) {
      box.replaceChildren(errorState("Audit error: " + e.message, renderAudit));
      $("auditCount").textContent = "—";
      $("auditMore").hidden = true;
    }
    return;
  }
  if (seq !== auditSeq) return; // superseded by a newer render — don't overwrite

  for (const r of rows) {
    if (r.actor) auditActors.add(r.actor);
    if (r.action) auditActions.add(r.action);
    if (r.targetType) auditTargetTypes.add(r.targetType);
  }
  refreshAuditFilterOptions();

  // A full page at the requested limit means older entries were cut off — say so with "+".
  const maybeMore = rows.length >= requested;
  $("auditCount").textContent = `${rows.length}${maybeMore ? "+" : ""}`;
  // "Load more" refetches at the next rung of the ladder (the API has no offset). Hidden when
  // the server returned fewer than requested (we have it all) or the 1000 clamp is reached.
  $("auditMore").hidden = !maybeMore || requested >= AUDIT_LIMITS[AUDIT_LIMITS.length - 1];

  if (!rows.length) {
    box.replaceChildren(emptyState("No audit entries match."));
    return;
  }
  box.replaceChildren(...rows.map(auditRow));
}

function initAudit() {
  // Apply starts a fresh query → back to the first rung; ⟳ re-reads at the current depth.
  $("aApply").addEventListener("click", () => {
    auditLimit = AUDIT_LIMITS[0];
    renderAudit();
  });
  $("auditRefresh").addEventListener("click", renderAudit);
  $("auditMore").addEventListener("click", () => {
    const next = AUDIT_LIMITS.indexOf(auditLimit) + 1;
    if (next < AUDIT_LIMITS.length) auditLimit = AUDIT_LIMITS[next];
    renderAudit();
  });
  renderAudit();
}

// ---- poll loop ------------------------------------------------------------

async function refresh() {
  try {
    const data = await getDashboard();
    everLoaded = true;
    $("dashBanner").hidden = true;
    setLive(true);
    setUpdated(`updated ${fmtClock(data.generated_at)}`);
    renderKpis(data);
    renderCameras(data.cameras || [], data.camera_summary);
    renderServices(data.services || []);
    renderQueues(data.queues || {});
    renderLoadtest(data.loadtest || null);
  } catch (e) {
    setLive(false);
    setUpdated("disconnected");
    // Text-only banner: e.message can echo server output and must never be parsed as HTML.
    renderBanner($("dashBanner"), {
      mode: "error",
      message: everLoaded
        ? `Lost connection to the viewer — retrying every ${REFRESH_MS / 1000}s.`
        : "Couldn't load /api/dashboard. Make sure the viewer is running the latest build (restart hushai-viewer), then this page recovers automatically.",
      detail: String(e.message || e),
    });
  }
}

// createPoller supplies the busy-guard, the hidden-tab pause, and the refocus refresh the old
// setInterval + visibilitychange pair implemented. refresh() renders its own error state.
const poller = createPoller(refresh, { intervalMs: REFRESH_MS });

initTopbar({ section: "system" });
initAudit(); // once on init + Apply/⟳ — the audit trail is NOT on the 6s poll
poller.start();
