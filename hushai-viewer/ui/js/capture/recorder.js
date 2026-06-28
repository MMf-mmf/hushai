// Camera/mic recorder that cuts ~2s, independently-decodable, keyframe-aligned fMP4 segments.
//
// Strategy: rotate a FRESH MediaRecorder every ~2s. Each run produces a complete fMP4 file
// (ftyp+moov+moof+mdat) whose first frame is an IDR keyframe, because encoders start every
// run on a keyframe. This mirrors what feed_segments.py gets from ffmpeg's HLS-fMP4 muxer
// (one keyframe-aligned segment per cut) and is what clean HLS playback + per-segment vision
// need — a single continuous start(timeslice) recorder would emit chunks that are NOT
// keyframe-aligned, causing choppy playback.
//
// We REQUIRE H.264/AAC in MP4: it is the only format the NVR's HLS remux can play back.
// Firefox (WebM/VP8/Opus only) is intentionally unsupported; we feature-gate and refuse.

import { splitFragment, firstFragmentOffset } from "./mp4.js";

const SEGMENT_MS = 2000;

const VIDEO_MIME_CANDIDATES = [
  'video/mp4;codecs="avc1.42E01E,mp4a.40.2"',
  "video/mp4;codecs=avc1,mp4a",
  "video/mp4",
];
const AUDIO_MIME_CANDIDATES = ['audio/mp4;codecs="mp4a.40.2"', "audio/mp4"];

// First supported MP4 recording MIME for the mode, or null if none (e.g. Firefox).
export function pickMime(audioOnly) {
  if (typeof MediaRecorder === "undefined") return null;
  const cands = audioOnly ? AUDIO_MIME_CANDIDATES : VIDEO_MIME_CANDIDATES;
  for (const m of cands) {
    try {
      if (MediaRecorder.isTypeSupported(m)) return m;
    } catch {
      /* ignore */
    }
  }
  return null;
}

export class Recorder {
  constructor({ stream, audioOnly, onSegment, onError, onStopped }) {
    this.stream = stream;
    this.audioOnly = audioOnly;
    this.onSegment = onSegment;
    this.onError = onError;
    this.onStopped = onStopped;
    this.mime = pickMime(audioOnly);
    this.running = false;
    this.rec = null;
    this.timer = null;
    this.cachedInit = null; // reused only if a later run lacks its own init prefix
  }

  start() {
    if (!this.mime) {
      this.onError?.(
        new Error(
          "This browser can't record H.264/AAC MP4. Use Chrome/Edge 130+ or Safari.",
        ),
      );
      return false;
    }
    this.running = true;
    this._cycle();
    return true;
  }

  _cycle() {
    if (!this.running) return;
    const opts = { mimeType: this.mime, audioBitsPerSecond: 96_000 };
    if (!this.audioOnly) opts.videoBitsPerSecond = 2_000_000;

    let rec;
    try {
      rec = new MediaRecorder(this.stream, opts);
    } catch (e) {
      this.running = false;
      this.onError?.(e);
      return;
    }
    this.rec = rec;

    // Per-cycle closure state — no shared `this.*` that the next cycle could overwrite
    // before this run's async finalize reads it.
    const chunks = [];
    const startWallMs = Date.now();
    const startPerfMs = performance.now();

    rec.ondataavailable = (e) => {
      if (e.data && e.data.size) chunks.push(e.data);
    };
    rec.onerror = (e) =>
      this.onError?.(e?.error || new Error("MediaRecorder error"));
    rec.onstop = () => {
      const durMs = performance.now() - startPerfMs;
      this._finish(chunks, startWallMs, startPerfMs, durMs);
      if (this.running) this._cycle();
      else this.onStopped?.();
    };

    try {
      rec.start(); // no timeslice — one self-contained, keyframe-aligned file per cycle
    } catch (e) {
      this.running = false;
      this.onError?.(e);
      return;
    }
    this.timer = setTimeout(() => {
      try {
        if (rec.state !== "inactive") rec.stop();
      } catch {
        /* ignore */
      }
    }, SEGMENT_MS);
  }

  async _finish(chunks, startWallMs, startPerfMs, durMs) {
    if (!chunks.length) return;
    let u8;
    try {
      const blob = new Blob(chunks, { type: this.mime });
      u8 = new Uint8Array(await blob.arrayBuffer());
    } catch (e) {
      this.onError?.(e);
      return;
    }
    let init, body;
    const split = splitFragment(u8);
    if (split) {
      init = split.init;
      body = split.body;
      this.cachedInit = init;
    } else if (firstFragmentOffset(u8) === 0 && this.cachedInit) {
      // No init prefix in this run (rare): reuse the cached init, ship the whole blob as body.
      init = this.cachedInit;
      body = u8;
    } else {
      // Init-only or unparseable run: skip it. The queue will mark gap_before on the next one.
      this.onError?.(new Error("skipped an unparseable mp4 segment"));
      return;
    }
    this.onSegment?.({
      init,
      body,
      captureStartUnixMs: startWallMs,
      monotonicStartMs: startPerfMs,
      durationMs: durMs,
    });
  }

  stop() {
    this.running = false;
    if (this.timer) {
      clearTimeout(this.timer);
      this.timer = null;
    }
    const rec = this.rec;
    if (rec && rec.state !== "inactive") {
      // onstop fires, flushes the final segment, then calls onStopped (running is false now).
      try {
        rec.stop();
      } catch {
        this.onStopped?.();
      }
    } else {
      this.onStopped?.();
    }
  }
}
