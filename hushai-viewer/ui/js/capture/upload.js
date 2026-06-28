// Single-flight uploader: drains the queue one segment at a time, POSTing multipart
// {manifest, body} to the same-origin capture proxy (which injects the backend bearer — the
// page holds no token and triggers no CORS; see src/proxy.rs forward_capture). Status
// handling mirrors the Android Uploader.classify contract.

const INGEST_PATH = "/api/capture/segments";
const INITIAL_BACKOFF_MS = 1000;
const MAX_BACKOFF_MS = 30000;
const MAX_RESEND = 5; // 422 integrity retries before giving up on a segment
const IDLE_POLL_MS = 200;

function sleep(ms) {
  return new Promise((r) => setTimeout(r, ms));
}

export class Uploader {
  constructor(queue, { onProgress, onError } = {}) {
    this.queue = queue;
    this.onProgress = onProgress;
    this.onError = onError;
    this.running = false;
    this.uploaded = 0;
    this.lastStatus = null;
    this.backoff = INITIAL_BACKOFF_MS;
    this.abort = null;
  }

  start() {
    if (this.running) return;
    this.running = true;
    this._drain();
  }
  stop() {
    this.running = false;
    if (this.abort) {
      try {
        this.abort.abort();
      } catch {
        /* ignore */
      }
    }
  }

  async _drain() {
    while (this.running) {
      const item = this.queue.peek();
      if (!item) {
        await sleep(IDLE_POLL_MS);
        continue;
      }
      let outcome;
      try {
        outcome = await this._send(item);
      } catch (e) {
        // Network error / aborted fetch → retryable.
        outcome = { kind: "retry", detail: String(e?.message || e) };
      }

      if (outcome.kind === "accepted") {
        this.queue.shift();
        this.uploaded++;
        this.backoff = INITIAL_BACKOFF_MS;
        this.lastStatus = 200;
        this.onProgress?.(this._state());
      } else if (outcome.kind === "drop") {
        this.queue.shift();
        this.queue.markGap(item.streamId);
        this.lastStatus = outcome.status ?? null;
        this.onError?.(outcome.detail || "dropped a segment");
        this.onProgress?.(this._state());
      } else if (outcome.kind === "resend") {
        item.resends = (item.resends || 0) + 1;
        this.lastStatus = outcome.status ?? 422;
        if (item.resends > MAX_RESEND) {
          this.queue.shift();
          this.queue.markGap(item.streamId);
          this.onError?.("gave up after repeated 422 (integrity mismatch)");
        }
        this.onProgress?.(this._state());
        await sleep(500);
      } else {
        // retry: keep the item, back off (429/507/5xx/401/network).
        this.lastStatus = outcome.status ?? null;
        if (outcome.status === 401) {
          this.onError?.("401 — the viewer→backend token is wrong (set DEVICE_TOKEN)");
        }
        this.onProgress?.(this._state());
        await sleep(this.backoff);
        this.backoff = Math.min(this.backoff * 2, MAX_BACKOFF_MS);
      }
    }
  }

  async _send(item) {
    const fd = new FormData();
    fd.append(
      "manifest",
      new Blob([item.manifestBytes], { type: "application/x-protobuf" }),
      "manifest",
    );
    fd.append(
      "body",
      new Blob([item.body], { type: "application/octet-stream" }),
      "body",
    );
    this.abort = new AbortController();
    let res;
    try {
      res = await fetch(INGEST_PATH, {
        method: "POST",
        body: fd,
        signal: this.abort.signal,
      });
    } finally {
      this.abort = null;
    }
    const code = res.status;
    if (code === 200) return { kind: "accepted" };
    if (code === 422) return { kind: "resend", status: 422 };
    if (code === 400 || code === 409)
      return { kind: "drop", status: code, detail: `permanent error ${code}` };
    // 401, 429, 507, 5xx and anything else → retryable.
    return { kind: "retry", status: code };
  }

  _state() {
    return {
      uploaded: this.uploaded,
      depth: this.queue.depth,
      lastStatus: this.lastStatus,
    };
  }
}
