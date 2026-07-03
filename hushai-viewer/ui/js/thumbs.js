// Hover-preview thumbnails for the scrub bar. Wraps the stills endpoint
// (api.js thumbUrl -> /api/devices/{id}/thumb.jpg?t=<ns>) with:
//   - 2s-bucket quantization, aimed mid-bucket, so a wiggling cursor re-requests the
//     same URL and the browser (+ the server's content-addressed cache) can hit;
//   - a small in-page LRU of decoded Images, so re-hovering is instant and we never
//     hold more than a bounded number of JPEGs alive;
//   - an 80ms debounce with supersede-cancel, so dragging across the bar fires one
//     request for where the cursor SETTLES, not one per pixel.
//
// One consumer (the #tlPreview box) at a time — module-level debounce state is enough.

import { thumbUrl } from "./api.js";

const BUCKET_MS = 2000; // matches the 2s capture-segment cadence
const LRU_MAX = 200; // decoded Images kept alive
const DEBOUNCE_MS = 80;

// url -> { img, state: "loading"|"ok"|"err", waiters: [cb] }, in recency order
// (Map iteration order = insertion order; a hit is re-inserted to refresh recency).
const lru = new Map();
let debounceTimer = null;
let reqSeq = 0; // bumped on every request/cancel; stale loads compare against it

/** The quantized thumb URL for `ms`: 2s buckets, aimed at the bucket midpoint. */
export function urlFor(deviceId, ms) {
  return thumbUrl(deviceId, Math.floor(ms / BUCKET_MS) * BUCKET_MS + BUCKET_MS / 2);
}

/** Request the thumb near (deviceId, ms). Debounced; a newer request (or cancel())
 *  supersedes this one and its callback never fires. cb(url) on success, cb(null)
 *  when the server has no frame there. */
export function request(deviceId, ms, cb) {
  clearTimeout(debounceTimer);
  const seq = ++reqSeq;
  debounceTimer = setTimeout(() => {
    const url = urlFor(deviceId, ms);
    load(url, (ok) => {
      if (seq !== reqSeq) return; // superseded while the image was loading
      cb(ok ? url : null);
    });
  }, DEBOUNCE_MS);
}

/** Drop any pending/in-flight request (hover left, drag started, device switched). */
export function cancel() {
  clearTimeout(debounceTimer);
  debounceTimer = null;
  reqSeq++;
}

// Resolve `url` through the LRU: cached verdicts answer synchronously, an in-flight
// load queues the callback, a miss starts a new Image (evicting the oldest entry).
function load(url, done) {
  const hit = lru.get(url);
  if (hit) {
    lru.delete(url);
    lru.set(url, hit); // refresh recency
    if (hit.state === "ok") return done(true);
    if (hit.state === "err") return done(false);
    hit.waiters.push(done);
    return;
  }
  const img = new Image();
  const entry = { img, state: "loading", waiters: [done] };
  lru.set(url, entry);
  while (lru.size > LRU_MAX) lru.delete(lru.keys().next().value); // oldest first
  const settle = (ok) => {
    entry.state = ok ? "ok" : "err";
    for (const w of entry.waiters.splice(0)) w(ok);
  };
  img.onload = () => settle(true);
  img.onerror = () => settle(false);
  img.src = url;
}
