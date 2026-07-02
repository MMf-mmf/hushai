// Wraps the <video> element + an hls.js instance and exposes a WALL-CLOCK API
// (seek/read by absolute time), hiding HLS media-time entirely. Relies on the
// backend stamping every continuous run with #EXT-X-PROGRAM-DATE-TIME, so each
// fragment carries an absolute `programDateTime` (ms) and a continuous `start`
// (media seconds) that hls.js has already adjusted across discontinuities.

const Hls = window.Hls;

const HLS_CONFIG = {
  enableWorker: true,
  lowLatencyMode: false,
  backBufferLength: 90,
  maxBufferLength: 30,
  maxMaxBufferLength: 120,
  maxBufferHole: 0.5,
  startPosition: -1,
  fragLoadingMaxRetry: 4,
  manifestLoadingMaxRetry: 4,
  levelLoadingMaxRetry: 4,
};

export class Player {
  constructor(videoEl, { onError, onNotice } = {}) {
    this.video = videoEl;
    this.onError = onError || (() => {});
    this.onNotice = onNotice || (() => {});
    this.hls = null;
    this.fragments = []; // {programDateTime (ms), start (s), duration (s)}
    this.pendingSeekMs = null;
    this.supported = !!(Hls && Hls.isSupported());
    this.nativeHls = !this.supported && videoEl.canPlayType("application/vnd.apple.mpegurl");
    this._nativeNoticed = false; // reduced-accuracy notice shown at most once per instance
    this._frameDur = null; // measured seconds-per-frame (rVFC deltas); null until known
    this._measuring = false; // a frame-duration measurement is in flight
    this._gen = 0; // load generation — lets stale async callbacks self-discard
  }

  // Load a new master playlist (a device+window). Optionally seek once ready.
  load(src, { seekMs = null, autoplay = true } = {}) {
    this.destroy();
    this.fragments = [];
    this.pendingSeekMs = seekMs;
    this._autoplay = autoplay;
    this._gen++;
    this._frameDur = null; // a new source may have a different frame rate — re-measure
    this._measuring = false;

    if (this.supported) {
      const hls = new Hls(HLS_CONFIG);
      this.hls = hls;
      hls.attachMedia(this.video);
      hls.on(Hls.Events.MEDIA_ATTACHED, () => hls.loadSource(src));
      hls.on(Hls.Events.MANIFEST_PARSED, () => this._onReady());
      hls.on(Hls.Events.LEVEL_LOADED, (_e, data) => this._captureFragments(data));
      hls.on(Hls.Events.LEVEL_UPDATED, (_e, data) => this._captureFragments(data));
      hls.on(Hls.Events.ERROR, (_e, data) => this._onHlsError(hls, data));
    } else if (this.nativeHls) {
      if (!this._nativeNoticed) {
        this._nativeNoticed = true;
        this.onNotice("Native HLS playback: reduced seek accuracy");
      }
      this.video.src = src;
      this.video.addEventListener("loadedmetadata", () => this._onReady(), { once: true });
    } else {
      this.onError("This browser cannot play HLS (no MSE and no native support).");
    }
  }

  _captureFragments(data) {
    const frags = data?.details?.fragments;
    if (frags && frags.length) {
      this.fragments = frags.map((f) => ({
        pdt: f.programDateTime,
        start: f.start,
        duration: f.duration,
      }));
      if (this.pendingSeekMs != null) {
        const ms = this.pendingSeekMs;
        this.pendingSeekMs = null;
        this.seekToWallClock(ms);
      }
    }
  }

  _onReady() {
    if (this.pendingSeekMs != null && (this.fragments.length || this.nativeHls)) {
      const ms = this.pendingSeekMs;
      this.pendingSeekMs = null;
      this.seekToWallClock(ms);
    }
    if (this._autoplay) this.video.play().catch(() => {});
    this._maybeMeasureFrameDur();
  }

  // ---- the core: absolute wall-clock time -> media currentTime --------------

  seekToWallClock(targetMs) {
    if (!Number.isFinite(targetMs)) return;
    if (this.nativeHls) {
      this._seekNativeWallClock(targetMs);
      return;
    }
    if (!this.fragments.length) {
      this.pendingSeekMs = targetMs; // applied on next LEVEL_LOADED
      return;
    }
    let f = this._fragAt(targetMs);
    if (f && f.duration <= 0.05) f = null; // degenerate sliver — land on a real neighbor instead
    if (!f) f = this._nearestFrag(targetMs);
    if (!f) return;
    // Seek a safe offset into the fragment; on very short fragments the clamp math
    // collapses (or goes negative), so land exactly on the fragment start instead.
    const t =
      f.duration > 0.1
        ? f.start + Math.min(Math.max((targetMs - f.pdt) / 1000, 0), f.duration - 0.05)
        : f.start;
    try {
      this.video.currentTime = t;
    } catch {
      /* not seekable yet */
      this.pendingSeekMs = targetMs;
    }
  }

  // Native-HLS (Safari) wall-clock seek: there is no hls.js fragment list, so map through
  // the playlist's absolute start (video.getStartDate() reads #EXT-X-PROGRAM-DATE-TIME)
  // and clamp into the seekable range. Coarser than the fragment path — see onNotice.
  _seekNativeWallClock(targetMs) {
    const start = typeof this.video.getStartDate === "function" ? this.video.getStartDate() : null;
    if (!start || isNaN(start.getTime())) {
      this.pendingSeekMs = targetMs; // metadata not parsed yet — applied on loadedmetadata
      return;
    }
    let t = (targetMs - start.getTime()) / 1000;
    const s = this.video.seekable;
    if (s.length) t = Math.min(Math.max(t, s.start(0)), s.end(s.length - 1));
    try {
      this.video.currentTime = t;
    } catch {
      this.pendingSeekMs = targetMs;
    }
  }

  // Absolute wall-clock (ms) of the frame currently on screen.
  currentWallClockMs() {
    if (this.hls && this.hls.playingDate) return this.hls.playingDate.getTime();
    if (this.nativeHls && typeof this.video.getStartDate === "function") {
      const start = this.video.getStartDate();
      if (start && !isNaN(start.getTime())) return start.getTime() + this.video.currentTime * 1000;
    }
    return this._mediaToWall(this.video.currentTime);
  }

  _fragAt(targetMs) {
    // binary search for the fragment whose [pdt, pdt+dur) contains targetMs
    const a = this.fragments;
    let lo = 0,
      hi = a.length - 1,
      ans = null;
    while (lo <= hi) {
      const mid = (lo + hi) >> 1;
      const f = a[mid];
      if (targetMs < f.pdt) hi = mid - 1;
      else if (targetMs >= f.pdt + f.duration * 1000) lo = mid + 1;
      else {
        ans = f;
        break;
      }
    }
    return ans;
  }

  _nearestFrag(targetMs) {
    let best = null,
      bestD = Infinity;
    for (const f of this.fragments) {
      if (f.duration <= 0.05) continue; // never land on a degenerate sliver
      const d = Math.min(Math.abs(targetMs - f.pdt), Math.abs(targetMs - (f.pdt + f.duration * 1000)));
      if (d < bestD) {
        bestD = d;
        best = f;
      }
    }
    return best;
  }

  _mediaToWall(t) {
    // inverse mapping via the fragment list (fallback when playingDate is null).
    // Fragments are sorted ascending by `start`; binary-search the LAST one with
    // f.start <= t so a discontinuity overlap resolves to the fragment actually playing.
    const a = this.fragments;
    if (!a.length) return 0;
    let lo = 0,
      hi = a.length - 1,
      ans = -1;
    while (lo <= hi) {
      const mid = (lo + hi) >> 1;
      if (a[mid].start <= t) {
        ans = mid;
        lo = mid + 1;
      } else hi = mid - 1;
    }
    // t precedes every fragment (media time need not start at 0) — extrapolate off the first.
    if (ans < 0) return a[0].pdt + (t - a[0].start) * 1000;
    const f = a[ans];
    return Math.min(f.pdt + (t - f.start) * 1000, f.pdt + f.duration * 1000);
  }

  // The wall-clock extent currently buffered/loaded. `fromMs`/`toMs` is the outer envelope
  // (first..last fragment); `ranges` are the merged contiguous PDT runs (fragments within
  // 1s of each other coalesce), so callers can tell loaded footage from a gap inside it.
  loadedRangeMs() {
    if (this.nativeHls) return this._nativeLoadedRange();
    if (!this.fragments.length) return null;
    const ranges = [];
    for (const f of this.fragments) {
      const end = f.pdt + f.duration * 1000;
      const prev = ranges[ranges.length - 1];
      if (prev && f.pdt - prev.toMs <= 1000) prev.toMs = Math.max(prev.toMs, end);
      else ranges.push({ fromMs: f.pdt, toMs: end });
    }
    return { fromMs: ranges[0].fromMs, toMs: ranges[ranges.length - 1].toMs, ranges };
  }

  // True when wall-clock `ms` falls inside a loaded (merged) range — not merely inside
  // the envelope, which can straddle unloaded gaps.
  isLoadedAt(ms) {
    const r = this.loadedRangeMs();
    return !!r && r.ranges.some((x) => ms >= x.fromMs && ms <= x.toMs);
  }

  // Native-HLS twin of loadedRangeMs: no fragment list, so derive the ranges from
  // video.seekable anchored at getStartDate().
  _nativeLoadedRange() {
    const start = typeof this.video.getStartDate === "function" ? this.video.getStartDate() : null;
    if (!start || isNaN(start.getTime())) return null;
    const base = start.getTime();
    const s = this.video.seekable;
    if (!s.length) return null;
    const ranges = [];
    for (let i = 0; i < s.length; i++) {
      ranges.push({ fromMs: base + s.start(i) * 1000, toMs: base + s.end(i) * 1000 });
    }
    return { fromMs: ranges[0].fromMs, toMs: ranges[ranges.length - 1].toMs, ranges };
  }

  // ---- frame stepping --------------------------------------------------------

  // Pause and nudge currentTime by exactly one frame (`dir` = ±1). The frame duration
  // comes from measured requestVideoFrameCallback mediaTime deltas (cached per load);
  // 1/30 until a measurement lands, or on browsers without rVFC.
  stepFrame(dir) {
    this.video.pause();
    this._maybeMeasureFrameDur(); // no-op once measured; arms early for the next play
    const dur = this._frameDur ?? 1 / 30;
    let t = this.video.currentTime + dir * dur;
    const s = this.video.seekable;
    if (s.length) t = Math.min(Math.max(t, s.start(0)), s.end(s.length - 1));
    try {
      this.video.currentTime = t;
    } catch {
      /* not seekable yet */
    }
  }

  // One-shot: watch two consecutively presented frames and cache their mediaTime delta.
  // Frames only present during playback, so this is kicked on ready/play and simply
  // waits until they do; load() bumps _gen so a callback from a torn-down source
  // can't poison the new one.
  _maybeMeasureFrameDur() {
    const v = this.video;
    if (this._frameDur != null || this._measuring) return;
    if (typeof v.requestVideoFrameCallback !== "function") return;
    this._measuring = true;
    const gen = this._gen;
    let last = null;
    const cb = (_now, meta) => {
      if (gen !== this._gen) return; // superseded by a newer load()
      const d = last != null ? meta.mediaTime - last : null;
      // Accept only plausible frame durations (8.3ms..100ms): seeks and dropped
      // frames produce outlier deltas that must not stick.
      if (d != null && d > 1 / 120 && d < 1 / 10) {
        this._frameDur = d;
        this._measuring = false;
        return;
      }
      last = meta.mediaTime;
      v.requestVideoFrameCallback(cb);
    };
    v.requestVideoFrameCallback(cb);
  }

  _onHlsError(hls, data) {
    if (!data.fatal) return;
    switch (data.type) {
      case Hls.ErrorTypes.NETWORK_ERROR:
        hls.startLoad();
        break;
      case Hls.ErrorTypes.MEDIA_ERROR:
        hls.recoverMediaError();
        break;
      default:
        this.onError(`Playback error: ${data.details || data.type}`);
        this.destroy();
    }
  }

  play() {
    this._maybeMeasureFrameDur();
    return this.video.play();
  }
  pause() {
    this.video.pause();
  }
  setRate(r) {
    this.video.playbackRate = r;
  }
  setMuted(m) {
    this.video.muted = m;
  }
  setVolume(v) {
    this.video.volume = v;
  }

  destroy() {
    if (this.hls) {
      this.hls.destroy();
      this.hls = null;
    } else if (this.nativeHls) {
      this.video.removeAttribute("src");
      this.video.load();
    }
    this.fragments = [];
  }
}
