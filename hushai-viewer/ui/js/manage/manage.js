// File-management page: per-device storage usage, rename, retention policy, and footage/device
// deletion. Built on the shared primitives (dom.js el(), poll.js, toast.js, nav.js); all calls
// go to hushai-backend's /v1/devices* surface via the viewer proxy (export is a viewer route).

import {
  getManagedDevices,
  getDeviceUsage,
  renameDevice,
  setRetention,
  deleteDevice,
  deleteFootageDay,
  bulkDeleteFootageDays,
  exportUrl,
} from "../api.js";
import { confirmAction, confirmOpen } from "../confirm.js";
import { humanBytes, dateLabel } from "../time.js";
import { el, renderBanner } from "../dom.js";
import { toast } from "../toast.js";
import { createPoller } from "../poll.js";
import { initTopbar, setLive, setUpdated } from "../nav.js";
import "../search/omni.js"; // "/" or Cmd+K global search palette

const $ = (id) => document.getElementById(id);
const TZ = Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC";
const REFRESH_MS = 15000;

let devices = [];
const usage = new Map(); // deviceId -> days[]
const expanded = new Set(); // open drill-downs
const selected = new Map(); // deviceId -> Set(dayString)
let busy = false; // direct reload() calls (mutations / ⟳) must not stack with a poll tick
let everLoaded = false;

function deviceLabel(d) {
  return d.displayName || d.id;
}

// Export kind: muxed if present, else video; audio-only devices can't export video.
function kindFor(d) {
  return d.hasMuxed ? "muxed" : d.hasVideo ? "video" : null;
}

function dayLabel(date) {
  const [y, m, d] = date.split("-").map(Number);
  return new Date(y, m - 1, d).toLocaleDateString(undefined, {
    weekday: "short",
    year: "numeric",
    month: "short",
    day: "numeric",
  });
}

function triggerDownload(url) {
  const a = document.createElement("a");
  a.href = url;
  a.download = "";
  document.body.appendChild(a);
  a.click();
  a.remove();
}

function isEditing() {
  const a = document.activeElement;
  return !!a && a.tagName === "INPUT" && a.closest("#mgmtList") != null;
}

// ---- KPI tiles ------------------------------------------------------------

function kpi(label, value, sub) {
  return el("div", { class: "kpi" }, [
    el("div", { class: "kpi-label", text: label }),
    el("div", { class: "kpi-value", text: value }),
    el("div", { class: "kpi-sub", text: sub || "" }),
  ]);
}

function renderKpis() {
  const root = $("mgmtKpis");
  root.replaceChildren();
  const totalBytes = devices.reduce((a, d) => a + (d.bytes || 0), 0);
  const totalSeg = devices.reduce((a, d) => a + (d.segmentCount || 0), 0);
  const withPolicy = devices.filter((d) => d.retentionDays != null).length;
  root.append(
    kpi("Devices", String(devices.length), `${withPolicy} with retention`),
    kpi("Footage on disk", humanBytes(totalBytes), `${totalSeg.toLocaleString()} segments`),
  );
}

// ---- rendering ------------------------------------------------------------

function render() {
  const list = $("mgmtList");
  list.replaceChildren();
  if (!devices.length) {
    list.appendChild(el("div", { class: "empty muted", text: "No cameras have reported footage yet." }));
    return;
  }
  for (const d of devices) list.appendChild(deviceCard(d));
}

function deviceCard(d) {
  const details = el("details", { class: "card mgmt-device" });
  details.open = expanded.has(d.id);

  // -- summary: name + rename, stats, retention, delete-device --
  const summary = el("summary", { class: "mgmt-summary" });

  const nameInput = el("input", {
    class: "mgmt-name-input",
    type: "text",
    value: d.displayName || "",
    placeholder: d.id,
    title: "Friendly name",
  });
  const saveName = el("button", { class: "ghost", type: "button", text: "Rename" });
  const nameWrap = el("div", { class: "mgmt-name" }, [nameInput, saveName]);
  if (deviceLabel(d) !== d.id) {
    nameWrap.appendChild(el("span", { class: "muted small mono mgmt-rawid", text: d.id, title: "device id" }));
  }
  saveName.addEventListener("click", (e) => {
    e.preventDefault();
    e.stopPropagation();
    doRename(d, nameInput.value);
  });
  nameInput.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      doRename(d, nameInput.value);
    }
  });
  // Don't let clicks on the controls toggle the <summary>.
  for (const n of [nameInput, saveName]) n.addEventListener("click", (e) => e.stopPropagation());

  const range =
    d.earliestMs && d.latestMs ? `${dateLabel(d.earliestMs)} – ${dateLabel(d.latestMs)}` : "no footage";
  const stats = el("div", { class: "mgmt-stats small muted" }, [
    el("span", { class: "pill", text: d.sourceKind || "?" }),
    el("span", { text: `${(d.segmentCount || 0).toLocaleString()} segments` }),
    el("span", { text: humanBytes(d.bytes) }),
    el("span", { text: range }),
  ]);

  const retInput = el("input", {
    class: "mgmt-ret-input",
    type: "number",
    min: "1",
    placeholder: "∞",
    value: d.retentionDays != null ? String(d.retentionDays) : "",
    title: "Days of footage to keep (blank = keep forever)",
  });
  const retSet = el("button", { class: "ghost", type: "button", text: "Set" });
  const ret = el("div", { class: "mgmt-retention" }, [
    el("span", { class: "muted small", text: "Keep last" }),
    retInput,
    el("span", { class: "muted small", text: "days" }),
    retSet,
  ]);
  retSet.addEventListener("click", (e) => {
    e.preventDefault();
    e.stopPropagation();
    doRetention(d, retInput.value);
  });
  retInput.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      doRetention(d, retInput.value);
    }
  });
  for (const n of [retInput, retSet]) n.addEventListener("click", (e) => e.stopPropagation());

  const delDev = el("button", { class: "danger mgmt-del-device", type: "button", text: "Delete device" });
  delDev.addEventListener("click", (e) => {
    e.preventDefault();
    e.stopPropagation();
    doDeleteDevice(d);
  });

  summary.append(nameWrap, stats, ret, delDev);
  details.appendChild(summary);

  // -- body: per-day usage (lazy) --
  const body = el("div", { class: "mgmt-body" });
  details.appendChild(body);
  if (details.open) renderUsage(d, body);

  details.addEventListener("toggle", () => {
    if (details.open) {
      expanded.add(d.id);
      if (usage.has(d.id)) renderUsage(d, body);
      else loadUsage(d.id);
    } else {
      expanded.delete(d.id);
    }
  });
  return details;
}

function renderUsage(d, body) {
  body.replaceChildren();
  const days = usage.get(d.id);
  if (days == null) {
    body.appendChild(el("div", { class: "muted small", text: "Loading…" }));
    return;
  }
  if (!days.length) {
    body.appendChild(el("div", { class: "mgmt-usage-empty muted small", text: "No footage stored for this device." }));
    return;
  }

  const sel = selected.get(d.id) || new Set();
  selected.set(d.id, sel);
  const kind = kindFor(d);

  // bulk bar
  const selAll = el("input", { type: "checkbox", title: "Select all days" });
  selAll.checked = sel.size === days.length && days.length > 0;
  const delSel = el("button", { class: "danger", type: "button" });
  const expSel = el("button", { class: "ghost", type: "button", text: "Export selected" });
  const updateBulk = () => {
    delSel.textContent = `Delete selected (${sel.size})`;
    delSel.disabled = sel.size === 0;
    expSel.disabled = sel.size === 0 || !kind;
    selAll.checked = sel.size === days.length && days.length > 0;
  };
  selAll.addEventListener("change", () => {
    sel.clear();
    if (selAll.checked) for (const day of days) sel.add(day.date);
    renderUsage(d, body);
  });
  delSel.addEventListener("click", () => doBulkDelete(d, [...sel], days));
  expSel.addEventListener("click", () => doExportSelected(d, [...sel], days, kind));
  const bar = el("div", { class: "mgmt-bulkbar" }, [
    el("label", { class: "mgmt-selall" }, [selAll, el("span", { class: "muted small", text: "All" })]),
    el("span", { class: "spacer" }),
    expSel,
    delSel,
  ]);
  body.appendChild(bar);

  // per-day rows
  const table = el("div", { class: "usage-table" });
  for (const day of days) {
    const cb = el("input", { type: "checkbox" });
    cb.checked = sel.has(day.date);
    cb.addEventListener("change", () => {
      if (cb.checked) sel.add(day.date);
      else sel.delete(day.date);
      updateBulk();
    });
    const exp = kind
      ? el("a", {
          class: "usage-export",
          href: exportUrl(d.id, day.startMs, day.endMs, kind),
          download: "",
          title: "Download this day as MP4",
          text: "⬇ MP4",
        })
      : el("span", { class: "muted small", text: "—" });
    const del = el("button", { class: "danger usage-del", type: "button", text: "Delete" });
    del.addEventListener("click", () => doDeleteDay(d, day));
    const row = el("div", { class: "usage-row" }, [
      el("label", { class: "usage-pick" }, [cb]),
      el("span", { class: "usage-date", text: dayLabel(day.date) }),
      el("span", { class: "usage-num num", text: `${day.segmentCount.toLocaleString()} seg` }),
      el("span", { class: "usage-num num", text: humanBytes(day.bytes) }),
      el("span", { class: "usage-act" }, [exp, del]),
    ]);
    table.appendChild(row);
  }
  body.appendChild(table);
  updateBulk();
}

// ---- mutations ------------------------------------------------------------

async function doRename(d, raw) {
  const name = (raw || "").trim();
  if (!name) {
    toast("Name can't be empty.", { kind: "error" });
    return;
  }
  if (name === d.displayName) return;
  try {
    await renameDevice(d.id, name);
    await reload();
  } catch {
    toast("Couldn't rename that device.", { kind: "error" });
  }
}

async function doRetention(d, raw) {
  const s = (raw || "").trim();
  let days = null;
  if (s !== "") {
    const n = Number(s);
    if (!Number.isInteger(n) || n < 1) {
      toast("Retention must be a whole number of days ≥ 1 (or blank to keep forever).", { kind: "error" });
      return;
    }
    days = n;
  }
  try {
    await setRetention(d.id, days);
    await reload();
  } catch {
    toast("Couldn't update the retention policy.", { kind: "error" });
  }
}

async function doDeleteDay(d, day) {
  const ok = await confirmAction({
    title: "Delete a day of footage",
    message: `Delete ${day.segmentCount.toLocaleString()} segments (${humanBytes(day.bytes)}) from ${dayLabel(
      day.date,
    )} on “${deviceLabel(d)}”? This permanently removes the footage from disk and can't be undone.`,
    confirmLabel: "Delete this day",
  });
  if (!ok) return;
  try {
    await deleteFootageDay(d.id, TZ, day.date);
    (selected.get(d.id) || new Set()).delete(day.date);
    await reload();
  } catch {
    toast("Couldn't delete that day.", { kind: "error" });
  }
}

async function doBulkDelete(d, dayList, allDays) {
  if (!dayList.length) return;
  const picked = allDays.filter((x) => dayList.includes(x.date));
  const seg = picked.reduce((a, x) => a + x.segmentCount, 0);
  const bytes = picked.reduce((a, x) => a + x.bytes, 0);
  const entireHistory = dayList.length === allDays.length;
  const ok = await confirmAction({
    title: entireHistory ? "Delete ALL footage for this device" : "Delete selected days",
    message: `Delete ${dayList.length} day(s) — ${seg.toLocaleString()} segments (${humanBytes(
      bytes,
    )}) — from “${deviceLabel(d)}”? This can't be undone.`,
    confirmLabel: `Delete ${dayList.length} day(s)`,
    requireText: entireHistory ? deviceLabel(d) : null,
  });
  if (!ok) return;
  try {
    await bulkDeleteFootageDays(d.id, TZ, dayList);
    selected.delete(d.id);
    await reload();
  } catch {
    toast("Couldn't delete the selected days.", { kind: "error" });
  }
}

async function doDeleteDevice(d) {
  const ok = await confirmAction({
    title: "Delete device",
    message: `Delete device “${deviceLabel(d)}” and ALL its footage — ${(d.segmentCount || 0).toLocaleString()} segments (${humanBytes(
      d.bytes,
    )})? The device and every recording are permanently removed. This can't be undone.`,
    confirmLabel: "Delete device",
    requireText: deviceLabel(d),
  });
  if (!ok) return;
  try {
    await deleteDevice(d.id);
    expanded.delete(d.id);
    usage.delete(d.id);
    selected.delete(d.id);
    await reload();
  } catch {
    toast("Couldn't delete that device.", { kind: "error" });
  }
}

function doExportSelected(d, dayList, allDays, kind) {
  if (!dayList.length || !kind) return;
  const picked = allDays.filter((x) => dayList.includes(x.date));
  const from = Math.min(...picked.map((x) => x.startMs));
  const to = Math.max(...picked.map((x) => x.endMs));
  triggerDownload(exportUrl(d.id, from, to, kind));
}

// ---- load + poll ----------------------------------------------------------

async function loadUsage(id) {
  try {
    usage.set(id, await getDeviceUsage(id, TZ));
  } catch {
    usage.set(id, []);
    toast("Couldn't load that device's footage breakdown.", { kind: "error" });
  }
  render();
}

async function reload() {
  if (busy) return;
  busy = true;
  try {
    devices = await getManagedDevices();
    everLoaded = true;
    $("mgmtBanner").hidden = true;
    setLive(true);
    setUpdated(`updated ${new Date().toLocaleTimeString()}`);

    const ids = new Set(devices.map((d) => d.id));
    for (const id of [...expanded]) {
      if (!ids.has(id)) {
        expanded.delete(id);
        usage.delete(id);
        selected.delete(id);
        continue;
      }
      try {
        usage.set(id, await getDeviceUsage(id, TZ));
      } catch {
        /* keep stale */
      }
    }
    renderKpis();
    render();
    $("mgmtCount").textContent = `${devices.length} device${devices.length === 1 ? "" : "s"}`;
  } catch (e) {
    setLive(false);
    setUpdated("disconnected");
    // Text-only banner: e.message can echo server output and must never be parsed as HTML.
    renderBanner($("mgmtBanner"), {
      mode: "error",
      message: everLoaded
        ? `Lost connection — retrying every ${REFRESH_MS / 1000}s.`
        : "Couldn't load devices. Make sure hushai-backend is running and the viewer's BACKEND_TOKEN / DEVICE_TOKEN is set, then it recovers automatically.",
      detail: String(e.message || e),
    });
  } finally {
    busy = false;
  }
}

// Background refresh: don't clobber typing / an open confirm dialog (isPaused); user-invoked
// reload()s (mutations, the ⟳ button) still go through directly, like the old force flag did.
const poller = createPoller(reload, {
  intervalMs: REFRESH_MS,
  isPaused: () => isEditing() || confirmOpen(),
});

function start() {
  initTopbar({ section: "files" });
  // Page-specific ⟳ button, re-inserted next to the shared topbar's logout button.
  $("btnLogout")?.before(
    el("button", { id: "mgmtRefresh", class: "ghost", type: "button", title: "Refresh", text: "⟳", onclick: () => reload() }),
  );
  const focus = new URLSearchParams(location.search).get("device");
  if (focus) expanded.add(focus); // auto-open the drill-down for a linked device
  poller.start();
}

start();
