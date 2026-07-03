// The viewer's events UI layer, two pieces:
//   1. Events drawer — a collapsible rail between the video stage and the chat dock
//      (markup skeleton in index.html #eventsDrawer, toggled by #eventsToggle in the
//      timeline controls). Filter chips (severity + this-camera/all-cameras) over a
//      newest-first list; clicking a row jumps the player there.
//   2. Alert bell — the topbar 🔔 with an unacked-deliveries badge and a dropdown
//      (positioned like #aiInfoPop) offering per-item Ack and ▶ view.
//
// Coupling is deliberately thin: app.js owns the data flow (device selection, event
// polling, seeks) and passes callbacks + fetchers in; this module only renders. The
// selected camera's events are PUSHED via setEvents (one source shared with the
// timeline lane); the all-cameras scope refetches for itself via `fetchEvents`.

import { el, emptyState, errorState } from "./dom.js";
import { toast } from "./toast.js";
import { clock } from "./time.js";

const SEVERITIES = ["all", "info", "warning", "critical"];
const MAX_ROWS = 300; // list render cap; fetches are already limit-bounded upstream

// "just now" / "5m ago" / "3h ago" / "2d ago" for feed items.
function relTime(ms) {
  const s = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (s < 60) return "just now";
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.floor(m / 60);
  if (h < 48) return `${h}h ago`;
  return `${Math.floor(h / 24)}d ago`;
}

// ---- events drawer ---------------------------------------------------------------

/** Wire the events drawer. `onSeek(ms)` jumps within the current camera,
 *  `onSelectDevice(deviceId, ms)` switches camera + seeks, `fetchEvents(opts)` is
 *  api.js getEvents (used only for the all-cameras scope). Returns
 *  { setEvents(list), setDeviceId(id) } — app.js pushes the selected camera's events
 *  through setEvents so the drawer and the timeline lane share one source. */
export function initEventsOverlay({ onSeek, onSelectDevice, fetchEvents }) {
  const drawer = document.getElementById("eventsDrawer");
  const toggleBtn = document.getElementById("eventsToggle");
  const closeBtn = document.getElementById("eventsDrawerClose");
  const filters = document.getElementById("evFilters");
  const list = document.getElementById("evList");
  const count = document.getElementById("evCount");
  if (!drawer || !toggleBtn || !filters || !list) return null;

  let deviceId = null;
  let deviceEvents = []; // pushed by app.js for the selected camera
  let allEvents = null; // lazily fetched for the all-cameras scope (null = not yet)
  let allError = null;
  let allInflight = false;
  let sev = "all";
  let scope = "this"; // "this" | "all"

  function chip(label, active, onclick) {
    return el("button", {
      class: `ev-chip${active ? " active" : ""}`,
      type: "button",
      "aria-pressed": String(active),
      text: label,
      onclick,
    });
  }

  function renderFilters() {
    filters.replaceChildren(
      ...SEVERITIES.map((s) => chip(s, sev === s, () => setSev(s))),
      el("span", { class: "ev-chip-sep", "aria-hidden": "true" }),
      chip("this camera", scope === "this", () => setScope("this")),
      chip("all cameras", scope === "all", () => setScope("all")),
    );
  }

  function setSev(s) {
    sev = s;
    renderFilters();
    renderList();
  }

  function setScope(s) {
    if (scope === s) return;
    scope = s;
    renderFilters();
    if (scope === "all") refreshAll();
    else renderList();
  }

  // The all-cameras scope isn't part of app.js's device-scoped polling — it fetches
  // for itself (and re-fetches whenever new device-scoped data lands while it's open).
  async function refreshAll() {
    if (allInflight) return;
    allInflight = true;
    allError = null;
    if (allEvents == null) renderList(); // first fetch: show the loading state
    try {
      allEvents = await fetchEvents({ limit: 500 });
    } catch (e) {
      allError = e?.message || String(e);
    }
    allInflight = false;
    renderList();
  }

  function openEvent(ev) {
    if (ev.deviceId && deviceId && ev.deviceId !== deviceId) onSelectDevice(ev.deviceId, ev.startMs);
    else onSeek(ev.startMs);
  }

  function row(ev) {
    return el(
      "button",
      { class: "ev-item", type: "button", onclick: () => openEvent(ev) },
      el("span", { class: `sev sev-${ev.severity}`, text: ev.severity }),
      el(
        "span",
        { class: "ev-item-main" },
        el("span", { class: "ev-item-type", text: ev.type }),
        ev.subjectLabel ? el("span", { class: "ev-item-sub", text: ev.subjectLabel }) : null,
        scope === "all" && ev.deviceId ? el("span", { class: "ev-item-dev", text: ev.deviceId }) : null,
      ),
      el("span", { class: "ev-item-when mono", text: clock(ev.startMs) }),
    );
  }

  function setCount(text) {
    if (count) count.textContent = text;
  }

  function renderList() {
    if (drawer.hidden) return; // nothing to lay out while collapsed
    if (scope === "all" && allError) {
      setCount("");
      list.replaceChildren(errorState(`Events: ${allError}`, refreshAll));
      return;
    }
    const src = scope === "this" ? deviceEvents : allEvents;
    if (src == null) {
      setCount("");
      list.replaceChildren(emptyState("Loading events…"));
      return;
    }
    const rows = src
      .filter((ev) => sev === "all" || ev.severity === sev)
      .sort((a, b) => b.startMs - a.startMs); // newest first
    setCount(rows.length ? String(rows.length) : "");
    if (!rows.length) {
      list.replaceChildren(emptyState(sev === "all" ? "No events yet." : `No ${sev} events.`));
      return;
    }
    list.replaceChildren(...rows.slice(0, MAX_ROWS).map(row));
  }

  function setOpen(open) {
    drawer.hidden = !open;
    toggleBtn.setAttribute("aria-pressed", String(open));
    toggleBtn.classList.toggle("active", open);
    if (open) {
      if (scope === "all" && allEvents == null) refreshAll();
      renderList();
    }
  }

  toggleBtn.onclick = () => setOpen(drawer.hidden);
  if (closeBtn) closeBtn.onclick = () => setOpen(false);
  renderFilters();

  return {
    /** The selected camera's events (app.js pushes on every change). */
    setEvents(events) {
      deviceEvents = events || [];
      if (drawer.hidden) return;
      if (scope === "this") renderList();
      else refreshAll(); // new data landed — keep the cross-camera view fresh too
    },
    /** Which camera is open, so row clicks know seek vs switch-then-seek. */
    setDeviceId(id) {
      deviceId = id;
      if (scope === "all" && !drawer.hidden) renderList(); // device chips stay, but rows don't change
    },
  };
}

// ---- alert bell -------------------------------------------------------------------

/** Wire the topbar bell (#bellBtn/#bellBadge/#bellPop). `fetchFeed()` returns api.js
 *  getEventFeed items, `ack(deliveryId)` acknowledges one, `onView(item)` jumps the
 *  player to the alert's footage. Returns { refresh } — app.js calls it on every other
 *  auto-refresh tick. The badge count is mirrored into document.title. */
export function initAlertBell({ fetchFeed, ack, onView }) {
  const btn = document.getElementById("bellBtn");
  const badge = document.getElementById("bellBadge");
  const pop = document.getElementById("bellPop");
  if (!btn || !badge || !pop) return { refresh: async () => {} };

  const baseTitle = document.title || "Hushai Viewer";
  let items = [];

  const unacked = () => items.filter((i) => !i.acknowledged);

  function renderBadge() {
    const n = unacked().length;
    badge.hidden = n === 0;
    badge.textContent = n > 99 ? "99+" : String(n);
    document.title = n > 0 ? `(${n}) ${baseTitle}` : baseTitle;
  }

  function bellRow(item) {
    const ackBtn = el("button", {
      type: "button",
      text: "Ack",
      title: "Acknowledge this alert",
      onclick: async (e) => {
        e.stopPropagation();
        ackBtn.disabled = true;
        try {
          await ack(item.deliveryId);
          item.acknowledged = true;
          renderBadge();
          renderPop();
        } catch (err) {
          ackBtn.disabled = false;
          toast(`Acknowledge failed: ${err?.message || err}`, { kind: "error" });
        }
      },
    });
    const viewBtn =
      item.eventStartMs != null
        ? el("button", {
            type: "button",
            text: "▶",
            title: "View footage",
            "aria-label": "View footage",
            onclick: (e) => {
              e.stopPropagation();
              close();
              onView(item);
            },
          })
        : null;
    return el(
      "div",
      { class: "bell-item" },
      el("span", { class: `sev sev-${item.severity || "info"}`, text: item.severity || "info" }),
      el(
        "span",
        { class: "bell-item-main" },
        el("span", { class: "bell-item-label", text: item.subjectLabel || item.eventType || "event" }),
        el("span", {
          class: "bell-item-meta",
          text: [item.deviceId, relTime(item.createdMs)].filter(Boolean).join(" · "),
        }),
      ),
      viewBtn,
      ackBtn,
    );
  }

  function renderPop() {
    const rows = unacked();
    const listEl = el("div", { class: "bell-pop-list" });
    if (!rows.length) listEl.append(el("div", { class: "bell-empty", text: "No unacknowledged alerts." }));
    else for (const item of rows) listEl.append(bellRow(item));
    pop.replaceChildren(
      listEl,
      el("div", { class: "bell-foot" }, el("a", { href: "/events.html", text: "Open Alerts center →" })),
    );
  }

  function close() {
    if (pop.hidden) return;
    pop.hidden = true;
    btn.setAttribute("aria-expanded", "false");
  }

  btn.addEventListener("click", (e) => {
    e.stopPropagation(); // keep the document-level outside-click closer out of it
    const willOpen = pop.hidden;
    if (willOpen) renderPop();
    pop.hidden = !willOpen;
    btn.setAttribute("aria-expanded", String(willOpen));
  });
  document.addEventListener("click", (e) => {
    if (!pop.hidden && !pop.contains(e.target)) close();
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") close();
  });

  async function refresh() {
    let next;
    try {
      next = await fetchFeed();
    } catch {
      return; // transient — the next tick retries; keep the last known state
    }
    items = next;
    renderBadge();
    if (!pop.hidden) renderPop();
  }

  return { refresh };
}
