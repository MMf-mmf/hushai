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
  constructor(videoEl, { onError } = {}) {
    this.video = videoEl;
    this.onError = onError || (() => {});
    this.hls = null;
    this.fragments = []; // {programDateTime (ms), start (s), duration (s)}
    this.pendingSeekMs = null;
    this.supported = !!(Hls && Hls.isSupported());
    this.nativeHls = !this.supported && videoEl.canPlayType("application/vnd.apple.mpegurl");
  }

  // Load a new master playlist (a device+window). Optionally seek once ready.
  load(src, { seekMs = null, autoplay = true } = {}) {
    this.destroy();
    this.fragments = [];
    this.pendingSeekMs = seekMs;
    this._autoplay = autoplay;

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
    if (this.pendingSeekMs != null && this.fragments.length) {
      const ms = this.pendingSeekMs;
      this.pendingSeekMs = null;
      this.seekToWallClock(ms);
    }
    if (this._autoplay) this.video.play().catch(() => {});
  }

  // ---- the core: absolute wall-clock time -> media currentTime --------------

  seekToWallClock(targetMs) {
    if (!this.fragments.length) {
      this.pendingSeekMs = targetMs; // applied on next LEVEL_LOADED
      return;
    }
    const f = this._fragAt(targetMs) ?? this._nearestFrag(targetMs);
    if (!f) return;
    const within = Math.min(Math.max((targetMs - f.pdt) / 1000, 0), Math.max(f.duration - 0.05, 0));
    try {
      this.video.currentTime = f.start + within;
    } catch {
      /* not seekable yet */
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
      const d = Math.min(Math.abs(targetMs - f.pdt), Math.abs(targetMs - (f.pdt + f.duration * 1000)));
      if (d < bestD) {
        bestD = d;
        best = f;
      }
    }
    return best;
  }

  _mediaToWall(t) {
    // inverse mapping via the fragment list (fallback when playingDate is null)
    for (const f of this.fragments) {
      if (t >= f.start && t < f.start + f.duration) return f.pdt + (t - f.start) * 1000;
    }
    return this.fragments.length ? this.fragments[0].pdt + t * 1000 : 0;
  }

  // The wall-clock extent currently buffered/loaded (first..last fragment).
  loadedRangeMs() {
    if (!this.fragments.length) return null;
    const first = this.fragments[0];
    const last = this.fragments[this.fragments.length - 1];
    return { fromMs: first.pdt, toMs: last.pdt + last.duration * 1000 };
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
