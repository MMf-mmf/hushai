// Shared polling loop: busy-guarded (a slow tick never stacks), visibility-aware
// (skips while the tab is hidden, fires immediately on return), exponential backoff
// while the poll fn rejects, and an `isPaused` hook for pages that must not refresh
// mid-edit. Replaces the four hand-rolled setInterval loops that drifted apart.
//
// Uses a setTimeout chain instead of setInterval so the busy-guard and backoff are
// structural rather than bolted on.

export function createPoller(
  fn,
  { intervalMs, maxBackoffMs = intervalMs * 8, pauseWhenHidden = true, isPaused = () => false } = {},
) {
  let timer = null;
  let running = false; // start() called and not stop()ed
  let inflight = false;
  let delay = intervalMs;

  function schedule(ms) {
    if (!running) return;
    clearTimeout(timer);
    timer = setTimeout(tick, ms);
  }

  async function tick() {
    if (!running || inflight) return;
    if ((pauseWhenHidden && document.hidden) || isPaused()) {
      schedule(intervalMs);
      return;
    }
    inflight = true;
    try {
      await fn();
      delay = intervalMs; // healthy again — reset backoff
    } catch {
      delay = Math.min(delay * 2, maxBackoffMs); // fn is expected to render its own error state
    } finally {
      inflight = false;
      schedule(delay);
    }
  }

  function onVisible() {
    if (!document.hidden) kick();
  }

  /** Run a tick now (if idle) and restart the cadence from it. */
  function kick() {
    if (!running || inflight) return;
    clearTimeout(timer);
    tick();
  }

  function start({ immediate = true } = {}) {
    if (running) return;
    running = true;
    delay = intervalMs;
    document.addEventListener("visibilitychange", onVisible);
    window.addEventListener("pagehide", stop);
    if (immediate) tick();
    else schedule(delay);
  }

  function stop() {
    running = false;
    clearTimeout(timer);
    timer = null;
    document.removeEventListener("visibilitychange", onVisible);
    window.removeEventListener("pagehide", stop);
  }

  return { start, stop, kick };
}
