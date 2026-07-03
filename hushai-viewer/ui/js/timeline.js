// The NVR scrub bar, drawn on a canvas. Renders recorded coverage vs gaps over an
// absolute wall-clock window, with adaptive tick labels, session-boundary marks, a
// hover tooltip, and a draggable playhead. Clicking a gap snaps to recorded content.
//
// All times are ms. Two transforms (xOf/tOf) drive everything — nothing is laid out
// per-segment, so a busy day stays cheap (the backend pre-coalesces to spans).

import { stageColors, severityColors, cssVar, prefersReducedMotion } from "./theme.js";

const LADDER = [
  250, 500, 1e3, 2e3, 5e3, 1e4, 15e3, 3e4, 6e4, 12e4, 3e5, 6e5, 9e5, 18e5, 36e5, 72e5, 108e5,
  216e5, 432e5, 864e5,
];

const TRACK_TOP = 30;
const TRACK_H = 46;
const MIN_WINDOW = 5_000; // 5s most-zoomed-in
const HIT_SLOP_TOUCH = 22; // ±px hit tolerance for touch pointers (edge grips, marker hits)
const GRIP_SLOP = 7; // ±px hit tolerance for selection edge grips with a mouse
const PINCH_MIN_DIST = 12; // px floor so pinch scaling can't blow up as the fingers converge
const MS_TOOLTIP_SPAN = 120_000; // under this visible span the tooltip gains millisecond precision
const GAP_HATCH_MAX_SPAN = 30 * 60_000; // hatch gap regions only when zoomed in past 30 minutes

// Events lane: a thin marker strip between the coverage track and the AI ribbons.
// Severity glyphs (◆ critical / ▲ warning / • info) at each event's start, a span
// underline for events wide enough to read as a range, and ×N cluster chips where
// markers would pile up.
const EVENTS_Y0 = TRACK_TOP + TRACK_H + 4; // events lane top (=80)
const EVENTS_H = 12;
const EVENT_BIN_PX = 12; // cluster bin width: >2 events per bin collapse to one ×N chip
const EVENT_HIT_SLOP = 6; // ±px mouse hit tolerance on markers (touch uses HIT_SLOP_TOUCH)
const EVENT_WIDE_PX = 6; // events spanning more than this also draw a 3px underline bar
const EVENT_LOOKBACK_MS = 3600_000; // catch underlines of events that started before the window
const EVENT_TOOLTIP_MAX = 3; // tooltip lists at most this many events, then "+N more"

// AI processing-status ribbons drawn under the events lane (audio lane, then vision).
// Each is a labeled track so the two lanes are always identifiable; a faint base shows the
// lane even where it has no data, and colored intervals + a left label sit on top.
const RIBBON_GAP = 5; // breather between events lane and the first ribbon
const RIBBON_H = 13; // each lane ribbon (tall enough for a left label + easy hover)
const RIBBON_PAD = 4; // gap between the two ribbons
const RIBBON_Y0 = EVENTS_Y0 + EVENTS_H + RIBBON_GAP; // audio ribbon top (=97)
const RIBBON_Y1 = RIBBON_Y0 + RIBBON_H + RIBBON_PAD; // vision ribbon top (=114)
const RIBBON_BOTTOM = RIBBON_Y1 + RIBBON_H; // =127

// Pipeline-stage colors. Deliberately NOT the coverage teal or session amber, so the
// ribbons never read as "coverage". Separable by lightness (+ motion on processing,
// + a chip in the tooltip / popover) for colorblind safety. The palette itself lives
// in the CSS --stage-*/--tl-* tokens (styles.css :root), read once via theme.js.
const STATUS_COLORS = stageColors();
const SEV_COLORS = severityColors();
const SEV_RANK = { info: 0, warning: 1, critical: 2 }; // cluster chips take the worst
const TRACK_BG = cssVar("--tl-track", "#15171c");
const TICK_COLOR = cssVar("--tl-tick", "#79839a");
const COV_HI = cssVar("--tl-cov-hi", "#2ee6d6");
const COV_LO = cssVar("--tl-cov-lo", "#1aa899");
const SESSION_COLOR = cssVar("--tl-session", "rgba(255,174,87,0.85)");
const ACCENT = cssVar("--accent", "#2ee6d6");
const LIVE_COLOR = cssVar("--sev-critical", "#ff7a7a");

const pad = (n) => String(n).padStart(2, "0");

export class Timeline {
  constructor(canvas, { onSeek, onWindowChange, onHover, onSelectionChange, onEventClick } = {}) {
    this.canvas = canvas;
    this.ctx = canvas.getContext("2d");
    this.onSeek = onSeek || (() => {});
    this.onWindowChange = onWindowChange || (() => {});
    this.onHover = onHover || (() => {});
    this.onSelectionChange = onSelectionChange || (() => {});
    this.onEventClick = onEventClick || (() => {});

    this.from = 0;
    this.to = 1;
    this.boundsFrom = 0;
    this.boundsTo = 1;
    this.coverage = [];
    this.sessions = [];
    this.events = []; // [{id,severity,type,subjectLabel,startMs,endMs,...}] sorted by startMs
    this._eventClusters = []; // ×N chip hit areas from the last render: [{x0,x1,fromMs,toMs}]
    this._hoverSlop = EVENT_HIT_SLOP; // widened to HIT_SLOP_TOUCH while a touch pointer hovers
    this.procAudio = []; // [{startMs,endMs,status,lastError,sentences,speakers}]
    this.procVision = []; // [{startMs,endMs,status,lastError,faces,objects}]
    this._procEnabled = true;
    this._hasProcessing = false; // any visible 'processing' interval -> drive the shimmer
    this._animPhase = 0;
    this.playheadMs = null;
    this.hoverX = null;
    this.ghostMs = null; // while scrubbing
    this.liveEdgeMs = null; // newest-footage cap (bar + pulsing dot); null hides it
    this.cssW = 0;
    this.cssH = 0;

    // Range selection, shared by Shift+drag zoom-to-selection and (persistently) by
    // export mode: with `selectMode` on, a plain track-drag creates/adjusts `_sel`
    // via draggable edge grips instead of scrubbing.
    this.selectMode = false;
    this._sel = null; // {fromMs,toMs} or null
    this._selBefore = null; // snapshot for Escape-cancel of an in-flight selection drag
    this._escOnKey = null; // keydown listener armed only while a cancellable drag runs

    this._pointers = new Map(); // active pointerId -> {x,y}; two entries = pinch
    this._drag = null; // {mode:'scrub'|'pan'|'zoomsel'|'selnew'|'seledge'|'pinch', ...}
    this._wire();
    this._resize();
    new ResizeObserver(() => this._resize()).observe(canvas);
  }

  setBounds(fromMs, toMs) {
    this.boundsFrom = fromMs;
    this.boundsTo = toMs;
  }
  setWindow(fromMs, toMs) {
    this.from = fromMs;
    this.to = Math.max(toMs, fromMs + MIN_WINDOW);
    this.render();
  }
  setData({ coverage, sessionBoundariesMs }) {
    this.coverage = coverage || [];
    this.sessions = sessionBoundariesMs || [];
    this.render();
  }
  setPlayhead(ms) {
    this.playheadMs = ms;
    this.render();
  }
  // Event markers for the events lane. Kept sorted by startMs so rendering/hit-testing
  // can binary-search the visible slice — thousands of events stay cheap.
  setEvents(events) {
    this.events = (events || []).slice().sort((a, b) => a.startMs - b.startMs);
    this.render();
  }
  // Whether a pointer gesture (scrub/pan/selection/pinch) is in flight — the app hides
  // the hover preview during gestures without reaching into private state.
  isDragging() {
    return this._drag != null;
  }
  // Newest-footage cap: a 2px bar + small dot at `ms` (null hides it). The dot pulses
  // via tickAnim; under prefers-reduced-motion it sits static.
  setLiveEdge(ms) {
    if (ms === this.liveEdgeMs) return;
    this.liveEdgeMs = ms;
    this.render();
  }
  // Per-lane AI processing-status intervals. Independent of coverage/window/playhead.
  setProcessing({ audio, vision }) {
    this.procAudio = audio || [];
    this.procVision = vision || [];
    this._hasProcessing =
      this.procAudio.some((i) => i.status === "processing") ||
      this.procVision.some((i) => i.status === "processing");
    this.render();
  }
  setProcessingEnabled(on) {
    this._procEnabled = !!on;
    this.render();
  }
  // The per-lane AI status at wall-clock `ms`; a lane is `null` where it's not applicable
  // (e.g. audio-only stretch has no vision lane). Used by the tooltip and the live badge.
  statusAt(ms) {
    const find = (arr) => arr.find((i) => ms >= i.startMs && ms <= i.endMs) || null;
    return { audio: find(this.procAudio), vision: find(this.procVision) };
  }
  // Advance the shimmer + live-edge pulse. Driven by the app rAF loop; only repaints
  // when something is actually animating, so a paused, fully-processed bar stays cheap.
  tickAnim(nowMs) {
    const shimmer = this._procEnabled && this._hasProcessing;
    const livePulse =
      this.liveEdgeMs != null &&
      this.liveEdgeMs >= this.from &&
      this.liveEdgeMs <= this.to &&
      !prefersReducedMotion();
    if (!shimmer && !livePulse) return;
    this._animPhase = (nowMs / 1200) % 1;
    this.render();
  }

  // ---- transforms -----------------------------------------------------------
  xOf(ms) {
    return ((ms - this.from) / (this.to - this.from)) * this.cssW;
  }
  tOf(x) {
    return this.from + (x / this.cssW) * (this.to - this.from);
  }

  // nearest covered ms to `ms` (snap a click in a gap to recorded content)
  snap(ms) {
    if (!this.coverage.length) return ms;
    for (const c of this.coverage) if (ms >= c.startMs && ms <= c.endMs) return ms;
    let best = ms,
      bestD = Infinity;
    for (const c of this.coverage) {
      for (const edge of [c.startMs, c.endMs]) {
        const d = Math.abs(ms - edge);
        if (d < bestD) {
          bestD = d;
          best = edge;
        }
      }
    }
    return best;
  }

  // snap() plus how far it moved — lets the caller tell the user about a gap jump.
  snapInfo(ms) {
    const snapped = this.snap(ms);
    return { ms: snapped, movedMs: snapped - ms };
  }

  isCovered(ms) {
    return this.coverage.some((c) => ms >= c.startMs && ms <= c.endMs);
  }

  // ---- range selection --------------------------------------------------------
  // Programmatic set (does NOT fire onSelectionChange — that callback narrates user
  // gestures; a caller setting it already knows). `setSelection(null)` clears.
  setSelection(fromMs, toMs = null) {
    this._sel =
      fromMs == null || toMs == null
        ? null
        : { fromMs: Math.min(fromMs, toMs), toMs: Math.max(fromMs, toMs) };
    this.render();
  }
  getSelection() {
    return this._sel ? { ...this._sel } : null;
  }
  // Gesture-driven update: keeps fromMs <= toMs and tells the owner.
  _setSelFromDrag(fromMs, toMs) {
    this._sel =
      fromMs == null || toMs == null
        ? null
        : { fromMs: Math.min(fromMs, toMs), toMs: Math.max(fromMs, toMs) };
    this.onSelectionChange(this.getSelection());
  }
  // Which selection edge grip (if any) `x` hits; touch gets the wider target.
  _selEdgeAt(x, slop) {
    if (!this._sel) return null;
    const df = Math.abs(x - this.xOf(this._sel.fromMs));
    const dt = Math.abs(x - this.xOf(this._sel.toMs));
    if (df <= slop && df <= dt) return "from";
    if (dt <= slop) return "to";
    return null;
  }

  // ---- rendering ------------------------------------------------------------
  _resize() {
    const dpr = window.devicePixelRatio || 1;
    const rect = this.canvas.getBoundingClientRect();
    this.cssW = rect.width;
    this.cssH = rect.height;
    this.canvas.width = Math.round(rect.width * dpr);
    this.canvas.height = Math.round(rect.height * dpr);
    this.ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    this.render();
  }

  render() {
    const ctx = this.ctx;
    const W = this.cssW;
    const H = this.cssH;
    if (!W) return;
    ctx.clearRect(0, 0, W, H);

    // track background
    ctx.fillStyle = TRACK_BG;
    roundRect(ctx, 0, TRACK_TOP, W, TRACK_H, 6);
    ctx.fill();

    // Zoomed in, hatch the gaps so "no data here" reads differently from "not loaded yet".
    if (this.to - this.from < GAP_HATCH_MAX_SPAN) this._drawGapHatch();

    // gridlines + tick labels
    const step = this._chooseStep();
    const offset = new Date(this.from).getTimezoneOffset() * 60000;
    let t = Math.ceil((this.from - offset) / step) * step + offset;
    ctx.textBaseline = "alphabetic";
    ctx.font = "11px ui-monospace, SFMono-Regular, Menlo, monospace";
    for (; t <= this.to; t += step) {
      const x = this.xOf(t);
      ctx.strokeStyle = "rgba(255,255,255,0.05)";
      ctx.beginPath();
      ctx.moveTo(x, TRACK_TOP);
      ctx.lineTo(x, TRACK_TOP + TRACK_H);
      ctx.stroke();
      ctx.fillStyle = TICK_COLOR;
      ctx.fillText(this._tickLabel(t, step), x + 4, 16);
    }

    // coverage spans
    for (const c of this.coverage) {
      const x0 = Math.max(0, this.xOf(c.startMs));
      const x1 = Math.min(W, this.xOf(c.endMs));
      const w = Math.max(1, x1 - x0);
      if (x1 < 0 || x0 > W) continue;
      const grad = ctx.createLinearGradient(0, TRACK_TOP, 0, TRACK_TOP + TRACK_H);
      grad.addColorStop(0, COV_HI);
      grad.addColorStop(1, COV_LO);
      ctx.fillStyle = grad;
      ctx.fillRect(x0, TRACK_TOP + 4, w, TRACK_H - 8);
    }

    // session boundary marks
    for (const s of this.sessions) {
      const x = this.xOf(s);
      if (x < 0 || x > W) continue;
      ctx.strokeStyle = SESSION_COLOR;
      ctx.beginPath();
      ctx.moveTo(x, TRACK_TOP);
      ctx.lineTo(x, TRACK_TOP + TRACK_H);
      ctx.stroke();
    }

    // range selection band (persistent) / zoom-to-selection band (while dragging)
    this._drawSelection();

    // events lane (severity markers + cluster chips), between track and ribbons
    this._drawEvents();

    // AI processing-status ribbons (audio lane, then vision lane)
    if (this._procEnabled) {
      this._drawRibbon(this.procAudio, RIBBON_Y0, "Audio");
      this._drawRibbon(this.procVision, RIBBON_Y1, "Vision");
    }

    // live-edge cap (newest footage): 2px bar + pulsing dot
    this._drawLiveEdge();

    // hover hairline + tooltip
    if (this.hoverX != null && this._drag == null) {
      const ms = this.tOf(this.hoverX);
      const hairBottom = this._procEnabled ? RIBBON_BOTTOM : EVENTS_Y0 + EVENTS_H;
      ctx.strokeStyle = "rgba(255,255,255,0.25)";
      ctx.beginPath();
      ctx.moveTo(this.hoverX, TRACK_TOP);
      ctx.lineTo(this.hoverX, hairBottom);
      ctx.stroke();
      this._tooltip(ms, this.hoverX);
    }

    // ghost (while scrubbing) + its tooltip, so the landing time reads mid-drag
    if (this.ghostMs != null) {
      this._playhead(this.ghostMs, "rgba(255,255,255,0.5)");
      if (this._drag?.mode === "scrub") this._tooltip(this.ghostMs, this.xOf(this.ghostMs));
    }
    // playhead
    if (this.playheadMs != null) this._playhead(this.playheadMs, "#ffffff");
  }

  _playhead(ms, color) {
    const x = this.xOf(ms);
    if (x < -2 || x > this.cssW + 2) return;
    const ctx = this.ctx;
    ctx.fillStyle = color;
    ctx.fillRect(x - 1, TRACK_TOP - 4, 2, TRACK_H + 8);
    ctx.beginPath();
    ctx.moveTo(x - 5, TRACK_TOP - 4);
    ctx.lineTo(x + 5, TRACK_TOP - 4);
    ctx.lineTo(x, TRACK_TOP + 4);
    ctx.closePath();
    ctx.fill();
  }

  // Diagonal hatch over the gap x-ranges of the visible window, in the same inset band
  // as the coverage fill. Only drawn once coverage data exists — before the first fetch
  // the whole track IS "not loaded yet", which is exactly what the hatch must not claim.
  _drawGapHatch() {
    if (!this.coverage.length) return;
    const pat = hatchPattern(this.ctx);
    if (!pat) return;
    const ctx = this.ctx;
    ctx.fillStyle = pat;
    const fill = (a, b) => {
      const x0 = Math.max(0, this.xOf(a));
      const x1 = Math.min(this.cssW, this.xOf(b));
      if (x1 > x0) ctx.fillRect(x0, TRACK_TOP + 4, x1 - x0, TRACK_H - 8);
    };
    const spans = this.coverage.slice().sort((a, b) => a.startMs - b.startMs);
    let cur = this.from;
    for (const c of spans) {
      if (c.startMs > cur) fill(cur, Math.min(c.startMs, this.to));
      cur = Math.max(cur, c.endMs);
      if (cur >= this.to) break;
    }
    if (cur < this.to) fill(cur, this.to);
  }

  // Translucent accent band for the persistent range selection (edge grips in
  // selectMode) or the transient Shift+drag zoom-select band — same rendering,
  // different lifetime.
  _drawSelection() {
    const dz = this._drag?.mode === "zoomsel" ? this._drag : null;
    const band = dz
      ? { fromMs: Math.min(dz.startMs, dz.curMs), toMs: Math.max(dz.startMs, dz.curMs) }
      : this._sel;
    if (!band) return;
    const ctx = this.ctx;
    const W = this.cssW;
    const x0 = this.xOf(band.fromMs);
    const x1 = this.xOf(band.toMs);
    if (x1 < 0 || x0 > W) return;
    ctx.save();
    ctx.globalAlpha = 0.16;
    ctx.fillStyle = ACCENT;
    ctx.fillRect(Math.max(0, x0), TRACK_TOP, Math.min(W, x1) - Math.max(0, x0), TRACK_H);
    ctx.restore();
    ctx.strokeStyle = ACCENT;
    for (const x of [x0, x1]) {
      if (x < 0 || x > W) continue;
      ctx.beginPath();
      ctx.moveTo(x, TRACK_TOP);
      ctx.lineTo(x, TRACK_TOP + TRACK_H);
      ctx.stroke();
    }
    // Edge grips (drag handles) — only for the persistent selection in select mode.
    if (!dz && this.selectMode) {
      ctx.fillStyle = ACCENT;
      for (const x of [x0, x1]) {
        if (x < -4 || x > W + 4) continue;
        roundRect(ctx, x - 3, TRACK_TOP + TRACK_H / 2 - 8, 6, 16, 3);
        ctx.fill();
      }
    }
  }

  // 2px newest-footage bar in --sev-critical with a small dot on top. The dot's halo
  // pulses via _animPhase (advanced by tickAnim); static under prefers-reduced-motion.
  _drawLiveEdge() {
    const ms = this.liveEdgeMs;
    if (ms == null) return;
    const x = this.xOf(ms);
    if (x < 0 || x > this.cssW) return;
    const ctx = this.ctx;
    ctx.fillStyle = LIVE_COLOR;
    ctx.fillRect(x - 1, TRACK_TOP, 2, TRACK_H);
    ctx.beginPath();
    ctx.arc(x, TRACK_TOP - 6, 3, 0, Math.PI * 2);
    ctx.fill();
    if (!prefersReducedMotion()) {
      const pulse = 0.5 + 0.5 * Math.sin(this._animPhase * Math.PI * 2);
      ctx.save();
      ctx.globalAlpha = 0.45 * (1 - pulse);
      ctx.beginPath();
      ctx.arc(x, TRACK_TOP - 6, 3 + 4 * pulse, 0, Math.PI * 2);
      ctx.fill();
      ctx.restore();
    }
  }

  // ---- events lane ------------------------------------------------------------

  // First index in the (startMs-sorted) events array with startMs >= ms.
  _eventsLo(ms) {
    let lo = 0,
      hi = this.events.length;
    while (lo < hi) {
      const mid = (lo + hi) >> 1;
      if (this.events[mid].startMs < ms) lo = mid + 1;
      else hi = mid;
    }
    return lo;
  }

  // The events intersecting the visible window, via binary search on startMs. A bounded
  // lookback catches events that STARTED before the window but whose span underline still
  // reaches into it (events longer than the lookback are rare enough to accept missing).
  _visibleEvents() {
    if (!this.events.length) return [];
    const out = [];
    for (let i = this._eventsLo(this.from - EVENT_LOOKBACK_MS); i < this.events.length; i++) {
      const ev = this.events[i];
      if (ev.startMs > this.to) break;
      if (Math.max(ev.startMs, ev.endMs ?? ev.startMs) < this.from) continue;
      out.push(ev);
    }
    return out;
  }

  // Faint lane base + span underlines + severity glyphs, with >2-per-bin pileups
  // collapsed into one ×N chip in the worst severity's color. Rebuilds the chip hit
  // areas (`_eventClusters`) every pass, so click dispatch always matches the pixels.
  _drawEvents() {
    const ctx = this.ctx;
    const W = this.cssW;
    // faint base so the lane is identifiable even where there are no markers
    ctx.fillStyle = "rgba(255,255,255,0.045)";
    ctx.fillRect(0, EVENTS_Y0, W, EVENTS_H);
    this._eventClusters = [];
    if (!this.events.length) return;
    const visible = this._visibleEvents();
    if (!visible.length) return;

    // 1) span underlines for events wide enough to read as a range (drawn under glyphs)
    for (const ev of visible) {
      const x0 = this.xOf(ev.startMs);
      const x1 = this.xOf(ev.endMs ?? ev.startMs);
      if (x1 - x0 <= EVENT_WIDE_PX) continue;
      const a = Math.max(0, x0);
      const b = Math.min(W, x1);
      if (b <= a) continue;
      ctx.save();
      ctx.globalAlpha = 0.55;
      ctx.fillStyle = SEV_COLORS[ev.severity] || SEV_COLORS.info;
      ctx.fillRect(a, EVENTS_Y0 + EVENTS_H - 3, b - a, 3);
      ctx.restore();
    }

    // 2) bucket by 12px bin; ≤2 per bin draw individual glyphs, >2 draw one ×N chip
    const bins = new Map();
    for (const ev of visible) {
      const bin = Math.floor(this.xOf(ev.startMs) / EVENT_BIN_PX);
      const arr = bins.get(bin);
      if (arr) arr.push(ev);
      else bins.set(bin, [ev]);
    }
    const cy = EVENTS_Y0 + EVENTS_H / 2;
    for (const [bin, evs] of bins) {
      const bx = bin * EVENT_BIN_PX;
      if (bx > W || bx + EVENT_BIN_PX < 0) continue;
      if (evs.length > 2) this._drawEventCluster(bx, evs);
      else for (const ev of evs) this._drawEventGlyph(ev, cy);
    }
  }

  // One severity glyph at the event's start: ◆ critical / ▲ warning / • info.
  _drawEventGlyph(ev, cy) {
    const x = this.xOf(ev.startMs);
    if (x < -5 || x > this.cssW + 5) return;
    const ctx = this.ctx;
    ctx.fillStyle = SEV_COLORS[ev.severity] || SEV_COLORS.info;
    ctx.beginPath();
    if (ev.severity === "critical") {
      // filled diamond
      ctx.moveTo(x, cy - 4.5);
      ctx.lineTo(x + 4, cy);
      ctx.lineTo(x, cy + 4.5);
      ctx.lineTo(x - 4, cy);
    } else if (ev.severity === "warning") {
      // filled triangle
      ctx.moveTo(x, cy - 4);
      ctx.lineTo(x + 4.2, cy + 3.5);
      ctx.lineTo(x - 4.2, cy + 3.5);
    } else {
      // 4px dot
      ctx.arc(x, cy, 2, 0, Math.PI * 2);
    }
    ctx.closePath();
    ctx.fill();
  }

  // A rounded ×N chip for a pileup bin, colored by the worst severity in it. Registers
  // its extent in _eventClusters so a click can zoom into the bin instead of guessing.
  _drawEventCluster(bx, evs) {
    const ctx = this.ctx;
    const W = this.cssW;
    const worst = evs.reduce(
      (w, e) => ((SEV_RANK[e.severity] ?? 0) > SEV_RANK[w] ? e.severity : w),
      "info",
    );
    ctx.font = "700 9px system-ui, -apple-system, Segoe UI, Roboto, sans-serif";
    const label = `×${evs.length}`;
    const w = ctx.measureText(label).width + 8;
    // centered on the bin, nudged fully into view at the canvas edges
    const cx = Math.min(Math.max(bx + EVENT_BIN_PX / 2, w / 2 + 1), W - w / 2 - 1);
    const x0 = cx - w / 2;
    ctx.fillStyle = SEV_COLORS[worst] || SEV_COLORS.info;
    roundRect(ctx, x0, EVENTS_Y0 + 0.5, w, EVENTS_H - 1, 5);
    ctx.fill();
    ctx.fillStyle = "#0b0d10";
    ctx.textBaseline = "middle";
    ctx.fillText(label, x0 + 4, EVENTS_Y0 + EVENTS_H / 2 + 0.5);
    ctx.textBaseline = "alphabetic";
    this._eventClusters.push({
      x0,
      x1: x0 + w,
      fromMs: Math.min(...evs.map((e) => e.startMs)),
      toMs: Math.max(...evs.map((e) => Math.max(e.startMs, e.endMs ?? e.startMs))),
    });
  }

  // Events whose marker sits within ±slop of `x`: the glyph at startMs, or — for wide
  // events — anywhere along the span underline. Feeds the tooltip and click dispatch.
  _eventsNear(x, slop) {
    if (!this.events.length) return [];
    const out = [];
    for (const ev of this._visibleEvents()) {
      const x0 = this.xOf(ev.startMs);
      const x1 = this.xOf(ev.endMs ?? ev.startMs);
      const wide = x1 - x0 > EVENT_WIDE_PX;
      if (Math.abs(x - x0) <= slop || (wide && x >= x0 - slop && x <= x1 + slop)) out.push(ev);
    }
    return out;
  }

  // Pointerdown dispatch for the events zone. Returns true when consumed: a ×N cluster
  // chip (or an ambiguous marker pileup) zooms to the bin extent with ×3 padding; a
  // single marker fires onEventClick. False = nothing hit, caller falls back to scrub.
  _eventClickAt(x, slop) {
    const cluster = this._eventClusters.find((c) => x >= c.x0 - slop && x <= c.x1 + slop);
    if (cluster) {
      const span = Math.max(cluster.toMs - cluster.fromMs, 1000);
      this.fit(cluster.fromMs - span, cluster.toMs + span);
      return true;
    }
    const hits = this._eventsNear(x, slop);
    if (!hits.length) return false;
    if (hits.length === 1) {
      this.onEventClick(hits[0]);
      return true;
    }
    const a = Math.min(...hits.map((e) => e.startMs));
    const b = Math.max(...hits.map((e) => Math.max(e.startMs, e.endMs ?? e.startMs)));
    const span = Math.max(b - a, 1000);
    this.fit(a - span, b + span);
    return true;
  }

  // One lane ribbon: a faint base track (so the lane is always visible + identifiable),
  // a colored band per status interval, and a left label chip. `processing` gets an
  // animated shimmer so it reads as active. No interval (lane n/a, or a gap) leaves the
  // faint base showing — distinct from the dim-slate 'pending'.
  _drawRibbon(intervals, y, label) {
    const ctx = this.ctx;
    const W = this.cssW;
    // faint base so an empty / not-applicable lane still reads as a labeled track
    ctx.fillStyle = "rgba(255,255,255,0.045)";
    ctx.fillRect(0, y, W, RIBBON_H);
    for (const iv of intervals) {
      let x0 = this.xOf(iv.startMs);
      let x1 = this.xOf(iv.endMs);
      if (x1 < 0 || x0 > W) continue;
      x0 = Math.max(0, x0);
      x1 = Math.min(W, x1);
      const w = Math.max(1, x1 - x0);
      ctx.fillStyle = STATUS_COLORS[iv.status] || STATUS_COLORS.pending;
      ctx.fillRect(x0, y, w, RIBBON_H);
      if (iv.status === "processing") {
        ctx.save();
        ctx.beginPath();
        ctx.rect(x0, y, w, RIBBON_H);
        ctx.clip();
        const period = 44; // px between marching highlight bands
        const shift = this._animPhase * period;
        for (let bx = x0 - period + shift; bx < x1 + period; bx += period) {
          const g = ctx.createLinearGradient(bx, 0, bx + period, 0);
          g.addColorStop(0, "rgba(255,255,255,0)");
          g.addColorStop(0.5, "rgba(255,255,255,0.34)");
          g.addColorStop(1, "rgba(255,255,255,0)");
          ctx.fillStyle = g;
          ctx.fillRect(bx, y, period, RIBBON_H);
        }
        ctx.restore();
      }
    }
    this._laneLabel(label, y);
  }

  // A small persistent lane label ("AUDIO"/"VISION") pinned to the left of a ribbon, on a
  // translucent chip so it stays legible over whatever color is underneath.
  _laneLabel(label, y) {
    const ctx = this.ctx;
    ctx.font = "600 9px system-ui, -apple-system, Segoe UI, Roboto, sans-serif";
    const txt = label.toUpperCase();
    const w = ctx.measureText(txt).width + 12;
    ctx.fillStyle = "rgba(8,10,14,0.7)";
    roundRect(ctx, 0, y, w, RIBBON_H, 2);
    ctx.fill();
    ctx.fillStyle = "#9aa3b4";
    ctx.textBaseline = "middle";
    ctx.fillText(txt, 6, y + RIBBON_H / 2 + 0.5);
    ctx.textBaseline = "alphabetic";
  }

  _tooltip(ms, x) {
    const ctx = this.ctx;
    const covered = this.isCovered(ms);
    // Millisecond precision once the window is tight enough for it to mean anything.
    const when = this.to - this.from < MS_TOOLTIP_SPAN ? fmtFullMs(ms) : fmtFull(ms);
    const lines = [
      { text: `${when}   ${covered ? "● recorded" : "○ gap"}`, color: covered ? "#cdd6e4" : "#79839a", chip: null },
    ];
    if (this._procEnabled && covered) {
      const st = this.statusAt(ms);
      if (st.audio)
        lines.push({ text: `Audio · ${laneSummaryText("audio", st.audio)}`, color: "#cdd6e4", chip: STATUS_COLORS[st.audio.status] });
      if (st.vision)
        lines.push({ text: `Vision · ${laneSummaryText("vision", st.vision)}`, color: "#cdd6e4", chip: STATUS_COLORS[st.vision.status] });
    }
    // Event markers near the hover x append their own lines (sev-colored chip each).
    // Hover-only — the mid-scrub ghost tooltip stays a pure time readout.
    if (this._drag == null) {
      const hits = this._eventsNear(x, this._hoverSlop);
      for (const ev of hits.slice(0, EVENT_TOOLTIP_MAX)) {
        const text = [ev.type, ev.subjectLabel, fmtFull(ev.startMs)].filter(Boolean).join(" · ");
        lines.push({ text, color: "#cdd6e4", chip: SEV_COLORS[ev.severity] || SEV_COLORS.info });
      }
      if (hits.length > EVENT_TOOLTIP_MAX)
        lines.push({ text: `+${hits.length - EVENT_TOOLTIP_MAX} more events`, color: "#79839a", chip: null });
    }
    ctx.font = "11px ui-monospace, SFMono-Regular, Menlo, monospace";
    const lineH = 16;
    const chipW = 14; // reserved left space when a line carries a status chip
    const w = Math.max(...lines.map((l) => ctx.measureText(l.text).width + (l.chip ? chipW : 0))) + 18;
    const h = 10 + lineH * lines.length;
    const tx = Math.min(Math.max(x - w / 2, 2), this.cssW - w - 2);
    // Sit ABOVE the track so the tooltip never covers the ribbons it describes; clamp into view.
    let ty = TRACK_TOP - 8 - h;
    if (ty < 2) ty = 2;
    ctx.fillStyle = "rgba(10,12,16,0.96)";
    roundRect(ctx, tx, ty, w, h, 5);
    ctx.fill();
    ctx.strokeStyle = "rgba(255,255,255,0.14)";
    ctx.stroke();
    ctx.textBaseline = "middle";
    lines.forEach((l, i) => {
      const cy = ty + 5 + lineH * i + lineH / 2;
      let textX = tx + 9;
      if (l.chip) {
        ctx.fillStyle = l.chip;
        roundRect(ctx, tx + 9, cy - 4, 8, 8, 2);
        ctx.fill();
        textX = tx + 9 + chipW;
      }
      ctx.fillStyle = l.color;
      ctx.fillText(l.text, textX, cy);
    });
    ctx.textBaseline = "alphabetic";
  }

  _chooseStep() {
    const minPx = 92;
    const need = (this.to - this.from) * (minPx / this.cssW);
    return LADDER.find((s) => s >= need) ?? LADDER[LADDER.length - 1];
  }
  _tickLabel(ms, step) {
    const d = new Date(ms);
    return step < 60000
      ? `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`
      : `${pad(d.getHours())}:${pad(d.getMinutes())}`;
  }

  // ---- interaction ----------------------------------------------------------
  _wire() {
    const c = this.canvas;
    c.addEventListener("pointermove", (e) => this._onMove(e));
    c.addEventListener("pointerleave", () => {
      this.hoverX = null;
      this.onHover(null);
      this.render();
    });
    c.addEventListener("pointerdown", (e) => this._onDown(e));
    window.addEventListener("pointerup", (e) => this._onUp(e));
    window.addEventListener("pointercancel", (e) => this._onCancel(e));
    c.addEventListener(
      "wheel",
      (e) => {
        e.preventDefault();
        this._onWheel(e);
      },
      { passive: false },
    );
  }
  _localX(e) {
    return e.clientX - this.canvas.getBoundingClientRect().left;
  }
  // Which horizontal band `y` (canvas-local px) falls in. Pointer handling dispatches on
  // this: the labels pan, the track scrubs, the events lane clicks markers (else scrubs),
  // the ribbons are hover/tooltip-only.
  _zoneAt(y) {
    if (y < TRACK_TOP) return "labels";
    if (y >= RIBBON_Y0) return "ribbons";
    if (y >= EVENTS_Y0) return "events"; // marker lane, incl. the breather below it
    return "track"; // the coverage track, incl. the small breather above the events lane
  }
  _onDown(e) {
    const rect = this.canvas.getBoundingClientRect();
    const x = e.clientX - rect.left;
    const y = e.clientY - rect.top;
    this.canvas.setPointerCapture?.(e.pointerId);
    this._pointers.set(e.pointerId, { x, y });
    // A second finger turns any one-finger gesture into a pinch (nothing is committed).
    if (this._pointers.size === 2) {
      this._startPinch();
      return;
    }
    if (this._pointers.size > 2 || this._drag?.mode === "pinch") return;
    // Middle-button drag pans from anywhere; otherwise the hit zone decides.
    if (e.button === 1) e.preventDefault(); // suppress the browser's middle-click autoscroll
    const zone = e.button === 1 ? "labels" : this._zoneAt(y);
    if (zone === "labels") {
      this._drag = { mode: "pan", startX: x, startFrom: this.from, startTo: this.to };
    } else if (zone === "track" || zone === "events") {
      const raw = this.tOf(x);
      // Event markers win over scrubbing in their lane (plain clicks only — selection
      // and zoom-select gestures keep their meaning even when started over the lane).
      if (zone === "events" && !e.shiftKey && !this.selectMode) {
        const slop = e.pointerType === "touch" ? HIT_SLOP_TOUCH : EVENT_HIT_SLOP;
        if (this._eventClickAt(x, slop)) return;
      }
      if (e.shiftKey && !this.selectMode) {
        // Zoom-to-selection: accent band while dragging, fit() on release, Esc cancels.
        this._drag = { mode: "zoomsel", startMs: raw, curMs: raw };
        this._armEscCancel();
      } else if (this.selectMode) {
        // Select mode: grab an edge grip when hit, else start a fresh selection.
        this._armEscCancel();
        const slop = e.pointerType === "touch" ? HIT_SLOP_TOUCH : GRIP_SLOP;
        const edge = this._selEdgeAt(x, slop);
        if (edge) {
          this._drag = { mode: "seledge", edge };
        } else {
          this._drag = { mode: "selnew", startMs: raw };
          this._setSelFromDrag(raw, raw);
        }
      } else {
        // Plain drag scrubs; Alt bypasses gap-snap for exact-millisecond seeks.
        this._drag = { mode: "scrub", noSnap: e.altKey };
        this.ghostMs = e.altKey ? raw : this.snap(raw);
      }
      this.render();
    }
    // "ribbons" starts no drag — that band only hovers.
  }
  _onMove(e) {
    const x = this._localX(e);
    // Two-finger pinch owns the pointer stream: zoom/pan only, no hover/ghost.
    if (this._drag?.mode === "pinch") {
      if (this._pointers.has(e.pointerId) && this._pointers.size >= 2) this._pinchMove(e);
      return;
    }
    if (this._pointers.has(e.pointerId)) {
      this._pointers.set(e.pointerId, {
        x,
        y: e.clientY - this.canvas.getBoundingClientRect().top,
      });
    }
    this.hoverX = x;
    this._hoverSlop = e.pointerType === "touch" ? HIT_SLOP_TOUCH : EVENT_HIT_SLOP;
    const d = this._drag;
    if (d?.mode === "scrub") {
      this.ghostMs = d.noSnap ? this.tOf(x) : this.snap(this.tOf(x));
    } else if (d?.mode === "pan") {
      const dt = ((x - d.startX) / this.cssW) * (d.startTo - d.startFrom);
      this._panTo(d.startFrom - dt, d.startTo - dt);
      return;
    } else if (d?.mode === "zoomsel") {
      d.curMs = this.tOf(x);
    } else if (d?.mode === "selnew") {
      this._setSelFromDrag(d.startMs, this.tOf(x));
    } else if (d?.mode === "seledge") {
      this._dragSelEdge(this.tOf(x));
    }
    this.onHover(this.tOf(x));
    this.render();
  }
  // Move the grabbed selection edge to `ms`; crossing the other edge hands the grip over.
  _dragSelEdge(ms) {
    const s = this._sel;
    const d = this._drag;
    if (!s || !d) return;
    const anchor = d.edge === "from" ? s.toMs : s.fromMs;
    if (d.edge === "from" && ms > anchor) d.edge = "to";
    else if (d.edge === "to" && ms < anchor) d.edge = "from";
    this._setSelFromDrag(anchor, ms);
  }
  _onUp(e) {
    if (e && this._pointers.has(e.pointerId)) {
      this._pointers.delete(e.pointerId);
      if (this._drag?.mode === "pinch") {
        // A lifted finger ends the pinch. The survivor does NOT become a scrub —
        // it has to start its own pointerdown.
        if (this._pointers.size < 2) this._drag = null;
        this.render();
        return;
      }
    }
    const d = this._drag;
    if (!d) return;
    this._drag = null;
    this._disarmEscCancel();
    if (d.mode === "scrub" && this.ghostMs != null) {
      const ms = this.ghostMs;
      this.ghostMs = null;
      this.onSeek(ms, { exact: !!d.noSnap });
    } else if (d.mode === "zoomsel") {
      const a = Math.min(d.startMs, d.curMs);
      const b = Math.max(d.startMs, d.curMs);
      // Bands under ~4px are accidental shift-clicks — do nothing rather than dive to 5s.
      if (this.xOf(b) - this.xOf(a) >= 4) this.fit(a, b); // fit() enforces MIN_WINDOW
    } else if (d.mode === "selnew" && this._sel) {
      // A no-drag click in select mode clears instead of leaving a zero-width selection.
      if (this.xOf(this._sel.toMs) - this.xOf(this._sel.fromMs) < 3)
        this._setSelFromDrag(null, null);
    }
    this.render();
  }
  // A cancelled pointer (touch handed off to scrolling, capture lost) abandons the
  // gesture: clear the drag + ghost without seeking, restore a mid-drag selection.
  _onCancel(e) {
    if (e) this._pointers.delete(e.pointerId);
    const d = this._drag;
    this._drag = null;
    this.ghostMs = null;
    if (d && (d.mode === "selnew" || d.mode === "seledge")) {
      const prev = this._selBefore;
      this._setSelFromDrag(prev?.fromMs ?? null, prev?.toMs ?? null);
    }
    this._disarmEscCancel();
    this.render();
  }

  // ---- pinch (two pointers) -----------------------------------------------------
  _startPinch() {
    // Entering pinch abandons whatever one-finger gesture was underway.
    const d = this._drag;
    if (d && (d.mode === "selnew" || d.mode === "seledge")) {
      const prev = this._selBefore;
      this._setSelFromDrag(prev?.fromMs ?? null, prev?.toMs ?? null);
    }
    this._disarmEscCancel();
    this._drag = { mode: "pinch" };
    this.ghostMs = null;
    this.hoverX = null;
    this.onHover(null);
    this.render();
  }
  // One pointer of the pair moved: rescale the span about the pinch midpoint's time
  // (the _onWheel anchor math) plus the midpoint's own x-delta pan, in one _panTo.
  _pinchMove(e) {
    const rect = this.canvas.getBoundingClientRect();
    const [idA, idB] = [...this._pointers.keys()];
    const prevA = this._pointers.get(idA);
    const prevB = this._pointers.get(idB);
    const cur = { x: e.clientX - rect.left, y: e.clientY - rect.top };
    const curA = e.pointerId === idA ? cur : prevA;
    const curB = e.pointerId === idB ? cur : prevB;
    const prevDist = Math.max(Math.abs(prevA.x - prevB.x), PINCH_MIN_DIST);
    const curDist = Math.max(Math.abs(curA.x - curB.x), PINCH_MIN_DIST);
    const anchor = this.tOf((prevA.x + prevB.x) / 2); // the time pinned under the midpoint
    const span = this.to - this.from;
    const newSpan = Math.min(Math.max(span * (prevDist / curDist), MIN_WINDOW), this._maxSpan());
    const from = anchor - ((curA.x + curB.x) / 2 / this.cssW) * newSpan;
    this._pointers.set(e.pointerId, cur);
    this._panTo(from, from + newSpan);
  }

  // ---- Escape-cancel for selection / zoom-select drags ----------------------------
  _armEscCancel() {
    if (this._escOnKey) return;
    this._selBefore = this.getSelection();
    this._escOnKey = (ev) => {
      if (ev.key === "Escape") this._cancelDrag();
    };
    window.addEventListener("keydown", this._escOnKey);
  }
  _disarmEscCancel() {
    if (!this._escOnKey) return;
    window.removeEventListener("keydown", this._escOnKey);
    this._escOnKey = null;
    this._selBefore = null;
  }
  _cancelDrag() {
    const d = this._drag;
    if (!d) return;
    this._drag = null;
    this.ghostMs = null;
    if (d.mode === "selnew" || d.mode === "seledge") {
      const prev = this._selBefore;
      this._setSelFromDrag(prev?.fromMs ?? null, prev?.toMs ?? null);
    }
    this._disarmEscCancel();
    this.render();
  }
  _onWheel(e) {
    const x = this._localX(e);
    const anchor = this.tOf(x);
    const span = this.to - this.from;
    const factor = e.deltaY > 0 ? 1.2 : 1 / 1.2; // wheel down = zoom out
    let newSpan = Math.min(Math.max(span * factor, MIN_WINDOW), this._maxSpan());
    const frac = (anchor - this.from) / span;
    let from = anchor - frac * newSpan;
    let to = from + newSpan;
    this._panTo(from, to);
  }
  _maxSpan() {
    return Math.max((this.boundsTo - this.boundsFrom) * 1.1, MIN_WINDOW);
  }
  _panTo(from, to) {
    const span = to - from;
    const padMs = span * 0.05;
    const lo = this.boundsFrom - padMs;
    const hi = this.boundsTo + padMs;
    if (from < lo) {
      from = lo;
      to = lo + span;
    }
    if (to > hi) {
      to = hi;
      from = hi - span;
    }
    this.from = from;
    this.to = to;
    this.render();
    this.onWindowChange(from, to);
  }

  zoom(factor) {
    const mid = (this.from + this.to) / 2;
    const span = Math.min(Math.max((this.to - this.from) * factor, MIN_WINDOW), this._maxSpan());
    this._panTo(mid - span / 2, mid + span / 2);
  }
  fit(fromMs, toMs) {
    this.setWindow(fromMs, toMs);
    this.onWindowChange(this.from, this.to);
  }
}

function fmtFull(ms) {
  const d = new Date(ms);
  return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
}

// HH:MM:SS.mmm — the tooltip's precision form for tight (sub-2-minute) windows.
function fmtFullMs(ms) {
  return `${fmtFull(ms)}.${String(Math.floor(ms % 1000)).padStart(3, "0")}`;
}

// Cached 8x8 diagonal-hatch tile for gap regions ("no data here", vs. the plain dark
// track meaning "not loaded"). Built lazily once from the first context that asks.
let _hatch = null;
function hatchPattern(ctx) {
  if (_hatch) return _hatch;
  const tile = document.createElement("canvas");
  tile.width = tile.height = 8;
  const g = tile.getContext("2d");
  g.strokeStyle = "rgba(255,255,255,0.07)";
  g.lineWidth = 1.5;
  g.beginPath();
  // The main diagonal plus the two corner halves, so the tile repeats seamlessly.
  g.moveTo(0, 8);
  g.lineTo(8, 0);
  g.moveTo(-4, 4);
  g.lineTo(4, -4);
  g.moveTo(4, 12);
  g.lineTo(12, 4);
  g.stroke();
  _hatch = ctx.createPattern(tile, "repeat");
  return _hatch;
}

const plural = (n, w) => `${n} ${w}${n === 1 ? "" : "s"}`;

// One tooltip line's text for a lane's status + output (the colored chip carries the state
// color). `done` shows what it produced; `error` shows the worker's message (truncated);
// `skipped` says why (static scene / silent audio); pending/processing show the stage word.
function laneSummaryText(kind, s) {
  let extra = "";
  if (s.status === "done") {
    extra =
      kind === "audio"
        ? ` — ${plural(s.sentences || 0, "sentence")}, ${plural(s.speakers || 0, "speaker")}`
        : ` — ${plural(s.faces || 0, "face")}, ${plural(s.objects || 0, "object")}`;
  } else if (s.status === "error" && s.lastError) {
    const e = s.lastError.length > 44 ? `${s.lastError.slice(0, 44)}…` : s.lastError;
    extra = ` — ${e}`;
  } else if (s.status === "skipped") {
    extra = kind === "audio" ? " (silent)" : " (static)";
  }
  return `${s.status}${extra}`;
}

function roundRect(ctx, x, y, w, h, r) {
  ctx.beginPath();
  ctx.moveTo(x + r, y);
  ctx.arcTo(x + w, y, x + w, y + h, r);
  ctx.arcTo(x + w, y + h, x, y + h, r);
  ctx.arcTo(x, y + h, x, y, r);
  ctx.arcTo(x, y, x + w, y, r);
  ctx.closePath();
}
