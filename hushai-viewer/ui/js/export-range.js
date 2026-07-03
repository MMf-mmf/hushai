// The in-player clip-export bar: a floating panel above the scrub bar (markup skeleton
// in index.html #exportBar), visible while export mode is on. Shows a live range +
// duration readout for the timeline's selection, a source picker (muxed/video, only
// when the device has both), inline warnings (gaps in coverage / over the server's 6h
// clamp), and a plain <a download> whose href tracks api.js exportUrl() — the session
// cookie authorizes the streamed MP4, so no fetch-to-blob dance is needed.
//
// Coupling is deliberately thin (the events-overlay pattern): app.js owns the state
// (export mode, the selection via timeline.js) and pushes changes in through setRange /
// setVisible; this module only renders and builds the download href.

import { exportUrl } from "./api.js";
import { clock, humanDur } from "./time.js";

// Mirrors the backend clamp (VIEWER_MAX_WINDOW_NANOS default; see src/export.rs) and
// WINDOW_MS in app.js. Longer selections still export — the server trims to this.
const MAX_EXPORT_MS = 6 * 3600 * 1000;

const KIND_LABELS = { muxed: "video + audio", video: "video only" };

// "14:02:10" -> "140210" for the suggested download filename.
const hms = (ms) => clock(ms).replaceAll(":", "");

/** Wire the export bar. `getDevice()` returns the selected device (id + hasMuxed /
 *  hasVideo caps), `isCovered(ms)` is timeline.isCovered (for the gap warning),
 *  `onExit()` leaves export mode (Cancel, or after a download click). Returns
 *  { setRange(fromMs, toMs|null), setVisible(on) } — app.js pushes the timeline's
 *  selection through setRange and toggles the bar with export mode. */
export function initExportBar({ getDevice, isCovered, onExit }) {
  const bar = document.getElementById("exportBar");
  const rangeEl = document.getElementById("exportRange");
  const kindWrap = document.getElementById("exportKindWrap");
  const kindSel = document.getElementById("exportKind");
  const warnEl = document.getElementById("exportWarn");
  const dlLink = document.getElementById("exportDownload");
  const cancelBtn = document.getElementById("exportCancel");
  if (!bar || !rangeEl || !kindSel || !warnEl || !dlLink) return null;

  let range = null; // {fromMs,toMs} or null — pushed by app.js from the timeline selection
  let kind = "muxed";

  // The export kinds this device can serve. Muxed (video+audio) first — it's the
  // backend default and what a user downloading "the clip" almost always wants.
  function deviceKinds() {
    const d = getDevice();
    const kinds = [];
    if (d?.hasMuxed) kinds.push("muxed");
    if (d?.hasVideo) kinds.push("video");
    return kinds.length ? kinds : ["muxed"]; // no caps (audio-only): backend default
  }

  // Rebuild the source picker from the current device's caps; hidden when there is no
  // real choice. Keeps the previous pick when the new device still offers it.
  function refreshKinds() {
    const kinds = deviceKinds();
    if (!kinds.includes(kind)) kind = kinds[0];
    kindSel.replaceChildren(
      ...kinds.map((k) => {
        const opt = document.createElement("option");
        opt.value = k;
        opt.textContent = KIND_LABELS[k] || k;
        opt.selected = k === kind;
        return opt;
      }),
    );
    if (kindWrap) kindWrap.hidden = kinds.length < 2;
  }

  // Muted inline warnings: a selection spanning gaps still exports (the MP4 just jumps
  // across them), and an over-6h selection is trimmed server-side — say so up front.
  function warnings() {
    if (!range) return "";
    const out = [];
    const mid = (range.fromMs + range.toMs) / 2;
    const holed = [range.fromMs, mid, range.toMs].some((ms) => !isCovered(ms));
    if (holed) out.push("selection has gaps — export will jump");
    if (range.toMs - range.fromMs > MAX_EXPORT_MS) out.push("over 6h — server will trim");
    return out.join(" · ");
  }

  function render() {
    const d = getDevice();
    const ready = !!(range && d && range.toMs > range.fromMs);
    rangeEl.textContent = ready
      ? `${clock(range.fromMs)} – ${clock(range.toMs)} · ${humanDur(range.toMs - range.fromMs)}`
      : "drag on the timeline to select a range";
    rangeEl.classList.toggle("muted", !ready);

    const warn = ready ? warnings() : "";
    warnEl.textContent = warn;
    warnEl.hidden = !warn;

    // The anchor is the download itself: keep its href tracking the selection, and
    // visually disable it (no href = not a link) while there is nothing to export.
    if (ready) {
      dlLink.href = exportUrl(d.id, range.fromMs, range.toMs, kind);
      dlLink.setAttribute("download", `${d.id}-${hms(range.fromMs)}-${hms(range.toMs)}.mp4`);
      dlLink.removeAttribute("aria-disabled");
      dlLink.classList.remove("disabled");
    } else {
      dlLink.removeAttribute("href");
      dlLink.setAttribute("aria-disabled", "true");
      dlLink.classList.add("disabled");
    }
  }

  kindSel.onchange = () => {
    kind = kindSel.value;
    render();
  };
  dlLink.addEventListener("click", (e) => {
    if (!dlLink.hasAttribute("href")) {
      e.preventDefault();
      return;
    }
    // The download is on its way — leave export mode behind it. Deferred: exiting
    // clears the selection, which strips this anchor's href, and the browser only
    // reads the href AFTER dispatch when it starts the download.
    setTimeout(onExit, 0);
  });
  if (cancelBtn) cancelBtn.onclick = () => onExit();

  return {
    /** The timeline selection changed: (fromMs, toMs) or (null) when cleared. */
    setRange(fromMs, toMs = null) {
      range = fromMs == null || toMs == null ? null : { fromMs, toMs };
      render();
    },
    /** Show/hide with export mode. Showing re-reads the device's kind caps. */
    setVisible(on) {
      if (on) {
        refreshKinds();
        render();
      }
      bar.hidden = !on;
    },
  };
}
