// Detection overlay: draws AI bounding boxes + labels over the playing video, synced
// to wall-clock playback. People show their name (or "Unidentified"); objects show
// their class label; plates show the license-plate string. Detections are sampled
// ~3 frames per ~2s segment (so boxes update
// ~1.5x/sec), so we snap the overlay to the nearest sampled frame for the current time
// and clear it when no sample is within tolerance.
//
// bbox coords are ORIGINAL-frame pixels == the video's intrinsic resolution (the viewer
// remux is `-c copy`, no rescale), so we scale them against video.videoWidth/videoHeight,
// accounting for the `#video { object-fit: contain }` letterbox. Mirrors timeline.js for
// the DPR/resize/roundRect canvas idioms and player.js for the binary-search shape.

import { getDetections } from "./api.js";

const SNAP_TOLERANCE_MS = 400; // ~half the ~660ms inter-sample interval; stale boxes clear
const MIN_SCORE = 0.3; // hide low-confidence boxes
const UNIDENTIFIED = "Unidentified";

export class Detections {
  constructor(canvas, video) {
    this.canvas = canvas;
    this.video = video;
    this.ctx = canvas.getContext("2d");
    this.onError = null;

    this.active = false;
    this.deviceId = null;
    this.fromMs = 0;
    this.toMs = 0;
    this.groups = []; // [{tMs, boxes:[{kind,label,personId,bbox,score}]}] ascending by tMs
    this.groupTimes = []; // parallel tMs array for binary search
    this.truncated = false;

    this._reqSeq = 0;
    this._lastGroup = undefined; // redraw-diff: skip repaint when nothing changed
    this._lastRectKey = "";
    this.cssW = 0;
    this.cssH = 0;

    this._resize();
    new ResizeObserver(() => this._resize()).observe(canvas);
  }

  // Show/hide the overlay. The same video keeps playing in both modes.
  setActive(on) {
    this.active = on;
    if (on) {
      this.canvas.classList.add("show");
      this.canvas.setAttribute("aria-hidden", "false");
      this._resize(); // canvas was display:none (0x0) -> pick up real size now
      this._forceRedraw();
    } else {
      this.canvas.classList.remove("show");
      this.canvas.setAttribute("aria-hidden", "true");
      this._clear();
    }
  }

  // Fetch + index detections for a device/window. No-op for the already-loaded range.
  async setWindow(deviceId, fromMs, toMs) {
    if (
      deviceId === this.deviceId &&
      fromMs === this.fromMs &&
      toMs === this.toMs &&
      this.groups.length
    )
      return;
    this.deviceId = deviceId;
    this.fromMs = fromMs;
    this.toMs = toMs;
    const seq = ++this._reqSeq;
    try {
      const data = await getDetections(deviceId, fromMs, toMs);
      if (seq !== this._reqSeq) return; // superseded by a newer device/window
      this._index(data.frames);
      this.truncated = data.truncated;
      this._forceRedraw();
    } catch (e) {
      if (seq !== this._reqSeq) return;
      this._index([]);
      if (this.onError) this.onError(e);
    }
  }

  _index(frames) {
    this.groups = frames || [];
    this.groupTimes = this.groups.map((g) => g.tMs);
    this._forceRedraw();
  }

  // Per-rAF hook from the app ticker. `ms` = current wall-clock. Cheap when idle:
  // only repaints when the snapped frame-group or the canvas geometry changes.
  onTick(ms) {
    if (!this.active) return;
    const g = this._nearestGroup(ms);
    const r = this._videoRect();
    const rectKey = r ? `${r.scale.toFixed(4)}|${r.offX.toFixed(1)}|${r.offY.toFixed(1)}` : "0";
    if (g === this._lastGroup && rectKey === this._lastRectKey) return;
    this._lastGroup = g;
    this._lastRectKey = rectKey;
    this._draw(g, r);
  }

  // Nearest sampled frame to `ms` within tolerance (binary search; mirrors player._fragAt).
  _nearestGroup(ms) {
    const t = this.groupTimes;
    if (!t.length) return null;
    let lo = 0,
      hi = t.length - 1;
    while (lo < hi) {
      const mid = (lo + hi) >> 1;
      if (t[mid] < ms) lo = mid + 1;
      else hi = mid;
    }
    let i = lo; // first index with t[i] >= ms
    if (i > 0 && Math.abs(t[i - 1] - ms) <= Math.abs(t[i] - ms)) i -= 1;
    return Math.abs(t[i] - ms) <= SNAP_TOLERANCE_MS ? this.groups[i] : null;
  }

  // Map original-frame pixels -> canvas CSS coords for `object-fit: contain`.
  _videoRect() {
    const vw = this.video.videoWidth,
      vh = this.video.videoHeight;
    if (!vw || !vh || !this.cssW || !this.cssH) return null; // metadata not loaded yet
    const scale = Math.min(this.cssW / vw, this.cssH / vh);
    return { scale, offX: (this.cssW - vw * scale) / 2, offY: (this.cssH - vh * scale) / 2 };
  }

  _draw(group, rect) {
    const ctx = this.ctx;
    ctx.clearRect(0, 0, this.cssW, this.cssH);
    if (!group || !rect) return;
    ctx.font = "600 12px system-ui, -apple-system, Segoe UI, Roboto, sans-serif";
    ctx.textBaseline = "alphabetic";
    let drawn = 0;
    for (const b of group.boxes) {
      if (b.score != null && b.score < MIN_SCORE) continue;
      const [x, y, w, h] = b.bbox;
      const cx = rect.offX + x * rect.scale;
      const cy = rect.offY + y * rect.scale;
      const cw = w * rect.scale;
      const ch = h * rect.scale;
      const color = colorFor(b);
      ctx.lineWidth = 2;
      ctx.strokeStyle = color;
      ctx.strokeRect(cx, cy, cw, ch);
      this._label(labelFor(b), cx, cy, color);
      drawn++;
    }
    if (this.truncated) {
      ctx.fillStyle = "rgba(255,174,87,0.9)";
      ctx.fillText(`${drawn} shown · detections truncated`, 8, this.cssH - 8);
    }
  }

  // A filled label chip with dark text, clamped inside the canvas, flipped below the
  // box top when it would clip off the top edge.
  _label(text, x, y, color) {
    const ctx = this.ctx;
    const padX = 5,
      hChip = 17;
    const w = ctx.measureText(text).width + padX * 2;
    let ty = y - hChip;
    if (ty < 0) ty = y; // flip inside the box
    const tx = Math.min(Math.max(x, 0), Math.max(0, this.cssW - w));
    roundRect(ctx, tx, ty, w, hChip, 3);
    ctx.fillStyle = color;
    ctx.fill();
    ctx.fillStyle = "#04120f";
    ctx.fillText(text, tx + padX, ty + 12);
  }

  _clear() {
    this.ctx.clearRect(0, 0, this.cssW, this.cssH);
  }
  _forceRedraw() {
    this._lastGroup = undefined;
    this._lastRectKey = "";
  }

  _resize() {
    const dpr = window.devicePixelRatio || 1;
    const rect = this.canvas.getBoundingClientRect();
    this.cssW = rect.width;
    this.cssH = rect.height;
    this.canvas.width = Math.round(rect.width * dpr);
    this.canvas.height = Math.round(rect.height * dpr);
    this.ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    this._forceRedraw(); // geometry changed -> repaint on next tick
  }
}

function labelFor(b) {
  if (b.kind === "person") return b.label && b.label.trim() ? b.label.trim() : UNIDENTIFIED;
  if (b.kind === "plate") return b.label && b.label.trim() ? b.label.trim() : "plate";
  return b.label && b.label.trim() ? b.label.trim() : "object";
}

// People: stable hashed hue when identified, amber when not. Objects: accent teal
// (the class label already disambiguates them). Plates: a distinct warm amber so a license
// plate reads apart from faces/objects at a glance. Dark chip text reads on all of them.
function colorFor(b) {
  if (b.kind === "object") return "#2ee6d6";
  if (b.kind === "plate") return "#ffd24a";
  if (b.personId) return `hsl(${hueFromId(b.personId)} 75% 58%)`;
  return "#ffae57";
}

function hueFromId(id) {
  let h = 0;
  for (let i = 0; i < id.length; i++) h = (h * 31 + id.charCodeAt(i)) >>> 0;
  return h % 360;
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
