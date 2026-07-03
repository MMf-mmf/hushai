// The Events page: the in-app alert feed (acknowledge / mark-all-read), the watchlist (subjects of
// interest with inline notes + sightings), the materialized event stream (filter + click-to-jump-
// the-timeline), and the alert-rule manager (list / enable-disable / edit / delete / create).
// Vanilla ES module, no build step — mirrors dashboard.js / manage.js. Reads the proxied
// /v1/events*, /v1/events/feed, /v1/alert-rules*, /v1/watchlist endpoints (see api.js). All
// user-supplied text (subject labels, plate OCR, rule names) is rendered via textContent — never
// innerHTML.

import {
  getDevices, getEvents, getEventFeed, ackDelivery,
  getAlertRules, createAlertRule, updateAlertRule, deleteAlertRule,
  getWatchlist, updateWatch, removeWatch, sampleFaceUrl, samplePlateUrl,
} from "../api.js";
import { el, errorState } from "../dom.js";
import { toast } from "../toast.js";
import { createPoller } from "../poll.js";
import { confirmAction } from "../confirm.js";
import { initTopbar, setLive, setUpdated } from "../nav.js";
import "../search/omni.js"; // "/" or Cmd+K global search palette

const $ = (id) => document.getElementById(id);

let devices = [];
let eventsSeq = 0; // monotonic render guards: a stale (slow) response must not clobber a newer render
let feedSeq = 0;
let watchSeq = 0;
let lastFeedItems = []; // what the feed currently shows — the "Mark all read" work list

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

// <input type="date"> value ("YYYY-MM-DD") → ms at LOCAL midnight. new Date("YYYY-MM-DD") would
// parse as UTC midnight and skew the filter by the tz offset.
function localDateToMs(v) {
  if (!v) return undefined;
  const [y, m, d] = v.split("-").map(Number);
  if (!isFinite(y) || !isFinite(m) || !isFinite(d)) return undefined;
  return new Date(y, m - 1, d).getTime();
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
// "Unread only" maps to the backend's 'pending' delivery status: feed-channel rows are minted
// 'pending' and stay there until ack flips them to 'acknowledged' (the sent/failed states belong
// to the webhook/push retry worker, which skips the feed channel). See events.rs FeedQuery.
function feedStatusFilter() {
  return $("feedStatus").value === "all" ? undefined : "pending";
}

async function renderFeed() {
  const box = $("feed");
  const seq = ++feedSeq;
  let items;
  try {
    items = await getEventFeed({ status: feedStatusFilter(), limit: 100 });
  } catch (e) {
    if (seq === feedSeq) {
      box.replaceChildren(errorState("Feed error: " + e.message, renderFeed));
      $("feedCount").textContent = "—";
    }
    return false;
  }
  if (seq !== feedSeq) return true; // superseded by a newer render — don't overwrite
  lastFeedItems = items;
  const unread = items.filter((i) => !i.acknowledged).length;
  $("feedCount").textContent = `${unread} unread`;
  $("feedAckAll").disabled = unread === 0;
  if (!items.length) {
    box.replaceChildren(el("div", {
      class: "empty",
      text: $("feedStatus").value === "all"
        ? "No alerts yet. Create a rule below — matching events will notify here."
        : "No unread alerts. Switch Show → All to see acknowledged ones.",
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
    disabled: !!i.acknowledged,
    onclick: async () => {
      ackBtn.disabled = true;
      try {
        await ackDelivery(i.deliveryId);
        await renderFeed();
      } catch (err) {
        toast("Acknowledge failed: " + err.message, { kind: "error" });
        ackBtn.disabled = false;
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

// Sequentially ack every unread delivery the feed currently lists. Sequential on purpose: one
// POST per row keeps the backend's per-row audit/404 semantics and avoids a burst of parallel
// writes; 100 rows is the feed cap so worst case is quick anyway.
async function ackAllFeed() {
  const btn = $("feedAckAll");
  const targets = lastFeedItems.filter((i) => !i.acknowledged);
  if (!targets.length) {
    toast("Nothing unread.");
    return;
  }
  btn.disabled = true;
  let ok = 0, failed = 0;
  for (const t of targets) {
    try {
      await ackDelivery(t.deliveryId);
      ok++;
    } catch {
      failed++;
    }
  }
  toast(
    failed ? `Marked ${ok} read · ${failed} failed` : `Marked ${ok} read`,
    { kind: failed ? "error" : "success" },
  );
  await renderFeed(); // re-enables the button via the unread count
}

// ---- watchlist ------------------------------------------------------------------
// Subjects of interest (person/plate). Server-side a watch IS a managed alert rule, so the
// enable/disable pill here flips that rule — rule renders are refreshed alongside. Loaded on
// init + Refresh only (not the 8s poll): the list changes through user actions, and re-rendering
// would tear down open sighting expanders and in-progress note edits.
async function renderWatchlist() {
  const box = $("watchlist");
  const seq = ++watchSeq;
  let rows;
  try {
    rows = await getWatchlist();
  } catch (e) {
    if (seq === watchSeq) {
      box.replaceChildren(errorState("Watchlist error: " + e.message, renderWatchlist));
      $("watchCount").textContent = "—";
    }
    return false;
  }
  if (seq !== watchSeq) return true;
  $("watchCount").textContent = `${rows.length}`;
  if (!rows.length) {
    box.replaceChildren(el("div", {
      class: "empty",
      text: "Nothing on the watchlist. Add people from the People modal (☆ Watch) or plates from the Plates modal.",
    }));
    return true;
  }
  box.replaceChildren(...rows.map(watchRow));
  return true;
}

function watchLabel(w) {
  return w.current_label || w.label || String(w.subject_id).slice(0, 8);
}

// Click-to-edit note. Enter/blur saves (blur after Enter is a no-op via the `done` latch),
// Escape cancels. The backend PATCH COALESCEs an absent reason, so we always send the string —
// an emptied note saves as "" rather than null (which the backend would ignore).
function reasonEditor(w) {
  const span = el("span", {
    class: "watch-reason" + (w.reason ? "" : " placeholder"),
    text: w.reason || "Add a note…",
    title: "Click to edit the note",
    role: "button",
    tabindex: "0",
  });
  const beginEdit = () => {
    const input = el("input", {
      class: "watch-reason-input",
      type: "text",
      placeholder: "Why is this on the watchlist?",
    });
    input.value = w.reason || "";
    let done = false;
    const finish = async (save) => {
      if (done) return;
      done = true;
      const next = input.value.trim();
      if (!save || next === (w.reason || "")) {
        input.replaceWith(span);
        return;
      }
      input.disabled = true;
      try {
        await updateWatch(w.watch_id, { reason: next });
        await renderWatchlist();
      } catch (e) {
        toast("Note save failed: " + e.message, { kind: "error" });
        input.replaceWith(span);
      }
    };
    input.addEventListener("keydown", (ev) => {
      if (ev.key === "Enter") { ev.preventDefault(); finish(true); }
      else if (ev.key === "Escape") finish(false);
    });
    input.addEventListener("blur", () => finish(true));
    span.replaceWith(input);
    input.focus();
    input.select();
  };
  span.addEventListener("click", beginEdit);
  span.addEventListener("keydown", (ev) => {
    if (ev.key === "Enter" || ev.key === " ") { ev.preventDefault(); beginEdit(); }
  });
  return span;
}

// Compact sighting row (sev · type · camera · when) that deep-links the main viewer.
function sightingRow(e) {
  const tag = e.deviceId ? "a" : "div";
  const props = { class: "watch-sighting" };
  if (e.deviceId) {
    props.href = eventLink(e.deviceId, e.startMs);
    props.title = "Jump to this moment";
  }
  return el(tag, props,
    sevChip(e.severity),
    el("span", { class: "ev-type", text: e.type }),
    el("span", { class: "ev-when", text: `${deviceName(e.deviceId)} · ${whenLabel(e.startMs)}` }));
}

// Lazy expander: sightings are fetched on first open only (kept across open/close toggles).
function sightingsBlock(w) {
  const box = el("div", { class: "watch-sightings", hidden: true });
  let loaded = false;
  const btn = el("button", {
    class: "link",
    type: "button",
    text: "▸ sightings",
    onclick: async () => {
      const opening = box.hidden;
      box.hidden = !opening;
      btn.textContent = opening ? "▾ sightings" : "▸ sightings";
      if (!opening || loaded) return;
      box.replaceChildren(el("div", { class: "empty", text: "Loading sightings…" }));
      let evs;
      try {
        evs = await getEvents({ subjectId: w.subject_id, limit: 50 });
      } catch (e) {
        box.replaceChildren(errorState("Sightings error: " + e.message));
        return;
      }
      loaded = true;
      box.replaceChildren(...(evs.length
        ? evs.map(sightingRow)
        : [el("div", { class: "empty", text: "No sightings recorded yet." })]));
    },
  });
  return { btn, box };
}

function watchRow(w) {
  const { btn: sightBtn, box: sightBox } = sightingsBlock(w);

  // Representative crop; the proxy injects the bearer so a plain <img> works. Subjects without a
  // stored sample 404 — onerror hides the img instead of showing the broken-image glyph.
  const thumbUrl = w.subject_type === "person" ? sampleFaceUrl(w.subject_id)
    : w.subject_type === "plate" ? samplePlateUrl(w.subject_id) : null;
  const thumb = thumbUrl ? el("img", { class: "watch-thumb", src: thumbUrl, alt: "", loading: "lazy" }) : null;
  if (thumb) thumb.addEventListener("error", () => { thumb.style.display = "none"; });

  const label = watchLabel(w);
  const toggle = el("button", {
    class: w.enabled ? "pill on" : "pill off",
    type: "button",
    text: w.enabled ? "on" : "off",
    title: w.enabled ? "Alerts firing — click to pause" : "Paused — click to resume alerts",
    onclick: async () => {
      toggle.disabled = true;
      try {
        await updateWatch(w.watch_id, { enabled: !w.enabled });
        // The toggle flips the managed alert rule too — keep both sections truthful.
        await Promise.all([renderWatchlist(), renderRules()]);
      } catch (e) {
        toast("Toggle failed: " + e.message, { kind: "error" });
        toggle.disabled = false;
      }
    },
  });
  const unwatch = el("button", {
    class: "link",
    type: "button",
    text: "unwatch",
    onclick: async () => {
      const ok = await confirmAction({
        title: "Remove from watchlist",
        message: `Stop watching “${label}”? Its managed alert rule is deleted too (past alerts are kept).`,
        confirmLabel: "Unwatch",
      });
      if (!ok) return;
      try {
        await removeWatch(w.watch_id);
        await Promise.all([renderWatchlist(), renderRules()]);
      } catch (e) {
        toast("Unwatch failed: " + e.message, { kind: "error" });
      }
    },
  });

  return el("div", { class: "watch-entry" },
    el("div", { class: "watch-row" },
      thumb,
      el("div", { class: "grow" },
        el("div", {},
          el("strong", { text: label }),
          el("span", { class: "watch-tag", text: w.subject_type })),
        reasonEditor(w)),
      sightBtn, toggle, unwatch),
    sightBox);
}

// ---- event stream ---------------------------------------------------------------
async function renderEvents() {
  const box = $("events");
  const seq = ++eventsSeq;
  const filters = {
    deviceId: $("fDevice").value || undefined,
    eventType: $("fType").value || undefined,
    severity: $("fSeverity").value || undefined,
    subjectType: $("fSubjectType").value || undefined,
    subjectId: $("fSubjectId").value.trim() || undefined,
    sinceMs: localDateToMs($("fSince").value),
    limit: 200,
  };
  let evs;
  try {
    evs = await getEvents(filters);
  } catch (e) {
    if (seq === eventsSeq) {
      box.replaceChildren(errorState("Events error: " + e.message, renderEvents));
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
let editingRule = null; // full original rule while the form is in edit mode (null = create mode)
let editingPrefillDev = ""; // what the camera select was prefilled to, to detect "untouched"

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
    box.replaceChildren(errorState("Rules error: " + e.message, renderRules));
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
        // Watch-managed rules surface their enabled state as the watchlist pill too.
        await Promise.all([renderRules(), renderWatchlist()]);
      } catch (e) {
        toast("Toggle failed: " + e.message, { kind: "error" });
      }
    },
  });
  const edit = el("button", {
    class: "link",
    text: "edit",
    onclick: () => startEditRule(r),
  });
  const del = el("button", {
    class: "link",
    text: "delete",
    onclick: async () => {
      const ok = await confirmAction({
        title: "Delete alert rule",
        message: `Delete rule “${r.name}”? Deliveries already sent are kept.`,
        confirmLabel: "Delete rule",
      });
      if (!ok) return;
      try {
        await deleteAlertRule(r.rule_id);
        if (editingRule?.rule_id === r.rule_id) exitEditMode();
        await Promise.all([renderRules(), renderWatchlist()]);
      } catch (e) {
        toast("Delete failed: " + e.message, { kind: "error" });
      }
    },
  });
  return el("div", { class: "rule-card" },
    el("span", { class: r.enabled ? "pill on" : "pill off", text: r.enabled ? "on" : "off" }),
    el("div", { class: "grow" },
      el("div", {}, el("strong", { text: r.name })),
      el("div", { class: "summary", text: ruleSummary(r) })),
    toggle, edit, del);
}

// Pre-fill the create form from an existing rule (the reverse of the submit assembly) and flip
// the form into edit mode. PATCH is a FULL replace server-side, so fields the form can't express
// (subject scoping, days_of_week, a multi-camera list, non-feed/webhook channels, the original
// tz) are carried through from the original on save — see submitRule.
function startEditRule(r) {
  editingRule = r;
  $("rName").value = r.name;
  for (const c of document.querySelectorAll("#rTypes input")) {
    c.checked = !!r.event_types?.includes(c.value);
  }
  // The form offers one camera; prefill the first (fall back to "all" if it's not in the list —
  // an untouched select keeps the ORIGINAL device_ids on save either way).
  const dev = r.device_ids?.[0] ?? "";
  $("rDevice").value = dev;
  editingPrefillDev = $("rDevice").value; // "" if the option didn't exist
  $("rSeverity").value = r.min_severity || "info";
  $("rFrom").value = r.time_start_minutes != null ? minToHHMM(r.time_start_minutes) : "";
  $("rTo").value = r.time_end_minutes != null ? minToHHMM(r.time_end_minutes) : "";
  $("rCooldown").value = String(r.cooldown_secs ?? 300);
  const chans = Array.isArray(r.channels) ? r.channels : [];
  $("rChFeed").checked = chans.some((c) => c?.type === "feed");
  $("rChHook").checked = chans.some((c) => c?.type === "webhook");
  $("rHookUrl").value = chans.find((c) => c?.type === "webhook")?.url || "";

  $("ruleSubmit").textContent = "💾 Save rule";
  $("ruleCancel").hidden = false;
  const msg = $("ruleFormMsg");
  msg.className = "small";
  msg.textContent = `Editing “${r.name}”`;
  $("ruleForm").scrollIntoView({ behavior: "smooth", block: "start" });
  $("rName").focus();
}

function exitEditMode() {
  editingRule = null;
  editingPrefillDev = "";
  $("ruleForm").reset();
  $("rSeverity").value = "warning";
  $("rCooldown").value = "300";
  $("rChFeed").checked = true;
  $("ruleSubmit").textContent = "＋ Create rule";
  $("ruleCancel").hidden = true;
  const msg = $("ruleFormMsg");
  msg.className = "small";
  msg.textContent = "";
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
  // Channel types the form doesn't know (e.g. push) survive an edit round-trip.
  if (editingRule && Array.isArray(editingRule.channels)) {
    channels.push(...editingRule.channels.filter((c) => c?.type !== "feed" && c?.type !== "webhook"));
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
  // An untouched camera select on edit keeps the rule's ORIGINAL device list (which may hold
  // several ids the single-select can't display); any change replaces it with the new pick.
  const deviceIds = editingRule && dev === editingPrefillDev
    ? editingRule.device_ids ?? []
    : (dev ? [dev] : []);
  const body = {
    name,
    // Editing must not silently re-enable a paused rule; creation starts enabled.
    enabled: editingRule ? editingRule.enabled : true,
    event_types: eventTypes,
    device_ids: deviceIds,
    min_severity: $("rSeverity").value,
    time_start_minutes: from,
    time_end_minutes: to,
    // The rule's local-time window is evaluated in the operator's tz (the browser's IANA zone).
    // On edit we keep the rule's ORIGINAL tz: the prefill showed its minutes as stored, so
    // swapping tz on save would silently shift the window.
    tz: editingRule
      ? (editingRule.tz || "UTC")
      : (Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC"),
    cooldown_secs: Math.max(0, Number($("rCooldown").value) || 0),
    channels,
  };
  if (editingRule) {
    // PATCH full-replaces; omitted fields would be nulled. Carry the scoping the form can't edit
    // (watch-managed rules live or die by subject_type/subject_ids).
    body.subject_type = editingRule.subject_type ?? null;
    body.subject_ids = editingRule.subject_ids ?? [];
    body.days_of_week = editingRule.days_of_week ?? [];
  }
  try {
    if (editingRule) {
      await updateAlertRule(editingRule.rule_id, body);
      exitEditMode();
      toast("Rule saved.", { kind: "success" });
    } else {
      await createAlertRule(body);
      exitEditMode(); // same reset path as create's old inline reset
      msg.textContent = "Rule created.";
    }
    await Promise.all([renderRules(), renderWatchlist()]);
  } catch (err) {
    msg.textContent = (editingRule ? "Save failed: " : "Create failed: ") + err.message;
    msg.className = "small err";
  }
}

// Don't claim "ok" / a fresh timestamp when a section failed — the live dot must not lie.
function markLive(allOk) {
  setUpdated((allOk ? "updated " : "partial · ") + new Date().toLocaleTimeString());
  setLive(allOk);
}

async function refreshAll() {
  const oks = await Promise.all([renderFeed(), renderWatchlist(), renderEvents(), renderRules()]);
  $("banner").style.display = "none";
  markLive(oks.every(Boolean));
}

// Light polling so new alerts/events surface without a manual reload (rules + watchlist only
// change through user actions here, which re-render them directly — and re-rendering the
// watchlist on a timer would tear down open expanders/note edits). createPoller supplies the
// in-flight guard, the hidden-tab pause, and the refocus kick the old hand-rolled loop implemented.
const poller = createPoller(async () => {
  const oks = await Promise.all([renderFeed(), renderEvents()]);
  markLive(oks.every(Boolean));
}, { intervalMs: 8000 });

async function main() {
  initTopbar({ section: "events" });
  await loadDevices();
  $("fApply").addEventListener("click", () => { renderEvents(); renderFeed(); renderWatchlist(); });
  for (const id of ["fDevice", "fType", "fSeverity", "fSubjectType", "fSubjectId", "fSince"]) {
    $(id).addEventListener("change", renderEvents);
  }
  $("feedStatus").addEventListener("change", renderFeed);
  $("feedAckAll").addEventListener("click", ackAllFeed);
  $("ruleForm").addEventListener("submit", submitRule);
  $("ruleCancel").addEventListener("click", exitEditMode);
  await refreshAll();
  poller.start({ immediate: false });
}

main().catch((e) => {
  const b = $("banner");
  b.textContent = "Failed to load: " + e.message;
  b.className = "dash-banner error";
});

// Tiny debug handle for headless verification (localhost-only tool).
if (["localhost", "127.0.0.1", "::1"].includes(location.hostname)) {
  window.eventsDebug = {
    renderFeed, renderEvents, renderRules, renderWatchlist, refreshAll, ackAllFeed,
    startEditRule, exitEditMode,
    get devices() { return devices; },
    get editingRule() { return editingRule; },
  };
}
