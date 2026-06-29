// The Events page: the in-app alert feed (acknowledge), the materialized event stream (filter +
// click-to-jump-the-timeline), and the alert-rule manager (list / enable-disable / delete / create).
// Vanilla ES module, no build step — mirrors dashboard.js / manage.js. Reads the proxied
// /v1/events*, /v1/events/feed, /v1/alert-rules* endpoints (see api.js). All user-supplied text
// (subject labels, plate OCR, rule names) is rendered via textContent — never innerHTML.

import {
  getDevices, getEvents, getEventFeed, ackDelivery,
  getAlertRules, createAlertRule, updateAlertRule, deleteAlertRule,
} from "../api.js";

const $ = (id) => document.getElementById(id);

let devices = [];
let pollTimer = null;
let polling = false; // in-flight guard so slow ticks don't stack
let eventsSeq = 0; // monotonic render guards: a stale (slow) response must not clobber a newer render
let feedSeq = 0;

/** Minimal DOM builder. `text` sets textContent (XSS-safe); `on*` adds a listener. */
function el(tag, props = {}, ...kids) {
  const n = document.createElement(tag);
  for (const [k, v] of Object.entries(props)) {
    if (v == null) continue;
    if (k === "class") n.className = v;
    else if (k === "text") n.textContent = v;
    else if (k.startsWith("on") && typeof v === "function") n.addEventListener(k.slice(2), v);
    else n.setAttribute(k, v);
  }
  for (const kid of kids) if (kid != null) n.append(kid);
  return n;
}

function deviceName(id) {
  const d = devices.find((x) => x.id === id);
  return d ? d.displayName || d.id : id || "—";
}

function whenLabel(ms) {
  if (!ms) return "—";
  const t = new Date(ms).toLocaleString([], {
    month: "short", day: "numeric", hour: "2-digit", minute: "2-digit",
  });
  const diff = (Date.now() - ms) / 1000;
  if (diff >= 0 && diff < 60) return `just now · ${t}`;
  if (diff >= 0 && diff < 3600) return `${Math.floor(diff / 60)}m ago · ${t}`;
  if (diff >= 0 && diff < 86400) return `${Math.floor(diff / 3600)}h ago · ${t}`;
  return t;
}

function sevChip(sev) {
  return el("span", { class: `sev sev-${sev || "info"}`, text: sev || "info" });
}

// Deep-link into the main viewer at this camera + instant (app.js init honors ?device=&t=).
function eventLink(deviceId, ms) {
  return `/?device=${encodeURIComponent(deviceId)}&t=${Math.floor(ms || 0)}`;
}

async function loadDevices() {
  try {
    devices = await getDevices();
  } catch {
    devices = [];
  }
  for (const selId of ["fDevice", "rDevice"]) {
    const sel = $(selId);
    for (const d of devices) sel.append(el("option", { value: d.id, text: d.displayName || d.id }));
  }
}

// ---- alert feed -----------------------------------------------------------------
async function renderFeed() {
  const box = $("feed");
  const seq = ++feedSeq;
  let items;
  try {
    items = await getEventFeed({ limit: 100 });
  } catch (e) {
    if (seq === feedSeq) {
      box.replaceChildren(el("div", { class: "err", text: "Feed error: " + e.message }));
      $("feedCount").textContent = "—";
    }
    return false;
  }
  if (seq !== feedSeq) return true; // superseded by a newer render — don't overwrite
  const unread = items.filter((i) => !i.acknowledged).length;
  $("feedCount").textContent = `${unread} unread`;
  if (!items.length) {
    box.replaceChildren(el("div", {
      class: "empty",
      text: "No alerts yet. Create a rule below — matching events will notify here.",
    }));
    return true;
  }
  box.replaceChildren(...items.map(feedRow));
  return true;
}

function feedRow(i) {
  const ackBtn = el("button", {
    class: "ghost",
    text: i.acknowledged ? "✓ read" : "Acknowledge",
    disabled: i.acknowledged ? "" : null,
    onclick: async (ev) => {
      ev.currentTarget.disabled = true;
      try {
        await ackDelivery(i.deliveryId);
        await renderFeed();
      } catch (err) {
        alert("Acknowledge failed: " + err.message);
        ev.currentTarget.disabled = false;
      }
    },
  });
  // Deep-link to the EVENT's footage moment when known (the alert-fire time is close but not exact),
  // falling back to the delivery time only when the event was purged.
  const view = i.deviceId
    ? el("a", { class: "navlink", href: eventLink(i.deviceId, i.eventStartMs ?? i.createdMs), text: "▶ view" })
    : null;
  return el("div", { class: "feed-item" + (i.acknowledged ? " muted" : "") },
    sevChip(i.severity),
    el("div", { class: "grow" },
      el("div", {},
        el("span", { class: "ev-type", text: i.eventType || "event" }),
        i.subjectLabel ? el("span", { class: "ev-sub", text: " · " + i.subjectLabel }) : null),
      el("div", { class: "ev-when", text: `${deviceName(i.deviceId)} · ${whenLabel(i.createdMs)}` })),
    view, ackBtn);
}

// ---- event stream ---------------------------------------------------------------
async function renderEvents() {
  const box = $("events");
  const seq = ++eventsSeq;
  const filters = {
    deviceId: $("fDevice").value || undefined,
    eventType: $("fType").value || undefined,
    severity: $("fSeverity").value || undefined,
    limit: 200,
  };
  let evs;
  try {
    evs = await getEvents(filters);
  } catch (e) {
    if (seq === eventsSeq) {
      box.replaceChildren(el("div", { class: "err", text: "Events error: " + e.message }));
      $("eventCount").textContent = "—";
    }
    return false;
  }
  // A slow poll started under old filters must not overwrite the freshly-filtered list the user
  // just requested (a newer renderEvents bumped eventsSeq).
  if (seq !== eventsSeq) return true;
  $("eventCount").textContent = `${evs.length}`;
  if (!evs.length) {
    box.replaceChildren(el("div", {
      class: "empty",
      text: "No events match. Events appear as the worker processes footage (faces, plates, objects, speech).",
    }));
    return true;
  }
  box.replaceChildren(...evs.map(eventRow));
  return true;
}

function eventRow(e) {
  // The real ▶ view <a> is the single interactive control — keyboard-reachable + cmd/middle-clickable.
  // (We dropped the whole-row click handler: a div+click is invisible to keyboards, and a per-row
  // tabindex would spam tab stops. The anchor covers both mouse and keyboard accessibly.)
  const link = e.deviceId ? eventLink(e.deviceId, e.startMs) : null;
  return el("div", { class: "ev-row" },
    sevChip(e.severity),
    el("span", { class: "ev-type", text: e.type }),
    el("span", { class: "ev-sub", text: e.subjectLabel || (e.subjectType ? `(${e.subjectType})` : "—") }),
    el("span", { class: "ev-when", text: `${deviceName(e.deviceId)} · ${whenLabel(e.startMs)}` }),
    link ? el("a", { class: "navlink", href: link, title: "Jump to this moment", text: "▶ view" }) : el("span", {}));
}

// ---- alert rules ----------------------------------------------------------------
function minToHHMM(m) {
  const h = Math.floor(m / 60), mm = m % 60;
  return `${String(h).padStart(2, "0")}:${String(mm).padStart(2, "0")}`;
}

function ruleSummary(r) {
  const parts = [];
  parts.push(r.event_types?.length ? r.event_types.join(", ") : "any event");
  parts.push(r.device_ids?.length ? `${r.device_ids.length} camera(s)` : "all cameras");
  parts.push(`≥ ${r.min_severity}`);
  if (r.time_start_minutes != null && r.time_end_minutes != null) {
    parts.push(`${minToHHMM(r.time_start_minutes)}–${minToHHMM(r.time_end_minutes)} ${r.tz || "UTC"}`);
  }
  const chans = Array.isArray(r.channels) ? r.channels.map((c) => c.type).join("+") : "feed";
  parts.push(chans);
  parts.push(`cooldown ${r.cooldown_secs}s`);
  return parts.join(" · ");
}

async function renderRules() {
  const box = $("rules");
  let rules;
  try {
    rules = await getAlertRules();
  } catch (e) {
    box.replaceChildren(el("div", { class: "err", text: "Rules error: " + e.message }));
    $("ruleCount").textContent = "—";
    return false;
  }
  $("ruleCount").textContent = `${rules.length}`;
  if (!rules.length) {
    box.replaceChildren(el("div", { class: "empty", text: "No alert rules yet. Create one below." }));
    return true;
  }
  box.replaceChildren(...rules.map(ruleCard));
  return true;
}

function ruleCard(r) {
  const toggle = el("button", {
    class: "link",
    text: r.enabled ? "disable" : "enable",
    onclick: async () => {
      try {
        await updateAlertRule(r.rule_id, { ...r, enabled: !r.enabled });
        await renderRules();
      } catch (e) {
        alert("Toggle failed: " + e.message);
      }
    },
  });
  const del = el("button", {
    class: "link",
    text: "delete",
    onclick: async () => {
      if (!confirm(`Delete rule "${r.name}"?`)) return;
      try {
        await deleteAlertRule(r.rule_id);
        await renderRules();
      } catch (e) {
        alert("Delete failed: " + e.message);
      }
    },
  });
  return el("div", { class: "rule-card" },
    el("span", { class: r.enabled ? "pill" : "pill off", text: r.enabled ? "on" : "off" }),
    el("div", { class: "grow" },
      el("div", {}, el("strong", { text: r.name })),
      el("div", { class: "summary", text: ruleSummary(r) })),
    toggle, del);
}

function hhmmToMin(v) {
  if (!v) return null;
  const [h, m] = v.split(":").map(Number);
  if (!isFinite(h) || !isFinite(m)) return null;
  return h * 60 + m;
}

async function submitRule(ev) {
  ev.preventDefault();
  const msg = $("ruleFormMsg");
  msg.className = "small";
  msg.textContent = "";
  const name = $("rName").value.trim();
  if (!name) { msg.textContent = "Name required."; msg.className = "small err"; return; }

  const eventTypes = [...document.querySelectorAll("#rTypes input:checked")].map((c) => c.value);
  const channels = [];
  if ($("rChFeed").checked) channels.push({ type: "feed" });
  if ($("rChHook").checked) {
    const url = $("rHookUrl").value.trim();
    if (!url) { msg.textContent = "Webhook URL required."; msg.className = "small err"; return; }
    let parsed;
    try { parsed = new URL(url); } catch { parsed = null; }
    if (!parsed || (parsed.protocol !== "http:" && parsed.protocol !== "https:")) {
      msg.textContent = "Webhook URL must be a valid http(s) URL."; msg.className = "small err"; return;
    }
    channels.push({ type: "webhook", url });
  }
  if (!channels.length) { msg.textContent = "Pick at least one channel."; msg.className = "small err"; return; }

  const from = hhmmToMin($("rFrom").value);
  const to = hhmmToMin($("rTo").value);
  if ((from == null) !== (to == null)) {
    msg.textContent = "Set both from + until, or neither.";
    msg.className = "small err";
    return;
  }
  if (from != null && from === to) {
    // Mirror the backend's zero-width rejection so the user gets an inline message, not a 400.
    msg.textContent = "Start and end must differ (omit both for always-on).";
    msg.className = "small err";
    return;
  }
  const dev = $("rDevice").value;
  const body = {
    name,
    enabled: true,
    event_types: eventTypes,
    device_ids: dev ? [dev] : [],
    min_severity: $("rSeverity").value,
    time_start_minutes: from,
    time_end_minutes: to,
    // The rule's local-time window is evaluated in the operator's tz (the browser's IANA zone).
    tz: Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC",
    cooldown_secs: Math.max(0, Number($("rCooldown").value) || 0),
    channels,
  };
  try {
    await createAlertRule(body);
    $("ruleForm").reset();
    $("rSeverity").value = "warning";
    $("rCooldown").value = "300";
    $("rChFeed").checked = true;
    msg.textContent = "Rule created.";
    await renderRules();
  } catch (err) {
    msg.textContent = "Create failed: " + err.message;
    msg.className = "small err";
  }
}

async function refreshAll() {
  const oks = await Promise.all([renderFeed(), renderEvents(), renderRules()]);
  $("banner").style.display = "none";
  const allOk = oks.every(Boolean);
  // Don't claim "ok" / a fresh timestamp when a section failed — the live dot must not lie.
  $("generatedAt").textContent = (allOk ? "updated " : "partial · ") + new Date().toLocaleTimeString();
  $("liveDot").classList.toggle("ok", allOk);
}

// One poll tick: skip when the tab is hidden or a prior tick is still running (no stacking).
async function pollTick() {
  if (document.hidden || polling) return;
  polling = true;
  try {
    await Promise.all([renderFeed(), renderEvents()]);
  } finally {
    polling = false;
  }
}

async function main() {
  await loadDevices();
  $("fApply").addEventListener("click", () => { renderEvents(); renderFeed(); });
  for (const id of ["fDevice", "fType", "fSeverity"]) $(id).addEventListener("change", renderEvents);
  $("ruleForm").addEventListener("submit", submitRule);
  await refreshAll();
  // Light polling so new alerts/events surface without a manual reload.
  pollTimer = setInterval(pollTick, 8000);
  // Resume promptly when the tab is refocused; tear the timer down on navigate-away.
  document.addEventListener("visibilitychange", () => { if (!document.hidden) pollTick(); });
  window.addEventListener("pagehide", () => { if (pollTimer) { clearInterval(pollTimer); pollTimer = null; } });
}

main().catch((e) => {
  const b = $("banner");
  b.textContent = "Failed to load: " + e.message;
  b.className = "dash-banner err";
});

// Tiny debug handle for headless verification (localhost-only tool).
window.eventsDebug = { renderFeed, renderEvents, renderRules, refreshAll, get devices() { return devices; } };
