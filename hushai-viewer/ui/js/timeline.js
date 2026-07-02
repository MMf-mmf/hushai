// The NVR scrub bar, drawn on a canvas. Renders recorded coverage vs gaps over an
// absolute wall-clock window, with adaptive tick labels, session-boundary marks, a
// hover tooltip, and a draggable playhead. Clicking a gap snaps to recorded content.
//
// All times are ms. Two transforms (xOf/tOf) drive everything — nothing is laid out
// per-segment, so a busy day stays cheap (the backend pre-coalesces to spans).

import { stageColors, cssVar } from "./theme.js";

const LADDER = [
  1e3, 2e3, 5e3, 1e4, 15e3, 3e4, 6e4, 12e4, 3e5, 6e5, 9e5, 18e5, 36e5, 72e5, 108e5, 216e5, 432e5,
  864e5,
];

const TRACK_TOP = 30;
const TRACK_H = 46;
const MIN_WINDOW = 20_000; // 20s most-zoomed-in

// AI processing-status ribbons drawn under the coverage track (audio lane, then vision).
// Each is a labeled track so the two lanes are always identifiable; a faint base shows the
// lane even where it has no data, and colored intervals + a left label sit on top.
const RIBBON_GAP = 5; // breather between track and the first ribbon
const RIBBON_H = 13; // each lane ribbon (tall enough for a left label + easy hover)
const RIBBON_PAD = 4; // gap between the two ribbons
const RIBBON_Y0 = TRACK_TOP + TRACK_H + RIBBON_GAP; // audio ribbon top (=81)
const RIBBON_Y1 = RIBBON_Y0 + RIBBON_H + RIBBON_PAD; // vision ribbon top (=98)
const RIBBON_BOTTOM = RIBBON_Y1 + RIBBON_H; // =111

// Pipeline-stage colors. Deliberately NOT the coverage teal or session amber, so the
// ribbons never read as "coverage". Separable by lightness (+ motion on processing,
// + a chip in the tooltip / popover) for colorblind safety. The palette itself lives
// in the CSS --stage-*/--tl-* tokens (styles.css :root), read once via theme.js.
const STATUS_COLORS = stageColors();
const TRACK_BG = cssVar("--tl-track", "#15171c");
const TICK_COLOR = cssVar("--tl-tick", "#79839a");
const COV_HI = cssVar("--tl-cov-hi", "#2ee6d6");
const COV_LO = cssVar("--tl-cov-lo", "#1aa899");
const SESSION_COLOR = cssVar("--tl-session", "rgba(255,174,87,0.85)");

const pad = (n) => String(n).padStart(2, "0");

export class Timeline {
  constructor(canvas, { onSeek, onWindowChange, onHover } = {}) {
    this.canvas = canvas;
    this.ctx = canvas.getContext("2d");
    this.onSeek = onSeek || (() => {});
    this.onWindowChange = onWindowChange || (() => {});
    this.onHover = onHover || (() => {});

    this.from = 0;
    this.to = 1;
    this.boundsFrom = 0;
    this.boundsTo = 1;
    this.coverage = [];
    this.sessions = [];
    this.procAudio = []; // [{startMs,endMs,status,lastError,sentences,speakers}]
    this.procVision = []; // [{startMs,endMs,status,lastError,faces,objects}]
    this._procEnabled = true;
    this._hasProcessing = false; // any visible 'processing' interval -> drive the shimmer
    this._animPhase = 0;
    this.playheadMs = null;
    this.hoverX = null;
    this.ghostMs = null; // while scrubbing
    this.cssW = 0;
    this.cssH = 0;

    this._drag = null; // {mode:'scrub'|'pan', startX, startFrom, startTo}
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
  // Advance the shimmer. Driven by the app rAF loop; only repaints when something is
  // actually processing, so a paused, fully-processed bar stays cheap.
  tickAnim(nowMs) {
    if (!this._procEnabled || !this._hasProcessing) return;
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

  isCovered(ms) {
    return this.coverage.some((c) => ms >= c.startMs && ms <= c.endMs);
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

    // AI processing-status ribbons (audio lane, then vision lane)
    if (this._procEnabled) {
      this._drawRibbon(this.procAudio, RIBBON_Y0, "Audio");
      this._drawRibbon(this.procVision, RIBBON_Y1, "Vision");
    }

    // hover hairline + tooltip
    if (this.hoverX != null && this._drag == null) {
      const ms = this.tOf(this.hoverX);
      const hairBottom = this._procEnabled ? RIBBON_BOTTOM : TRACK_TOP + TRACK_H;
      ctx.strokeStyle = "rgba(255,255,255,0.25)";
      ctx.beginPath();
      ctx.moveTo(this.hoverX, TRACK_TOP);
      ctx.lineTo(this.hoverX, hairBottom);
      ctx.stroke();
      this._tooltip(ms, this.hoverX);
    }

    // ghost (while scrubbing)
    if (this.ghostMs != null) this._playhead(this.ghostMs, "rgba(255,255,255,0.5)");
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
    const lines = [
      { text: `${fmtFull(ms)}   ${covered ? "● recorded" : "○ gap"}`, color: covered ? "#cdd6e4" : "#79839a", chip: null },
    ];
    if (this._procEnabled && covered) {
      const st = this.statusAt(ms);
      if (st.audio)
        lines.push({ text: `Audio · ${laneSummaryText("audio", st.audio)}`, color: "#cdd6e4", chip: STATUS_COLORS[st.audio.status] });
      if (st.vision)
        lines.push({ text: `Vision · ${laneSummaryText("vision", st.vision)}`, color: "#cdd6e4", chip: STATUS_COLORS[st.vision.status] });
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
  _onDown(e) {
    const x = this._localX(e);
    const y = e.clientY - this.canvas.getBoundingClientRect().top;
    this.canvas.setPointerCapture?.(e.pointerId);
    if (y < TRACK_TOP) {
      this._drag = { mode: "pan", startX: x, startFrom: this.from, startTo: this.to };
    } else {
      this._drag = { mode: "scrub" };
      this.ghostMs = this.snap(this.tOf(x));
      this.render();
    }
  }
  _onMove(e) {
    const x = this._localX(e);
    this.hoverX = x;
    if (this._drag?.mode === "scrub") {
      this.ghostMs = this.snap(this.tOf(x));
    } else if (this._drag?.mode === "pan") {
      const dt = ((x - this._drag.startX) / this.cssW) * (this._drag.startTo - this._drag.startFrom);
      this._panTo(this._drag.startFrom - dt, this._drag.startTo - dt);
      return;
    }
    this.onHover(this.tOf(x));
    this.render();
  }
  _onUp() {
    if (this._drag?.mode === "scrub" && this.ghostMs != null) {
      const ms = this.ghostMs;
      this.ghostMs = null;
      this._drag = null;
      this.onSeek(ms);
      this.render();
    } else {
      this._drag = null;
    }
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
